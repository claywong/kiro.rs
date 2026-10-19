//! 凭据使用率排序键：已用 credit / 套餐总 credit。
//!
//! 同层按「消耗」均衡时，success_count 只是次数，不同套餐额度差别很大；
//! 使用率更能反映账号离用完还有多远。已用值取自 `credit_usage_cache`（traces.db 汇总，
//! 每分钟刷新），总额由订阅等级决定。
//!
//! 本模块是本地调度策略，上游不涉及，独立成文件以减少与 token_manager.rs 的冲突。
//! @author wangzhong

use std::collections::HashMap;

use crate::kiro::model::credentials::KiroCredentials;

/// 凭据 id → 使用率千分比（0~1000）。缺失视为 0。
pub(super) type UsageKeys = HashMap<u64, u32>;

pub(super) fn usage_key_of(keys: &UsageKeys, id: u64) -> u32 {
    keys.get(&id).copied().unwrap_or(0)
}

/// 订阅等级对应的总 credit。无法识别的等级返回 None。
fn plan_credit_quota(title: Option<&str>) -> Option<f64> {
    let upper = title?.to_uppercase();
    if upper.contains("FREE") {
        None
    } else if upper.contains("MAX") {
        Some(5000.0)
    } else if upper.contains("POWER") {
        Some(10000.0)
    } else if upper.contains("PRO+") || upper.contains("PRO PLUS") || upper.contains("PRO_PLUS") {
        Some(2000.0)
    } else if upper.contains("PRO") {
        Some(1000.0)
    } else {
        None
    }
}

/// 凭据的总 credit：优先按订阅等级，其次回退到人工设置的 `credit_limit`。
fn total_quota(credentials: &KiroCredentials) -> Option<f64> {
    plan_credit_quota(credentials.subscription_title.as_deref()).or_else(|| {
        credentials
            .credit_limit
            .filter(|limit| *limit > 0.0 && limit.is_finite())
    })
}

/// 使用率千分比，上限 1000。总额或已用未知时返回 0（视为全新，排在前面）。
pub(super) fn usage_permille(credentials: &KiroCredentials, used: Option<f64>) -> u32 {
    let (Some(total), Some(used)) = (total_quota(credentials), used) else {
        return 0;
    };
    if !used.is_finite() || used <= 0.0 {
        return 0;
    }
    ((used / total) * 1000.0).round().clamp(0.0, 1000.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(title: Option<&str>, limit: Option<f64>) -> KiroCredentials {
        KiroCredentials {
            subscription_title: title.map(str::to_string),
            credit_limit: limit,
            ..Default::default()
        }
    }

    #[test]
    fn quota_by_plan() {
        assert_eq!(plan_credit_quota(Some("KIRO PRO")), Some(1000.0));
        assert_eq!(plan_credit_quota(Some("KIRO PRO+")), Some(2000.0));
        assert_eq!(plan_credit_quota(Some("KIRO PRO MAX")), Some(5000.0));
        assert_eq!(plan_credit_quota(Some("KIRO POWER")), Some(10000.0));
        assert_eq!(plan_credit_quota(Some("KIRO FREE")), None);
        assert_eq!(plan_credit_quota(None), None);
    }

    #[test]
    fn permille_uses_plan_then_credit_limit() {
        assert_eq!(usage_permille(&cred(Some("KIRO PRO"), None), Some(250.0)), 250);
        assert_eq!(usage_permille(&cred(Some("KIRO POWER"), None), Some(250.0)), 25);
        // 等级未知时回退 credit_limit
        assert_eq!(usage_permille(&cred(None, Some(4000.0)), Some(1000.0)), 250);
        // 等级优先于 credit_limit
        assert_eq!(usage_permille(&cred(Some("KIRO PRO"), Some(4000.0)), Some(500.0)), 500);
    }

    fn manager_with(titles: &[&str]) -> super::super::MultiTokenManager {
        let creds = titles
            .iter()
            .enumerate()
            .map(|(i, t)| KiroCredentials {
                id: Some(i as u64 + 1),
                subscription_title: Some(t.to_string()),
                ..Default::default()
            })
            .collect();
        super::super::MultiTokenManager::new(
            crate::model::config::Config::default(),
            creds,
            None,
            None,
            false,
        )
        .unwrap()
    }

    #[test]
    fn select_prefers_lower_usage_ratio_over_success_count() {
        // A(PRO, 已用 800/1000=80%)  B(POWER, 已用 1500/10000=15%)
        let m = manager_with(&["KIRO PRO", "KIRO POWER"]);
        m.credit_usage_cache.lock().extend([(1, 800.0), (2, 1500.0)]);
        // B 的成功次数更多，但使用率更低，应优先选 B
        m.report_success(2);
        m.report_success(2);
        let pick = m.select_next_credential(None, None).map(|(id, _)| id);
        assert_eq!(pick, Some(2));
    }

    #[test]
    fn select_falls_back_to_success_count_without_usage_data() {
        let m = manager_with(&["KIRO PRO", "KIRO PRO"]);
        m.report_success(1);
        let pick = m.select_next_credential(None, None).map(|(id, _)| id);
        assert_eq!(pick, Some(2), "无使用数据时仍按 success_count 最少");
    }

    #[test]
    fn permille_defaults_to_zero_and_clamps() {
        assert_eq!(usage_permille(&cred(None, None), Some(500.0)), 0);
        assert_eq!(usage_permille(&cred(Some("KIRO PRO"), None), None), 0);
        assert_eq!(usage_permille(&cred(Some("KIRO PRO"), None), Some(5000.0)), 1000);
        assert_eq!(usage_permille(&cred(Some("KIRO PRO"), None), Some(f64::NAN)), 0);
    }
}
