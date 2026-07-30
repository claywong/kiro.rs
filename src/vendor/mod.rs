//! 卖家（Key 供应商）对接模块
//!
//! - 入站：`/webhook/vendor/{token}` 接收 `new_keys_available` / `all_keys_dead`，
//!   按 `event_id` 幂等落库并计入告警。手动模式下**不触发任何扣费或凭据变更**；
//!   自动模式下会在通过失效确认后异步提取（见 [`auto`]）。
//! - 出站：`/api/admin/vendor/*` 由管理面板显式调用，提取 Key、查余额库存、兑换充值。
//!
//! 另有次级卖家 kiroapp（[`kiroapp`] / [`kiroapp_service`]），只支持查库存余额与
//! 手动提取一个 Key，挂在 `/api/admin/vendor/kiroapp/*`。两家共用 [`import`] 入库。
//!
//! @author wangzhong

pub mod auto;
pub mod client;
pub mod handlers;
pub mod import;
pub mod kiroapp;
pub mod kiroapp_service;
pub mod router;
pub mod schedule;
pub mod service;
pub mod store;

pub use handlers::VendorState;
pub use kiroapp_service::KiroappService;
pub use router::{create_vendor_admin_router, create_vendor_webhook_router};
pub use service::VendorService;
pub use store::{SharedVendorStore, VendorStore};
