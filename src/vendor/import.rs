//! 卖家 Key 入库的共用实现
//!
//! 主卖家与次级卖家（kiroapp）拿到 Key 后的入库动作完全一致：复用 admin 的
//! `import_one_credential`，去重 / 验活 / 失败回滚与批量导入同一套逻辑。
//! 两家的差异只在分组与 RPM 默认值，故提成参数由调用方给。
//!
//! @author wangzhong

use std::sync::Arc;

use crate::admin::AdminService;
use crate::admin::types::AddCredentialRequest;

use super::store::PurchaseOutcome;

/// 把 `ksk_` Key 逐条入库。
///
/// `source_channel` 写成 `vendor:<order_id>` 形式，用于事后追溯这批 Key 的来源；
/// kiroapp 无订单号，由调用方传入自己的标识。
pub async fn import_keys(
    admin: &Arc<AdminService>,
    keys: Vec<String>,
    source_channel: &str,
    groups: Vec<String>,
    rpm_limit: u32,
) -> PurchaseOutcome {
    let mut outcome = PurchaseOutcome::default();
    for key in keys {
        let req = AddCredentialRequest {
            refresh_token: None,
            access_token: None,
            profile_arn: None,
            expires_at: None,
            auth_method: "api_key".to_string(),
            provider: None,
            client_id: None,
            client_secret: None,
            start_url: None,
            token_endpoint: None,
            issuer_url: None,
            scopes: None,
            priority: 0,
            rpm_limit,
            region: None,
            auth_region: None,
            api_region: None,
            machine_id: None,
            email: None,
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            kiro_api_key: Some(key),
            endpoint: None,
            groups: groups.clone(),
            source_channel: Some(source_channel.to_string()),
        };

        let result = admin.import_one_credential(req, true).await;
        use crate::admin::ImportStatus;
        match result.status {
            ImportStatus::Verified | ImportStatus::Imported => outcome.imported += 1,
            ImportStatus::Duplicate => outcome.duplicated += 1,
            ImportStatus::Failed => {
                outcome.failed += 1;
                if outcome.last_error.is_none() {
                    outcome.last_error = result.error.clone();
                }
            }
        }
    }
    outcome
}
