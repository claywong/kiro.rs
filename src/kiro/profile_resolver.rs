//! external_idp（企业 Azure 租户）账号的 profileArn 惰性解析。
//!
//! 移植自 Kiro-Go：企业 SSO 登录不返回 profileArn，token 刷新也不返回，只能在首次数据面调用前
//! 通过 CodeWhisperer 的 `ListAvailableProfiles`（携带 `TokenType: EXTERNAL_IDP` 头）单独解析。
//! 解析出的 ARN 自带 region，驱动数据面 region 选择；缓存后后续请求零往返直接命中。
//!
//! 关键点：Azure 租户账号登录时 region 默认 us-east-1，但其 profile 可能位于其它 region
//! （如 eu-central-1）。因此按候选 region 逐个探测，直到某个 region 返回 profile。
//! 只有 external_idp 或无 region 的账号才探测 fallback region；其它 auth_method 已携带权威 region。

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration as StdDuration;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// us-east-1 的 CodeWhisperer REST 主机。其它 region 没有 codewhisperer.{region} 主机，
/// 统一由 q.{region}.amazonaws.com 提供。
const KIRO_REST_HOST_US_EAST_1: &str = "codewhisperer.us-east-1.amazonaws.com";

/// 家 region 未知时探测的默认 region 集合。us-east-1 是所有登录的历史默认；
/// eu-central-1 是 EU 版 Azure 租户 profile（如 KiroProfile-eu-central-1）所在。
/// 可用 KIRO_PROFILE_REGIONS 环境变量（逗号分隔）覆盖。
const DEFAULT_KIRO_PROFILE_REGIONS: &[&str] = &["us-east-1", "eu-central-1"];

/// "unsupported" 类错误（Builder ID 不支持 profile 查询）的抑制冷却时间。
const PROFILE_ARN_UNSUPPORTED_COOLDOWN: StdDuration = StdDuration::from_secs(24 * 60 * 60);

/// 解析抑制冷却表：key -> 冷却截止时间戳（秒）。用于避免对已知不支持的账号反复探测。
fn cooldowns() -> &'static Mutex<HashMap<String, i64>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 从 profileArn 提取 region。ARN 形如
/// `arn:aws:codewhisperer:eu-central-1:123456789012:profile/XXXX`。
pub fn region_from_profile_arn(profile_arn: &str) -> Option<String> {
    let parts: Vec<&str> = profile_arn.trim().splitn(6, ':').collect();
    if parts.len() < 6 || parts[0] != "arn" || parts[2] != "codewhisperer" {
        return None;
    }
    let region = parts[3].trim();
    if region.is_empty() {
        None
    } else {
        Some(region.to_string())
    }
}

/// 判断是否为 external_idp 账号。
fn is_external_idp(credentials: &KiroCredentials) -> bool {
    credentials
        .auth_method
        .as_deref()
        .is_some_and(|m| m.eq_ignore_ascii_case("external_idp"))
}

/// 冷却表 key：优先 provider+id，回退 provider+email。
fn cooldown_key(credentials: &KiroCredentials) -> String {
    let provider = credentials.provider.as_deref().unwrap_or("").trim();
    if let Some(id) = credentials.id {
        return format!("{}\x00{}", provider, id);
    }
    let email = credentials.email.as_deref().unwrap_or("").trim();
    format!("{}\x00{}", provider, email)
}

fn suppress_resolution(credentials: &KiroCredentials) {
    let key = cooldown_key(credentials);
    let until = chrono::Utc::now().timestamp() + PROFILE_ARN_UNSUPPORTED_COOLDOWN.as_secs() as i64;
    cooldowns().lock().insert(key, until);
}

fn is_resolution_suppressed(credentials: &KiroCredentials) -> bool {
    let key = cooldown_key(credentials);
    let mut map = cooldowns().lock();
    match map.get(&key).copied() {
        Some(until) => {
            if chrono::Utc::now().timestamp() > until {
                map.remove(&key);
                false
            } else {
                true
            }
        }
        None => false,
    }
}

/// 判断账号是否需要探测 fallback region：仅 external_idp（登录时 region 默认 us-east-1）
/// 或完全无 region 的账号需要；其它 auth_method 已携带权威 region。
fn should_probe_fallback_regions(credentials: &KiroCredentials) -> bool {
    let region_empty = credentials
        .region
        .as_deref()
        .map(|r| r.trim().is_empty())
        .unwrap_or(true);
    region_empty || is_external_idp(credentials)
}

/// 返回按顺序去重的待探测 region 列表。账号当前配置的 region 永远最先。
fn profile_region_candidates(credentials: &KiroCredentials, config: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |region: &str| {
        let r = region.trim();
        if !r.is_empty() && !out.iter().any(|x| x == r) {
            out.push(r.to_string());
        }
    };

    // 账号配置的 auth region 最先
    push(credentials.effective_auth_region(config));

    if !should_probe_fallback_regions(credentials) {
        return out;
    }

    if let Ok(env) = std::env::var("KIRO_PROFILE_REGIONS") {
        let env = env.trim().to_string();
        if !env.is_empty() {
            for r in env.split(',') {
                push(r);
            }
            return out;
        }
    }
    for r in DEFAULT_KIRO_PROFILE_REGIONS {
        push(r);
    }
    out
}

/// 构造指定 region 的 ListAvailableProfiles 端点。us-east-1 用 codewhisperer REST 主机，
/// 其它 region 用 q.{region}.amazonaws.com。
fn list_profiles_endpoint(region: &str) -> String {
    let region = region.trim();
    if region.is_empty() || region == "us-east-1" {
        format!("https://{}/ListAvailableProfiles", KIRO_REST_HOST_US_EAST_1)
    } else {
        format!("https://q.{}.amazonaws.com/ListAvailableProfiles", region)
    }
}

#[derive(serde::Deserialize)]
struct ListProfilesResponse {
    #[serde(default)]
    profiles: Vec<ProfileEntry>,
}

#[derive(serde::Deserialize)]
struct ProfileEntry {
    #[serde(default)]
    arn: String,
}

/// ListAvailableProfiles 探测的错误分类，用于决定是否重试。
enum ProbeError {
    /// 空 profile 列表或 4xx（非 429）——账号状态，不重试
    Authoritative(String),
    /// 网络错误 / 5xx / 429——瞬时，可重试
    Transient(String),
    /// Builder ID 不支持（403 特定消息）——跨 region 权威，短路
    Unsupported(String),
}

impl ProbeError {
    fn message(&self) -> &str {
        match self {
            ProbeError::Authoritative(m) | ProbeError::Transient(m) | ProbeError::Unsupported(m) => {
                m
            }
        }
    }
}

/// 对单个 region 调用一次 ListAvailableProfiles。
async fn list_profiles_in_region(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> Result<String, ProbeError> {
    let endpoint = list_profiles_endpoint(region);
    let host = endpoint
        .strip_prefix("https://")
        .and_then(|s| s.split('/').next())
        .unwrap_or("")
        .to_string();

    let machine_id = crate::kiro::machine_id::generate_from_credentials(credentials, config)
        .unwrap_or_default();

    let client = build_client(proxy, 60, config.tls_backend)
        .map_err(|e| ProbeError::Transient(format!("构建 HTTP 客户端失败: {}", e)))?;

    let is_builder_id = credentials
        .provider
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case("BuilderId"));

    let mut req = client
        .post(&endpoint)
        .body(r#"{"maxResults":10}"#)
        .header("content-type", "application/json")
        .header("Accept", "application/json")
        .header(
            "x-amz-user-agent",
            format!(
                "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
                config.kiro_version, machine_id
            ),
        )
        .header(
            "user-agent",
            format!(
                "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
                config.system_version, config.node_version, config.kiro_version, machine_id
            ),
        )
        .header("host", host)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close");

    if is_external_idp(credentials) {
        req = req.header("TokenType", "EXTERNAL_IDP");
    }

    let resp = req
        .send()
        .await
        .map_err(|e| ProbeError::Transient(format!("请求失败: {}", e)))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let msg = format!("HTTP {}: {}", status.as_u16(), body);
        // Builder ID 不支持是跨 region 权威
        if is_builder_id
            && status.as_u16() == 403
            && body.contains("AWS Builder ID is not supported for this operation")
        {
            return Err(ProbeError::Unsupported(msg));
        }
        // 5xx / 429 瞬时可重试，其余 4xx 权威
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(ProbeError::Transient(msg));
        }
        return Err(ProbeError::Authoritative(msg));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| ProbeError::Transient(format!("读取响应失败: {}", e)))?;
    let parsed: ListProfilesResponse = serde_json::from_str(&body)
        .map_err(|e| ProbeError::Authoritative(format!("解析 profile 列表失败: {}", e)))?;
    for p in parsed.profiles {
        let arn = p.arn.trim();
        if !arn.is_empty() {
            return Ok(arn.to_string());
        }
    }
    Err(ProbeError::Authoritative("empty profile list".to_string()))
}

/// 对单个 region 带退避重试 ListAvailableProfiles（瞬时错误重试，权威错误立即返回）。
async fn list_profiles_with_retry(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
    region: &str,
) -> Result<String, ProbeError> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut backoff = StdDuration::from_millis(200);
    let mut last: Option<ProbeError> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        match list_profiles_in_region(credentials, config, token, proxy, region).await {
            Ok(arn) => return Ok(arn),
            Err(e) => {
                let transient = matches!(e, ProbeError::Transient(_));
                last = Some(e);
                if !transient || attempt == MAX_ATTEMPTS {
                    return Err(last.unwrap());
                }
                tracing::debug!(
                    "[ProfileArn] ListAvailableProfiles 瞬时失败 region={} (第 {}/{} 次): {}",
                    region,
                    attempt,
                    MAX_ATTEMPTS,
                    last.as_ref().unwrap().message()
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    Err(last.unwrap())
}

/// 跨候选 region 探测 profileArn，返回首个找到的。
/// Builder ID "unsupported" 403 跨 region 权威，短路。
async fn resolve_across_regions(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Result<String, ProbeError> {
    let mut last: Option<ProbeError> = None;
    for region in profile_region_candidates(credentials, config) {
        match list_profiles_with_retry(credentials, config, token, proxy, &region).await {
            Ok(arn) if !arn.trim().is_empty() => return Ok(arn),
            Ok(_) => {}
            Err(e) => {
                if matches!(e, ProbeError::Unsupported(_)) {
                    return Err(e);
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| ProbeError::Authoritative("no available Kiro profile".to_string())))
}

/// 解析账号的 profileArn（若已有则直接返回）。首选 ListAvailableProfiles 跨 region 探测。
/// 返回 `Ok(Some(arn))` 表示解析成功；`Ok(None)` 表示软失败（不支持 / 被抑制 / 空列表），
/// 调用方应继续无 profileArn 的请求；`Err` 仅用于不应吞掉的硬错误（当前不产生）。
pub async fn resolve_profile_arn(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> Option<String> {
    if let Some(arn) = credentials.profile_arn.as_deref() {
        let arn = arn.trim();
        if !arn.is_empty() {
            return Some(arn.to_string());
        }
    }

    if is_resolution_suppressed(credentials) {
        tracing::debug!("[ProfileArn] 解析被抑制（此前 Builder ID 查询不支持），跳过");
        return None;
    }

    match resolve_across_regions(credentials, config, token, proxy).await {
        Ok(arn) => Some(arn),
        Err(ProbeError::Unsupported(msg)) => {
            suppress_resolution(credentials);
            tracing::debug!("[ProfileArn] Builder ID profile 查询不支持: {}", msg);
            None
        }
        Err(e) => {
            tracing::warn!("[ProfileArn] 解析 profileArn 失败: {}", e.message());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_from_profile_arn() {
        assert_eq!(
            region_from_profile_arn(
                "arn:aws:codewhisperer:eu-central-1:123456789012:profile/ABC"
            ),
            Some("eu-central-1".to_string())
        );
        assert_eq!(region_from_profile_arn("not-an-arn"), None);
        assert_eq!(region_from_profile_arn(""), None);
    }

    #[test]
    fn test_list_profiles_endpoint() {
        assert_eq!(
            list_profiles_endpoint("us-east-1"),
            "https://codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles"
        );
        assert_eq!(
            list_profiles_endpoint("eu-central-1"),
            "https://q.eu-central-1.amazonaws.com/ListAvailableProfiles"
        );
    }
}
