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
use serde_json::{Map, Value};

use super::protocol::PurchasedKey;
use super::store::PurchaseOutcome;

pub(crate) const MANUAL_REVIEW_ERROR: &str =
    "凭证既不是 ksk_ API Key，也不包含有效 refreshToken，已保存订单并转人工处理";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedVendorCredential {
    ApiKey(String),
    Social {
        refresh_token: String,
        access_token: Option<String>,
        profile_arn: Option<String>,
        expires_at: Option<String>,
        provider: Option<String>,
        auth_region: Option<String>,
        machine_id: Option<String>,
        email: Option<String>,
    },
}

/// 卖家自动入库目前只支持 Kiro API Key。
///
/// 前后空白由卖家协议层和此处共同容错，但前缀保持大小写敏感，避免把账号、密码、
/// OAuth JSON 等其它交付内容误写进 `kiroApiKey`。
pub(crate) fn is_kiro_api_key(value: &str) -> bool {
    value.trim().starts_with("ksk_")
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_social_credential(value: &Value) -> Option<ParsedVendorCredential> {
    let object = value.as_object()?;
    Some(ParsedVendorCredential::Social {
        refresh_token: optional_string(object, "refreshToken")?,
        access_token: optional_string(object, "accessToken"),
        profile_arn: optional_string(object, "profileArn"),
        expires_at: optional_string(object, "expiresAt"),
        provider: optional_string(object, "provider"),
        auth_region: optional_string(object, "authRegion")
            .or_else(|| optional_string(object, "region")),
        machine_id: optional_string(object, "machineId"),
        email: optional_string(object, "email"),
    })
}

/// 识别卖家交付内容：`ksk_` 是 API Key；JSON 中的 `refreshToken` 是 Social。
///
/// JSON 支持单账号对象、账号数组和 `{ "accounts": [...] }`。一个 payload 中只要有
/// 任一条无法识别，就整体转人工，避免静默丢掉数组里的部分账号。
pub(crate) fn parse_vendor_credentials(value: &str) -> Option<Vec<ParsedVendorCredential>> {
    let trimmed = value.trim();
    if is_kiro_api_key(trimmed) {
        return Some(vec![ParsedVendorCredential::ApiKey(trimmed.to_string())]);
    }

    let parsed: Value = serde_json::from_str(trimmed).ok()?;
    let items: Vec<&Value> = match &parsed {
        Value::Array(items) => items.iter().collect(),
        Value::Object(object) => match object.get("accounts") {
            Some(Value::Array(items)) => items.iter().collect(),
            Some(_) => return None,
            None => vec![&parsed],
        },
        _ => return None,
    };
    if items.is_empty() {
        return None;
    }

    items.into_iter().map(parse_social_credential).collect()
}

/// 把卖家交付的 `ksk_` API Key / Social refreshToken 逐条入库。
///
/// `source_channel` 写成 `vendor:<order_id>` 形式，用于事后追溯这批 Key 的来源；
/// kiroapp 无订单号，由调用方传入自己的标识。
/// `api_region` 是**订单级**成交区域（如 `eu-central-1`）。单张卡自带区域时
/// （[`PurchasedKey::region`]）以卡上的为准 —— kiro.red 的双区混发商品同一单里
/// 各张卡分属不同区，用订单级区域会让一半凭证连错端点、报凭证失效。
/// `priority` 是调度优先级，**数值越小越优先**；由调用方按家给，见
/// [`VendorConfig::effective_default_priority`](crate::model::config::VendorConfig::effective_default_priority)。
/// `credit_limit` 是 credit 使用上限（美元），None 表示不限制。
pub async fn import_keys(
    admin: &Arc<AdminService>,
    keys: Vec<PurchasedKey>,
    source_channel: &str,
    groups: Vec<String>,
    rpm_limit: u32,
    api_region: Option<String>,
    priority: u32,
    credit_limit: Option<f64>,
) -> PurchaseOutcome {
    let mut outcome = PurchaseOutcome::default();
    for pk in keys {
        let Some(credentials) = parse_vendor_credentials(&pk.key) else {
            outcome.failed += 1;
            if outcome.last_error.is_none() {
                outcome.last_error = Some(MANUAL_REVIEW_ERROR.to_string());
            }
            continue;
        };
        // 卡上带区就用卡上的，否则回落订单级
        let region_for_key = pk
            .region
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| api_region.clone());

        for credential in credentials {
            let req = match credential {
                ParsedVendorCredential::ApiKey(key) => AddCredentialRequest {
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
                    priority,
                    rpm_limit,
                    credit_limit,
                    region: None,
                    auth_region: None,
                    api_region: region_for_key.clone(),
                    machine_id: None,
                    email: None,
                    proxy_url: None,
                    proxy_username: None,
                    proxy_password: None,
                    kiro_api_key: Some(key),
                    endpoint: None,
                    groups: groups.clone(),
                    source_channel: Some(source_channel.to_string()),
                },
                ParsedVendorCredential::Social {
                    refresh_token,
                    access_token,
                    profile_arn,
                    expires_at,
                    provider,
                    auth_region,
                    machine_id,
                    email,
                } => AddCredentialRequest {
                    refresh_token: Some(refresh_token),
                    access_token,
                    profile_arn,
                    expires_at,
                    auth_method: "social".to_string(),
                    provider,
                    client_id: None,
                    client_secret: None,
                    start_url: None,
                    token_endpoint: None,
                    issuer_url: None,
                    scopes: None,
                    priority,
                    rpm_limit,
                    credit_limit,
                    region: None,
                    auth_region,
                    api_region: region_for_key.clone(),
                    machine_id,
                    email,
                    proxy_url: None,
                    proxy_username: None,
                    proxy_password: None,
                    kiro_api_key: None,
                    endpoint: None,
                    groups: groups.clone(),
                    source_channel: Some(source_channel.to_string()),
                },
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
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::{ParsedVendorCredential, is_kiro_api_key, parse_vendor_credentials};

    #[test]
    fn 只接受去除空白后以ksk前缀开头的凭证() {
        assert!(is_kiro_api_key("ksk_live_abc"));
        assert!(is_kiro_api_key("  ksk_live_abc\n"));

        assert!(!is_kiro_api_key(""));
        assert!(!is_kiro_api_key("KSK_live_abc"));
        assert!(!is_kiro_api_key("aor_refresh_token"));
        assert!(!is_kiro_api_key(r#"{"refreshToken":"aor_x"}"#));
    }

    #[test]
    fn refresh_token数组识别为social并保留展示字段() {
        let parsed = parse_vendor_credentials(
            r#"[{"email":"person@example.com","refreshToken":"aor_test","provider":"Github"}]"#,
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec![ParsedVendorCredential::Social {
                refresh_token: "aor_test".to_string(),
                access_token: None,
                profile_arn: None,
                expires_at: None,
                provider: Some("Github".to_string()),
                auth_region: None,
                machine_id: None,
                email: Some("person@example.com".to_string()),
            }]
        );
    }

    #[test]
    fn social对象和accounts包装均可识别() {
        assert!(parse_vendor_credentials(r#"{"refreshToken":"aor_one"}"#).is_some());
        assert!(
            parse_vendor_credentials(
                r#"{"accounts":[{"refreshToken":"aor_two","provider":"Google"}]}"#
            )
            .is_some()
        );
    }

    #[test]
    fn 缺refresh_token或数组部分无效时整体转人工() {
        assert!(parse_vendor_credentials("account----password").is_none());
        assert!(parse_vendor_credentials(r#"{"email":"person@example.com"}"#).is_none());
        assert!(
            parse_vendor_credentials(
                r#"[{"refreshToken":"aor_ok"},{"email":"missing@example.com"}]"#
            )
            .is_none()
        );
    }
}
