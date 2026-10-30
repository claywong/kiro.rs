//! 会话亲和总开关（本地运维便利特性）
//!
//! `session_affinity_enabled` 原本只在启动时读一次 config，运行期改不了。这里把它做成
//! 运行时可切换的开关，并经 Admin API / 设置页暴露。
//!
//! # 语义
//! - **关闭只是让 `acquire_context_for_session` 忽略 session_key**：不绑定、不命中，命中后才有的
//!   RPM 放宽与 429 原号重试随之失效。
//! - **关闭不清空绑定表**：已有绑定留着，靠 TTL 自然过期。TTL 内重新打开，旧绑定继续生效，
//!   对上游 prompt cache 有利。若需要干净状态，等 TTL 过期或重启进程。
//! - 持久化失败则回滚内存值，与 `set_account_throttle_config` 一致。
//!
//! 亲和 RPM 放宽倍数、触顶后的短等上限与溢出开关（见 `affinity_rpm.rs`）也在这里运行期可调，
//! 语义相同。
//!
//! @author wangzhong

use std::sync::atomic::Ordering;

use super::MultiTokenManager;

impl MultiTokenManager {
    /// 会话亲和总开关当前是否开启
    pub fn is_session_affinity_enabled(&self) -> bool {
        self.session_affinity_enabled.load(Ordering::Relaxed)
    }

    /// 设置会话亲和总开关（Admin API）。值未变化时直接返回，不写配置文件。
    pub fn set_session_affinity_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let _update_guard = self.runtime_config_update_lock.lock();

        let previous = self.is_session_affinity_enabled();
        if previous == enabled {
            return Ok(());
        }

        self.session_affinity_enabled
            .store(enabled, Ordering::Relaxed);

        if let Err(err) =
            self.update_config_file(move |config| config.session_affinity_enabled = enabled)
        {
            self.session_affinity_enabled
                .store(previous, Ordering::Relaxed);
            return Err(err);
        }

        tracing::info!(
            "会话亲和总开关已{}（现有绑定保留，按 TTL 自然过期）",
            if enabled { "开启" } else { "关闭" }
        );
        Ok(())
    }

    /// 亲和命中时 RPM 允许突破到的倍数
    pub fn session_affinity_rpm_multiplier(&self) -> f64 {
        f64::from_bits(self.session_affinity_rpm_multiplier_bits.load(Ordering::Relaxed))
    }

    /// 亲和触顶时服务端等待名额释放的上限（毫秒）
    pub fn session_affinity_cap_max_wait_ms(&self) -> u64 {
        self.session_affinity_cap_max_wait_ms.load(Ordering::Relaxed)
    }

    /// 亲和触顶且无法短等时是否溢出到其它凭据
    pub fn is_session_affinity_overflow_enabled(&self) -> bool {
        self.session_affinity_overflow_enabled.load(Ordering::Relaxed)
    }

    /// 设置亲和 RPM 放宽倍数与触顶处理（Admin API），`None` 表示该项不变。
    /// 调用方负责按 [`SESSION_AFFINITY_RPM_MULTIPLIER_RANGE`] /
    /// [`SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS`] 校验。
    pub fn set_session_affinity_cap_config(
        &self,
        rpm_multiplier: Option<f64>,
        max_wait_ms: Option<u64>,
        overflow_enabled: Option<bool>,
    ) -> anyhow::Result<()> {
        let _update_guard = self.runtime_config_update_lock.lock();

        let prev = (
            self.session_affinity_rpm_multiplier(),
            self.session_affinity_cap_max_wait_ms(),
            self.is_session_affinity_overflow_enabled(),
        );
        let next = (
            rpm_multiplier.unwrap_or(prev.0),
            max_wait_ms.unwrap_or(prev.1),
            overflow_enabled.unwrap_or(prev.2),
        );
        if next == prev {
            return Ok(());
        }

        self.store_session_affinity_cap_config(next);
        let (multiplier, wait, overflow) = next;
        if let Err(err) = self.update_config_file(move |config| {
            config.session_affinity_rpm_multiplier = multiplier;
            config.session_affinity_cap_max_wait_ms = wait;
            config.session_affinity_overflow_enabled = overflow;
        }) {
            self.store_session_affinity_cap_config(prev);
            return Err(err);
        }

        tracing::info!(
            rpm_multiplier = multiplier,
            max_wait_ms = wait,
            overflow_enabled = overflow,
            "会话亲和 RPM 放宽与触顶处理已更新"
        );
        Ok(())
    }

    fn store_session_affinity_cap_config(&self, (multiplier, wait, overflow): (f64, u64, bool)) {
        self.session_affinity_rpm_multiplier_bits
            .store(multiplier.to_bits(), Ordering::Relaxed);
        self.session_affinity_cap_max_wait_ms
            .store(wait, Ordering::Relaxed);
        self.session_affinity_overflow_enabled
            .store(overflow, Ordering::Relaxed);
    }
}

/// 亲和触顶短等上限的最大可设值（毫秒）：等满一个 RPM 窗口（60s）必然有名额，再长没有意义
pub const SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS: u64 = 60_000;

/// 亲和 RPM 放宽倍数的可设区间（1 = 不放宽）
pub const SESSION_AFFINITY_RPM_MULTIPLIER_RANGE: std::ops::RangeInclusive<f64> = 1.0..=10.0;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::{Duration, Utc};

    use super::super::{AffinityOutcome, CallContext, MultiTokenManager};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;

    fn manager_with(config: Config, n: usize) -> MultiTokenManager {
        let creds = (0..n)
            .map(|i| KiroCredentials {
                access_token: Some(format!("t{i}")),
                expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
                ..KiroCredentials::default()
            })
            .collect();
        MultiTokenManager::new(config, creds, None, None, false).unwrap()
    }

    async fn acquire(manager: &MultiTokenManager, session: &str) -> CallContext {
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
            .unwrap()
    }

    #[test]
    fn switch_follows_config_default_and_toggles() {
        let manager = manager_with(Config::default(), 1);
        assert!(manager.is_session_affinity_enabled());

        manager.set_session_affinity_enabled(false).unwrap();
        assert!(!manager.is_session_affinity_enabled());

        manager.set_session_affinity_enabled(true).unwrap();
        assert!(manager.is_session_affinity_enabled());
    }

    /// 关闭后不再绑定 / 命中，且不清空绑定表；TTL 内重新打开，旧绑定继续命中。
    #[tokio::test]
    async fn disable_keeps_bindings_and_reenable_resumes_them() {
        let manager = manager_with(Config::default(), 3);

        let first = acquire(&manager, "s1").await;
        assert_eq!(first.affinity, AffinityOutcome::Bind);
        assert_eq!(manager.session_affinity.len(), 1);

        manager.set_session_affinity_enabled(false).unwrap();
        let while_off = acquire(&manager, "s1").await;
        assert_eq!(while_off.affinity, AffinityOutcome::None, "关闭期间不命中");
        assert_eq!(manager.session_affinity.len(), 1, "关闭不清空绑定表");
        let new_while_off = acquire(&manager, "s2").await;
        assert_eq!(new_while_off.affinity, AffinityOutcome::None, "关闭期间不新增绑定");
        assert_eq!(manager.session_affinity.len(), 1);

        manager.set_session_affinity_enabled(true).unwrap();
        let resumed = acquire(&manager, "s1").await;
        assert_eq!(resumed.affinity, AffinityOutcome::Hit, "重开后旧绑定恢复命中");
        assert_eq!(resumed.id, first.id);
    }

    #[test]
    fn cap_handling_follows_config_and_persists() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-affinity-cap-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"sessionAffinityCapMaxWaitMs":1500}"#).unwrap();

        let manager = manager_with(Config::load(&config_path).unwrap(), 1);
        assert_eq!(manager.session_affinity_cap_max_wait_ms(), 1500);
        assert!(manager.is_session_affinity_overflow_enabled(), "溢出默认开启");
        assert_eq!(manager.session_affinity_rpm_multiplier(), 3.0, "倍数默认 3");

        // 只改一项，其它保持
        manager.set_session_affinity_cap_config(None, None, Some(false)).unwrap();
        assert_eq!(manager.session_affinity_cap_max_wait_ms(), 1500);
        assert!(!manager.is_session_affinity_overflow_enabled());

        manager.set_session_affinity_cap_config(Some(1.5), Some(0), None).unwrap();
        assert_eq!(manager.session_affinity_rpm_multiplier(), 1.5);
        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.session_affinity_rpm_multiplier, 1.5);
        assert_eq!(persisted.session_affinity_cap_max_wait_ms, 0);
        assert!(!persisted.session_affinity_overflow_enabled);

        std::fs::remove_file(&config_path).unwrap();
    }

    #[test]
    fn switch_persists_to_config_file() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-affinity-switch-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"sessionAffinityEnabled":true}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager = manager_with(config, 1);

        manager.set_session_affinity_enabled(false).unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert!(!persisted.session_affinity_enabled);
        assert!(!manager.is_session_affinity_enabled());

        std::fs::remove_file(&config_path).unwrap();
    }
}
