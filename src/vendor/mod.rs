//! 卖家（Key 供应商）对接模块
//!
//! - 入站：`/webhook/vendor/{token}` 接收 `new_keys_available` / `all_keys_dead`，
//!   按 `event_id` 幂等落库并计入告警。手动模式下**不触发任何扣费或凭据变更**；
//!   自动模式下会在通过失效确认后异步提取（见 [`auto`]）。
//! - 出站：`/api/admin/vendor/*` 由管理面板显式调用，提取 Key、查余额库存、兑换充值。
//!
//! 各家卖家的协议差异收敛在 [`protocol`] 与 `flavor_*` 里，上层只见中立结构。
//! 注意 `kiroapp` 这个词有歧义：[`flavor_kiroapp`] 是 kiroapp**.io**（`/api/me/*`，
//! 功能完整），[`flavor_kiroapp_cc`] 是 kiroapp**.cc**（`/openapi/*`，只有库存 /
//! 余额 / 提取三个接口，无 webhook）。两者是不同的卖家。
//! [`flavor_drop`] 是第四家 drop.kiro.ss（`/api/v1/*`，人民币计价、下单可能异步）。
//! [`flavor_kiromarket`] 是第五家 kiro-market（api.91kiro.com，`/api/my/*`，
//! 与首家同路径同鉴权，但 `keys` 是带逐张实付的对象数组、余额不在库存接口里）。
//! [`flavor_kirored`] 是第六家 kiro.red，与前五家协议**根本不同**：email + 密码
//! 登录换 JWT、每个请求带签名、响应体 AES 加密、无 webhook、商品（SKU + 积分）
//! 下单。整套管线自成一体，不走 [`client`] 的通用请求路径。
//! [`flavor_kiroooo`] 是第七家 kiro.ooo，又与首家同前缀同鉴权，但**余额语义不同**：
//! 本家 `profile.remaining` 恒为 0，真实余额在 `credits` —— 照首家映射会让整家
//! 静默不可用（面板余额 0、自动提取恒算出 0 个可提）。提货走 `/my/keys/claim`。
//!
//! @author wangzhong

pub mod auto;
pub mod client;
pub mod handlers;
pub mod import;
pub mod pool_gate;
pub mod router;
pub mod schedule;
pub mod service;
pub mod store;
// 多卖家支持：协议抽象 + 各家 flavor 实现 + 注册表。
// 单独成模块而非塞进既有文件，便于接入第三家时只加文件。
pub mod flavor_drop;
pub mod flavor_kiroapp;
pub mod flavor_kiroapp_cc;
pub mod flavor_kiromarket;
pub mod flavor_kiroooo;
pub mod flavor_kirored;
pub mod flavor_legacy;
pub mod protocol;
pub mod registry;

pub use handlers::VendorState;
pub use router::{create_vendor_admin_router, create_vendor_webhook_router};
pub use store::{SharedVendorStore, VendorStore};
// 本地新增导出单独成行。
pub use registry::VendorRegistry;
