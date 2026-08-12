//! Admin API 模块
//!
//! 提供凭据管理和监控功能的 HTTP API
//!
//! # 功能
//! - 查询所有凭据状态
//! - 启用/禁用凭据
//! - 修改凭据优先级
//! - 重置失败计数
//! - 查询凭据余额
//!
//! # 使用
//! ```ignore
//! let admin_service = AdminService::new(token_manager.clone(), endpoint_names);
//! let admin_state = AdminState::new(admin_api_key, admin_service);
//! let admin_router = create_admin_router(admin_state);
//! ```

mod error;
mod handlers;
mod middleware;
pub mod proxy_pool;
mod router;
mod service;
pub mod types;
mod binary_update;
pub mod client_keys;
pub mod groups;
pub mod usage_stats;
pub mod trace_db;
// 本地新增模块单独成行，避免上游增删模块时反复冲突。
pub mod recent_spend;
pub mod health_gate;
pub mod health_probe;
pub mod traffic_ingress;
mod schedulable_client;

pub use client_keys::ClientKeyManager;
pub use groups::GroupManager;
pub use middleware::AdminState;
/// 供卖家对接路由复用同一套 adminApiKey 认证
pub use middleware::admin_auth_middleware;
pub use router::create_admin_router;
pub use service::AdminService;
/// 供卖家对接模块复用凭据入库（去重 / 验活 / 回滚）的结果分类
pub(crate) use service::ImportStatus;
pub use usage_stats::{UsageAggregator, UsageRecorder};
pub use trace_db::{SharedTraceStore, TraceStore};
