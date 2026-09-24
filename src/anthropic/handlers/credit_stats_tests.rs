//! 累计 credit 的请求结算回归测试。
//! @author wangzhong
use super::*;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

#[test]
fn credit_stats_accumulate_actual_usage_per_credential_without_logs() {
    let manager = std::sync::Arc::new(
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
            None,
            false,
        )
        .unwrap(),
    );
    let hook = UsageRecordHook {
        recorder: None,
        aggregator: None,
        client_keys: None,
        credit_token_manager: Some(manager.clone()),
        key_id: 0,
        model: "test-model".into(),
        effort: None,
        started_at: Instant::now(),
        tracer: None,
    };

    hook.record(1, 0, 0, 0, 0, 0.25, "success");
    hook.record(1, 0, 0, 0, 0, 0.5, "error");
    hook.record(2, 0, 0, 0, 0, 2.0, "success");
    for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
        hook.record(1, 0, 0, 0, 0, invalid, "error");
    }
    hook.record(999, 0, 0, 0, 0, 10.0, "success");
    manager.report_success(1);
    manager.reset_success_count(None).unwrap();

    let snapshot = manager.snapshot();
    assert_eq!(snapshot.entries[0].total_credits, 0.75);
    assert_eq!(snapshot.entries[1].total_credits, 2.0);
    assert_eq!(snapshot.entries[0].success_count, 0);
}
