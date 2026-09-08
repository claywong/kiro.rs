//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};
use crate::kiro::kiro_version;

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
            "aws-sdk-js/{} KiroIDE-{}-{}",
            kiro_version::USAGE_API_AWS_SDK_VERSION,
            kiro_version::effective_ide(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        let sdk = kiro_version::USAGE_API_AWS_SDK_VERSION;
        format!(
            "aws-sdk-js/{} ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#{} m/E KiroIDE-{}-{}",
            sdk,
            ctx.config.system_version,
            ctx.config.node_version,
            sdk,
            kiro_version::effective_ide(&ctx.config.kiro_version),
            ctx.machine_id
        )
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
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("tokentype", token_type);
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("tokentype", token_type);
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, ctx.credentials.streaming_profile_arn().as_deref())
    }
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: Option<&str>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.to_string());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::inject_profile_arn;
    use serde_json::Value;

    /// IDE 端点 UA 必须钉死在 [`IDE_UA_KIRO_VERSION`]，且与用量链路 SDK 版本一致。
    ///
    /// 上游把 UA 里的版本号当准入条件：发出不存在的 `KiroIDE-2.3.0`（`config.kiro_version`
    /// 的默认值，语义上属于 kiro-cli）会被判 403 `User is not authorized to make this call.`
    /// 版本号还必须与 SDK 版本成对——故一并断言，防止只改一处。
    #[test]
    fn test_ide_user_agent_pins_version() {
        use crate::kiro::endpoint::RequestContext;
        use crate::kiro::kiro_version::{
            CLI_DEFAULT_KIRO_VERSION, IDE_UA_KIRO_VERSION, USAGE_API_AWS_SDK_VERSION,
        };
        use crate::kiro::model::credentials::KiroCredentials;
        use crate::model::config::Config;

        let config = Config::default();
        let credentials = KiroCredentials::default();
        let ctx = RequestContext {
            credentials: &credentials,
            token: "t",
            machine_id: "m",
            config: &config,
        };
        let endpoint = super::IdeEndpoint::new();

        for ua in [endpoint.user_agent(&ctx), endpoint.x_amz_user_agent(&ctx)] {
            assert!(
                ua.contains(&format!("KiroIDE-{}-m", IDE_UA_KIRO_VERSION)),
                "UA 应钉死 IDE 版本 {IDE_UA_KIRO_VERSION}: {ua}"
            );
            assert!(
                !ua.contains(&format!("KiroIDE-{}", CLI_DEFAULT_KIRO_VERSION)),
                "UA 不应出现 kiro-cli 默认版本: {ua}"
            );
            assert!(
                ua.contains(&format!("aws-sdk-js/{}", USAGE_API_AWS_SDK_VERSION)),
                "UA 的 SDK 版本应与用量链路统一: {ua}"
            );
        }
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, arn.as_deref());
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
        let result = inject_profile_arn(body, None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, arn.as_deref());
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, arn.as_deref());
        assert_eq!(result, "not-valid-json");
    }
}
