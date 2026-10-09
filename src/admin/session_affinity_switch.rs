//! 会话亲和总开关的 Admin API（本地运维便利特性）
//!
//! - `GET  /api/admin/config/session-affinity` → `{ "enabled": bool }`
//! - `PUT  /api/admin/config/session-affinity`，请求体 `{ "enabled": bool }`
//!
//! 开关语义见 `kiro::token_manager::session_affinity_switch`：关闭不清空现有绑定。
//! 独立成文件而不是塞进 `handlers.rs` / `types.rs` / `service.rs`，是为了合并上游时
//! 不与这些大文件的改动相撞。
//!
//! @author wangzhong

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::middleware::AdminState;
use super::types::AdminErrorResponse;

/// 会话亲和开关响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAffinityResponse {
    /// 总开关是否开启
    pub enabled: bool,
}

/// 设置会话亲和开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionAffinityRequest {
    pub enabled: bool,
}

/// GET /api/admin/config/session-affinity
pub async fn get_session_affinity(State(state): State<AdminState>) -> impl IntoResponse {
    Json(SessionAffinityResponse {
        enabled: state.service.token_manager().is_session_affinity_enabled(),
    })
}

/// PUT /api/admin/config/session-affinity
pub async fn set_session_affinity(
    State(state): State<AdminState>,
    Json(payload): Json<SetSessionAffinityRequest>,
) -> impl IntoResponse {
    let manager = state.service.token_manager();
    match manager.set_session_affinity_enabled(payload.enabled) {
        Ok(()) => Json(SessionAffinityResponse {
            enabled: manager.is_session_affinity_enabled(),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(AdminErrorResponse::new("internal_error", e.to_string())),
        )
            .into_response(),
    }
}
