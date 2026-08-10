//! Kiro Drop（drop.kiro.ss）的协议实现：`/api/my/*` + `X-API-Key: usr-xxx`
//!
//! **按 2026-07-31 的 <https://drop.kiro.ss/docs> 实现。** 该文档在同一天内改过
//! 一次：早先那版走 `/api/v1/reservation`（报价 + 下单，可能返 202 待对账、金额分
//! USD/CNY 两套），现已全部撤掉，改成与首家几乎一致的 `/api/my/*`。若日后又对不上，
//! 先比对文档而不是猜 —— 上一轮就是照着旧文档实现完才发现接口已换。
//!
//! 与 [`super::flavor_legacy`] 的关系：路径、鉴权头、下单参数（`count` +
//! `client_order_id`）、事件名（`new_keys_available` / `all_keys_dead` / `test`）
//! 与幂等语义**全部相同**，故不再重复建模这些，只处理两处真实差异：
//!
//! 1. **金额是字符串**（`"remaining": "884.400000"`），首家给的是数字。legacy 的
//!    DTO 用 `f64`，直接复用会整份解析失败，因此本模块自带 DTO，用 [`Decimalish`]
//!    同时接字符串与数字。另外本家的**单价按美元计价**（早期是人民币），系统其余
//!    部分都是人民币口径，故单价在 DTO 边界乘 [`USD_TO_CNY`]，见 [`amount_cny`]。
//!    **余额类字段（`remaining` / `quota` / `used_quota` / stock 的 `balance`）本身
//!    就是人民币**，不换汇，见 [`amount`]。
//! 2. **库存走 `/api/me/stock`**（注意是 `me` 不是 `my`）。它一次给出库存、单价与
//!    余额；`/api/status` 的 `keys_stock` 只有数量，作为兜底保留。本家的
//!    `/api/my/stock` 实测 404 —— 只有这一个端点落在 `/api/me` 下。
//!
//! 实测另有三个 legacy 端点在本家不存在（均 404），故对应能力关闭：
//! `/api/my/gen-logs`、`/api/my/purchase-orders`、`/api/my/redeem`。
//!
//! @author wangzhong

use serde::Deserialize;

use super::protocol::{ProfileInfo, PurchaseResult, PurchasedKey, StockInfo};

/// 账户信息（余额 + webhook 配置）。与首家同路径，但金额是字符串。
pub const PATH_PROFILE: &str = "/api/my/profile";
/// 下单。参数 `count` + `client_order_id`，与首家一致。
pub const PATH_PURCHASE: &str = "/api/my/purchase";
/// 系统状态。也是库存的兜底来源（`keys_stock`），见 [`PATH_STOCK`]。
pub const PATH_STATUS: &str = "/api/status";
/// 库存与报价。**路径是 `/api/me/stock`**，本家的 `/api/my/stock` 实测 404 ——
/// 只有这一个端点在 `/api/me` 下，其余账号维度接口都在 `/api/my`。
pub const PATH_STOCK: &str = "/api/me/stock";
pub const PATH_WEBHOOK: &str = "/api/my/webhook";
pub const PATH_WEBHOOK_TEST: &str = "/api/my/webhook/test";

/// 金额兼容层：本家用字符串传金额（`"884.400000"`），但数字形态也接 ——
/// 对方哪天改成数字不必回来改代码。解析不了当缺失，不让一个金额字段
/// 拖垮整份响应（余额显示不出来是小事，profile 整体解析失败是大事）。
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

/// 把可选的金额字段转成 f64（**原样，未换汇**）。
///
/// **余额类字段用这个**：本家的 `remaining` / `quota` / `used_quota` /
/// stock 的 `balance` 本身就是人民币，换汇会把余额虚增 7 倍。
/// 只有**单价**是美元，走 [`amount_cny`]。
pub fn amount(v: &Option<Decimalish>) -> Option<f64> {
    v.as_ref().and_then(Decimalish::to_f64)
}

/// 本家**单价**的美元 → 人民币换汇率。
///
/// 本家单价早期按人民币计价，现已改为**美元**，而系统其余部分（面板展示、单价
/// 上限、扣费统计）全按人民币口径。若把美元单价直接接进去，2.2 USD 会被当成
/// 2.2 CNY —— 单价上限判断失真。故在 DTO 边界一次性换算。
///
/// **只对单价生效**：余额类字段（`remaining` / `quota` / `used_quota` / stock 的
/// `balance`）卖家给的就是人民币，走 [`amount`] 原样接入。
pub const USD_TO_CNY: f64 = 7.0;

/// 把卖家的美元**单价**换算成人民币，**保留两位小数**。
///
/// 仅用于 `price` 字段。余额类字段用 [`amount`] —— 口径错了不会报错，只会静默
/// 算错钱。
///
/// 换汇后必须收敛到两位：卖家单价本就是两位小数（`"2.20"`），但 `2.2 * 7.0` 在
/// 二进制浮点下是 `15.400000000000002`，而面板直接渲染这个数字（不做格式化），
/// 会显示成一长串尾数。金额只保留到分，与卖家的报价精度一致。
pub fn amount_cny(v: &Option<Decimalish>) -> Option<f64> {
    amount(v).map(|v| round2(v * USD_TO_CNY))
}

/// 四舍五入到两位小数（金额精度到分）。
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 计数字段的兼容层。理由与 [`Decimalish`] 相同但后果重得多：本家已知会把
/// 金额字符串化，`purchased` 哪天跟着变成 `"2"` 是完全合理的演进，而这个字段
/// 在**下单响应**里 —— 一旦整份解析失败，此时 HTTP 已是 2xx、钱已经扣了，
/// 等于把付过费的 Key 直接扔掉。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum Countish {
    Num(u32),
    Text(String),
    /// 字段缺失或是 null
    #[default]
    Absent,
}

impl Countish {
    pub fn to_u32(&self) -> Option<u32> {
        match self {
            Self::Num(v) => Some(*v),
            // 卖家可能给 "2" 也可能给 "2.0"，后者按截断取整
            Self::Text(s) => {
                let t = s.trim();
                t.parse::<u32>()
                    .ok()
                    .or_else(|| t.parse::<f64>().ok().map(|f| f.max(0.0) as u32))
            }
            Self::Absent => None,
        }
    }
}

/// 单张 Key 的两种可能形态。
///
/// 文档给的是 `[{"key":"ksk_..."}]`，但同一份文档里金额已经出现过「本该是数字
/// 却给字符串」，故对 Key 数组也留一手：裸字符串数组 `["ksk_..."]` 同样能读。
/// 这个字段同样在下单响应里，解析失败等于丢弃已付费的 Key。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum KeyEntry {
    Object { key: String },
    Bare(String),
}

impl KeyEntry {
    pub fn value(&self) -> &str {
        match self {
            Self::Object { key } => key.trim(),
            Self::Bare(s) => s.trim(),
        }
    }
}

/// `GET /api/my/profile` 响应。字段名与首家相同，只有金额类型不同。
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub name: Option<String>,
    /// 累计充值总额（**CNY**，原样接出）
    #[serde(default)]
    pub quota: Option<Decimalish>,
    /// 当前可用余额（**CNY**，原样接出）
    #[serde(default)]
    pub remaining: Option<Decimalish>,
    /// 累计消费总额（**CNY**，原样接出）
    #[serde(default)]
    pub used_quota: Option<Decimalish>,
    /// 已配置的 webhook 地址，未配置时是空串
    #[serde(default)]
    pub webhook_url: Option<String>,
}

impl From<ProfileResponse> for ProfileInfo {
    fn from(r: ProfileResponse) -> Self {
        Self {
            name: r.name,
            email: None,
            // 与首家一致：「可用余额」叫 remaining，不是 quota
            // 三个数都是人民币，不换汇（只有单价是美元）
            balance: amount(&r.remaining),
            quota: amount(&r.quota),
            used_quota: amount(&r.used_quota),
            // 文档未给限购字段
            min_purchase: None,
            max_purchase: None,
            // 空串按未配置处理，否则面板会显示一个空地址当作「已配」
            webhook_url: r.webhook_url.filter(|s| !s.trim().is_empty()),
            created_at: None,
        }
    }
}

/// `GET /api/status` 里与库存有关的那部分。
///
/// 同一个端点还给 `keys_active` / `keys_dead` / `generating`，但那些走
/// `system_status` 能力、由 [`super::flavor_legacy::VendorSystemStatus`] 承接
/// （它字段全可选且带 `flatten` 兜未知键，形态与本家一致）。这里只取下单要用的
/// 可购买数，不重复建模。
///
/// 这条路现在是 [`StockResponse`] 的**兜底**：`/api/me/stock` 能一次给出库存、
/// 单价与余额，信息更全，故优先走它；它不可用时才退回这里。
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResponse {
    /// **可购买的库存数量** —— 下单上限看这个
    #[serde(default)]
    pub keys_stock: Option<u32>,
}

impl From<StatusResponse> for StockInfo {
    fn from(r: StatusResponse) -> Self {
        Self {
            available: r.keys_stock.unwrap_or(0),
            // 该端点没有单价字段，余额需另调 profile
            price_min: None,
            price_max: None,
            balance: None,
            // 该卖家不分区
            zones: Vec::new(),
        }
    }
}

/// `GET /api/me/stock` 响应 —— 库存、单价与余额一次给全。
///
/// 注意路径是 `/api/me/stock`（**不是** `/api/my/stock`，后者实测 404）。本家其余
/// 账号维度接口都在 `/api/my` 下，只有这一个在 `/api/me`，别顺手改成 my。
///
/// 金额沿用 [`Decimalish`]：本家 `price` 与 `balance` 都是字符串
/// （`"2.20"` / `"340.500000"`）。**单位不同**：`price` 是美元（折 CNY，见
/// [`USD_TO_CNY`]），`balance` 本身就是人民币（原样）。
#[derive(Debug, Clone, Deserialize)]
pub struct StockResponse {
    /// 当前可提取库存数
    #[serde(default)]
    pub stock: Option<u32>,
    /// 单价（**USD**）。本家单一定价，故 min 与 max 同值。
    #[serde(default)]
    pub price: Option<Decimalish>,
    /// 可用余额（**CNY**，不换汇），与 `/api/my/profile` 的 `remaining` 同一个数，
    /// 接上后面板不必再为余额单独发一次 profile 请求。
    #[serde(default)]
    pub balance: Option<Decimalish>,
}

impl From<StockResponse> for StockInfo {
    fn from(r: StockResponse) -> Self {
        // 本家报价是美元，折成人民币再往下走；余额本身是人民币，不折
        let price = amount_cny(&r.price);
        Self {
            available: r.stock.unwrap_or(0),
            // 单一定价：区间的两端都是同一个价，面板会因此显示单值而非范围
            price_min: price,
            price_max: price,
            balance: amount(&r.balance),
            // 该卖家不分区
            zones: Vec::new(),
        }
    }
}

/// `POST /api/my/purchase` 响应。
///
/// **这条路上的每个字段都用兼容类型**，因为它是扣费路径：响应到手时 HTTP 已是
/// 2xx、钱已经扣了，任何一个字段的类型不符导致整份解析失败，都等于把付过费的
/// Key 扔掉。宁可读出一个语义不全的结果，也不能整份失败。
#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseResponse {
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub purchased: Countish,
    /// 购买后的剩余余额（**CNY**，字符串，不换汇）
    #[serde(default)]
    pub remaining: Option<Decimalish>,
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
}

impl From<PurchaseResponse> for PurchaseResult {
    fn from(r: PurchaseResponse) -> Self {
        let keys: Vec<PurchasedKey> = r
            .keys
            .iter()
            .map(KeyEntry::value)
            .filter(|k| !k.is_empty())
            .map(|k| PurchasedKey {
                key: k.to_string(),
                account: None,
                password: None,
                issuer_url: None,
                price: None,
                // 本家的区域是订单级的（zone），逐张不带区
                region: None,
            })
            .collect();
        // 回显数与实际条数不一致时取较大者：少算会漏入库，而入库本身按 Key
        // 去重，多算不会重复扣费。与首家同样的兜底。
        let purchased = r.purchased.to_u32().unwrap_or(0).max(keys.len() as u32);
        Self {
            purchased,
            requested: None,
            // 本家的「剩余」是账户余额，本身就是人民币，不换汇
            remaining: amount(&r.remaining),
            // 文档未给单价与扣费明细
            unit_price: None,
            total_debit: None,
            order_id: r.client_order_id,
            keys,
            // 不回显是否幂等重放，保守记 false
            replayed: false,
            zone: None,
        }
    }
}

/// 按状态码补一句本家的语义。
///
/// 本家的 `404` 意为**库存不足**而非路径写错、`403` 是余额不足、`409` 是订单号
/// 冲突。卖家给了 message 时不必多说；但它返空体或 HTML（网关拦截）时，面板上
/// 只会剩一个裸状态码 —— 运维看到 404 的第一反应必然是「路径错了/接口又改了」，
/// 方向完全跑偏。故仅在**没有可读 message 时**补这句兜底。
pub fn status_hint(status: u16) -> Option<&'static str> {
    match status {
        403 => Some("余额不足"),
        404 => Some("库存不足，无可用 Key（本家此码不表示路径错误）"),
        409 => Some("订单号冲突：同一订单号此前用过不同数量，或价格超过上限"),
        _ => None,
    }
}

/// 从本家的嵌套错误体里取人类可读信息。
///
/// 形态是 `{"error":{"code":..,"message":..,"details":{},"request_id":..}}`，
/// [`super::client::VendorClient::parse`] 里的扁平 `{"error":"文本"}` 认不出来，
/// 不单独解会把整段 JSON 连 request_id 一起塞进面板。
///
/// `code` 一并带上：本家的 401 分 `AUTH_REQUIRED`（没认到凭证）与
/// `API_TOKEN_INVALID`（Key 不对）两种，只看 message 分不清该改哪里。
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

    /// 用文档给的 profile 样本
    #[test]
    fn 解析账户信息_真实样本() {
        let raw = r#"{"name":"someone@example.com","quota":"2000.000000",
            "remaining":"884.400000","used_quota":"1115.600000",
            "webhook_url":"https://your-server.example/hook"}"#;
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(raw).unwrap().into();
        // 余额类字段卖家给的就是人民币，不换汇
        assert_eq!(p.balance, Some(884.4), "可用余额取 remaining 而非 quota");
        assert_eq!(p.quota, Some(2000.0));
        assert_eq!(p.used_quota, Some(1115.6));
        assert_eq!(p.webhook_url.as_deref(), Some("https://your-server.example/hook"));
        assert_eq!(p.name.as_deref(), Some("someone@example.com"));
    }

    /// 金额是字符串——这正是不能直接复用 legacy DTO 的原因
    #[test]
    fn 金额兼容字符串与数字() {
        let text: ProfileResponse = serde_json::from_str(r#"{"remaining":"60.560000"}"#).unwrap();
        assert_eq!(amount(&text.remaining), Some(60.56));

        let num: ProfileResponse = serde_json::from_str(r#"{"remaining":60.56}"#).unwrap();
        assert_eq!(amount(&num.remaining), Some(60.56));

        // 解析不了的串当缺失，不能让整份 profile 失败
        let bad: ProfileResponse =
            serde_json::from_str(r#"{"remaining":"待定","name":"x"}"#).unwrap();
        assert_eq!(amount(&bad.remaining), None);
        assert_eq!(bad.name.as_deref(), Some("x"), "其余字段仍要读出来");
    }

    /// 换汇这一步单独钉住：本家改成美元计价后，接出去的必须是人民币。
    /// `amount` 保持原样（看卖家原始报价用），`amount_cny` 负责折算。
    #[test]
    fn 美元金额按汇率折成人民币() {
        let r: StockResponse = serde_json::from_str(r#"{"price":"2.20"}"#).unwrap();
        assert_eq!(amount(&r.price), Some(2.2), "amount 给原始美元数");
        // 恰好 15.40 而非 15.400000000000002 —— 换汇后收敛到两位，面板直接渲染
        // 这个数字，不收敛就会显示成一长串尾数
        assert_eq!(amount_cny(&r.price), Some(15.4), "2.20 USD → 15.40 CNY");

        // 缺失与不可解析仍是 None，换汇不该把它变成 0
        assert_eq!(amount_cny(&None), None);
        let bad: StockResponse = serde_json::from_str(r#"{"price":"暂无"}"#).unwrap();
        assert_eq!(amount_cny(&bad.price), None);
    }

    /// 换汇结果必须是两位小数的「干净」数字。
    ///
    /// 卖家报价本身两位（`"2.20"`），乘 7 后二进制浮点会带尾数。面板不做格式化，
    /// 直接把这个 f64 渲染出来，故收敛必须发生在后端。
    #[test]
    fn 换汇后收敛到两位小数() {
        let cases = [
            ("2.20", 15.4),   // 2.2 * 7 = 15.400000000000002
            ("30.00", 210.0), // 精确值，不该被动到
            ("0.07", 0.49),   // 0.07 * 7 = 0.48999999999999994
            ("1.23", 8.61),   // 1.23 * 7 = 8.610000000000001
        ];
        for (raw, want) in cases {
            let r: StockResponse =
                serde_json::from_str(&format!(r#"{{"price":"{raw}"}}"#)).unwrap();
            let got = amount_cny(&r.price).unwrap();
            assert_eq!(got, want, "{raw} USD 折人民币应为 {want}，实得 {got}");
        }
    }

    /// 未配置 webhook 时返回空串，不能当成「已配置一个空地址」
    #[test]
    fn 空webhook地址按未配置处理() {
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(r#"{"webhook_url":""}"#)
            .unwrap()
            .into();
        assert!(p.webhook_url.is_none());
    }

    /// 用文档给的 status 样本。其余字段由 legacy 的 VendorSystemStatus 承接，
    /// 这里只关心可购买数。这条路现在是 `/api/me/stock` 的兜底。
    #[test]
    fn 解析库存_status兜底样本() {
        let raw = r#"{"keys_active":5,"keys_dead":0,"keys_stock":25,"generating":false}"#;
        let s: StatusResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(s.keys_stock, Some(25));

        let stock: StockInfo = s.into();
        assert_eq!(stock.available, 25);
        // 兜底路径只有数量，单价与余额拿不到
        assert!(stock.price_min.is_none());
        assert!(stock.balance.is_none());
    }

    /// 用线上 `/api/me/stock` 的真实返回做样本。金额是字符串，是本家的既有特征。
    #[test]
    fn 解析库存报价_真实样本() {
        let raw = r#"{"balance":"340.500000","price":"2.20","stock":0}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 0);
        // 单一定价：区间两端同值，面板据此显示单值而非范围。
        // 2.20 USD → 15.40 CNY（字面量而非 2.2 * USD_TO_CNY：换汇后已收敛到两位）
        assert_eq!(s.price_min, Some(15.4));
        assert_eq!(s.price_max, Some(15.4));
        // 余额顺带给出，省一次 profile 请求；它本身是人民币，不换汇
        assert_eq!(s.balance, Some(340.5));
        // 该家不分区
        assert!(s.zones.is_empty());
        assert!(s.pick_zone().is_none());
    }

    /// 混合口径的回归锚：文档明写 `price` 是 **USD**、`balance` 是 **CNY**。
    ///
    /// 这两个字段在同一份响应里、同为字符串、同为金额，唯一区别是单位 ——
    /// 一律换汇或一律不换汇都不会报错，只会静默算错钱。故用文档原样的数值钉住：
    /// 余额若被误折成 14420，单价上限与余额告警会同时失真。
    #[test]
    fn 库存报价_单价美元而余额人民币() {
        let raw = r#"{"stock":120,"price":"30.00","balance":"2060.00"}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 120);
        // 单价是美元，折人民币：30 USD → 210 CNY
        assert_eq!(s.price_min, Some(210.0));
        assert_eq!(s.price_max, Some(210.0));
        // 余额本身就是人民币，原样接出，绝不是 2060 × 7
        assert_eq!(s.balance, Some(2060.0), "余额是 CNY，换汇会虚增 7 倍");
    }

    #[test]
    fn 解析库存报价_金额给数字也能吃下() {
        // 对方哪天把字符串改成数字，不该整份解析失败
        let raw = r#"{"stock":7,"price":2.2,"balance":340.5}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 7);
        assert_eq!(s.price_min, Some(15.4));
        assert_eq!(s.balance, Some(340.5));
    }

    #[test]
    fn 解析库存报价_缺字段不报错() {
        let s: StockInfo = serde_json::from_str::<StockResponse>("{}").unwrap().into();
        assert_eq!(s.available, 0, "缺库存按没货");
        assert!(s.price_min.is_none());
        assert!(s.balance.is_none());
    }

    #[test]
    fn 解析库存报价_金额无法解析时只丢该字段() {
        // 单价读不出来不该连库存一起丢 —— 库存是下单要用的，单价只是展示
        let raw = r#"{"stock":3,"price":"暂无","balance":"340.5"}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 3);
        assert!(s.price_min.is_none());
        assert_eq!(s.balance, Some(340.5));
    }

    /// 跨模块契约：`/api/status` 的完整响应由 legacy 的 `VendorSystemStatus`
    /// 承接（见本文件 `StatusResponse` 的注释）。这里用同一份文档样本把它钉住 ——
    /// 否则 Drop 的响应形态若与那个结构分叉（如 `generating` 变成字符串），
    /// 整份反序列化失败、状态卡片全空，而只测 `StatusResponse` 的用例仍然全绿。
    #[test]
    fn 系统状态由legacy结构承接() {
        let raw = r#"{"keys_active":5,"keys_dead":0,"keys_stock":25,"generating":false}"#;
        let s: super::super::flavor_legacy::VendorSystemStatus =
            serde_json::from_str(raw).expect("legacy 结构必须能吃下 Drop 的 status 响应");
        assert_eq!(s.keys_active, Some(5));
        assert_eq!(s.keys_dead, Some(0));
        assert_eq!(s.keys_stock, Some(25));
        assert_eq!(s.generating, Some(false));
        // 四个字段都该落进已建模字段，而非 flatten 兜底
        assert!(s.extra.is_empty(), "不该有字段落进 extra: {:?}", s.extra);
    }

    #[test]
    fn 库存缺字段时按0而非报错() {
        let s: StatusResponse = serde_json::from_str("{}").unwrap();
        let stock: StockInfo = s.into();
        assert_eq!(stock.available, 0, "缺字段按没货，不能让状态卡片整体失败");
    }

    /// 用文档给的下单成功样本
    #[test]
    fn 解析下单成功_真实样本() {
        let raw = r#"{"client_order_id":"0123456789abcdef0123456789abcdef","purchased":2,
            "remaining":"884.400000","keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.remaining, Some(884.4), "剩余即账户余额，本就是人民币，不换汇");
        assert_eq!(
            r.order_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert!(!r.replayed);
    }

    #[test]
    fn 出货数取回显与实际条数的较大者() {
        // 回显 1 但实发 2 条，按实际条数算，否则会漏入库
        let raw = r#"{"purchased":1,"keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);

        // 反方向：回显 2 实发 1。取较大者会让统计偏大，但入库只认 keys 数组，
        // 不会凭空造出凭据，也不会多扣费。
        let raw = r#"{"purchased":2,"keys":[{"key":"ksk_a"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 1, "入库只认实际条数");
    }

    /// 扣费路径的容错：`purchased` 被字符串化时**不能整份解析失败**。
    ///
    /// 这不是假想 —— 本家已知把金额写成字符串（`"remaining":"884.4"`），
    /// `purchased` 跟着变形完全合理。而此时 HTTP 是 2xx、钱已经扣了，
    /// 整份失败等于把付过费的 Key 扔掉。
    #[test]
    fn 字符串化的出货数仍能读出() {
        let raw = r#"{"purchased":"2","remaining":"884.4","keys":[{"key":"ksk_a"},{"key":"ksk_b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .expect("purchased 为字符串时不能整份失败")
            .into();
        assert_eq!(r.purchased, 2);
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.remaining, Some(884.4));

        // 带小数的字符串按截断取整
        let raw = r#"{"purchased":"2.0","keys":[]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 2);

        // 认不出的值当缺失，靠 keys 条数兜底，仍不整份失败
        let raw = r#"{"purchased":"未知","keys":[{"key":"ksk_a"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.purchased, 1, "回落到实际条数");
    }

    /// 同上：`keys` 若变成裸字符串数组也要能读
    #[test]
    fn 裸字符串形态的key数组仍能读出() {
        let raw = r#"{"purchased":2,"keys":["ksk_a","ksk_b"]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .expect("裸字符串 keys 数组不能整份失败")
            .into();
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.keys[0].key, "ksk_a");

        // 两种形态混在一起也要能读
        let raw = r#"{"purchased":2,"keys":[{"key":"ksk_a"},"ksk_b"]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.keys[1].key, "ksk_b");
    }

    #[test]
    fn 状态码语义兜底() {
        // 本家的 404 是「没货」而不是「路径错」，这句兜底就是为了防止误判方向
        assert!(status_hint(404).unwrap().contains("库存不足"));
        assert!(status_hint(404).unwrap().contains("路径"), "要明确排除路径错的误解");
        assert!(status_hint(403).unwrap().contains("余额不足"));
        assert!(status_hint(409).unwrap().contains("订单号"));
        // 没有特殊语义的码不编造说法
        assert!(status_hint(500).is_none());
        assert!(status_hint(401).is_none(), "401 的两种 code 由 error_message 区分");
    }

    #[test]
    fn 空key条目被剔除() {
        let raw = r#"{"purchased":0,"keys":[{"key":"  "},{"key":"ksk_a"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw).unwrap().into();
        assert_eq!(r.keys.len(), 1);
        assert_eq!(r.keys[0].key, "ksk_a");
    }

    /// 用真实的 401 返回做样本
    #[test]
    fn 解析嵌套错误体_真实样本() {
        let raw = r#"{"error":{"code":"API_TOKEN_INVALID","details":{},
            "message":"API Key 无效","request_id":"req_9ec835d9"}}"#;
        let msg = error_message(raw).unwrap();
        assert!(msg.contains("API Key 无效"));
        // 两种 401 的处置不同（没带头 vs Key 不对），code 必须带上
        assert!(msg.contains("API_TOKEN_INVALID"), "实际: {msg}");
        // request_id 不该混进面板文案
        assert!(!msg.contains("req_"), "实际: {msg}");
    }

    #[test]
    fn 解析扁平错误体() {
        assert_eq!(
            error_message(r#"{"error":"余额不足"}"#).as_deref(),
            Some("余额不足")
        );
    }

    #[test]
    fn 只有code时也给出信息() {
        let raw = r#"{"error":{"code":"NOT_FOUND","details":{}}}"#;
        assert_eq!(error_message(raw).as_deref(), Some("NOT_FOUND"));
    }

    #[test]
    fn 无法识别的错误体返回none() {
        assert!(error_message("upstream boom").is_none());
        assert!(error_message(r#"{"ok":true}"#).is_none());
        assert!(error_message(r#"{"error":{"message":"  "}}"#).is_none());
    }
}
