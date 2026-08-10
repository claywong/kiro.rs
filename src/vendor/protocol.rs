//! 卖家协议抽象：协议风味、能力集与中立数据结构
//!
//! 不同卖家的 HTTP 形态差异很大（路径前缀、鉴权头、字段名、分页信封），但
//! 上层业务（提取入库、事件幂等、失效确认）完全一致。本模块定义**与卖家无关**
//! 的中立结构，由各 flavor 模块负责把自家 DTO 翻译过来，service 层只见中立结构。
//!
//! 新增第三家卖家时的改动面：加一个 `VendorFlavor` 变体 + 一个 `flavor_*.rs`，
//! 其余各层不动。
//!
//! @author wangzhong

use serde::{Deserialize, Serialize};

/// 卖家协议风味。决定路径前缀、鉴权头形态与响应字段映射。
///
/// 反序列化走 [`VendorFlavor::parse`] 的宽松匹配（大小写、点号、下划线都容忍），
/// 且**无法识别时直接报错而非静默回退** —— 拼错的 flavor 名若被当成默认值，
/// 会对着错误的路径和鉴权头发请求，症状是一片 401/404，很难定位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VendorFlavor {
    /// 首家对接的卖家：`/api/my/*` + `X-API-Key: usr-xxx`。
    /// 独有能力：系统状态、开号记录、webhook 远程管理。
    #[default]
    Legacy,
    /// kiroapp.io：`/api/me/*` + `Authorization: Bearer km_xxx`。
    /// 独有能力：积分流水、我的密钥、最早密钥时间；阶梯定价。
    Kiroapp,
    /// kiroapp.cc：`/openapi/*` + `Authorization: Bearer km_xxx`。
    /// 简化版协议：只有库存、余额、提取，无流水、无密钥列表、无阶梯定价。
    KiroappCc,
    /// Kiro Drop（drop.kiro.ss）：`/api/my/*` + `X-API-Key: usr-xxx`。
    /// 与 [`Self::Legacy`] 高度相似（同路径、同下单参数、同事件名），差异只有
    /// 两处：金额是字符串、库存来自 `/api/status` 的 `keys_stock`。
    /// 实测无兑换码 / 开号记录 / 订单列表（均 404）。
    Drop,
    /// kiro-market（api.91kiro.com）：`/api/my/*` + `X-API-Key: usr-xxx`。
    /// 与 [`Self::Legacy`] 同路径同鉴权同下单参数，差异在响应形态：`keys` 是带
    /// 逐张 `paid` 的对象数组（阶梯定价，提货跨车次会混价）、档案套在 `profile`
    /// 键下、余额不在库存接口里。另有质保期内自动退款机制（无需我方动作）。
    Kiromarket,
    /// kiro.red（kiro.red）：与前五家协议**根本不同**，逻辑物理隔离在
    /// [`super::flavor_kirored`]，不共用 [`super::client::VendorClient`] 的
    /// 请求管线。差异：
    /// - 鉴权：email + 密码登录换 JWT（7 天过期，进程级缓存），非静态 Key
    /// - 请求：每个请求带 `X-Signature`（url+method+ts 双重 MD5）与时间戳头
    /// - 响应：`X-Signature-Status: 1` 时 body 是 AES-128-CBC 密文，key/iv 由请求签名派生
    /// - 发货：**无 webhook**，下单即发货，卡密在订单详情 `cards[].content` 里
    /// - 下单：商品（SKU + 积分）模型，需先拉 products 选品再 `POST /user/order/create`
    Kirored,
}

impl VendorFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Kiroapp => "kiroapp",
            Self::KiroappCc => "kiroapp-cc",
            Self::Drop => "drop",
            Self::Kiromarket => "kiromarket",
            Self::Kirored => "kirored",
        }
    }

    /// 宽松解析，便于配置里写 `kiroapp` / `kiroApp` / `kiroapp.io` 都能识别。
    /// 无法识别时返回 None，由调用方决定报错还是回退默认。
    ///
    /// 注意：只有**原本就为空**的输入才回退 Legacy。含非 ASCII 的名字（如中文）
    /// 归一化后也会变空串，这类必须报 None —— 否则拼错的 flavor 名会被静默
    /// 当成首家协议，对着错误的路径和鉴权头发请求。
    pub fn parse(s: &str) -> Option<Self> {
        if s.trim().is_empty() {
            return Some(Self::Legacy);
        }
        let norm: String = s
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match norm.as_str() {
            "legacy" | "my" | "default" => Some(Self::Legacy),
            "kiroapp" | "kiroappio" | "me" => Some(Self::Kiroapp),
            "kiroappcc" | "openapi" => Some(Self::KiroappCc),
            "drop" | "dropkiross" | "kirodrop" => Some(Self::Drop),
            // 归一化会去掉非字母数字，故 `kiro-market` / `api.91kiro.com` 等写法
            // 都能落到这几个别名上
            "kiromarket" | "91kiro" | "kiro91" | "api91kirocom" | "market" => {
                Some(Self::Kiromarket)
            }
            // 归一化去掉非字母数字，故 `kiro.red` / `kiro-red` 等写法都落到这里。
            // 注意不能只用 `kiro` 之类过宽的别名，会与前几家的 `kiroapp` 混淆。
            "kirored" | "kiroredcom" | "red" => Some(Self::Kirored),
            _ => None,
        }
    }

    /// 所有可选值，用于报错时给出提示
    pub fn all_names() -> &'static str {
        "legacy, kiroapp, kiroapp-cc, drop, kiromarket, kirored"
    }

    /// 该风味支持哪些能力。面板据此决定展示或隐藏对应卡片。
    pub fn capabilities(self) -> VendorCapabilities {
        match self {
            Self::Legacy => VendorCapabilities {
                // `/api/status` 的存活 / 失效 / 存货数对本家没有可用数据，关掉这一位，
                // 免得面板上白挂一张空卡（前端按 caps.systemStatus 决定是否渲染）
                system_status: false,
                gen_logs: true,
                webhook_manage: true,
                purchase_orders: true,
                redeem: true,
                ledger: false,
                my_keys: false,
                earliest_key: false,
                batch_scoped_purchase: false,
                tiered_pricing: false,
                // 该卖家库存按 us / eu 分区，各区单价独立、不跨区补货
                zoned_purchase: true,
            },
            Self::Kiroapp => VendorCapabilities {
                system_status: false,
                gen_logs: false,
                // webhook 地址在卖家网页「设置 → Webhook 配置」里填，没有开放 API
                webhook_manage: false,
                purchase_orders: true,
                redeem: true,
                ledger: true,
                my_keys: true,
                earliest_key: true,
                // 推送里带 order_id，可只拉该批次产出的 Key
                batch_scoped_purchase: true,
                // 单价按母号累计产量分档，同一单里各 Key 可能不同价
                tiered_pricing: true,
                // 库存按 us / eu 分区，各区单价独立、不跨区补货。下单必须带 region，
                // 否则卖家只从默认区（us）取货，该区缺货时直接 404 而不会自动换区。
                zoned_purchase: true,
            },
            Self::KiroappCc => VendorCapabilities {
                system_status: false,
                gen_logs: false,
                webhook_manage: false,
                purchase_orders: true,
                redeem: true,
                ledger: false,
                my_keys: false,
                earliest_key: false,
                batch_scoped_purchase: false,
                tiered_pricing: false,
                zoned_purchase: false,
            },
            Self::Drop => VendorCapabilities {
                // `/api/status` 既是系统状态也是本家唯一的库存来源
                system_status: true,
                // 以下四项在本家实测均 404，故关闭，避免面板给出点了报错的按钮
                gen_logs: false,
                purchase_orders: false,
                redeem: false,
                // 有 PUT /api/my/webhook 与测试推送两个接口
                webhook_manage: true,
                ledger: false,
                my_keys: false,
                earliest_key: false,
                batch_scoped_purchase: false,
                tiered_pricing: false,
                zoned_purchase: false,
            },
            Self::Kiromarket => VendorCapabilities {
                // 文档未给 /api/status 之类的系统状态端点
                system_status: false,
                // 有车次概念（GET /api/my/rounds），但那不是「开号批次记录 +
                // 平均间隔」，面板那张卡对不上，不开
                gen_logs: false,
                // GET / PUT /api/my/webhook 与 POST /api/my/webhook/test 都有
                webhook_manage: true,
                purchase_orders: true,
                redeem: true,
                ledger: true,
                my_keys: true,
                // 无「最早密钥时间」接口
                earliest_key: false,
                // 补货推送给的是提货幂等键（purchase_order_id），不是可定向拉取的
                // 批次 id —— 下单只能按区提，不能指定车次
                batch_scoped_purchase: false,
                // 单价按整车产出量查阶梯，且随车次存活时长逐档降价；提货按最早
                // 入库先给、会跨车次，故同一单可能混价，总额只能以卖家返回为准
                tiered_pricing: true,
                // us / eu 严格隔离，不跨区补货。不显式传 zone 时卖家只从美区取，
                // 美区缺货就直接返回缺货
                zoned_purchase: true,
            },
            Self::Kirored => VendorCapabilities {
                // 无 /api/status 之类端点
                system_status: false,
                // 有车次批次概念，但那是商品维度的库存快照，不是「开号记录 + 平均间隔」
                gen_logs: false,
                // 无 webhook —— 发货靠下单后主动查订单详情
                webhook_manage: false,
                // 有 /user/order/index 历史订单
                purchase_orders: true,
                // 站点有兑换码充值，但本次对接只做手动提取，兑换接口暂不实现，
                // 故关掉以与 client 层的 unsupported 保持一致（避免面板给出点了报错的按钮）
                redeem: false,
                // 有积分流水（订单本身即消费流水）——但无独立 ledger 端点，关掉
                ledger: false,
                // 站点有 /user/order/index，但本次对接只做手动提取，
                // 且该端点 DTO 与 kiroapp/kiromarket 不同，未实现映射，关掉
                my_keys: false,
                earliest_key: false,
                batch_scoped_purchase: false,
                // 积分定价，单件商品固定积分价，不是同单混价的阶梯
                tiered_pricing: false,
                // 区编码在卡密 content 里（如 ----us-east-1），不是下单参数；
                // 选区靠运行时挑 health=good 的商品，故不开分区能力
                zoned_purchase: false,
            },
        }
    }
}

/// 手写而非 derive：必须与 [`VendorFlavor::as_str`] 一致输出 `kiroapp-cc`。
///
/// derive + `rename_all = "camelCase"` 会写成 `kiroappCc`，而文档、`all_names()`
/// 的报错提示、`as_str()` 用的都是 `kiroapp-cc`。面板切换提取模式时会把整个
/// config.json 写回（见 `VendorService::set_mode`），两者不一致会让用户手写的
/// `kiroapp-cc` 被悄悄改成另一种拼法。
impl Serialize for VendorFlavor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for VendorFlavor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "无法识别的卖家协议风味 {:?}，可选值: {}",
                raw,
                Self::all_names()
            ))
        })
    }
}

/// 单个卖家支持的能力集。前端据此隐藏无意义的卡片与按钮，
/// 避免对不支持的接口发请求后拿一堆 404 当错误展示。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorCapabilities {
    /// 卖家系统状态（存活 / 失效 / 存货 Key 数）
    pub system_status: bool,
    /// 开号批次记录与平均间隔，用于估算下一批何时到
    pub gen_logs: bool,
    /// 可通过 API 读写卖家侧保存的 webhook URL
    pub webhook_manage: bool,
    /// 历史提取订单列表
    pub purchase_orders: bool,
    /// 兑换码充值
    pub redeem: bool,
    /// 积分流水
    pub ledger: bool,
    /// 我名下的密钥列表
    pub my_keys: bool,
    /// 最早密钥时间（估算账龄）
    pub earliest_key: bool,
    /// 下单时可指定开号批次 id，只拉该批次的 Key
    pub batch_scoped_purchase: bool,
    /// 阶梯定价：同一单里各 Key 单价可能不同，总价需以卖家返回为准
    pub tiered_pricing: bool,
    /// 分区库存：库存按区隔离、各区单价独立，下单需指定 zone
    pub zoned_purchase: bool,
}

/// 单个区域的库存与报价（中立）。
///
/// 仅 `zoned_purchase` 能力的卖家会给。各区**严格隔离、不跨区补货**：
/// 下单不显式指定区时卖家只从它自己的默认区取货，该区缺货就直接返回缺货，
/// 不会用别区的号顶上。因此选区必须由我们主动做，见 [`StockInfo::pick_zone`]。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneStock {
    /// 区域代码，下单时原样回传给卖家（如 `us` / `eu`）
    pub zone: String,
    /// 人类可读名称，如「美国区」。缺失时前端回退显示 `zone`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 本区当前可提取数量（已综合余额、库存与每母号上限）
    pub available: u32,
    /// 本区仓库存货数。可能大于 `available`（受单次上限压制）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stock: Option<u32>,
    /// 本区单价。各区独立设置，**不要按文档硬编码** —— 实测线上与文档不一致。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<f64>,
    /// 本区是否开放。关闭的区即使有存货也提不出来。
    pub enabled: bool,
    /// 发车时间（Unix 秒）。车次制卖家（kiro.red）才有，其余家为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub departed_at: Option<i64>,
    /// 存活时长（秒）。**语义随车次状态而变**：车还活着时是「已存活多久」、
    /// 会随时间增长；车已死时是「总共活了多久」的终值。前端据此展示，
    /// 不要当成「预计还能活多久」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alive_secs: Option<i64>,
    /// 卖家给的存活时长文案（如「26 分钟 46 秒」）。直接用它可避免我们重算
    /// 与卖家口径不一致；缺失时前端按 `alive_secs` 自行格式化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alive_text: Option<String>,
}

/// 库存与报价（中立）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockInfo {
    /// 本轮最大可提取数量。分区卖家这里是**各区之和**，单独看它会误导 ——
    /// 它大于 0 只说明「某个区有货」，不代表默认区有货。
    pub available: u32,
    /// 最低单价。阶梯定价的卖家给区间，单一定价的卖家不给。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_min: Option<f64>,
    /// 最高单价
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_max: Option<f64>,
    /// 账户余额。部分卖家在库存接口里一并给出，可省一次 profile 请求。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    /// 分区库存。空表示该卖家不分区，下单不必带 zone。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<ZoneStock>,
}

impl StockInfo {
    /// 选一个可下单的区：**开放 + 有货，其中单价最低者**。
    ///
    /// 同价时取 `available` 大的，让单次能提的量尽可能多；仍相同则按 `zone`
    /// 字典序，保证结果稳定（否则同一份库存两次调用可能选到不同区，
    /// 幂等重试就会撞上卖家侧的 409）。
    ///
    /// 返回 `None` 有两种情形，调用方无需区分：不分区（zones 为空），
    /// 或所有区都无货。前者应当不带 zone 下单，后者下单必然缺货。
    pub fn pick_zone(&self) -> Option<&ZoneStock> {
        self.zones
            .iter()
            .filter(|z| z.enabled && z.available > 0)
            .min_by(|a, b| {
                // 缺单价的区排在最后：价格未知时不该被优先选中
                let pa = a.unit_price.unwrap_or(f64::INFINITY);
                let pb = b.unit_price.unwrap_or(f64::INFINITY);
                pa.total_cmp(&pb)
                    .then(b.available.cmp(&a.available))
                    .then_with(|| a.zone.cmp(&b.zone))
            })
    }

    /// 按区代码找，用于校验前端传来的 zone 是否真的存在
    pub fn find_zone(&self, zone: &str) -> Option<&ZoneStock> {
        self.zones.iter().find(|z| z.zone == zone)
    }
}

/// 单张提取到的密钥（中立）。
///
/// 除 `key` 外全部可选：首家卖家只给 `key`，kiroapp 额外给 AWS 账号密码与
/// issuer_url。这些附加字段目前只做展示与留痕，不参与凭据入库。
#[derive(Debug, Clone, Deserialize)]
pub struct PurchasedKey {
    pub key: String,
    pub account: Option<String>,
    pub password: Option<String>,
    pub issuer_url: Option<String>,
    /// 这一张实际扣了多少（阶梯定价下同单各不相同）
    pub price: Option<f64>,
}

/// 下单结果（中立）
#[derive(Debug, Clone, Default)]
pub struct PurchaseResult {
    /// 卖家实际出货数
    pub purchased: u32,
    /// 卖家回显的请求数（余额不足时会小于它成交）
    pub requested: Option<u32>,
    /// 提取后卖家侧剩余（首家是账户余额，kiroapp 是剩余库存，语义由 flavor 决定）
    pub remaining: Option<f64>,
    /// 本单实际均价
    pub unit_price: Option<f64>,
    /// 实际扣费总额。阶梯定价下这是唯一权威数字。
    pub total_debit: Option<f64>,
    /// 卖家侧订单 / 批次 id
    pub order_id: Option<String>,
    pub keys: Vec<PurchasedKey>,
    /// true 表示本次是幂等重放，未重复扣款
    pub replayed: bool,
    /// 本单实际成交的区域（卖家回显）。分区卖家必须透出到面板 ——
    /// 否则积分扣了却看不出花在哪个区。
    pub zone: Option<String>,
}

/// 账户档案（中立）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// 当前可用余额 / 积分
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    /// 总配额（首家有，kiroapp 无）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<f64>,
    /// 已用配额
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_quota: Option<f64>,
    /// 单次最小购买数
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_purchase: Option<u32>,
    /// 单次最大购买数。面板提取弹窗据此限制输入上限。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_purchase: Option<u32>,
    /// 卖家侧保存的 webhook URL（仅 `webhook_manage` 能力可用时有意义）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 历史订单（中立）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchased: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_debit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 兑换结果（中立）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemResult {
    /// 本次到账额度
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota: Option<f64>,
    /// 兑换后余额
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_quota: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redeemed_at: Option<String>,
    /// true 表示这张码此前已兑换过，本次未改动余额
    pub replayed: bool,
}

/// 积分流水单条（中立，仅 `ledger` 能力）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// 变动类型，如 `purchase_debit` / `stripe_recharge`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<String>,
    /// 带符号金额
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_after: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 名下密钥单条（中立，仅 `my_keys` 能力）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorKeyInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 密钥明文。仅在需要与本地凭据池对账时使用，不主动展示给前端。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 该 Key 被我买下的时刻
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchased_at: Option<String>,
    /// 该 Key 被开出来的时刻。库存新鲜度只能从这里推断 ——
    /// 卖家的库存接口不给任何时间字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 最早密钥时间（中立，仅 `earliest_key` 能力）。用于估算账龄。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EarliestKeyInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

/// 分页信封（中立）。首家卖家返回裸数组，统一包装成本结构后上层无需分支。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paged<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
}

impl<T> Default for Paged<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            total: None,
            page: None,
            page_size: None,
            pages: None,
        }
    }
}

impl<T> Paged<T> {
    /// 把裸数组包装成单页信封
    pub fn from_vec(items: Vec<T>) -> Self {
        let total = items.len() as u32;
        Self {
            items,
            total: Some(total),
            page: Some(1),
            page_size: Some(total.max(1)),
            pages: Some(1),
        }
    }
}

/// 出站调用失败，携带 HTTP 状态码便于上层按 403/404/409 分别处理
#[derive(Debug)]
pub struct VendorApiError {
    pub status: Option<u16>,
    pub message: String,
}

impl VendorApiError {
    /// 该 flavor 不支持此能力。不发请求直接返回，避免把 404 当成故障展示。
    pub fn unsupported(what: &str) -> Self {
        Self {
            status: None,
            message: format!("该卖家不支持{what}"),
        }
    }
}

impl std::fmt::Display for VendorApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(code) => write!(f, "卖家接口返回 {}: {}", code, self.message),
            None => write!(f, "卖家接口调用失败: {}", self.message),
        }
    }
}

impl std::error::Error for VendorApiError {}

/// 按字符边界截断，避免把多字节 UTF-8 切坏
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_宽松解析() {
        assert_eq!(VendorFlavor::parse("legacy"), Some(VendorFlavor::Legacy));
        assert_eq!(VendorFlavor::parse(""), Some(VendorFlavor::Legacy));
        assert_eq!(VendorFlavor::parse("kiroapp"), Some(VendorFlavor::Kiroapp));
        assert_eq!(VendorFlavor::parse("kiroApp"), Some(VendorFlavor::Kiroapp));
        assert_eq!(
            VendorFlavor::parse("kiroapp.io"),
            Some(VendorFlavor::Kiroapp)
        );
        assert_eq!(VendorFlavor::parse("某家新卖家"), None);
    }

    #[test]
    fn 配置里的风味宽松反序列化() {
        // 序列化后的规范名
        assert_eq!(
            serde_json::to_string(&VendorFlavor::Kiroapp).unwrap(),
            r#""kiroapp""#
        );

        // 各种写法都能读回来
        for raw in [r#""kiroapp""#, r#""kiroApp""#, r#""kiroapp.io""#, r#""KIROAPP""#] {
            assert_eq!(
                serde_json::from_str::<VendorFlavor>(raw).unwrap(),
                VendorFlavor::Kiroapp,
                "解析失败: {raw}"
            );
        }
        assert_eq!(
            serde_json::from_str::<VendorFlavor>(r#""legacy""#).unwrap(),
            VendorFlavor::Legacy
        );
        // 空串按默认（等价于未配置）
        assert_eq!(
            serde_json::from_str::<VendorFlavor>(r#""""#).unwrap(),
            VendorFlavor::Legacy
        );

        // 拼错必须报错而非静默回退，且提示可选值
        let err = serde_json::from_str::<VendorFlavor>(r#""kiroapp2""#).unwrap_err();
        assert!(err.to_string().contains("legacy"), "错误缺少提示: {err}");
    }

    #[test]
    fn 缺省风味为首家() {
        // vendors 项里不写 flavor 时走 serde 默认，等于 legacy
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            flavor: VendorFlavor,
        }
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(h.flavor, VendorFlavor::Legacy);
    }

    #[test]
    fn 能力集按风味区分() {
        let legacy = VendorFlavor::Legacy.capabilities();
        assert!(legacy.gen_logs, "首家有开号记录");
        assert!(legacy.webhook_manage);
        assert!(!legacy.ledger);
        assert!(!legacy.tiered_pricing);

        let kiro = VendorFlavor::Kiroapp.capabilities();
        assert!(!kiro.gen_logs, "kiroapp 没有开号记录接口");
        assert!(!kiro.webhook_manage, "webhook 地址只能在网页里配");
        assert!(kiro.ledger);
        assert!(kiro.earliest_key);
        assert!(kiro.batch_scoped_purchase);
        assert!(kiro.tiered_pricing);
    }

    #[test]
    fn 分页信封包装裸数组() {
        let p = Paged::from_vec(vec![1, 2, 3]);
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.total, Some(3));
        assert_eq!(p.pages, Some(1));

        // 空数组不能给出 page_size = 0，前端拿它做除数会出 NaN
        let empty = Paged::<u8>::from_vec(vec![]);
        assert_eq!(empty.total, Some(0));
        assert_eq!(empty.page_size, Some(1));
    }

    #[test]
    fn truncate_不切坏多字节字符() {
        assert_eq!(truncate("中文测试", 2), "中文…");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn 不支持能力的错误不带状态码() {
        let e = VendorApiError::unsupported("开号记录");
        assert!(e.status.is_none());
        assert!(e.message.contains("开号记录"));
    }
}

/// 本地新增测试单独成块，避免插进上游 `mod tests` 中间引发合并冲突。
#[cfg(test)]
mod local_tests {
    use super::*;

    /// kiroapp.io 的分区能力必须开启。
    ///
    /// 这一位曾经是 false，而解析层（`stock_us` / `stock_eu` → zones）和发送层
    /// （zone → `body["region"]`）都已就位，导致 `resolve_zone` 提前返回 None、
    /// 下单不带 region，卖家只从默认区取货，us 缺货时直接 404 且不跨区补。
    #[test]
    fn kiroapp_具备分区提取能力() {
        assert!(
            VendorFlavor::Kiroapp.capabilities().zoned_purchase,
            "kiroapp.io 库存按 us / eu 分区，下单必须带 region"
        );
    }

    /// 首家的 `/api/status` 拿不到有用的存活 / 失效 / 存货数，这一位要关着，
    /// 否则面板会渲染一张永远是 `—` 的「卖家存货 Key」卡。
    ///
    /// 注意这不影响库存：首家的库存走独立的 `PATH_STOCK`，与本能力无关。
    #[test]
    fn 首家不开系统状态() {
        assert!(!VendorFlavor::Legacy.capabilities().system_status);
        // Drop 家的 /api/status 是它唯一的库存兜底来源，那边必须保持开启
        assert!(VendorFlavor::Drop.capabilities().system_status);
    }

    /// 只有确实分区的卖家才开这一位 —— 误开会让 `resolve_zone` 因 zones 为空
    /// 而报 NoZoneInStock，把本来能提的单挡掉。
    #[test]
    fn 不分区的卖家不开该能力() {
        assert!(!VendorFlavor::KiroappCc.capabilities().zoned_purchase);
        assert!(!VendorFlavor::Drop.capabilities().zoned_purchase);
    }

    // ============ 第五家 kiro-market ============

    /// 各种写法都要能落到同一个变体上。归一化会去掉非字母数字，
    /// 故连字符、点号、大小写都容忍。
    #[test]
    fn kiromarket_宽松解析() {
        for raw in [
            "kiromarket",
            "kiro-market",
            "kiroMarket",
            "KIROMARKET",
            "91kiro",
            "api.91kiro.com",
        ] {
            assert_eq!(
                VendorFlavor::parse(raw),
                Some(VendorFlavor::Kiromarket),
                "解析失败: {raw}"
            );
        }
    }

    /// 序列化形态必须与 `as_str()` / 报错提示一致。
    ///
    /// 面板切换提取模式时会把整个 config.json 写回，两者不一致会让用户手写的
    /// `kiromarket` 被悄悄改成另一种拼法（`kiroapp-cc` 就踩过这个坑）。
    #[test]
    fn kiromarket_序列化形态稳定() {
        assert_eq!(
            serde_json::to_string(&VendorFlavor::Kiromarket).unwrap(),
            r#""kiromarket""#
        );
        assert_eq!(VendorFlavor::Kiromarket.as_str(), "kiromarket");
        // 报错提示里要能看到这个可选值
        assert!(VendorFlavor::all_names().contains("kiromarket"));
    }

    /// 不能与既有四家撞名
    #[test]
    fn kiromarket_不与既有家撞名() {
        assert_ne!(VendorFlavor::parse("kiroapp"), Some(VendorFlavor::Kiromarket));
        assert_ne!(VendorFlavor::parse("drop"), Some(VendorFlavor::Kiromarket));
        assert_ne!(
            VendorFlavor::parse("kiroapp-cc"),
            Some(VendorFlavor::Kiromarket)
        );
    }

    /// 分区能力必须开启：本家 us / eu 严格隔离、不跨区补货，
    /// 不带 zone 下单时卖家只从美区取，美区缺货就直接返回缺货。
    /// 这一位若是 false，`resolve_zone` 会提前返回 None、下单不带 zone。
    #[test]
    fn kiromarket_能力集() {
        let c = VendorFlavor::Kiromarket.capabilities();
        assert!(c.zoned_purchase, "us / eu 严格隔离，下单必须带 zone");
        // 单价按整车产出量查阶梯、且随存活时长降价，提货跨车次会混价
        assert!(c.tiered_pricing, "同一单可能混价，总额只能以卖家返回为准");
        assert!(c.webhook_manage, "有 GET / PUT /api/my/webhook");
        assert!(c.purchase_orders);
        assert!(c.redeem);
        assert!(c.ledger);
        assert!(c.my_keys);
        // 以下三项本家没有对应端点，开了面板会挂空卡或给出点了报错的按钮
        assert!(!c.system_status);
        assert!(!c.gen_logs);
        assert!(!c.earliest_key);
        // 补货推送给的是提货幂等键，不是可定向拉取的批次 id
        assert!(!c.batch_scoped_purchase);
    }

    // ============ 第六家 kiro.red ============

    /// 各种写法都要能落到同一个变体上。归一化去掉非字母数字，
    /// 故连字符、点号、大小写都容忍。
    #[test]
    fn kirored_宽松解析() {
        for raw in ["kirored", "kiro-red", "kiro.red", "KiroRed", "KIRORED"] {
            assert_eq!(
                VendorFlavor::parse(raw),
                Some(VendorFlavor::Kirored),
                "解析失败: {raw}"
            );
        }
    }

    /// 不能与既有五家撞名。尤其 `kiroapp` 前缀相近，必须区分。
    #[test]
    fn kirored_不与既有家撞名() {
        assert_ne!(VendorFlavor::parse("kiroapp"), Some(VendorFlavor::Kirored));
        assert_ne!(VendorFlavor::parse("kiroappcc"), Some(VendorFlavor::Kirored));
        assert_ne!(VendorFlavor::parse("kiromarket"), Some(VendorFlavor::Kirored));
        assert_ne!(VendorFlavor::parse("drop"), Some(VendorFlavor::Kirored));
    }

    /// 序列化形态必须与 as_str / 报错提示一致，否则面板写回 config 会改拼法。
    #[test]
    fn kirored_序列化形态稳定() {
        assert_eq!(
            serde_json::to_string(&VendorFlavor::Kirored).unwrap(),
            r#""kirored""#
        );
        assert_eq!(VendorFlavor::Kirored.as_str(), "kirored");
        assert!(VendorFlavor::all_names().contains("kirored"));
    }

    /// 能力集：无 webhook（靠下单即发货），不分区（区编码在卡密里）。
    #[test]
    fn kirored_能力集() {
        let c = VendorFlavor::Kirored.capabilities();
        assert!(!c.webhook_manage, "kiro.red 无 webhook");
        assert!(!c.zoned_purchase, "区编码在卡密 content 里，不是下单参数");
        assert!(!c.tiered_pricing, "积分定价，单件固定价");
        assert!(!c.redeem, "本次对接不做兑换，与 client 层 unsupported 保持一致");
        assert!(c.purchase_orders, "有历史订单");
        assert!(!c.my_keys, "本次未实现密钥列表映射");
        // 以下几项本家没有对应端点
        assert!(!c.system_status);
        assert!(!c.gen_logs);
        assert!(!c.earliest_key);
        assert!(!c.batch_scoped_purchase);
        assert!(!c.ledger);
    }
}
