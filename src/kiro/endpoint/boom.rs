//! Boom 端点
//!
//! 专为 `metadata.type = boom` 的账号设计，组合了：
//! - cli 的 URL（`https://q.{region}.amazonaws.com/`）、Content-Type、`x-amz-target`、
//!   body transform（注入 profileArn、KIRO_CLI origin）
//! - ide 的 UA（`KiroIDE-*` 格式）
//!
//! ## 为什么需要这个组合
//!
//! 上游对同一账号的模型访问权限取决于请求头中的客户端标识：
//! - **ide URL + ide UA**（即当前 `ide` 端点）：上游按 IDE 模型列表过滤，FREE 号拿不到
//!   opus-5.5 等模型，返回 `400 INVALID_MODEL_ID`。
//! - **cli URL + amazonq UA**（即当前 `cli` 端点）：同样被模型列表限制，400。
//! - **cli URL + ide UA**（本端点）：上游模型限制不生效，200。
//!
//! 通过实测验证（2026-11-06），连续 3 轮 opus-5.5 和 sonnet-5.5 各成功，credit
//! 用量与有 opus 权限的 1585 号完全一致，确认不是被静默降级。

use reqwest::RequestBuilder;
use uuid::Uuid;

use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::kiro_version;

pub const BOOM_ENDPOINT_NAME: &str = "boom";

pub struct BoomEndpoint;

impl BoomEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    /// ide 格式的 x-amz-user-agent（KiroIDE UA）
    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/{} KiroIDE-{}-{}",
            kiro_version::USAGE_API_AWS_SDK_VERSION,
            kiro_version::effective_ide(&ctx.config.kiro_version),
            ctx.machine_id
        )
    }

    /// ide 格式的 user-agent（KiroIDE UA）
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

impl Default for BoomEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for BoomEndpoint {
    fn name(&self) -> &'static str {
        BOOM_ENDPOINT_NAME
    }

    /// boom 端点不参与默认的 ide↔cli 轮换，boom 号始终固定打这个端点。
    fn in_default_rotation(&self) -> bool {
        false
    }

    fn content_type(&self) -> &'static str {
        // cli URL 要求 amz-json-1.0
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        // cli URL：根路径，由 x-amz-target 头决定操作
        format!("https://q.{}.amazonaws.com/", self.api_region(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            // cli URL 所需的 target 头
            .header(
                "x-amz-target",
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
            )
            .header("x-amzn-codewhisperer-optout", "false")
            // ide 格式的 UA
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
        // cli 的 body transform：注入 profileArn、KIRO_CLI origin，移除 agentContinuationId
        crate::kiro::endpoint::cli::set_origin_kiro_cli(
            body,
            ctx.credentials.streaming_profile_arn().as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::kiro_version::{IDE_UA_KIRO_VERSION, USAGE_API_AWS_SDK_VERSION};

    fn dummy_creds() -> crate::kiro::model::credentials::KiroCredentials {
        crate::kiro::model::credentials::KiroCredentials::default()
    }

    fn dummy_config() -> crate::model::config::Config {
        crate::model::config::Config::default()
    }

    #[test]
    fn test_boom_endpoint_name() {
        assert_eq!(BoomEndpoint::new().name(), BOOM_ENDPOINT_NAME);
    }

    #[test]
    fn test_boom_not_in_default_rotation() {
        assert!(!BoomEndpoint::new().in_default_rotation());
    }

    #[test]
    fn test_boom_content_type_is_amz_json() {
        assert_eq!(BoomEndpoint::new().content_type(), "application/x-amz-json-1.0");
    }

    #[test]
    fn test_boom_uses_ide_ua_format() {
        let ep = BoomEndpoint::new();
        let creds = dummy_creds();
        let config = dummy_config();
        let machine_id = "testmachineid";
        let ctx = RequestContext {
            credentials: &creds,
            token: "tok",
            machine_id,
            config: &config,
        };
        let ua = ep.x_amz_user_agent(&ctx);
        // UA 必须是 KiroIDE 格式，而非 AmazonQ-For-CLI 格式
        assert!(ua.contains(&format!("KiroIDE-{}", IDE_UA_KIRO_VERSION)), "UA: {ua}");
        assert!(ua.contains(USAGE_API_AWS_SDK_VERSION), "UA: {ua}");
        assert!(!ua.contains("AmazonQ-For-CLI"), "UA 不应包含 AmazonQ-For-CLI: {ua}");
    }

    #[test]
    fn test_boom_api_url_uses_cli_root_path() {
        let ep = BoomEndpoint::new();
        let creds = dummy_creds();
        let config = dummy_config();
        let ctx = RequestContext {
            credentials: &creds,
            token: "tok",
            machine_id: "m",
            config: &config,
        };
        let url = ep.api_url(&ctx);
        // URL 必须以 / 结尾（cli 格式），而非 /generateAssistantResponse（ide 格式）
        assert!(url.ends_with('/'), "URL: {url}");
        assert!(url.contains("amazonaws.com"), "URL: {url}");
    }
}
