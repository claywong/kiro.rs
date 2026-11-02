//! KiroShop 管理 API（`/api/admin/kiroshop/*`，adminApiKey 认证）
//! 与入站 webhook（`/webhook/kiroshop/{token}`，HMAC 验签）
//!
//! @author wangzhong

use std::sync::Arc;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::client::{KiroshopError, ShopEvent};
use super::service::{BuyParams, KiroshopService};

const MAX_WEBHOOK_BODY: usize = 64 * 1024;

#[derive(Clone)]
pub struct KiroshopState {
    pub service: Arc<KiroshopService>,
}

fn err(e: KiroshopError) -> Response {
    let status = e
        .status
        .and_then(|c| StatusCode::from_u16(c).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let mut body = json!({ "error": e.to_string() });
    if let Some(code) = &e.code {
        body["code"] = json!(code);
    }
    (status, Json(body)).into_response()
}

fn not_configured() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "KiroShop 未配置（config.json 缺 kiroshop 段或 apiKey 为空）" })),
    )
        .into_response()
}

fn ok_json(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}

// ============ 状态与商品 ============

/// GET /status：账户 + 池子 + 在途 + 告警。面板首屏一次拉全。
async fn status(State(st): State<KiroshopState>) -> Response {
    let service = &st.service;
    let cfg = service.config();
    if !cfg.is_ready() {
        return not_configured();
    }
    let client = service.api();
    let (account, products) = tokio::join!(client.account(), client.products());
    let account = account
        .inspect_err(|e| tracing::warn!("KiroShop /status 查账户失败: {e}"))
        .ok()
        .or_else(|| service.last_account());
    let products_summary: Vec<Value> = products
        .map(|ps| {
            ps.iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "displayName": p.display_name,
                        "price": p.price,
                        "available": p.available(),
                        "backorder": p.stock.backorder,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ok_json(json!({
        "configured": true,
        "enabled": cfg.enabled,
        "baseUrl": cfg.base_url,
        "account": account,
        "products": products_summary,
        "pool": {
            "count": service.pool_count(),
            "inflight": service.inflight_count(),
            "target": if cfg.auto_buy.enabled { cfg.auto_buy.pool_target } else { 0 },
        },
        "autoBuy": cfg.auto_buy,
        "unackedAlerts": service.store().unacked_local_count().unwrap_or(0),
        "trackedCount": service.store().tracked_orders().map(|t| t.len()).unwrap_or(0),
        "lastError": service.last_error(),
        "webhookPath": format!("/webhook/kiroshop/{}", cfg.webhook_path_token),
    }))
}

/// GET /products
async fn products(State(st): State<KiroshopState>) -> Response {
    match st.service.api().products().await {
        Ok(ps) => ok_json(json!({ "data": ps })),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuoteBody {
    product_id: i64,
    quantity: u32,
    #[serde(default)]
    option_key: Option<String>,
    #[serde(default)]
    coupon_code: Option<String>,
    #[serde(default)]
    allow_backorder: bool,
}

/// POST /quote：报价预览（不下单、不扣钱）
async fn quote(State(st): State<KiroshopState>, Json(b): Json<QuoteBody>) -> Response {
    let req = super::client::OrderRequest {
        product_id: b.product_id,
        quantity: b.quantity.max(1),
        option_key: b.option_key.filter(|s| !s.trim().is_empty()),
        coupon_code: b.coupon_code.filter(|s| !s.trim().is_empty()),
        pay_with: "balance".into(),
        backorder: if b.allow_backorder { "allow" } else { "deny" }.into(),
        max_amount: None,
        client_ref: None,
    };
    match st.service.api().quote(&req).await {
        Ok(q) => ok_json(json!({ "data": q })),
        Err(e) => err(e),
    }
}

// ============ 购买与订单 ============

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuyBody {
    product_id: i64,
    quantity: u32,
    #[serde(default)]
    option_key: Option<String>,
    #[serde(default)]
    coupon_code: Option<String>,
    #[serde(default)]
    max_amount: Option<f64>,
    #[serde(default)]
    allow_backorder: bool,
    #[serde(default)]
    client_ref: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// POST /buy：完整下单（报价 → 建单 → 支付 → 取号 → 入库）
async fn buy(State(st): State<KiroshopState>, Json(b): Json<BuyBody>) -> Response {
    let outcome = st
        .service
        .buy(BuyParams {
            product_id: b.product_id,
            quantity: b.quantity.max(1),
            option_key: b.option_key.filter(|s| !s.trim().is_empty()),
            coupon_code: b.coupon_code.filter(|s| !s.trim().is_empty()),
            max_amount: b.max_amount.filter(|m| m > &0.0),
            allow_backorder: b.allow_backorder,
            client_ref: b.client_ref.filter(|s| !s.trim().is_empty()),
            idempotency_key: b.idempotency_key.filter(|s| !s.trim().is_empty()),
            source: "manual".into(),
        })
        .await;
    match outcome {
        Ok(o) => ok_json(json!({ "data": o })),
        Err(e) => err(e),
    }
}

/// GET /orders：本地跟踪 + 卖家侧全量（面板合并渲染，标记 tracked）
async fn orders(State(st): State<KiroshopState>) -> Response {
    let tracked = st.service.store().tracked_orders().unwrap_or_default();
    let shop = st
        .service
        .api()
        .all_orders()
        .await
        .unwrap_or_default();
    ok_json(json!({ "tracked": tracked, "shop": shop }))
}

/// GET /orders/{id}：详情 + 补号 + 售后 + 本地跟踪行
async fn order_detail(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    let client = st.service.api();
    let (detail, supplements, after_sale) = tokio::join!(
        client.order(&id),
        client.supplements(&id),
        client.after_sale_list(&id)
    );
    match detail {
        Ok(d) => ok_json(json!({
            "order": d,
            "supplements": supplements.unwrap_or_default(),
            "afterSale": after_sale.unwrap_or_default(),
            "tracked": st.service.store().tracked_order(&id).ok().flatten(),
        })),
        Err(e) => err(e),
    }
}

async fn pay(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    match st.service.api().pay_order(&id).await {
        Ok(d) => ok_json(json!({ "data": d })),
        Err(e) => err(e),
    }
}

async fn cancel(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    match st.service.api().cancel_order(&id).await {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

/// POST /orders/{id}/pickup：手动取号 + 增量入库
async fn pickup(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    match st.service.manual_pickup(&id).await {
        Ok(o) => ok_json(json!({ "data": o })),
        Err(e) => err(e),
    }
}

/// POST /orders/{id}/import：只做增量拿号入库（不取号；历史单/网页单补导入用）
async fn import_delivery(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    let outcome = st.service.poll_delivery_for_order(&id).await;
    let data = outcome
        .map(|o| {
            json!({ "imported": o.imported, "duplicated": o.duplicated, "failed": o.failed,
                    "lastError": o.last_error })
        })
        .unwrap_or_else(|| json!({ "imported": 0, "duplicated": 0, "failed": 0 }));
    ok_json(json!({ "data": data }))
}

async fn supplements(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    match st.service.api().supplements(&id).await {
        Ok(v) => ok_json(json!({ "data": v })),
        Err(e) => err(e),
    }
}

/// POST /orders/{id}/supplements/{sid}/pickup：领取补号并入库
async fn pickup_supplement(
    State(st): State<KiroshopState>,
    Path((id, sid)): Path<(String, i64)>,
) -> Response {
    match st.service.pickup_supplement(&id, sid).await {
        Ok(o) => ok_json(json!({ "data": o })),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AfterSaleBody {
    kind: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn after_sale(State(st): State<KiroshopState>, Path(id): Path<String>) -> Response {
    match st.service.api().after_sale_list(&id).await {
        Ok(v) => ok_json(json!({ "data": v })),
        Err(e) => err(e),
    }
}

async fn after_sale_create(
    State(st): State<KiroshopState>,
    Path(id): Path<String>,
    Json(b): Json<AfterSaleBody>,
) -> Response {
    let kind = b.kind.trim();
    if !["banned", "login", "quota", "missing", "other"].contains(&kind) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "kind 必须是 banned/login/quota/missing/other 之一" })),
        )
            .into_response();
    }
    match st
        .service
        .api()
        .after_sale_create(&id, kind, b.reason.as_deref())
        .await
    {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

// ============ 事件与告警 ============

#[derive(Deserialize)]
struct EventsQuery {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    unacked_only: Option<bool>,
}

async fn events(State(st): State<KiroshopState>, Query(q): Query<EventsQuery>) -> Response {
    let evs = st
        .service
        .store()
        .list_local_events(q.limit.unwrap_or(100).min(500), q.unacked_only.unwrap_or(false))
        .unwrap_or_default();
    ok_json(json!({
        "data": evs,
        "unacked": st.service.store().unacked_local_count().unwrap_or(0),
    }))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AckBody {
    #[serde(default)]
    event_id: Option<i64>,
}

async fn ack_events(State(st): State<KiroshopState>, Json(b): Json<AckBody>) -> Response {
    let id = b.event_id;
    match st.service.store().ack_local_events(id) {
        Ok(n) => ok_json(json!({ "acked": n })),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

// ============ 账户 ============

async fn account(State(st): State<KiroshopState>) -> Response {
    match st.service.api().account().await {
        Ok(a) => ok_json(json!({ "data": a })),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

async fn balance_logs(State(st): State<KiroshopState>, Query(q): Query<PageQuery>) -> Response {
    match st
        .service
        .api()
        .balance_logs(q.page.unwrap_or(1), q.size.unwrap_or(20))
        .await
    {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DailyLimitBody {
    daily_pay_limit: f64,
}

async fn set_daily_limit(State(st): State<KiroshopState>, Json(b): Json<DailyLimitBody>) -> Response {
    if !(0.0..=1_000_000.0).contains(&b.daily_pay_limit) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "daily_pay_limit 取值 0 ~ 1000000（0 = 不限）" })),
        )
            .into_response();
    }
    match st
        .service
        .api()
        .set_daily_limit(b.daily_pay_limit)
        .await
    {
        Ok(a) => ok_json(json!({ "data": a })),
        Err(e) => err(e),
    }
}

// ============ webhook 管理 ============

async fn webhook_get(State(st): State<KiroshopState>) -> Response {
    let mut v = match st.service.api().webhook_get().await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    // 附带本机路径提示（面板拼公网地址用）
    v["localPath"] = json!(format!(
        "/webhook/kiroshop/{}",
        st.service.config().webhook_path_token
    ));
    v["signatureVerified"] = json!(!st.service.config().webhook_secret.is_empty());
    ok_json(v)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebhookPutBody {
    url: String,
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

/// PUT /webhook：设置回调。首次设置时卖家只回一次 secret —— 自动捕获并持久化，
/// 入站验签立即可用，不需要用户手工抄。
async fn webhook_put(State(st): State<KiroshopState>, Json(b): Json<WebhookPutBody>) -> Response {
    let url = b.url.trim();
    if !(url.starts_with("https://")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "回调地址必须是 https（卖家要求 443 端口、域名解析到公网）" })),
        )
            .into_response();
    }
    let mut resp = match st
        .service
        .api()
        .webhook_put(url, b.events.as_deref(), b.enabled)
        .await
    {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    capture_webhook_secret(&st, &mut resp, "PUT /webhook");
    ok_json(resp)
}

async fn webhook_delete(State(st): State<KiroshopState>) -> Response {
    match st.service.api().webhook_delete().await {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

async fn webhook_test(State(st): State<KiroshopState>) -> Response {
    match st.service.api().webhook_test().await {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

/// POST /webhook/rotate-secret：换签名密钥。新密钥同样只显示一次，自动落库。
async fn webhook_rotate(State(st): State<KiroshopState>) -> Response {
    let mut resp = match st.service.api().webhook_rotate_secret().await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    capture_webhook_secret(&st, &mut resp, "rotate-secret");
    ok_json(resp)
}

#[derive(Deserialize, Default)]
struct DeliveriesQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
}

async fn webhook_deliveries(State(st): State<KiroshopState>, Query(q): Query<DeliveriesQuery>) -> Response {
    match st
        .service
        .api()
        .webhook_deliveries(
            q.status.as_deref().filter(|s| !s.is_empty()),
            q.page.unwrap_or(1),
            q.size.unwrap_or(20),
        )
        .await
    {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

async fn webhook_retry(State(st): State<KiroshopState>, Path(id): Path<i64>) -> Response {
    match st.service.api().webhook_retry(id).await {
        Ok(v) => ok_json(v),
        Err(e) => err(e),
    }
}

/// 从 webhook 响应里抓 secret（只显示一次）并写进配置段
fn capture_webhook_secret(st: &KiroshopState, resp: &mut Value, from: &str) {
    let secret = ["data", "secret"]
        .iter()
        .try_fold(resp.clone(), |v, k| v.get(k).cloned().ok_or(()))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .or_else(|| resp.get("secret").and_then(Value::as_str).map(str::to_string))
        .filter(|s| !s.trim().is_empty());
    if let Some(secret) = secret {
        let mut cfg = st.service.config();
        if cfg.webhook_secret != secret {
            cfg.webhook_secret = secret.clone();
            let _ = st.service.update_config(cfg);
            tracing::info!("KiroShop：webhook 签名密钥已捕获并持久化（{from}）");
        }
        // 脱敏：不把密钥全文再吐给前端（已落库）
        if let Some(d) = resp.get_mut("data").and_then(|d| d.as_object_mut()) {
            d.insert("secret".into(), json!("已保存"));
        }
        if let Some(o) = resp.as_object_mut() {
            o.remove("secret");
        }
    }
}

// ============ 设置 ============

/// GET /settings：当前配置（apiKey 脱敏）
async fn settings_get(State(st): State<KiroshopState>) -> Response {
    let cfg = st.service.config();
    let masked = mask_key(&cfg.api_key);
    ok_json(json!({
        "data": {
            "enabled": cfg.enabled,
            "baseUrl": cfg.base_url,
            "apiKeyMasked": masked,
            "apiKeySet": !cfg.api_key.trim().is_empty(),
            "groups": cfg.groups,
            "rpmLimit": cfg.rpm_limit,
            "priority": cfg.priority,
            "creditLimit": cfg.credit_limit,
            "apiRegion": cfg.api_region,
            "eventsPollIntervalSecs": cfg.events_poll_interval_secs,
            "deliveryPollIntervalSecs": cfg.delivery_poll_interval_secs,
            "balanceAlertThreshold": cfg.balance_alert_threshold,
            "autoBuy": cfg.auto_buy,
            "webhookPath": format!("/webhook/kiroshop/{}", cfg.webhook_path_token),
            "webhookSecretSet": !cfg.webhook_secret.is_empty(),
        }
    }))
}

fn mask_key(key: &str) -> String {
    let k = key.trim();
    if k.len() <= 10 {
        return if k.is_empty() { String::new() } else { "***".into() };
    }
    format!("{}…{}", &k[..8], &k[k.len() - 4..])
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SettingsUpdate {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    rpm_limit: Option<u32>,
    #[serde(default)]
    priority: Option<u32>,
    #[serde(default)]
    credit_limit: Option<f64>,
    #[serde(default)]
    api_region: Option<String>,
    #[serde(default)]
    events_poll_interval_secs: Option<u64>,
    #[serde(default)]
    delivery_poll_interval_secs: Option<u64>,
    #[serde(default)]
    balance_alert_threshold: Option<f64>,
    #[serde(default)]
    auto_buy: Option<super::config::AutoBuyConfig>,
    #[serde(default)]
    webhook_secret: Option<String>,
}

/// PUT /settings：字段级更新，落 config.json 并热生效
async fn settings_put(State(st): State<KiroshopState>, Json(b): Json<SettingsUpdate>) -> Response {
    let mut cfg = st.service.config();
    if let Some(v) = b.enabled {
        cfg.enabled = v;
    }
    if let Some(v) = b.base_url.map(|s| s.trim().trim_end_matches('/').to_string()) {
        if !v.is_empty() {
            cfg.base_url = v;
        }
    }
    if let Some(v) = b.api_key.map(|s| s.trim().to_string()) {
        if !v.is_empty() {
            cfg.api_key = v;
        }
    }
    if let Some(v) = b.groups {
        cfg.groups = v;
    }
    if let Some(v) = b.rpm_limit.filter(|r| *r >= 1) {
        cfg.rpm_limit = v;
    }
    if let Some(v) = b.priority {
        cfg.priority = v;
    }
    match b.credit_limit {
        Some(v) if v > 0.0 => cfg.credit_limit = Some(v),
        Some(_) => cfg.credit_limit = None,
        None => {}
    }
    if let Some(v) = b.api_region.map(|s| s.trim().to_string()) {
        cfg.api_region = v;
    }
    if let Some(v) = b.events_poll_interval_secs.filter(|s| *s >= 10) {
        cfg.events_poll_interval_secs = v;
    }
    if let Some(v) = b.delivery_poll_interval_secs.filter(|s| *s >= 15) {
        cfg.delivery_poll_interval_secs = v;
    }
    match b.balance_alert_threshold {
        Some(v) if v > 0.0 => cfg.balance_alert_threshold = Some(v),
        Some(_) => cfg.balance_alert_threshold = None,
        None => {}
    }
    if let Some(v) = b.auto_buy {
        cfg.auto_buy = v;
    }
    if let Some(v) = b.webhook_secret.map(|s| s.trim().to_string()) {
        cfg.webhook_secret = v;
    }
    match st.service.update_config(cfg) {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "message": "设置已保存并即时生效" })),
        )
            .into_response(),
        Err(e) => err(e),
    }
}

// ============ 入站 webhook ============

/// POST /webhook/kiroshop/{token}：验签 → 落库去重 → 处理 → 200
async fn receive_webhook(
    State(st): State<KiroshopState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !st.service.webhook_token_matches(&token) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if body.len() > MAX_WEBHOOK_BODY {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let signature = headers
        .get("x-kiroshop-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !st.service.verify_webhook_signature(signature, &body) {
        tracing::warn!("KiroShop webhook 验签失败");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "签名校验失败" })),
        )
            .into_response();
    }
    let ev: ShopEvent = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("事件体解析失败: {e}") })),
            )
                .into_response();
        }
    };
    match st.service.handle_shop_event(&ev).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => {
            // 5xx 让卖家按 30s/2m/10m/... 重试 8 次；本地轮询也会兜底
            tracing::warn!(event = ev.id, "KiroShop webhook 处理失败: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// ============ 路由 ============

pub fn create_kiroshop_admin_router(state: KiroshopState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/products", get(products))
        .route("/quote", post(quote))
        .route("/buy", post(buy))
        .route("/orders", get(orders))
        .route("/orders/{id}", get(order_detail))
        .route("/orders/{id}/pay", post(pay))
        .route("/orders/{id}/cancel", post(cancel))
        .route("/orders/{id}/pickup", post(pickup))
        .route("/orders/{id}/import", post(import_delivery))
        .route("/orders/{id}/supplements", get(supplements))
        .route("/orders/{id}/supplements/{sid}/pickup", post(pickup_supplement))
        .route("/orders/{id}/after-sale", get(after_sale).post(after_sale_create))
        .route("/events", get(events))
        .route("/events/ack", post(ack_events))
        .route("/account", get(account))
        .route("/account/balance-logs", get(balance_logs))
        .route("/account/daily-limit", put(set_daily_limit))
        .route("/webhook", get(webhook_get).put(webhook_put).delete(webhook_delete))
        .route("/webhook/test", post(webhook_test))
        .route("/webhook/rotate-secret", post(webhook_rotate))
        .route("/webhook/deliveries", get(webhook_deliveries))
        .route("/webhook/deliveries/{id}/retry", post(webhook_retry))
        .route("/settings", get(settings_get).put(settings_put))
        .with_state(state)
}

/// 入站 webhook 路由（挂到根：`/webhook/kiroshop/{token}`）
pub fn create_kiroshop_webhook_router(state: KiroshopState) -> Router {
    Router::new()
        .route(
            "/webhook/kiroshop/{token}",
            post(receive_webhook).layer(axum::extract::DefaultBodyLimit::max(MAX_WEBHOOK_BODY)),
        )
        .with_state(state)
}
