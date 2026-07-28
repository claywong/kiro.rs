//! 近 1 分钟额度消耗跟踪（本地运维便利特性）
//!
//! 凭证列表里 RPM 只回答「请求有多密」，回答不了「烧钱有多快」。本模块按凭证维护一个
//! 60 秒滑动窗口，累加上游 `meteringEvent` 上报的 credits，给前端一个「最近 1 分钟消耗的
//! 余额」读数，用于判断某个号是否正在被高成本请求快速抽干。
//!
//! # 设计取舍
//! - **进程内内存，不落盘**：这是一个瞬时观测量，重启后从 0 重新填满窗口即可，
//!   不值得为它引入持久化。
//! - **不复用 trace_db 聚合**：trace 受 `traceEnabled` 开关控制，关掉后读数会静默变 0；
//!   本跟踪器挂在用量记账钩子上，与 trace 开关无关。
//! - **全局单例**：仅为在 `UsageRecordHook` 里少加一条依赖注入链路。上游那条钩子路径上
//!   本地只加一行调用，合并时冲突面最小。
//!
//! @author wangzhong

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 滑动窗口长度，与凭证列表 RPM 的 60 秒窗口对齐，便于两个读数横向对照。
const WINDOW: Duration = Duration::from_secs(60);

/// 单条消耗样本
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    credits: f64,
}

/// 按凭证的 credits 滑动窗口
#[derive(Default)]
pub struct RecentSpendTracker {
    /// credential_id -> 窗口内样本（按时间升序，队首最旧）
    inner: Mutex<HashMap<u64, Vec<Sample>>>,
}

impl RecentSpendTracker {
    /// 记录一次消耗。`credits <= 0`（含失败请求、非计费请求）直接忽略，
    /// 避免用空样本把 map 撑大。
    pub fn record(&self, credential_id: u64, credits: f64) {
        if credential_id == 0 || !credits.is_finite() || credits <= 0.0 {
            return;
        }
        let now = Instant::now();
        let mut guard = self.inner.lock();
        let samples = guard.entry(credential_id).or_default();
        prune(samples, now);
        samples.push(Sample {
            at: now,
            credits,
        });
    }

    /// 快照当前各凭证窗口内的 credits 合计。
    ///
    /// 顺手清理过期样本与空凭证条目，使长期不用的号不会一直占着 map。
    pub fn snapshot(&self) -> HashMap<u64, f64> {
        let now = Instant::now();
        let mut guard = self.inner.lock();
        let mut out = HashMap::with_capacity(guard.len());
        guard.retain(|id, samples| {
            prune(samples, now);
            if samples.is_empty() {
                return false;
            }
            out.insert(*id, samples.iter().map(|s| s.credits).sum());
            true
        });
        out
    }

    /// 删除某凭证的窗口数据（凭证被删除时调用，避免读数挂在已消失的 id 上）。
    #[allow(dead_code)]
    pub fn forget(&self, credential_id: u64) {
        self.inner.lock().remove(&credential_id);
    }
}

/// 丢弃窗口外样本（样本按时间升序，找到第一个仍在窗口内的位置整体前移）。
fn prune(samples: &mut Vec<Sample>, now: Instant) {
    let Some(cutoff) = now.checked_sub(WINDOW) else {
        return;
    };
    match samples.iter().position(|s| s.at > cutoff) {
        Some(0) => {}
        Some(keep_from) => samples.drain(..keep_from).for_each(drop),
        None => samples.clear(),
    }
}

/// 进程内全局跟踪器
static TRACKER: OnceLock<RecentSpendTracker> = OnceLock::new();

/// 获取全局跟踪器（首次访问时初始化）
pub fn tracker() -> &'static RecentSpendTracker {
    TRACKER.get_or_init(RecentSpendTracker::default)
}

/// 窗口长度（秒），供响应体自描述，前端不必硬编码 60。
pub fn window_secs() -> u64 {
    WINDOW.as_secs()
}

/// GET /api/admin/credentials/recent-spend
/// 返回各凭证近 `windowSecs` 秒消耗的 credits：
/// `{ "windowSecs": 60, "spend": { "<credentialId>": 1.234 } }`
pub async fn credential_recent_spend() -> axum::response::Response {
    let spend: HashMap<String, f64> = tracker()
        .snapshot()
        .into_iter()
        .map(|(id, credits)| (id.to_string(), credits))
        .collect();
    axum::response::IntoResponse::into_response(axum::Json(serde_json::json!({
        "windowSecs": window_secs(),
        "spend": spend,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同一凭证的多次消耗在窗口内累加
    #[test]
    fn sums_credits_within_window() {
        let t = RecentSpendTracker::default();
        t.record(1, 0.5);
        t.record(1, 0.25);
        t.record(2, 1.0);
        let snap = t.snapshot();
        assert_eq!(snap.get(&1).copied(), Some(0.75));
        assert_eq!(snap.get(&2).copied(), Some(1.0));
    }

    /// 非计费样本（0 / 负值 / NaN / 未命中凭证）不进窗口
    #[test]
    fn ignores_non_billable_samples() {
        let t = RecentSpendTracker::default();
        t.record(1, 0.0);
        t.record(1, -1.0);
        t.record(1, f64::NAN);
        t.record(0, 5.0);
        assert!(t.snapshot().is_empty());
    }

    /// 窗口外样本被丢弃，凭证条目随之消失
    #[test]
    fn prunes_expired_samples() {
        let now = Instant::now();
        let mut samples = vec![
            Sample {
                at: now - WINDOW * 2,
                credits: 1.0,
            },
            Sample {
                at: now,
                credits: 2.0,
            },
        ];
        prune(&mut samples, now);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].credits, 2.0);

        // 全部过期 → 清空
        let mut all_old = vec![Sample {
            at: now - WINDOW * 2,
            credits: 1.0,
        }];
        prune(&mut all_old, now);
        assert!(all_old.is_empty());
    }

    /// forget 清掉指定凭证
    #[test]
    fn forget_drops_credential() {
        let t = RecentSpendTracker::default();
        t.record(7, 1.0);
        t.forget(7);
        assert!(t.snapshot().is_empty());
    }
}
