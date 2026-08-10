//! 首家卖家的协议实现：`/api/my/*` + `X-API-Key: usr-xxx`
//!
//! 本模块只负责「原始 DTO → 中立结构」的翻译，不发请求（HTTP 在 [`super::client`]）。
//! 独有能力：`/api/status` 系统状态、`/api/my/gen-logs` 开号记录、webhook 远程管理。
//!
//! @author wangzhong

use serde::{Deserialize, Serialize};

use super::protocol::{
    OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey, RedeemResult, StockInfo, ZoneStock,
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

/// 单个 Key 条目。除 `key` 外三个字段是后来补的，老号可能只有 `key`，故都可选。
#[derive(Debug, Clone, Deserialize)]
pub struct VendorKey {
    pub key: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub issuer_url: Option<String>,
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
    /// 实际成交区域
    #[serde(default)]
    pub zone: Option<String>,
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 本单实际扣的积分。卖家按 `unit_price × purchased` 算，但以它为准。
    #[serde(default)]
    pub total_credits: Option<f64>,
    #[serde(default)]
    pub order_id: Option<String>,
}

impl From<PurchaseResponse> for PurchaseResult {
    fn from(r: PurchaseResponse) -> Self {
        let keys: Vec<PurchasedKey> = r
            .keys
            .into_iter()
            .map(|k| PurchasedKey {
                key: k.key,
                account: k.account,
                password: k.password,
                issuer_url: k.issuer_url,
                // 该卖家单一定价，逐张单价就是本区单价
                price: r.unit_price,
                // 本家的区域是订单级的（zone），逐张不带区
                region: None,
            })
            .collect();
        // 该卖家不回显 purchased 与实际条数的差异，取较大者兜底
        let purchased = r.purchased.max(keys.len() as u32);
        Self {
            purchased,
            requested: None,
            remaining: r.remaining,
            unit_price: r.unit_price,
            // 卖家给了权威扣费额就用它，没给则按单价 × 成交数兜底
            total_debit: r
                .total_credits
                .or_else(|| r.unit_price.map(|p| p * purchased as f64)),
            order_id: r.order_id,
            keys,
            // 该卖家不回显是否重放，无法区分，保守记 false
            replayed: false,
            zone: r.zone,
        }
    }
}

/// `GET /api/my/stock` 单个区域条目
#[derive(Debug, Clone, Deserialize)]
pub struct StockZone {
    pub zone: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub max: u32,
    #[serde(default)]
    pub stock: Option<u32>,
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 卖家不给时按开放处理 —— 老版本响应没有这个字段
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// `GET /api/my/stock` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct StockResponse {
    #[serde(default)]
    pub max: u32,
    /// 账户积分余额，省一次 profile 请求
    #[serde(default)]
    pub quota: Option<f64>,
    /// 分区库存。老版本响应没有这个字段，此时退化成不分区行为。
    #[serde(default)]
    pub zones: Vec<StockZone>,
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        let zones: Vec<ZoneStock> = r
            .zones
            .into_iter()
            .map(|z| ZoneStock {
                zone: z.zone,
                label: z.label,
                available: z.max,
                stock: z.stock,
                unit_price: z.unit_price,
                enabled: z.enabled,
                // 该家无车次概念
                departed_at: None,
                alive_secs: None,
                alive_text: None,
            })
            .collect();
        // 各区单价不同，报价取区间。只算「开放且有货」的区 —— 把 0 库存区的价
        // 算进来会让面板显示一个实际提不到的价位。
        let prices: Vec<f64> = zones
            .iter()
            .filter(|z| z.enabled && z.available > 0)
            .filter_map(|z| z.unit_price)
            .collect();
        Self {
            // 顶层 max 是卖家给的聚合值；缺失时按各区之和兜底
            available: if r.max > 0 {
                r.max
            } else {
                zones.iter().filter(|z| z.enabled).map(|z| z.available).sum()
            },
            price_min: prices.iter().copied().reduce(f64::min),
            price_max: prices.iter().copied().reduce(f64::max),
            balance: r.quota,
            zones,
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
    fn 库存转中立结构_无分区() {
        // 老版本响应没有 zones，退化成单区行为
        let s: StockInfo = StockResponse {
            max: 7,
            quota: None,
            zones: Vec::new(),
        }
        .into();
        assert_eq!(s.available, 7);
        assert!(s.zones.is_empty(), "无 zones 时不应凭空造出区");
        assert!(s.price_min.is_none());
        assert!(s.balance.is_none());
        // 不分区时选区必须返回 None，让下单不带 zone
        assert!(s.pick_zone().is_none());
    }

    /// 用线上 `/api/my/stock` 的真实返回做样本（美国区已空、欧洲区有货）
    #[test]
    fn 库存转中立结构_分区真实样本() {
        let raw = r#"{"max":10,"max_purchase":10,"min":1,"quota":812,"reserved":0,
            "zones":[
              {"available":0,"enabled":true,"label":"美国区","max":0,"stock":0,
               "unit_price":20,"zone":"us"},
              {"available":17,"enabled":true,"label":"欧洲区","max":10,"stock":17,
               "unit_price":15,"zone":"eu"}]}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 10);
        assert_eq!(s.balance, Some(812.0), "库存接口的 quota 可当余额用");
        assert_eq!(s.zones.len(), 2);
        assert_eq!(s.zones[0].zone, "us");
        assert_eq!(s.zones[0].available, 0);
        assert_eq!(s.zones[0].unit_price, Some(20.0));
        assert_eq!(s.zones[1].label.as_deref(), Some("欧洲区"));
        assert_eq!(s.zones[1].stock, Some(17));

        // 报价区间只算有货的区：美国区 0 库存，20 这个价实际提不到
        assert_eq!(s.price_min, Some(15.0));
        assert_eq!(s.price_max, Some(15.0));

        // 关键：必须选到欧洲区。选美国区（卖家的默认区）会直接缺货
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("eu"));
    }

    #[test]
    fn 选区_按单价取低并跳过无货与关停() {
        let mk = |zone: &str, max: u32, price: f64, enabled: bool| StockZone {
            zone: zone.to_string(),
            label: None,
            max,
            stock: None,
            unit_price: Some(price),
            enabled,
        };
        // 最便宜的区关停了，次便宜的区无货 —— 只能选第三个
        let s: StockInfo = StockResponse {
            max: 0,
            quota: None,
            zones: vec![
                mk("cheap-off", 50, 5.0, false),
                mk("cheap-empty", 0, 8.0, true),
                mk("ok", 3, 12.0, true),
                mk("pricey", 99, 30.0, true),
            ],
        }
        .into();
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("ok"));
        // 顶层 max 缺失时按各区之和兜底，关停区不计入
        assert_eq!(s.available, 0 + 3 + 99);
    }

    #[test]
    fn 选区_全部无货返回none() {
        let s: StockInfo = StockResponse {
            max: 0,
            quota: None,
            zones: vec![StockZone {
                zone: "us".to_string(),
                label: None,
                max: 0,
                stock: Some(0),
                unit_price: Some(20.0),
                enabled: true,
            }],
        }
        .into();
        assert!(s.pick_zone().is_none());
        // 全区无货时不应给出报价区间
        assert!(s.price_min.is_none());
    }

    #[test]
    fn 选区_同价时结果稳定() {
        // 同价同量的两个区，多次调用必须选同一个 —— 否则幂等重试会换区，
        // 被卖家当成第二笔单再扣一次积分
        let zones: Vec<StockZone> = ["eu", "us", "ap"]
            .iter()
            .map(|z| StockZone {
                zone: z.to_string(),
                label: None,
                max: 5,
                stock: None,
                unit_price: Some(10.0),
                enabled: true,
            })
            .collect();
        let s: StockInfo = StockResponse {
            max: 0,
            quota: None,
            zones,
        }
        .into();
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("ap"));
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("ap"));
    }

    #[test]
    fn 选区_缺单价的区不被优先() {
        let s: StockInfo = StockResponse {
            max: 0,
            quota: None,
            zones: vec![
                StockZone {
                    zone: "unknown-price".to_string(),
                    label: None,
                    max: 100,
                    stock: None,
                    unit_price: None,
                    enabled: true,
                },
                StockZone {
                    zone: "known".to_string(),
                    label: None,
                    max: 1,
                    stock: None,
                    unit_price: Some(99.0),
                    enabled: true,
                },
            ],
        }
        .into();
        // 价格未知的区可能任意贵，不该因为量大就被选中
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("known"));
    }

    #[test]
    fn 下单转中立结构_带区域与扣费() {
        let raw = r#"{"client_order_id":"0123456789abcdef0123456789abcdef","purchased":2,
            "remaining":4500,"zone":"eu","unit_price":15,"total_credits":30,
            "order_id":"a1b2c3",
            "keys":[{"key":"kiro-1","account":"u@e.com","password":"p1",
                     "issuer_url":"https://i1"},
                    {"key":"kiro-2","account":"v@e.com","password":"p2",
                     "issuer_url":"https://i2"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.zone.as_deref(), Some("eu"));
        assert_eq!(r.unit_price, Some(15.0));
        assert_eq!(r.total_debit, Some(30.0), "扣费以卖家的 total_credits 为准");
        assert_eq!(r.order_id.as_deref(), Some("a1b2c3"));
        // 四个字段都要接住，早先只留了 key
        assert_eq!(r.keys[0].account.as_deref(), Some("u@e.com"));
        assert_eq!(r.keys[0].password.as_deref(), Some("p1"));
        assert_eq!(r.keys[1].issuer_url.as_deref(), Some("https://i2"));
        assert_eq!(r.keys[1].price, Some(15.0));
    }

    #[test]
    fn 下单转中立结构_缺total时按单价兜底() {
        let raw = r#"{"purchased":3,"unit_price":15,"keys":[{"key":"a"},{"key":"b"},
            {"key":"c"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.total_debit, Some(45.0));
        // 老响应只有 key，其余三个字段缺失不能解析失败
        assert!(r.keys[0].account.is_none());
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
