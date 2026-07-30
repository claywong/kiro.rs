//! 卖家对接服务层：事件解析、提取入库、失效确认、告警计数
//!
//! 设计约束：提取数量一旦绑定就不可更改（卖家侧同订单号改 count 会 409），
//! 这是整个模块所有取舍的出发点。
//!
//! - 手动模式（默认）：入站 webhook **只落库不花钱**，扣费一律由面板显式触发。
//! - 自动模式：仅当上一轮 `all_keys_dead` 已确认「名下卖家 Key 全部失效」时，
//!   才在收到 `new_keys_available` 后自动提取，且只提最小数量。判定规则见
//!   [`super::auto`]。
//!
//! @author wangzhong

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::admin::AdminService;
use crate::http_client::ProxyConfig;
use crate::model::config::{TlsBackend, VendorConfig};

use super::auto;
use super::client::{VendorApiError, VendorClient};
// 本地新增模块单独成行，避免上游改动这批 use 时反复冲突。
use super::schedule;
use super::store::{
    IncomingEvent, PurchaseOutcome, PurchaseStatus, PurchaseTrigger, SharedVendorStore,
    ValidationStatus, VendorEventKind,
};

/// 提取入库的汇总结果（返回给前端）
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseImportResult {
    /// 本次实际绑定并提交的数量
    pub count: u32,
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

/// 服务层错误
#[derive(Debug)]
pub enum VendorServiceError {
    /// 未配置卖家对接
    NotConfigured,
    /// 事件不存在
    EventNotFound,
    /// 该事件不是 `new_keys_available`，没有可提取的订单号
    NotPurchasable,
    /// 该订单号已绑定其它数量，必须改用该值重试
    CountLocked { bound: u32 },
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
            Self::Upstream(e) => write!(f, "{e}"),
            Self::Storage(e) => write!(f, "本地存储错误: {e}"),
        }
    }
}

/// 卖家对接服务
pub struct VendorService {
    config: Option<VendorConfig>,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    store: SharedVendorStore,
    admin: Arc<AdminService>,
    /// 提取模式的运行时值。`config.auto_purchase` 只是启动快照，面板切换后
    /// 以本字段为准 —— 读它而不是读 config。
    auto_purchase: AtomicBool,
}

impl VendorService {
    pub fn new(
        config: Option<VendorConfig>,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        store: SharedVendorStore,
        admin: Arc<AdminService>,
    ) -> Self {
        let auto_purchase = config.as_ref().is_some_and(|c| c.auto_purchase);
        Self {
            config,
            proxy,
            tls_backend,
            store,
            admin,
            auto_purchase: AtomicBool::new(auto_purchase),
        }
    }

    pub fn store(&self) -> &SharedVendorStore {
        &self.store
    }

    /// 启动时的配置快照。`auto_purchase` 字段可能已被面板改过，
    /// 判断提取模式请用 [`Self::auto_purchase`]。
    pub fn config(&self) -> Option<&VendorConfig> {
        self.config.as_ref()
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

    /// 写回 config.json 的 `vendor.autoPurchase`。
    ///
    /// 重新从磁盘加载再改单个字段，避免把进程内的旧快照整体覆盖上去 ——
    /// 与 `AdminService::persist_log_governance_config` 同一套做法。
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
        let vendor = config
            .vendor
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("config.json 缺少 vendor 段，无法持久化提取模式"))?;
        vendor.auto_purchase = enabled;
        config
            .save()
            .with_context(|| format!("写入配置文件失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 校验入站路径 token。未配置或 token 为空一律拒绝。
    pub fn verify_path_token(&self, token: &str) -> bool {
        let Some(cfg) = &self.config else {
            return false;
        };
        if !cfg.inbound_enabled() {
            return false;
        }
        crate::common::auth::constant_time_eq(token, cfg.webhook_path_token.trim())
    }

    /// 构建出站客户端
    fn client(&self) -> Result<VendorClient, VendorServiceError> {
        let cfg = self.config.as_ref().ok_or(VendorServiceError::NotConfigured)?;
        VendorClient::new(cfg, self.proxy.as_ref(), self.tls_backend)
            .map_err(|_| VendorServiceError::NotConfigured)
    }

    /// 解析入站 payload。字段缺失时尽量保留原文，避免丢事件。
    pub fn parse_event(raw: &[u8]) -> Option<IncomingEvent> {
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

        Some(IncomingEvent {
            event_id,
            kind: VendorEventKind::from_str(event_type),
            purchase_order_id: obj
                .get("purchase_order_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string()),
            message: obj
                .get("message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            new_keys: obj.get("new_keys").and_then(|v| v.as_u64()).map(|v| v as u32),
            dead: obj.get("dead").and_then(|v| v.as_u64()).map(|v| v as u32),
            raw_payload: String::from_utf8_lossy(raw).to_string(),
        })
    }

    /// 按事件手动提取并入库。
    ///
    /// `count` 为本次希望提取的数量；若该事件此前已绑定过其它数量，直接返回
    /// [`VendorServiceError::CountLocked`]，不会向卖家发请求 —— 避免白撞一次 409。
    pub async fn purchase_for_event(
        &self,
        event_id: &str,
        count: u32,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        self.purchase_for_event_with_trigger(event_id, count, PurchaseTrigger::Manual)
            .await
    }

    /// 同 [`Self::purchase_for_event`]，但记录触发方式（自动模式用）
    pub async fn purchase_for_event_with_trigger(
        &self,
        event_id: &str,
        count: u32,
        trigger: PurchaseTrigger,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let client = self.client()?;

        let record = self
            .store
            .get_event(event_id)
            .map_err(|e| VendorServiceError::Storage(e.to_string()))?
            .ok_or(VendorServiceError::EventNotFound)?;

        let order_id = record
            .purchase_order_id
            .clone()
            .ok_or(VendorServiceError::NotPurchasable)?;

        // 抢占绑定：并发点击只有一个能拿到本次 count，其余得到已绑定值
        let effective = match self
            .store
            .bind_count(event_id, count)
            .map_err(|e| VendorServiceError::Storage(e.to_string()))?
        {
            Ok(v) => v,
            Err(bound) if bound == count => bound, // 同数量重试，卖家侧幂等重放
            Err(bound) => return Err(VendorServiceError::CountLocked { bound }),
        };

        self.purchase_and_import(&client, event_id, &order_id, effective, trigger)
            .await
    }

    /// 不依赖 webhook 事件的主动提取（自行生成订单号）。
    /// 不写事件表 —— 没有对应事件行可绑定，幂等由调用方复用订单号保证。
    pub async fn purchase_ad_hoc(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let client = self.client()?;
        let resp = client
            .purchase(count, client_order_id)
            .await
            .map_err(VendorServiceError::Upstream)?;
        let remaining = resp.remaining;
        let keys: Vec<String> = resp.keys.into_iter().map(|k| k.key).collect();
        let purchased = resp.purchased.max(keys.len() as u32);
        let outcome = self.import_keys(keys, client_order_id).await;
        Ok(PurchaseImportResult {
            count,
            purchased,
            imported: outcome.imported,
            duplicated: outcome.duplicated,
            failed: outcome.failed,
            remaining,
            error: outcome.last_error,
        })
    }

    /// 提取 + 入库 + 结果写回事件行
    async fn purchase_and_import(
        &self,
        client: &VendorClient,
        event_id: &str,
        order_id: &str,
        count: u32,
        trigger: PurchaseTrigger,
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let resp = match client.purchase(count, order_id).await {
            Ok(r) => r,
            Err(e) => {
                // 记失败但保留 bound_count，便于按同一数量重试
                let outcome = PurchaseOutcome {
                    last_error: Some(e.to_string()),
                    ..Default::default()
                };
                let _ =
                    self.store
                        .finish_purchase(event_id, PurchaseStatus::Failed, trigger, &outcome);
                return Err(VendorServiceError::Upstream(e));
            }
        };

        let remaining = resp.remaining;
        let keys: Vec<String> = resp.keys.into_iter().map(|k| k.key).collect();
        let purchased = resp.purchased.max(keys.len() as u32);

        let mut outcome = self.import_keys(keys, order_id).await;
        outcome.purchased = purchased;

        let status = if outcome.failed > 0 && outcome.imported == 0 {
            PurchaseStatus::Failed
        } else {
            PurchaseStatus::Done
        };
        if let Err(e) = self
            .store
            .finish_purchase(event_id, status, trigger, &outcome)
        {
            tracing::warn!("写回提取结果失败 event_id={}: {}", event_id, e);
        }

        Ok(PurchaseImportResult {
            count,
            purchased,
            imported: outcome.imported,
            duplicated: outcome.duplicated,
            failed: outcome.failed,
            remaining,
            error: outcome.last_error,
        })
    }

    /// 把提取到的 `ksk_` Key 逐条入库。复用 admin 的 `import_one_credential`：
    /// 去重、验活、失败回滚的逻辑与批量导入完全一致。
    async fn import_keys(&self, keys: Vec<String>, order_id: &str) -> PurchaseOutcome {
        let cfg = self.config.as_ref();
        let groups = cfg.map(|c| c.default_groups.clone()).unwrap_or_default();
        let rpm_limit = cfg.map(|c| c.default_rpm_limit).unwrap_or(10);
        self.import_keys_with(keys, order_id, groups, rpm_limit).await
    }

    /// 指定分组与 RPM 的入库。次级卖家（kiroapp）有自己的默认值，
    /// 不能套用主卖家配置，故把这两项提成参数，具体实现见 [`super::import`]。
    async fn import_keys_with(
        &self,
        keys: Vec<String>,
        order_id: &str,
        groups: Vec<String>,
        rpm_limit: u32,
    ) -> PurchaseOutcome {
        super::import::import_keys(
            &self.admin,
            keys,
            &format!("vendor:{order_id}"),
            groups,
            rpm_limit,
        )
        .await
    }

    // ============ 自动模式 ============

    /// 自动模式单次提取上限。配了时段表且当前时刻命中时以该段为准。
    ///
    /// 时刻取本地时区（与 usageStats 一致），容器内需正确设置 `TZ`，
    /// 否则「下午」会按 UTC 判定、偏 8 小时。
    pub fn auto_max_count(&self) -> u32 {
        let Some(cfg) = self.config.as_ref() else {
            return 1;
        };
        schedule::max_count_at(
            &cfg.auto_purchase_schedule,
            cfg.auto_purchase_max_count,
            chrono::Local::now().time(),
        )
    }

    /// 当前命中的时段描述（如 `14:00–23:00`），供面板说明「为什么是这个数」
    pub fn auto_active_window(&self) -> Option<String> {
        let cfg = self.config.as_ref()?;
        schedule::active_window_label(&cfg.auto_purchase_schedule, chrono::Local::now().time())
    }

    /// 从凭据池取出卖家 Key 的状态切片
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
        let c = auto::census(&self.vendor_key_states());
        let (status, detail) = auto::conclude(c, window_expired);
        if let Err(e) = self.store.set_validation(event_id, status, &detail) {
            tracing::warn!("写入失效确认结论失败 event_id={}: {}", event_id, e);
        }
        tracing::info!(
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
    pub fn spawn_auto_purchase(self: &Arc<Self>, event_id: String, new_keys: Option<u32>) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(reason) = svc.try_auto_purchase(&event_id, new_keys).await {
                tracing::info!(event_id = %event_id, reason = %reason, "自动提取已跳过");
                if let Err(e) = svc.store.record_skip(&event_id, &reason) {
                    tracing::warn!("记录跳过原因失败 event_id={}: {}", event_id, e);
                }
            }
        });
    }

    /// 自动提取的实际流程。`Err(原因)` 表示本次不提取，原因会写回事件行。
    async fn try_auto_purchase(&self, event_id: &str, new_keys: Option<u32>) -> Result<(), String> {
        // 1. 失效确认：必须有一条已确认失效、且尚未被消费的 all_keys_dead
        let dead = self
            .store
            .latest_dead_event()
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

        // 2. 数量：三者取最小，为 0 则无可提
        let stock = self
            .stock()
            .await
            .map_err(|e| format!("查询可提取上限失败: {e}"))?;
        // 只读一次：时段边界附近两次调用可能拿到不同值，会让判定与文案对不上
        let configured_max = self.auto_max_count();
        let count = auto::decide_count(new_keys, stock, configured_max);
        if count == 0 {
            return Err(format!(
                "可提取数量为 0（事件声明 {}，卖家上限 {}，配置上限 {}）",
                new_keys.map(|v| v.to_string()).unwrap_or("-".into()),
                stock,
                configured_max
            ));
        }

        // 3. 消费确认额度。抢占式，确保一次确认只授权一轮提取
        let consumed = self
            .store
            .consume_validation(&dead.event_id)
            .map_err(|e| format!("消费失效确认失败: {e}"))?;
        if !consumed {
            return Err("失效确认已被其他自动提取取用".to_string());
        }

        // 4. 下单。此处开始不可逆
        tracing::info!(
            event_id = %event_id,
            count,
            dead_event = %dead.event_id,
            "自动提取开始"
        );
        match self
            .purchase_for_event_with_trigger(event_id, count, PurchaseTrigger::Auto)
            .await
        {
            Ok(r) => {
                tracing::info!(
                    event_id = %event_id,
                    purchased = r.purchased,
                    imported = r.imported,
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

    pub async fn stock(&self) -> Result<u32, VendorServiceError> {
        let client = self.client()?;
        client
            .stock()
            .await
            .map(|s| s.max)
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn profile(&self) -> Result<super::client::ProfileResponse, VendorServiceError> {
        let client = self.client()?;
        client.profile().await.map_err(VendorServiceError::Upstream)
    }

    /// 卖家系统状态：存活 / 失效 / 存货 Key 数
    pub async fn system_status(
        &self,
    ) -> Result<super::client::VendorSystemStatus, VendorServiceError> {
        let client = self.client()?;
        client
            .system_status()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 最近 50 条提取订单，用于跟本地事件对账、发现漏提
    pub async fn purchase_orders(
        &self,
    ) -> Result<Vec<super::client::PurchaseOrder>, VendorServiceError> {
        let client = self.client()?;
        client
            .purchase_orders()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    /// 卖家近期开号批次与平均间隔
    pub async fn gen_logs(&self) -> Result<super::client::GenLogsResponse, VendorServiceError> {
        let client = self.client()?;
        client.gen_logs().await.map_err(VendorServiceError::Upstream)
    }

    pub async fn redeem(
        &self,
        code: &str,
    ) -> Result<super::client::RedeemResponse, VendorServiceError> {
        let client = self.client()?;
        client
            .redeem(code)
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn test_webhook(&self) -> Result<serde_json::Value, VendorServiceError> {
        let client = self.client()?;
        client
            .test_webhook()
            .await
            .map_err(VendorServiceError::Upstream)
    }

    pub async fn set_webhook_url(&self, url: &str) -> Result<(), VendorServiceError> {
        let client = self.client()?;
        client
            .set_webhook_url(url)
            .await
            .map_err(VendorServiceError::Upstream)
    }
}

/// `event_id` 缺失时的兜底 ID：原文 SHA256 前 32 位十六进制。
/// 同一份 payload 重投得到同一个 ID，仍能去重。
fn fallback_event_id(raw: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(raw);
    hex::encode(&digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析新key就绪事件() {
        let raw = r#"{"event":"new_keys_available","event_id":"abc123","purchase_order_id":"0123456789abcdef0123456789abcdef","message":"新一轮 10 个 Key 已就绪","new_keys":10}"#;
        let e = VendorService::parse_event(raw.as_bytes()).unwrap();
        assert_eq!(e.event_id, "abc123");
        assert_eq!(e.kind, VendorEventKind::NewKeysAvailable);
        assert_eq!(
            e.purchase_order_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(e.new_keys, Some(10));
        assert_eq!(e.dead, None);
    }

    #[test]
    fn 解析全部失效事件() {
        let raw = r#"{"event":"all_keys_dead","event_id":"def456","message":"本轮全部 5 个 Key 已失效","dead":5}"#;
        let e = VendorService::parse_event(raw.as_bytes()).unwrap();
        assert_eq!(e.kind, VendorEventKind::AllKeysDead);
        assert_eq!(e.dead, Some(5));
        assert!(e.purchase_order_id.is_none());
    }

    #[test]
    fn 缺失event_id时用原文哈希兜底且稳定() {
        let raw = br#"{"event":"all_keys_dead","dead":3}"#;
        let a = VendorService::parse_event(raw).unwrap();
        let b = VendorService::parse_event(raw).unwrap();
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.event_id.len(), 32);
    }

    #[test]
    fn 非法json返回none() {
        assert!(VendorService::parse_event(b"not json").is_none());
        // 顶层非对象也拒绝
        assert!(VendorService::parse_event(b"[1,2,3]").is_none());
    }

    #[test]
    fn 未知事件类型归为unknown() {
        let raw = br#"{"event":"brand_new","event_id":"x1"}"#;
        let e = VendorService::parse_event(raw).unwrap();
        assert_eq!(e.kind, VendorEventKind::Unknown);
    }
}
