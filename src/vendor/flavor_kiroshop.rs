//! KiroShop（kiro.yonghu.shop）的协议实现：`/api/v1/*` + `Authorization: Bearer ks_live_xxx`
//!
//! 与前几家都不同：这是一家**商品制**店铺，没有「提取 N 个 Key」的概念，
//! 流程是 选商品 → `POST /orders` 建单 → `POST /orders/{id}/pickup` 取号。
//! 交付内容是 **JSON 凭据**（IdC 型：email / refreshToken / clientId / clientSecret ...），
//! 不是 `ksk_` Key，由 [`super::import`] 的 JSON 解析入库。
//!
//! 几处实测结论（文档没写）：
//! - 文档 server 写的 `/shop-v2` 前缀是错的，真实根路径就是域名下的 `/api/v1`。
//! - 建单时 `paid_at` 与 `created_at` 仅差几十毫秒，疑似建单即从余额扣款。
//! - 对已取号订单再次 pickup 是幂等的：`pickup.already = true` 且仍返回 delivery。
//! - 没有余额接口，面板余额留空。
//!
//! 幂等：建单必须带 `Idempotency-Key`，同客户同键同参重试返回原订单，因此
//! 超时重试时复用同一个 `client_order_id` 不会重复扣款。
//!
//! 商品映射成「区」（zone = 商品 id）复用面板现有的选区交互；档位固定「普通」。
//!
//! @author wangzhong

use serde::Deserialize;
use serde_json::Value;

use super::protocol::{
    OrderInfo, Paged, PurchaseResult, PurchasedKey, StockInfo, VendorApiError, ZoneStock, truncate,
};

pub const PATH_PRODUCTS: &str = "/api/v1/products";
pub const PATH_ORDERS: &str = "/api/v1/orders";

/// 默认档位。另一档「加急」贵 ¥10，不在本对接范围。
pub const DEFAULT_OPTION_KEY: &str = "normal";

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(default)]
    meta: Option<Meta>,
}

#[derive(Debug, Deserialize)]
struct Meta {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    pages: Option<u32>,
    #[serde(default)]
    total: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProductOption {
    pub key: String,
    #[serde(default)]
    pub price: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProductStock {
    #[serde(default)]
    pub available: Option<i64>,
    #[serde(default)]
    pub in_stock: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Product {
    pub id: i64,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub price: Option<f64>,
    #[serde(default)]
    pub options: Vec<ProductOption>,
    #[serde(default)]
    pub stock: ProductStock,
}

impl Product {
    /// 选档位：优先「普通」，没有就退到第一个；一个都没有则不传 option_key。
    pub fn pick_option(&self) -> Option<&ProductOption> {
        self.options
            .iter()
            .find(|o| o.key == DEFAULT_OPTION_KEY)
            .or_else(|| self.options.first())
    }

    /// 该档位的单价，缺失时回落商品标价
    pub fn unit_price(&self) -> Option<f64> {
        self.pick_option().and_then(|o| o.price).or(self.price)
    }

    /// 可下单库存。`in_stock = false` 一律视为 0。
    pub fn available(&self) -> u32 {
        if self.stock.in_stock == Some(false) {
            return 0;
        }
        self.stock.available.unwrap_or(0).max(0) as u32
    }
}

/// 商品列表 → 中立库存（每个商品一个「区」）
pub fn products_to_stock(products: &[Product]) -> StockInfo {
    let zones: Vec<ZoneStock> = products
        .iter()
        .map(|p| ZoneStock {
            zone: p.id.to_string(),
            label: Some(p.display_name.clone()),
            available: p.available(),
            stock: Some(p.available()),
            unit_price: p.unit_price(),
            enabled: true,
            departed_at: None,
            alive_secs: None,
            alive_text: None,
        })
        .collect();
    let orderable: Vec<&ZoneStock> = zones.iter().filter(|z| z.available > 0).collect();
    let prices = orderable.iter().filter_map(|z| z.unit_price);
    StockInfo {
        available: orderable.iter().map(|z| z.available).sum(),
        price_min: prices.clone().fold(None, |a: Option<f64>, v| Some(a.map_or(v, |x| x.min(v)))),
        price_max: prices.fold(None, |a: Option<f64>, v| Some(a.map_or(v, |x| x.max(v)))),
        balance: None,
        zones,
    }
}

#[derive(Debug, Deserialize)]
struct OrderRow {
    id: String,
    #[serde(default)]
    product_id: i64,
    #[serde(default)]
    quantity: Option<u32>,
    #[serde(default)]
    amount: Option<f64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
}

/// 订单列表 → 中立分页。余额充值单（`product_id == 0`）不是取号订单，剔除。
fn orders_to_paged(rows: Vec<OrderRow>, meta: Option<Meta>) -> Paged<OrderInfo> {
    let items = rows
        .into_iter()
        .filter(|r| r.product_id != 0)
        .map(|r| {
            let picked = r.status.as_deref() == Some("picked_up");
            OrderInfo {
                client_order_id: None,
                order_id: Some(r.id),
                requested: r.quantity,
                // 只有 picked_up 才算已交付；其余状态不能冒充成交
                purchased: if picked { r.quantity } else { Some(0) },
                total_debit: r.amount,
                created_at: r.created_at,
            }
        })
        .collect();
    Paged {
        items,
        total: meta.as_ref().and_then(|m| m.total),
        page: meta.as_ref().and_then(|m| m.page),
        page_size: None,
        pages: meta.as_ref().and_then(|m| m.pages),
    }
}

/// 取错误文案：兼容 `{"error":"文本"}` / `{"error":{"message":..}}` / `{"message":..}` / `{"detail":..}`
pub fn error_message(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let pick = |v: &Value| v.as_str().map(str::to_string);
    if let Some(e) = v.get("error") {
        if let Some(s) = pick(e) {
            return Some(s);
        }
        if let Some(s) = e.get("message").and_then(pick) {
            return Some(s);
        }
    }
    v.get("message")
        .and_then(pick)
        .or_else(|| v.get("detail").and_then(pick))
}

/// 从取号响应 / 订单详情里抽出交付内容（每项一段 JSON 凭据字符串）
pub fn delivery_contents(v: &Value) -> Vec<String> {
    v.pointer("/data/delivery/items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("content").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 建单响应里取订单 id（兼容 `data.id` / `data.order.id`）
fn order_id_of(v: &Value) -> Option<String> {
    v.pointer("/data/id")
        .or_else(|| v.pointer("/data/order/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub struct KiroshopClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl KiroshopClient {
    pub fn new(http: reqwest::Client, base_url: String, api_key: String) -> Self {
        Self {
            http,
            base_url,
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// 发请求并取回 `(状态码, 响应体)`；非 2xx 转成带文案的错误
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, String), VendorApiError> {
        let resp = req
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;
        if !status.is_success() {
            return Err(VendorApiError {
                status: Some(status.as_u16()),
                message: error_message(&text).unwrap_or_else(|| truncate(&text, 300)),
            });
        }
        Ok((status.as_u16(), text))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, VendorApiError> {
        let (code, text) = self
            .send(self.http.get(self.url(path)).query(query))
            .await?;
        serde_json::from_str(&text).map_err(|e| VendorApiError {
            status: Some(code),
            message: format!("解析响应失败: {e}；原文片段: {}", truncate(&text, 200)),
        })
    }

    async fn fetch_products(&self) -> Result<Vec<Product>, VendorApiError> {
        let env: Envelope<Vec<Product>> = self.get_json(PATH_PRODUCTS, &[]).await?;
        Ok(env.data)
    }

    pub async fn stock(&self) -> Result<StockInfo, VendorApiError> {
        Ok(products_to_stock(&self.fetch_products().await?))
    }

    pub async fn purchase_orders(
        &self,
        page: Option<u32>,
    ) -> Result<Paged<OrderInfo>, VendorApiError> {
        let env: Envelope<Vec<OrderRow>> = self
            .get_json(PATH_ORDERS, &[("page", page.unwrap_or(1).to_string())])
            .await?;
        Ok(orders_to_paged(env.data, env.meta))
    }

    /// 建单 + 取号。`zone` 是商品 id，**必填**（本家没有「随便来一个」）。
    ///
    /// `client_order_id` 同时是建单的 `Idempotency-Key`：超时重试复用它即命中原订单，
    /// 不会重复扣款。建单成功后取号失败时错误里点明订单号与「不要换号重试」。
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
        zone: Option<&str>,
    ) -> Result<PurchaseResult, VendorApiError> {
        let product_id: i64 = zone
            .map(str::trim)
            .and_then(|z| z.parse().ok())
            .ok_or_else(|| VendorApiError {
                status: None,
                message: "KiroShop 下单必须指定商品（zone 为商品 id）".to_string(),
            })?;
        let products = self.fetch_products().await?;
        let product = products
            .iter()
            .find(|p| p.id == product_id)
            .ok_or_else(|| VendorApiError {
                status: None,
                message: format!("商品 {product_id} 不存在"),
            })?;
        let quantity = count.max(1);
        let unit_price = product.unit_price();

        let mut body = serde_json::json!({ "product_id": product_id, "quantity": quantity });
        if let Some(opt) = product.pick_option() {
            body["option_key"] = serde_json::json!(opt.key);
        }
        tracing::info!(
            vendor = "kiroshop",
            product = %product.display_name,
            product_id,
            quantity,
            order = %client_order_id,
            "KiroShop 下单"
        );
        let (_, text) = self
            .send(
                self.http
                    .post(self.url(PATH_ORDERS))
                    .header("Idempotency-Key", client_order_id)
                    .json(&body),
            )
            .await?;
        let created: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let order_id = order_id_of(&created).ok_or_else(|| VendorApiError {
            status: None,
            message: format!("建单成功但未返回订单 id: {}", truncate(&text, 200)),
        })?;

        let pickup_path = format!("{PATH_ORDERS}/{order_id}/pickup");
        let (_, text) = self
            .send(self.http.post(self.url(&pickup_path)))
            .await
            .map_err(|e| VendorApiError {
                status: e.status,
                message: format!(
                    "订单 {order_id} 已创建，但取号失败：{}；用同一订单号重试即可，不要换号重复下单",
                    e.message
                ),
            })?;
        let picked: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let contents = delivery_contents(&picked);
        let amount = picked
            .pointer("/data/order/amount")
            .and_then(Value::as_f64)
            .or_else(|| unit_price.map(|p| p * quantity as f64));

        Ok(PurchaseResult {
            purchased: contents.len() as u32,
            requested: Some(quantity),
            remaining: None,
            unit_price,
            total_debit: amount,
            order_id: Some(order_id),
            keys: contents
                .into_iter()
                .map(|c| PurchasedKey {
                    key: c,
                    account: None,
                    password: None,
                    issuer_url: None,
                    price: unit_price,
                    region: None,
                })
                .collect(),
            // 取号接口自己标了「已取过」：说明这是对旧订单的重放，没有新扣费
            replayed: picked
                .pointer("/data/pickup/already")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            zone: Some(product_id.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(json: &str) -> Product {
        serde_json::from_str(json).unwrap()
    }

    const PRO_MAX: &str = r#"{"id":405,"display_name":"Pro Max","price":68.0,
        "options":[{"key":"rush","price":78.0},{"key":"normal","price":68.0}],
        "stock":{"available":44,"in_stock":true}}"#;

    #[test]
    fn 档位优先普通而非列表第一项() {
        let p = product(PRO_MAX);
        assert_eq!(p.pick_option().unwrap().key, "normal");
        assert_eq!(p.unit_price(), Some(68.0));
    }

    #[test]
    fn 没有普通档位时退到第一项_无档位用标价() {
        let p = product(r#"{"id":1,"price":9.0,"options":[{"key":"x","price":5.0}]}"#);
        assert_eq!(p.pick_option().unwrap().key, "x");
        let p = product(r#"{"id":2,"price":9.0}"#);
        assert!(p.pick_option().is_none());
        assert_eq!(p.unit_price(), Some(9.0));
    }

    #[test]
    fn 库存映射为区_零库存区不计入可提() {
        let zero = r#"{"id":406,"display_name":"Power","options":[{"key":"normal","price":203.0}],
            "stock":{"available":0,"in_stock":true}}"#;
        let s = products_to_stock(&[product(PRO_MAX), product(zero)]);
        assert_eq!(s.available, 44);
        assert_eq!(s.zones.len(), 2);
        assert_eq!(s.price_min, Some(68.0));
        // 价格区间只看有货的商品，零库存的 203 不该把上限抬上去
        assert_eq!(s.price_max, Some(68.0));
        assert_eq!(s.pick_zone().unwrap().zone, "405");
    }

    #[test]
    fn in_stock为false时库存按零算() {
        let p = product(r#"{"id":1,"stock":{"available":9,"in_stock":false}}"#);
        assert_eq!(p.available(), 0);
    }

    #[test]
    fn 错误文案兼容多种形态() {
        assert_eq!(error_message(r#"{"error":"库存不足"}"#).unwrap(), "库存不足");
        assert_eq!(
            error_message(r#"{"error":{"message":"限购"}}"#).unwrap(),
            "限购"
        );
        assert_eq!(error_message(r#"{"message":"m"}"#).unwrap(), "m");
        assert!(error_message("<html>").is_none());
    }

    #[test]
    fn 取号响应抽出交付内容() {
        let v: Value = serde_json::from_str(
            r#"{"data":{"delivery":{"type":"card","items":[{"content":" {\"a\":1} "},{"content":""}]},
                "pickup":{"already":true}}}"#,
        )
        .unwrap();
        assert_eq!(delivery_contents(&v), vec![r#"{"a":1}"#.to_string()]);
    }

    #[test]
    fn 订单列表剔除充值单_未取号不算成交() {
        let rows: Vec<OrderRow> = serde_json::from_str(
            r#"[{"id":"a","product_id":405,"quantity":1,"amount":68.0,"status":"picked_up"},
                {"id":"topup-x","product_id":0,"quantity":1,"amount":68.0,"status":"picked_up"},
                {"id":"b","product_id":405,"quantity":2,"amount":136.0,"status":"paid"}]"#,
        )
        .unwrap();
        let p = orders_to_paged(rows, None);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].purchased, Some(1));
        assert_eq!(p.items[1].purchased, Some(0));
    }
}
