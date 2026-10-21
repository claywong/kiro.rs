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
/// Runtime 端点名称（`runtime.{region}.kiro.dev`）
pub const RUNTIME_ENDPOINT_NAME: &str = "runtime";
/// Legacy CodeWhisperer 端点名称（固定 `codewhisperer.us-east-1.amazonaws.com`）
pub const CODEWHISPERER_ENDPOINT_NAME: &str = "codewhisperer";

/// IDE 风格端点的三种主机形态。请求头 / 请求体完全一致，只有名称与主机不同。
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdeHost {
    /// `q.{region}.amazonaws.com`（Legacy Kiro IDE）
    LegacyQ,
    /// `runtime.{region}.kiro.dev`
    Runtime,
    /// `codewhisperer.us-east-1.amazonaws.com`（region 固定）
    CodeWhisperer,
}

/// Kiro IDE 端点
pub struct IdeEndpoint {
    host: IdeHost,
}

impl IdeEndpoint {
    pub fn new() -> Self {
        Self { host: IdeHost::LegacyQ }
    }

    /// Runtime 端点：`https://runtime.{region}.kiro.dev/generateAssistantResponse`
    pub fn runtime() -> Self {
        Self { host: IdeHost::Runtime }
    }

    /// Legacy CodeWhisperer 端点：`https://codewhisperer.us-east-1.amazonaws.com/...`
    pub fn codewhisperer() -> Self {
        Self { host: IdeHost::CodeWhisperer }
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        match self.host {
            IdeHost::LegacyQ => format!("q.{}.amazonaws.com", self.api_region(ctx)),
            IdeHost::Runtime => format!("runtime.{}.kiro.dev", self.api_region(ctx)),
            IdeHost::CodeWhisperer => "codewhisperer.us-east-1.amazonaws.com".to_string(),
        }
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
        match self.host {
            IdeHost::LegacyQ => IDE_ENDPOINT_NAME,
            IdeHost::Runtime => RUNTIME_ENDPOINT_NAME,
            IdeHost::CodeWhisperer => CODEWHISPERER_ENDPOINT_NAME,
        }
    }

    /// 只有 Legacy IDE 参与 429 原号重试的默认轮换；Runtime / CodeWhisperer 仅供
    /// 短速刷随机端点开关使用，避免注册它们后改变存量的 ide↔cli 轮换行为。
    fn in_default_rotation(&self) -> bool {
        self.host == IdeHost::LegacyQ
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/generateAssistantResponse", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/mcp", self.host(ctx))
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
    fn test_runtime_and_codewhisperer_urls_and_rotation() {
        use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
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
        let region = credentials.effective_api_region(&config).to_string();

        let ide = super::IdeEndpoint::new();
        let rt = super::IdeEndpoint::runtime();
        let cw = super::IdeEndpoint::codewhisperer();

        assert_eq!(ide.api_url(&ctx), format!("https://q.{region}.amazonaws.com/generateAssistantResponse"));
        assert_eq!(rt.api_url(&ctx), format!("https://runtime.{region}.kiro.dev/generateAssistantResponse"));
        assert_eq!(cw.api_url(&ctx), "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse");
        assert_eq!((ide.name(), rt.name(), cw.name()), ("ide", "runtime", "codewhisperer"));
        // 只有 ide 参与默认的 429 轮换，注册 runtime/codewhisperer 不改变存量行为
        assert!(ide.in_default_rotation());
        assert!(!rt.in_default_rotation());
        assert!(!cw.in_default_rotation());
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
