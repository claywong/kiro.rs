//! 首家卖家的协议实现：`/api/my/*` + `X-API-Key: usr-xxx`
//!
//! 本模块只负责「原始 DTO → 中立结构」的翻译，不发请求（HTTP 在 [`super::client`]）。
//! 独有能力：`/api/status` 系统状态、`/api/my/gen-logs` 开号记录、webhook 远程管理。
//!
//! @author wangzhong

use serde::{Deserialize, Serialize};

use super::protocol::{
    OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey, RedeemResult, StockInfo,
};

/// 路径前缀。该卖家的账号维度接口在 `/api/my` 下，系统状态在 `/api/status`。
pub const PATH_STOCK: &str = "/api/my/stock";
pub const PATH_PURCHASE: &str = "/api/my/purchase";
pub const PATH_PROFILE: &str = "/api/my/profile";
pub const PATH_ORDERS: &str = "/api/my/purchase-orders";
pub const PATH_REDEEM: &str = "/api/my/redeem";
pub const PATH_STATUS: &str = "/api/status";
pub const PATH_GEN_LOGS: &str = "/api/my/gen-logs";
pub const PATH_WEBHOOK: &str = "/api/my/webhook";
pub const PATH_WEBHOOK_TEST: &str = "/api/my/webhook/test";

/// 单个 Key 条目。该卖家只保证 `key` 字段。
#[derive(Debug, Clone, Deserialize)]
pub struct VendorKey {
    pub key: String,
}

/// `POST /api/my/purchase` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseResponse {
    #[serde(default)]
    pub purchased: u32,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub keys: Vec<VendorKey>,
}

impl From<PurchaseResponse> for PurchaseResult {
    fn from(r: PurchaseResponse) -> Self {
        let keys: Vec<PurchasedKey> = r
            .keys
            .into_iter()
            .map(|k| PurchasedKey {
                key: k.key,
                account: None,
                password: None,
                issuer_url: None,
                price: None,
            })
            .collect();
        // 该卖家不回显 purchased 与实际条数的差异，取较大者兜底
        let purchased = r.purchased.max(keys.len() as u32);
        Self {
            purchased,
            requested: None,
            remaining: r.remaining,
            unit_price: None,
            total_debit: None,
            order_id: None,
            keys,
            // 该卖家不回显是否重放，无法区分，保守记 false
            replayed: false,
        }
    }
}

/// `GET /api/my/stock` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct StockResponse {
    #[serde(default)]
    pub max: u32,
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        Self {
            available: r.max,
            // 该卖家不在库存接口给价格与余额，余额需另调 profile
            price_min: None,
            price_max: None,
            balance: None,
        }
    }
}

/// `GET /api/status` 响应 —— 卖家账号维度的 Key 数量与库存
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct VendorSystemStatus {
    /// 卖家侧当前存活 Key 数
    #[serde(default)]
    pub keys_active: Option<u32>,
    /// 卖家侧已失效 Key 数
    #[serde(default)]
    pub keys_dead: Option<u32>,
    /// 卖家侧尚未售出的存货 Key 数
    #[serde(default)]
    pub keys_stock: Option<u32>,
    /// 卖家侧 Key 累计总数（含已失效）
    #[serde(default)]
    pub keys_total: Option<u32>,
    /// 卖家侧是否正在生成新 Key
    #[serde(default)]
    pub generating: Option<bool>,
    /// 卖家侧已运行秒数
    #[serde(default)]
    pub uptime_seconds: Option<f64>,
    /// 卖家侧启动时刻，形如 `2026-07-25 20:59:33`（无时区标记）
    #[serde(default)]
    pub started_at: Option<String>,
    /// 卖家侧是否开启自动检测
    #[serde(default)]
    pub auto_check: Option<bool>,
    /// 卖家侧是否开启自动生成
    #[serde(default)]
    pub auto_generate: Option<bool>,
    /// 自动检测间隔。卖家用字符串给（如 "20"），故不解析成数字
    #[serde(default)]
    pub check_interval: Option<String>,
    /// 其余未建模字段原样透传，卖家新增字段时不必改一轮后端
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// `GET /api/my/gen-logs` 单条开号记录
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GenLogEntry {
    /// 开号时刻，形如 `2026-07-28 23:27:36`（无时区标记）
    #[serde(default)]
    pub created_at: Option<String>,
    /// 该批开出的 Key 数
    #[serde(default)]
    pub count: Option<u32>,
    /// 卖家侧状态，如 "done"
    #[serde(default)]
    pub status: Option<String>,
}

/// `GET /api/my/gen-logs` 响应 —— 卖家近期开号批次，用于判断出号节奏
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GenLogsResponse {
    /// 相邻两批的平均间隔（分钟）。卖家算好给的，不足两批时可能缺失
    #[serde(default)]
    pub avg_interval_min: Option<f64>,
    #[serde(default)]
    pub items: Vec<GenLogEntry>,
}

/// `GET /api/my/profile` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfileResponse {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub used_quota: Option<f64>,
    #[serde(default)]
    pub webhook_url: Option<String>,
}

impl From<ProfileResponse> for ProfileInfo {
    fn from(r: ProfileResponse) -> Self {
        Self {
            name: r.name,
            email: None,
            // 该卖家的「可用余额」叫 remaining
            balance: r.remaining,
            quota: r.quota,
            used_quota: r.used_quota,
            min_purchase: None,
            max_purchase: None,
            webhook_url: r.webhook_url,
            created_at: None,
        }
    }
}

/// `POST /api/my/redeem` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RedeemResponse {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub previous_quota: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub created_by_name: Option<String>,
    #[serde(default)]
    pub redeemed_at: Option<String>,
    /// true 表示这张码此前已兑换过，本次未改动余额
    #[serde(default)]
    pub replayed: bool,
}

impl From<RedeemResponse> for RedeemResult {
    fn from(r: RedeemResponse) -> Self {
        Self {
            quota: r.quota,
            balance: r.balance,
            previous_quota: r.previous_quota,
            redeemed_at: r.redeemed_at,
            replayed: r.replayed,
        }
    }
}

/// `GET /api/my/purchase-orders` 单条订单
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PurchaseOrder {
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub requested: Option<u32>,
    #[serde(default)]
    pub purchased: Option<u32>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<PurchaseOrder> for OrderInfo {
    fn from(o: PurchaseOrder) -> Self {
        Self {
            client_order_id: o.client_order_id,
            order_id: None,
            requested: o.requested,
            purchased: o.purchased,
            total_debit: None,
            created_at: o.created_at,
        }
    }
}

/// 该卖家的订单接口返回裸数组，统一包装成分页信封
pub fn orders_to_paged(orders: Vec<PurchaseOrder>) -> Paged<OrderInfo> {
    Paged::from_vec(orders.into_iter().map(OrderInfo::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用卖家 `/api/status` 的真实返回做样本
    #[test]
    fn 解析系统状态_真实样本() {
        let raw = r#"{"auto_check":true,"auto_generate":true,"check_interval":"20",
            "generating":false,"keys_active":200,"keys_dead":5857,"keys_stock":57,
            "keys_total":6076,"started_at":"2026-07-25 20:59:33","uptime_seconds":7179}"#;
        let s: VendorSystemStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(s.keys_active, Some(200));
        assert_eq!(s.keys_dead, Some(5857));
        assert_eq!(s.keys_stock, Some(57));
        assert_eq!(s.keys_total, Some(6076));
        assert_eq!(s.generating, Some(false));
        assert_eq!(s.uptime_seconds, Some(7179.0));
        assert_eq!(s.started_at.as_deref(), Some("2026-07-25 20:59:33"));
        assert_eq!(s.auto_check, Some(true));
        assert_eq!(s.auto_generate, Some(true));
        // 间隔是字符串，不能当数字解析
        assert_eq!(s.check_interval.as_deref(), Some("20"));
        assert!(s.extra.is_empty(), "已建模字段不应落进 extra");
    }

    #[test]
    fn 解析系统状态_容忍缺字段与未知字段() {
        // 卖家少给字段时不能整体解析失败，否则状态卡片全空
        let partial: VendorSystemStatus = serde_json::from_str(r#"{"keys_stock":0}"#).unwrap();
        assert_eq!(partial.keys_stock, Some(0));
        assert_eq!(partial.keys_active, None);
        assert_eq!(partial.uptime_seconds, None);

        // 卖家新增字段走 extra 透传，不报错
        let unknown: VendorSystemStatus =
            serde_json::from_str(r#"{"keys_stock":1,"brand_new_field":"x"}"#).unwrap();
        assert_eq!(
            unknown.extra.get("brand_new_field").and_then(|v| v.as_str()),
            Some("x")
        );
    }

    /// 用卖家 `/api/my/gen-logs` 的真实返回做样本
    #[test]
    fn 解析开号记录_真实样本() {
        let raw = r#"{"avg_interval_min":52.25,"items":[
            {"created_at":"2026-07-28 23:27:36","count":200,"status":"done"},
            {"created_at":"2026-07-28 22:50:45","count":200,"status":"done"}]}"#;
        let g: GenLogsResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(g.avg_interval_min, Some(52.25));
        assert_eq!(g.items.len(), 2);
        assert_eq!(g.items[0].count, Some(200));
        assert_eq!(g.items[0].status.as_deref(), Some("done"));
        assert_eq!(g.items[0].created_at.as_deref(), Some("2026-07-28 23:27:36"));
    }

    #[test]
    fn 解析开号记录_容忍空与缺字段() {
        // 从未开号时 items 为空，avg 缺失，不能整体解析失败
        let empty: GenLogsResponse = serde_json::from_str(r#"{"items":[]}"#).unwrap();
        assert!(empty.items.is_empty());
        assert!(empty.avg_interval_min.is_none());

        // 单条缺字段也要能读出其余部分
        let partial: GenLogsResponse =
            serde_json::from_str(r#"{"items":[{"count":10}]}"#).unwrap();
        assert_eq!(partial.items[0].count, Some(10));
        assert!(partial.items[0].created_at.is_none());
    }

    #[test]
    fn 库存转中立结构() {
        let s: StockInfo = StockResponse { max: 7 }.into();
        assert_eq!(s.available, 7);
        // 该卖家不给价格与余额
        assert!(s.price_min.is_none());
        assert!(s.balance.is_none());
    }

    #[test]
    fn 档案转中立结构_余额取remaining() {
        let raw = r#"{"name":"我","quota":100,"remaining":42,"used_quota":58}"#;
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(raw).unwrap().into();
        assert_eq!(p.balance, Some(42.0), "可用余额取 remaining 而非 quota");
        assert_eq!(p.quota, Some(100.0));
        assert_eq!(p.used_quota, Some(58.0));
        // 该卖家没有限购字段
        assert!(p.max_purchase.is_none());
    }

    #[test]
    fn 下单转中立结构_purchased取较大者() {
        let raw = r#"{"purchased":1,"remaining":9,"keys":[{"key":"a"},{"key":"b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        // 卖家回显 1 但实发 2 条，按实际条数算，否则会漏入库
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.remaining, Some(9.0));
        // 该卖家无阶梯定价，不给扣费明细
        assert!(r.total_debit.is_none());
        assert!(!r.replayed);
    }

    #[test]
    fn 订单裸数组包装成分页() {
        let raw = r#"[{"client_order_id":"abc","requested":2,"purchased":2,
            "created_at":"2026-07-28 10:00:00"}]"#;
        let orders: Vec<PurchaseOrder> = serde_json::from_str(raw).unwrap();
        let paged = orders_to_paged(orders);
        assert_eq!(paged.total, Some(1));
        assert_eq!(paged.items[0].client_order_id.as_deref(), Some("abc"));
        assert_eq!(paged.items[0].purchased, Some(2));
    }
}
