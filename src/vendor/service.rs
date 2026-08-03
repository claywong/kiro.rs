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

use crate::admin::AdminService;
use crate::http_client::ProxyConfig;
use crate::model::config::{TlsBackend, VendorConfig};

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
    IncomingEvent, PurchaseOutcome, PurchaseStatus, PurchaseTrigger, SharedVendorStore,
    ValidationStatus, VendorEventKind,
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
        Self {
            config,
            proxy,
            tls_backend,
            store,
            admin,
            auto_purchase: AtomicBool::new(auto_purchase),
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
        // 来源渠道带上供应商 id，便于按家盘点与对账
        let source_channel = format!("{}{}:{}", auto::VENDOR_CHANNEL_PREFIX, self.vendor_id(), order_id);

        let keys: Vec<String> = resp.keys.iter()
            .filter(|k| !k.key.trim().is_empty())
            .map(|k| k.key.clone())
            .collect();

        // 根据实际成交区域设置 api_region：eu 需要 eu-central-1，us 或不分区用默认
        let api_region = resp.zone.as_deref().and_then(|z| {
            if z == "eu" {
                Some("eu-central-1".to_string())
            } else {
                None
            }
        });

        super::import::import_keys(
            &self.admin,
            keys,
            &source_channel,
            groups,
            rpm_limit,
            api_region,
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

    /// 自动提取。仅在自动模式 + 上一轮失效已确认时真正下单。
    ///
    /// 检查顺序按「代价从小到大、可逆到不可逆」排列：先读本地确认结论（零成本），
    /// 再查卖家可提取上限（一次出站），最后才 `bind_count` —— 绑定是唯一不可逆的
    /// 一步，之前任何一环给出否定结论都只记跳过，订单号仍留给手动提取。
    ///
    /// 多家并发时还要过 [`super::pool_gate`] 的总量闸，见 `try_auto_purchase`。
    pub fn spawn_auto_purchase(self: &Arc<Self>, event_id: String, new_keys: Option<u32>) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(reason) = svc.try_auto_purchase(&event_id, new_keys).await {
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

    /// 自动提取的实际流程。`Err(原因)` 表示本次不提取，原因会写回事件行。
    async fn try_auto_purchase(&self, event_id: &str, new_keys: Option<u32>) -> Result<(), String> {
        let vid = self.vendor_id();

        // 1. 失效确认：必须有一条**本供应商的**、已确认失效且尚未被消费的 all_keys_dead
        let dead = self
            .store
            .latest_dead_event(vid)
            .map_err(|e| format!("读取失效确认记录失败: {e}"))?
            .ok_or_else(|| "尚未收到「全部失效」事件，无法确认旧 Key 已失效".to_string())?;

        let status = dead
            .validation_status
            .as_deref()
            .and_then(ValidationStatus::from_str);
        match status {
            Some(ValidationStatus::ConfirmedDead) => {}
            Some(ValidationStatus::Pending) => {
                return Err("旧 Key 失效确认仍在观察中，本轮不自动提取".to_string());
            }
            Some(ValidationStatus::StillAlive) => {
                return Err(dead
                    .validation_detail
                    .unwrap_or_else(|| "本地仍有健康的卖家 Key".to_string()));
            }
            Some(ValidationStatus::Inconclusive) | None => {
                return Err(dead
                    .validation_detail
                    .unwrap_or_else(|| "旧 Key 是否失效无法确认".to_string()));
            }
        }
        if dead.validation_used {
            return Err("上一次失效确认已用于此前的自动提取，需新的失效事件".to_string());
        }

        // 2. 全局提取锁。必须在盘点之前拿到，并持有到下单+导入结束 ——
        //    否则三家并发时会同时读到「池里 0 个存活」再同时下单，闸门形同虚设。
        //    未启用池闸时不必付串行化的代价，直接跳过取锁。
        let _gate = if self.pool_gate.enabled() {
            Some(self.pool_gate.acquire().await?)
        } else {
            None
        };

        // 3. 全局池量闸。零成本本地读，故排在出站查库存之前。
        //    这里重新盘点而非复用步骤 1 的结论：等锁期间别家可能已经补过货了，
        //    锁前的池量视图已经过期。
        self.pool_gate
            .check(auto::pool_alive(&self.vendor_key_states()))?;

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

        // 5. 消费确认额度。抢占式，确保一次确认只授权一轮提取
        let consumed = self
            .store
            .consume_validation(vid, &dead.event_id)
            .map_err(|e| format!("消费失效确认失败: {e}"))?;
        if !consumed {
            return Err("失效确认已被其他自动提取取用".to_string());
        }

        // 6. 下单。此处开始不可逆。`_gate` 持有到本函数结束，
        //    确保后来者盘点时能看到这批新导入的 Key。
        tracing::info!(
            vendor_id = %vid,
            event_id = %event_id,
            count,
            zone = ?zone_hint,
            dead_event = %dead.event_id,
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
                },
                PurchasedKey {
                    key: "sk-b".to_string(),
                    account: None,
                    password: None,
                    issuer_url: None,
                    price: Some(40.0),
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
