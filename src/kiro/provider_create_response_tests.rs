//! CreateResponse 回退 × provider 重试链路的集成测试
//!
//! 本地桩服务同时模拟旧接口（`/api`，固定返回用户级 429）和 CreateResponse
//! （`/cr`，返回 SSE），验证 auto / always / off 三档的行为差异。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use chrono::{Duration, Utc};
use parking_lot::Mutex;
use reqwest::RequestBuilder;

use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::events::Event;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{Config, CreateResponseMode};

const RATE_LIMITED_BODY: &str =
    r#"{"message":"Too many requests","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
const BODY: &str = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"content":"hi","modelId":"claude-sonnet-5"}}}}"#;
const SSE_OK: &str = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"kiro:credits\":{\"value\":0.25}}}}\n\n\
data: [DONE]\n\n";

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

#[derive(Default)]
struct Stub {
    legacy_hits: AtomicUsize,
    cr_hits: AtomicUsize,
    /// CreateResponse 依次返回的状态码；耗尽后返回 200 + SSE。
    cr_statuses: Mutex<Vec<u16>>,
    cr_headers: Mutex<Vec<HashMap<String, String>>>,
    cr_bodies: Mutex<Vec<String>>,
}

async fn spawn_stub(stub: Arc<Stub>) -> String {
    let legacy = stub.clone();
    let cr = stub.clone();
    let app = Router::new()
        .route(
            "/api",
            post(move || {
                let stub = legacy.clone();
                async move {
                    stub.legacy_hits.fetch_add(1, Ordering::Relaxed);
                    (StatusCode::TOO_MANY_REQUESTS, RATE_LIMITED_BODY.to_string())
                }
            }),
        )
        .route(
            "/cr",
            post(move |headers: HeaderMap, body: String| {
                let stub = cr.clone();
                async move {
                    stub.cr_hits.fetch_add(1, Ordering::Relaxed);
                    let seen = headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    stub.cr_headers.lock().push(seen);
                    stub.cr_bodies.lock().push(body);
                    let next = {
                        let mut statuses = stub.cr_statuses.lock();
                        (!statuses.is_empty()).then(|| statuses.remove(0))
                    };
                    match next {
                        Some(code) if code != 200 => {
                            (StatusCode::from_u16(code).unwrap(), "boom".to_string()).into_response()
                        }
                        _ => ([("content-type", "text/event-stream")], SSE_OK.to_string()).into_response(),
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn build_provider(base: &str, mode: CreateResponseMode) -> KiroProvider {
    let mut config = Config::default();
    config.create_response_mode = mode;
    let cred = KiroCredentials {
        access_token: Some("tok-1".to_string()),
        expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        profile_arn: Some("arn:aws:codewhisperer:us-east-1:000000000000:profile/test".to_string()),
        ..KiroCredentials::default()
    };
    let manager = Arc::new(MultiTokenManager::new(config, vec![cred], None, None, false).unwrap());
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    endpoints.insert("stub".to_string(), Arc::new(StubEndpoint { base: base.to_string() }));
    let provider = KiroProvider::new(manager, endpoints, "stub".to_string());
    *provider.create_response_url_override.lock() = Some(format!("{base}/cr"));
    provider
}

/// 读完响应体并按 event-stream 解码，返回助手文本和计费量。
async fn read_text(response: reqwest::Response) -> (String, f64) {
    let bytes = response.bytes().await.unwrap();
    let mut decoder = EventStreamDecoder::new();
    decoder.feed(&bytes).unwrap();
    let (mut text, mut credits) = (String::new(), 0.0);
    for frame in decoder.decode_iter() {
        match Event::from_frame(frame.unwrap()).unwrap() {
            Event::AssistantResponse(e) => text.push_str(&e.content),
            Event::Metering(m) => credits += m.usage,
            _ => {}
        }
    }
    (text, credits)
}

#[tokio::test]
async fn auto_falls_back_to_create_response_on_user_rate_limit() {
    let stub = Arc::new(Stub::default());
    let base = spawn_stub(stub.clone()).await;
    let provider = build_provider(&base, CreateResponseMode::Auto);

    let call = provider.call_api(BODY, None, None, None, None).await.unwrap();
    assert_eq!(call.credential_id, 1, "回退发生在同一凭据上");
    let (text, credits) = read_text(call.response).await;
    assert_eq!(text, "OK");
    assert!((credits - 0.25).abs() < 1e-9);

    assert_eq!(stub.legacy_hits.load(Ordering::Relaxed), 1, "先试旧接口");
    assert_eq!(stub.cr_hits.load(Ordering::Relaxed), 1);
    let headers = stub.cr_headers.lock()[0].clone();
    assert_eq!(headers.get("authorization").map(String::as_str), Some("Bearer tok-1"));
    assert_eq!(headers.get("x-amz-sso-bearer").map(String::as_str), Some("tok-1"));
    assert_eq!(
        headers.get("x-amzn-kiro-profile").map(String::as_str),
        Some("arn:aws:codewhisperer:us-east-1:000000000000:profile/test")
    );
    let body: serde_json::Value = serde_json::from_str(&stub.cr_bodies.lock()[0]).unwrap();
    assert_eq!(body["model"], "claude-sonnet-5");
    assert_eq!(body["stream"], true);
    assert_eq!(body["input"][0]["content"], "hi");
}

#[tokio::test]
async fn off_never_touches_create_response() {
    let stub = Arc::new(Stub::default());
    let base = spawn_stub(stub.clone()).await;
    let provider = build_provider(&base, CreateResponseMode::Off);

    assert!(provider.call_api(BODY, None, None, None, None).await.is_err());
    assert_eq!(stub.cr_hits.load(Ordering::Relaxed), 0);
    assert!(stub.legacy_hits.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn always_skips_the_legacy_endpoint() {
    let stub = Arc::new(Stub::default());
    let base = spawn_stub(stub.clone()).await;
    let provider = build_provider(&base, CreateResponseMode::Always);

    let call = provider.call_api(BODY, None, None, None, None).await.unwrap();
    let (text, _) = read_text(call.response).await;
    assert_eq!(text, "OK");
    assert_eq!(stub.legacy_hits.load(Ordering::Relaxed), 0);
    assert_eq!(stub.cr_hits.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn auto_keeps_original_429_handling_when_create_response_also_fails() {
    let stub = Arc::new(Stub::default());
    *stub.cr_statuses.lock() = vec![500; 20];
    let base = spawn_stub(stub.clone()).await;
    let provider = build_provider(&base, CreateResponseMode::Auto);

    assert!(provider.call_api(BODY, None, None, None, None).await.is_err());
    assert!(stub.cr_hits.load(Ordering::Relaxed) >= 1, "确实尝试过回退");
}

#[tokio::test]
async fn auto_fallback_retries_create_response_429_once() {
    let stub = Arc::new(Stub::default());
    *stub.cr_statuses.lock() = vec![429];
    let base = spawn_stub(stub.clone()).await;
    let provider = build_provider(&base, CreateResponseMode::Auto);

    let call = provider.call_api(BODY, None, None, None, None).await.unwrap();
    let (text, _) = read_text(call.response).await;
    assert_eq!(text, "OK");
    assert_eq!(stub.legacy_hits.load(Ordering::Relaxed), 1);
    assert_eq!(stub.cr_hits.load(Ordering::Relaxed), 2, "429 后等待 1.5 秒并重试一次");
}
