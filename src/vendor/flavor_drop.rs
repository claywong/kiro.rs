//! Kiro Drop（drop.kiro.ss）的协议实现：`/api/v1/*` + `X-API-Key: usr-xxx`
//!
//! 与首家卖家（[`super::flavor_legacy`]）同用 `X-API-Key` 鉴权，但接口形态完全
//! 不同，只有四个端点：
//!
//! | 端点 | 用途 |
//! |---|---|
//! | `GET /api/v1/reservation?quantity=N` | 报价 + 库存 + 余额（一次拿全） |
//! | `POST /api/v1/reservation` | 扣余额下单，200 出货 / 202 待对账 |
//! | `GET /api/v1/orders/{order_id}` | 202 后轮询取 Key |
//! | `PUT /api/my/webhook`、`POST /api/my/webhook/test` | webhook 远程管理 |
//!
//! 本家的两个特点决定了上层处理方式：
//!
//! 1. **下单可能异步**。返回 202 + `status: "pending"` 时钱已经扣了但 Key 还没
//!    定下来，必须拿 `order_id` 轮询而不是换单号重下 —— 换单号等于再扣一次。
//!    轮询在 [`super::client::VendorClient::purchase`] 里做。
//! 2. **金额以人民币计**。报价同时给 USD 与 CNY，实际扣款走 `total_amount_cny`，
//!    故中立结构里的价格字段统一取 CNY，与面板上的余额（也是 CNY）同币种。
//!
//! 没有兑换码、没有订单列表、没有积分流水、没有名下密钥列表，也没有开号记录。
//!
//! @author wangzhong

use serde::Deserialize;

use super::protocol::{ProfileInfo, PurchaseResult, PurchasedKey, StockInfo};

/// 报价与下单共用一个路径，GET 报价、POST 下单。
pub const PATH_RESERVATION: &str = "/api/v1/reservation";
/// 订单查询前缀，后面直接拼 `order_id`。
pub const PATH_ORDER_PREFIX: &str = "/api/v1/orders/";
/// webhook 地址读写。与首家同路径，但本家没有 profile 接口回显它。
pub const PATH_WEBHOOK: &str = "/api/my/webhook";
pub const PATH_WEBHOOK_TEST: &str = "/api/my/webhook/test";

/// 报价时用的探测数量。取 1 是因为报价接口的 `quantity` 会参与校验：
/// 超过库存或超过 `max_count` 直接 400，而查库存的场景下我们恰恰还不知道上限。
pub const PROBE_QUANTITY: u32 = 1;

/// 待对账订单的轮询节奏。文档只说「等待对账完成后查询」，未给时限，
/// 故取一个够用又不至于把请求打满的间隔。
pub const ORDER_POLL_INTERVAL_SECS: u64 = 3;
/// 轮询次数上限。超时不报错 —— 钱已扣，返回已知信息让人工按 order_id 核对。
pub const ORDER_POLL_MAX_ATTEMPTS: u32 = 20;

/// 卖家用字符串传金额（如 `"115.600000"`），部分字段又可能是数字。
/// 两种都接，解析失败当缺失，不让一个金额字段拖垮整份响应。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Decimalish {
    Num(f64),
    Text(String),
}

impl Decimalish {
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Self::Num(v) => Some(*v),
            Self::Text(s) => s.trim().parse::<f64>().ok(),
        }
    }
}

/// 把可选的金额字段转成 f64
pub fn amount(v: &Option<Decimalish>) -> Option<f64> {
    v.as_ref().and_then(Decimalish::to_f64)
}

/// `GET /api/v1/reservation?quantity=N` 响应。
///
/// 一次给齐库存、限购、单价与余额，故本家的「库存」与「档案」两个视图
/// 都由它派生，不必分两次请求。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct QuoteResponse {
    /// 本轮可售数量
    #[serde(default)]
    pub stock: u32,
    /// 单次最大购买数
    #[serde(default)]
    pub max_count: Option<u32>,
    /// 人民币单价
    #[serde(default)]
    pub unit_price_cny: Option<Decimalish>,
    /// 账户可用余额（人民币）
    #[serde(default)]
    pub available_balance: Option<Decimalish>,
    #[serde(default)]
    pub goods_name: Option<String>,
}

impl QuoteResponse {
    /// 实际可提取上限：库存与限购取小者。
    ///
    /// 只报 `stock` 会让面板显示一个下不了的数 —— 超过 `max_count` 的请求
    /// 卖家直接 400，自动模式会白撞一次。
    pub fn effective_available(&self) -> u32 {
        match self.max_count {
            Some(max) => self.stock.min(max),
            None => self.stock,
        }
    }
}

impl From<QuoteResponse> for StockInfo {
    fn from(r: QuoteResponse) -> Self {
        let price = amount(&r.unit_price_cny);
        Self {
            available: r.effective_available(),
            // 无阶梯定价，min = max
            price_min: price,
            price_max: price,
            // 报价接口顺带给余额，省一次请求
            balance: amount(&r.available_balance),
        }
    }
}

impl From<QuoteResponse> for ProfileInfo {
    fn from(r: QuoteResponse) -> Self {
        let balance = amount(&r.available_balance);
        Self {
            name: r.goods_name.clone(),
            email: None,
            balance,
            // 本家没有配额概念，余额即全部可用额度
            quota: balance,
            used_quota: None,
            min_purchase: None,
            max_purchase: r.max_count,
            // webhook 地址只能写、读不回来
            webhook_url: None,
            created_at: None,
        }
    }
}

/// 提取到的单张 Key
#[derive(Debug, Clone, Deserialize)]
pub struct DropKey {
    #[serde(default)]
    pub key: String,
}

/// `POST /api/v1/reservation` 与 `GET /api/v1/orders/{id}` 的共用响应体。
///
/// 两个端点形状一致，故用同一个结构 —— 轮询拿到的就是下单本该返回的东西。
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// 请求数量
    #[serde(default)]
    pub quantity: Option<u32>,
    /// 实际出货数
    #[serde(default)]
    pub purchased_count: Option<u32>,
    /// 实际扣款（人民币）。阶梯定价不存在，但仍以此为准。
    #[serde(default)]
    pub total_amount_cny: Option<Decimalish>,
    /// `completed` / `pending` / 其它终态
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub remaining_balance: Option<Decimalish>,
    #[serde(default)]
    pub keys: Vec<DropKey>,
}

impl OrderResponse {
    /// 是否仍在等待对账。只有明确的 `pending` 才继续轮询 ——
    /// 未知状态当终态处理，避免对着一个永不变化的字段死循环。
    pub fn is_pending(&self) -> bool {
        self.status
            .as_deref()
            .map(|s| s.trim().eq_ignore_ascii_case("pending"))
            .unwrap_or(false)
    }

    /// 是否已经拿到 Key。轮询在「有 Key」时也应停 ——
    /// 有卖家会在出货后仍留着 pending 之外的中间状态。
    pub fn has_keys(&self) -> bool {
        self.keys.iter().any(|k| !k.key.trim().is_empty())
    }
}

impl From<OrderResponse> for PurchaseResult {
    fn from(r: OrderResponse) -> Self {
        let keys: Vec<PurchasedKey> = r
            .keys
            .into_iter()
            .filter(|k| !k.key.trim().is_empty())
            .map(|k| PurchasedKey {
                key: k.key.trim().to_string(),
                account: None,
                password: None,
                issuer_url: None,
                price: None,
            })
            .collect();

        // 卖家回显的 purchased_count 与实际条数不一致时取较大者：
        // 少算会漏入库，而入库本身按 Key 去重，多算不会重复扣费。
        let purchased = r
            .purchased_count
            .unwrap_or(0)
            .max(keys.len() as u32);
        let total_debit = amount(&r.total_amount_cny);
        Self {
            purchased,
            requested: r.quantity,
            // 本家的「剩余」是账户余额，与首家同义
            remaining: amount(&r.remaining_balance),
            unit_price: total_debit.filter(|_| purchased > 0).map(|t| t / purchased as f64),
            total_debit,
            order_id: r.order_id,
            keys,
            // 幂等重放不回显，无从区分，保守记 false
            replayed: false,
        }
    }
}

/// 从本家的嵌套错误体里取人类可读信息。
///
/// 形态是 `{"error":{"code":"...","message":"...","request_id":"..."}}`，
/// [`super::client::VendorClient::parse`] 里的扁平 `{"error":"文本"}` 认不出来，
/// 不单独解会把整段 JSON 连 request_id 一起塞进面板。
///
/// `code` 也带上：`AUTH_REQUIRED` 与 `API_TOKEN_INVALID` 在本家是两种不同的
/// 401（前者是没带头，后者是 Key 不对），只看 message 分不清该改哪里。
pub fn error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let msg = err.get("message").and_then(|m| m.as_str()).map(str::trim);
    let code = err.get("code").and_then(|c| c.as_str()).map(str::trim);
    match (msg.filter(|s| !s.is_empty()), code.filter(|s| !s.is_empty())) {
        (Some(m), Some(c)) => Some(format!("{m}（{c}）")),
        (Some(m), None) => Some(m.to_string()),
        (None, Some(c)) => Some(c.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用文档给的报价样本
    #[test]
    fn 解析报价_真实样本() {
        let raw = r#"{"goods_id":1,"goods_name":"Kiro Key","stock":25,"quantity":2,
            "max_count":20,"unit_price_usd":"8.50","total_price_usd":"17.00",
            "exchange_rate":"6.8","unit_price_cny":"57.800000","total_price_cny":"115.600000",
            "currency":"CNY","available_balance":"1000.000000"}"#;
        let q: QuoteResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(q.stock, 25);
        assert_eq!(q.max_count, Some(20));

        let s: StockInfo = q.into();
        // 库存 25 但限购 20，可提取上限取小者
        assert_eq!(s.available, 20);
        assert_eq!(s.price_min, Some(57.8));
        assert_eq!(s.price_max, Some(57.8), "无阶梯定价，min = max");
        assert_eq!(s.balance, Some(1000.0), "报价接口顺带给余额");
    }

    #[test]
    fn 库存低于限购时取库存() {
        let q: QuoteResponse =
            serde_json::from_str(r#"{"stock":3,"max_count":20}"#).unwrap();
        assert_eq!(q.effective_available(), 3);
    }

    #[test]
    fn 报价缺限购时按库存() {
        let q: QuoteResponse = serde_json::from_str(r#"{"stock":7}"#).unwrap();
        assert_eq!(q.effective_available(), 7);
    }

    #[test]
    fn 报价转档案_余额与限购() {
        let raw = r#"{"stock":5,"max_count":20,"available_balance":"884.400000",
            "goods_name":"Kiro Key"}"#;
        let p: ProfileInfo = serde_json::from_str::<QuoteResponse>(raw).unwrap().into();
        assert_eq!(p.balance, Some(884.4));
        assert_eq!(p.quota, Some(884.4), "无配额概念，余额即全部额度");
        assert_eq!(p.max_purchase, Some(20));
        assert_eq!(p.name.as_deref(), Some("Kiro Key"));
        assert!(p.webhook_url.is_none(), "地址只能写不能读");
    }

    #[test]
    fn 金额字段兼容数字与字符串() {
        let text: QuoteResponse =
            serde_json::from_str(r#"{"stock":1,"unit_price_cny":"57.8"}"#).unwrap();
        assert_eq!(amount(&text.unit_price_cny), Some(57.8));

        let num: QuoteResponse =
            serde_json::from_str(r#"{"stock":1,"unit_price_cny":57.8}"#).unwrap();
        assert_eq!(amount(&num.unit_price_cny), Some(57.8));

        // 解析不了的串当缺失，不能让整份响应失败
        let bad: QuoteResponse =
            serde_json::from_str(r#"{"stock":1,"unit_price_cny":"待定"}"#).unwrap();
        assert_eq!(amount(&bad.unit_price_cny), None);
        assert_eq!(bad.stock, 1);
    }

    /// 用文档给的下单成功样本
    #[test]
    fn 解析下单成功_真实样本() {
        let raw = r#"{"order_id":"store_abc","client_order_id":"0123456789abcdef0123456789abcdef",
            "goods_name":"Kiro Key","quantity":2,"purchased_count":2,
            "total_amount_cny":"115.600000","status":"completed",
            "remaining_balance":"884.400000","keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#;
        let o: OrderResponse = serde_json::from_str(raw).unwrap();
        assert!(!o.is_pending());
        assert!(o.has_keys());

        let r: PurchaseResult = o.into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.requested, Some(2));
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.total_debit, Some(115.6));
        assert_eq!(r.unit_price, Some(57.8));
        assert_eq!(r.remaining, Some(884.4), "剩余即账户余额");
        assert_eq!(r.order_id.as_deref(), Some("store_abc"));
    }

    #[test]
    fn 待对账订单需继续轮询() {
        let raw = r#"{"order_id":"store_x","status":"pending","quantity":1,"keys":[]}"#;
        let o: OrderResponse = serde_json::from_str(raw).unwrap();
        assert!(o.is_pending());
        assert!(!o.has_keys());
    }

    /// 未知状态当终态：否则字段永不变化会把轮询拖满
    #[test]
    fn 未知状态不视为待对账() {
        for raw in [
            r#"{"status":"failed"}"#,
            r#"{"status":""}"#,
            r#"{"quantity":1}"#,
        ] {
            let o: OrderResponse = serde_json::from_str(raw).unwrap();
            assert!(!o.is_pending(), "不该继续轮询: {raw}");
        }
        // 大小写与空白容忍
        let o: OrderResponse = serde_json::from_str(r#"{"status":" Pending "}"#).unwrap();
        assert!(o.is_pending());
    }

    #[test]
    fn 出货数取回显与实际条数的较大者() {
        // 回显 1 但实发 2 条，按实际条数算，否则会漏入库
        let raw = r#"{"purchased_count":1,"keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<OrderResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 2);
    }

    #[test]
    fn 空key条目被剔除() {
        let raw = r#"{"purchased_count":0,"keys":[{"key":"  "},{"key":"ksk_a"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<OrderResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 1);
        assert_eq!(r.keys[0].key, "ksk_a");
    }

    #[test]
    fn 零个key时不产生inf单价() {
        let raw = r#"{"purchased_count":0,"total_amount_cny":"115.6","keys":[]}"#;
        let r: PurchaseResult = serde_json::from_str::<OrderResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 0);
        assert!(r.unit_price.is_none(), "0 个 Key 不能算出单价");
        // 扣费仍要留着，人工核对时需要看到钱确实扣了
        assert_eq!(r.total_debit, Some(115.6));
    }

    /// 用真实的 401 返回做样本
    #[test]
    fn 解析嵌套错误体_真实样本() {
        let raw = r#"{"error":{"code":"API_TOKEN_INVALID","details":{},
            "message":"API Key 无效","request_id":"req_9ec835d9"}}"#;
        let msg = error_message(raw).unwrap();
        assert!(msg.contains("API Key 无效"));
        // code 要带上：两种 401 的处置不同（没带头 vs Key 不对）
        assert!(msg.contains("API_TOKEN_INVALID"), "实际: {msg}");
        // request_id 不该混进面板文案
        assert!(!msg.contains("req_"), "实际: {msg}");
    }

    #[test]
    fn 解析扁平错误体() {
        assert_eq!(error_message(r#"{"error":"余额不足"}"#).as_deref(), Some("余额不足"));
    }

    #[test]
    fn 只有code时也给出信息() {
        let raw = r#"{"error":{"code":"OUT_OF_STOCK","details":{}}}"#;
        assert_eq!(error_message(raw).as_deref(), Some("OUT_OF_STOCK"));
    }

    #[test]
    fn 无法识别的错误体返回none() {
        assert!(error_message("upstream boom").is_none());
        assert!(error_message(r#"{"ok":true}"#).is_none());
        assert!(error_message(r#"{"error":{"message":"  "}}"#).is_none());
    }
}
