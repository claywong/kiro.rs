//! 自动提取模式的判定规则（纯函数，不碰 IO）
//!
//! 自动模式的授权链条是：收到 `all_keys_dead` → 在观察窗口内反复盘点本地凭据池，
//! 确认名下卖家 Key 确已全部失效 → 下一轮 `new_keys_available` 才允许自动扣费，
//! 且只提最小数量。任何一环给不出肯定结论，就退回手动。
//!
//! 判定只读本地凭据状态（`disabled` / `disabled_reason` / `failure_count`），
//! 不打上游探活 —— 本地状态本就是真实请求失败累积出来的，够用且零成本。
//!
//! @author wangzhong

use std::time::Duration;

use super::store::ValidationStatus;

/// 观察窗口内的重查间隔
pub const VALIDATION_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// 观察窗口总长。卖家说"全部失效"的那一刻，本地往往还没探到 —— 本地的
/// `disabled` / `failure_count` 要靠真实请求打上去失败才累积，故需要留出观察时间。
pub const VALIDATION_WINDOW: Duration = Duration::from_secs(180);

/// 提取入库时写入的 `source_channel` 前缀（见 `service.rs` 的 `import_keys`）
pub const VENDOR_CHANNEL_PREFIX: &str = "vendor:";

/// 本地连续失败自动禁用阈值，与 `token_manager` 的 `MAX_FAILURES_PER_CREDENTIAL`
/// 对齐。达到该值的凭据必然已被置为 disabled，这里再判一次是防御性的。
const LOCAL_FAILURE_THRESHOLD: u32 = 3;

/// 判定所需的凭据状态切片。
///
/// 刻意不直接用 `CredentialEntrySnapshot` —— 判定规则只关心这四个字段，
/// 用自带类型可让规则脱离凭据池独立测试，上游快照加减字段也不波及此处。
#[derive(Debug, Clone, Default)]
pub struct VendorKeyState {
    pub source_channel: Option<String>,
    pub disabled: bool,
    pub disabled_reason: Option<String>,
    pub failure_count: u32,
}

/// 单张卖家 Key 的健康归类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyHealth {
    /// 还能用
    Alive,
    /// 确认失效（因失败被自动禁用）
    Dead,
    /// 说不清：被人工禁用或禁用未记原因。它不反映 Key 本身状态，不能单独当作
    /// 失效证据；但它同样不可用，故不阻塞「已无可用 Key」的结论（见 [`conclude`]）
    Ambiguous,
}

/// 本地凭据池中卖家 Key 的盘点结果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VendorKeyCensus {
    pub total: usize,
    pub alive: usize,
    pub dead: usize,
    pub ambiguous: usize,
}

/// 归类单条凭据。
///
/// 人工禁用（`Manual`）单独归为 [`KeyHealth::Ambiguous`]：用户在面板上手动停用
/// 一张还能用的 Key 是常见操作，把它算作失效会让自动模式凭一个假信号去扣费。
fn classify(entry: &VendorKeyState) -> KeyHealth {
    if entry.disabled {
        return match entry.disabled_reason.as_deref() {
            Some("Manual") => KeyHealth::Ambiguous,
            // 其余禁用原因均由上游拒绝或刷新失败触发，是失效的直接证据
            Some(_) => KeyHealth::Dead,
            // 禁用但没记原因：无从判断，不当作证据
            None => KeyHealth::Ambiguous,
        };
    }
    if entry.failure_count >= LOCAL_FAILURE_THRESHOLD {
        return KeyHealth::Dead;
    }
    KeyHealth::Alive
}

/// 盘点凭据池里所有来自卖家的 Key。
///
/// 按 `source_channel` 的 `vendor:` 前缀筛选 —— 自建或其他渠道导入的凭据
/// 与卖家轮换无关，不该影响补货判断。
pub fn census(entries: &[VendorKeyState]) -> VendorKeyCensus {
    let mut c = VendorKeyCensus::default();
    for entry in entries.iter().filter(|e| {
        e.source_channel
            .as_deref()
            .is_some_and(|s| s.starts_with(VENDOR_CHANNEL_PREFIX))
    }) {
        c.total += 1;
        match classify(entry) {
            KeyHealth::Alive => c.alive += 1,
            KeyHealth::Dead => c.dead += 1,
            KeyHealth::Ambiguous => c.ambiguous += 1,
        }
    }
    c
}

/// 由盘点结果得出确认结论。
///
/// `window_expired` 为 false 时（观察窗口内），只有"确认全部失效"是终态，
/// 其余一律 [`ValidationStatus::Pending`] 继续观察 —— 卖家刚说失效时本地通常
/// 还没探到，此刻的"仍然健康"不是结论。
///
/// 待定（人工禁用 / 禁用未记原因）不阻塞结论：这类 Key 同样处于不可用状态，
/// 拿不到它"是否真失效"的证据不代表它能用。因此判据只看有无存活的 Key —— 一旦
/// `alive == 0` 即刻确认，不必等窗口走完：观察窗口的意义是等本地失败计数追上现实，
/// 而"已无可用 Key"这个结论不依赖失效证据，再等也不会更成立。
pub fn conclude(c: VendorKeyCensus, window_expired: bool) -> (ValidationStatus, String) {
    // 池里没有卖家 Key：没有任何存活的东西，补货的前提天然成立
    if c.total == 0 {
        return (
            ValidationStatus::ConfirmedDead,
            "本地没有来自卖家的凭据，无需确认".to_string(),
        );
    }

    if c.alive == 0 {
        let detail = if c.ambiguous == 0 {
            format!("{} 张卖家 Key 均已失效", c.dead)
        } else {
            format!(
                "已无可用卖家 Key：失效 {} / 人工禁用或无禁用原因 {}（共 {}）",
                c.dead, c.ambiguous, c.total
            )
        };
        return (ValidationStatus::ConfirmedDead, detail);
    }

    // 仍有存活的 Key：窗口内继续观察，等本地状态追上卖家的说法
    if !window_expired {
        return (
            ValidationStatus::Pending,
            format!(
                "观察中：存活 {} / 失效 {} / 待定 {}（共 {}）",
                c.alive, c.dead, c.ambiguous, c.total
            ),
        );
    }

    (
        ValidationStatus::StillAlive,
        format!(
            "观察窗口结束仍有 {} 张卖家 Key 健康（共 {}），未触发自动提取",
            c.alive, c.total
        ),
    )
}

/// 自动模式的提取数量：三者取最小。
///
/// 数量一旦提交就与订单号永久绑定、无法改小，自动模式没有人工复核，
/// 因此宁可少提 —— 少提还能再手动补，多提是永久的。
pub fn decide_count(new_keys: Option<u32>, stock_max: u32, configured_max: u32) -> u32 {
    new_keys.unwrap_or(0).min(stock_max).min(configured_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        channel: Option<&str>,
        disabled: bool,
        reason: Option<&str>,
        failures: u32,
    ) -> VendorKeyState {
        VendorKeyState {
            source_channel: channel.map(String::from),
            disabled,
            disabled_reason: reason.map(String::from),
            failure_count: failures,
        }
    }

    #[test]
    fn 只统计卖家来源的凭据() {
        let entries = vec![
            entry(Some("vendor:abc"), true, Some("TooManyFailures"), 3),
            entry(Some("手动导入"), false, None, 0),
            entry(None, false, None, 0),
        ];
        let c = census(&entries);
        assert_eq!(c.total, 1);
        assert_eq!(c.dead, 1);
        assert_eq!(c.alive, 0);
    }

    #[test]
    fn 人工禁用归为待定而非失效() {
        let entries = vec![entry(Some("vendor:abc"), true, Some("Manual"), 0)];
        let c = census(&entries);
        assert_eq!(c.ambiguous, 1);
        assert_eq!(c.dead, 0);
        // 待定 Key 同样不可用，不阻塞确认，且无需等窗口走完
        let (status, detail) = conclude(c, false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
        assert!(detail.contains("已无可用卖家 Key"));
        assert_eq!(conclude(c, true).0, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 禁用但无原因窗口结束后也确认() {
        let entries = vec![
            entry(Some("vendor:a"), true, None, 0),
            entry(Some("vendor:b"), true, Some("Manual"), 0),
            entry(Some("vendor:c"), true, Some("TooManyFailures"), 3),
        ];
        let c = census(&entries);
        assert_eq!(c.ambiguous, 2);
        assert_eq!(c.dead, 1);
        assert_eq!(c.alive, 0);
        // 首次盘点即确认，不必等 3 分钟窗口
        assert_eq!(conclude(c, false).0, ValidationStatus::ConfirmedDead);
        assert_eq!(conclude(c, true).0, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 有存活时待定不影响仍然健康的结论() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("Manual"), 0),
            entry(Some("vendor:b"), false, None, 0),
        ];
        let c = census(&entries);
        assert_eq!(conclude(c, true).0, ValidationStatus::StillAlive);
    }

    #[test]
    fn 全部失效才确认() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:b"), true, Some("InvalidRefreshToken"), 0),
        ];
        let (status, detail) = conclude(census(&entries), false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
        assert!(detail.contains('2'));
    }

    #[test]
    fn 窗口内仍有健康的记为观察中() {
        let entries = vec![
            entry(Some("vendor:a"), true, Some("TooManyFailures"), 3),
            entry(Some("vendor:b"), false, None, 0),
        ];
        let c = census(&entries);
        assert_eq!(conclude(c, false).0, ValidationStatus::Pending);
        // 窗口结束才落定为「仍然健康」
        assert_eq!(conclude(c, true).0, ValidationStatus::StillAlive);
    }

    #[test]
    fn 未达阈值的失败仍算健康() {
        let entries = vec![entry(Some("vendor:a"), false, None, 2)];
        assert_eq!(census(&entries).alive, 1);
        let entries = vec![entry(Some("vendor:a"), false, None, 3)];
        assert_eq!(census(&entries).dead, 1);
    }

    #[test]
    fn 池里没有卖家key时直接确认() {
        let (status, _) = conclude(census(&[]), false);
        assert_eq!(status, ValidationStatus::ConfirmedDead);
    }

    #[test]
    fn 提取数量取三者最小() {
        assert_eq!(decide_count(Some(10), 5, 1), 1);
        assert_eq!(decide_count(Some(10), 0, 3), 0);
        assert_eq!(decide_count(None, 5, 3), 0);
        assert_eq!(decide_count(Some(2), 5, 3), 2);
    }
}
