//! 卖家对接 HTTP 处理器
//!
//! 分两组：
//! - 入站 webhook（`/webhook/vendor/{token}`）：无 API Key，靠不可猜测的路径 token
//!   校验，并据此反查归属哪一家卖家。落库 + 告警后立即返回；后续动作（失效确认、
//!   自动提取）一律异步派发，手动模式下不做任何扣费动作。
//! - 管理接口（`/api/admin/vendor/*`）：走 adminApiKey 认证，手动触发提取 / 兑换 /
//!   测试。多卖家用 `?vendorId=xxx` 指定目标，缺省落到配置里的第一家。
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

use super::registry::VendorRegistry;
use super::service::{VendorService, VendorServiceError};
use super::store::{
    DEFAULT_QUERY_LIMIT, IncomingEvent, PurchaseTrigger, RecordOutcome, VendorEventKind,
};

/// 入站 webhook 与管理接口共享的状态
#[derive(Clone)]
pub struct VendorState {
    pub registry: std::sync::Arc<VendorRegistry>,
}

impl VendorState {
    pub fn new(registry: std::sync::Arc<VendorRegistry>) -> Self {
        Self { registry }
    }
}

fn err_response(e: VendorServiceError) -> Response {
    err_response_with(e, &[])
}

/// 同 [`err_response`]，但往正文里额外并入若干字段。
///
/// 提取失败时用它带上订单号：钱可能已经扣了，而用**同一个订单号**原样重试是
/// 卖家提供的唯一安全取回手段，换号重试等于再扣一次。
fn err_response_with(e: VendorServiceError, extra: &[(&str, serde_json::Value)]) -> Response {
    let status = match &e {
        VendorServiceError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
        VendorServiceError::EventNotFound => StatusCode::NOT_FOUND,
        VendorServiceError::NotPurchasable => StatusCode::BAD_REQUEST,
        VendorServiceError::CountLocked { .. } => StatusCode::CONFLICT,
        VendorServiceError::UnknownZone { .. } => StatusCode::BAD_REQUEST,
        // 与卖家缺货同义，沿用它的 409
        VendorServiceError::NoZoneInStock => StatusCode::CONFLICT,
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
    // 让面板能把可选区域直接渲染成下拉项，不必再查一次库存
    if let VendorServiceError::UnknownZone { known, .. } = &e {
        body["knownZones"] = serde_json::json!(known);
    }
    for (k, v) in extra {
        body[*k] = v.clone();
    }
    (status, Json(body)).into_response()
}

/// 指定的 vendorId 查不到时的响应。
///
/// 刻意不回退到默认卖家：提取接口会真实扣费，对着错误的卖家下单是不可逆的。
fn unknown_vendor(vendor_id: Option<&str>) -> Response {
    let msg = match vendor_id {
        Some(id) => format!("找不到 id 为 {id} 的卖家配置"),
        None => "未配置任何卖家对接".to_string(),
    };
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

/// 所有管理接口共用的卖家选择参数
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VendorSelector {
    /// 目标卖家 id。缺省用配置里的第一家。
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// 按选择参数取出服务实例，取不到时返回现成的错误响应
fn pick<'a>(
    state: &'a VendorState,
    sel: &VendorSelector,
) -> Result<&'a std::sync::Arc<VendorService>, Response> {
    state
        .registry
        .resolve(sel.vendor_id.as_deref())
        .ok_or_else(|| unknown_vendor(sel.vendor_id.as_deref()))
}

// ============ 入站 webhook ============

/// `POST /webhook/vendor/{token}`
///
/// 认证：路径 token 常量时间比对，不匹配返回 404（不泄露端点是否存在）。
/// 多卖家时各家 token 不同，比对命中的那一家就是事件来源。
/// 语义：解析 → 按 `(vendor_id, event_id)` 幂等落库 → 立刻 200。
/// 不触发提取，不改凭据状态。
pub async fn receive_webhook(
    State(state): State<VendorState>,
    Path(token): Path<String>,
    body: Bytes,
) -> Response {
    // 不区分「未配置」「token 错」「哪一家」，统一 404
    let Some(service) = state.registry.find_by_path_token(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(event) = VendorService::parse_event(service.vendor_id(), service.flavor(), &body)
    else {
        tracing::warn!(
            vendor_id = %service.vendor_id(),
            "卖家 webhook payload 无法解析（{} 字节），已忽略",
            body.len()
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "payload 不是合法 JSON 对象" })),
        )
            .into_response();
    };

    match service.store().record_event(&event) {
        Ok(RecordOutcome::Inserted) => {
            tracing::info!(
                vendor_id = %event.vendor_id,
                event_id = %event.event_id,
                event = event.kind.as_str(),
                new_keys = ?event.new_keys,
                dead = ?event.dead,
                "收到卖家 webhook 事件"
            );
            // 只对首次收到的事件做后续动作，且全部异步 —— 提取加逐条验活可能耗时
            // 数十秒，同步执行会让卖家侧超时重投。
            dispatch_event(service, &event);
        }
        Ok(RecordOutcome::Duplicate) => {
            tracing::info!(
                vendor_id = %event.vendor_id,
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
/// - `key_revoked_abuse` / `test`：只落库，不派发任何动作。
fn dispatch_event(service: &std::sync::Arc<VendorService>, event: &IncomingEvent) {
    match event.kind {
        VendorEventKind::AllKeysDead => {
            service.spawn_dead_validation(event.event_id.clone());
        }
        VendorEventKind::NewKeysAvailable => {
            if !service.auto_purchase() {
                return;
            }
            if event.purchase_order_id.is_none() {
                tracing::info!(
                    vendor_id = %event.vendor_id,
                    event_id = %event.event_id,
                    "自动模式已开启，但事件缺少订单号，跳过自动提取"
                );
                return;
            }
            service.spawn_auto_purchase(event.event_id.clone(), event.new_keys);
        }
        // 滥用回收要人工介入（换号、排查调用方），程序不自动补货
        VendorEventKind::KeyRevokedAbuse => {
            tracing::warn!(
                vendor_id = %event.vendor_id,
                event_id = %event.event_id,
                "卖家通报密钥因滥用被回收，需人工处置"
            );
        }
        VendorEventKind::Test | VendorEventKind::Unknown => {}
    }
}

// ============ 管理接口 ============

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// `GET /api/admin/vendor/vendors` —— 卖家清单与各家能力集
///
/// 面板据此渲染顶部标签页，并按能力集决定隐藏哪些卡片。不发任何出站请求，
/// 保证标签页在卖家接口不可用时也能正常渲染。
pub async fn list_vendors(State(state): State<VendorState>) -> Response {
    let store = state.registry.all().first().map(|s| s.store());
    let items: Vec<serde_json::Value> = state
        .registry
        .all()
        .iter()
        .map(|s| {
            let unacked = store
                .and_then(|st| st.unacked_count(Some(s.vendor_id())).ok())
                .unwrap_or(0);
            serde_json::json!({
                "vendorId": s.vendor_id(),
                "name": s.display_name(),
                "flavor": s.flavor().as_str(),
                "capabilities": s.capabilities(),
                "inboundEnabled": s.config().inbound_enabled(),
                "autoPurchase": s.auto_purchase(),
                // 逐家独立：开着的只看自己，关着的按全局阈值判
                "perChannel": s.per_channel(),
                "unacked": unacked,
            })
        })
        .collect();

    Json(serde_json::json!({
        "vendors": items,
        "defaultVendorId": state.registry.default_service().map(|s| s.vendor_id()),
        // 全局提取限制。放在这里而不是按家查的 /status —— 它跨供应商，
        // 塞进单家状态会让「切换标签页后这个值变不变」变成一个需要解释的问题。
        "poolTarget": state.registry.pool_gate().target(),
        // 自动提取总闸。同为全局量，故与 poolTarget 并列。
        "autoPurchaseEnabled": state.registry.pool_gate().auto_enabled(),
    }))
    .into_response()
}

/// `GET /api/admin/vendor/events` —— 事件列表 + 未确认数
pub async fn list_events(State(state): State<VendorState>, Query(q): Query<ListQuery>) -> Response {
    let limit = q.limit.unwrap_or(DEFAULT_QUERY_LIMIT);
    let sel = VendorSelector {
        vendor_id: q.vendor_id.clone(),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let vid = service.vendor_id();
    let store = service.store();

    match (store.list_events(Some(vid), limit), store.unacked_count(Some(vid))) {
        (Ok(events), Ok(unacked)) => Json(serde_json::json!({
            "vendorId": vid,
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

/// `GET /api/admin/vendor/status` —— 顶部状态条：配置状态 + 余额 + 库存 +
/// 各家独有指标 + 未确认数
///
/// 出站请求按该卖家的能力集裁剪后并发发出；任一失败不影响其余字段
/// （各自返回对应 error 字段）。不支持的能力压根不发请求。
pub async fn get_status(State(state): State<VendorState>, Query(sel): Query<VendorSelector>) -> Response {
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let cfg = service.config();
    let caps = service.capabilities();
    let unacked = service
        .store()
        .unacked_count(Some(service.vendor_id()))
        .unwrap_or(0);

    let mut body = serde_json::json!({
        "vendorId": service.vendor_id(),
        "name": service.display_name(),
        "flavor": service.flavor().as_str(),
        "capabilities": caps,
        "configured": cfg.outbound_enabled(),
        "inboundEnabled": cfg.inbound_enabled(),
        "unacked": unacked,
        "defaultGroups": cfg.default_groups.clone(),
        "defaultRpmLimit": cfg.default_rpm_limit,
        "defaultApiRegion": cfg.default_api_region.clone(),
        "defaultAuthRegion": cfg.default_auth_region.clone(),
        // 运行时值，可能已被面板改过，与 config.json 的启动快照不一定一致
        "autoPurchase": service.auto_purchase(),
        // 当前时刻实际生效的上限（已应用时段表），面板展示与判定必须用它，
        // 否则配了时段表后显示的数与真实行为不一致
        "autoPurchaseMaxCount": service.auto_max_count(),
        // 未命中任何时段时的兜底值，用于在面板上区分「按时段」还是「按默认」
        "autoPurchaseBaseMaxCount": cfg.auto_purchase_max_count,
        "autoPurchaseWindow": service.auto_active_window(),
        // 逐渠道补货（运行时值）。开着则本家只看自己有没有存活 Key，
        // 不看全局池量；关着则按 autoPurchasePoolTarget 判总量
        "autoPurchasePerChannel": service.per_channel(),
        // 库存轮询：给没有 webhook 的家（kirored / kiroapp-cc）补上自动提取的触发源。
        // 0 表示未启用。透出实际生效值（已抬到下限）而非配置原值 —— 面板显示一个
        // 与真实节奏不符的间隔，会让人误判「怎么没按我配的频率查」。
        "stockPollIntervalSecs": service.stock_poll_interval(),
        // 轮询是否遵循全局总闸。关着时总闸停了也继续发现新车（但仍不会自动下单）
        // 必须读**运行时值**而非 cfg 的启动快照：面板切换后 config.json 与 AtomicBool
        // 都已更新，但 cfg 是进程启动时那份，永远返回旧值 —— 症状是开关点了就弹回去，
        // 看着像「关闭失败」，而请求其实全成功了。同 autoPurchase / perChannel。
        "stockPollRespectGlobalGate": service.stock_poll_respect_gate(),
    });

    // 库存与档案两家都有；其余按能力集选择性发起
    let (profile, stock) = tokio::join!(service.profile(), service.stock());
    match profile {
        Ok(p) => body["profile"] = serde_json::to_value(&p).unwrap_or_default(),
        Err(e) => body["profileError"] = serde_json::json!(e.to_string()),
    }
    match stock {
        Ok(s) => {
            // stockMax 保留旧字段名，面板既有逻辑不必改
            body["stockMax"] = serde_json::json!(s.available);
            body["stock"] = serde_json::to_value(&s).unwrap_or_default();
        }
        Err(e) => body["stockError"] = serde_json::json!(e.to_string()),
    }

    if caps.system_status {
        match service.system_status().await {
            Ok(s) => body["system"] = serde_json::to_value(&s).unwrap_or_default(),
            Err(e) => body["systemError"] = serde_json::json!(e.to_string()),
        }
    }
    if caps.gen_logs {
        match service.gen_logs().await {
            Ok(g) => body["genLogs"] = serde_json::to_value(&g).unwrap_or_default(),
            Err(e) => body["genLogsError"] = serde_json::json!(e.to_string()),
        }
    }
    if caps.earliest_key {
        match service.earliest_key().await {
            Ok(k) => body["earliestKey"] = serde_json::to_value(&k).unwrap_or_default(),
            Err(e) => body["earliestKeyError"] = serde_json::json!(e.to_string()),
        }
    }

    Json(body).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagedQuery {
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

/// `GET /api/admin/vendor/orders` —— 卖家侧历史提取订单
pub async fn list_orders(State(state): State<VendorState>, Query(q): Query<PagedQuery>) -> Response {
    let sel = VendorSelector {
        vendor_id: q.vendor_id.clone(),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service.purchase_orders(q.page, q.page_size).await {
        // orders 保留旧字段名（数组），并额外给出分页信息
        Ok(paged) => Json(serde_json::json!({
            "orders": paged.items,
            "total": paged.total,
            "page": paged.page,
            "pageSize": paged.page_size,
            "pages": paged.pages,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerQuery {
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    /// 按变动类型过滤，如 `purchase_debit`
    #[serde(default)]
    pub r#type: Option<String>,
}

/// `GET /api/admin/vendor/ledger` —— 积分流水（仅支持该能力的卖家）
pub async fn list_ledger(State(state): State<VendorState>, Query(q): Query<LedgerQuery>) -> Response {
    let sel = VendorSelector {
        vendor_id: q.vendor_id.clone(),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service
        .ledger(q.page, q.page_size, q.r#type.as_deref())
        .await
    {
        Ok(paged) => Json(paged).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MyKeysQuery {
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    /// 是否包含已失效的密钥
    #[serde(default)]
    pub history: Option<bool>,
}

/// `GET /api/admin/vendor/keys` —— 卖家侧名下密钥（仅支持该能力的卖家）
///
/// 用途是对账与判断 Key 新鲜度（卖家的库存接口不给任何时间字段，
/// 这里的 `createdAt` 是开号时刻）。响应里含密钥明文，仅在管理接口内使用。
pub async fn list_my_keys(State(state): State<VendorState>, Query(q): Query<MyKeysQuery>) -> Response {
    let sel = VendorSelector {
        vendor_id: q.vendor_id.clone(),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service
        .my_keys(q.history.unwrap_or(false), q.page, q.page_size)
        .await
    {
        Ok(paged) => Json(paged).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseRequest {
    /// 希望提取的数量。事件已绑定过其它数量时返回 409 并带上 boundCount。
    pub count: u32,
    #[serde(default)]
    pub vendor_id: Option<String>,
    /// 指定区域（分区卖家）。留空则自动选「开放有货中单价最低」的区。
    /// 事件已绑定过区域时以绑定值为准，本字段被忽略。
    #[serde(default)]
    pub zone: Option<String>,
}

/// `POST /api/admin/vendor/events/{event_id}/purchase` —— 按事件提取并入库
pub async fn purchase_for_event(
    State(state): State<VendorState>,
    Path(event_id): Path<String>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<PurchaseRequest>,
) -> Response {
    if req.count == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "提取数量必须大于 0" })),
        )
            .into_response();
    }
    // body 里的 vendorId 优先，其次 query
    let sel = VendorSelector {
        vendor_id: req.vendor_id.clone().or(sel.vendor_id),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service
        .purchase_for_event_zoned(
            &event_id,
            req.count,
            req.zone.as_deref(),
            PurchaseTrigger::Manual,
        )
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
    #[serde(default)]
    pub vendor_id: Option<String>,
    /// 指定区域（分区卖家）。留空则自动选「开放有货中单价最低」的区。
    /// 本路径不落库，重试时须自行带上响应回显的 zone。
    #[serde(default)]
    pub zone: Option<String>,
}

/// `POST /api/admin/vendor/purchase` —— 不依赖事件的主动提取
pub async fn purchase_ad_hoc(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<AdHocPurchaseRequest>,
) -> Response {
    if req.count == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "提取数量必须大于 0" })),
        )
            .into_response();
    }
    let sel = VendorSelector {
        vendor_id: req.vendor_id.clone().or(sel.vendor_id),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let order_id = req
        .client_order_id
        .filter(|s| is_hex32(s))
        .unwrap_or_else(new_order_id);

    // 下单**前**先把订单号落日志。这条路径不写事件表，订单号只存在于本次请求的
    // 内存里 —— 若下单超时或断连，卖家侧很可能已扣费并锁定 Key，而订单号一丢
    // 就再也取不回那笔单（重试要用同一个号才会命中幂等重放）。日志是唯一线索。
    tracing::info!(
        vendor_id = %service.vendor_id(),
        order_id = %order_id,
        count = req.count,
        "主动提取开始"
    );

    match service
        .purchase_ad_hoc(req.count, &order_id, req.zone.as_deref())
        .await
    {
        Ok(result) => {
            let mut body = serde_json::to_value(&result).unwrap_or_default();
            body["clientOrderId"] = serde_json::json!(order_id);
            body["vendorId"] = serde_json::json!(service.vendor_id());
            Json(body).into_response()
        }
        Err(e) => {
            // 失败也必须回显订单号：钱可能已经扣了，面板要能提示「用这个号原样
            // 重试」。换号重试等于再扣一次，而本家没有订单列表可供事后对账。
            tracing::warn!(
                vendor_id = %service.vendor_id(),
                order_id = %order_id,
                "主动提取失败（若为超时/断连，卖家侧可能已扣费，请用同一订单号重试）: {}",
                e
            );
            err_response_with(
                e,
                &[
                    ("clientOrderId", serde_json::json!(order_id)),
                    ("vendorId", serde_json::json!(service.vendor_id())),
                ],
            )
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeRequest {
    /// true = 自动提取，false = 手动提取
    pub auto_purchase: bool,
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// `PUT /api/admin/vendor/mode` —— 切换某家的提取模式
///
/// 未配置卖家对接时拒绝：开了也没有出站能力，只会给出一个误导性的「自动」状态。
pub async fn set_mode(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<SetModeRequest>,
) -> Response {
    let sel = VendorSelector {
        vendor_id: req.vendor_id.clone().or(sel.vendor_id),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if !service.config().outbound_enabled() {
        return err_response(VendorServiceError::NotConfigured);
    }
    let result = service.set_auto_purchase(req.auto_purchase);
    tracing::info!(
        vendor_id = %service.vendor_id(),
        auto_purchase = result.auto_purchase,
        persisted = result.persisted,
        "提取模式已切换"
    );
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPoolTargetRequest {
    /// 池中存活卖家 Key 达到此数即不再自动补货。0 = 不启用
    pub pool_target: u32,
}

/// `PUT /api/admin/vendor/pool-target` —— 设置全局提取限制
///
/// 与 [`set_mode`] 的两点不同，都源于「这是全局设置」：
/// - 不接受 `vendorId`，也不 `pick()` 某一家 —— 阈值跨供应商共享。
/// - 不校验 `outbound_enabled` —— 全局约束与某一家配没配对接无关。
///
/// 仍需至少有一家已注册：持久化要借用服务持有的配置路径，且没有任何卖家时
/// 这个设置也无从生效。
pub async fn set_pool_target(
    State(state): State<VendorState>,
    Json(req): Json<SetPoolTargetRequest>,
) -> Response {
    let Some(service) = state.registry.default_service() else {
        return err_response(VendorServiceError::NotConfigured);
    };
    let result = service.set_pool_target(req.pool_target);
    tracing::info!(
        pool_target = result.pool_target,
        persisted = result.persisted,
        "全局提取限制已更新"
    );
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAutoEnabledRequest {
    /// false = 全局关闭自动提取，任何家都不再自动下单
    pub auto_purchase_enabled: bool,
}

/// `PUT /api/admin/vendor/auto-purchase-enabled` —— 切换自动提取总闸
///
/// 与 [`set_pool_target`] 同一套写法，理由相同（这是全局设置）：
/// - 不接受 `vendorId`，也不 `pick()` 某一家 —— 总闸跨供应商。
/// - 不校验 `outbound_enabled` —— 全局约束与某一家配没配对接无关。
///
/// 与 [`set_mode`] 的区别在于范围：那个改某一家的模式，这个压住所有家，
/// 且**不修改**各家的 `autoPurchase`（重开后各家回到原模式）。
pub async fn set_auto_purchase_enabled(
    State(state): State<VendorState>,
    Json(req): Json<SetAutoEnabledRequest>,
) -> Response {
    let Some(service) = state.registry.default_service() else {
        return err_response(VendorServiceError::NotConfigured);
    };
    let result = service.set_auto_purchase_enabled(req.auto_purchase_enabled);
    tracing::info!(
        auto_purchase_enabled = result.auto_purchase_enabled,
        persisted = result.persisted,
        "自动提取总闸已切换"
    );
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPerChannelRequest {
    /// true = 逐渠道补货（本家无存活即补），false = 全局阈值
    pub per_channel: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetStockPollRespectGateRequest {
    /// true = 轮询遵循全局总闸，false = 总闸关着也继续轮询
    pub respect: bool,
}

/// `PUT /api/admin/vendor/per-channel?vendorId=xxx` —— 设置**某一家**的逐渠道补货
///
/// 与 `set_pool_target` 不同，这是**逐家**设置，要认 `vendorId` —— 每家可以各自
/// 决定是「只看自己」还是「按全局总量」，混合配置是本特性的用法而非误配。
pub async fn set_per_channel(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<SetPerChannelRequest>,
) -> Response {
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let result = service.set_per_channel(req.per_channel);
    tracing::info!(
        vendor_id = %service.vendor_id(),
        per_channel = result.per_channel,
        persisted = result.persisted,
        "逐渠道补货已更新"
    );
    Json(result).into_response()
}

/// `PUT /api/admin/vendor/stock-poll-respect-gate?vendorId=xxx` —— 设置库存轮询是否遵循全局总闸
///
/// 与 `set_per_channel` 一样是**逐家**设置 —— 每家可以各自决定遵不遵循总闸。
pub async fn set_stock_poll_respect_gate(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<SetStockPollRespectGateRequest>,
) -> Response {
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let result = service.set_stock_poll_respect_gate(req.respect);
    tracing::info!(
        vendor_id = %service.vendor_id(),
        respect = result.respect,
        persisted = result.persisted,
        "库存轮询总闸遵循已更新"
    );
    Json(result).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckRequest {
    /// 指定事件 ID；留空表示该卖家全部标记已知悉
    #[serde(default, alias = "event_id")]
    pub event_id: Option<String>,
    /// 目标卖家；留空表示所有卖家全部标记
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// `POST /api/admin/vendor/events/ack` —— 标记事件已知悉（消红点）
pub async fn ack_events(State(state): State<VendorState>, Json(req): Json<AckRequest>) -> Response {
    // 未指定卖家时对所有卖家生效（面板「全部已读」），此时任取一个 store 即可 ——
    // 各家共用同一个事件库
    let store = match state.registry.all().first() {
        Some(s) => s.store(),
        None => return unknown_vendor(None),
    };
    // 指定了卖家就必须存在，避免静默无操作让用户以为已读成功
    if let Some(id) = req.vendor_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && state.registry.get(id).is_none()
    {
        return unknown_vendor(Some(id));
    }

    match store.ack(req.vendor_id.as_deref(), req.event_id.as_deref()) {
        Ok(n) => Json(serde_json::json!({ "acked": n })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemRequest {
    pub code: String,
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// `POST /api/admin/vendor/redeem` —— 兑换码充值
pub async fn redeem(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<RedeemRequest>,
) -> Response {
    let sel = VendorSelector {
        vendor_id: req.vendor_id.clone().or(sel.vendor_id),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service.redeem(req.code.trim()).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => err_response(e),
    }
}

/// `POST /api/admin/vendor/webhook/test` —— 让卖家往已保存 URL 推一条测试消息
pub async fn test_webhook(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
) -> Response {
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service.test_webhook().await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetWebhookRequest {
    pub webhook_url: String,
    #[serde(default)]
    pub vendor_id: Option<String>,
}

/// `PUT /api/admin/vendor/webhook` —— 更新卖家侧保存的 webhook URL
pub async fn set_webhook_url(
    State(state): State<VendorState>,
    Query(sel): Query<VendorSelector>,
    Json(req): Json<SetWebhookRequest>,
) -> Response {
    let sel = VendorSelector {
        vendor_id: req.vendor_id.clone().or(sel.vendor_id),
    };
    let service = match pick(&state, &sel) {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    match service.set_webhook_url(req.webhook_url.trim()).await {
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

    /// 前端既有代码发的是 `{"event_id": "..."}`（snake_case），
    /// 新代码发 camelCase，两者都要能解析，否则「已读」会静默失效
    #[test]
    fn ack请求兼容两种字段名() {
        let snake: AckRequest = serde_json::from_str(r#"{"event_id":"e1"}"#).unwrap();
        assert_eq!(snake.event_id.as_deref(), Some("e1"));

        let camel: AckRequest = serde_json::from_str(r#"{"eventId":"e2"}"#).unwrap();
        assert_eq!(camel.event_id.as_deref(), Some("e2"));

        let empty: AckRequest = serde_json::from_str("{}").unwrap();
        assert!(empty.event_id.is_none());
        assert!(empty.vendor_id.is_none());
    }

    #[test]
    fn 选择器缺省与空串等价() {
        let none: VendorSelector = serde_json::from_str("{}").unwrap();
        assert!(none.vendor_id.is_none());
        let given: VendorSelector = serde_json::from_str(r#"{"vendorId":"kiroapp"}"#).unwrap();
        assert_eq!(given.vendor_id.as_deref(), Some("kiroapp"));
    }
}
