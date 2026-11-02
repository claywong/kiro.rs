//! KiroShop（kiro.yonghu.shop）独立对接模块
//!
//! 与 `vendor` 框架**刻意解耦**：那套是「ksk_ Key 即时交付」模型，装不下这家
//! 的商品制购买（预定排队、增量拿号、补号、售后、事件流）。本模块自成一体：
//!
//! - [`client`]：客户 API v1.2 全量端点的类型化客户端
//! - [`config`]：config.json 顶层 `kiroshop` 段（含从旧 vendors 条目的迁移）
//! - [`store`]：SQLite 在途跟踪 / 事件去重 / 面板告警
//! - [`service`]：购买主流程、增量收货入库、事件与发货轮询、自动补货、HMAC 验签
//! - [`handlers`]：`/api/admin/kiroshop/*` 管理接口与 `/webhook/kiroshop/{token}` 入站
//!
//! 唯一复用的旧设施是 [`crate::vendor::import`] 的凭据解析入库
//! （`ksk_` / Social / IdC JSON 一套通吃）与 admin 的 `import_one_credential`。
//!
//! @author wangzhong

pub mod client;
pub mod config;
pub mod handlers;
pub mod service;
pub mod store;

pub use config::migrate_from_vendors;
pub use handlers::{KiroshopState, create_kiroshop_admin_router, create_kiroshop_webhook_router};
pub use service::KiroshopService;
pub use store::KiroshopStore;
