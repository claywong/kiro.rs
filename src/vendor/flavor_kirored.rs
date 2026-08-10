//! 第六家卖家 kiro.red 对接（flavor = `kirored`）
//!
//! 与前五家协议**根本不同**，故整套请求管线自成一体，不复用
//! [`super::client::VendorClient`] 的 `auth` / `parse` / `post_json`：
//!
//! - **鉴权**：email + 密码登录换 JWT，7 天过期，进程级缓存（见 [`token_cache`]）。
//! - **请求签名**：每个请求带 `X-Signature` —— 对 `{url,method,timestamp,localTimestamp}`
//!   的 JSON 做 base64 后**双重 MD5**（[`sign_request`]）。`url` 必须是**完整路径**
//!   （含 `/api` 前缀），用相对路径会被判「签名校验异常」。
//! - **响应加密**：响应头 `X-Signature-Status: 1` 时，body 是 base64 的 AES-128-CBC
//!   密文，key / iv 由请求签名再派生（[`decrypt_response`]）。
//! - **发货**：无 webhook。下单即发货，卡密在 `GET /user/order/detail` 的
//!   `cards[].content` 里（形如 `ksk_xxx----region` 或 `ksk_xxx----账号----密码----region----url`）。
//! - **下单模型**：商品（SKU + 积分）。先拉 `products` 选品（[`pick_product`]），
//!   再 `POST /user/order/create {sku_id,quantity}`。
//!
//! 逆向依据来自站点前端 `router.js` 的请求拦截器与响应解密函数。
//!
//! @author wangzhong

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use md5::{Digest, Md5};
use serde::Deserialize;

use super::protocol::{
    OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey, StockInfo, VendorApiError,
    ZoneStock,
};

/// AES-128-CBC 解密器类型别名（RustCrypto）。
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

// ============ 路径常量 ============

/// 登录换 token
pub const PATH_LOGIN: &str = "/api/user/auth/login";
/// 商品列表（POST，body `{}`）
pub const PATH_PRODUCTS: &str = "/api/common/products";
/// 账户信息
pub const PATH_USER_INFO: &str = "/api/user/user/info";
/// 下单（POST `{sku_id,quantity}`）
pub const PATH_ORDER_CREATE: &str = "/api/user/order/create";
/// 订单详情（POST `{id}`），卡密在这里
pub const PATH_ORDER_DETAIL: &str = "/api/user/order/detail";
/// 历史订单列表（POST `{page,page_size}`）
pub const PATH_ORDER_INDEX: &str = "/api/user/order/index";

/// 卡密里各字段的分隔符
const CARD_SEP: &str = "----";

// ============ 签名与解密（纯函数，可离线单测） ============

fn md5_hex(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    let mut out = String::with_capacity(32);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 当前 Unix 秒。
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 计算某个请求的签名。
///
/// 逻辑严格对齐前端：`JSON.stringify({url,method,timestamp,localTimestamp})`
/// → base64 → MD5 → 再 MD5。字段顺序与紧凑格式（无空格）都必须一致，
/// 差一个空格签名就变，故这里手工拼 JSON 而不用 serde（serde 会按结构体
/// 字段序，但空格与转义细节不受我们控制，手拼最稳）。
///
/// `full_path` 必须是含 `/api` 前缀的完整路径。
pub fn sign_request(full_path: &str, method: &str, ts: i64) -> String {
    let payload = format!(
        r#"{{"url":"{}","method":"{}","timestamp":{},"localTimestamp":{}}}"#,
        full_path,
        method.to_uppercase(),
        ts,
        ts
    );
    let b64 = base64_encode(payload.as_bytes());
    let first = md5_hex(b64.as_bytes());
    md5_hex(first.as_bytes())
}

/// base64 标准编码（带 padding），与前端 `btoa` 一致。
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(s: &str) -> Result<Vec<u8>, VendorApiError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| VendorApiError {
            status: None,
            message: format!("响应密文 base64 解码失败: {e}"),
        })
}

/// 解密响应体。key / iv 均由请求签名派生：
/// `iv = md5(signature)[..16]`，`key = md5(iv)[..16]`，两者按 **UTF-8 字节**
/// 直接当 16 字节密钥（不是 hex 解码）。AES-128-CBC + PKCS7。
pub fn decrypt_response(cipher_b64: &str, signature: &str) -> Result<String, VendorApiError> {
    let iv_hex = md5_hex(signature.as_bytes());
    let iv = &iv_hex.as_bytes()[..16];
    let key_hex = md5_hex(iv);
    let key = &key_hex.as_bytes()[..16];

    let mut buf = base64_decode(cipher_b64)?;
    let dec = Aes128CbcDec::new(key.into(), iv.into());
    let plain = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| VendorApiError {
            status: None,
            message: format!("响应解密失败（可能签名不匹配）: {e}"),
        })?;
    String::from_utf8(plain.to_vec()).map_err(|e| VendorApiError {
        status: None,
        message: format!("解密结果非 UTF-8: {e}"),
    })
}

// ============ 进程级 token 缓存 ============

/// 登录态缓存条目
#[derive(Clone)]
struct CachedToken {
    token: String,
    /// 过期时刻（Unix 秒）。留 5 分钟余量提前失效，避免边界上用到刚过期的 token。
    expire_at: i64,
}

/// 缓存 key 用 `base_url\nemail`，同一进程内多家 kirored（理论上）互不干扰。
fn token_cache() -> &'static Mutex<HashMap<String, CachedToken>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(base_url: &str, email: &str) -> String {
    format!("{base_url}\n{email}")
}

/// 取缓存里仍然有效的 token（未过期）。
fn cached_valid_token(base_url: &str, email: &str) -> Option<String> {
    let guard = token_cache().lock().ok()?;
    let entry = guard.get(&cache_key(base_url, email))?;
    if entry.expire_at > now_secs() {
        Some(entry.token.clone())
    } else {
        None
    }
}

/// 写入 token 缓存。`expires_in` 为卖家给的有效期（秒）。
fn store_token(base_url: &str, email: &str, token: String, expires_in: i64) {
    if let Ok(mut guard) = token_cache().lock() {
        // 留 5 分钟余量
        let expire_at = now_secs() + expires_in.max(0) - 300;
        guard.insert(
            cache_key(base_url, email),
            CachedToken { token, expire_at },
        );
    }
}

/// 清掉某账号的缓存（token 被判无效时调用，下次重新登录）。
fn invalidate_token(base_url: &str, email: &str) {
    if let Ok(mut guard) = token_cache().lock() {
        guard.remove(&cache_key(base_url, email));
    }
}

// ============ DTO（卖家响应形态 → 中立结构） ============

/// 统一响应信封：`{code, data, message}`。`code == 0` 为成功。
#[derive(Debug, Deserialize)]
struct Envelope<T: Default> {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    data: Option<T>,
    #[serde(default)]
    message: Option<String>,
}

impl<T: Default> Envelope<T> {
    /// 取出 data，非成功码转为带原始 message 的错误。
    fn into_data(self, what: &str) -> Result<T, VendorApiError> {
        if self.code == 0 {
            self.data.ok_or_else(|| VendorApiError {
                status: None,
                message: format!("{what}：响应 code=0 但缺 data"),
            })
        } else {
            Err(VendorApiError {
                status: None,
                message: format!(
                    "{what}失败（code={}）: {}",
                    self.code,
                    self.message.unwrap_or_default()
                ),
            })
        }
    }
}

/// 登录返回
#[derive(Debug, Deserialize, Default)]
struct LoginData {
    #[serde(default)]
    token: String,
    /// token 有效期（秒）
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    user: Option<UserData>,
}

/// 账户信息
#[derive(Debug, Deserialize, Default)]
struct UserData {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    points: Option<f64>,
    #[serde(default)]
    total_points_used: Option<f64>,
}

/// `/api/user/user/info` 的 `data` —— 档案嵌在 `user` 键下，与登录响应同构。
///
/// 少这一层会让 `UserData` 的字段全部落空成 `None`，症状是面板余额空白而
/// 接口不报错（`code=0`，只是解不出字段），故单独建型而非直接解 [`UserData`]。
#[derive(Debug, Deserialize, Default)]
struct UserInfoData {
    #[serde(default)]
    user: Option<UserData>,
}

/// 商品列表信封 `{list:[...], total}`
#[derive(Debug, Deserialize, Default)]
struct ProductList {
    #[serde(default)]
    list: Vec<Product>,
}

/// 单个商品。只解我们选品与展示要用的字段。
#[derive(Debug, Deserialize, Default, Clone)]
pub struct Product {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub sku_id: Option<i64>,
    #[serde(default)]
    pub name: String,
    /// 最低积分单价
    #[serde(default)]
    pub min_point_price: Option<f64>,
    #[serde(default)]
    pub point_price: Option<f64>,
    /// 是否可下单（缺货时为 false）
    #[serde(default)]
    pub purchasable: Option<bool>,
    #[serde(default)]
    pub in_stock: Option<bool>,
    /// SKU 库存数
    #[serde(default)]
    pub sku_stock: Option<u32>,
    /// 当前可提取数（综合库存、余额等的结果）
    #[serde(default)]
    pub available: Option<u32>,
    /// 最新批次，健康度看这里
    #[serde(default)]
    pub latest_batch: Option<LatestBatch>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct LatestBatch {
    /// `good` / `dead` / `none`
    #[serde(default)]
    pub health: String,
    /// 发车时间（Unix 秒）—— 本批次入库的时刻
    #[serde(default)]
    pub import_time: Option<i64>,
    /// 存活秒数。**活车是「已存活多久」、死车是最终存活时长**，见
    /// [`ZoneStock::alive_secs`](super::protocol::ZoneStock::alive_secs)。
    #[serde(default)]
    pub max_alive_seconds: Option<i64>,
    /// 卖家给的存活时长文案，如「26 分钟 46 秒」
    #[serde(default)]
    pub max_alive_text: Option<String>,
    /// 死亡时间（Unix 秒）。活车为 `0`，故取用前需判非零。
    #[serde(default)]
    pub dead_time: Option<i64>,
}

impl Product {
    /// 本商品当前是否健康（最新批次 health == good）。
    pub fn is_healthy(&self) -> bool {
        self.latest_batch
            .as_ref()
            .map(|b| b.health.eq_ignore_ascii_case("good"))
            .unwrap_or(false)
    }

    /// 本商品是否有实际库存可下单。
    ///
    /// 综合多个字段判断：`purchasable` 为 true，或 `available` / `sku_stock` 大于 0。
    /// 早期实现因抓包时看到活车的 `purchasable` 为 false（疑似库存快照滞后）而故意
    /// 忽略此标志，但实测该标志**与实际库存一致**（都为 0 时下单会失败），
    /// 故现在恢复为必要条件。
    pub fn has_stock(&self) -> bool {
        self.purchasable.unwrap_or(false)
            || self.available.unwrap_or(0) > 0
            || self.sku_stock.unwrap_or(0) > 0
    }

    /// 选品排序用的积分价：优先 point_price，回退 min_point_price。
    fn price(&self) -> f64 {
        self.point_price
            .or(self.min_point_price)
            .unwrap_or(f64::INFINITY)
    }
}

/// 下单返回。字段不完全确定，故只解订单号相关字段，卡密统一走订单详情再拉。
#[derive(Debug, Deserialize, Default)]
struct CreateOrderData {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    order_no: Option<String>,
}

/// 历史订单列表信封 `{list, total, page, page_size, counters}`。
///
/// 两处用它：下单响应缺数字 `id` 时按 `order_no` 反查（详情接口只认数字 `id`），
/// 以及面板的历史订单对账（[`orders_to_paged`]）。
#[derive(Debug, Deserialize, Default)]
pub struct OrderIndexData {
    #[serde(default)]
    pub list: Vec<OrderIndexItem>,
    #[serde(default)]
    pub total: Option<u32>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub page_size: Option<u32>,
}

/// 历史订单一行。实测样本见 [`orders_to_paged`] 的单测。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OrderIndexItem {
    /// 数字自增 id（字符串形态，如 `"612"`）—— 详情接口要的就是它
    #[serde(default)]
    pub id: Option<String>,
    /// 24 位业务单号，如 `202608102107130000960141`
    #[serde(default)]
    pub order_no: Option<String>,
    /// 下单方自带的幂等键。我们下单不传，故本家实测恒为 null。
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// 本单消耗积分（**未扣退款**，净支出要减 `refund_points`）
    #[serde(default)]
    pub point_cost: Option<f64>,
    /// 已退还积分。整单退款时等于 `point_cost`。
    #[serde(default)]
    pub refund_points: Option<f64>,
    /// 发货状态，1 已发货
    #[serde(default)]
    pub deliver_status: Option<i64>,
    /// 下单时刻（unix 秒）
    #[serde(default)]
    pub create_time: Option<i64>,
    /// 明细行。要的数量在这里，顶层 `item_count` 是**明细行数**而非件数。
    #[serde(default)]
    pub items: Vec<OrderIndexLine>,
}

/// 历史订单的明细行
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OrderIndexLine {
    /// 本行件数
    #[serde(default)]
    pub quantity: Option<u32>,
    /// 本行发货状态，1 已发货
    #[serde(default)]
    pub deliver_status: Option<i64>,
}

impl From<OrderIndexItem> for OrderInfo {
    fn from(o: OrderIndexItem) -> Self {
        // 件数在明细行里。顶层 `item_count` 是明细行数（单商品单行时恒为 1），
        // 拿它当件数会把「一单提 3 张」记成 1 张。
        let requested: u32 = o.items.iter().filter_map(|l| l.quantity).sum();
        // 已发货件数：整单已发货时全算，否则只算发了货的行。
        // 卖家对未发货单不会给卡密，此时算 0 才与本地入库数对得上。
        let delivered_all = o.deliver_status == Some(1);
        let purchased: u32 = o
            .items
            .iter()
            .filter(|l| delivered_all || l.deliver_status == Some(1))
            .filter_map(|l| l.quantity)
            .sum();
        // 净支出 = 消耗 − 已退。整单退款后这里是 0，不能只报 point_cost，
        // 否则对账时把退掉的钱算成了花掉的。
        let total_debit = o.point_cost.map(|c| {
            let net = c - o.refund_points.unwrap_or(0.0);
            if net < 0.0 { 0.0 } else { net }
        });
        Self {
            // client_order_id 原样透出（API 下单时我们传的幂等键，网页下单时为 null）
            client_order_id: o.client_order_id.clone().filter(|s| !s.trim().is_empty()),
            // order_id 优先用卖家的业务单号，回退幂等键 —— API 下单时卖家不给
            // order_no 只给 client_order_id，此时用后者才能与本地 purchase_events
            // 表的 order_id 列对上（那里也是优先 order_no、回退 client_order_id）
            order_id: o
                .order_no
                .or(o.client_order_id)
                .filter(|s| !s.trim().is_empty()),
            requested: Some(requested),
            purchased: Some(purchased),
            total_debit,
            created_at: o.create_time.filter(|t| *t > 0).and_then(ts_to_rfc3339),
        }
    }
}

/// unix 秒转 RFC3339 字符串，与 kiroapp / kiromarket 给的字符串时间对齐。
fn ts_to_rfc3339(ts: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0).map(|d| d.to_rfc3339())
}

/// 把历史订单信封折成中立分页。分页字段缺失时按卖家实际返回条数兜底。
pub fn orders_to_paged(data: OrderIndexData) -> Paged<OrderInfo> {
    let count = data.list.len() as u32;
    let page_size = data.page_size.filter(|n| *n > 0);
    let total = data.total;
    // 总页数由 total / page_size 推；缺任一则不给（前端按 None 隐藏页码）
    let pages = match (total, page_size) {
        (Some(t), Some(ps)) => Some(t.div_ceil(ps)),
        _ => None,
    };
    Paged {
        items: data.list.into_iter().map(OrderInfo::from).collect(),
        total: total.or(Some(count)),
        page: data.page.or(Some(1)),
        page_size: page_size.or(Some(count.max(1))),
        pages,
    }
}

/// 订单详情信封 `{item, items, ...}`
#[derive(Debug, Deserialize, Default)]
struct OrderDetailData {
    #[serde(default)]
    items: Vec<OrderDetailItem>,
}

#[derive(Debug, Deserialize, Default)]
struct OrderDetailItem {
    #[serde(default)]
    cards: Vec<OrderCard>,
}

#[derive(Debug, Deserialize, Default)]
struct OrderCard {
    /// 完整卡密串，形如 `ksk_xxx----region` 或 `ksk_xxx----账号----密码----region----url`
    #[serde(default)]
    content: String,
    #[serde(default)]
    account: Option<String>,
}

/// 把一条卡密串拆成中立 [`PurchasedKey`]。
///
/// 约定分隔符 `----`：第一段是 `ksk_` 开头的 API Key，本地入库只认它；
/// 其余段（账号 / 密码 / region / issuer_url）尽量识别后填进展示字段。
fn parse_card(card: &OrderCard) -> Option<PurchasedKey> {
    let content = card.content.trim();
    if content.is_empty() {
        return None;
    }
    let parts: Vec<&str> = content.split(CARD_SEP).map(str::trim).collect();
    let key = parts.first().copied().unwrap_or("").to_string();
    if key.is_empty() {
        return None;
    }
    // 识别 issuer_url（以 http 开头的段）
    let issuer_url = parts
        .iter()
        .find(|p| p.starts_with("http"))
        .map(|s| s.to_string());
    // 账号：优先用卖家给的独立 account 字段，否则取第二段（若不是 region/url）
    let account = card.account.clone().or_else(|| {
        parts.get(1).and_then(|p| {
            let p = *p;
            if p.starts_with("http") || looks_like_region(p) {
                None
            } else {
                Some(p.to_string())
            }
        })
    });
    // 区域：任一段形如 AWS 区域标识即取之。这家「双区混发」商品同一单里
    // 各张卡的区不同，必须逐张记下来 —— 订单级 zone（商品 id）表达不了。
    let region = parts
        .iter()
        .skip(1)
        .find(|p| looks_like_region(p))
        .copied()
        .map(|s| s.to_ascii_lowercase());
    Some(PurchasedKey {
        key,
        account,
        password: None,
        issuer_url,
        price: None,
        region,
    })
}

/// 粗略判断某段是否是区域码（如 `us-east-1` / `eu-central-1`）。
fn looks_like_region(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    (s.starts_with("us") || s.starts_with("eu") || s.starts_with("ap"))
        && s.contains('-')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

// ============ 选品 ============

/// 从商品列表选一个可下单的：**健康且有库存、积分最低**者。
///
/// - 必须同时满足 `health == good` 和有实际库存（`purchasable` 或 `available > 0`）。
///   只有批次活着但库存已耗尽的商品会被过滤掉。
/// - 同为可订购时取积分价最低者，价同则按 id 字典序保证结果稳定（重试幂等）。
/// - 无任何可订购商品时返回 None，调用方据此报「当前无车可提」。
pub fn pick_product(products: &[Product]) -> Option<&Product> {
    products
        .iter()
        .filter(|p| p.is_healthy() && p.has_stock() && p.sku_id.is_some())
        .min_by(|a, b| {
            a.price()
                .total_cmp(&b.price())
                .then_with(|| a.id.cmp(&b.id))
        })
}

// ============ 请求管线 ============

/// kiro.red 出站客户端。与 [`super::client::VendorClient`] 相互独立。
pub struct KiroredClient {
    http: reqwest::Client,
    base_url: String,
    email: String,
    password: String,
}

impl KiroredClient {
    pub fn new(http: reqwest::Client, base_url: String, email: String, password: String) -> Self {
        Self {
            http,
            base_url,
            email,
            password,
        }
    }

    fn url(&self, full_path: &str) -> String {
        format!("{}{}", self.base_url, full_path)
    }

    /// 发一个带签名的 POST，自动解密响应并解析成信封。
    ///
    /// `auth_token` 为 None 时不带登录头（登录接口本身用）。返回 `(Envelope, ())`。
    async fn signed_post<T: for<'de> Deserialize<'de> + Default>(
        &self,
        full_path: &str,
        body: &serde_json::Value,
        auth_token: Option<&str>,
    ) -> Result<Envelope<T>, VendorApiError> {
        let ts = now_secs();
        let sig = sign_request(full_path, "POST", ts);
        let mut req = self
            .http
            .post(self.url(full_path))
            .header("X-Signature", &sig)
            .header("X-Timestamp", ts.to_string())
            .header("X-localTimestamp", ts.to_string())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body);
        if let Some(tok) = auth_token {
            req = req
                .header("X-Token", tok)
                .header("Authorization", format!("Bearer {tok}"));
        }
        let resp = req.send().await.map_err(|e| VendorApiError {
            status: None,
            message: e.to_string(),
        })?;

        let status = resp.status();
        // 响应头决定 body 是否加密：X-Signature-Status == 1 时是 AES 密文
        let encrypted = resp
            .headers()
            .get("x-signature-status")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
        let raw = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;

        let plaintext = if encrypted {
            // 密文外层是 JSON 字符串字面量（带引号），先剥掉引号再解密
            let cipher = raw.trim().trim_matches('"');
            decrypt_response(cipher, &sig)?
        } else {
            raw
        };

        serde_json::from_str::<Envelope<T>>(&plaintext).map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!(
                "解析响应失败: {e}；原文片段: {}",
                super::protocol::truncate(&plaintext, 200)
            ),
        })
    }

    /// 确保有可用 token：命中缓存直接返回，否则登录换取并写缓存。
    async fn ensure_token(&self) -> Result<String, VendorApiError> {
        if let Some(tok) = cached_valid_token(&self.base_url, &self.email) {
            return Ok(tok);
        }
        let body = serde_json::json!({
            "email": self.email,
            "password": self.password,
        });
        let env: Envelope<LoginData> = self.signed_post(PATH_LOGIN, &body, None).await?;
        let data = env.into_data("登录")?;
        if data.token.is_empty() {
            return Err(VendorApiError {
                status: None,
                message: "登录成功但未返回 token".to_string(),
            });
        }
        // expires_in 缺失时按 1 小时保守缓存
        let ttl = if data.expires_in > 0 {
            data.expires_in
        } else {
            3600
        };
        store_token(&self.base_url, &self.email, data.token.clone(), ttl);
        Ok(data.token)
    }

    /// 带 token 的 POST，遇到疑似鉴权失效时清缓存重登一次。
    async fn authed_post<T: for<'de> Deserialize<'de> + Default>(
        &self,
        full_path: &str,
        body: &serde_json::Value,
    ) -> Result<Envelope<T>, VendorApiError> {
        let token = self.ensure_token().await?;
        let env = self.signed_post::<T>(full_path, body, Some(&token)).await;
        // code 401 或 message 含「登录/未授权」时，清缓存重登重试一次
        if let Ok(ref e) = env {
            if e.code == 401 {
                invalidate_token(&self.base_url, &self.email);
                let token = self.ensure_token().await?;
                return self.signed_post::<T>(full_path, body, Some(&token)).await;
            }
        }
        env
    }

    // ---------- 对上层暴露的业务方法（返回中立结构） ----------

    /// 拉商品列表。
    async fn fetch_products(&self) -> Result<Vec<Product>, VendorApiError> {
        let env: Envelope<ProductList> = self
            .authed_post(PATH_PRODUCTS, &serde_json::json!({}))
            .await?;
        Ok(env.into_data("拉取商品列表")?.list)
    }

    /// 库存与报价：把商品列表折叠成中立 [`StockInfo`]。
    ///
    /// kiro.red 是商品制，没有「可提取张数」的概念。这里把**可订购商品数**
    /// （健康且有库存）当作 `available`（>0 表示当前有车可买），并把每个健康商品
    /// 作为一个 `zone` 透出（zone=商品 id、label=名称、unit_price=积分价），
    /// 供面板展示车次。缺货商品的 `zone.available` 为 0，面板会显示但标灰。
    pub async fn stock(&self) -> Result<StockInfo, VendorApiError> {
        let products = self.fetch_products().await?;
        let healthy: Vec<&Product> = products.iter().filter(|p| p.is_healthy()).collect();
        let zones: Vec<ZoneStock> = healthy
            .iter()
            .map(|p| {
                let batch = p.latest_batch.as_ref();
                let has_stock = p.has_stock();
                ZoneStock {
                    zone: p.id.clone(),
                    label: Some(p.name.clone()),
                    // 有库存时为 1（可下单），否则为 0（面板显示但标灰）
                    available: if has_stock { 1 } else { 0 },
                    // 透出卖家的实际库存数
                    stock: p.sku_stock.or(p.available),
                    unit_price: p.point_price.or(p.min_point_price),
                    enabled: true,
                    // 0 是卖家表示「无」的写法，别让前端显示 1970 年
                    departed_at: batch.and_then(|b| b.import_time).filter(|t| *t > 0),
                    alive_secs: batch.and_then(|b| b.max_alive_seconds),
                    alive_text: batch.and_then(|b| b.max_alive_text.clone()),
                }
            })
            .collect();
        // price_min 只从有库存的商品里算
        let orderable: Vec<&Product> = products.iter().filter(|p| p.is_healthy() && p.has_stock()).collect();
        let price_min = orderable
            .iter()
            .filter_map(|p| p.point_price.or(p.min_point_price))
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.min(v)))
            });
        Ok(StockInfo {
            // available 是有库存可下单的商品数
            available: orderable.len() as u32,
            price_min,
            price_max: None,
            balance: None,
            zones,
        })
    }

    /// 账户档案：余额（积分）、已用积分。
    pub async fn profile(&self) -> Result<ProfileInfo, VendorApiError> {
        let env: Envelope<UserInfoData> = self
            .authed_post(PATH_USER_INFO, &serde_json::json!({}))
            .await?;
        let user = env
            .into_data("获取账户信息")?
            .user
            .unwrap_or_default();
        Ok(ProfileInfo {
            name: user.username,
            email: user.email,
            balance: user.points,
            quota: None,
            used_quota: user.total_points_used,
            min_purchase: Some(1),
            max_purchase: Some(1),
            webhook_url: None,
            created_at: None,
        })
    }

    /// 下单提取。完整流程：选品 → 下单 → 查订单详情拿卡密。
    ///
    /// `count` 对本家意义有限：商品是按件买的，这里把它当作购买数量传给卖家，
    /// 但绝大多数拼车商品 `batch_limit == 1`，故实际多为 1。
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<PurchaseResult, VendorApiError> {
        // 1. 选品
        let products = self.fetch_products().await?;
        let chosen = pick_product(&products).ok_or_else(|| VendorApiError {
            status: None,
            message: "当前无车可提（所有商品均已缺货或批次已失效）".to_string(),
        })?;
        let sku_id = chosen.sku_id.ok_or_else(|| VendorApiError {
            status: None,
            message: format!("选中商品 {} 缺 sku_id", chosen.name),
        })?;
        let quantity = count.max(1);
        tracing::info!(
            vendor = "kirored",
            product = %chosen.name,
            sku_id,
            quantity,
            order = %client_order_id,
            "kiro.red 下单"
        );

        // 2. 下单
        let create_body = serde_json::json!({
            "sku_id": sku_id,
            "quantity": quantity,
        });
        let env: Envelope<CreateOrderData> =
            self.authed_post(PATH_ORDER_CREATE, &create_body).await?;
        let order = env.into_data("下单")?;
        // 详情接口**只认数字自增 id**，传 24 位 order_no 会得到「订单不存在」。
        // 下单响应有时只给 order_no，此时按单号去历史列表反查数字 id。
        let numeric_id = order
            .id
            .as_ref()
            .map(value_to_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) && s.len() < 12);
        let order_no = order.order_no.clone().filter(|s| !s.trim().is_empty());
        let detail_id = match (numeric_id, &order_no) {
            (Some(id), _) => id,
            (None, Some(no)) => self.resolve_order_id(no).await?,
            (None, None) => {
                return Err(VendorApiError {
                    status: None,
                    message: "下单成功但未返回订单号，无法拉取卡密".to_string(),
                })
            }
        };

        // 3. 查订单详情拿卡密（不依赖下单响应体结构）
        //    到这一步积分已经扣掉、订单已成立，取卡密失败**不代表下单失败**。
        //    错误里必须点明这一点，否则运维看到「订单不存在」会以为没买成而重复下单。
        let keys = self
            .fetch_order_keys(&detail_id)
            .await
            .map_err(|e| VendorApiError {
                status: e.status,
                message: format!(
                    "下单已成功（订单 {}，积分已扣），但取卡密失败：{}；请到卖家后台查看卡密，不要重复下单",
                    order_no.as_deref().unwrap_or(&detail_id),
                    e.message
                ),
            })?;
        // 对外展示优先用业务单号，便于与卖家后台核对
        let order_id = order_no.unwrap_or(detail_id);
        Ok(PurchaseResult {
            purchased: keys.len() as u32,
            requested: Some(quantity),
            remaining: None,
            unit_price: chosen.point_price.or(chosen.min_point_price),
            total_debit: chosen
                .point_price
                .or(chosen.min_point_price)
                .map(|p| p * keys.len() as f64),
            order_id: Some(order_id),
            keys,
            replayed: false,
            zone: Some(chosen.id.clone()),
        })
    }

    /// 历史提取订单，供面板与本地事件对账。
    ///
    /// 分页参数与卖家一致（`page` 从 1 起算）。这家的列表接口是 POST + 签名，
    /// 不能复用 [`VendorClient::get_with`](super::client::VendorClient)。
    pub async fn purchase_orders(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Paged<OrderInfo>, VendorApiError> {
        let body = serde_json::json!({
            "page": page.unwrap_or(1).max(1),
            "page_size": page_size.unwrap_or(50).clamp(1, 100),
        });
        let env: Envelope<OrderIndexData> = self.authed_post(PATH_ORDER_INDEX, &body).await?;
        Ok(orders_to_paged(env.into_data("查询历史订单")?))
    }

    /// 按 24 位业务单号反查详情接口要的数字自增 id。
    ///
    /// 详情接口只接受数字 `id`（传 `order_no` 返回 code=1「订单不存在」，传
    /// `{order_no:...}` 返回「请求参数异常」），而下单响应有时只给 `order_no`，
    /// 故这里翻第一页历史订单按单号匹配。刚下的单必然在首页。
    async fn resolve_order_id(&self, order_no: &str) -> Result<String, VendorApiError> {
        let body = serde_json::json!({ "page": 1, "page_size": 20 });
        let env: Envelope<OrderIndexData> = self.authed_post(PATH_ORDER_INDEX, &body).await?;
        let list = env.into_data("查询历史订单")?.list;
        list.iter()
            .find(|o| o.order_no.as_deref().map(str::trim) == Some(order_no.trim()))
            .and_then(|o| o.id.clone())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| VendorApiError {
                status: None,
                message: format!(
                    "下单成功（单号 {order_no}）但在历史订单首页未找到该单，无法定位卡密；\
                     请到卖家后台核对，不要重复下单"
                ),
            })
    }

    /// 按数字自增 id 拉卡密（订单详情的 `items[].cards[]`）。
    ///
    /// `order_id` 必须是数字自增 id，不能是业务单号 —— 见 [`Self::resolve_order_id`]。
    async fn fetch_order_keys(&self, order_id: &str) -> Result<Vec<PurchasedKey>, VendorApiError> {
        // 详情接口的 id 可传数字或数字字符串，两者都接受
        let id_value: serde_json::Value = order_id
            .parse::<i64>()
            .map(serde_json::Value::from)
            .unwrap_or_else(|_| serde_json::Value::from(order_id));
        let body = serde_json::json!({ "id": id_value });
        let env: Envelope<OrderDetailData> = self.authed_post(PATH_ORDER_DETAIL, &body).await?;
        let detail = env.into_data("查询订单详情")?;
        let keys: Vec<PurchasedKey> = detail
            .items
            .iter()
            .flat_map(|it| it.cards.iter())
            .filter_map(parse_card)
            .collect();
        if keys.is_empty() {
            return Err(VendorApiError {
                status: None,
                message: format!("订单 {order_id} 详情里没有可用卡密（可能尚未发货）"),
            });
        }
        Ok(keys)
    }
}

/// JSON 值转字符串：字符串取原值，数字取其字面。
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string().trim_matches('"').to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 签名向量由前端算法离线复算得到（见开发记录），锁死实现不跑偏。
    #[test]
    fn 签名双重md5与前端一致() {
        let sig = sign_request("/api/common/products", "POST", 1700000000);
        assert_eq!(sig, "fa5f1601aa8d0f3a1dcde695149afa90");
    }

    /// method 大小写无关（前端 toUpperCase）。
    #[test]
    fn 签名method大写归一() {
        let a = sign_request("/api/common/products", "post", 1700000000);
        let b = sign_request("/api/common/products", "POST", 1700000000);
        assert_eq!(a, b);
    }

    /// 用已知 key/iv 加密的密文能正确解回明文。
    #[test]
    fn 解密还原明文() {
        let sig = "fa5f1601aa8d0f3a1dcde695149afa90";
        let cipher = "2780rUbZwrqSOSI1bfVryOHCHDW8sxqxbODyvBVHvuA=";
        let plain = decrypt_response(cipher, sig).unwrap();
        assert_eq!(plain, r#"{"code":0,"msg":"ok"}"#);
    }

    /// 签名不匹配时解密报错，而不是返回垃圾。
    #[test]
    fn 解密错误签名报错() {
        let cipher = "2780rUbZwrqSOSI1bfVryOHCHDW8sxqxbODyvBVHvuA=";
        assert!(decrypt_response(cipher, "deadbeef").is_err());
    }

    /// 造一个**有库存**的商品。`purchasable` 留 false 是刻意的 —— 抓包里活车的
    /// 这个字段常为 false（疑似库存快照滞后），有货靠 `available` 体现，正是
    /// [`Product::has_stock`] 要覆盖的形态。缺货场景请显式把 `available` 改成 0。
    fn product(id: &str, sku: Option<i64>, price: f64, health: &str) -> Product {
        Product {
            id: id.to_string(),
            sku_id: sku,
            name: format!("商品{id}"),
            min_point_price: Some(price),
            point_price: Some(price),
            purchasable: Some(false),
            in_stock: Some(false),
            sku_stock: None,
            available: Some(1),
            latest_batch: Some(LatestBatch {
                health: health.to_string(),
                ..Default::default()
            }),
        }
    }

    /// 只选健康商品，且取积分最低者。
    #[test]
    fn 选品取健康且最便宜() {
        let products = vec![
            product("1", Some(1), 12.0, "dead"),
            product("55", Some(58), 10.0, "good"),
            product("57", Some(60), 12.0, "good"),
        ];
        let chosen = pick_product(&products).expect("应选出健康商品");
        assert_eq!(chosen.id, "55", "健康里最便宜的是 55");
    }

    /// 全不健康时返回 None（不瞎买）。
    #[test]
    fn 无健康车返回none() {
        let products = vec![
            product("1", Some(1), 12.0, "dead"),
            product("2", Some(2), 10.0, "none"),
        ];
        assert!(pick_product(&products).is_none());
    }

    /// 健康但缺 sku_id 的商品不能选（无法下单）。
    #[test]
    fn 缺skuid不选() {
        let products = vec![product("9", None, 5.0, "good")];
        assert!(pick_product(&products).is_none());
    }

    /// 纯 APIKEY 卡密：`ksk_xxx----region`，只入 key，region 识别为非账号。
    #[test]
    fn 解析纯apikey卡密() {
        let card = OrderCard {
            content: "ksk_abc123----us-east-1".to_string(),
            account: None,
        };
        let pk = parse_card(&card).unwrap();
        assert_eq!(pk.key, "ksk_abc123");
        assert!(pk.account.is_none(), "region 段不应被当成账号");
        assert!(pk.issuer_url.is_none());
        assert_eq!(
            pk.region.as_deref(),
            Some("us-east-1"),
            "region 段必须留下来，否则入库拿不到区、凭证会连错端点"
        );
    }

    /// 双区混发：同一单里两张卡分属不同区，各自的区必须独立带出来。
    /// 订单级 zone 是商品 id，表达不了这个差异 —— 这条锁住那个坑。
    #[test]
    fn 同单双区各自带区() {
        let a = parse_card(&OrderCard {
            content: "ksk_a----us-east-1".to_string(),
            account: None,
        })
        .unwrap();
        let b = parse_card(&OrderCard {
            content: "ksk_b----eu-central-1".to_string(),
            account: None,
        })
        .unwrap();
        assert_eq!(a.region.as_deref(), Some("us-east-1"));
        assert_eq!(b.region.as_deref(), Some("eu-central-1"));
    }

    /// 账密+key 卡密：`ksk_xxx----账号----密码----region----url`。
    #[test]
    fn 解析账密卡密() {
        let card = OrderCard {
            content:
                "ksk_k9----thomasjones22008----56cdd9a318!aA1----eu-central-1----https://d-9.awsapps.com/start"
                    .to_string(),
            account: None,
        };
        let pk = parse_card(&card).unwrap();
        assert_eq!(pk.key, "ksk_k9");
        assert_eq!(pk.account.as_deref(), Some("thomasjones22008"));
        assert_eq!(
            pk.issuer_url.as_deref(),
            Some("https://d-9.awsapps.com/start")
        );
        assert_eq!(pk.region.as_deref(), Some("eu-central-1"));
    }

    // ============ 历史订单 ============

    /// 卖家 `/api/user/order/index` 的真实返回（探针抓取，单号与 IP 已改）。
    const ORDER_INDEX_SAMPLE: &str = r#"{"list":[{"id":"612",
        "order_no":"202608102107130000960141","client_order_id":null,"source":"web",
        "user_id":96,"type":1,"is_reserve":0,"point_cost":15,"pay_status":1,
        "pay_time":1786367233,"deliver_status":1,"deliver_time":1786367233,
        "item_count":1,"contact_email":"","refund_time":0,"refund_points":0,
        "refund_reason":"","user_remark":"","remark":"","ip":"1.2.3.4",
        "create_time":1786367233,"update_time":1786367233,
        "items":[{"order_id":612,"product_id":55,"product_name":"Kiro 拼车 纯APIKEY 双区混发",
        "sku_name":"标准版","quantity":1,"point_price":15,"point_subtotal":15,
        "deliver_status":1}],"product_summary":"Kiro 拼车 纯APIKEY 双区混发",
        "pay_status_text":"已完成","deliver_status_text":"已发货"}],
        "total":7,"page":1,"page_size":5,
        "counters":{"all":7,"unpaid":0,"paid":7,"refunded":0,"canceled":0}}"#;

    /// 真实样本要能解析并映射到中立结构，分页信息按卖家给的走。
    #[test]
    fn 历史订单映射真实样本_网页下单() {
        let data: OrderIndexData =
            serde_json::from_str(ORDER_INDEX_SAMPLE).expect("真实返回应可解析");
        let paged = orders_to_paged(data);
        assert_eq!(paged.total, Some(7));
        assert_eq!(paged.page, Some(1));
        assert_eq!(paged.page_size, Some(5));
        assert_eq!(paged.pages, Some(2), "7 条 / 每页 5 = 2 页");
        let o = &paged.items[0];
        assert_eq!(o.order_id.as_deref(), Some("202608102107130000960141"));
        assert!(
            o.client_order_id.is_none(),
            "网页下单不带幂等键，该字段应为 null"
        );
        assert_eq!(o.requested, Some(1));
        assert_eq!(o.purchased, Some(1));
        assert_eq!(o.total_debit, Some(15.0));
        assert_eq!(o.created_at.as_deref(), Some("2026-08-10T13:07:13+00:00"));
    }

    /// API 下单时卖家不给 order_no、只给 client_order_id（我们的幂等键）。
    /// order_id 要回退到 client_order_id 才能与本地 purchase_events 表对上账。
    #[test]
    fn 历史订单映射_api下单回退幂等键() {
        let raw = r#"{"list":[{"id":"700","order_no":null,
            "client_order_id":"fd8a2e8860b690f0d17f279fde00a975","point_cost":15,
            "deliver_status":1,"create_time":1786386553,
            "items":[{"quantity":1,"deliver_status":1}]}],"total":1}"#;
        let paged = orders_to_paged(serde_json::from_str(raw).unwrap());
        let o = &paged.items[0];
        assert_eq!(
            o.client_order_id.as_deref(),
            Some("fd8a2e8860b690f0d17f279fde00a975")
        );
        // order_id 回退到 client_order_id，否则对不上本地记录
        assert_eq!(
            o.order_id.as_deref(),
            Some("fd8a2e8860b690f0d17f279fde00a975")
        );
    }

    /// 件数取明细行的 quantity 之和，不能用顶层 item_count —— 后者是**行数**，
    /// 一单提 3 张（单行 quantity=3）时它是 1，会把提取量记少。
    #[test]
    fn 件数取明细行数量之和而非行数() {
        let raw = r#"{"list":[{"id":"1","order_no":"n1","point_cost":45,
            "deliver_status":1,"item_count":1,"create_time":1786367233,
            "items":[{"quantity":3,"deliver_status":1}]}],
            "total":1,"page":1,"page_size":20}"#;
        let paged = orders_to_paged(serde_json::from_str(raw).unwrap());
        assert_eq!(paged.items[0].requested, Some(3));
        assert_eq!(paged.items[0].purchased, Some(3));
    }

    /// 未发货的单不该报出货数 —— 卖家此时不给卡密，本地也没入库。
    #[test]
    fn 未发货单出货数为零() {
        let raw = r#"{"list":[{"id":"2","order_no":"n2","point_cost":15,
            "deliver_status":0,"create_time":1786367233,
            "items":[{"quantity":2,"deliver_status":0}]}],"total":1}"#;
        let paged = orders_to_paged(serde_json::from_str(raw).unwrap());
        assert_eq!(paged.items[0].requested, Some(2), "要的还是 2 件");
        assert_eq!(paged.items[0].purchased, Some(0), "但一件都没发");
    }

    /// 退款单的净支出要扣掉已退积分，否则对账时把退回的钱算成花掉的。
    #[test]
    fn 退款单净支出扣掉已退积分() {
        let raw = r#"{"list":[{"id":"3","order_no":"n3","point_cost":15,
            "refund_points":15,"deliver_status":1,"create_time":1786367233,
            "items":[{"quantity":1,"deliver_status":1}]}],"total":1}"#;
        let paged = orders_to_paged(serde_json::from_str(raw).unwrap());
        assert_eq!(paged.items[0].total_debit, Some(0.0));
    }

    /// 分页字段缺失时按实际条数兜底，不能给出 0 页让前端以为没数据。
    #[test]
    fn 分页字段缺失时按条数兜底() {
        let raw = r#"{"list":[{"id":"4","order_no":"n4","items":[{"quantity":1}]}]}"#;
        let paged = orders_to_paged(serde_json::from_str(raw).unwrap());
        assert_eq!(paged.total, Some(1));
        assert_eq!(paged.page, Some(1));
        assert!(paged.pages.is_none(), "推不出总页数时不瞎给");
    }

    /// 空列表是合法状态（新账号没下过单），不该报错。
    #[test]
    fn 历史订单空列表() {
        let paged = orders_to_paged(serde_json::from_str(r#"{"list":[],"total":0}"#).unwrap());
        assert!(paged.items.is_empty());
        assert_eq!(paged.total, Some(0));
    }

    /// 空卡密返回 None。
    #[test]
    fn 空卡密返回none() {
        let card = OrderCard {
            content: "   ".to_string(),
            account: None,
        };
        assert!(parse_card(&card).is_none());
    }

    /// 账户信息的 `data` 外面还套一层 `user`。样例取自卖家真实返回，少这层会让
    /// 余额静默变 `None`（接口 `code=0` 不报错，只是面板空白）。
    #[test]
    fn 账户信息解出嵌套user里的余额() {
        let raw = r#"{"code":0,"data":{"user":{"id":"96",
            "username":"kiro_6e202b50","email":"c@example.com","points":66,
            "total_points_used":34,"order_count":3,"status":1}},"message":"成功"}"#;
        let env: Envelope<UserInfoData> = serde_json::from_str(raw).unwrap();
        let user = env.into_data("获取账户信息").unwrap().user.unwrap();
        assert_eq!(user.points, Some(66.0));
        assert_eq!(user.total_points_used, Some(34.0));
        assert_eq!(user.username.as_deref(), Some("kiro_6e202b50"));
    }

    /// 车次的发车时间与存活时长要透到 zone 上。样例取自卖家真实返回。
    #[test]
    fn 商品解出发车时间与存活时长() {
        let raw = r#"{"id":"55","name":"纯APIKEY 双区混发","sku_id":58,
            "point_price":12,"latest_batch":{"health":"good","import_time":1786337683,
            "max_alive_seconds":1606,"max_alive_text":"26 分钟 46 秒","dead_time":0}}"#;
        let p: Product = serde_json::from_str(raw).unwrap();
        let b = p.latest_batch.as_ref().unwrap();
        assert_eq!(b.import_time, Some(1786337683));
        assert_eq!(b.max_alive_seconds, Some(1606));
        assert_eq!(b.max_alive_text.as_deref(), Some("26 分钟 46 秒"));
        assert!(p.is_healthy());
    }

    /// `latest_batch` 整个缺失时，时间字段退化为 `None` 而非 panic。
    #[test]
    fn 商品缺批次时时间字段为空() {
        let p: Product = serde_json::from_str(r#"{"id":"9","name":"x"}"#).unwrap();
        assert!(p.latest_batch.is_none());
        assert!(!p.is_healthy());
    }

    /// `data` 里没有 `user` 键时不 panic，退化为全 `None`。
    #[test]
    fn 账户信息缺user键时退化为空() {
        let env: Envelope<UserInfoData> =
            serde_json::from_str(r#"{"code":0,"data":{},"message":"成功"}"#).unwrap();
        let user = env.into_data("获取账户信息").unwrap().user.unwrap_or_default();
        assert_eq!(user.points, None);
    }
}
