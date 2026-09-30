//! 会话亲和命中的 RPM 硬上限。
//!
//! 亲和命中为保住上游 prompt cache 允许突破 RPM（软约束），但不能无限突破：
//! 凭据级 `rpm_limit` 与池级 `account_rpm_limit` 各自放宽到 `limit × 倍数`
//! （`session_affinity_rpm_multiplier`，默认 3），任一窗口达到该值即返回 429 +
//! Retry-After，不改绑——客户端退避后回到原号，缓存仍在。
//! 被拒的请求没有发往上游，不写入任何 RPM 窗口。
//! @author wangzhong

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{MultiTokenManager, RPM_WINDOW};
use crate::kiro::error::UpstreamRateLimitError;

impl MultiTokenManager {
    /// 选号确定、Token 就绪后的 RPM 记账。
    ///
    /// `Ok(false)`：名额被并发请求抢先占用，调用方应重新选号；
    /// `Err`：亲和命中且已达放宽上限，直接向客户端返回 429。
    ///
    /// 仅真实业务请求（`update_current`）计入池级窗口；Admin 只读模型发现不消耗额度。
    /// per-cred 记账必须先于写亲和表：名额被抢时重新选号，绑定不会指向没有名额的号。
    pub(super) fn record_selected_rpm(
        &self,
        id: u64,
        is_affinity_hit: bool,
        update_current: bool,
        per_cred_rpm_recorded: Option<&HashSet<u64>>,
    ) -> anyhow::Result<bool> {
        // Some：在此记 per-cred RPM（集合内的凭据已记过，跳过）；None：调用方自行记账
        let record_cred = per_cred_rpm_recorded.is_some_and(|recorded| !recorded.contains(&id));
        if is_affinity_hit {
            self.record_affinity_hit_rpm(id, update_current, record_cred)?;
            return Ok(true);
        }
        if update_current && !self.record_request(id) {
            // Token 获取期间额度可能被其它并发请求抢先占用；重新选号。
            return Ok(false);
        }
        if record_cred && !self.try_record_request(id) {
            return Ok(false);
        }
        Ok(true)
    }

    /// 亲和命中：在同一把锁内检查两个窗口的放宽上限，都未达到才一起记账。
    fn record_affinity_hit_rpm(
        &self,
        id: u64,
        record_pool: bool,
        record_cred: bool,
    ) -> anyhow::Result<()> {
        let now = Instant::now();
        let multiplier = self.config.session_affinity_rpm_multiplier;
        let pool_limit = if self.account_rpm_limit_enabled.load(Ordering::Relaxed) {
            self.account_rpm_limit.load(Ordering::Relaxed)
        } else {
            0
        };

        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
            return Ok(());
        };
        let cred_cap = affinity_rpm_cap(entry.credentials.rpm_limit, multiplier);
        let pool_cap = affinity_rpm_cap(pool_limit, multiplier);

        // 同号重试（已记过账）不再占名额，也不因上限被拒。
        let cred_retry = record_cred
            .then(|| cap_retry_after_secs(&entry.recent_requests, cred_cap, now))
            .flatten();
        let pool_retry = record_pool
            .then(|| cap_retry_after_secs(&entry.rpm_window, pool_cap, now))
            .flatten();
        if let Some(retry_after) = cred_retry.max(pool_retry) {
            tracing::warn!(
                credential_id = id,
                cred_rpm = entry.recent_requests.len(),
                cred_cap,
                pool_rpm = entry.rpm_window.len(),
                pool_cap,
                retry_after,
                "会话亲和命中已达 RPM 放宽上限，返回 429"
            );
            return Err(anyhow::Error::new(UpstreamRateLimitError::new(Some(
                retry_after.to_string(),
            ))));
        }

        if record_cred {
            push_in_window(&mut entry.recent_requests, now);
        }
        if record_pool {
            if pool_limit == 0 {
                // 与 `record_request` 一致：池级限流关闭时不保留窗口。
                entry.rpm_window.clear();
            } else {
                push_in_window(&mut entry.rpm_window, now);
            }
        }
        Ok(())
    }
}

/// 亲和命中允许的 RPM 上限；`limit == 0`（不限速）时返回 0 表示不设上限。
fn affinity_rpm_cap(limit: u32, multiplier: f64) -> usize {
    if limit == 0 {
        return 0;
    }
    let cap = (f64::from(limit) * multiplier.max(1.0)).floor();
    (cap as usize).max(limit as usize)
}

/// 窗口内请求数已达 `cap` 时，返回计数回落到 `cap - 1` 需等待的秒数（至少 1）；
/// 未达上限或 `cap == 0` 返回 None。
///
/// 窗口可能因之前的放行而多于 `cap`，要等最早的 `fresh - cap + 1` 条都过期，
/// 即看从新往旧数第 `cap` 条的时间戳。
fn cap_retry_after_secs(window: &VecDeque<Instant>, cap: usize, now: Instant) -> Option<u64> {
    if cap == 0 {
        return None;
    }
    let mut fresh = window
        .iter()
        .filter(|&&ts| now.duration_since(ts) < RPM_WINDOW);
    let count = fresh.clone().count();
    if count < cap {
        return None;
    }
    let release_at = *fresh.nth(count - cap)? + RPM_WINDOW;
    let remaining = release_at.saturating_duration_since(now);
    Some(
        remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
            .max(1),
    )
}

/// 清理过期时间戳后记入本次请求。
fn push_in_window(window: &mut VecDeque<Instant>, now: Instant) {
    while window
        .front()
        .is_some_and(|&ts| now.duration_since(ts) >= RPM_WINDOW)
    {
        window.pop_front();
    }
    window.push_back(now);
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{affinity_rpm_cap, cap_retry_after_secs};

    fn manager(config: Config, n: usize, rpm_limit: u32) -> MultiTokenManager {
        let creds = (0..n)
            .map(|i| KiroCredentials {
                access_token: Some(format!("t{i}")),
                expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
                rpm_limit,
                ..KiroCredentials::default()
            })
            .collect();
        MultiTokenManager::new(config, creds, None, None, false).unwrap()
    }

    async fn acquire(manager: &MultiTokenManager, session: &str) -> anyhow::Result<CallContext> {
        manager
            .acquire_context_for_session(
                None,
                None,
                None,
                &HashSet::new(),
                Some(session),
                &HashSet::new(),
            )
            .await
    }

    fn retry_after_of(err: &anyhow::Error) -> u64 {
        err.downcast_ref::<UpstreamRateLimitError>()
            .expect("应为 429 限流错误")
            .retry_after()
            .expect("应带 Retry-After")
            .parse()
            .unwrap()
    }

    fn window_ago(secs: &[u64], now: Instant) -> VecDeque<Instant> {
        secs.iter()
            .map(|&s| now - StdDuration::from_secs(s))
            .collect()
    }

    #[test]
    fn cap_is_limit_times_multiplier_and_never_below_limit() {
        assert_eq!(affinity_rpm_cap(5, 3.0), 15);
        assert_eq!(affinity_rpm_cap(5, 1.5), 7);
        assert_eq!(affinity_rpm_cap(5, 0.5), 5, "倍数小于 1 按 1 处理");
        assert_eq!(affinity_rpm_cap(0, 3.0), 0, "不限速的号不设上限");
    }

    #[test]
    fn retry_after_waits_for_cap_th_newest_to_expire() {
        let now = Instant::now();
        // 窗口 5 条、上限 3：要等最早 3 条（50s/40s/30s 前）过期，看 30s 前那条 → 30 秒
        let window = window_ago(&[50, 40, 30, 20, 10], now);
        assert_eq!(cap_retry_after_secs(&window, 3, now), Some(30));
        // 正好到上限：看最早那条
        assert_eq!(cap_retry_after_secs(&window, 5, now), Some(10));
        assert_eq!(cap_retry_after_secs(&window, 6, now), None);
        assert_eq!(cap_retry_after_secs(&window, 0, now), None);
        // 已过期的时间戳不计数
        let stale = window_ago(&[70, 65, 10], now);
        assert_eq!(cap_retry_after_secs(&stale, 2, now), None);
    }

    #[tokio::test]
    async fn affinity_hit_capped_at_three_times_cred_rpm() {
        let manager = manager(Config::default(), 2, 5);
        let first = acquire(&manager, "s1").await.unwrap();
        assert_eq!(first.affinity, AffinityOutcome::Bind);
        // 首绑占 1 个名额，再命中 14 次到 15（= 5 × 3）
        for _ in 0..14 {
            let ctx = acquire(&manager, "s1").await.unwrap();
            assert_eq!((ctx.id, ctx.affinity), (first.id, AffinityOutcome::Hit));
        }
        // 另一个号仍空闲，也不改绑：直接 429
        let Err(err) = acquire(&manager, "s1").await else {
            panic!("应被 429 拒绝")
        };
        assert!((1..=60).contains(&retry_after_of(&err)));
        let count = {
            let entries = manager.entries.lock();
            entries
                .iter()
                .find(|e| e.id == first.id)
                .unwrap()
                .recent_requests
                .len()
        };
        assert_eq!(count, 15, "被拒的请求不应写入窗口");
        assert_eq!(
            manager.session_affinity.lookup("s1", Instant::now()),
            Some(first.id)
        );
    }

    #[tokio::test]
    async fn affinity_hit_capped_at_multiplier_of_pool_rpm() {
        let mut config = Config::default();
        config.account_rpm_limit_enabled = true;
        config.account_rpm_limit = 2;
        config.session_affinity_rpm_multiplier = 2.0;
        let manager = manager(config, 1, 0);
        acquire(&manager, "s1").await.unwrap();
        for _ in 0..3 {
            acquire(&manager, "s1").await.unwrap();
        }
        let Err(err) = acquire(&manager, "s1").await else {
            panic!("应被 429 拒绝")
        };
        retry_after_of(&err);
    }

    #[tokio::test]
    async fn same_credential_retry_is_not_rejected_by_cap() {
        let manager = manager(Config::default(), 1, 1);
        let first = acquire(&manager, "s1").await.unwrap();
        for _ in 0..2 {
            acquire(&manager, "s1").await.unwrap();
        }
        // 已记过账的同号重试不占名额，放宽上限到了也不拒
        let recorded = HashSet::from([first.id]);
        let retry = manager
            .acquire_context_for_session(None, None, None, &HashSet::new(), Some("s1"), &recorded)
            .await
            .unwrap();
        assert_eq!(retry.id, first.id);
    }
}
