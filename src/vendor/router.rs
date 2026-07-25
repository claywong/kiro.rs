//! 卖家对接路由
//!
//! @author wangzhong

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post, put},
};

use super::handlers::{
    VendorState, ack_events, get_status, list_events, purchase_ad_hoc, purchase_for_event, receive_webhook,
    redeem, set_webhook_url, test_webhook, list_orders,
};

/// 入站 webhook 请求体上限（64KB）。卖家 payload 只有几百字节，不需要给到
/// Anthropic 路由那种 50MB。
const MAX_WEBHOOK_BODY_SIZE: usize = 64 * 1024;

/// 入站 webhook 路由（挂在 `/webhook` 下，无 API Key 认证，靠路径 token 校验）
pub fn create_vendor_webhook_router(state: VendorState) -> Router {
    Router::new()
        .route("/vendor/{token}", post(receive_webhook))
        .layer(DefaultBodyLimit::max(MAX_WEBHOOK_BODY_SIZE))
        .with_state(state)
}

/// 管理接口路由（由调用方 nest 到 `/api/admin/vendor` 并套 adminApiKey 认证）
pub fn create_vendor_admin_router(state: VendorState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/events", get(list_events))
        .route("/events/ack", post(ack_events))
        .route("/events/{event_id}/purchase", post(purchase_for_event))
        .route("/purchase", post(purchase_ad_hoc))
        .route("/orders", get(list_orders))
        .route("/redeem", post(redeem))
        .route("/webhook", put(set_webhook_url))
        .route("/webhook/test", post(test_webhook))
        .with_state(state)
}
