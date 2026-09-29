//! 会话亲和 × provider 重试链路的集成测试
//!
//! 起一个本地 HTTP 桩服务，按 Bearer token 区分凭据：指定凭据固定返回用户级 429，
//! 其余返回 200。验证亲和命中的凭据先原号重试 N 次再换号，并改绑到新凭据。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use chrono::{Duration, Utc};
use parking_lot::Mutex;
use reqwest::RequestBuilder;

use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::kiro::token_manager::{AffinityOutcome, MultiTokenManager};
use crate::model::config::Config;

const RATE_LIMITED_BODY: &str =
    r#"{"message":"Too many requests, please wait before trying again.","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;

/// 指向本地桩服务的测试端点
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

/// 桩服务状态：被限流的 token 集合 + 每个 token 收到的请求数
#[derive(Default)]
struct StubState {
    limited: Mutex<Vec<String>>,
    hits: Mutex<HashMap<String, usize>>,
    total: AtomicUsize,
}

async fn spawn_stub(state: Arc<StubState>) -> String {
    let app = Router::new().route(
        "/api",
        post(move |headers: HeaderMap| {
            let state = state.clone();
            async move {
                let token = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .trim_start_matches("Bearer ")
                    .to_string();
                state.total.fetch_add(1, Ordering::Relaxed);
                *state.hits.lock().entry(token.clone()).or_default() += 1;
                if state.limited.lock().contains(&token) {
                    (StatusCode::TOO_MANY_REQUESTS, RATE_LIMITED_BODY.to_string())
                } else {
                    (StatusCode::OK, String::new())
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

fn build_provider(base: String, n: usize) -> KiroProvider {
    let mut config = Config::default();
    config.session_affinity_429_retry_delay_ms = 1;
    let creds = (0..n)
        .map(|i| KiroCredentials {
            access_token: Some(format!("t{}", i + 1)),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            // 非占位 profileArn：跳过真实 profileArn 解析的网络调用
            profile_arn: Some(format!(
                "arn:aws:codewhisperer:us-east-1:000000000000:profile/test{i}"
            )),
            ..KiroCredentials::default()
        })
        .collect();
    let manager = Arc::new(MultiTokenManager::new(config, creds, None, None, false).unwrap());
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    endpoints.insert("stub".to_string(), Arc::new(StubEndpoint { base }));
    KiroProvider::with_proxy(manager, None, endpoints, "stub".to_string())
}

const BODY: &str = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-sonnet-5"}}}}"#;

#[tokio::test]
async fn affinity_hit_retries_same_credential_then_rebinds() {
    let state = Arc::new(StubState::default());
    let base = spawn_stub(state.clone()).await;
    let provider = build_provider(base, 3);

    // 首次请求：绑定到调度选中的号
    let first = provider
        .call_api(BODY, None, None, None, Some("sess-1"))
        .await
        .unwrap();
    let bound = first.credential_id;
    let bound_token = format!("t{bound}");

    // 绑定号开始 429：应原号共打 3 次（1 + 重试 2），再换号成功
    state.limited.lock().push(bound_token.clone());
    state.hits.lock().clear();
    let second = provider
        .call_api(BODY, None, None, None, Some("sess-1"))
        .await
        .unwrap();
    assert_ne!(second.credential_id, bound, "原号重试耗尽后应换号");
    assert_eq!(
        state.hits.lock().get(&bound_token).copied(),
        Some(3),
        "亲和凭据应原号请求 3 次"
    );

    // 改绑：后续请求直接打新号，不再碰原号
    state.hits.lock().clear();
    let third = provider
        .call_api(BODY, None, None, None, Some("sess-1"))
        .await
        .unwrap();
    assert_eq!(third.credential_id, second.credential_id);
    assert_eq!(state.hits.lock().get(&bound_token), None);
}

#[tokio::test]
async fn non_affinity_429_fails_over_immediately() {
    let state = Arc::new(StubState::default());
    let base = spawn_stub(state.clone()).await;
    let provider = build_provider(base, 2);

    // 无 session：首号 429 直接换号，不做原号重试
    let probe = provider.call_api(BODY, None, None, None, None).await.unwrap();
    let token = format!("t{}", probe.credential_id);
    state.limited.lock().push(token.clone());
    state.hits.lock().clear();

    // 让无 session 调度再次落到被限流的号：它 success_count 更高，least-used 会避开，
    // 所以直接验证「被限流号最多被打 1 次」即可覆盖两种调度结果。
    let result = provider.call_api(BODY, None, None, None, None).await.unwrap();
    assert_ne!(result.credential_id, probe.credential_id);
    assert!(state.hits.lock().get(&token).copied().unwrap_or(0) <= 1);
}

#[tokio::test]
async fn affinity_outcome_reported_to_trace_sink() {
    use crate::admin::trace_db::{TraceAttempt, TraceSink};

    #[derive(Default)]
    struct Sink(Mutex<Vec<&'static str>>);
    impl TraceSink for Sink {
        fn on_attempt(&self, _attempt: TraceAttempt) {}
        fn on_affinity(&self, affinity: &'static str) {
            self.0.lock().push(affinity);
        }
    }

    let state = Arc::new(StubState::default());
    let base = spawn_stub(state).await;
    let provider = build_provider(base, 2);
    let sink = Sink::default();
    provider.call_api(BODY, None, Some(&sink), None, Some("s")).await.unwrap();
    provider.call_api(BODY, None, Some(&sink), None, Some("s")).await.unwrap();
    provider.call_api(BODY, None, Some(&sink), None, None).await.unwrap();
    assert_eq!(
        *sink.0.lock(),
        vec![
            AffinityOutcome::Bind.as_str(),
            AffinityOutcome::Hit.as_str(),
            AffinityOutcome::None.as_str()
        ]
    );
}
