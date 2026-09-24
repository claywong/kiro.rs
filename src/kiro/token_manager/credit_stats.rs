//! 本站凭据累计 credit 统计。
//! @author wangzhong

use super::MultiTokenManager;

impl MultiTokenManager {
    /// 累计上游实际计量值；沿用成功次数的持久化与退出刷盘机制。
    pub(crate) fn record_local_credits(&self, id: u64, credits: f64) {
        if id == 0 || !credits.is_finite() || credits <= 0.0 {
            return;
        }
        {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                return;
            };
            let total = entry.total_credits + credits;
            if !total.is_finite() {
                return;
            }
            entry.total_credits = total;
        }
        self.save_stats_debounced();
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn manager(path: Option<PathBuf>) -> MultiTokenManager {
        MultiTokenManager::new(
            Config::default(),
            vec![
                KiroCredentials {
                    id: Some(1),
                    ..Default::default()
                },
                KiroCredentials {
                    id: Some(2),
                    ..Default::default()
                },
            ],
            None,
            path,
            false,
        )
        .unwrap()
    }

    #[test]
    fn credit_stats_load_legacy_file_and_survive_restart() {
        let dir = std::env::temp_dir().join(format!("kiro-credit-stats-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.json");
        std::fs::write(
            dir.join("kiro_stats.json"),
            r#"{"1":{"success_count":7,"last_used_at":null}}"#,
        )
        .unwrap();
        {
            let manager = manager(Some(path.clone()));
            assert_eq!(manager.snapshot().entries[0].total_credits, 0.0);
            assert_eq!(manager.snapshot().entries[0].success_count, 7);
            manager.record_local_credits(1, 1.25);
            manager.record_local_credits(1, 0.5);
        }
        {
            let manager = manager(Some(path));
            assert_eq!(manager.snapshot().entries[0].total_credits, 1.75);
            assert_eq!(manager.snapshot().entries[0].success_count, 7);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn credit_stats_survive_serialization() {
        let stats: StatsEntry = serde_json::from_value(serde_json::json!({
            "success_count": 3,
            "last_used_at": null,
            "total_credits": 1.25
        }))
        .unwrap();

        let saved = serde_json::to_value(stats).unwrap();

        assert_eq!(saved["total_credits"], 1.25);
    }
}
