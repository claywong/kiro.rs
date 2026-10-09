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
}

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
