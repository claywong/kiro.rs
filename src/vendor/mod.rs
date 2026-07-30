//! 卖家（Key 供应商）对接模块
//!
//! - 入站：`/webhook/vendor/{token}` 接收 `new_keys_available` / `all_keys_dead`，
//!   按 `event_id` 幂等落库并计入告警。手动模式下**不触发任何扣费或凭据变更**；
//!   自动模式下会在通过失效确认后异步提取（见 [`auto`]）。
//! - 出站：`/api/admin/vendor/*` 由管理面板显式调用，提取 Key、查余额库存、兑换充值。
//!
//! @author wangzhong

pub mod auto;
pub mod client;
pub mod handlers;
pub mod router;
pub mod schedule;
pub mod service;
pub mod store;
// 多卖家支持：协议抽象 + 各家 flavor 实现 + 注册表。
// 单独成模块而非塞进既有文件，便于接入第三家时只加文件。
pub mod flavor_kiroapp;
pub mod flavor_legacy;
pub mod protocol;
pub mod registry;

pub use handlers::VendorState;
pub use router::{create_vendor_admin_router, create_vendor_webhook_router};
pub use store::{SharedVendorStore, VendorStore};
// 本地新增导出单独成行。
pub use registry::VendorRegistry;
