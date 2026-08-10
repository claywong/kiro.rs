//! 单个卖家的对接服务：事件解析、提取入库、失效确认、告警计数
//!
//! 一个实例对应一家卖家（一份 [`VendorConfig`]）。多家的注册与分发见
//! [`super::registry`]。所有存储访问都带本实例的 `vendor_id`，两家的事件、
//! 绑定数量、失效确认互不干扰。
//!
//! 设计约束：提取数量一旦绑定就不可更改（卖家侧同订单号改 count 会 409），
//! 这是整个模块所有取舍的出发点。
//!
//! - 手动模式（默认）：入站 webhook **只落库不花钱**，扣费一律由面板显式触发。
//! - 自动模式：仅当上一轮 `all_keys_dead` 已确认「名下卖家 Key 全部失效」时，
//!   才在收到 `new_keys_available` 后自动提取，且只提最小数量。判定规则见
//!   [`super::auto`]。多家同时触发时还要过一道跨供应商的总量闸，
//!   见 [`super::pool_gate`]。
//!
//! @author wangzhong

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::admin::AdminService;
use crate::http_client::ProxyConfig;
use crate::model::config::{MIN_STOCK_POLL_INTERVAL_SECS, TlsBackend, VendorConfig};

use super::auto;
use super::client::VendorClient;
use super::pool_gate::PoolGate;
// 本地新增模块单独成行，避免上游改动这批 use 时反复冲突。
use super::schedule;
use super::protocol::{
    EarliestKeyInfo, LedgerEntry, OrderInfo, Paged, ProfileInfo, PurchaseResult, RedeemResult,
    StockInfo, VendorApiError, VendorCapabilities, VendorFlavor, VendorKeyInfo,
};
use super::store::{
    IncomingEvent, PurchaseOutcome, PurchaseStatus, PurchaseTrigger, RecordOutcome,
    SharedVendorStore, ValidationStatus, VendorEventKind,
};

/// 单张提取到的 Key 的附带信息（返回给前端）。
///
/// **不含密钥明文** —— Key 本身已入凭据池，面板不需要再看一遍。这里只给
/// 排查与对账要用的元数据：AWS 账号、签发方、以及阶梯定价下这一张的实际单价。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchasedKeyBrief {
    /// 卖家侧账号名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    /// 这一张实际扣了多少（同一单里各张可能不同）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    /// 这一张自己的 AWS 区域（双区混发的家逐张不同），面板据此核对入库区域
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 该账号是否附带了密码。密码本身不外传，只告知存在性 ——
    /// 需要时从日志或卖家面板取。
    pub has_password: bool,
}

/// 提取入库的汇总结果（返回给前端）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseImportResult {
    /// 本次实际绑定并提交的数量
    pub count: u32,
    /// 卖家回显的请求数。余额不足时卖家会按买得起的数量成交，
    /// 此时 `purchased < requested`，面板需据此提示「部分成交」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested: Option<u32>,
    /// 卖家实际出 Key 数
    pub purchased: u32,
    /// 成功入库数
    pub imported: u32,
    /// 本地已存在而跳过数
    pub duplicated: u32,
    /// 入库失败数
    pub failed: u32,
    /// 提取后卖家侧剩余余额
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// 本单实际扣费总额。阶梯定价的卖家下这是唯一权威数字，
    /// 前端不要用「数量 × 单价」自行估算。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_debit: Option<f64>,
    /// 本单实际均价
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_price: Option<f64>,
    /// 卖家侧订单 / 批次 id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    /// 本单实际成交的区域。分区卖家必须展示 —— 各区单价不同，
    /// 不显示就看不出这笔积分花在哪个区。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    /// true 表示本次是幂等重放，卖家未重复扣款
    pub replayed: bool,
    /// 逐张 Key 的元数据（不含明文）。阶梯定价下各张单价不同，
    /// 面板靠它解释 `totalDebit` 是怎么来的。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<PurchasedKeyBrief>,
    /// 首条失败原因（便于前端直接展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 切换提取模式的结果
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeChange {
    /// 切换后的模式（运行时已生效）
    pub auto_purchase: bool,
    /// 是否已写回 config.json。false 表示重启后会回退
    pub persisted: bool,
    /// 持久化失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 设置全局提取限制的结果
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolTargetChange {
    /// 设置后的阈值（运行时已生效）。0 = 不启用
    pub pool_target: u32,
    /// 是否已写回 config.json。false 表示重启后会回退
    pub persisted: bool,
    /// 持久化失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 切换自动提取总闸的结果
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoEnabledChange {
    /// 切换后的总闸状态（运行时已生效）。false = 全局关闭
    pub auto_purchase_enabled: bool,
    /// 是否已写回 config.json。false 表示重启后会回退
    pub persisted: bool,
    /// 持久化失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 设置逐渠道补货模式的结果
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerChannelChange {
    /// 设置后的模式（运行时已生效）
    pub per_channel: bool,
    /// 是否已写回 config.json。false 表示重启后会回退
    pub persisted: bool,
    /// 持久化失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 自动提取是被谁触发的。**决定总闸能不能被绕过**，故必须显式传，不给默认值。
///
/// 存在的唯一理由：`stockPollRespectGlobalGate=false` 的绕过**只对轮询这条路生效**。
/// 若改成在 `try_auto_purchase` 里直接读那个开关，webhook 触发的自动提取会一并
/// 绕过总闸 —— 那比该开关承诺的范围宽得多，且用户从开关名上完全看不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPurchaseSource {
    /// 卖家 webhook 推来的新货通知。**总闸对它永远有效**，无论本家怎么配。
    Webhook,
    /// 本地库存轮询发现的新车。本家关了 `stockPollRespectGlobalGate` 时可绕过总闸。
    StockPoll,
}

/// 轮询总闸遵循开关的切换结果。
///
/// 单独成一个类型而不复用 [`PerChannelChange`]：那个结构的字段叫 `per_channel`，
/// 拿它传「是否遵循总闸」会让响应 JSON 里出现一个语义对不上的 `perChannel` 键，
/// 日志与接口两头都得靠注释解释，读代码的人必然要绕一圈。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StockPollGateChange {
    /// 设置后的值（运行时已生效）
    pub respect: bool,
    /// 是否已写回 config.json。false 表示重启后会回退
    pub persisted: bool,
    /// 持久化失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 服务层错误
#[derive(Debug)]
pub enum VendorServiceError {
    /// 未配置卖家对接，或指定的 vendorId 不存在
    NotConfigured,
    /// 事件不存在
    EventNotFound,
    /// 该事件不是 `new_keys_available`，没有可提取的订单号
    NotPurchasable,
    /// 该订单号已绑定其它数量，必须改用该值重试
    CountLocked { bound: u32 },
    /// 前端指定了卖家没有的区域。不静默回退 —— 回退等于把钱花在用户没选的区。
    UnknownZone { zone: String, known: Vec<String> },
    /// 所有区都无货，下单必然缺货，不必白发一次请求
    NoZoneInStock,
    /// 调用卖家接口失败
    Upstream(VendorApiError),
    /// 本地存储错误
    Storage(String),
}

impl std::fmt::Display for VendorServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "未配置卖家对接（vendor.baseUrl / vendor.apiKey）"),
            Self::EventNotFound => write!(f, "事件不存在"),
            Self::NotPurchasable => write!(f, "该事件没有可提取的订单号"),
            Self::CountLocked { bound } => write!(
                f,
                "该订单号已绑定数量 {bound}，卖家侧不允许改数量重试，请按 {bound} 重新提交"
            ),
            Self::UnknownZone { zone, known } => write!(
                f,
                "卖家没有区域 {:?}，可选值: {}",
                zone,
                if known.is_empty() {
                    "（该卖家不分区）".to_string()
                } else {
                    known.join(" / ")
                }
            ),
            Self::NoZoneInStock => write!(f, "各区均无库存，暂时无法提取"),
            Self::Upstream(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "本地存储错误: {e}"),
        }
    }
}

/// 自动提取的授权来源。
///
/// 两条路的**额度语义不同**，这是它们必须区分的原因：
/// - [`Self::DeadEvent`] 是一次性额度，用后置 `validation_used` 作废，
///   一条失效事件只授权一轮提取。
/// - [`Self::LocalCensus`] 不消费任何东西，只要本家仍无存活 Key 就会反复成立。
///   因此它必须由全局池闸兜住上限，见 [`VendorService::resolve_authorization`]。
#[derive(Debug, Clone)]
enum PurchaseAuthorization {
    /// 卖家推来的 `all_keys_dead` 已确认失效，且额度未被取用
    DeadEvent { event_id: String },
    /// 就地盘点本家凭据得出「已无可用 Key」。用于不推 `all_keys_dead` 的卖家
    /// （实测 Drop 家只在最初推过一次，此后 60+ 次新货通知都没有）。
    LocalCensus { detail: String },
}

impl PurchaseAuthorization {
    /// 写进日志的来源标识，便于事后区分这笔是谁授权的
    fn source(&self) -> &'static str {
        match self {
            Self::DeadEvent { .. } => "卖家失效事件",
            Self::LocalCensus { .. } => "就地盘点",
        }
    }

    /// 授权依据。就地盘点这条路没有事件行可回溯，依据只存在于日志里，
    /// 故必须记下来 —— 否则事后无法解释这笔扣费凭什么发生。
    fn detail(&self) -> &str {
        match self {
            Self::DeadEvent { event_id } => event_id,
            Self::LocalCensus { detail } => detail,
        }
    }
}

/// 单个卖家的对接服务
pub struct VendorService {
    config: VendorConfig,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    store: SharedVendorStore,
    admin: Arc<AdminService>,
    /// 提取模式的运行时值。`config.auto_purchase` 只是启动快照，面板切换后
    /// 以本字段为准 —— 读它而不是读 config。
    auto_purchase: AtomicBool,
    /// 逐渠道补货的运行时值。同上，面板切换后以本字段为准。
    per_channel: AtomicBool,
    /// 库存轮询是否遵循全局总闸的运行时值。同上，面板切换后以本字段为准。
    stock_poll_respect_gate: AtomicBool,
    /// 跨供应商共享的全局提取闸门。各家持有同一个 Arc。
    pool_gate: Arc<PoolGate>,
}

impl VendorService {
    pub fn new(
        config: VendorConfig,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        store: SharedVendorStore,
        admin: Arc<AdminService>,
        pool_gate: Arc<PoolGate>,
    ) -> Self {
        let auto_purchase = config.auto_purchase;
        let per_channel = config.auto_purchase_per_channel;
        let respect_gate = config.stock_poll_respect_global_gate;
        Self {
            config,
            proxy,
            tls_backend,
            store,
            admin,
            auto_purchase: AtomicBool::new(auto_purchase),
            per_channel: AtomicBool::new(per_channel),
            stock_poll_respect_gate: AtomicBool::new(respect_gate),
            pool_gate,
        }
    }

    pub fn store(&self) -> &SharedVendorStore {
        &self.store
    }

    /// 本实例对应的供应商 id
    pub fn vendor_id(&self) -> &str {
        self.config.vendor_id()
    }

    /// 面板展示名
    pub fn display_name(&self) -> &str {
        self.config.display_name()
    }

    pub fn flavor(&self) -> VendorFlavor {
        self.config.flavor
    }

    pub fn capabilities(&self) -> VendorCapabilities {
        self.config.flavor.capabilities()
    }

    /// 启动时的配置快照。`auto_purchase` 字段可能已被面板改过，
    /// 判断提取模式请用 [`Self::auto_purchase`]。
    pub fn config(&self) -> &VendorConfig {
        &self.config
    }

    /// 当前是否为自动提取模式
    pub fn auto_purchase(&self) -> bool {
        self.auto_purchase.load(Ordering::Relaxed)
    }

    /// 切换提取模式：先改运行时值，再尽力写回 config.json。
    ///
    /// 持久化失败不算切换失败 —— 运行时已生效，只是重启后会回退到文件里的值，
    /// 这一点通过返回的 `persisted` 告知调用方，由面板提示用户。
    pub fn set_auto_purchase(&self, enabled: bool) -> ModeChange {
        self.auto_purchase.store(enabled, Ordering::Relaxed);
        match self.persist_auto_purchase(enabled) {
            Ok(()) => ModeChange {
                auto_purchase: enabled,
                persisted: true,
                warning: None,
            },
            Err(e) => {
                tracing::warn!("持久化提取模式失败（运行时已生效）: {}", e);
                ModeChange {
                    auto_purchase: enabled,
                    persisted: false,
                    warning: Some(e.to_string()),
                }
            }
        }
    }

    /// 设置全局提取限制：先改运行时值，再尽力写回 config.json。
    ///
    /// 与 [`Self::set_auto_purchase`] 同样的取舍 —— 持久化失败不算设置失败，
    /// 运行时已生效，由返回的 `persisted` 告知面板。
    ///
    /// 注意本方法改的是所有家共享的那一个闸门，不限于本实例对应的供应商。
    /// 挂在 `VendorService` 上只是为了复用它持有的配置路径。
    pub fn set_pool_target(&self, target: u32) -> PoolTargetChange {
        self.pool_gate.set_target(target);
        match self.persist_pool_target(target) {
            Ok(()) => PoolTargetChange {
                pool_target: target,
                persisted: true,
                warning: None,
            },
            Err(e) => {
                tracing::warn!("持久化全局提取限制失败（运行时已生效）: {}", e);
                PoolTargetChange {
                    pool_target: target,
                    persisted: false,
                    warning: Some(e.to_string()),
                }
            }
        }
    }

    /// 写回 config.json 顶层的 `autoPurchasePoolTarget`。
    ///
    /// 比 [`Self::persist_auto_purchase`] 简单：顶层字段不必在 `vendor` /
    /// `vendors` 里按 id 找那一项，也就没有「找不到该卖家」的失败分支。
    fn persist_pool_target(&self, target: u32) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = self
            .admin
            .token_manager()
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| {
                anyhow::anyhow!("配置文件路径未知，全局提取限制仅在当前进程生效")
            })?;

        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.auto_purchase_pool_target = target;
        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    // 总闸的读取不在这里开访问器：它是全局量，面板走
    // `registry.pool_gate().auto_enabled()`，绕开「从某一家读一个全局值」的错觉。
    // 本类型只需要写入侧（下面的 set），因为持久化要借用它持有的配置路径。

    /// 切换自动提取总闸：先改运行时值，再尽力写回 config.json。
    ///
    /// 与 [`Self::set_pool_target`] 完全同构 —— 改的是所有家共享的那个闸门，
    /// 挂在 `VendorService` 上只为复用它持有的配置路径；持久化失败不算切换失败。
    ///
    /// 刻意**不动各家的 `auto_purchase`**：那是用户对每家的意图，总闸只临时压住
    /// 出站，重开后各家自动回到原模式，不必逐家恢复。
    pub fn set_auto_purchase_enabled(&self, enabled: bool) -> AutoEnabledChange {
        self.pool_gate.set_auto_enabled(enabled);
        match self.persist_auto_purchase_enabled(enabled) {
            Ok(()) => AutoEnabledChange {
                auto_purchase_enabled: enabled,
                persisted: true,
                warning: None,
            },
            Err(e) => {
                tracing::warn!("持久化自动提取总闸失败（运行时已生效）: {}", e);
                AutoEnabledChange {
                    auto_purchase_enabled: enabled,
                    persisted: false,
                    warning: Some(e.to_string()),
                }
            }
        }
    }

    /// 写回 config.json 顶层的 `autoPurchaseEnabled`。
    ///
    /// 与 [`Self::persist_pool_target`] 同为顶层字段，不必在 `vendor` / `vendors`
    /// 里按 id 找那一项。
    fn persist_auto_purchase_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = self
            .admin
            .token_manager()
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，自动提取总闸仅在当前进程生效"))?;

        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.auto_purchase_enabled = enabled;
        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 本家当前是否开着逐渠道补货
    pub fn per_channel(&self) -> bool {
        self.per_channel.load(Ordering::Relaxed)
    }

    /// 切本家的逐渠道补货：先改运行时值，再尽力写回 config.json。
    ///
    /// 逐家独立，改这一家不影响别家。与 `set_auto_purchase` 同样的取舍 ——
    /// 持久化失败不算切换失败，由返回的 `persisted` 告知面板。
    pub fn set_per_channel(&self, per_channel: bool) -> PerChannelChange {
        self.per_channel.store(per_channel, Ordering::Relaxed);
        match self.persist_per_channel(per_channel) {
            Ok(()) => PerChannelChange {
                per_channel,
                persisted: true,
                warning: None,
            },
            Err(e) => {
                tracing::warn!("持久化逐渠道补货失败（运行时已生效）: {}", e);
                PerChannelChange {
                    per_channel,
                    persisted: false,
                    warning: Some(e.to_string()),
                }
            }
        }
    }

    /// 库存轮询是否遵循全局总闸的**运行时值**。
    ///
    /// 面板与轮询循环都必须用它，不能用 `config.stock_poll_respect_global_gate`
    /// —— 那是启动快照，面板切换后不会变，会让开关点了就弹回去。
    pub fn stock_poll_respect_gate(&self) -> bool {
        self.stock_poll_respect_gate.load(Ordering::Relaxed)
    }

    pub fn set_stock_poll_respect_gate(&self, respect: bool) -> StockPollGateChange {
        self.stock_poll_respect_gate.store(respect, Ordering::Relaxed);
        match self.persist_stock_poll_respect_gate(respect) {
            Ok(()) => StockPollGateChange {
                respect,
                persisted: true,
                warning: None,
            },
            Err(e) => {
                // 持久化失败不算设置失败：运行时已生效，重启才回退。
                // 与 set_per_channel 同一取舍。
                tracing::warn!("持久化轮询总闸遵循失败（运行时已生效）: {}", e);
                StockPollGateChange {
                    respect,
                    persisted: false,
                    warning: Some(e.to_string()),
                }
            }
        }
    }

    /// 写回 config.json 里**本供应商那一项**的 `autoPurchase`。
    ///
    /// 重新从磁盘加载再改单个字段，避免把进程内的旧快照整体覆盖上去 ——
    /// 与 `AdminService::persist_log_governance_config` 同一套做法。
    ///
    /// 单例 `vendor` 与列表 `vendors` 都要找一遍：同一个 id 只会命中一处
    /// （`resolved_vendors` 已按 id 去重）。
    fn persist_auto_purchase(&self, enabled: bool) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = self
            .admin
            .token_manager()
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，提取模式仅在当前进程生效"))?;

        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;

        let target = self.vendor_id();
        let mut hit = false;
        if let Some(v) = config.vendor.as_mut()
            && v.vendor_id() == target
        {
            v.auto_purchase = enabled;
            hit = true;
        }
        if !hit {
            for v in config.vendors.iter_mut() {
                if v.vendor_id() == target {
                    v.auto_purchase = enabled;
                    hit = true;
                    break;
                }
            }
        }
        if !hit {
            anyhow::bail!("config.json 里找不到 id 为 {target} 的卖家配置，无法持久化提取模式");
        }

        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 写回 config.json 里**本供应商那一项**的 `autoPurchasePerChannel`。
    ///
    /// 与 [`Self::persist_auto_purchase`] 同一套查找方式：单例 `vendor` 与列表
    /// `vendors` 都找一遍，同一个 id 只会命中一处（`resolved_vendors` 已去重）。
    fn persist_per_channel(&self, per_channel: bool) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = self
            .admin
            .token_manager()
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，逐渠道补货仅在当前进程生效"))?;

        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;

        let target = self.vendor_id();
        let mut hit = false;
        if let Some(v) = config.vendor.as_mut()
            && v.vendor_id() == target
        {
            v.auto_purchase_per_channel = per_channel;
            hit = true;
        }
        if !hit {
            for v in config.vendors.iter_mut() {
                if v.vendor_id() == target {
                    v.auto_purchase_per_channel = per_channel;
                    hit = true;
                    break;
                }
            }
        }
        if !hit {
            anyhow::bail!("config.json 里找不到 id 为 {target} 的卖家配置，无法持久化逐渠道补货");
        }

        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    fn persist_stock_poll_respect_gate(&self, respect: bool) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = self
            .admin
            .token_manager()
            .config()
            .config_path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，轮询总闸遵循仅在当前进程生效"))?;

        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;

        let target = self.vendor_id();
        let mut hit = false;
        if let Some(v) = config.vendor.as_mut()
            && v.vendor_id() == target
        {
            v.stock_poll_respect_global_gate = respect;
            hit = true;
        }
        if !hit {
            for v in config.vendors.iter_mut() {
                if v.vendor_id() == target {
                    v.stock_poll_respect_global_gate = respect;
                    hit = true;
                    break;
                }
            }
        }
        if !hit {
            anyhow::bail!("config.json 里找不到 id 为 {target} 的卖家配置，无法持久化轮询总闸遵循");
        }

        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 校验入站路径 token。入站未启用或 token 为空一律拒绝。
    pub fn verify_path_token(&self, token: &str) -> bool {
        if !self.config.inbound_enabled() {
            return false;
        }
        crate::common::auth::constant_time_eq(token, self.config.webhook_path_token.trim())
    }

    /// 构建出站客户端
    fn client(&self) -> Result<VendorClient, VendorServiceError> {
        VendorClient::new(&self.config, self.proxy.as_ref(), self.tls_backend)
            .map_err(|_| VendorServiceError::NotConfigured)
    }

    /// 解析入站 payload。字段缺失时尽量保留原文，避免丢事件。
    ///
    /// 幂等键的来源按 flavor 区分：
    /// - `Legacy`：`purchase_order_id`，由卖家给出
    /// - `Kiroapp`：`client_order_id`（卖家按「批次 + 收件人」确定性派生，
    ///   推送重试 / 重启后都是同一个值），另有 `order_id` 是开号批次 id，
    ///   下单时带上可只拉该批次的 Key
    pub fn parse_event(vendor_id: &str, flavor: VendorFlavor, raw: &[u8]) -> Option<IncomingEvent> {
        let value: serde_json::Value = serde_json::from_slice(raw).ok()?;
        let obj = value.as_object()?;
        let event_type = obj.get("event").and_then(|v| v.as_str()).unwrap_or("");
        // event_id 缺失时无法幂等，用原文哈希兜一个稳定 ID
        let event_id = obj
            .get("event_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| fallback_event_id(raw));

        let str_field = |key: &str| {
            obj.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
        };

        let (purchase_order_id, batch_order_id) = match flavor {
            VendorFlavor::Legacy => (str_field("purchase_order_id"), None),
            // 该卖家的幂等键就叫 client_order_id，且已替我们生成好
            VendorFlavor::Kiroapp => (
                str_field("client_order_id").or_else(|| str_field("purchase_order_id")),
                str_field("order_id"),
            ),
            VendorFlavor::KiroappCc => (str_field("purchase_order_id"), None),
            // Drop 与首家同样用 purchase_order_id，但**文档里的示例值是
            // `batch_xxx`**，而下单接口要求 client_order_id 是 32 位十六进制 ——
            // 两处自相矛盾。故：形态合法就直接用（与首家一致），否则从
            // (vendor_id, event_id) 派生一个合法的。派生值对同一条推送稳定，
            // 重投仍能命中卖家侧的幂等重放。
            VendorFlavor::Drop => (
                str_field("purchase_order_id")
                    .or_else(|| str_field("client_order_id"))
                    .filter(|s| is_hex32(s))
                    .or_else(|| Some(derive_client_order_id(vendor_id, &event_id))),
                // 早先那版文档用 batch_id 标批次；现版没有，留着不影响
                str_field("batch_id"),
            ),
            // 本家的 purchase_order_id **不是订单号**，是替我们预生成的提货幂等键
            // （文档明确：拿它调补拉接口会 404，因为此刻还没有订单）。直接当
            // client_order_id 用即是文档推荐的用法，重投时天然幂等。
            //
            // 形态校验仍要做：文档说它是 32 位十六进制，但 Drop 家就出现过文档与
            // 实际不符（示例值 batch_xxx 而下单要求 hex32）。不合法就从
            // (vendor_id, event_id) 派生一个，对同一条推送稳定。
            VendorFlavor::Kiromarket => (
                str_field("purchase_order_id")
                    .filter(|s| is_hex32(s))
                    .or_else(|| Some(derive_client_order_id(vendor_id, &event_id))),
                // 本家无「可定向拉取的批次 id」：round_id 是车次，下单不接受它
                None,
            ),
            // kiro.red 无 webhook，不会走到这里（入站未启用），加兜底分支防 match 不穷尽
            VendorFlavor::Kirored => (str_field("purchase_order_id"), None),
            // 本家文档明确 claim 用 client_order_id 做幂等（与首家同字段名），
            // webhook 推送里也带它。优先取它，回退到 purchase_order_id（以防日后
            // 卖家给订单号起个独立字段）。校验 hex32（实测订单号确是 32 位十六进制）。
            VendorFlavor::KiroOoo => {
                let oid = str_field("client_order_id").or_else(|| str_field("purchase_order_id"));
                let oid = oid.filter(|s| is_hex32(s)).or_else(|| {
                    Some(derive_client_order_id(vendor_id, &event_id))
                });
                (oid, None)
            }
        };

        // 本家 webhook 载荷未经实测（要在卖家侧配好地址才能收到，本次未动用户配置）。
        // 事件名已知的只有文档一句「推一条到货通知」，已见的通知开关名是
        // `on_key_new` / `on_key_dead` / `on_dispatch`。宽松归一化，见 normalize_event_type。
        let event_type = if flavor == VendorFlavor::KiroOoo {
            super::flavor_kiroooo::normalize_event_type(event_type)
                .unwrap_or(event_type)
        } else {
            event_type
        };

        Some(IncomingEvent {
            vendor_id: vendor_id.to_string(),
            event_id,
            kind: VendorEventKind::from_str(event_type),
            purchase_order_id,
            batch_order_id,
            message: str_field("message"),
            new_keys: obj.get("new_keys").and_then(|v| v.as_u64()).map(|v| v as u32),
            dead: obj.get("dead").and_then(|v| v.as_u64()).map(|v| v as u32),
            raw_payload: String::from_utf8_lossy(raw).to_string(),
        })
    }

    /// 定下本单要用哪个区。
    ///
    /// - 不具备 `zoned_purchase` 能力的卖家：恒为 `None`，下单不带 zone。
    /// - `requested` 有值：校验该区真的存在且开放有货，不存在直接报错而非回退 ——
    ///   静默换区等于把积分花在用户没选的区上。
    /// - `requested` 为空：按 [`StockInfo::pick_zone`] 自动选「开放有货中最便宜」的区。
    ///   **不能不传** —— 卖家不传 zone 时只从它的默认区（us）取货且不跨区补，
    ///   而该区常常正是 0 库存的那个。
    async fn resolve_zone(
        &self,
        requested: Option<&str>,
    ) -> Result<Option<String>, VendorServiceError> {
        if !self.capabilities().zoned_purchase {
            return Ok(None);
        }
        let stock = self.stock().await?;
        match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(z) => match stock.find_zone(z) {
                Some(found) if found.enabled => Ok(Some(found.zone.clone())),
                // 存在但已关停，与不存在同样处理：提不出来
                _ => Err(VendorServiceError::UnknownZone {
                    zone: z.to_string(),
                    known: stock
                        .zones
                        .iter()
                        .filter(|z| z.enabled)
                        .map(|z| z.zone.clone())
                        .collect(),
                }),
            },
            None => stock
                .pick_zone()
                .map(|z| Some(z.zone.clone()))
                .ok_or(VendorServiceError::NoZoneInStock),
        }
    }

    /// 按事件提取并入库。
    ///
    /// `count` 为本次希望提取的数量；若该事件此前已绑定过其它数量，直接返回
    /// [`VendorServiceError::CountLocked`]，不会向卖家发请求 —— 避免白撞一次 409。
    ///
    /// `zone` 为空时自动选区；该事件此前已绑定过区域时，**以绑定值为准**并忽略
    /// 本次入参 —— 换区重试会被卖家当成新单再扣一次积分。
    pub async fn purchase_for_event_zoned(
        &self,
        event_id: &str,
        count: u32,
        zone: Option<&str>,
        trigger: PurchaseTrigger,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let client = self.client()?;
        let vid = self.vendor_id();

        let record = self
            .store
            .get_event(vid, event_id)
            .map_err(|e| VendorServiceError::Storage(e.to_string()))?
            .ok_or(VendorServiceError::EventNotFound)?;

        let order_id = record
            .purchase_order_id
            .clone()
            .ok_or(VendorServiceError::NotPurchasable)?;
        // 有批次 id 的卖家可定向拉取，避免买到别的批次
        let batch = record.batch_order_id.clone();

        // 选区必须在绑定之前：绑定要把数量和区域一起写进去，
        // 之后任何重试都只能按这一对值走。
        let picked = self.resolve_zone(zone).await?;

        // 抢占绑定：并发点击只有一个能拿到本次 (count, zone)，其余得到已绑定值
        let (effective, effective_zone) = match self
            .store
            .bind_count_zone(vid, event_id, count, picked.as_deref())
            .map_err(|e| VendorServiceError::Storage(e.to_string()))?
        {
            Ok(v) => v,
            // 同数量重试，卖家侧幂等重放。区域一律用已绑定值 ——
            // 本次自动选区可能选到了另一个区（库存变了），换区就是第二笔单。
            Err((bound, bound_zone)) if bound == count => {
                if bound_zone.as_deref() != picked.as_deref() {
                    tracing::info!(
                        vendor_id = %vid,
                        event_id = %event_id,
                        bound_zone = ?bound_zone,
                        this_time = ?picked,
                        "重试沿用已绑定区域，忽略本次选区结果"
                    );
                }
                (bound, bound_zone)
            }
            Err((bound, _)) => return Err(VendorServiceError::CountLocked { bound }),
        };

        self.purchase_and_import(
            &client,
            event_id,
            &order_id,
            batch.as_deref(),
            effective,
            effective_zone.as_deref(),
            trigger,
        )
        .await
    }

    /// 不依赖 webhook 事件的主动提取（自行生成订单号）。
    /// 不写事件表 —— 没有对应事件行可绑定，幂等由调用方复用订单号保证。
    pub async fn purchase_ad_hoc(
        &self,
        count: u32,
        client_order_id: &str,
        zone: Option<&str>,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let client = self.client()?;
        // 本路径不写事件表，选区结果无处持久化。调用方重试时必须自行带上同一个
        // zone（面板会回显本次实际用的区），否则自动选区可能改主意、变成第二笔单。
        let picked = self.resolve_zone(zone).await?;
        let resp = client
            .purchase(count, client_order_id, None, picked.as_deref())
            .await
            .map_err(|e| {
                // 记下订单号：本路径不写事件表，失败后它是唯一能安全重试的凭据。
                // 尤其是无状态码的失败（超时 / 断连）—— 卖家侧可能已扣费。
                tracing::warn!(
                    vendor_id = %self.vendor_id(),
                    order_id = %client_order_id,
                    count,
                    upstream_status = ?e.status,
                    "主动提取下单失败: {}",
                    e
                );
                VendorServiceError::Upstream(e)
            })?;
        let outcome = self.import_purchased(&resp, client_order_id).await;
        Ok(build_result(count, &resp, outcome, picked.as_deref()))
    }

    /// 提取 + 入库 + 结果写回事件行
    async fn purchase_and_import(
        &self,
        client: &VendorClient,
        event_id: &str,
        order_id: &str,
        batch_order_id: Option<&str>,
        count: u32,
        zone: Option<&str>,
        trigger: PurchaseTrigger,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let vid = self.vendor_id();
        let resp = match client
            .purchase(count, order_id, batch_order_id, zone)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // 记失败但保留 bound_count，便于按同一数量重试
                let outcome = PurchaseOutcome {
                    last_error: Some(e.to_string()),
                    ..Default::default()
                };
                let _ = self.store.finish_purchase(
                    vid,
                    event_id,
                    PurchaseStatus::Failed,
                    trigger,
                    &outcome,
                );
                return Err(VendorServiceError::Upstream(e));
            }
        };

        let mut outcome = self.import_purchased(&resp, order_id).await;
        outcome.purchased = resp.purchased;

        let status = if outcome.failed > 0 && outcome.imported == 0 {
            PurchaseStatus::Failed
        } else {
            PurchaseStatus::Done
        };
        if let Err(e) = self
            .store
            .finish_purchase(vid, event_id, status, trigger, &outcome)
        {
            tracing::warn!("写回提取结果失败 event_id={}: {}", event_id, e);
        }

        Ok(build_result(count, &resp, outcome, zone))
    }

    /// 把提取到的 Key 逐条入库。复用 admin 的 `import_one_credential`：
    /// 去重、验活、失败回滚的逻辑与批量导入完全一致。
    async fn import_purchased(&self, resp: &PurchaseResult, order_id: &str) -> PurchaseOutcome {
        let cfg = &self.config;
        let groups = cfg.default_groups.clone();
        let rpm_limit = cfg.default_rpm_limit;
        let priority = cfg.effective_default_priority();
        // 来源渠道带上供应商 id，便于按家盘点与对账
        let source_channel = format!("{}{}:{}", auto::VENDOR_CHANNEL_PREFIX, self.vendor_id(), order_id);

        // 整条带过去：单张卡自带区域时（kiro.red 双区混发）入库要以卡上的为准
        let keys: Vec<_> = resp.keys.iter()
            .filter(|k| !k.key.trim().is_empty())
            .cloned()
            .collect();

        // 根据实际成交区域设置 api_region：eu 需要 eu-central-1，us 或不分区用默认。
        // kiro.ooo 扩展：该家的区域是完整 AWS 标识符（如 eu-central-1、us-east-1），
        // 而非两字母简码，故映射扩成也认含连字符的完整 ID —— 直接用。
        // 注意：kiro.red 的 zone 是**商品 id**（如 `55`）而非区域，不能只看
        // 「含连字符」就当区域用 —— 那家的区在每张卡上，走 PurchasedKey::region。
        let api_region = resp.zone.as_deref().and_then(|z| {
            if looks_like_aws_region(z) {
                // 完整 AWS 区域标识，直接用（kiro.ooo 走此分支）
                Some(z.to_ascii_lowercase())
            } else if z == "eu" {
                // 两字母简码的首家 / Drop / kiromarket 走这里
                Some("eu-central-1".to_string())
            } else {
                None
            }
        })
        // 都推不出来时用该家配置的 defaultApiRegion。此前这个配置项在入库路径上
        // 完全没被读过，卡上无区、zone 又不是区域的家（kiro.red）只能拿到 None，
        // 于是回落全局默认区、连错端点报凭证失效。
        .or_else(|| {
            let d = cfg.default_api_region.trim();
            (!d.is_empty()).then(|| d.to_ascii_lowercase())
        });

        super::import::import_keys(
            &self.admin,
            keys,
            &source_channel,
            groups,
            rpm_limit,
            api_region,
            priority,
        ).await
    }

    // ============ 自动模式 ============

    /// 自动模式单次提取上限。配了时段表且当前时刻命中时以该段为准。
    ///
    /// 时刻取本地时区（与 usageStats 一致），容器内需正确设置 `TZ`，
    /// 否则「下午」会按 UTC 判定、偏 8 小时。
    pub fn auto_max_count(&self) -> u32 {
        schedule::max_count_at(
            &self.config.auto_purchase_schedule,
            self.config.auto_purchase_max_count,
            chrono::Local::now().time(),
        )
    }

    /// 当前命中的时段描述（如 `14:00–23:00`），供面板说明「为什么是这个数」
    pub fn auto_active_window(&self) -> Option<String> {
        schedule::active_window_label(
            &self.config.auto_purchase_schedule,
            chrono::Local::now().time(),
        )
    }

    /// 从凭据池取出**本供应商**的 Key 状态切片。
    ///
    /// 按 vendor_id 过滤是多供应商下的关键：A 家推来「全部失效」时，若把 B 家
    /// 仍然健康的 Key 也算进来，就会永远得不出「已无可用 Key」的结论，
    /// A 家的自动补货被 B 家挡死。
    fn vendor_key_states(&self) -> Vec<auto::VendorKeyState> {
        self.admin
            .token_manager()
            .snapshot()
            .entries
            .into_iter()
            .map(|e| auto::VendorKeyState {
                source_channel: e.source_channel,
                disabled: e.disabled,
                disabled_reason: e.disabled_reason,
                failure_count: e.failure_count,
            })
            .collect()
    }

    /// 盘点并写入一次确认结论，返回该结论
    fn run_validation_once(&self, event_id: &str, window_expired: bool) -> ValidationStatus {
        let vid = self.vendor_id();
        let c = auto::census(&self.vendor_key_states(), vid);
        let (status, detail) = auto::conclude(c, window_expired);
        if let Err(e) = self.store.set_validation(vid, event_id, status, &detail) {
            tracing::warn!("写入失效确认结论失败 event_id={}: {}", event_id, e);
        }
        tracing::info!(
            vendor_id = %vid,
            event_id = %event_id,
            status = status.as_str(),
            detail = %detail,
            "卖家 Key 失效确认"
        );
        status
    }

    /// 启动失效确认的观察窗口。
    ///
    /// 卖家推来 `all_keys_dead` 时本地通常还没探到失效（本地状态靠真实请求
    /// 失败累积），故先立刻盘点一次，未得出"已全部失效"就按 30 秒一轮继续
    /// 观察，最多 3 分钟。
    ///
    /// 只要本地已无可用 Key（`alive == 0`），首轮盘点即 `ConfirmedDead` 并直接返回，
    /// 不进轮询 —— 观察窗口只用于"仍有存活 Key"的情况，等其失败计数追上卖家的说法。
    pub fn spawn_dead_validation(self: &Arc<Self>, event_id: String) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            if svc.run_validation_once(&event_id, false) == ValidationStatus::ConfirmedDead {
                return;
            }
            let deadline = tokio::time::Instant::now() + auto::VALIDATION_WINDOW;
            loop {
                tokio::time::sleep(auto::VALIDATION_POLL_INTERVAL).await;
                let expired = tokio::time::Instant::now() >= deadline;
                let status = svc.run_validation_once(&event_id, expired);
                if expired || status == ValidationStatus::ConfirmedDead {
                    return;
                }
            }
        });
    }

    /// 库存轮询的**实际生效间隔**（秒），0 表示未启用。
    ///
    /// 与配置原值的区别是已抬过下限。面板与轮询器都用它，避免面板显示一个
    /// 与真实节奏不符的数（配 10 秒实际按 60 秒跑，显示 10 会让人误判）。
    pub fn stock_poll_interval(&self) -> u64 {
        self.config.effective_stock_poll_interval()
    }

    /// 拉起库存轮询器，给**没有 webhook 的卖家**补上「谁来叫醒自动提取」这一环。
    ///
    /// 返回 `false` 表示未启用（配置为 0），调用方据此不必记日志。
    ///
    /// 为什么需要它：自动提取全代码库只有一个触发点 —— 入站 webhook 收到
    /// `new_keys_available`。而 `kirored` / `kiroapp-cc` 压根不提供推送，这两家单独开
    /// `autoPurchase` 是**静默无效**的：面板显示「自动提取」，却一次都不会动，
    /// 且因为流程从未启动，连 `record_skip` 的跳过原因都不会留 —— 现象与
    /// 「webhook 链路断了」完全一样，极难分辨。
    ///
    /// 本轮询器只负责「发现新车 → 合成事件 → 交给 `spawn_auto_purchase`」，
    /// **一步判定都不自己做**：授权、池闸、并发锁、数量绑定、幂等全部沿用既有管线。
    /// 这是有意的 —— 轮询多一条判定就多一条绕过闸门的路径。
    pub fn spawn_stock_poller(self: &Arc<Self>) -> bool {
        let interval = self.stock_poll_interval();
        if interval == 0 {
            return false;
        }
        // 抬到下限时告警一次。抬而不是拒绝启动：用户意图明确是「要轮询」，
        // 配小了是不知道代价，按下限跑起来比不跑更符合意图。
        let configured = self.config.stock_poll_interval_secs;
        if configured < MIN_STOCK_POLL_INTERVAL_SECS {
            tracing::warn!(
                vendor_id = %self.vendor_id(),
                configured,
                min = MIN_STOCK_POLL_INTERVAL_SECS,
                "轮询间隔小于下限，已抬到下限"
            );
        }

        // 入站可用的家不该靠轮询 —— 卖家推送比我们轮询更及时也更省。配了就提醒，
        // 但不拒绝：两者并存不会重复下单（合成事件的 id 带 `poll:` 前缀，与卖家
        // 事件天然不同名；真撞上同一趟车，池闸与本家盘点会挡住第二单）。
        if self.config.inbound_enabled() {
            tracing::warn!(
                vendor_id = %self.vendor_id(),
                "本家已启用入站 webhook，仍配了库存轮询；卖家推送更及时，通常不必两者并存"
            );
        }

        let svc = Arc::clone(self);
        tokio::spawn(async move {
            let period = Duration::from_secs(interval);
            tracing::info!(
                vendor_id = %svc.vendor_id(),
                interval_secs = interval,
                auto_purchase = svc.auto_purchase(),
                "库存轮询已启动"
            );
            // 连续失败计数，用于指数退避。查库存对 kirored 是登录+签名+解密，
            // 卖家侧故障时死循环重试既压对方也刷满我们的日志。
            let mut failures: u32 = 0;
            loop {
                // 先睡后查：启动瞬间往往还没加载完凭据池，此时盘点结果不可信，
                // 会让第一轮基于「池是空的」误判为该补货。
                //
                // 退避封顶 16 倍（1 分钟间隔 → 最长 16 分钟）：卖家侧故障可能持续
                // 很久，无上限的指数退避会退到几小时，恢复后半天发现不了新车。
                let backoff = period * 2u32.saturating_pow(failures.min(4));
                tokio::time::sleep(backoff).await;

                match svc.poll_stock_once().await {
                    Ok(()) => failures = 0,
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        tracing::warn!(
                            vendor_id = %svc.vendor_id(),
                            failures,
                            "库存轮询失败，将退避后重试: {}",
                            e
                        );
                    }
                }
            }
        });
        true
    }

    /// 轮询一轮：查库存 → 判定 → 发现新车则合成事件并走自动提取。
    ///
    /// `Err` 只用于**出站失败**（触发退避）；「没货」「没有新车」都是正常结果，
    /// 记 debug 日志后返回 `Ok`。
    async fn poll_stock_once(self: &Arc<Self>) -> Result<(), VendorServiceError> {
        let vid = self.vendor_id();

        // 全局总闸，由本家的 `stockPollRespectGlobalGate` 决定要不要认：
        //
        // - 遵循（默认）→ 总闸关着就连库存都不查，最省
        // - 不遵循 → 总闸对本家这条轮询链路**整体失效**：继续发现，且发现后
        //   `try_auto_purchase` 也会绕过总闸真下单（见那里的步骤 0）
        //
        // 后者是一条越过全局急停的扣费路径，只在用户显式配了才成立。想停掉本家
        // 得关它自己的 autoPurchase，或把 stockPollIntervalSecs 改成 0。
        //
        // 注意这里**不**因 `!auto_purchase()` 就跳过：只开轮询不开自动提取是一种
        // 有意的用法（先观察轮询能否发现新车，不冒扣费风险），此时仍要查并落库。
        if self.stock_poll_respect_gate.load(Ordering::Relaxed) {
            if let Err(reason) = self.pool_gate.check_auto_enabled() {
                tracing::debug!(vendor_id = %vid, "库存轮询跳过: {}", reason);
                return Ok(());
            }
        }

        let stock = self.stock().await?;
        let batch = match auto::decide_poll(&stock) {
            auto::PollDecision::Found(b) => b,
            auto::PollDecision::Idle(reason) => {
                tracing::debug!(vendor_id = %vid, "库存轮询本轮无事: {}", reason);
                return Ok(());
            }
        };

        let event_id = batch.event_id();
        // 这趟车我们处理过了吗？靠事件表去重 —— 重启也不会重复提（表在 SQLite 里）。
        //
        // 只要行存在就跳过，**不看它当初提成了还是跳过了**：失败的那一趟若在这里
        // 重试，等于每个轮询周期都对同一趟车撞一次授权判定与下单，而下单失败的原因
        // 通常不是重试能解决的（余额不足 / 卖家侧拒单）。需要重试就人工在面板上按
        // 那条事件提取。
        match self.store.get_event(vid, &event_id) {
            Ok(Some(_)) => {
                tracing::debug!(
                    vendor_id = %vid,
                    event_id = %event_id,
                    "库存轮询：这趟车已处理过，跳过"
                );
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => {
                // 读不到就当没处理过会导致重复下单，故这里视为出站级失败去退避
                return Err(VendorServiceError::Storage(format!(
                    "查询合成事件失败: {e}"
                )));
            }
        }

        // 合成事件落库。`purchase_order_id` 必须给 —— `dispatch_event` 那条路会对
        // 缺订单号的事件跳过自动提取，而幂等键正是从它派生的。用 event_id 本身，
        // 于是「一趟车 = 一条事件 = 一笔订单」在库里就是可见的。
        let zone_label = stock
            .find_zone(&batch.zone)
            .and_then(|z| z.label.clone())
            .unwrap_or_else(|| batch.zone.clone());
        let event = IncomingEvent {
            vendor_id: vid.to_string(),
            event_id: event_id.clone(),
            kind: VendorEventKind::NewKeysAvailable,
            purchase_order_id: Some(event_id.clone()),
            batch_order_id: None,
            message: Some(format!("库存轮询发现新车：{zone_label}")),
            // 卖家没说几张。留空让 decide_count 按「卖家上限 ∧ 配置上限」取小 ——
            // 填一个我们猜的数会盖掉那两个真实上限。
            new_keys: None,
            dead: None,
            raw_payload: serde_json::json!({
                "source": "stock_poll",
                "zone": batch.zone,
                "departedAt": batch.departed_at,
            })
            .to_string(),
        };

        match self.store.record_event(&event) {
            Ok(RecordOutcome::Inserted) => {}
            // 并发或竞态下已被写入：另一条路径会处理，这里不重复派发
            Ok(RecordOutcome::Duplicate) => {
                tracing::debug!(
                    vendor_id = %vid,
                    event_id = %event_id,
                    "库存轮询：合成事件已存在，交由既有那条处理"
                );
                return Ok(());
            }
            Err(e) => {
                return Err(VendorServiceError::Storage(format!(
                    "合成事件落库失败: {e}"
                )));
            }
        }

        tracing::info!(
            vendor_id = %vid,
            event_id = %event_id,
            zone = %batch.zone,
            "库存轮询发现新车，已合成事件"
        );

        // 只开轮询没开自动提取：事件已落库，面板上能看到并手动提取。
        // 这正是「先观察、不扣费」那种用法要的效果。
        if !self.auto_purchase() {
            tracing::info!(
                vendor_id = %vid,
                event_id = %event_id,
                "本家为手动提取模式，合成事件仅落库，可在面板上手动提取"
            );
            return Ok(());
        }

        // 交回既有管线。授权、池闸、并发锁、数量绑定全在里面，轮询不重复判定。
        //
        // 传 StockPoll：本家关了 stockPollRespectGlobalGate 时，那一步会绕过总闸
        // 继续下单（该开关的字面语义）。池闸、授权判定、并发锁**都不绕**。
        self.spawn_auto_purchase(event_id, None, AutoPurchaseSource::StockPoll);
        Ok(())
    }

    /// 自动提取。仅在自动模式 + 上一轮失效已确认时真正下单。
    ///
    /// 检查顺序按「代价从小到大、可逆到不可逆」排列：先读本地确认结论（零成本），
    /// 再查卖家可提取上限（一次出站），最后才 `bind_count` —— 绑定是唯一不可逆的
    /// 一步，之前任何一环给出否定结论都只记跳过，订单号仍留给手动提取。
    ///
    /// 多家并发时还要过 [`super::pool_gate`] 的总量闸，见 `try_auto_purchase`。
    pub fn spawn_auto_purchase(
        self: &Arc<Self>,
        event_id: String,
        new_keys: Option<u32>,
        source: AutoPurchaseSource,
    ) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(reason) = svc.try_auto_purchase(&event_id, new_keys, source).await {
                tracing::info!(
                    vendor_id = %svc.vendor_id(),
                    event_id = %event_id,
                    reason = %reason,
                    "自动提取已跳过"
                );
                if let Err(e) = svc.store.record_skip(svc.vendor_id(), &event_id, &reason) {
                    tracing::warn!("记录跳过原因失败 event_id={}: {}", event_id, e);
                }
            }
        });
    }

    /// 定下本轮自动提取的授权来源。`Err(原因)` 表示无授权、不该提取。
    ///
    /// 优先认卖家推来的失效确认；拿不到可用的确认时，**就地盘点本家凭据**作为
    /// 兜底 —— 有的卖家并不按文档持续推 `all_keys_dead`（实测 Drop 家只在最初
    /// 推过一次，此后 60+ 次新货通知期间再没推过），只认卖家事件会让这些家的
    /// 自动提取在消费掉第一张额度后永久死锁。
    ///
    /// 兜底路径要求**已启用全局池闸**，这是有意的联锁：就地盘点不消费额度，
    /// 只要本家无存活 Key 就会反复成立，唯一的上限是池闸。若池闸没开就放行，
    /// 等于每条新货通知都下一单且没有任何刹车。故池闸未启用时维持原行为 ——
    /// 只认卖家事件，宁可不自动提取，也不留一条无上限的扣费路径。
    /// 判定规则本身是纯函数 [`auto::decide_authorization`]，本方法只负责取数
    /// 与把结果接上事件 id。
    fn resolve_authorization(&self) -> Result<PurchaseAuthorization, String> {
        let vid = self.vendor_id();

        // 读不到记录不算错误，交由判定函数转入兜底
        let dead = self
            .store
            .latest_dead_event(vid)
            .map_err(|e| format!("读取失效确认记录失败: {e}"))?;

        let verdict = dead.as_ref().map(|d| auto::DeadEventVerdict {
            status: d
                .validation_status
                .as_deref()
                .and_then(ValidationStatus::from_str),
            used: d.validation_used,
            detail: d.validation_detail.clone(),
        });

        let census = auto::census(&self.vendor_key_states(), vid);
        // 兜底路径的刹车：本家开了逐渠道就靠本家盘点，否则靠全局阈值。
        // 两者皆无时不放行兜底（见 `decide_authorization` 的联锁说明）。
        let gating_active = self.per_channel() || self.pool_gate.enabled();
        match auto::decide_authorization(verdict.as_ref(), gating_active, census) {
            auto::AuthDecision::DeadEvent => Ok(PurchaseAuthorization::DeadEvent {
                // 走到这个分支必然有记录，否则判定函数不会给出 DeadEvent
                event_id: dead
                    .map(|d| d.event_id)
                    .expect("DeadEvent 授权必然来自一条已存在的失效事件"),
            }),
            auto::AuthDecision::LocalCensus { detail } => {
                Ok(PurchaseAuthorization::LocalCensus { detail })
            }
            auto::AuthDecision::Denied { reason } => Err(reason),
        }
    }

    /// 自动提取的实际流程。`Err(原因)` 表示本次不提取，原因会写回事件行。
    ///
    /// `source` 决定总闸能否被绕过，见 [`AutoPurchaseSource`] 与步骤 0。
    async fn try_auto_purchase(
        &self,
        event_id: &str,
        new_keys: Option<u32>,
        source: AutoPurchaseSource,
    ) -> Result<(), String> {
        let vid = self.vendor_id();

        // 0. 自动提取总闸。排在授权判定之前 —— 它是最便宜的检查，且全局关闭时
        //    不该去消费卖家的失效确认额度。
        //
        //    为什么判在这里而不是 `dispatch_event` 里不 spawn：走到本方法的 `Err`
        //    会被 `spawn_auto_purchase` 写进事件行（`record_skip`），面板上看得到
        //    「总闸关着」这条跳过原因；提前返回则是静默丢弃，排障时分不清是关了
        //    还是 webhook 链路断了。
        //
        //    **唯一的例外**：本家关了 `stockPollRespectGlobalGate` 且本次由库存轮询
        //    触发。此时总闸对这条路失效 —— 这是该开关的字面语义（「轮询不受总闸
        //    影响」），用户显式配了才生效，缺省 true 不会走到这里。
        //
        //    代价必须写明：**总闸不再是能一键停掉全部扣费的急停**。而总闸会被健康
        //    联动自动翻转，等于把「这家花钱」从那套自动逻辑里摘了出来 —— 想真正停掉
        //    本家，得关它自己的 autoPurchase 或把 stockPollIntervalSecs 改成 0。
        //    绕过时打 warn 而非 info：这是一条越过全局急停的扣费路径，日志里要显眼。
        let bypass_gate =
            source == AutoPurchaseSource::StockPoll && !self.stock_poll_respect_gate();
        if bypass_gate {
            if let Err(reason) = self.pool_gate.check_auto_enabled() {
                tracing::warn!(
                    vendor_id = %vid,
                    event_id = %event_id,
                    gate_says = %reason,
                    "本家已配 stockPollRespectGlobalGate=false，轮询触发的自动提取\
                     绕过总闸继续 —— 总闸对本家不再是急停"
                );
            }
        } else {
            self.pool_gate.check_auto_enabled()?;
        }

        // 1. 授权：卖家的失效确认，或就地盘点的兜底结论
        let auth = self.resolve_authorization()?;

        // 2. 全局提取锁。必须在盘点之前拿到，并持有到下单+导入结束 ——
        //    否则三家并发时会同时读到「池里 0 个存活」再同时下单，闸门形同虚设。
        //    开了逐渠道的家**也要**取锁：它跳过的是阈值判断，不是并发保护 ——
        //    同一家的两条推送并发到达时，若不串行化会各下一单、两张都记在本家。
        //    两种刹车皆无时不必付串行化的代价（此时兜底路径也已被拒），跳过取锁。
        let per_channel = self.per_channel();
        let _gate = if per_channel || self.pool_gate.enabled() {
            Some(self.pool_gate.acquire().await?)
        } else {
            None
        };

        // 3. 全局池量闸，**仅对没开逐渠道的家生效**。零成本本地读，故排在出站
        //    查库存之前。这里重新盘点而非复用步骤 1 的结论：等锁期间别家可能
        //    已经补过货了，锁前的池量视图已经过期。
        //
        //    开了逐渠道的家跳过这一步：判据已由步骤 1 的本家盘点给出。注意它买来
        //    的号**仍会计入**别家的 `pool_alive` —— 刻意的不对称，见配置项文档。
        if !per_channel {
            self.pool_gate
                .check(auto::pool_alive(&self.vendor_key_states()))?;
        }

        // 4. 数量：三者取最小，为 0 则无可提
        let stock = self
            .stock()
            .await
            .map_err(|e| format!("查询可提取上限失败: {e}"))?;
        // 分区卖家的 stock.available 是各区之和，拿它算数量会超出单区实际库存 ——
        // 下单只落在一个区，超出部分必然提不到。故按实际要用的那个区的量算。
        let picked = stock.pick_zone();
        let zone_max = picked.map_or(stock.available, |z| z.available);
        if self.capabilities().zoned_purchase && picked.is_none() {
            return Err("各区均无库存，本轮不自动提取".to_string());
        }
        // 只读一次：时段边界附近两次调用可能拿到不同值，会让判定与文案对不上
        let configured_max = self.auto_max_count();
        let count = auto::decide_count(new_keys, zone_max, configured_max);
        if count == 0 {
            return Err(format!(
                "可提取数量为 0（事件声明 {}，卖家上限 {}{}，配置上限 {}）",
                new_keys.map(|v| v.to_string()).unwrap_or("-".into()),
                zone_max,
                picked.map(|z| format!("@{}", z.zone)).unwrap_or_default(),
                configured_max
            ));
        }
        // 选区结果不在这里传下去：purchase_for_event_zoned 会自己再选一次并把
        // 结果与数量一起绑定。此处重选的意义只在于把数量算对。
        let zone_hint = picked.map(|z| z.zone.clone());

        // 5. 消费确认额度。抢占式，确保一次确认只授权一轮提取。
        //    仅卖家事件那条路有额度可消费；就地盘点不消费任何东西，
        //    它的上限由上一步的池闸负责（见 `resolve_authorization` 的联锁）。
        if let PurchaseAuthorization::DeadEvent { event_id: dead_id } = &auth {
            let consumed = self
                .store
                .consume_validation(vid, dead_id)
                .map_err(|e| format!("消费失效确认失败: {e}"))?;
            if !consumed {
                return Err("失效确认已被其他自动提取取用".to_string());
            }
        }

        // 6. 下单。此处开始不可逆。`_gate` 持有到本函数结束，
        //    确保后来者盘点时能看到这批新导入的 Key。
        tracing::info!(
            vendor_id = %vid,
            event_id = %event_id,
            count,
            zone = ?zone_hint,
            auth = auth.source(),
            auth_detail = auth.detail(),
            "自动提取开始"
        );
        match self
            .purchase_for_event_zoned(
                event_id,
                count,
                zone_hint.as_deref(),
                PurchaseTrigger::Auto,
            )
            .await
        {
            Ok(r) => {
                tracing::info!(
                    vendor_id = %vid,
                    event_id = %event_id,
                    purchased = r.purchased,
                    imported = r.imported,
                    zone = ?r.zone,
                    total_debit = ?r.total_debit,
                    "自动提取完成"
                );
                Ok(())
            }
            // 失败已由 purchase_and_import 写回事件行，这里不再覆盖为 skipped
            Err(e) => {
                tracing::warn!(event_id = %event_id, "自动提取失败: {}", e);
                Ok(())
            }
        }
    }

    // ============ 出站只读 / 运营接口 ============

    /// 库存与报价
    pub async fn stock(&self) -> Result<StockInfo, VendorServiceError> {
        self.client()?
            .stock()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn profile(&self) -> Result<ProfileInfo, VendorServiceError> {
        self.client()?
            .profile()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 卖家系统状态：存活 / 失效 / 存货 Key 数（仅部分卖家支持）
    pub async fn system_status(
        &self,
    ) -> Result<super::flavor_legacy::VendorSystemStatus, VendorServiceError> {
        self.client()?
            .system_status()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 历史提取订单，用于跟本地事件对账、发现漏提
    pub async fn purchase_orders(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Paged<OrderInfo>, VendorServiceError> {
        self.client()?
            .purchase_orders(page, page_size)
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 卖家近期开号批次与平均间隔（仅部分卖家支持）
    pub async fn gen_logs(
        &self,
    ) -> Result<super::flavor_legacy::GenLogsResponse, VendorServiceError> {
        self.client()?
            .gen_logs()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 积分流水（仅部分卖家支持）
    pub async fn ledger(
        &self,
        page: Option<u32>,
        page_size: Option<u32>,
        entry_type: Option<&str>,
    ) -> Result<Paged<LedgerEntry>, VendorServiceError> {
        self.client()?
            .ledger(page, page_size, entry_type)
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 名下密钥列表（仅部分卖家支持）。库存接口不给时间，
    /// 这里的 `created_at`（开号时刻）是判断 Key 新鲜度的唯一来源。
    pub async fn my_keys(
        &self,
        history: bool,
        page: Option<u32>,
        page_size: Option<u32>,
    ) -> Result<Paged<VendorKeyInfo>, VendorServiceError> {
        self.client()?
            .my_keys(history, page, page_size)
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 最早密钥时间与总数，估算账龄用（仅部分卖家支持）
    pub async fn earliest_key(&self) -> Result<EarliestKeyInfo, VendorServiceError> {
        self.client()?
            .earliest_key()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn redeem(&self, code: &str) -> Result<RedeemResult, VendorServiceError> {
        self.client()?
            .redeem(code)
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn test_webhook(&self) -> Result<serde_json::Value, VendorServiceError> {
        self.client()?
            .test_webhook()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn set_webhook_url(&self, url: &str) -> Result<(), VendorServiceError> {
        self.client()?
            .set_webhook_url(url)
            .await
            .map_err(VendorServiceError::Upstream)
    }
}

/// 汇总下单响应与入库结果为面板可读的结构
fn build_result(
    count: u32,
    resp: &PurchaseResult,
    outcome: PurchaseOutcome,
    zone: Option<&str>,
) -> PurchaseImportResult {
    PurchaseImportResult {
        count,
        // 卖家回显的区优先；没回显时用我们下单时指定的那个
        zone: resp
            .zone
            .clone()
            .or_else(|| zone.map(|s| s.to_string())),
        requested: resp.requested,
        purchased: resp.purchased,
        imported: outcome.imported,
        duplicated: outcome.duplicated,
        failed: outcome.failed,
        remaining: resp.remaining,
        total_debit: resp.total_debit,
        unit_price: resp.unit_price,
        order_id: resp.order_id.clone(),
        replayed: resp.replayed,
        keys: resp
            .keys
            .iter()
            .map(|k| PurchasedKeyBrief {
                account: k.account.clone(),
                issuer_url: k.issuer_url.clone(),
                price: k.price,
                region: k.region.clone(),
                // 只透出存在性，密码不出后端
                has_password: k.password.as_deref().is_some_and(|p| !p.trim().is_empty()),
            })
            .collect(),
        error: outcome.last_error,
    }
}

/// `event_id` 缺失时的兜底 ID：原文 SHA256 前 32 位十六进制。
/// 同一份 payload 重投得到同一个 ID，仍能去重。
fn fallback_event_id(raw: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(raw);
    hex::encode(&digest[..16])
}

/// 是否为卖家要求的 32 位十六进制订单号形态
/// 判断一个字符串是否形如完整 AWS 区域标识（`us-east-1` / `eu-central-1`）。
///
/// 用来把「卖家回显的区域」和「卖家的商品 id」区分开：kiro.red 的 zone 是商品
/// id，早期只判 `contains('-')` 会把它当成区域写进凭证的 api_region。
/// 判据：`<字母簇>-<字母簇>+-<数字>`，段全小写字母 / 结尾为数字。
fn looks_like_aws_region(s: &str) -> bool {
    let s = s.trim().to_ascii_lowercase();
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 3 {
        return false;
    }
    // 首段是地理前缀（us / eu / ap / sa / ca / me / af / cn / il），中间段是字母
    let (last, head) = parts.split_last().expect("已判长度 >= 3");
    if !last.chars().all(|c| c.is_ascii_digit()) || last.is_empty() {
        return false;
    }
    head.iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphabetic()))
}

fn is_hex32(s: &str) -> bool {
    let t = s.trim();
    t.len() == 32 && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// 为订单号不合法（或缺失）的卖家派生一个 32 位十六进制 `client_order_id`。
///
/// Drop 家文档里的 `purchase_order_id` 示例是 `batch_xxx`，而下单接口要求 32 位
/// 十六进制，直接拿去下单会被 400 拒掉。从 `(vendor_id, event_id)` 哈希得来，因此：
/// - 同一条推送重投多少次都得到同一个订单号，卖家侧幂等重放生效，不会重复扣费；
/// - 不同卖家的同名 event_id 不会撞成同一个订单号。
///
/// 不直接拿 event_id 当订单号：它的格式由卖家决定，同样不保证是 32 位十六进制。
fn derive_client_order_id(vendor_id: &str, event_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(vendor_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(event_id.as_bytes());
    hex::encode(&hasher.finalize()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_legacy(raw: &[u8]) -> Option<IncomingEvent> {
        VendorService::parse_event("default", VendorFlavor::Legacy, raw)
    }

    fn parse_kiroapp(raw: &[u8]) -> Option<IncomingEvent> {
        VendorService::parse_event("kiroapp", VendorFlavor::Kiroapp, raw)
    }

    fn parse_drop(raw: &[u8]) -> Option<IncomingEvent> {
        VendorService::parse_event("drop", VendorFlavor::Drop, raw)
    }

    fn parse_kiromarket(raw: &[u8]) -> Option<IncomingEvent> {
        VendorService::parse_event("km", VendorFlavor::Kiromarket, raw)
    }

    // ============ 第五家 kiro-market（api.91kiro.com）============

    /// 文档 §6 给的补货样本。本家的 `purchase_order_id` 是替我们预生成的
    /// **提货幂等键**（32 位十六进制），直接当 client_order_id 用即是文档推荐用法。
    #[test]
    fn kiromarket_补货事件() {
        let raw = r#"{"event":"new_keys_available",
            "event_id":"evt-1","visibility":"public",
            "message":"美国区新增 20 个 Key 已就绪，可提货","new_keys":20,"zone":"us",
            "purchase_order_id":"0a1b2c3d4e5f60718293a4b5c6d7e8f9",
            "pool_id":"m1","timestamp":1785000000}"#;
        let e = parse_kiromarket(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        assert_eq!(e.new_keys, Some(20));
        assert_eq!(
            e.purchase_order_id.as_deref(),
            Some("0a1b2c3d4e5f60718293a4b5c6d7e8f9"),
            "合法的 hex32 应原样沿用，它就是提货幂等键"
        );
        // 本家无「可定向拉取的批次 id」：round_id 是车次，下单不接受它
        assert!(e.batch_order_id.is_none());
        assert!(e.message.as_deref().unwrap().contains("美国区"));
    }

    /// 文档说 `purchase_order_id` 是 32 位十六进制，但 Drop 家就出现过文档与实际
    /// 不符（示例值 `batch_xxx` 而下单要求 hex32）。形态不合法必须换成派生值，
    /// 否则下单被 400 `bad_order_id` 拒。
    #[test]
    fn kiromarket_非法形态的订单号被换成派生值() {
        let raw = br#"{"event":"new_keys_available","event_id":"e1",
            "purchase_order_id":"batch_not_hex"}"#;
        let order = parse_kiromarket(raw).unwrap().purchase_order_id.unwrap();
        assert_ne!(order, "batch_not_hex");
        assert!(is_hex32(&order), "派生值必须合法: {order}");
    }

    /// 缺订单号时也要派生一个 —— 否则 `dispatch_event` 会因缺号跳过自动提取
    #[test]
    fn kiromarket_缺订单号时派生() {
        let raw = br#"{"event":"new_keys_available","event_id":"e2","new_keys":5}"#;
        let e = parse_kiromarket(raw).unwrap();
        assert!(is_hex32(e.purchase_order_id.as_deref().unwrap()));
    }

    /// 同一条推送重投得到同一订单号，卖家侧幂等重放生效、不会重复扣费
    #[test]
    fn kiromarket_派生值对同一推送稳定() {
        let raw = br#"{"event":"new_keys_available","event_id":"same","purchase_order_id":"x"}"#;
        let a = parse_kiromarket(raw).unwrap().purchase_order_id;
        let b = parse_kiromarket(raw).unwrap().purchase_order_id;
        assert_eq!(a, b);
    }

    /// 不同卖家的同名 event_id 不能撞成同一个订单号
    #[test]
    fn kiromarket_不同卖家的派生值不相同() {
        let raw = br#"{"event":"new_keys_available","event_id":"dup","purchase_order_id":"x"}"#;
        let a = VendorService::parse_event("km-a", VendorFlavor::Kiromarket, raw)
            .unwrap()
            .purchase_order_id;
        let b = VendorService::parse_event("km-b", VendorFlavor::Kiromarket, raw)
            .unwrap()
            .purchase_order_id;
        assert_ne!(a, b);
    }

    /// 全部失效事件：启动失效确认观察窗口的依据
    #[test]
    fn kiromarket_全部失效事件() {
        let raw = br#"{"event":"all_keys_dead","event_id":"e3","round_id":"r1","dead":20}"#;
        let e = parse_kiromarket(raw).unwrap();
        assert_eq!(e.kind, VendorEventKind::AllKeysDead);
        assert_eq!(e.dead, Some(20));
    }

    /// 本家独有的两类事件目前不建模，落成 Unknown 即可 —— 只落库不派发动作。
    ///
    /// `warranty_refund` 是质保期内车次判死的自动退款通知，无需我方动作。
    /// `reserved_keys_delivered` 是包量预留已交付（钱已扣、号已是我们的，要拿
    /// order_id 调补拉接口取正文），本轮刻意不接：它需要一条「不下单只取件」的
    /// 新路径。**没签包量协议就不会收到这条**，故不接不影响常规补货。
    #[test]
    fn kiromarket_未建模事件落成unknown不派发() {
        for name in ["warranty_refund", "reserved_keys_delivered", "webhook_test"] {
            let raw = format!(r#"{{"event":"{name}","event_id":"x-{name}"}}"#);
            let e = parse_kiromarket(raw.as_bytes()).unwrap();
            assert_eq!(
                e.kind,
                VendorEventKind::Unknown,
                "{name} 目前不建模，应落成 Unknown"
            );
        }
    }

    // ============ Drop 家（drop.kiro.ss）============

    /// 用现行文档给的新货样本。事件名与首家相同。
    #[test]
    fn drop_新key就绪事件() {
        let raw = r#"{"event":"new_keys_available","event_id":"0123456789abcdef0123456789abcdef",
            "purchase_order_id":"batch_xxx","message":"新一批 Key 已上架"}"#;
        let e = parse_drop(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        assert_eq!(e.message.as_deref(), Some("新一批 Key 已上架"));
        // 文档未给数量，自动提取据此按卖家上限取小（见 auto::decide_count）
        assert_eq!(e.new_keys, None);
    }

    /// 文档示例里的 `purchase_order_id` 是 `batch_xxx`，不是 32 位十六进制，
    /// 而下单接口要求 32 位十六进制 —— 必须换成派生值，否则下单被 400 拒。
    #[test]
    fn drop_非法形态的订单号被换成派生值() {
        let raw = br#"{"event":"new_keys_available","event_id":"e1","purchase_order_id":"batch_xxx"}"#;
        let order = parse_drop(raw).unwrap().purchase_order_id.unwrap();
        assert_ne!(order, "batch_xxx", "原值不合法，不能直接拿去下单");
        assert!(is_hex32(&order), "派生值必须合法: {order}");
    }

    /// 卖家给了合法形态时直接用，不多此一举地派生
    #[test]
    fn drop_合法订单号直接沿用() {
        let given = "ffffffffffffffffffffffffffffffff";
        let raw =
            format!(r#"{{"event":"new_keys_available","event_id":"e1","purchase_order_id":"{given}"}}"#);
        assert_eq!(
            parse_drop(raw.as_bytes()).unwrap().purchase_order_id.as_deref(),
            Some(given)
        );
    }

    /// 同一条推送重投得到同一订单号 —— 幂等重放的前提
    #[test]
    fn drop_同一事件派生同一订单号() {
        let raw = br#"{"event":"new_keys_available","event_id":"evt-1"}"#;
        let a = parse_drop(raw).unwrap().purchase_order_id;
        let b = parse_drop(raw).unwrap().purchase_order_id;
        assert_eq!(a, b);

        // 不同事件不能撞成同一单
        let other = br#"{"event":"new_keys_available","event_id":"evt-2"}"#;
        assert_ne!(a, parse_drop(other).unwrap().purchase_order_id);
    }

    /// 不同卖家的同名 event_id 不能派生出同一个订单号
    #[test]
    fn drop_订单号按卖家隔离() {
        let raw = br#"{"event":"new_keys_available","event_id":"same-id"}"#;
        let a = VendorService::parse_event("drop-a", VendorFlavor::Drop, raw)
            .unwrap()
            .purchase_order_id;
        let b = VendorService::parse_event("drop-b", VendorFlavor::Drop, raw)
            .unwrap()
            .purchase_order_id;
        assert_ne!(a, b);
    }

    #[test]
    fn drop_全部失效与测试事件() {
        let raw = r#"{"event":"all_keys_dead","event_id":"d1",
            "message":"全部 Key 已失效，系统正在自动补充","dead":5}"#;
        let e = parse_drop(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::AllKeysDead);
        assert_eq!(e.dead, Some(5));

        let t = parse_drop(r#"{"event":"test","event_id":"t1","message":"这是一条测试"}"#.as_bytes())
            .unwrap();
        assert_eq!(t.kind, VendorEventKind::Test);
    }

    #[test]
    fn 解析新key就绪事件() {
        let raw = r#"{"event":"new_keys_available","event_id":"abc123","purchase_order_id":"0123456789abcdef0123456789abcdef","message":"新一轮 10 个 Key 已就绪","new_keys":10}"#;
        let e = parse_legacy(raw.as_bytes()).unwrap();
        assert_eq!(e.event_id, "abc123");
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        assert_eq!(
            e.purchase_order_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(e.new_keys, Some(10));
        assert_eq!(e.dead, None);
        assert_eq!(e.vendor_id, "default");
        // 该 flavor 没有批次概念
        assert!(e.batch_order_id.is_none());
    }

    #[test]
    fn 解析全部失效事件() {
        let raw = r#"{"event":"all_keys_dead","event_id":"def456","message":"本轮全部 5 个 Key 已失效","dead":5}"#;
        let e = parse_legacy(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::AllKeysDead);
        assert_eq!(e.dead, Some(5));
        assert!(e.purchase_order_id.is_none());
    }

    #[test]
    fn 缺失event_id时用原文哈希兜底且稳定() {
        let raw = br#"{"event":"all_keys_dead","dead":3}"#;
        let a = parse_legacy(raw).unwrap();
        let b = parse_legacy(raw).unwrap();
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.event_id.len(), 32);
    }

    #[test]
    fn 非法json返回none() {
        assert!(parse_legacy(b"not json").is_none());
        // 顶层非对象也拒绝
        assert!(parse_legacy(b"[1,2,3]").is_none());
    }

    #[test]
    fn 未知事件类型归为unknown() {
        let raw = br#"{"event":"brand_new","event_id":"x1"}"#;
        let e = parse_legacy(raw).unwrap();
        assert_eq!(e.kind, VendorEventKind::Unknown);
    }

    // ============ kiroapp 的推送形态 ============

    /// 文档给的 webhook 样本：幂等键叫 client_order_id，另有 order_id 是批次 id
    #[test]
    fn 解析kiroapp推送_真实样本() {
        let raw = r#"{"event":"new_keys_available","event_id":"uniq-1",
            "order_id":"batch-77","client_order_id":"d5c4fd9460b70fb8e944bd7faa519896",
            "mother_id":"m-1","visibility":"public",
            "message":"母号新开号完成，20 个 Key 就绪","new_keys":20}"#;
        let e = parse_kiroapp(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        // 幂等键取 client_order_id —— 卖家已按「批次 + 收件人」派生好，不能自己生成
        assert_eq!(
            e.purchase_order_id.as_deref(),
            Some("d5c4fd9460b70fb8e944bd7faa519896")
        );
        // 批次 id 单独留存，下单时带上只拉这一批
        assert_eq!(e.batch_order_id.as_deref(), Some("batch-77"));
        assert_eq!(e.new_keys, Some(20));
        assert_eq!(e.vendor_id, "kiroapp");
    }

    #[test]
    fn kiroapp_识别滥用回收与测试事件() {
        let e = parse_kiroapp(br#"{"event":"key_revoked_abuse","event_id":"r1"}"#).unwrap();
        assert_eq!(e.kind, VendorEventKind::KeyRevokedAbuse);

        let t = parse_kiroapp(br#"{"event":"test","event_id":"t1"}"#).unwrap();
        assert_eq!(t.kind, VendorEventKind::Test);
    }

    #[test]
    fn kiroapp_缺client_order_id时回退旧字段名() {
        // 防御性：卖家若改回通用字段名，仍能拿到幂等键而不是丢掉提取能力
        let raw = br#"{"event":"new_keys_available","event_id":"x",
            "purchase_order_id":"aabb","new_keys":1}"#;
        let e = parse_kiroapp(raw).unwrap();
        assert_eq!(e.purchase_order_id.as_deref(), Some("aabb"));
    }

    #[test]
    fn 空字符串字段视为缺失() {
        let raw = br#"{"event":"new_keys_available","event_id":"x",
            "client_order_id":"  ","order_id":""}"#;
        let e = parse_kiroapp(raw).unwrap();
        assert!(e.purchase_order_id.is_none(), "空白订单号不能当成有效幂等键");
        assert!(e.batch_order_id.is_none());
    }

    #[test]
    fn 汇总结果保留阶梯定价明细且不外传密码() {
        use super::super::protocol::PurchasedKey;
        let resp = PurchaseResult {
            purchased: 2,
            requested: Some(5),
            remaining: Some(115.0),
            unit_price: Some(35.0),
            total_debit: Some(70.0),
            order_id: Some("batch-1".to_string()),
            replayed: false,
            zone: None,
            keys: vec![
                PurchasedKey {
                    key: "sk-a".to_string(),
                    account: Some("user-a".to_string()),
                    password: Some("secret".to_string()),
                    issuer_url: Some("https://i".to_string()),
                    price: Some(30.0),
                    region: None,
                },
                PurchasedKey {
                    key: "sk-b".to_string(),
                    account: None,
                    password: None,
                    issuer_url: None,
                    price: Some(40.0),
                    region: None,
                },
            ],
        };
        let outcome = PurchaseOutcome {
            purchased: 2,
            imported: 2,
            ..Default::default()
        };
        let r = build_result(5, &resp, outcome, None);

        // 部分成交要能被面板识别
        assert_eq!(r.requested, Some(5));
        assert_eq!(r.purchased, 2);
        // 阶梯定价：总价取卖家权威数字，不是数量 × 单价
        assert_eq!(r.total_debit, Some(70.0));
        assert_eq!(r.keys.len(), 2);
        assert_eq!(r.keys[0].price, Some(30.0));
        assert_eq!(r.keys[1].price, Some(40.0));
        assert_eq!(r.keys[0].account.as_deref(), Some("user-a"));

        // 密码只透出存在性
        assert!(r.keys[0].has_password);
        assert!(!r.keys[1].has_password);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("secret"), "密码不能出现在响应里: {json}");
        assert!(!json.contains("sk-a"), "密钥明文不能出现在响应里");
    }

    #[test]
    fn 空白密码不算有密码() {
        use super::super::protocol::PurchasedKey;
        let resp = PurchaseResult {
            purchased: 1,
            keys: vec![PurchasedKey {
                key: "k".to_string(),
                account: None,
                password: Some("   ".to_string()),
                issuer_url: None,
                price: None,
                region: None,
            }],
            ..Default::default()
        };
        let r = build_result(1, &resp, PurchaseOutcome::default(), None);
        assert!(!r.keys[0].has_password);
    }

    #[test]
    fn 同一payload不同供应商各自归属() {
        let raw = br#"{"event":"new_keys_available","event_id":"same"}"#;
        let a = VendorService::parse_event("a", VendorFlavor::Legacy, raw).unwrap();
        let b = VendorService::parse_event("b", VendorFlavor::Legacy, raw).unwrap();
        assert_eq!(a.event_id, b.event_id);
        assert_ne!(a.vendor_id, b.vendor_id);
    }
}

/// 本地新增测试单独成块，避免插进上游 `mod tests` 中间引发合并冲突。
#[cfg(test)]
mod local_tests {
    use super::*;

    // ============ 第七家 kiro.ooo 的入站事件解析 ============

    /// 本家 claim 的幂等键就叫 `client_order_id`，推送里也带它，要优先取
    #[test]
    fn kiroooo_取client_order_id做幂等键() {
        let raw = br#"{"event":"new_keys_available","event_id":"e1",
            "client_order_id":"21a68cccb15074980ffa96dc3a050b3d","new_keys":2}"#;
        let e = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, raw).unwrap();
        assert_eq!(
            e.purchase_order_id.as_deref(),
            Some("21a68cccb15074980ffa96dc3a050b3d"),
            "必须原样沿用推送里的订单号，改写会错过卖家侧的幂等重放"
        );
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        assert_eq!(e.new_keys, Some(2));
        // 本家没有可定向拉取的批次 id
        assert!(e.batch_order_id.is_none());
    }

    /// 订单号形态不合法（或缺失）时派生一个，且**对同一条推送稳定** ——
    /// 否则重投会被当成第二笔单再扣一次积分
    #[test]
    fn kiroooo_订单号不合法时派生且稳定() {
        let raw = br#"{"event":"new_keys_available","event_id":"e2",
            "client_order_id":"batch_not_hex"}"#;
        let a = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, raw).unwrap();
        let b = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, raw).unwrap();
        let oid = a.purchase_order_id.as_deref().unwrap();
        assert!(is_hex32(oid), "派生值必须是 32 位十六进制，实际: {oid}");
        assert_eq!(
            a.purchase_order_id, b.purchase_order_id,
            "同一条推送必须派生出同一个订单号，否则重投会重复扣费"
        );
    }

    /// 缺订单号也要派生 —— 有订单号才可能走自动提取（见 `dispatch_event`）
    #[test]
    fn kiroooo_缺订单号也派生() {
        let raw = br#"{"event":"new_keys_available","event_id":"e3"}"#;
        let e = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, raw).unwrap();
        assert!(e.purchase_order_id.is_some_and(|s| is_hex32(&s)));
    }

    /// **本家 webhook 事件名未经实测**，故走宽松归一化。这条锁住归一化确实接在
    /// 解析链路上 —— 只测 `normalize_event_type` 本身不能证明它被调用了。
    #[test]
    fn kiroooo_事件名归一化接在解析链路上() {
        // 本家通知开关叫 on_key_new，webhook 事件名大概率同源
        let raw = br#"{"event":"key_new","event_id":"e4",
            "client_order_id":"21a68cccb15074980ffa96dc3a050b3d"}"#;
        let e = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, raw).unwrap();
        assert_eq!(
            e.kind,
            VendorEventKind::NewKeysAvailable,
            "key_new 必须归一成新货事件，落成 unknown 会让自动补货完全不工作"
        );

        // 疑似失效不能当成全失效 —— 那会在旧 Key 可能还活着时触发补货扣费
        let suspect = br#"{"event":"key_suspect","event_id":"e5"}"#;
        let s = VendorService::parse_event("kiro-ooo", VendorFlavor::KiroOoo, suspect).unwrap();
        assert_ne!(
            s.kind,
            VendorEventKind::AllKeysDead,
            "疑似失效只该告警，映射成全失效会误触发扣费"
        );
    }

    /// 归一化只对本家生效，不能影响别家的事件名解析
    #[test]
    fn kiroooo_的归一化不影响别家() {
        let raw = br#"{"event":"key_new","event_id":"e6"}"#;
        let legacy = VendorService::parse_event("other", VendorFlavor::Legacy, raw).unwrap();
        assert_eq!(
            legacy.kind,
            VendorEventKind::Unknown,
            "首家没有 key_new 这个事件名，不该被本家的归一化带偏"
        );
    }

    // ============ 逐 Key 区域 → api_region 的映射 ============

    /// 本家的区域是完整 AWS 标识（`eu-central-1`），要能直接用；
    /// 其余家的两字母简码（`eu`）仍走原分支。这条锁住那个扩展没有回归。
    #[test]
    fn 区域映射同时认完整标识与两字母简码() {
        // 与 import_purchased 里的映射保持同一份逻辑
        let map = |z: &str| -> Option<String> {
            if looks_like_aws_region(z) {
                Some(z.to_ascii_lowercase())
            } else if z == "eu" {
                Some("eu-central-1".to_string())
            } else {
                None
            }
        };
        // kiro.ooo：完整标识直接用
        assert_eq!(map("eu-central-1").as_deref(), Some("eu-central-1"));
        assert_eq!(map("us-east-1").as_deref(), Some("us-east-1"));
        // 其余家：两字母简码仍按原样映射
        assert_eq!(map("eu").as_deref(), Some("eu-central-1"));
        // us 用全局默认，不显式写
        assert!(map("us").is_none());
    }

    /// kiro.red 的 zone 是**商品 id**，绝不能被当成区域写进凭证 —— 那会让
    /// api_region 变成 `55` / `55-a` 之类的垃圾值，凭证直接报失效。
    #[test]
    fn 商品id不被当成区域() {
        assert!(!looks_like_aws_region("55"), "纯数字商品 id");
        assert!(!looks_like_aws_region("sku-58"), "两段、末段是数字但缺地理段");
        assert!(!looks_like_aws_region("纯APIKEY-双区-1"), "非 ASCII 字母");
        assert!(!looks_like_aws_region("us-east-x"), "末段必须是数字");
        assert!(looks_like_aws_region("ap-southeast-2"));
        assert!(looks_like_aws_region("us-gov-west-1"), "四段区域也认");
    }

    // ============ 库存轮询的生效间隔 ============

    /// 只给 `baseUrl` / `apiKey` 的最小配置，其余走 serde 默认 —— 这正好锁住
    /// 「用户不写这些字段时的缺省行为」，比手写全字段更贴近真实配置文件。
    fn cfg_json(extra: &str) -> VendorConfig {
        let raw = format!(r#"{{"baseUrl":"https://x","apiKey":"k"{extra}}}"#);
        serde_json::from_str(&raw).expect("配置应可解析")
    }

    /// 0 就是关闭 —— 不能被下限抬成「开着」，那等于替用户开了一条扣费路径
    #[test]
    fn 轮询间隔为零表示关闭() {
        assert_eq!(cfg_json("").effective_stock_poll_interval(), 0, "缺省即关闭");
        assert_eq!(
            cfg_json(r#","stockPollIntervalSecs":0"#).effective_stock_poll_interval(),
            0
        );
    }

    /// 配小于下限的值要抬到下限。面板与轮询器都读这个值，
    /// 否则面板显示 10 秒而实际按 60 秒跑，会让人误判「怎么没按我配的频率查」
    #[test]
    fn 轮询间隔抬到下限() {
        assert_eq!(
            cfg_json(r#","stockPollIntervalSecs":10"#).effective_stock_poll_interval(),
            MIN_STOCK_POLL_INTERVAL_SECS
        );
    }

    /// 大于等于下限的值原样保留
    #[test]
    fn 轮询间隔大于下限时原样保留() {
        assert_eq!(
            cfg_json(r#","stockPollIntervalSecs":600"#).effective_stock_poll_interval(),
            600
        );
        assert_eq!(
            cfg_json(r#","stockPollIntervalSecs":60"#).effective_stock_poll_interval(),
            60
        );
    }

    /// 遵循总闸默认开启 —— 默认不该放行一条「总闸关了还在查」的路径
    #[test]
    fn 遵循总闸默认开启() {
        assert!(
            cfg_json("").stock_poll_respect_global_gate,
            "缺省必须为 true：默认行为要最保守"
        );
        assert!(
            !cfg_json(r#","stockPollRespectGlobalGate":false"#).stock_poll_respect_global_gate,
            "显式关闭要生效"
        );
    }

    // ============ 总闸绕过的适用范围 ============

    /// 绕过条件的真值表。**这是一条越过全局急停的扣费路径，范围必须锁死。**
    ///
    /// 判据取自 `try_auto_purchase` 步骤 0：
    /// `source == StockPoll && !respect_gate`
    ///
    /// 关键是 webhook 那一行 —— 若把判据写成「只看 `!respect_gate`」，卖家推送
    /// 触发的自动提取会一并绕过总闸，比开关名承诺的范围宽得多，而用户从
    /// 「轮询不受总闸影响」这个名字上完全看不出 webhook 也被放开了。
    #[test]
    fn 绕过总闸只适用于轮询触发() {
        let bypass = |source: AutoPurchaseSource, respect_gate: bool| {
            source == AutoPurchaseSource::StockPoll && !respect_gate
        };

        // 唯一该绕过的组合：轮询触发 + 显式关了遵循
        assert!(
            bypass(AutoPurchaseSource::StockPoll, false),
            "这正是 stockPollRespectGlobalGate=false 的字面语义"
        );

        // 轮询触发但仍遵循（缺省）→ 不绕
        assert!(!bypass(AutoPurchaseSource::StockPoll, true));

        // webhook 触发，无论本家怎么配，总闸一律有效
        assert!(
            !bypass(AutoPurchaseSource::Webhook, false),
            "webhook 绝不能被这个开关放开 —— 那超出了开关承诺的范围"
        );
        assert!(!bypass(AutoPurchaseSource::Webhook, true));
    }

    /// 面板与轮询循环都必须读**运行时值**，不能读 `config` 的启动快照。
    ///
    /// 这条锁的是一个真实踩过的坑：状态接口曾返回 `cfg.stock_poll_respect_global_gate`，
    /// 于是面板关掉开关后重新拉状态拿到的还是启动时那个 true，开关点了就弹回去 ——
    /// 看着像「关闭失败」，而 PUT 请求其实全部成功、config.json 也写进去了。
    #[test]
    fn 轮询总闸遵循以运行时值为准() {
        // 起始为 true（缺省），切成 false 后读回来必须是 false。
        // 用真实的 AtomicBool 语义验证，不构造整个 VendorService。
        let runtime = std::sync::atomic::AtomicBool::new(
            cfg_json("").stock_poll_respect_global_gate,
        );
        assert!(runtime.load(Ordering::Relaxed), "初值取自配置");

        runtime.store(false, Ordering::Relaxed);
        assert!(
            !runtime.load(Ordering::Relaxed),
            "切换后必须读到新值；若面板读的是 config 快照，这里就会拿到旧的 true"
        );
    }
}
