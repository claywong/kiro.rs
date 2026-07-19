//! 凭证成本账本
//!
//! 记录每个凭证的「购买成本」与「额度快照」，按**使用率折算**每天成本，
//! 独立于凭证生命周期持久化（凭证删除后仍需展示历史成本）。
//!
//! # 核算口径
//! - 单价 `unit_price = purchase_cost / quota_basis`（quota_basis 为录入/验活时快照的
//!   上游 `usage_limit`）。
//! - 某天某凭证的**日常摊销** = `min(当天消耗 credits × 单价, 该凭证剩余未摊销成本)`，
//!   累计摊销封顶到 `purchase_cost`，避免超摊。
//! - 凭证被**删除**或**自动禁用**（InvalidRefreshToken / TooManyFailures /
//!   TooManyRefreshFailures / InvalidConfig）时，剩余未摊销成本一次性计入**废弃当天**。
//! - 恒等式：每个凭证的累计总成本 = `purchase_cost`（要么摊满，要么废弃日补齐）。
//!
//! # 持久化
//! 内存 `HashMap<u64, CostEntry>` + JSON 落盘到 `cache_dir/cost_ledger.json`，
//! 每次变更后写盘。**不依赖任何外部存储**。
//!
//! @author wangzhong

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::admin::types::{CostPoint, CostSeriesResponse};

/// 触发废弃的自动禁用原因（与 DisabledReason 序列化字符串对齐）。
/// QuotaExceeded（额度耗尽，剩余≈0）与 Manual（手动禁用，可能恢复）不算废弃。
pub const DISCARD_DISABLED_REASONS: &[&str] = &[
    "InvalidRefreshToken",
    "TooManyFailures",
    "TooManyRefreshFailures",
    "InvalidConfig",
];

/// 单个凭证的成本账本条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostEntry {
    /// 购买成本（货币金额）
    pub purchase_cost: f64,
    /// 额度快照（单价分母）；录入/验活时快照 usage_limit，未取到时为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_basis: Option<f64>,
    /// 录入日期（本地 YYYY-MM-DD）
    pub created_date: String,
    /// 废弃日期（本地 YYYY-MM-DD）；None 表示未废弃
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_date: Option<String>,
    /// 废弃原因（"deleted" 或某个 DisabledReason 字符串）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discard_reason: Option<String>,
}

impl CostEntry {
    /// 单价：quota_basis 有效（> 0）时返回 purchase_cost / quota_basis，否则 None。
    fn unit_price(&self) -> Option<f64> {
        match self.quota_basis {
            Some(basis) if basis > 0.0 && self.purchase_cost >= 0.0 => {
                Some(self.purchase_cost / basis)
            }
            _ => None,
        }
    }
}

/// 成本账本（线程安全 + JSON 落盘）
pub struct CostLedger {
    inner: Mutex<HashMap<u64, CostEntry>>,
    path: Option<PathBuf>,
}

pub type SharedCostLedger = Arc<CostLedger>;

impl CostLedger {
    /// 从指定路径加载（文件不存在/损坏时返回空账本）
    pub fn load(path: Option<PathBuf>) -> Self {
        let map = match &path {
            Some(p) => Self::load_from(p),
            None => HashMap::new(),
        };
        Self {
            inner: Mutex::new(map),
            path,
        }
    }

    fn load_from(path: &PathBuf) -> HashMap<u64, CostEntry> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };
        // 文件中用字符串 key 兼容 JSON
        let map: HashMap<String, CostEntry> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析成本账本失败，将忽略: {}", e);
                return HashMap::new();
            }
        };
        map.into_iter()
            .filter_map(|(k, v)| Some((k.parse::<u64>().ok()?, v)))
            .collect()
    }

    fn save(&self) {
        let path = match &self.path {
            Some(p) => p,
            None => return,
        };
        let map: HashMap<String, CostEntry> = {
            let guard = self.inner.lock();
            guard.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
        };
        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存成本账本失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化成本账本失败: {}", e),
        }
    }

    /// 录入/编辑时设置购买成本。
    /// - `Some(cost)` 且 cost >= 0：设置购买成本（保留已有 quota_basis / 废弃状态）。
    /// - `None` 或负值：清除该凭证的成本记录。
    pub fn set_purchase_cost(&self, id: u64, cost: Option<f64>) {
        {
            let mut guard = self.inner.lock();
            match cost {
                Some(c) if c.is_finite() && c >= 0.0 => {
                    let entry = guard.entry(id).or_insert_with(|| CostEntry {
                        purchase_cost: c,
                        quota_basis: None,
                        created_date: today_local(),
                        discarded_date: None,
                        discard_reason: None,
                    });
                    entry.purchase_cost = c;
                }
                _ => {
                    guard.remove(&id);
                }
            }
        }
        self.save();
    }

    /// 快照/回填额度基数（单价分母）。仅在该凭证已有成本记录时生效。
    /// `force = false` 时仅当 quota_basis 为空才回填（避免覆盖录入时的快照）。
    pub fn snapshot_quota_basis(&self, id: u64, usage_limit: f64, force: bool) {
        if !(usage_limit.is_finite() && usage_limit > 0.0) {
            return;
        }
        let mut changed = false;
        {
            let mut guard = self.inner.lock();
            if let Some(entry) = guard.get_mut(&id) {
                if force || entry.quota_basis.is_none() {
                    if entry.quota_basis != Some(usage_limit) {
                        entry.quota_basis = Some(usage_limit);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.save();
        }
    }

    /// 标记凭证废弃（删除/自动禁用）。仅在有成本记录且未废弃时生效，幂等。
    /// `reason`："deleted" 或某个 DisabledReason 字符串。
    pub fn mark_discarded(&self, id: u64, reason: &str) {
        let mut changed = false;
        {
            let mut guard = self.inner.lock();
            if let Some(entry) = guard.get_mut(&id) {
                if entry.discarded_date.is_none() {
                    entry.discarded_date = Some(today_local());
                    entry.discard_reason = Some(reason.to_string());
                    changed = true;
                }
            }
        }
        if changed {
            tracing::info!("凭证 #{} 成本废弃：{}（剩余成本计入今日）", id, reason);
            self.save();
        }
    }

    /// 依据当前凭证快照检测自动禁用型废弃。
    /// `disabled_creds`：`(id, disabled_reason)` 列表（仅传 disabled=true 的）。
    pub fn detect_auto_disabled(&self, disabled_creds: &[(u64, Option<String>)]) {
        for (id, reason) in disabled_creds {
            if let Some(r) = reason {
                if DISCARD_DISABLED_REASONS.contains(&r.as_str()) {
                    self.mark_discarded(*id, r);
                }
            }
        }
    }

    /// 读取单个凭证的成本条目（用于展示 / 单测）
    #[allow(dead_code)]
    pub fn get(&self, id: u64) -> Option<CostEntry> {
        self.inner.lock().get(&id).cloned()
    }

    /// 计算成本序列。
    ///
    /// # 参数
    /// - `daily_credits`：**全量历史**按天×凭证的 credits，`(day_ts, cred_id -> credits)`，
    ///   day_ts 为本地午夜 Unix 秒。传全量（非仅窗口内）是为了正确累积「剩余未摊销成本」
    ///   —— 某天的摊销取决于此前累计已摊多少。
    /// - `window_start_ts` / `window_end_ts`：展示窗口（闭开区间 `[start, end)`，本地午夜对齐）。
    /// - `currency`：货币符号。
    ///
    /// # 说明
    /// 逐日按时间顺序推进，先摊销当日消耗（封顶剩余成本），再处理当日废弃（补齐剩余）。
    /// 仅统计**有成本记录且单价可算**（quota_basis 有效）的凭证；quota_basis 暂空的凭证
    /// 当天成本按 0，待余额刷新回填后生效。输出仅保留落在窗口内的天。
    pub fn compute_cost_series(
        &self,
        daily_credits: &[(i64, HashMap<u64, f64>)],
        window_start_ts: i64,
        window_end_ts: i64,
        currency: &str,
    ) -> CostSeriesResponse {
        let entries = self.inner.lock().clone();

        // 每个可算凭证的剩余未摊销成本（初始 = purchase_cost）
        let mut remaining: HashMap<u64, f64> = HashMap::new();
        // 单价缓存
        let mut prices: HashMap<u64, f64> = HashMap::new();
        // 废弃日 ts -> 凭证列表
        let mut discard_by_day: HashMap<i64, Vec<u64>> = HashMap::new();

        for (id, entry) in &entries {
            let Some(price) = entry.unit_price() else {
                continue; // 单价不可算：跳过（成本按 0，回填后生效）
            };
            remaining.insert(*id, entry.purchase_cost);
            prices.insert(*id, price);
            if let Some(dd) = &entry.discarded_date {
                if let Some(dts) = local_midnight_ts(dd) {
                    discard_by_day.entry(dts).or_default().push(*id);
                }
            }
        }

        // 消耗天 ts -> (cred -> credits)，便于按天查找
        let credits_by_day: HashMap<i64, &HashMap<u64, f64>> =
            daily_credits.iter().map(|(ts, m)| (*ts, m)).collect();

        // 所有相关天（消耗天 ∪ 废弃天），升序推进
        let mut all_days: BTreeSet<i64> = BTreeSet::new();
        all_days.extend(credits_by_day.keys().copied());
        all_days.extend(discard_by_day.keys().copied());

        let mut points: Vec<CostPoint> = Vec::new();
        let (mut total_cost, mut total_amortized, mut total_discard) = (0.0f64, 0.0f64, 0.0f64);

        for day_ts in all_days {
            let mut amort_day = 0.0f64;
            let mut discard_day = 0.0f64;

            // 1) 当日日常摊销（封顶剩余成本）
            if let Some(day_credits) = credits_by_day.get(&day_ts) {
                for (id, credits) in day_credits.iter() {
                    let (Some(price), Some(rem)) = (prices.get(id), remaining.get_mut(id)) else {
                        continue;
                    };
                    if *rem <= 0.0 {
                        continue;
                    }
                    let raw = credits.max(0.0) * price;
                    let a = raw.min(*rem);
                    *rem -= a;
                    amort_day += a;
                }
            }

            // 2) 当日废弃补齐（剩余一次性计入）
            if let Some(ids) = discard_by_day.get(&day_ts) {
                for id in ids {
                    if let Some(rem) = remaining.get_mut(id) {
                        if *rem > 0.0 {
                            discard_day += *rem;
                            *rem = 0.0;
                        }
                    }
                }
            }

            // 仅保留窗口内的天（[start, end)）
            if day_ts >= window_start_ts && day_ts < window_end_ts {
                let total = amort_day + discard_day;
                total_cost += total;
                total_amortized += amort_day;
                total_discard += discard_day;
                points.push(CostPoint {
                    date: ts_to_local_date(day_ts),
                    amortized_cost: round2(amort_day),
                    discard_cost: round2(discard_day),
                    total_cost: round2(total),
                });
            }
        }

        points.sort_by(|a, b| a.date.cmp(&b.date));
        CostSeriesResponse {
            currency: currency.to_string(),
            total_cost: round2(total_cost),
            total_amortized: round2(total_amortized),
            total_discard: round2(total_discard),
            points,
        }
    }
}

/// 保留 2 位小数
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 本地午夜 Unix 秒 → 本地日期字符串（YYYY-MM-DD）
fn ts_to_local_date(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// 本地当天日期字符串（YYYY-MM-DD）
fn today_local() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// 本地日期字符串 → 本地午夜 Unix 秒（用于与天桶 ts 对齐排序）
fn local_midnight_ts(date_str: &str) -> Option<i64> {
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无落盘的内存账本
    fn mem_ledger() -> CostLedger {
        CostLedger::load(None)
    }

    /// 某本地日期的午夜 ts（测试构造消耗天用）
    fn day_ts(date: &str) -> i64 {
        local_midnight_ts(date).unwrap()
    }

    #[test]
    fn unit_price_requires_valid_basis() {
        let e = CostEntry {
            purchase_cost: 100.0,
            quota_basis: Some(1000.0),
            created_date: "2026-07-01".into(),
            discarded_date: None,
            discard_reason: None,
        };
        assert_eq!(e.unit_price(), Some(0.1));

        // quota_basis 缺失/为 0 → None
        let mut e2 = e.clone();
        e2.quota_basis = None;
        assert_eq!(e2.unit_price(), None);
        e2.quota_basis = Some(0.0);
        assert_eq!(e2.unit_price(), None);
    }

    #[test]
    fn set_and_clear_purchase_cost() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(50.0));
        assert_eq!(l.get(1).map(|e| e.purchase_cost), Some(50.0));

        // 负值 → 清除
        l.set_purchase_cost(1, Some(-1.0));
        assert!(l.get(1).is_none());

        // None → 清除（幂等）
        l.set_purchase_cost(2, None);
        assert!(l.get(2).is_none());
    }

    #[test]
    fn snapshot_quota_basis_respects_force() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0));

        // 非 force 回填空值
        l.snapshot_quota_basis(1, 1000.0, false);
        assert_eq!(l.get(1).and_then(|e| e.quota_basis), Some(1000.0));

        // 非 force 不覆盖已有值
        l.snapshot_quota_basis(1, 2000.0, false);
        assert_eq!(l.get(1).and_then(|e| e.quota_basis), Some(1000.0));

        // force 覆盖
        l.snapshot_quota_basis(1, 2000.0, true);
        assert_eq!(l.get(1).and_then(|e| e.quota_basis), Some(2000.0));

        // 无成本记录的凭证不创建条目
        l.snapshot_quota_basis(99, 500.0, true);
        assert!(l.get(99).is_none());
    }
    #[test]
    fn amortization_caps_at_purchase_cost() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0));
        l.snapshot_quota_basis(1, 1000.0, true); // 单价 0.1

        // 两天各消耗 800 credits（各值 80），累计应封顶到 100
        let daily = vec![
            (day_ts("2026-07-10"), HashMap::from([(1u64, 800.0)])),
            (day_ts("2026-07-11"), HashMap::from([(1u64, 800.0)])),
        ];
        let start = day_ts("2026-07-01");
        let end = day_ts("2026-08-01");
        let r = l.compute_cost_series(&daily, start, end, "¥");

        assert_eq!(r.points.len(), 2);
        assert_eq!(r.points[0].amortized_cost, 80.0);
        assert_eq!(r.points[1].amortized_cost, 20.0); // 封顶
        assert_eq!(r.total_amortized, 100.0);
        assert_eq!(r.total_discard, 0.0);
    }

    #[test]
    fn discard_dumps_residual_on_that_day() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0));
        l.snapshot_quota_basis(1, 1000.0, true); // 单价 0.1
        // 手动置废弃日
        {
            let mut g = l.inner.lock();
            let e = g.get_mut(&1).unwrap();
            e.discarded_date = Some("2026-07-15".into());
            e.discard_reason = Some("deleted".into());
        }

        // 7-10 消耗 200 credits → 摊 20；7-15 废弃 → 补齐剩余 80
        let daily = vec![(day_ts("2026-07-10"), HashMap::from([(1u64, 200.0)]))];
        let start = day_ts("2026-07-01");
        let end = day_ts("2026-08-01");
        let r = l.compute_cost_series(&daily, start, end, "¥");

        assert_eq!(r.total_amortized, 20.0);
        assert_eq!(r.total_discard, 80.0);
        // 恒等式：总成本 = 购买成本
        assert_eq!(r.total_cost, 100.0);
        let discard_day = r.points.iter().find(|p| p.date == "2026-07-15").unwrap();
        assert_eq!(discard_day.discard_cost, 80.0);
    }

    #[test]
    fn missing_quota_basis_yields_zero_cost() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0)); // 无 quota_basis → 单价不可算
        let daily = vec![(day_ts("2026-07-10"), HashMap::from([(1u64, 500.0)]))];
        let start = day_ts("2026-07-01");
        let end = day_ts("2026-08-01");
        let r = l.compute_cost_series(&daily, start, end, "¥");
        assert_eq!(r.total_cost, 0.0);
    }

    #[test]
    fn out_of_window_consumption_still_accrues() {
        // 窗口外的历史消耗应参与剩余成本累积（只是不出现在输出点里）
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0));
        l.snapshot_quota_basis(1, 1000.0, true); // 单价 0.1

        // 7-05（窗口外）消耗 900 → 摊 90；7-20（窗口内）消耗 900 → 只剩 10 可摊
        let daily = vec![
            (day_ts("2026-07-05"), HashMap::from([(1u64, 900.0)])),
            (day_ts("2026-07-20"), HashMap::from([(1u64, 900.0)])),
        ];
        let start = day_ts("2026-07-10");
        let end = day_ts("2026-07-31");
        let r = l.compute_cost_series(&daily, start, end, "¥");

        // 输出只含窗口内的 7-20
        assert_eq!(r.points.len(), 1);
        assert_eq!(r.points[0].date, "2026-07-20");
        assert_eq!(r.points[0].amortized_cost, 10.0); // 剩余被前面累积扣掉
    }

    #[test]
    fn mark_discarded_is_idempotent() {
        let l = mem_ledger();
        l.set_purchase_cost(1, Some(100.0));
        l.mark_discarded(1, "deleted");
        let first = l.get(1).unwrap().discarded_date.clone();
        // 再次废弃不覆盖日期与原因
        l.mark_discarded(1, "InvalidConfig");
        let e = l.get(1).unwrap();
        assert_eq!(e.discarded_date, first);
        assert_eq!(e.discard_reason.as_deref(), Some("deleted"));
    }

    #[test]
    fn detect_auto_disabled_only_discards_matching_reasons() {
        let l = mem_ledger();
        for id in 1..=3 {
            l.set_purchase_cost(id, Some(10.0));
        }
        let disabled = vec![
            (1u64, Some("TooManyFailures".to_string())), // 触发
            (2u64, Some("QuotaExceeded".to_string())),   // 不触发
            (3u64, Some("Manual".to_string())),          // 不触发
        ];
        l.detect_auto_disabled(&disabled);
        assert!(l.get(1).unwrap().discarded_date.is_some());
        assert!(l.get(2).unwrap().discarded_date.is_none());
        assert!(l.get(3).unwrap().discarded_date.is_none());
    }
}

