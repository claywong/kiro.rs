//! 会话亲和总开关的 Admin API（本地运维便利特性）
//!
//! - `GET  /api/admin/config/session-affinity`
//!   → `{ "enabled": bool, "rpmMultiplier": f64, "capMaxWaitMs": u64, "overflowEnabled": bool }`
//! - `PUT  /api/admin/config/session-affinity`，请求体同上，各字段可选（缺省表示不变）
//!
//! 开关语义见 `kiro::token_manager::session_affinity_switch`：关闭不清空现有绑定。
//! `rpmMultiplier` 是亲和命中的 RPM 放宽倍数，`capMaxWaitMs` / `overflowEnabled` 是触顶后的
//! 处理，见 `affinity_rpm.rs`。
//! 独立成文件而不是塞进 `handlers.rs` / `types.rs` / `service.rs`，是为了合并上游时
//! 不与这些大文件的改动相撞。
//!
//! @author wangzhong

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use super::middleware::AdminState;
use super::types::AdminErrorResponse;
use crate::kiro::token_manager::{
    SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS, SESSION_AFFINITY_RPM_MULTIPLIER_RANGE,
};

/// 会话亲和开关响应
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAffinityResponse {
    /// 总开关是否开启
    pub enabled: bool,
    /// 亲和命中时 RPM 允许突破到的倍数
    pub rpm_multiplier: f64,
    /// 亲和触顶时服务端等待名额释放的上限（毫秒）
    pub cap_max_wait_ms: u64,
    /// 亲和触顶且无法短等时是否溢出到其它凭据
    pub overflow_enabled: bool,
}

/// 设置会话亲和开关请求
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionAffinityRequest {
    pub enabled: Option<bool>,
    pub rpm_multiplier: Option<f64>,
    pub cap_max_wait_ms: Option<u64>,
    pub overflow_enabled: Option<bool>,
}

fn current_state(manager: &crate::kiro::token_manager::MultiTokenManager) -> SessionAffinityResponse {
    SessionAffinityResponse {
        enabled: manager.is_session_affinity_enabled(),
        rpm_multiplier: manager.session_affinity_rpm_multiplier(),
        cap_max_wait_ms: manager.session_affinity_cap_max_wait_ms(),
        overflow_enabled: manager.is_session_affinity_overflow_enabled(),
    }
}

fn bad_request(message: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(AdminErrorResponse::invalid_request(message)),
    )
        .into_response()
}

fn internal_error(e: anyhow::Error) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(AdminErrorResponse::new("internal_error", e.to_string())),
    )
        .into_response()
}

/// GET /api/admin/config/session-affinity
pub async fn get_session_affinity(State(state): State<AdminState>) -> impl IntoResponse {
    Json(current_state(state.service.token_manager()))
}

/// PUT /api/admin/config/session-affinity
pub async fn set_session_affinity(
    State(state): State<AdminState>,
    Json(payload): Json<SetSessionAffinityRequest>,
) -> impl IntoResponse {
    if payload
        .rpm_multiplier
        .is_some_and(|m| !SESSION_AFFINITY_RPM_MULTIPLIER_RANGE.contains(&m))
    {
        return bad_request(format!(
            "rpmMultiplier 须在 {} ~ {} 之间",
            SESSION_AFFINITY_RPM_MULTIPLIER_RANGE.start(),
            SESSION_AFFINITY_RPM_MULTIPLIER_RANGE.end()
        ));
    }
    if payload
        .cap_max_wait_ms
        .is_some_and(|ms| ms > SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS)
    {
        return bad_request(format!(
            "capMaxWaitMs 须在 0 ~ {SESSION_AFFINITY_CAP_MAX_WAIT_LIMIT_MS} 之间"
        ));
    }

    let manager = state.service.token_manager();
    if let Some(enabled) = payload.enabled
        && let Err(e) = manager.set_session_affinity_enabled(enabled)
    {
        return internal_error(e);
    }
    if let Err(e) = manager.set_session_affinity_cap_config(
        payload.rpm_multiplier,
        payload.cap_max_wait_ms,
        payload.overflow_enabled,
    ) {
        return internal_error(e);
    }
    Json(current_state(manager)).into_response()
}
