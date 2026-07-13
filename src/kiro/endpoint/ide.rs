//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext, UsageRequestParts};
use crate::kiro::model::credentials::KiroCredentials;

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }

    fn is_aws_sso_oidc_credentials(credentials: &KiroCredentials) -> bool {
        let auth_method = credentials.auth_method.as_deref();
        matches!(auth_method, Some("builder-id") | Some("idc"))
            || (credentials.client_id.is_some() && credentials.client_secret.is_some())
    }

    /// 外部 IdP（企业 SSO，如 Azure AD）token 必须携带 TokenType: EXTERNAL_IDP 头，
    /// 否则 CodeWhisperer 不识别 token 类型，会静默返回空 profile 列表并拒绝数据面调用。
    /// 携带后，已开通的账号能解析出 profile，未开通的会得到明确的 403。
    fn is_external_idp_credentials(credentials: &KiroCredentials) -> bool {
        credentials
            .auth_method
            .as_deref()
            .is_some_and(|m| m.eq_ignore_ascii_case("external_idp"))
    }

    /// 决定请求实际应携带的 profileArn。
    ///
    /// 规则（对齐 chaogei/Kiro-account-manager 参考实现）：
    /// - 凭据已有真实 profileArn（social 换取自带 / IdC 经 ListAvailableProfiles 解析）→ 使用它
    /// - 无 profileArn（如未解析的 builder-id）→ 返回 None，不发送
    ///
    /// 关键修正：IdC / Enterprise 账号一旦解析出真实 profileArn，数据面与额度端点
    /// 都必须携带，否则 CodeWhisperer 返回 403 "User is not authorized to make this call."。
    /// 旧逻辑对所有 SSO OIDC 凭据无条件剥离 profileArn，导致企业租户账号全线 403。
    fn mcp_profile_arn_header_value(credentials: &KiroCredentials) -> Option<&str> {
        // AWS SSO OIDC（builder-id / idc / 通过 client credentials 换取）凭据不携带 profileArn，
        // 由服务端按 token 自行解析。external_idp 走独立的 EXTERNAL_IDP 分支，不受此约束。
        if Self::is_aws_sso_oidc_credentials(credentials) {
            return None;
        }
        credentials.profile_arn.as_deref().filter(|s| !s.is_empty())
    }

    fn inject_profile_arn(
        request_body: &str,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<String> {
        // AWS SSO OIDC 凭据：剥离 body 中残留的 profileArn，交由服务端按 token 解析。
        if Self::is_aws_sso_oidc_credentials(credentials) {
            let mut request: serde_json::Value = serde_json::from_str(request_body)?;
            if let Some(obj) = request.as_object_mut() {
                obj.remove("profileArn");
            }
            return Ok(serde_json::to_string(&request)?);
        }

        // 非 SSO OIDC 凭据且无真实 profileArn。
        let Some(profile_arn) = Self::mcp_profile_arn_header_value(credentials) else {
            // 已显式声明 auth_method 的账号（如 social/personal 自带 profile）：body 原样透传。
            // 未声明 auth_method 的凭据：移除 body 里残留的占位符 profileArn，避免发送无效值。
            let has_explicit_auth_method = credentials
                .auth_method
                .as_deref()
                .is_some_and(|m| !m.trim().is_empty());
            if has_explicit_auth_method {
                return Ok(request_body.to_string());
            }
            let mut request: serde_json::Value = serde_json::from_str(request_body)?;
            if let Some(obj) = request.as_object_mut() {
                obj.remove("profileArn");
            }
            return Ok(serde_json::to_string(&request)?);
        };

        let mut request: serde_json::Value = serde_json::from_str(request_body)?;
        let obj = request
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("request body is not a JSON object"))?;
        obj.insert(
            "profileArn".to_string(),
            serde_json::Value::String(profile_arn.to_string()),
        );
        Ok(serde_json::to_string(&request)?)
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("content-type", "application/json")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if Self::is_external_idp_credentials(ctx.credentials) {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("content-type", "application/json")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(ctx.credentials) {
            req = req.header("x-amzn-kiro-profile-arn", profile_arn);
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        } else if Self::is_external_idp_credentials(ctx.credentials) {
            req = req.header("TokenType", "EXTERNAL_IDP");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> anyhow::Result<String> {
        Self::inject_profile_arn(body, ctx.credentials)
    }

    fn usage_request_parts(&self, ctx: &RequestContext<'_>) -> anyhow::Result<UsageRequestParts> {
        let host = self.host(ctx);
        let mut url = format!(
            "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
            host
        );
        if let Some(profile_arn) = Self::mcp_profile_arn_header_value(ctx.credentials) {
            url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
        }

        let mut headers = vec![
            (
                "x-amz-user-agent",
                format!(
                    "aws-sdk-js/1.0.0 KiroIDE-{}-{}",
                    ctx.config.kiro_version, ctx.machine_id
                ),
            ),
            (
                "user-agent",
                format!(
                    "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
                    ctx.config.system_version,
                    ctx.config.node_version,
                    ctx.config.kiro_version,
                    ctx.machine_id
                ),
            ),
            ("host", host),
            ("amz-sdk-invocation-id", Uuid::new_v4().to_string()),
            ("amz-sdk-request", "attempt=1; max=1".to_string()),
            ("Authorization", format!("Bearer {}", ctx.token)),
            ("Connection", "close".to_string()),
        ];

        if ctx.credentials.is_api_key_credential() {
            headers.push(("tokentype", "API_KEY".to_string()));
        } else if Self::is_external_idp_credentials(ctx.credentials) {
            headers.push(("TokenType", "EXTERNAL_IDP".to_string()));
        }

        Ok(UsageRequestParts { url, headers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use serde_json::Value;

    fn creds_with_arn(arn: Option<&str>) -> KiroCredentials {
        KiroCredentials {
            profile_arn: arn.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let creds = creds_with_arn(Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC"));
        let result = IdeEndpoint::inject_profile_arn(body, &creds).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let creds = creds_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &creds).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let creds = creds_with_arn(Some("new-arn"));
        let result = IdeEndpoint::inject_profile_arn(body, &creds).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_none_removes_existing() {
        // 无真实 profileArn 时应移除请求体里残留的占位符
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let creds = creds_with_arn(None);
        let result = IdeEndpoint::inject_profile_arn(body, &creds).unwrap();
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let creds = creds_with_arn(Some("arn:test"));
        assert!(IdeEndpoint::inject_profile_arn(body, &creds).is_err());
    }
}
