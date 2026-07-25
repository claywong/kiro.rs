//! 卖家对接服务层：事件解析、手动提取入库、告警计数
//!
//! 设计约束：入站 webhook **只落库不花钱**。所有扣费动作（`/api/my/purchase`）
//! 一律由管理面板显式触发，且提取数量一旦绑定就不可更改（卖家侧同订单号改
//! count 会 409）。
//!
//! @author wangzhong

use std::sync::Arc;

use crate::admin::AdminService;
use crate::admin::types::AddCredentialRequest;
use crate::http_client::ProxyConfig;
use crate::model::config::{TlsBackend, VendorConfig};

use super::client::{VendorApiError, VendorClient};
use super::store::{
    IncomingEvent, PurchaseOutcome, PurchaseStatus, SharedVendorStore, VendorEventKind,
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
}

impl VendorService {
    pub fn new(
        config: Option<VendorConfig>,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        store: SharedVendorStore,
        admin: Arc<AdminService>,
    ) -> Self {
        Self {
            config,
            proxy,
            tls_backend,
            store,
            admin,
        }
    }

    pub fn store(&self) -> &SharedVendorStore {
        &self.store
    }

    pub fn config(&self) -> Option<&VendorConfig> {
        self.config.as_ref()
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

        self.purchase_and_import(&client, event_id, &order_id, effective)
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
    ) -> Result<PurchaseImportResult, VendorServiceError> {
        let resp = match client.purchase(count, order_id).await {
            Ok(r) => r,
            Err(e) => {
                // 记失败但保留 bound_count，便于按同一数量重试
                let outcome = PurchaseOutcome {
                    last_error: Some(e.to_string()),
                    ..Default::default()
                };
                let _ = self
                    .store
                    .finish_purchase(event_id, PurchaseStatus::Failed, &outcome);
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
        if let Err(e) = self.store.finish_purchase(event_id, status, &outcome) {
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
        let purchase_cost = cfg.and_then(|c| c.default_purchase_cost);
        let rpm_limit = cfg.map(|c| c.default_rpm_limit).unwrap_or(10);

        let mut outcome = PurchaseOutcome::default();
        for key in keys {
            let req = AddCredentialRequest {
                refresh_token: None,
                access_token: None,
                profile_arn: None,
                expires_at: None,
                auth_method: "api_key".to_string(),
                provider: None,
                client_id: None,
                client_secret: None,
                start_url: None,
                token_endpoint: None,
                issuer_url: None,
                scopes: None,
                priority: 0,
                rpm_limit,
                region: None,
                auth_region: None,
                api_region: None,
                machine_id: None,
                email: None,
                proxy_url: None,
                proxy_username: None,
                proxy_password: None,
                kiro_api_key: Some(key),
                endpoint: None,
                groups: groups.clone(),
                source_channel: Some(format!("vendor:{order_id}")),
                purchase_cost,
            };

            let result = self.admin.import_one_credential(req, true).await;
            use crate::admin::ImportStatus;
            match result.status {
                ImportStatus::Verified | ImportStatus::Imported => outcome.imported += 1,
                ImportStatus::Duplicate => outcome.duplicated += 1,
                ImportStatus::Failed => {
                    outcome.failed += 1;
                    if outcome.last_error.is_none() {
                        outcome.last_error = result.error.clone();
                    }
                }
            }
        }
        outcome
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
