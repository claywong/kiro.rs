//! 卖家对接 HTTP 处理器
//!
//! 分两组：
//! - 入站 webhook（`/webhook/vendor/{token}`）：无 API Key，靠不可猜测的路径 token 校验。
//!   落库 + 告警后立即返回；后续动作（失效确认、自动提取）一律异步派发，
//!   手动模式下不做任何扣费动作。
//! - 管理接口（`/api/admin/vendor/*`）：走 adminApiKey 认证，手动触发提取 / 兑换 / 测试。
//!
//! @author wangzhong

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::service::{VendorService, VendorServiceError};
use super::store::{DEFAULT_QUERY_LIMIT, IncomingEvent, RecordOutcome, VendorEventKind};

/// 入站 webhook 与管理接口共享的状态
#[derive(Clone)]
pub struct VendorState {
    pub service: std::sync::Arc<VendorService>,
}

impl VendorState {
    pub fn new(service: std::sync::Arc<VendorService>) -> Self {
        Self { service }
    }
}

fn err_response(e: VendorServiceError) -> Response {
    let status = match &e {
        VendorServiceError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
        VendorServiceError::EventNotFound => StatusCode::NOT_FOUND,
        VendorServiceError::NotPurchasable => StatusCode::BAD_REQUEST,
        VendorServiceError::CountLocked { .. } => StatusCode::CONFLICT,
        VendorServiceError::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        // 原样透出卖家状态码（403 余额不足 / 404 无可用 Key / 409 数量冲突）
        VendorServiceError::Upstream(u) => u
            .status
            .and_then(|c| StatusCode::from_u16(c).ok())
            .unwrap_or(StatusCode::BAD_GATEWAY),
    };
    let bound = match &e {
        VendorServiceError::CountLocked { bound } => Some(*bound),
        _ => None,
    };
    let mut body = serde_json::json!({ "error": e.to_string() });
    if let Some(b) = bound {
        body["boundCount"] = serde_json::json!(b);
    }
    (status, Json(body)).into_response()
}

// ============ 入站 webhook ============

/// `POST /webhook/vendor/{token}`
///
/// 认证：路径 token 常量时间比对，不匹配返回 404（不泄露端点是否存在）。
/// 语义：解析 → 按 `event_id` 幂等落库 → 立刻 200。不触发提取，不改凭据状态。
pub async fn receive_webhook(
    State(state): State<VendorState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    if !state.service.verify_path_token(&token) {
        // 不区分「未配置」与「token 错」，统一 404
        return StatusCode::NOT_FOUND.into_response();
    }

    let Some(event) = VendorService::parse_event(&body) else {
        tracing::warn!(
            "卖家 webhook payload 无法解析（{} 字节），已忽略",
            body.len()
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "payload 不是合法 JSON 对象" })),
        )
            .into_response();
    };

    match state.service.store().record_event(&event) {
        Ok(RecordOutcome::Inserted) => {
            tracing::info!(
                event_id = %event.event_id,
                event = event.kind.as_str(),
                new_keys = ?event.new_keys,
                dead = ?event.dead,
                "收到卖家 webhook 事件"
            );
            // 只对首次收到的事件做后续动作，且全部异步 —— 提取加逐条验活可能耗时
            // 数十秒，同步执行会让卖家侧超时重投。
            dispatch_event(&state, &event);
        }
        Ok(RecordOutcome::Duplicate) => {
            tracing::info!(
                event_id = %event.event_id,
                "卖家 webhook 重投，已忽略（幂等）"
            );
        }
        Err(e) => {
            tracing::error!("卖家 webhook 落库失败 event_id={}: {}", event.event_id, e);
            // 仍返回 200：落库失败让对方无限重投没有意义，日志已留痕
        }
    }

    (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
}

/// 按事件类型派发后台动作。
///
/// - `all_keys_dead`：启动失效确认观察窗口。与提取模式无关 —— 手动模式下这个
///   结论也是有用的诊断信息，且用户可能随时切到自动。
/// - `new_keys_available`：仅自动模式下尝试提取，且需通过失效确认。
fn dispatch_event(state: &VendorState, event: &IncomingEvent) {
    match event.kind {
        VendorEventKind::AllKeysDead => {
            state
                .service
                .spawn_dead_validation(event.event_id.clone());
        }
        VendorEventKind::NewKeysAvailable => {
            if !state.service.auto_purchase() {
                return;
            }
            if event.purchase_order_id.is_none() {
                tracing::info!(
                    event_id = %event.event_id,
                    "自动模式已开启，但事件缺少订单号，跳过自动提取"
                );
                return;
            }
            state
                .service
                .spawn_auto_purchase(event.event_id.clone(), event.new_keys);
        }
        VendorEventKind::Unknown => {}
    }
}

// ============ 管理接口 ============

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/admin/vendor/events` —— 事件列表 + 未确认数
pub async fn list_events(
    State(state): State<VendorState>,
    Query(q): Query<ListQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    let store = state.service.store();
    match (store.list_events(limit), store.unacked_count()) {
        (Ok(events), Ok(unacked)) => Json(serde_json::json!({
            "events": events,
            "unacked": unacked,
        }))
        .into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/admin/vendor/status` —— 顶部状态条：配置状态 + 余额 + 库存 + 存货 +
/// 开号记录 + 未确认数
///
/// 四个出站请求并发发出；任一失败不影响其余字段（各自返回对应 error 字段）。
pub async fn get_status(State(state): State<VendorState>) -> Response {
    let cfg = state.service.config();
    let configured = cfg.map(|c| c.outbound_enabled()).unwrap_or(false);
    let inbound = cfg.map(|c| c.inbound_enabled()).unwrap_or(false);
    let unacked = state.service.store().unacked_count().unwrap_or(0);

    let mut body = serde_json::json!({
        "configured": configured,
        "inboundEnabled": inbound,
        "unacked": unacked,
        "defaultGroups": cfg.map(|c| c.default_groups.clone()).unwrap_or_default(),
        "defaultPurchaseCost": cfg.and_then(|c| c.default_purchase_cost),
        "defaultRpmLimit": cfg.map(|c| c.default_rpm_limit).unwrap_or(10),
        // 运行时值，可能已被面板改过，与 config.json 的启动快照不一定一致
        "autoPurchase": state.service.auto_purchase(),
        // 当前时刻实际生效的上限（已应用时段表），面板展示与判定必须用它，
        // 否则配了时段表后显示的数与真实行为不一致
        "autoPurchaseMaxCount": state.service.auto_max_count(),
        // 未命中任何时段时的兜底值，用于在面板上区分「按时段」还是「按默认」
        "autoPurchaseBaseMaxCount": cfg.map(|c| c.auto_purchase_max_count).unwrap_or(1),
        "autoPurchaseWindow": state.service.auto_active_window(),
    });

    if !configured {
        return Json(body).into_response();
    }

    let (profile, stock, system, gen_logs) = tokio::join!(
        state.service.profile(),
        state.service.stock(),
        state.service.system_status(),
        state.service.gen_logs(),
    );

    match profile {
        Ok(p) => body["profile"] = serde_json::to_value(&p).unwrap_or_default(),
        Err(e) => body["profileError"] = serde_json::json!(e.to_string()),
    }
    match stock {
        Ok(max) => body["stockMax"] = serde_json::json!(max),
        Err(e) => body["stockError"] = serde_json::json!(e.to_string()),
    }
    match system {
        Ok(s) => body["system"] = serde_json::to_value(&s).unwrap_or_default(),
        Err(e) => body["systemError"] = serde_json::json!(e.to_string()),
    }
    match gen_logs {
        Ok(g) => body["genLogs"] = serde_json::to_value(&g).unwrap_or_default(),
        Err(e) => body["genLogsError"] = serde_json::json!(e.to_string()),
    }

    Json(body).into_response()
}

/// `GET /api/admin/vendor/orders` —— 卖家侧最近 50 条提取订单
pub async fn list_orders(State(state): State<VendorState>) -> Response {
    match state.service.purchase_orders().await {
        Ok(orders) => Json(serde_json::json!({ "orders": orders })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseRequest {
    /// 希望提取的数量。事件已绑定过其它数量时返回 409 并带上 boundCount。
    pub count: u32,
}

/// `POST /api/admin/vendor/events/{event_id}/purchase` —— 按事件提取并入库
pub async fn purchase_for_event(
    State(state): State<VendorState>,
    Path(event_id): Path<String>,
    Json(req): Json<PurchaseRequest>,
) -> Response {
    if req.count == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "提取数量必须大于 0" })),
        )
            .into_response();
    }
    match state
        .service
        .purchase_for_event(&event_id, req.count)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdHocPurchaseRequest {
    pub count: u32,
    /// 32 位十六进制订单号。留空则服务端生成一个。
    #[serde(default)]
    pub client_order_id: Option<String>,
}

/// `POST /api/admin/vendor/purchase` —— 不依赖事件的主动提取
pub async fn purchase_ad_hoc(
    State(state): State<VendorState>,
    Json(req): Json<AdHocPurchaseRequest>,
) -> Response {
    if req.count == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "提取数量必须大于 0" })),
        )
            .into_response();
    }
    let order_id = req
        .client_order_id
        .filter(|s| is_hex32(s))
        .unwrap_or_else(new_order_id);

    match state.service.purchase_ad_hoc(req.count, &order_id).await {
        Ok(result) => {
            let mut body = serde_json::to_value(&result).unwrap_or_default();
            body["clientOrderId"] = serde_json::json!(order_id);
            Json(body).into_response()
        }
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeRequest {
    /// true = 自动提取，false = 手动提取
    pub auto_purchase: bool,
}

/// `PUT /api/admin/vendor/mode` —— 切换提取模式
///
/// 未配置卖家对接时拒绝：开了也没有出站能力，只会给出一个误导性的「自动」状态。
pub async fn set_mode(
    State(state): State<VendorState>,
    Json(req): Json<SetModeRequest>,
) -> Response {
    if !state
        .service
        .config()
        .map(|c| c.outbound_enabled())
        .unwrap_or(false)
    {
        return err_response(VendorServiceError::NotConfigured);
    }
    let result = state.service.set_auto_purchase(req.auto_purchase);
    tracing::info!(
        auto_purchase = result.auto_purchase,
        persisted = result.persisted,
        "提取模式已切换"
    );
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
pub struct AckRequest {
    /// 指定事件 ID；留空表示全部标记已知悉
    #[serde(default)]
    pub event_id: Option<String>,
}

/// `POST /api/admin/vendor/events/ack` —— 标记事件已知悉（消红点）
pub async fn ack_events(State(state): State<VendorState>, Json(req): Json<AckRequest>) -> Response {
    match state.service.store().ack(req.event_id.as_deref()) {
        Ok(n) => Json(serde_json::json!({ "acked": n })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct RedeemRequest {
    pub code: String,
}

/// `POST /api/admin/vendor/redeem` —— 兑换码充值
pub async fn redeem(State(state): State<VendorState>, Json(req): Json<RedeemRequest>) -> Response {
    match state.service.redeem(req.code.trim()).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err_response(e),
    }
}

/// `POST /api/admin/vendor/webhook/test` —— 让卖家往已保存 URL 推一条测试消息
pub async fn test_webhook(State(state): State<VendorState>) -> Response {
    match state.service.test_webhook().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetWebhookRequest {
    pub webhook_url: String,
}

/// `PUT /api/admin/vendor/webhook` —— 更新卖家侧保存的 webhook URL
pub async fn set_webhook_url(
    State(state): State<VendorState>,
    Json(req): Json<SetWebhookRequest>,
) -> Response {
    match state.service.set_webhook_url(req.webhook_url.trim()).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => err_response(e),
    }
}

/// 生成 32 位十六进制订单号
fn new_order_id() -> String {
    let a = uuid::Uuid::new_v4();
    a.simple().to_string()
}

/// 校验是否为 32 位十六进制串
fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 订单号格式校验() {
        assert!(is_hex32("0123456789abcdef0123456789abcdef"));
        assert!(is_hex32("0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_hex32("0123456789abcdef"));
        assert!(!is_hex32("0123456789abcdef0123456789abcdeg"));
        assert!(!is_hex32(""));
    }

    #[test]
    fn 生成的订单号符合格式() {
        for _ in 0..20 {
            assert!(is_hex32(&new_order_id()));
        }
    }
}
