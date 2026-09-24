//! 多轮搜索换号时的累计 credit 归属验证。
//! @author wangzhong
use super::*;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

#[test]
fn credit_stats_search_rounds_charge_actual_credentials_once_on_cancel() {
    let manager = Arc::new(
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
        started_at: std::time::Instant::now(),
        tracer: None,
    };

    {
        let mut settlement = WebSearchUsageSettlement::without_trace(hook);
        settlement.add(1, TokenUsage::default(), 0.25);
        settlement.add(2, TokenUsage::default(), 0.5);
        settlement.add(1, TokenUsage::default(), 1.0);
        // 模拟中断触发 Drop 结算，不得再将总额重复计到最后一个凭据。
    }

    let snapshot = manager.snapshot();
    assert_eq!(snapshot.entries[0].total_credits, 1.25);
    assert_eq!(snapshot.entries[1].total_credits, 0.5);
}
