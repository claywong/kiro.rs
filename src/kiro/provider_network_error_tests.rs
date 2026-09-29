//! 网络错误（error sending request）× provider 重试链路的集成测试
//!
//! 凭据 #1 配专属代理并指向一个已关闭的端口，请求必然在发送阶段失败；
//! 验证该失败只在本次请求内换号，不计入 failure_count、不会禁用凭据。
//!
//! @author wangzhong

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use chrono::{Duration, Utc};
use reqwest::RequestBuilder;

use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

struct StubEndpoint {
    base: String,
}

impl KiroEndpoint for StubEndpoint {
    fn name(&self) -> &'static str {
        "stub"
    }
    fn api_url(&self, _ctx: &RequestContext<'_>) -> String {
        format!("{}/api", self.base)
    }
    fn mcp_url(&self, _ctx: &RequestContext<'_>) -> String {
        format!("{}/mcp", self.base)
    }
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        req.header("Authorization", format!("Bearer {}", ctx.token))
    }
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        self.decorate_api(req, ctx)
    }
    fn transform_api_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }
}

async fn spawn_ok_stub() -> String {
    let app = Router::new().route("/api", post(|| async { (StatusCode::OK, String::new()) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// 拿一个当前没有监听的本地端口，作为「坏代理」
async fn dead_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

fn credential(i: usize, proxy_url: Option<String>) -> KiroCredentials {
    KiroCredentials {
        access_token: Some(format!("t{}", i + 1)),
        expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        profile_arn: Some(format!(
            "arn:aws:codewhisperer:us-east-1:000000000000:profile/test{i}"
        )),
        priority: i as u32,
        proxy_url,
        ..KiroCredentials::default()
    }
}

const BODY: &str = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-sonnet-5"}}}}"#;

#[tokio::test]
async fn network_error_with_own_proxy_fails_over_without_disabling() {
    let base = spawn_ok_stub().await;
    let bad_proxy = format!("http://127.0.0.1:{}", dead_port().await);
    // 凭据 #1 优先级最高但代理是坏的；凭据 #2 直连正常
    let creds = vec![credential(0, Some(bad_proxy)), credential(1, None)];
    let manager =
        Arc::new(MultiTokenManager::new(Config::default(), creds, None, None, false).unwrap());
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    endpoints.insert("stub".to_string(), Arc::new(StubEndpoint { base }));
    let provider = KiroProvider::with_proxy(manager.clone(), None, endpoints, "stub".to_string());

    // 连续多次（超过 MAX_FAILURES_PER_CREDENTIAL=3）都应换号成功
    for _ in 0..5 {
        let result = provider.call_api(BODY, None, None, None, None).await.unwrap();
        assert_eq!(result.credential_id, 2);
    }

    let snapshot = manager.snapshot();
    let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
    assert!(!first.disabled, "网络错误不应禁用凭据");
    assert_eq!(first.failure_count, 0, "网络错误不应计入 failure_count");
}
