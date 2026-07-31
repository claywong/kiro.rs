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
    /// kiroapp.io：`/api/me/*` + `Authorization: Bearer km-xxx`。
    /// 独有能力：积分流水、我的密钥、最早密钥时间；阶梯定价。
    Kiroapp,
    /// kiroapp.cc：`/openapi/*` + `Authorization: Bearer km-xxx`。
    /// 简化版协议：只有库存、余额、提取，无流水、无密钥列表、无阶梯定价。
    KiroappCc,
}

impl VendorFlavor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Kiroapp => "kiroapp",
            Self::KiroappCc => "kiroapp-cc",
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
            _ => None,
        }
    }

    /// 所有可选值，用于报错时给出提示
    pub fn all_names() -> &'static str {
        "legacy, kiroapp, kiroapp-cc"
    }

    /// 该风味支持哪些能力。面板据此决定展示或隐藏对应卡片。
    pub fn capabilities(self) -> VendorCapabilities {
        match self {
            Self::Legacy => VendorCapabilities {
                system_status: true,
                gen_logs: true,
                webhook_manage: true,
                purchase_orders: true,
                redeem: true,
                ledger: false,
                my_keys: false,
                earliest_key: false,
                batch_scoped_purchase: false,
                tiered_pricing: false,
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
}

/// 库存与报价（中立）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockInfo {
    /// 本轮最大可提取数量
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
