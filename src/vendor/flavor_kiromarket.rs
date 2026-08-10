//! 第五家卖家 kiro-market（api.91kiro.com）的协议实现：`/api/my/*` + `X-API-Key: usr-xxx`
//!
//! 本模块只负责「原始 DTO → 中立结构」的翻译，不发请求（HTTP 在 [`super::client`]）。
//!
//! 与 [`super::flavor_legacy`] 同路径前缀、同鉴权头、同下单参数名，但响应形态
//! 差异足以单独成家：
//!
//! - `keys` 是**对象数组**且每张带 `paid`（阶梯定价：同一单可能混着不同价的
//!   Key，因为提货按最早入库先给、会跨车次）。对账只能用 `total_credits` 与
//!   逐张 `paid`，`unit_price × 数量` 算出来会不等于实扣。
//! - 2026-08-07 起**不再下发** `account` / `password` / `issuer_url` / `endpoint`。
//!   前三个是子号的网页登录凭据，与调 API 用的 `key` 是两回事；卖家去掉它们是
//!   为了避免「同一份凭证在多个 IP 被使用」触发 AWS 封号。故中立结构里这三个
//!   字段对本家恒为 `None`。
//! - 余额**不在**库存接口里，要单独取 profile（首家的 `/api/my/stock` 顺带给
//!   `quota`，本家没有）。
//! - `zones[]` 没有 `enabled` 字段，一律按开放处理；`unit_price` 已是「按车次
//!   存活时长降过的现价」，`base_price` 才是基准价。展示现价用前者。
//!
//! @author wangzhong

use serde::Deserialize;

use super::protocol::{
    LedgerEntry, OrderInfo, Paged, ProfileInfo, PurchaseResult, PurchasedKey, RedeemResult,
    StockInfo, VendorKeyInfo, ZoneStock,
};

/// 路径前缀。账号维度接口都在 `/api/my` 下。
pub const PATH_STOCK: &str = "/api/my/stock";
pub const PATH_PURCHASE: &str = "/api/my/purchase";
pub const PATH_PROFILE: &str = "/api/my/profile";
pub const PATH_ORDERS: &str = "/api/my/orders";
pub const PATH_REDEEM: &str = "/api/my/redeem";
pub const PATH_LEDGER: &str = "/api/my/ledger";
pub const PATH_KEYS: &str = "/api/my/keys";
pub const PATH_WEBHOOK: &str = "/api/my/webhook";
pub const PATH_WEBHOOK_TEST: &str = "/api/my/webhook/test";

/// 分页上限。文档：`limit` 上限 200（订单列表）。
pub const MAX_LIMIT: u32 = 200;

// 单次提货上限（文档：count 范围 1–200，超出回 400 bad_count）刻意不定义常量：
// 提取数量由 `auto::decide_count` 按「卖家库存 max × 配置上限」夹取，而库存接口
// 给的 max 本身已封顶 200，再加一个常量就是第二个真相来源。

// ============ 库存 ============

/// `GET /api/my/stock` 的 `zones[]` 单项
#[derive(Debug, Clone, Deserialize)]
pub struct StockZone {
    pub zone: String,
    /// 完整 AWS 区域标识（`us-east-1` / `eu-central-1`）
    #[serde(default)]
    pub region: Option<String>,
    /// 本区当前可购量
    #[serde(default)]
    pub available: u32,
    /// 本区**现价**（已按车次存活时长降过），可直接展示。
    ///
    /// 卖家还给一个 `base_price`（降价前的基准价，供「原价 40 → 现价 25」展示），
    /// 但中立结构 [`ZoneStock`] 没有对应字段，故不建模 —— 建了就是个没人读的死
    /// 字段。要展示原价得先给中立结构加位置。
    #[serde(default)]
    pub unit_price: Option<f64>,
}

/// `GET /api/my/stock` 响应。
///
/// 卖家还给 `stock`（`public_available` / `my_private` / `my_keys`）、
/// `min_per_order` / `max_per_order` / `warranty_minutes`，但中立结构
/// [`StockInfo`] 只有「可提数量 + 报价 + 余额 + 分区」四类位置，那些都无处安放，
/// 故不建模。serde 默认忽略未知字段，多给不会解析失败。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StockResponse {
    /// 一次性最多能提的数量（= 公共余量，封顶 200）
    #[serde(default)]
    pub max: u32,
    /// 按 `us` / `eu` 固定顺序给全，没货的区 `available` 为 0
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
                // 卖家不给中文名，用 region 兜一个更可读的标签
                label: z.region,
                available: z.available,
                stock: None,
                // 展示与选区都用现价 —— base_price 是降价前的基准，
                // 拿它选区会按一个提不到的价位排序
                unit_price: z.unit_price,
                // 本家没有这个字段，一律按开放处理
                enabled: true,
            })
            .collect();
        // 报价区间只算「开放且有货」的区：把 0 库存区的价算进来，
        // 面板就会显示一个实际提不到的价位
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
            // 本家库存接口不给余额，要单独取 profile
            balance: None,
            zones,
        }
    }
}

// ============ 提货 ============

/// `POST /api/my/purchase` 的 `keys[]` 单项。
///
/// 注意 2026-08-07 起卖家不再下发 `account` / `password` / `issuer_url` / `endpoint`，
/// 故这里没有对应字段 —— 中立结构里那三个对本家恒为 `None`。
///
/// 只建模中立结构用得上的两个字段。卖家还给 `id` / `round_id` / `region` /
/// `zone` / `free` / `warranty_until`，但 [`PurchasedKey`] 只有「密钥 + 实付」
/// 两个位置能承接 —— 区域取自响应顶层的 `zone`（一单只来自一个区），
/// 质保则不需要我方动作（期内车次判死自动全额退款）。
#[derive(Debug, Clone, Deserialize)]
pub struct PurchasedKeyDto {
    pub key: String,
    /// 这一张实际扣掉的积分。阶梯定价下逐张不同，逐张之和恒等于 `total_credits`。
    #[serde(default)]
    pub paid: Option<f64>,
}

/// `POST /api/my/purchase` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseResponse {
    /// 卖家实际出货数。**必须按它而不是请求的 count 处理** ——
    /// 库存并发争抢，申请 5 个拿到 3 个是正常结果。
    #[serde(default)]
    pub purchased: u32,
    /// 真正的订单号，可拿它调补拉接口重新取键。
    ///
    /// 卖家还回显 `client_order_id`（我们发过去的幂等键）、`free_count`、
    /// `warranty_minutes`，但中立结构 [`PurchaseResult`] 没有对应位置：
    /// 幂等键本就是我方生成的、免费数与质保时长不影响入库与对账。
    #[serde(default)]
    pub order_id: Option<String>,
    /// 实际成交区域（卖家回显）
    #[serde(default)]
    pub zone: Option<String>,
    /// 本单单价。混价单里这只是其中一张的价，**不能乘数量当总额**。
    #[serde(default)]
    pub unit_price: Option<f64>,
    /// 本单实扣积分。这是对账的权威值。
    #[serde(default)]
    pub total_credits: Option<f64>,
    /// 提货后账户余额
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub keys: Vec<PurchasedKeyDto>,
}

impl From<PurchaseResponse> for PurchaseResult {
    fn from(r: PurchaseResponse) -> Self {
        // 逐张 paid 之和。**只在每张都给了 paid 时才可用** —— 缺一张就少算一份，
        // 会把实扣报少（面板据此对账，报少比报错更难发现）。
        let paid_sum = if !r.keys.is_empty() && r.keys.iter().all(|k| k.paid.is_some()) {
            Some(r.keys.iter().filter_map(|k| k.paid).sum::<f64>())
        } else {
            None
        };

        // 先取出来：下面 into_iter() 会把 r.keys 移走，闭包里再读 r 的字段
        // 就要依赖 partial move 与 disjoint capture 的交互，不值得赌
        let unit_price = r.unit_price;
        let keys: Vec<PurchasedKey> = r
            .keys
            .into_iter()
            .map(|k| PurchasedKey {
                key: k.key,
                // 这三样卖家已不再下发，见模块头注释
                account: None,
                password: None,
                issuer_url: None,
                // 阶梯定价：优先用这一张自己的实付，缺失时退回本单单价
                price: k.paid.or(unit_price),
            })
            .collect();
        // 卖家回显数与实际条数不一致时取较大者，避免漏入库
        let purchased = r.purchased.max(keys.len() as u32);
        Self {
            purchased,
            // 卖家不回显请求数（按 purchased 处理即可，见其字段注释）
            requested: None,
            remaining: r.remaining,
            unit_price,
            // 权威值优先：total_credits → 逐张 paid 之和 → 单价 × 成交数
            total_debit: r
                .total_credits
                .or(paid_sum)
                .or_else(|| unit_price.map(|p| p * purchased as f64)),
            order_id: r.order_id,
            keys,
            // 本家幂等重放返回**字节完全一致**的结果，没有任何标志位可区分，
            // 故无法判断，保守记 false（与首家同样处置）
            replayed: false,
            zone: r.zone,
        }
    }
}

// ============ 账号 ============

/// `GET /api/my/profile` 的 `profile` 子对象。
///
/// 只建模 [`ProfileInfo`] 能承接的字段。卖家还给 `id` / `role` /
/// `api_key_prefix` / `earned`（分成收入）以及三个持有上限相关的数
/// （`max_keys_held` / `hold_cap_effective` / `keys_held`）。
///
/// 持有上限那三个本可用来「买之前先判一下，省掉一次注定失败的下单」，但那要在
/// service 层加一道前置检查、且中立结构没有位置放它们 —— 属于独立的一件事，
/// 不在本次接入范围内。缺了它只是撞上 409 `purchase_cap_reached` 后由面板报错，
/// 不会错扣积分。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileBody {
    #[serde(default)]
    pub username: Option<String>,
    /// 账户余额（积分），领 Key 时扣的就是它
    #[serde(default)]
    pub balance: Option<f64>,
    /// 累计已花
    #[serde(default)]
    pub spent: Option<f64>,
    /// 自己车的通知地址
    #[serde(default)]
    pub webhook_private_url: Option<String>,
    /// 公共车（补货）的通知地址
    #[serde(default)]
    pub webhook_public_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// `GET /api/my/profile` 响应。档案套在 `profile` 键下，与首家的扁平形态不同。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileResponse {
    #[serde(default)]
    pub profile: ProfileBody,
}

impl From<ProfileResponse> for ProfileInfo {
    fn from(r: ProfileResponse) -> Self {
        let p = r.profile;
        Self {
            name: p.username,
            email: None,
            balance: p.balance,
            // 刻意留空：本家只给 balance 与 spent，没有「总配额」。
            // 用 balance + spent 凑一个会是错的 —— balance 里还含分成收入与
            // 质保退款，那个和不等于累计充值。
            quota: None,
            used_quota: p.spent,
            // 限购在库存接口（min_per_order / max_per_order），不在档案里。
            // 中立结构这两个字段目前无消费者，与首家 / Drop 同样留空。
            min_purchase: None,
            max_purchase: None,
            // 我们关心的是补货通道，取公共车地址；未配时退到自己车地址
            // （文档：私有地址留空会回落到公共地址，反向取才符合语义）
            webhook_url: p
                .webhook_public_url
                .filter(|s| !s.trim().is_empty())
                .or(p.webhook_private_url),
            created_at: p.created_at,
        }
    }
}

/// `POST /api/my/redeem` 响应
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RedeemResponse {
    /// 本次到账额度
    #[serde(default)]
    pub quota: Option<f64>,
    /// 兑换后余额
    #[serde(default)]
    pub balance: Option<f64>,
}

impl From<RedeemResponse> for RedeemResult {
    fn from(r: RedeemResponse) -> Self {
        Self {
            quota: r.quota,
            balance: r.balance,
            previous_quota: None,
            redeemed_at: None,
            // 本家不回显是否重复兑换：已用过的码直接回 404 redeem_invalid，
            // 走的是错误分支而非这里
            replayed: false,
        }
    }
}

// ============ 列表类：宽松信封 ============

/// 列表响应信封。**刻意宽松** —— 文档给了这三个列表接口的字段，却没有明确
/// 外层是裸数组还是带键的对象。猜错的后果是面板拿到一条「解析响应失败」而不是
/// 列表，故三种形态都认：
///
/// - 裸数组 `[...]`
/// - `{"items":[...]}` / `{"orders":[...]}` / `{"entries":[...]}` / `{"keys":[...]}`
///
/// 这些接口都是只读展示，认宽一点没有扣费风险。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Envelope<T> {
    /// 裸数组
    Bare(Vec<T>),
    /// 带键对象。四个别名指向同一字段，卖家用哪个都能接住。
    Wrapped {
        #[serde(default = "Vec::new", alias = "orders", alias = "entries", alias = "keys")]
        items: Vec<T>,
        #[serde(default)]
        total: Option<u32>,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        offset: Option<u32>,
    },
}

impl<T> Envelope<T> {
    /// 转成中立分页结构，同时把元素映射成中立类型。
    ///
    /// 本家分页用 `limit` / `offset`，中立结构用 `page` / `page_size`，
    /// 故按 offset 反算页码（offset 不是 limit 的整数倍时向下取整）。
    pub fn map_into<U: From<T>>(self) -> Paged<U> {
        let (items, total, limit, offset) = match self {
            Self::Bare(items) => (items, None, None, None),
            Self::Wrapped {
                items,
                total,
                limit,
                offset,
            } => (items, total, limit, offset),
        };
        let count = items.len() as u32;
        let mapped: Vec<U> = items.into_iter().map(U::from).collect();
        let page_size = limit.filter(|v| *v > 0).unwrap_or(count.max(1));
        Paged {
            items: mapped,
            total: total.or(Some(count)),
            page: Some(offset.unwrap_or(0) / page_size + 1),
            page_size: Some(page_size),
            // 总数未知时不编一个页数出来
            pages: total.map(|t| t.div_ceil(page_size)),
        }
    }
}

/// `GET /api/my/orders` 单条订单
#[derive(Debug, Clone, Deserialize)]
pub struct Order {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub client_order_id: Option<String>,
    /// 本单成交数量
    #[serde(default)]
    pub count: Option<u32>,
    /// 实扣积分。对账用它。
    ///
    /// 卖家还给 `unit_price` 与 `free_count`，两者都不建模：混价单里
    /// `unit_price` 只是其中一张的价，乘数量会与实扣不符 —— 中立结构的
    /// `total_debit` 只该由 `charged` 填，留着另一个字段反而容易被误用。
    #[serde(default)]
    pub charged: Option<f64>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<Order> for OrderInfo {
    fn from(o: Order) -> Self {
        Self {
            client_order_id: o.client_order_id,
            order_id: o.id,
            // 卖家只记成交数，不记当初请求了多少
            requested: None,
            purchased: o.count,
            // 文档明确：对账用 charged，不要用 unit_price × 数量
            total_debit: o.charged,
            created_at: o.created_at,
        }
    }
}

/// `GET /api/my/ledger` 单条流水。
///
/// 文档只列了 `reason` 的取值（`recharge` / `purchase` / `income` / `warranty` /
/// `clawback` / `adjust` / `commit`），未给完整字段名，故几个常见别名都接。
#[derive(Debug, Clone, Deserialize)]
pub struct Ledger {
    #[serde(default, alias = "id")]
    pub seq: Option<i64>,
    /// 变动类型。本家叫 `reason`，其余家叫 `type`。
    #[serde(default, alias = "type")]
    pub reason: Option<String>,
    /// 带符号金额
    #[serde(default, alias = "quota", alias = "delta")]
    pub amount: Option<f64>,
    #[serde(default, alias = "balance")]
    pub balance_after: Option<f64>,
    #[serde(default, alias = "note", alias = "detail")]
    pub memo: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<Ledger> for LedgerEntry {
    fn from(l: Ledger) -> Self {
        Self {
            seq: l.seq,
            entry_type: l.reason,
            amount: l.amount,
            balance_after: l.balance_after,
            memo: l.memo,
            created_at: l.created_at,
        }
    }
}

/// `GET /api/my/keys` 单条。**只给前缀，不给正文。**
///
/// 卖家还给密钥前缀与 `round_id` / `zone`，都不建模：[`VendorKeyInfo`] 里
/// 唯一能放密钥的字段语义是**正文**，塞前缀进去会让「与本地凭据池对账」
/// 按前缀误判成命中；车次与区域则没有对应位置。
#[derive(Debug, Clone, Deserialize)]
pub struct MyKey {
    #[serde(default)]
    pub id: Option<String>,
    /// `sold` 正常 / `dead` 已失效 / `revoked` 被吊销且不退积分。
    /// 判断能不能用看这个，不要看剩余额度。
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub purchased_at: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

impl From<MyKey> for VendorKeyInfo {
    fn from(k: MyKey) -> Self {
        Self {
            id: k.id,
            // 刻意留空：本家只给前缀。把前缀塞进 key_value 会让「与本地凭据池
            // 对账」按前缀误判成命中 —— 该字段的语义是密钥**正文**。
            key_value: None,
            // 本家不下发 AWS 账号（2026-08-07 起），前缀也不适合冒充账号名
            account: None,
            status: k.status,
            purchased_at: k.purchased_at,
            created_at: k.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ============ 库存 ============

    /// 文档 §5.2 给的库存样本。注意它与首家的差异：余额不在这里、
    /// zones 没有 enabled、unit_price 是降过的现价而 base_price 是基准价。
    #[test]
    fn 库存转中立结构_文档样本() {
        let raw = r#"{
            "stock": {"public_available": 12, "my_private": 0, "my_keys": 27},
            "zones": [
              {"zone":"us","region":"us-east-1","available":8,"unit_price":25,"base_price":40},
              {"zone":"eu","region":"eu-central-1","available":4,"unit_price":10,"base_price":10}
            ],
            "max": 12, "min_per_order": 1, "max_per_order": 200, "warranty_minutes": 10
        }"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();

        assert_eq!(s.available, 12, "顶层 max 是权威的可提数量");
        assert_eq!(s.zones.len(), 2);
        // 报价必须取现价而非基准价：拿 base_price（40）展示就会出现
        // 「页面挂着 40、实际扣 25」
        assert_eq!(s.price_min, Some(10.0));
        assert_eq!(s.price_max, Some(25.0));
        // 本家库存接口不给余额，必须单独取 profile
        assert!(s.balance.is_none(), "库存接口没有余额字段");
        // region 兜作 label，面板不至于只显示 us / eu
        assert_eq!(s.zones[0].label.as_deref(), Some("us-east-1"));
        // 关键：选最便宜且有货的区。选错区会按另一个价扣费
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("eu"));
    }

    /// 本家 zones 没有 enabled 字段，缺失时必须按开放处理 ——
    /// 若默认成 false，`pick_zone` 会一个区都选不出来，所有提取都被挡掉
    #[test]
    fn 库存_缺enabled字段按开放处理() {
        let raw = r#"{"max":5,"zones":[{"zone":"us","available":5,"unit_price":20}]}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert!(s.zones[0].enabled);
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("us"));
    }

    /// 美区空、欧区有货 —— 不显式选区就会撞上「只从美区取且不跨区补」
    #[test]
    fn 库存_美区空时选到欧区() {
        let raw = r#"{"max":4,"zones":[
            {"zone":"us","available":0,"unit_price":25},
            {"zone":"eu","available":4,"unit_price":30}]}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        // 欧区更贵，但美区无货，只能选它
        assert_eq!(s.pick_zone().map(|z| z.zone.as_str()), Some("eu"));
        // 报价区间只算有货的区：美区那个 25 实际提不到
        assert_eq!(s.price_min, Some(30.0));
    }

    #[test]
    fn 库存_顶层max缺失时按各区之和兜底() {
        let raw = r#"{"zones":[
            {"zone":"us","available":3,"unit_price":20},
            {"zone":"eu","available":4,"unit_price":10}]}"#;
        let s: StockInfo = serde_json::from_str::<StockResponse>(raw).unwrap().into();
        assert_eq!(s.available, 7);
    }

    // ============ 提货 ============

    /// 文档 §5.3 给的下单样本
    #[test]
    fn 下单转中立结构_文档样本() {
        let raw = r#"{
            "client_order_id":"0a1b2c3d4e5f60718293a4b5c6d7e8f9",
            "order_id":"ord-1","zone":"us","purchased":2,
            "unit_price":30,"total_credits":60,"remaining":4500,
            "keys":[
              {"id":"k1","round_id":"r1","key":"ksk_aaa","region":"us-east-1",
               "zone":"us","free":false,"paid":30,
               "warranty_until":"2026-08-01T12:34:56Z"},
              {"id":"k2","round_id":"r1","key":"ksk_bbb","region":"us-east-1",
               "zone":"us","free":false,"paid":30,
               "warranty_until":"2026-08-01T12:34:56Z"}],
            "free_count":0,"warranty_minutes":10
        }"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();

        assert_eq!(r.purchased, 2);
        assert_eq!(r.zone.as_deref(), Some("us"), "区域要透出，否则看不出钱花在哪个区");
        assert_eq!(r.total_debit, Some(60.0), "扣费以 total_credits 为准");
        assert_eq!(r.remaining, Some(4500.0));
        assert_eq!(r.order_id.as_deref(), Some("ord-1"));
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.keys[0].key, "ksk_aaa");
        assert_eq!(r.keys[0].price, Some(30.0));
        // 2026-08-07 起卖家不再下发这三样，恒为 None
        assert!(r.keys[0].account.is_none());
        assert!(r.keys[0].password.is_none());
        assert!(r.keys[0].issuer_url.is_none());
        // 本家幂等重放返回字节完全一致的响应，无从判断，保守记 false
        assert!(!r.replayed);
    }

    /// 混价单：提货按最早入库先给、会跨车次，故同一单里各张单价可能不同。
    /// `unit_price × 数量` 算出来是错的，必须用 `total_credits`。
    #[test]
    fn 下单_混价单以total_credits为准() {
        let raw = r#"{"purchased":3,"unit_price":30,"total_credits":75,"zone":"us",
            "keys":[{"key":"ksk_a","paid":30},{"key":"ksk_b","paid":25},
                    {"key":"ksk_c","paid":20}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.total_debit, Some(75.0));
        // 按 unit_price 乘出来会是 90，与实扣不符
        assert_ne!(r.total_debit, Some(90.0));
        // 逐张实付各自不同，面板靠它解释 totalDebit 怎么来的
        assert_eq!(r.keys[0].price, Some(30.0));
        assert_eq!(r.keys[2].price, Some(20.0));
    }

    /// 缺 total_credits 时退到逐张 paid 之和 —— 它恒等于 total_credits
    #[test]
    fn 下单_缺total时用逐张paid之和() {
        let raw = r#"{"purchased":3,"unit_price":30,
            "keys":[{"key":"a","paid":30},{"key":"b","paid":25},{"key":"c","paid":20}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.total_debit, Some(75.0), "应为逐张之和而非 30×3");
    }

    /// **只要有一张缺 paid 就不能用求和** —— 少算一份会把实扣报少，
    /// 而报少比报错更难发现（面板不会有任何异常提示）
    #[test]
    fn 下单_paid不齐时不求和而退回单价() {
        let raw = r#"{"purchased":3,"unit_price":30,
            "keys":[{"key":"a","paid":30},{"key":"b"},{"key":"c","paid":20}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        // 求和只有 50，会把实扣报少；应退到 单价 × 成交数 = 90
        assert_eq!(r.total_debit, Some(90.0));
        assert_ne!(r.total_debit, Some(50.0), "缺 paid 的张不能被当成 0 计入");
        // 缺 paid 的那张按本单单价兜底
        assert_eq!(r.keys[1].price, Some(30.0));
    }

    /// 库存并发争抢，申请 5 个拿到 3 个是正常结果，按实际成交处理
    #[test]
    fn 下单_部分成交() {
        let raw = r#"{"purchased":3,"unit_price":25,"total_credits":75,"zone":"eu",
            "keys":[{"key":"a","paid":25},{"key":"b","paid":25},{"key":"c","paid":25}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.purchased, 3);
        assert_eq!(r.zone.as_deref(), Some("eu"));
    }

    /// 回显数与实际条数不一致时取较大者，否则会漏入库
    #[test]
    fn 下单_purchased取较大者() {
        let raw = r#"{"purchased":1,"keys":[{"key":"a"},{"key":"b"}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.purchased, 2);
    }

    /// 免费交付（留自用车）：paid 为 0，不能因为「没花钱」就当成缺失
    #[test]
    fn 下单_免费交付实付为零() {
        let raw = r#"{"purchased":1,"total_credits":0,"free_count":1,
            "keys":[{"key":"a","free":true,"paid":0}]}"#;
        let r: PurchaseResult = serde_json::from_str::<PurchaseResponse>(raw)
            .unwrap()
            .into();
        assert_eq!(r.total_debit, Some(0.0));
        assert_eq!(r.keys[0].price, Some(0.0));
    }

    // ============ 档案与兑换 ============

    /// 文档 §5.1 给的档案样本。档案套在 profile 键下，与首家的扁平形态不同
    #[test]
    fn 档案转中立结构_文档样本() {
        let raw = r#"{"profile":{
            "id":"u1","username":"alice","role":"user",
            "balance":1400,"spent":600,"earned":0,
            "max_keys_held":20,"hold_cap_effective":10,"keys_held":7,
            "api_key_prefix":"usr-1a2b3c4d",
            "webhook_private_url":"","webhook_public_url":"https://e.com/hook",
            "created_at":"2026-07-30T12:00:00Z","last_login_at":"x"},
            "auth_mode":"api_key"}"#;
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(raw).unwrap().into();

        assert_eq!(p.name.as_deref(), Some("alice"));
        assert_eq!(p.balance, Some(1400.0));
        assert_eq!(p.used_quota, Some(600.0));
        // 刻意留空：balance + spent 不等于累计充值（balance 里还含分成收入与
        // 质保退款），凑一个数会是错的
        assert!(p.quota.is_none(), "本家没有「总配额」这个概念");
        // 取公共车地址（补货通道），private 为空串时不能选它
        assert_eq!(p.webhook_url.as_deref(), Some("https://e.com/hook"));
        assert_eq!(p.created_at.as_deref(), Some("2026-07-30T12:00:00Z"));
    }

    /// 公共地址未配时退到自己车地址
    #[test]
    fn 档案_公共地址为空时退到私有地址() {
        let raw = r#"{"profile":{"webhook_private_url":"https://p.com/h",
            "webhook_public_url":""}}"#;
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(raw).unwrap().into();
        assert_eq!(p.webhook_url.as_deref(), Some("https://p.com/h"));
    }

    /// 卖家少给字段时不能整体解析失败，否则面板的余额卡全空
    #[test]
    fn 档案_容忍缺字段与未知字段() {
        let p: ProfileInfo = serde_json::from_str::<ProfileResponse>(r#"{"profile":{}}"#)
            .unwrap()
            .into();
        assert!(p.balance.is_none());
        // 整个 profile 键缺失也不能失败
        let empty: ProfileInfo = serde_json::from_str::<ProfileResponse>("{}").unwrap().into();
        assert!(empty.name.is_none());
    }

    #[test]
    fn 兑换转中立结构() {
        let r: RedeemResult = serde_json::from_str::<RedeemResponse>(r#"{"quota":500,"balance":1900}"#)
            .unwrap()
            .into();
        assert_eq!(r.quota, Some(500.0));
        assert_eq!(r.balance, Some(1900.0));
        // 已用过的码走 404 redeem_invalid（错误分支），不会到这里
        assert!(!r.replayed);
    }

    // ============ 列表类信封 ============

    /// 文档没写这三个列表接口的外层形态，故裸数组与带键对象都要认。
    /// 猜错的后果是面板拿到一条「解析响应失败」而不是列表。
    #[test]
    fn 信封_认裸数组() {
        let raw = r#"[{"id":"o1","client_order_id":"c1","count":2,"charged":50,
            "created_at":"2026-08-01T00:00:00Z"}]"#;
        let env: Envelope<Order> = serde_json::from_str(raw).unwrap();
        let paged: Paged<OrderInfo> = env.map_into();
        assert_eq!(paged.total, Some(1));
        assert_eq!(paged.items[0].order_id.as_deref(), Some("o1"));
        assert_eq!(paged.items[0].purchased, Some(2));
        // 对账用 charged，不是 unit_price × 数量
        assert_eq!(paged.items[0].total_debit, Some(50.0));
    }

    #[test]
    fn 信封_认items与orders两种键名() {
        let by_items = r#"{"items":[{"id":"a"}],"total":9,"limit":20,"offset":40}"#;
        let e1: Envelope<Order> = serde_json::from_str(by_items).unwrap();
        let p1: Paged<OrderInfo> = e1.map_into();
        assert_eq!(p1.total, Some(9));
        assert_eq!(p1.page_size, Some(20));
        // offset 40 / limit 20 = 第 3 页
        assert_eq!(p1.page, Some(3));
        assert_eq!(p1.pages, Some(1), "9 条按每页 20 算是 1 页");

        let by_orders = r#"{"orders":[{"id":"b"},{"id":"c"}]}"#;
        let e2: Envelope<Order> = serde_json::from_str(by_orders).unwrap();
        let p2: Paged<OrderInfo> = e2.map_into();
        assert_eq!(p2.items.len(), 2);
        // 总数未知时按实际条数兜底，且不编一个页数出来
        assert_eq!(p2.total, Some(2));
    }

    /// 空列表不能给出 page_size = 0 —— 前端拿它做除数会出 NaN
    #[test]
    fn 信封_空列表的每页条数不为零() {
        let env: Envelope<Order> = serde_json::from_str("[]").unwrap();
        let paged: Paged<OrderInfo> = env.map_into();
        assert_eq!(paged.total, Some(0));
        assert_eq!(paged.page_size, Some(1));
        assert_eq!(paged.page, Some(1));
    }

    /// 流水的变动类型字段本家叫 reason，其余家叫 type，两个都要接住
    #[test]
    fn 流水_reason与type两种字段名() {
        let by_reason = r#"{"entries":[{"seq":3,"reason":"purchase","amount":-150,
            "balance_after":1850,"memo":"提货","created_at":"2026-08-01T00:00:00Z"}]}"#;
        let e: Envelope<Ledger> = serde_json::from_str(by_reason).unwrap();
        let p: Paged<LedgerEntry> = e.map_into();
        assert_eq!(p.items[0].entry_type.as_deref(), Some("purchase"));
        assert_eq!(p.items[0].amount, Some(-150.0), "支出带负号");
        assert_eq!(p.items[0].balance_after, Some(1850.0));

        let by_type = r#"[{"id":4,"type":"warranty","quota":150}]"#;
        let e2: Envelope<Ledger> = serde_json::from_str(by_type).unwrap();
        let p2: Paged<LedgerEntry> = e2.map_into();
        assert_eq!(p2.items[0].seq, Some(4));
        assert_eq!(p2.items[0].entry_type.as_deref(), Some("warranty"));
        assert_eq!(p2.items[0].amount, Some(150.0));
    }

    /// 密钥列表只给前缀。**不能把前缀塞进 key_value** ——
    /// 那个字段语义是密钥正文，用于与本地凭据池对账，塞前缀会误判成命中
    #[test]
    fn 密钥列表_不把前缀当正文() {
        let raw = r#"{"keys":[{"id":"k1","key_prefix":"ksk_abc","status":"sold",
            "purchased_at":"2026-08-01T00:00:00Z"}]}"#;
        let env: Envelope<MyKey> = serde_json::from_str(raw).unwrap();
        let paged: Paged<VendorKeyInfo> = env.map_into();
        assert_eq!(paged.items[0].id.as_deref(), Some("k1"));
        assert_eq!(paged.items[0].status.as_deref(), Some("sold"));
        assert!(
            paged.items[0].key_value.is_none(),
            "前缀不是正文，塞进去会让对账误判命中"
        );
    }

    /// 三种状态都要能读出来：判断能不能用看 status，不看剩余额度
    #[test]
    fn 密钥列表_三种状态() {
        let raw = r#"[{"id":"a","status":"sold"},{"id":"b","status":"dead"},
            {"id":"c","status":"revoked"}]"#;
        let env: Envelope<MyKey> = serde_json::from_str(raw).unwrap();
        let paged: Paged<VendorKeyInfo> = env.map_into();
        let states: Vec<&str> = paged
            .items
            .iter()
            .filter_map(|k| k.status.as_deref())
            .collect();
        assert_eq!(states, vec!["sold", "dead", "revoked"]);
    }
}
