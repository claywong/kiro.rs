//! KiroShop（kiro.yonghu.shop）客户 API v1.2 完整客户端
//!
//! 覆盖 openapi.yaml（`GET /api/v1/openapi.yaml`）的全部端点：
//! 商品 / 报价 / 订单（建单、支付、取消、取号、增量拿号）/ 补号 / 售后 /
//! 账户（余额、流水、每日限额）/ 事件流 / webhook 管理。
//!
//! 实测备注（文档没写清的）：
//! - Cloudflare 会按 TLS 指纹拦截（错误 1010）：urllib/部分 UA 过不去，
//!   reqwest 默认客户端实测可用，勿随意更换 HTTP 栈或加奇怪的头。
//! - 建单即从余额扣款（`paid_at` 与 `created_at` 仅差几十毫秒），但文档语义
//!   `pay_with` 缺省 `none`（待支付）。调用方应显式传 `pay_with=balance`，
//!   并在订单仍为 pending 时调 [`KiroshopClient::pay_order`] 兜底 —— 两处都做，
//!   无论站方默认值最终按哪边执行都正确。
//! - 限速：单 Key 60 次/分钟。后台轮询的节奏（事件 45s、发货 60s、账户 5min）
//!   已按此预算设计，勿无脑加密频。
//!
//! @author wangzhong

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::config::{KiroshopConfig, OPTION_NORMAL};

// ============ 错误 ============

/// 上游错误。`code` 是卖家错误码（`waiting_stock` / `not_paid` /
/// `insufficient_balance` / `price_changed` / …），调用方据此分支而不是解析文案。
#[derive(Debug, Clone)]
pub struct KiroshopError {
    pub status: Option<u16>,
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for KiroshopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.status, &self.code) {
            (Some(s), Some(c)) => write!(f, "[{s} {c}] {}", self.message),
            (Some(s), None) => write!(f, "[{s}] {}", self.message),
            (None, _) => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for KiroshopError {}

pub type Result<T> = std::result::Result<T, KiroshopError>;

/// 解析卖家错误体：`{"error":{"code":..,"message":..}}` / `{"error":"文本"}` /
/// `{"message":..}` / `{"detail":..}`。code 与 message 都尽量带出。
fn parse_error_body(body: &str) -> (Option<String>, String) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (None, truncate(body, 300));
    };
    if let Some(e) = v.get("error") {
        let code = e.get("code").and_then(Value::as_str).map(str::to_string);
        let msg = e
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| e.as_str())
            .map(str::to_string);
        if let Some(msg) = msg {
            return (code, msg);
        }
        if let Some(code) = code {
            return (Some(code.clone()), code);
        }
    }
    let msg = v
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| v.get("detail").and_then(Value::as_str));
    (None, msg.map(str::to_string).unwrap_or_else(|| truncate(body, 300)))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

// ============ DTO ============

/// 通用信封：`{"data": ...}`（部分带 `meta`）
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(default)]
    meta: Option<Value>,
}

fn unwrap_data<T: for<'de> Deserialize<'de>>(text: &str) -> Result<(T, Option<Value>)> {
    let env: Envelope<T> = serde_json::from_str(text).map_err(|e| KiroshopError {
        status: None,
        code: Some("parse_response".into()),
        message: format!("解析响应失败: {e}；原文片段: {}", truncate(text, 200)),
    })?;
    Ok((env.data, env.meta))
}

/// 商品档位（normal 普通 / rush 加急）
// 字段名对齐卖家 API 的 snake_case（in_stock / warranty_hours ...），序列化给面板也是同名
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ProductOption {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub warranty_hours: Option<f64>,
    #[serde(default)]
    pub warranty_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ProductStock {
    #[serde(default)]
    pub has_stock: Option<bool>,
    /// 没现货但可预定：付款后排队、到货自动发
    #[serde(default)]
    pub backorder: Option<bool>,
    #[serde(default)]
    pub eta: Option<String>,
    #[serde(default)]
    pub in_stock: Option<bool>,
    #[serde(default)]
    pub stock_level: Option<String>,
    #[serde(default)]
    pub available: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Product {
    pub id: i64,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub min_quantity_per_order: Option<u32>,
    #[serde(default)]
    pub max_quantity_per_order: Option<u32>,
    #[serde(default)]
    pub warranty_mode: Option<String>,
    #[serde(default)]
    pub warranty_hours: Option<f64>,
    #[serde(default)]
    pub options: Vec<ProductOption>,
    #[serde(default)]
    pub stock: ProductStock,
}

impl Product {
    /// 可下单的精确现货数；`in_stock = false` 一律按 0 处理
    pub fn available(&self) -> u32 {
        if self.stock.in_stock == Some(false) {
            return 0;
        }
        self.stock.available.unwrap_or(0).clamp(0, u32::MAX as i64) as u32
    }

    /// 指定档位单价，缺失回落商品标价
    pub fn option_price(&self, key: &str) -> Option<f64> {
        self.options
            .iter()
            .find(|o| o.key == key)
            .and_then(|o| o.price)
            .or(self.price)
    }

    /// 默认档位：普通档优先，其次第一个，都没有则 None（下单不传 option_key）
    pub fn default_option(&self) -> Option<&ProductOption> {
        self.options
            .iter()
            .find(|o| o.key == OPTION_NORMAL)
            .or_else(|| self.options.first())
    }
}

/// 订单（列表与详情共用；详情额外字段见 [`OrderDetail`）
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Order {
    pub id: String,
    #[serde(default)]
    pub product_id: i64,
    #[serde(default)]
    pub product_name: String,
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub quantity: Option<u32>,
    #[serde(default)]
    pub unit_price: Option<f64>,
    #[serde(default)]
    pub selected_option_key: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub status_text: Option<String>,
    #[serde(default)]
    pub after_sale_status: Option<String>,
    /// 已发出几个
    #[serde(default)]
    pub delivered: Option<u32>,
    #[serde(default)]
    pub lane: Option<String>,
    #[serde(default)]
    pub client_ref: Option<String>,
    #[serde(default)]
    pub refund: Option<Value>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub paid_at: Option<String>,
    #[serde(default)]
    pub picked_up_at: Option<String>,
    #[serde(default)]
    pub warranty_expires_at: Option<String>,
}

impl Order {
    pub fn status_str(&self) -> &str {
        self.status.as_deref().unwrap_or("unknown")
    }

    /// 是否已终态（不再需要轮询发货）
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status_str(),
            "picked_up" | "completed" | "supplemented" | "timeout" | "failed" | "refunded"
        )
    }

    pub fn delivered_count(&self) -> u32 {
        self.delivered.unwrap_or(0)
    }

    pub fn quantity_count(&self) -> u32 {
        self.quantity.unwrap_or(0)
    }
}

/// 订单详情：进度、预计时间、付款地址、已交付内容
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct OrderDetail {
    #[serde(flatten)]
    pub order: Order,
    #[serde(default)]
    pub eta: Option<String>,
    /// 退款后账号已收回，delivery 不再给凭证
    #[serde(default)]
    pub returned: Option<bool>,
    #[serde(default)]
    pub payment: Option<Value>,
    #[serde(default)]
    pub delivery: Option<Value>,
}

/// 增量拿号的一页
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct DeliveryPage {
    #[serde(default)]
    pub data: Vec<DeliveryItem>,
    #[serde(default)]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct DeliveryItem {
    /// 这一单里的序号，从 1 开始，按到货先后 —— 增量游标就是它
    #[serde(default)]
    pub seq: i64,
    /// 凭证原文（JSON 字符串）
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub delivered_at: Option<String>,
}

impl DeliveryPage {
    pub fn next_after_seq(&self) -> Option<i64> {
        self.meta
            .as_ref()
            .and_then(|m| m.get("next_after_seq"))
            .and_then(Value::as_i64)
    }
}

/// 账户信息（余额、今日已扣、每日上限、待支付单数、回调状态）
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct AccountInfo {
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub api_spent_today: Option<f64>,
    #[serde(default)]
    pub daily_pay_limit: Option<f64>,
    #[serde(default)]
    pub daily_pay_remaining: Option<f64>,
    #[serde(default)]
    pub key_can_pay: Option<bool>,
    #[serde(default)]
    pub max_pending_orders: Option<u32>,
    #[serde(default)]
    pub pending_orders: Option<u32>,
    #[serde(default)]
    pub order_expire_minutes: Option<u32>,
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub webhook: Option<Value>,
}

/// 报价（不下单、不扣钱）
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Quote {
    #[serde(default)]
    pub product_id: i64,
    #[serde(default)]
    pub option_key: Option<String>,
    #[serde(default)]
    pub quantity: Option<u32>,
    #[serde(default)]
    pub units: Option<u32>,
    #[serde(default)]
    pub unit_price: Option<f64>,
    #[serde(default)]
    pub original_amount: Option<f64>,
    /// 这一单要付多少
    #[serde(default)]
    pub amount: Option<f64>,
    #[serde(default)]
    pub discount: Option<Value>,
    #[serde(default)]
    pub discount_amount: Option<f64>,
    #[serde(default)]
    pub warranty_hours: Option<f64>,
    #[serde(default)]
    pub lane: Option<String>,
    /// 精确现货数
    #[serde(default)]
    pub available: Option<i64>,
    #[serde(default)]
    pub in_stock_now: Option<i64>,
    #[serde(default)]
    pub backorder: Option<bool>,
    #[serde(default)]
    pub backorder_units: Option<i64>,
    #[serde(default)]
    pub eta: Option<String>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub balance_enough: Option<bool>,
}

/// 事件（webhook 推送与 `/events` 拉取同构）。事件里不带账号凭证，
/// 收到 `order.delivery` 后用 `/orders/{id}/delivery` 拿号。
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ShopEvent {
    pub id: i64,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub order: Option<Order>,
    /// 事件附加数据（如 delivery 的 seq 范围）
    #[serde(default)]
    pub data: Option<Value>,
}

// ============ 请求构造 ============

/// 建单 / 报价请求体
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OrderRequest {
    pub product_id: i64,
    pub quantity: u32,
    /// 商品有 options 时必填（normal / rush）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub option_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coupon_code: Option<String>,
    /// balance = 下单即用余额扣款。显式传，不依赖卖家默认值。
    pub pay_with: String,
    /// 现货不够时排队（allow）还是拒单（deny）
    pub backorder: String,
    /// 价格保护：实际金额更高就拒单（409 price_changed）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<f64>,
    /// 调用方自己的单号，订单和事件里原样带回
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
}

// ============ 客户端 ============

pub struct KiroshopClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl KiroshopClient {
    pub fn new(http: reqwest::Client, config: &KiroshopConfig) -> Self {
        Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.trim().to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, String)> {
        let mut req = self
            .http
            .request(method, &self.url(path))
            .query(query)
            .timeout(Duration::from_secs(30))
            .bearer_auth(&self.api_key);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| KiroshopError {
            status: None,
            code: Some("network".into()),
            message: e.to_string(),
        })?;
        let status = resp.status().as_u16();
        // Retry-After：429 时卖家会带这个头，并进错误文案供退避
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp.text().await.map_err(|e| KiroshopError {
            status: Some(status),
            code: Some("read_body".into()),
            message: format!("读取响应体失败: {e}"),
        })?;
        if !(200..300).contains(&status) {
            let (code, mut message) = parse_error_body(&text);
            if status == 429 {
                if let Some(ra) = retry_after {
                    message = format!("{message}（Retry-After {ra}s）");
                }
            }
            return Err(KiroshopError {
                status: Some(status),
                code,
                message,
            });
        }
        Ok((status, text))
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, String)> {
        self.send(reqwest::Method::GET, path, query, &[], None).await
    }

    async fn post(&self, path: &str, body: Option<&Value>) -> Result<(u16, String)> {
        self.send(reqwest::Method::POST, path, &[], &[], body).await
    }

    async fn put(&self, path: &str, body: &Value) -> Result<(u16, String)> {
        self.send(reqwest::Method::PUT, path, &[], &[], Some(body)).await
    }

    async fn delete(&self, path: &str) -> Result<(u16, String)> {
        self.send(reqwest::Method::DELETE, path, &[], &[], None).await
    }

    async fn get_data<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let (_, text) = self.get(path, &[]).await?;
        Ok(unwrap_data::<T>(&text)?.0)
    }

    // ---- 商品 ----

    pub async fn products(&self) -> Result<Vec<Product>> {
        self.get_data("/api/v1/products").await
    }

    pub async fn product(&self, product_id: i64) -> Result<Product> {
        self.get_data(&format!("/api/v1/products/{product_id}")).await
    }

    /// 报价（不下单、不扣钱）
    pub async fn quote(&self, req: &OrderRequest) -> Result<Quote> {
        let (_, text) = self
            .post("/api/v1/quote", Some(&serde_json::to_value(req).unwrap_or(Value::Null)))
            .await?;
        Ok(unwrap_data::<Quote>(&text)?.0)
    }

    // ---- 订单 ----

    pub async fn orders(&self, page: u32, status: Option<&str>) -> Result<(Vec<Order>, Option<Value>)> {
        let mut query = vec![("page", page.to_string())];
        if let Some(s) = status {
            query.push(("status", s.to_string()));
        }
        let (_, text) = self.get("/api/v1/orders", &query).await?;
        let v: Value = serde_json::from_str(&text).map_err(|e| KiroshopError {
            status: None,
            code: Some("parse_response".into()),
            message: format!("解析订单列表失败: {e}"),
        })?;
        let items = v
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let orders: Vec<Order> = items
            .iter()
            .filter_map(|i| serde_json::from_value(i.clone()).ok())
            .collect();
        Ok((orders, v.get("meta").cloned()))
    }

    /// 拉全部订单（翻页走满）。订单不多，一次也就几页。
    pub async fn all_orders(&self) -> Result<Vec<Order>> {
        let mut all = Vec::new();
        for page in 1..=50u32 {
            let (rows, meta) = self.orders(page, None).await?;
            let pages = meta
                .as_ref()
                .and_then(|m| m.get("pages"))
                .and_then(Value::as_u64)
                .unwrap_or(1) as u32;
            all.extend(rows);
            if page >= pages {
                break;
            }
        }
        Ok(all)
    }

    pub async fn order(&self, order_id: &str) -> Result<OrderDetail> {
        self.get_data(&format!("/api/v1/orders/{order_id}")).await
    }

    /// 建单。`idempotency_key` 必填：同键同参重发返回原订单，超时重试复用它不会重复扣款。
    pub async fn create_order(
        &self,
        req: &OrderRequest,
        idempotency_key: &str,
    ) -> Result<OrderDetail> {
        let (_, text) = self
            .send(
                reqwest::Method::POST,
                "/api/v1/orders",
                &[],
                &[("Idempotency-Key", idempotency_key)],
                Some(&serde_json::to_value(req).unwrap_or(Value::Null)),
            )
            .await?;
        Ok(unwrap_data::<OrderDetail>(&text)?.0)
    }

    /// 用余额付一张待支付订单（幂等，`already_paid` 语义见 meta）
    pub async fn pay_order(&self, order_id: &str) -> Result<OrderDetail> {
        let (_, text) = self.post(&format!("/api/v1/orders/{order_id}/pay"), None).await?;
        Ok(unwrap_data::<OrderDetail>(&text)?.0)
    }

    /// 取消还没付款的订单
    pub async fn cancel_order(&self, order_id: &str) -> Result<Value> {
        let (_, text) = self.post(&format!("/api/v1/orders/{order_id}/cancel"), None).await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 取号（对已取号订单幂等：`data.pickup.already = true` 且仍返回 delivery）
    pub async fn pickup_order(&self, order_id: &str) -> Result<(OrderDetail, bool)> {
        let (_, text) = self.post(&format!("/api/v1/orders/{order_id}/pickup"), None).await?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let detail: OrderDetail = serde_json::from_value(
            v.get("data").cloned().unwrap_or(Value::Null),
        )
        .unwrap_or_default();
        let already = v
            .pointer("/data/pickup/already")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((detail, already))
    }

    /// 增量拿号：`after_seq` 之后新到的。游标用返回的 `next_after_seq`。
    pub async fn delivery(&self, order_id: &str, after_seq: i64, limit: u32) -> Result<DeliveryPage> {
        let limit = limit.clamp(1, 500);
        let (_, text) = self
            .get(
                &format!("/api/v1/orders/{order_id}/delivery"),
                &[
                    ("after_seq", after_seq.to_string()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        Ok(unwrap_data::<DeliveryPage>(&text)?.0)
    }

    // ---- 补号 ----

    /// 管理员补发的号（结构松散，原样透出给面板）
    pub async fn supplements(&self, order_id: &str) -> Result<Vec<Value>> {
        let (_, text) = self
            .get(&format!("/api/v1/orders/{order_id}/supplements"), &[])
            .await?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(v.get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// 领取补号（领取即代表原订单质保结束）
    pub async fn pickup_supplement(&self, order_id: &str, supplement_id: i64) -> Result<Value> {
        let (_, text) = self
            .post(
                &format!("/api/v1/orders/{order_id}/supplements/{supplement_id}/pickup"),
                None,
            )
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    // ---- 售后 ----

    pub async fn after_sale_list(&self, order_id: &str) -> Result<Vec<Value>> {
        let (_, text) = self
            .get(&format!("/api/v1/orders/{order_id}/after-sale"), &[])
            .await?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(v.get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// 提交售后申请。kind: banned / login / quota / missing / other
    pub async fn after_sale_create(&self, order_id: &str, kind: &str, reason: Option<&str>) -> Result<Value> {
        let mut body = serde_json::json!({ "kind": kind });
        if let Some(r) = reason.filter(|r| !r.trim().is_empty()) {
            body["reason"] = serde_json::json!(r.trim());
        }
        let (_, text) = self
            .post(&format!("/api/v1/orders/{order_id}/after-sale"), Some(&body))
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    // ---- 账户 ----

    pub async fn account(&self) -> Result<AccountInfo> {
        self.get_data("/api/v1/account").await
    }

    /// 余额流水（结构松散，原样透出）
    pub async fn balance_logs(&self, page: u32, size: u32) -> Result<Value> {
        let (_, text) = self
            .get(
                "/api/v1/account/balance-logs",
                &[
                    ("page", page.to_string()),
                    ("size", size.clamp(1, 100).to_string()),
                ],
            )
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 每日 API 扣款上限（0 = 不限）
    pub async fn set_daily_limit(&self, daily_pay_limit: f64) -> Result<AccountInfo> {
        let body = serde_json::json!({ "daily_pay_limit": daily_pay_limit });
        let (_, text) = self.put("/api/v1/account/settings", &body).await?;
        Ok(unwrap_data::<AccountInfo>(&text)?.0)
    }

    // ---- 事件与 webhook ----

    /// 事件流（拉取）。`after` 填上次拿到的最大事件 id。
    pub async fn events(&self, after: i64, limit: u32) -> Result<(Vec<ShopEvent>, Option<i64>, bool)> {
        let limit = limit.clamp(1, 200);
        let (_, text) = self
            .get("/api/v1/events", &[
                ("after", after.to_string()),
                ("limit", limit.to_string()),
            ])
            .await?;
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let items = v
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let events: Vec<ShopEvent> = items
            .iter()
            .filter_map(|i| serde_json::from_value(i.clone()).ok())
            .collect();
        let meta = v.get("meta").cloned().unwrap_or(Value::Null);
        let next_after = meta.get("next_after").and_then(Value::as_i64);
        let has_more = meta.get("has_more").and_then(Value::as_bool).unwrap_or(false);
        Ok((events, next_after, has_more))
    }

    /// 当前回调设置（含事件订阅）。**secret 只在第一次 PUT 时返回一次**，GET 拿不到。
    pub async fn webhook_get(&self) -> Result<Value> {
        self.get_data("/api/v1/webhook").await
    }

    /// 设置回调。返回体第一次带 `secret`（只显示一次），调用方必须立即持久化。
    pub async fn webhook_put(&self, url: &str, events: Option<&[String]>, enabled: bool) -> Result<Value> {
        let mut body = serde_json::json!({ "url": url, "enabled": enabled });
        if let Some(ev) = events {
            body["events"] = serde_json::json!(ev);
        }
        let (_, text) = self.put("/api/v1/webhook", &body).await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn webhook_delete(&self) -> Result<Value> {
        let (_, text) = self.delete("/api/v1/webhook").await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 马上发一条 ping，返回对方响应码
    pub async fn webhook_test(&self) -> Result<Value> {
        let (_, text) = self.post("/api/v1/webhook/test", None).await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 换签名密钥（旧的立即失效，新的只显示一次）
    pub async fn webhook_rotate_secret(&self) -> Result<Value> {
        let (_, text) = self.post("/api/v1/webhook/rotate-secret", None).await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 回调投递记录
    pub async fn webhook_deliveries(&self, status: Option<&str>, page: u32, size: u32) -> Result<Value> {
        let mut query = vec![
            ("page", page.to_string()),
            ("size", size.clamp(1, 100).to_string()),
        ];
        if let Some(s) = status {
            query.push(("status", s.to_string()));
        }
        let (_, text) = self.get("/api/v1/webhook/deliveries", &query).await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// 手动重发一条投递
    pub async fn webhook_retry(&self, delivery_id: i64) -> Result<Value> {
        let (_, text) = self
            .post(&format!("/api/v1/webhook/deliveries/{delivery_id}/retry"), None)
            .await?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误体解析带code与message() {
        let (code, msg) = parse_error_body(
            r#"{"error":{"code":"waiting_stock","message":"暂无现货","delivered":0,"eta":"约3小时"}}"#,
        );
        assert_eq!(code.as_deref(), Some("waiting_stock"));
        assert_eq!(msg, "暂无现货");
    }

    #[test]
    fn 错误体兼容纯文本与message形态() {
        let (code, msg) = parse_error_body(r#"{"error":"库存不足"}"#);
        assert_eq!(code, None);
        assert_eq!(msg, "库存不足");
        let (_, msg) = parse_error_body(r#"{"message":"m"}"#);
        assert_eq!(msg, "m");
        let (_, msg) = parse_error_body("<html>");
        assert!(msg.starts_with("<html>"));
    }

    #[test]
    fn 商品解析与库存语义() {
        let p: Product = serde_json::from_str(
            r#"{"id":403,"display_name":"Kiro Pro","price":72.0,
                "options":[{"key":"rush","price":82.0},{"key":"normal","price":72.0}],
                "stock":{"available":12,"in_stock":true,"backorder":true,"eta":"约1小时"}}"#,
        )
        .unwrap();
        assert_eq!(p.available(), 12);
        assert_eq!(p.default_option().unwrap().key, "normal");
        assert_eq!(p.option_price("rush"), Some(82.0));
        assert_eq!(p.option_price("normal"), Some(72.0));

        let off: Product = serde_json::from_str(
            r#"{"id":1,"stock":{"available":9,"in_stock":false}}"#,
        )
        .unwrap();
        assert_eq!(off.available(), 0);
    }

    #[test]
    fn 订单解析与终态判定() {
        let o: Order = serde_json::from_str(
            r#"{"id":"8e369c1e","product_id":403,"quantity":3,"delivered":0,
                "status":"paid","status_text":"已付款，等待发货 / 取号","amount":216.0,
                "client_ref":null,"order_warranty_mode":"first_login"}"#,
        )
        .unwrap();
        assert!(!o.is_terminal());
        assert_eq!(o.delivered_count(), 0);

        let done: Order = serde_json::from_str(r#"{"id":"x","status":"picked_up","delivered":3}"#).unwrap();
        assert!(done.is_terminal());
    }

    #[test]
    fn 增量拿号页解析() {
        let page: DeliveryPage = serde_json::from_str(
            r#"{"data":[{"seq":1,"content":"{\"a\":1}","delivered_at":null},
                {"seq":2,"content":" {\"b\":2} "}],"meta":{"delivered":2,"quantity":3,
                "status":"paid","next_after_seq":2,"has_more":false}}"#,
        )
        .unwrap();
        assert_eq!(page.data.len(), 2);
        assert_eq!(page.data[1].content.trim(), r#"{"b":2}"#);
        assert_eq!(page.next_after_seq(), Some(2));
    }

    #[test]
    fn 账户信息解析() {
        // 信封路径：unwrap_data 剥掉 {"data": ...}
        let (acc, _): (AccountInfo, Option<Value>) = unwrap_data(
            r#"{"data":{"api_spent_today":0.0,"balance":36.0,"customer_id":473,
                "daily_pay_limit":null,"key_can_pay":true,"max_pending_orders":5,
                "pending_orders":0,"webhook":{"enabled":false,"url":null}}}"#,
        )
        .unwrap();
        assert_eq!(acc.balance, Some(36.0));
        assert_eq!(acc.key_can_pay, Some(true));
        assert_eq!(acc.pending_orders, Some(0));
        assert_eq!(acc.daily_pay_limit, None);
    }

    #[test]
    fn 建单请求体蛇形命名并省略可选字段() {
        let req = OrderRequest {
            product_id: 403,
            quantity: 3,
            option_key: Some("normal".into()),
            coupon_code: None,
            pay_with: "balance".into(),
            backorder: "deny".into(),
            max_amount: Some(220.0),
            client_ref: Some("autobuy-1".into()),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["product_id"], 403);
        assert_eq!(v["pay_with"], "balance");
        assert_eq!(v["backorder"], "deny");
        assert!(v.get("coupon_code").is_none());
        assert_eq!(v["client_ref"], "autobuy-1");
    }

    #[test]
    fn 事件解析() {
        let ev: ShopEvent = serde_json::from_str(
            r#"{"id":42,"type":"order.delivery","created_at":"2026-11-02T06:00:00Z",
                "order":{"id":"o1","status":"paid","delivered":1},"data":{"seq_from":1,"seq_to":1}}"#,
        )
        .unwrap();
        assert_eq!(ev.r#type, "order.delivery");
        assert_eq!(ev.order.as_ref().unwrap().id, "o1");
    }
}
