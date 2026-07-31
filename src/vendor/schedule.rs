//! 自动提取数量的时段表（纯函数，不碰 IO、不读时钟）
//!
//! 需求背景：单张卖家 Key 的 RPM 有上限，下午与晚上的实际压力需要多持有一张，
//! 其余时段一张够用。故自动模式的「单次提取上限」不是常量，而是随本地时刻变化。
//!
//! 为什么做成「改上限」而不是「补一次提取」：一次 `all_keys_dead` 的确认结论
//! 只授权一轮自动提取（见 [`super::service`] 的 `consume_validation`），且
//! `bound_count` 一旦写入就与订单号永久绑定、无法改数量重试。所以「多要一个」
//! 只能在首次下单前决定，没有第二次机会。
//!
//! 时钟由调用方传入（`now` 参数），使本模块可完整单测而不依赖真实时间。
//!
//! @author wangzhong

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

/// 一条时段规则：`[from, to]` 内自动提取上限取 `max_count`
///
/// `start` / `end` 是 `from` / `to` 的别名：早期文档与 `config.example.json`
/// 用的是这组名字。两者都是必填字段（无 serde 默认值），不加别名会让按旧文档
/// 配置的人直接启动失败（`missing field from`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoPurchaseWindow {
    /// 起始时刻，`HH:MM`（本地时间）
    #[serde(alias = "start")]
    pub from: String,
    /// 结束时刻，`HH:MM`（本地时间），含该分钟。`to` 早于 `from` 视为跨午夜
    #[serde(alias = "end")]
    pub to: String,
    /// 该时段内的单次提取上限
    pub max_count: u32,
}

/// 解析 `HH:MM`。也接受 `HH:MM:SS`，解析失败返回 None。
fn parse_time(raw: &str) -> Option<NaiveTime> {
    let s = raw.trim();
    NaiveTime::parse_from_str(s, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
        .ok()
}

/// 当前时刻是否落在该时段内。
///
/// `to < from` 视为跨午夜（如 `22:00`–`02:00`），此时命中条件是
/// 「晚于 from」**或**「早于等于 to」。边界两端都含。
fn hits(window: &AutoPurchaseWindow, now: NaiveTime) -> bool {
    let (Some(from), Some(to)) = (parse_time(&window.from), parse_time(&window.to)) else {
        // 时刻写错时不命中，退回兜底上限 —— 宁可少提，不因配置笔误多扣费
        tracing::warn!(
            from = %window.from,
            to = %window.to,
            "自动提取时段的时刻格式无法解析，该段已忽略（需 HH:MM）"
        );
        return false;
    };
    if from <= to {
        now >= from && now <= to
    } else {
        now >= from || now <= to
    }
}

/// 求当前生效的自动提取上限。
///
/// 多段命中时取**最大**值：时段重叠通常是有意叠加（如「全天 1」+「晚高峰 2」），
/// 取大符合直觉。无命中时用 `fallback`（即 `autoPurchaseMaxCount`）。
pub fn max_count_at(windows: &[AutoPurchaseWindow], fallback: u32, now: NaiveTime) -> u32 {
    windows
        .iter()
        .filter(|w| hits(w, now))
        .map(|w| w.max_count)
        .max()
        .unwrap_or(fallback)
}

/// 命中的时段描述，供面板展示「当前为什么是这个数」。无命中返回 None。
pub fn active_window_label(windows: &[AutoPurchaseWindow], now: NaiveTime) -> Option<String> {
    let hit: Vec<&AutoPurchaseWindow> = windows.iter().filter(|w| hits(w, now)).collect();
    let top = hit.iter().max_by_key(|w| w.max_count)?;
    Some(format!("{}–{}", top.from.trim(), top.to.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(from: &str, to: &str, max: u32) -> AutoPurchaseWindow {
        AutoPurchaseWindow {
            from: from.to_string(),
            to: to.to_string(),
            max_count: max,
        }
    }

    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn 无时段表时用兜底值() {
        assert_eq!(max_count_at(&[], 1, at(15, 0)), 1);
        assert_eq!(max_count_at(&[], 3, at(3, 0)), 3);
    }

    #[test]
    fn 命中时段取时段值() {
        let s = vec![w("14:00", "23:59", 2)];
        assert_eq!(max_count_at(&s, 1, at(14, 0)), 2, "起点含边界");
        assert_eq!(max_count_at(&s, 1, at(18, 30)), 2);
        assert_eq!(max_count_at(&s, 1, at(23, 59)), 2, "终点含边界");
        assert_eq!(max_count_at(&s, 1, at(13, 59)), 1, "起点前用兜底");
        assert_eq!(max_count_at(&s, 1, at(0, 0)), 1, "次日凌晨用兜底");
    }

    #[test]
    fn 跨午夜时段() {
        let s = vec![w("22:00", "02:00", 2)];
        assert_eq!(max_count_at(&s, 1, at(23, 0)), 2);
        assert_eq!(max_count_at(&s, 1, at(1, 0)), 2);
        assert_eq!(max_count_at(&s, 1, at(2, 0)), 2, "终点含边界");
        assert_eq!(max_count_at(&s, 1, at(2, 1)), 1);
        assert_eq!(max_count_at(&s, 1, at(12, 0)), 1);
    }

    #[test]
    fn 多段重叠取最大() {
        let s = vec![
            w("00:00", "23:59", 1),
            w("14:00", "23:00", 2),
            w("20:00", "22:00", 3),
        ];
        assert_eq!(max_count_at(&s, 1, at(10, 0)), 1);
        assert_eq!(max_count_at(&s, 1, at(15, 0)), 2);
        assert_eq!(max_count_at(&s, 1, at(21, 0)), 3);
    }

    /// 配置笔误不能变成多扣费
    #[test]
    fn 时刻格式非法时忽略该段() {
        let s = vec![w("下午两点", "23:59", 5)];
        assert_eq!(max_count_at(&s, 1, at(15, 0)), 1);
        // 一段坏不影响另一段
        let s = vec![w("bad", "bad", 9), w("14:00", "23:59", 2)];
        assert_eq!(max_count_at(&s, 1, at(15, 0)), 2);
    }

    #[test]
    fn 接受带秒的时刻() {
        let s = vec![w("14:00:00", "23:59:59", 2)];
        assert_eq!(max_count_at(&s, 1, at(15, 0)), 2);
    }

    #[test]
    fn 时段上限为0表示该时段不自动提取() {
        let s = vec![w("02:00", "06:00", 0)];
        assert_eq!(max_count_at(&s, 1, at(3, 0)), 0, "夜间可显式停掉自动提取");
        assert_eq!(max_count_at(&s, 1, at(15, 0)), 1);
    }

    #[test]
    fn 命中时段的展示标签() {
        let s = vec![w("00:00", "23:59", 1), w("14:00", "23:00", 2)];
        assert_eq!(
            active_window_label(&s, at(15, 0)).as_deref(),
            Some("14:00–23:00")
        );
        assert!(active_window_label(&[], at(15, 0)).is_none());
    }
}
