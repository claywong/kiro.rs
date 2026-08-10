//! kiroapp.io 的协议实现：`/api/me/*` + `Authorization: Bearer km_xxx`
//!
//! 与首家的主要差异：
//! - 列表接口统一返回分页信封 `{items,total,page,page_size,pages}`
//! - `profile` 把档案嵌在 `user` 键下
//! - 阶梯定价：单价按母号累计产量分档，同一单里各 Key 可能不同价，
//!   总价必须以 `total_debit` 为准，前端无法预估
//! - webhook 载荷直接带 `client_order_id`（按「批次 + 收件人」确定性派生的幂等键），
//!   收到后原样带上即可，**不需要本地生成订单号、也不需要抢占绑定数量**
//! - 无 webhook 管理 API（地址在网页「设置 → Webhook 配置」里填）
//!
//! @author wangzhong

use serde::{Deserialize, Serialize};

use super::protocol::{
    EarliestKeyInfo, LedgerEntry, OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey,
    RedeemResult, StockInfo, VendorKeyInfo, ZoneStock,
};

pub const PATH_STOCK: &str = "/api/me/stock";
pub const PATH_PURCHASE: &str = "/api/me/purchase";
pub const PATH_PROFILE: &str = "/api/me/profile";
pub const PATH_ORDERS: &str = "/api/me/orders";
pub const PATH_REDEEM: &str = "/api/me/redeem";
pub const PATH_LEDGER: &str = "/api/me/ledger";
pub const PATH_KEYS: &str = "/api/me/keys";
pub const PATH_KEYS_CREATED_AT: &str = "/api/me/keys/created-at";

/// 列表接口的分页上限（卖家侧硬限制）
pub const MAX_PAGE_SIZE: u32 = 500;

/// `GET /api/me/stock` 响应。`price` 是 `price_min` 的向后兼容别名。
/// 新版按区分开库存与单价：`stock_us` / `stock_eu` / `price_us` / `price_eu`。
#[derive(Debug, Clone, Deserialize)]
pub struct StockResponse {
    /// 旧字段：全局可售量（不分区时用）
    #[serde(default)]
    pub stock: u32,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub price_min: Option<f64>,
    #[serde(default)]
    pub price_max: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,

    /// 新字段：分区库存与报价
    #[serde(default)]
    pub stock_us: Option<u32>,
    #[serde(default)]
    pub stock_eu: Option<u32>,
    #[serde(default)]
    pub price_us: Option<f64>,
    #[serde(default)]
    pub price_eu: Option<f64>,
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        // 优先检测分区字段。只要 stock_us / stock_eu 任一存在就走分区逻辑
        let has_zones = r.stock_us.is_some() || r.stock_eu.is_some();
        let zones = if has_zones {
            let mut v = Vec::new();
            if let Some(us) = r.stock_us {
                v.push(ZoneStock {
                    zone: "us".to_string(),
                    label: Some("美国区".to_string()),
                    available: us,
                    stock: Some(us),
                    unit_price: r.price_us,
                    enabled: true,
                    // 该家无车次概念
                    departed_at: None,
                    alive_secs: None,
                    alive_text: None,
                });
            }
            if let Some(eu) = r.stock_eu {
                v.push(ZoneStock {
                    zone: "eu".to_string(),
                    label: Some("欧洲区".to_string()),
                    available: eu,
                    stock: Some(eu),
                    unit_price: r.price_eu,
                    enabled: true,
                    departed_at: None,
                    alive_secs: None,
                    alive_text: None,
                });
            }
            v
        } else {
            Vec::new()
        };

        // available：分区时取各区之和，不分区时用 stock 字段
        let available = if has_zones {
            r.stock_us.unwrap_or(0) + r.stock_eu.unwrap_or(0)
        } else {
            r.stock
        };

        // price_min/max：分区时从 zones 重算，不分区时用响应原值
        let (price_min, price_max) = if has_zones {
            let prices: Vec<f64> = [r.price_us, r.price_eu]
                .iter()
                .filter_map(|&p| p)
                .collect();
            if prices.is_empty() {
                (None, None)
            } else {
                let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
                let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                (Some(min), Some(max))
            }
        } else {
            // price 是 price_min 的兼容别名
            (r.price_min.or(r.price), r.price_max)
        };

        Self {
            available,
            price_min,
            price_max,
            balance: r.balance,
            zones,
        }
    }
}

/// `POST /api/me/purchase` 响应里的单张密钥。
/// 除 key 外还给 AWS 账号密码与 issuer_url。
#[derive(Debug, Clone, Deserialize)]
pub struct KiroappKey {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub issuer_url: Option<String>,
    /// 这一张实际扣了多少
    #[serde(default)]
    pub price: Option<f64>,
}

/// `POST /api/me/purchase` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseResponse {
    #[serde(default)]
    pub purchased: u32,
    #[serde(default)]
    pub requested: Option<u32>,
    /// 提取后剩余库存（注意：不是余额）
    #[serde(default)]
    pub remaining: Option<f64>,
    /// 本单实际均价 = total_debit / purchased
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 实际扣费总额，阶梯定价下的权威数字
    #[serde(default)]
    pub total_debit: Option<f64>,
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub keys: Vec<KiroappKey>,
    #[serde(default)]
    pub replayed: bool,
    /// 实际成交区域（响应不带此字段，由调用方从请求参数补上）
    #[serde(skip)]
    pub region: Option<String>,
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
                price: k.price,
            })
            .collect();
        let purchased = r.purchased.max(keys.len() as u32);
        Self {
            purchased,
            requested: r.requested,
            remaining: r.remaining,
            unit_price: r.unit_price,
            total_debit: r.total_debit,
            order_id: r.order_id,
            keys,
            replayed: r.replayed,
            zone: r.region,
        }
    }
}

/// `GET /api/me/profile` 里的用户档案
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct KiroappUser {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub min_purchase: Option<u32>,
    #[serde(default)]
    pub max_purchase: Option<u32>,
    #[serde(default)]
    pub notify_new_batch: Option<bool>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// `GET /api/me/profile` 响应 —— 档案嵌在 `user` 键下
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub user: Option<KiroappUser>,
}

impl From<ProfileResponse> for ProfileInfo {
    fn from(r: ProfileResponse) -> Self {
        let u = r.user.unwrap_or(KiroappUser {
            id: None,
            name: None,
            email: None,
            balance: None,
            min_purchase: None,
            max_purchase: None,
            notify_new_batch: None,
            created_at: None,
        });
        Self {
            name: u.name,
            email: u.email,
            balance: u.balance,
            // 该卖家是纯余额制，没有总配额 / 已用配额的概念
            quota: None,
            used_quota: None,
            min_purchase: u.min_purchase,
            max_purchase: u.max_purchase,
            // webhook 地址只能在网页里配，API 读不到
            webhook_url: None,
            created_at: u.created_at,
        }
    }
}

/// `POST /api/me/redeem` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct RedeemResponse {
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub replayed: bool,
}

impl From<RedeemResponse> for RedeemResult {
    fn from(r: RedeemResponse) -> Self {
        Self {
            quota: r.quota,
            // 该卖家不回显兑换后余额，需另调 profile 或 stock
            balance: None,
            previous_quota: None,
            redeemed_at: None,
            replayed: r.replayed,
        }
    }
}

/// `GET /api/me/orders` 单条订单
#[derive(Debug, Clone, Deserialize)]
pub struct KiroappOrder {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub requested: Option<u32>,
    #[serde(default)]
    pub purchased: Option<u32>,
    #[serde(default)]
    pub total_debit: Option<f64>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<KiroappOrder> for OrderInfo {
    fn from(o: KiroappOrder) -> Self {
        Self {
            client_order_id: o.client_order_id,
            // 批次 id 字段名在订单列表里可能叫 id，取 order_id 优先
            order_id: o.order_id.or(o.id),
            requested: o.requested,
            purchased: o.purchased,
            total_debit: o.total_debit,
            created_at: o.created_at,
        }
    }
}

/// `GET /api/me/ledger` 单条流水
#[derive(Debug, Clone, Deserialize)]
pub struct KiroappLedgerEntry {
    #[serde(default)]
    pub seq: Option<i64>,
    /// 变动类型。`type` 是 Rust 关键字，改名映射。
    #[serde(default, rename = "type")]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub balance_after: Option<f64>,
    #[serde(default)]
    pub memo: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<KiroappLedgerEntry> for LedgerEntry {
    fn from(e: KiroappLedgerEntry) -> Self {
        Self {
            seq: e.seq,
            entry_type: e.entry_type,
            amount: e.amount,
            balance_after: e.balance_after,
            memo: e.memo,
            created_at: e.created_at,
        }
    }
}

/// `GET /api/me/keys` 单条密钥
#[derive(Debug, Clone, Deserialize)]
pub struct KiroappMyKey {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub key_value: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub purchased_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<KiroappMyKey> for VendorKeyInfo {
    fn from(k: KiroappMyKey) -> Self {
        Self {
            id: k.id,
            key_value: k.key_value,
            account: k.account,
            status: k.status,
            purchased_at: k.purchased_at,
            created_at: k.created_at,
        }
    }
}

/// `GET /api/me/keys/created-at` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct CreatedAtResponse {
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub count: Option<u32>,
}

impl From<CreatedAtResponse> for EarliestKeyInfo {
    fn from(r: CreatedAtResponse) -> Self {
        Self {
            created_at: r.created_at,
            count: r.count,
        }
    }
}

/// 卖家的分页信封。`items` 的元素类型由调用方指定。
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub total: Option<u32>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
    #[serde(default)]
    pub pages: Option<u32>,
}

impl<T> Envelope<T> {
    /// 转成中立分页结构，同时把元素映射成中立类型
    pub fn map_into<U: From<T>>(self) -> Paged<U> {
        Paged {
            items: self.items.into_iter().map(U::from).collect(),
            total: self.total,
            page: self.page,
            page_size: self.page_size,
            pages: self.pages,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文档给的库存样本
    #[test]
    fn 解析库存报价_真实样本() {
        let raw = r#"{"stock":120,"price":30,"price_min":30,"price_max":65,"balance":2060}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 120);
        assert_eq!(s.price_min, Some(30.0));
        assert_eq!(s.price_max, Some(65.0));
        // 余额顺带给出，可省一次 profile
        assert_eq!(s.balance, Some(2060.0));
        assert!(s.zones.is_empty(), "旧格式不带分区");
    }

    #[test]
    fn 解析库存报价_分区新格式() {
        let raw = r#"{"stock_us":108,"stock_eu":12,"price_us":20,"price_eu":15,"balance":2060}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 120, "available = stock_us + stock_eu");
        assert_eq!(s.price_min, Some(15.0), "最低价从 zones 重算");
        assert_eq!(s.price_max, Some(20.0));
        assert_eq!(s.balance, Some(2060.0));
        assert_eq!(s.zones.len(), 2);
        assert_eq!(s.zones[0].zone, "us");
        assert_eq!(s.zones[0].available, 108);
        assert_eq!(s.zones[0].unit_price, Some(20.0));
        assert_eq!(s.zones[1].zone, "eu");
        assert_eq!(s.zones[1].label.as_deref(), Some("欧洲区"));
        assert_eq!(s.zones[1].available, 12);
        assert_eq!(s.zones[1].unit_price, Some(15.0));
    }

    #[test]
    fn 解析库存报价_分区单区无货() {
        let raw = r#"{"stock_us":0,"stock_eu":12,"price_us":20,"price_eu":15}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 12);
        // 美区 0 库存，但 zone 仍要保留，让前端能渲染成禁用项
        assert_eq!(s.zones.len(), 2);
        assert_eq!(s.zones[0].available, 0);
    }

    #[test]
    fn 库存缺price_min时回退price() {
        let raw = r#"{"stock":5,"price":42}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.price_min, Some(42.0));
        assert!(s.price_max.is_none());
    }

    #[test]
    fn 库存为空时不报错() {
        let s: StockInfo = serde_json::from_str::<StockResponse>("{}").unwrap().into();
        assert_eq!(s.available, 0);
        assert!(s.balance.is_none());
    }

    /// 文档给的下单样本
    #[test]
    fn 解析下单结果_真实样本() {
        let raw = r#"{"purchased":5,"requested":5,"remaining":115,"unit_price":38,
            "total_debit":190,"order_id":"0d9f","keys":[
            {"key":"sk-1","account":"user-1","password":"p1",
             "issuer_url":"https://i","price":30}],"replayed":false}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.purchased, 5);
        assert_eq!(r.requested, Some(5));
        // 阶梯定价下总价以 total_debit 为准
        assert_eq!(r.total_debit, Some(190.0));
        assert_eq!(r.unit_price, Some(38.0));
        assert_eq!(r.order_id.as_deref(), Some("0d9f"));
        assert_eq!(r.keys[0].account.as_deref(), Some("user-1"));
        assert_eq!(r.keys[0].issuer_url.as_deref(), Some("https://i"));
        assert_eq!(r.keys[0].price, Some(30.0));
        assert!(!r.replayed);
    }

    #[test]
    fn 幂等重放标记被保留() {
        // 重放响应与原响应字节一致，replayed 是判断「本次没扣钱」的唯一依据
        let raw = r#"{"purchased":2,"total_debit":60,"keys":[{"key":"a"},{"key":"b"}],
            "replayed":true}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert!(r.replayed);
        assert_eq!(r.purchased, 2);
    }

    #[test]
    fn 余额不足时部分成交() {
        // 文档：余额不足按买得起的数量成交，purchased < requested
        let raw = r#"{"purchased":2,"requested":5,"total_debit":70,
            "keys":[{"key":"a","price":30},{"key":"b","price":40}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.requested, Some(5));
        // 同单不同价，验证阶梯定价被完整保留
        assert_eq!(r.keys[0].price, Some(30.0));
        assert_eq!(r.keys[1].price, Some(40.0));
    }

    /// 文档给的档案样本，注意嵌在 user 下
    #[test]
    fn 解析档案_真实样本() {
        let raw = r#"{"user":{"id":"a8d5","name":"alice","email":"alice@example.com",
            "balance":2060,"min_purchase":1,"max_purchase":10,
            "notify_new_batch":true,"created_at":"2026-07-29T09:51:32+09:00"}}"#;
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(raw).unwrap().into();
        assert_eq!(p.name.as_deref(), Some("alice"));
        assert_eq!(p.email.as_deref(), Some("alice@example.com"));
        assert_eq!(p.balance, Some(2060.0));
        assert_eq!(p.min_purchase, Some(1));
        assert_eq!(p.max_purchase, Some(10));
        // 纯余额制，无配额概念
        assert!(p.quota.is_none());
        assert!(p.used_quota.is_none());
    }

    #[test]
    fn 档案缺user键时不panic() {
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>("{}").unwrap().into();
        assert!(p.balance.is_none());
        assert!(p.name.is_none());
    }

    /// 文档给的流水样本，type 是关键字需改名映射
    #[test]
    fn 解析积分流水_真实样本() {
        let raw = r#"{"items":[{"seq":2,"type":"stripe_recharge","amount":60,
            "balance_after":2060,"ref_type":"recharge_order","ref_id":"47166cdc",
            "memo":"Stripe 充值","created_at":"2026-07-30T10:45:29+09:00"}],
            "total":2,"page":1,"page_size":50,"pages":1}"#;
        let env: Envelope<KiroappLedgerEntry> = serde_json::from_str(raw).unwrap();
        let paged: Paged<LedgerEntry> = env.map_into();
        assert_eq!(paged.total, Some(2));
        assert_eq!(paged.page_size, Some(50));
        assert_eq!(paged.items[0].seq, Some(2));
        assert_eq!(paged.items[0].entry_type.as_deref(), Some("stripe_recharge"));
        assert_eq!(paged.items[0].amount, Some(60.0));
        assert_eq!(paged.items[0].balance_after, Some(2060.0));
        assert_eq!(paged.items[0].memo.as_deref(), Some("Stripe 充值"));
    }

    #[test]
    fn 解析支出流水金额带负号() {
        let raw = r#"{"items":[{"seq":3,"type":"purchase_debit","amount":-190,
            "balance_after":1870}]}"#;
        let paged: Paged<LedgerEntry> = serde_json::from_str::<Envelope<KiroappLedgerEntry>>(raw)
            .unwrap()
            .map_into();
        assert_eq!(paged.items[0].amount, Some(-190.0));
    }

    /// 文档给的密钥列表样本
    #[test]
    fn 解析我的密钥_真实样本() {
        let raw = r#"{"items":[{"id":"k1","key_value":"sk-x","account":"user-1",
            "password":"p","issuer_url":"https://i","status":"sold",
            "purchased_at":"2026-07-30T09:00:00+09:00",
            "created_at":"2026-07-30T08:12:00+09:00"}],
            "total":42,"page":1,"page_size":50,"pages":1}"#;
        let paged: Paged<VendorKeyInfo> = serde_json::from_str::<Envelope<KiroappMyKey>>(raw)
            .unwrap()
            .map_into();
        assert_eq!(paged.total, Some(42));
        assert_eq!(paged.items[0].status.as_deref(), Some("sold"));
        // created_at 是开号时刻、purchased_at 是购买时刻，两者语义不同
        assert_eq!(
            paged.items[0].created_at.as_deref(),
            Some("2026-07-30T08:12:00+09:00")
        );
        assert_eq!(
            paged.items[0].purchased_at.as_deref(),
            Some("2026-07-30T09:00:00+09:00")
        );
    }

    #[test]
    fn 解析最早密钥时间() {
        let raw = r#"{"created_at":"2026-07-29T10:00:00+09:00","count":42}"#;
        let e: EarliestKeyInfo = serde_json::from_str::<CreatedAtResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(e.created_at.as_deref(), Some("2026-07-29T10:00:00+09:00"));
        assert_eq!(e.count, Some(42));
    }

    #[test]
    fn 解析兑换结果() {
        let r: RedeemResult = serde_json::from_str::<RedeemResponse>(r#"{"quota":100,"replayed":false}"#)
            .unwrap()
            .into();
        assert_eq!(r.quota, Some(100.0));
        assert!(!r.replayed);
        // 该卖家不回显兑换后余额
        assert!(r.balance.is_none());
    }

    #[test]
    fn 解析订单信封_批次id取order_id优先() {
        let raw = r#"{"items":[{"id":"row-1","order_id":"batch-9",
            "client_order_id":"abc","requested":3,"purchased":3,"total_debit":95}],
            "total":1,"page":1,"page_size":50,"pages":1}"#;
        let paged: Paged<OrderInfo> = serde_json::from_str::<Envelope<KiroappOrder>>(raw)
            .unwrap()
            .map_into();
        assert_eq!(paged.items[0].order_id.as_deref(), Some("batch-9"));
        assert_eq!(paged.items[0].total_debit, Some(95.0));
    }

    #[test]
    fn 空信封不报错() {
        let paged: Paged<OrderInfo> = serde_json::from_str::<Envelope<KiroappOrder>>("{}")
            .unwrap()
            .map_into();
        assert!(paged.items.is_empty());
        assert!(paged.total.is_none());
    }
}
