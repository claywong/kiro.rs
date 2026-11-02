//! KiroShop 独立配置（config.json 顶层 `kiroshop` 段）
//!
//! 刻意不复用 `vendors` 列表：那套是「ksk_ Key 即时交付」模型，装不下这家
//! 的预定排队、增量拿号、补号与售后。旧版本曾在 `vendors` 里放过一个
//! `flavor: "kiroshop"` 条目，启动时由 [`migrate_from_vendors`] 自动迁走。
//!
//! @author wangzhong

use serde::{Deserialize, Serialize};

use crate::model::config::{Config, VendorConfig};
use crate::vendor::protocol::VendorFlavor;

/// 默认普通档
pub const OPTION_NORMAL: &str = "normal";

fn default_base_url() -> String {
    "https://kiro.yonghu.shop".to_string()
}

fn default_true() -> bool {
    true
}

fn default_rpm_limit() -> u32 {
    5
}

fn default_priority() -> u32 {
    15
}

fn default_events_poll_secs() -> u64 {
    45
}

fn default_delivery_poll_secs() -> u64 {
    60
}

fn default_max_per_order() -> u32 {
    5
}

/// 自动补货规则。默认关；打开后按「池内可用号 + 在途订单 < 目标」补货。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoBuyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 池内可用 KiroShop 号 + 在途数量低于该值时触发补货（0 = 关闭判据）
    #[serde(default)]
    pub pool_target: u32,
    /// 单价超过该值的商品不参与自动补货（None = 不限）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unit_price: Option<f64>,
    /// 单笔订单数量上限（卖家单量限制之外的自我约束）
    #[serde(default = "default_max_per_order")]
    pub max_per_order: u32,
    /// 每日经 API 扣款预算（元；None = 不限，仅受卖家侧每日限额约束）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_budget: Option<f64>,
    /// 现货不足时是否允许转预定排队。自动补货默认拒（deny），避免无人值守挂单。
    #[serde(default)]
    pub allow_backorder: bool,
}

/// KiroShop 顶层配置段
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroshopConfig {
    /// 关闭后停一切出站轮询与自动补货，面板仍可手动操作
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// `ks_live_` 开头的客户 API Key
    #[serde(default)]
    pub api_key: String,
    /// 入站 webhook 路径 token（`/webhook/kiroshop/{token}`），不可猜测即可
    #[serde(default = "default_webhook_path_token")]
    pub webhook_path_token: String,
    /// 入站签名密钥（PUT /webhook 第一次或 rotate 时返回，只显示一次，必须存下来）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub webhook_secret: String,

    // ---- 入库默认值（对齐 admin 的 AddCredentialRequest 语义） ----
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_rpm_limit")]
    pub rpm_limit: u32,
    /// 数字越小越优先
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_limit: Option<f64>,
    /// 跨区账号需要的 apiRegion（如 eu-central-1），空 = 默认
    #[serde(default)]
    pub api_region: String,

    // ---- 轮询与告警 ----
    #[serde(default = "default_events_poll_secs")]
    pub events_poll_interval_secs: u64,
    #[serde(default = "default_delivery_poll_secs")]
    pub delivery_poll_interval_secs: u64,
    /// 余额低于该值时在面板告警（None = 不告警）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance_alert_threshold: Option<f64>,

    #[serde(default)]
    pub auto_buy: AutoBuyConfig,
}

fn default_webhook_path_token() -> String {
    // 每次启动随机（仅当配置里没写时）；保存一次后固定
    format!("ks{:016x}", fastrand::u64(..))
}

/// 手写而非 derive：与 serde 的 `#[serde(default = "default_max_per_order")]`
/// 保持同值（5）。derive Default 会给 maxPerOrder 填 0，迁移与 Default::default()
/// 两条路径都会踩到，语义上「单笔上限 0」再靠运行时 max(1) 兜底太绕。
impl Default for AutoBuyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            pool_target: 0,
            max_unit_price: None,
            max_per_order: default_max_per_order(),
            daily_budget: None,
            allow_backorder: false,
        }
    }
}

impl Default for KiroshopConfig {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }
}

impl KiroshopConfig {
    /// 配置完整可用的最低要求：API Key 非空
    pub fn is_ready(&self) -> bool {
        self.enabled && !self.api_key.trim().is_empty()
    }
}

/// 存量迁移：把 `vendors` 里 `flavor: "kiroshop"` 的条目搬进顶层 `kiroshop` 段。
///
/// 返回是否发生了改动（调用方负责 `config.save()`）。幂等：已有 `kiroshop`
/// 段时不动 `vendors` 里的旧条目 —— 用户可能两个面板都在用，以显式配置为准。
pub fn migrate_from_vendors(config: &mut Config) -> bool {
    if config.kiroshop.is_some() {
        return false;
    }
    let Some(idx) = config
        .vendors
        .iter()
        .position(|v| v.flavor == VendorFlavor::Kiroshop)
    else {
        return false;
    };
    let old: &VendorConfig = &config.vendors[idx];
    let migrated = KiroshopConfig {
        enabled: true,
        base_url: if old.base_url.trim().is_empty() {
            default_base_url()
        } else {
            old.base_url.trim().trim_end_matches('/').to_string()
        },
        api_key: old.api_key.trim().to_string(),
        webhook_path_token: default_webhook_path_token(),
        webhook_secret: String::new(),
        groups: old.default_groups.clone(),
        rpm_limit: old.default_rpm_limit.max(1),
        priority: old.effective_default_priority(),
        credit_limit: None,
        api_region: old.default_api_region.trim().to_string(),
        events_poll_interval_secs: default_events_poll_secs(),
        delivery_poll_interval_secs: default_delivery_poll_secs(),
        balance_alert_threshold: None,
        auto_buy: AutoBuyConfig::default(),
    };
    config.kiroshop = Some(migrated);
    // 移除旧条目：新模块接管后，旧面板再对着同一家下单会绕过在途跟踪与增量拿号
    config.vendors.remove(idx);
    tracing::info!("KiroShop：已把 vendors 里的旧条目迁移到顶层 kiroshop 配置段");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old_vendor_json() -> String {
        r#"{"vendors":[{"id":"kiroshop","name":"KiroShop","flavor":"kiroshop",
            "baseUrl":"https://kiro.yonghu.shop/","apiKey":"ks_live_x",
            "webhookPathToken":"","defaultGroups":["p"],"defaultRpmLimit":5,
            "defaultPriority":15,"defaultApiRegion":"","autoPurchase":false}],
            "autoPurchasePoolTarget":10}"#
            .to_string()
    }

    #[test]
    fn 缺省字段全部有默认值() {
        let c: KiroshopConfig = serde_json::from_str(r#"{"apiKey":"ks_live_x"}"#).unwrap();
        assert_eq!(c.base_url, "https://kiro.yonghu.shop");
        assert!(c.enabled);
        assert_eq!(c.rpm_limit, 5);
        assert_eq!(c.priority, 15);
        assert_eq!(c.events_poll_interval_secs, 45);
        assert!(!c.auto_buy.enabled);
        assert!(!c.webhook_path_token.is_empty());
        // serde 路径与 Default 路径必须同值（迁移走 Default，曾出现过 0 与 5 分叉）
        assert_eq!(c.auto_buy.max_per_order, 5);
        assert_eq!(AutoBuyConfig::default().max_per_order, 5);
    }

    #[test]
    fn 迁移搬走旧条目并归一字段() {
        let mut config: Config = serde_json::from_str(&old_vendor_json()).unwrap();
        assert!(migrate_from_vendors(&mut config));
        let ks = config.kiroshop.as_ref().unwrap();
        assert_eq!(ks.api_key, "ks_live_x");
        assert_eq!(ks.base_url, "https://kiro.yonghu.shop"); // 去掉尾斜杠
        assert_eq!(ks.groups, vec!["p".to_string()]);
        assert!(config.vendors.is_empty(), "旧条目应被移除");
        // 幂等：再跑一次不再改
        assert!(!migrate_from_vendors(&mut config));
    }

    #[test]
    fn 已有配置段时不迁移() {
        let raw = old_vendor_json();
        let vendors_body = &raw[raw.find('[').unwrap()..=raw.rfind(']').unwrap()];
        let mut config: Config = serde_json::from_str(&format!(
            "{{\"kiroshop\":{{\"apiKey\":\"ks_live_new\"}},\"vendors\":{vendors_body}}}"
        ))
        .unwrap();
        // vendors 里仍有 kiroshop 条目，但 kiroshop 段已存在 → 不动
        assert!(!migrate_from_vendors(&mut config));
        assert_eq!(config.vendors.len(), 1);
        assert_eq!(
            config.kiroshop.as_ref().unwrap().api_key,
            "ks_live_new",
            "已有段优先于旧条目"
        );
    }
}
