//! 次级卖家 kiroapp 服务层
//!
//! 只做两件事：查状态（库存 + 余额）、手动提取一个 Key 并入库。
//!
//! 刻意不做的事及原因：
//! - **不落事件表**：对方没有 webhook，没有事件可绑定；
//! - **不自动提取**：claim 无幂等键，也无数量参数，交给定时任务风险大于收益；
//! - **不重试**：claim 超时无法区分「未扣费」与「已扣费但响应丢失」，重发会二次扣费。
//!
//! @author wangzhong

use std::sync::Arc;

use crate::admin::AdminService;
use crate::http_client::ProxyConfig;
use crate::model::config::{KiroappConfig, TlsBackend};

use super::kiroapp::{KiroappApiError, KiroappBalance, KiroappClient, KiroappStock};

/// 单次提取的结果
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroappClaimResult {
    /// 从响应里识别出的 Key 数
    pub claimed: u32,
    /// 成功入库数
    pub imported: u32,
    /// 本地已存在而跳过数
    pub duplicated: u32,
    /// 入库失败数
    pub failed: u32,
    /// 首条失败原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 一个 Key 都没识别出来时回传原始响应，供面板提示人工核对。
    ///
    /// 这种情况意味着**可能已经扣费但 Key 没入库**，必须让人看见原文，
    /// 不能静默当成「提取到 0 个」。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// 服务层错误
#[derive(Debug)]
pub enum KiroappServiceError {
    /// 未配置 kiroapp 对接
    NotConfigured,
    /// 调用 kiroapp 接口失败
    Upstream(KiroappApiError),
}

impl std::fmt::Display for KiroappServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => {
                write!(f, "未配置 kiroapp 对接（kiroapp.baseUrl / kiroapp.apiKey）")
            }
            Self::Upstream(e) => write!(f, "{e}"),
        }
    }
}

/// kiroapp 对接服务
pub struct KiroappService {
    config: Option<KiroappConfig>,
    proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    admin: Arc<AdminService>,
}

impl KiroappService {
    pub fn new(
        config: Option<KiroappConfig>,
        proxy: Option<ProxyConfig>,
        tls_backend: TlsBackend,
        admin: Arc<AdminService>,
    ) -> Self {
        Self {
            config,
            proxy,
            tls_backend,
            admin,
        }
    }

    pub fn config(&self) -> Option<&KiroappConfig> {
        self.config.as_ref()
    }

    /// 是否已配置且可用
    pub fn enabled(&self) -> bool {
        self.config.as_ref().is_some_and(|c| c.enabled())
    }

    fn client(&self) -> Result<KiroappClient, KiroappServiceError> {
        let cfg = self
            .config
            .as_ref()
            .ok_or(KiroappServiceError::NotConfigured)?;
        KiroappClient::new(cfg, self.proxy.as_ref(), self.tls_backend)
            .map_err(|_| KiroappServiceError::NotConfigured)
    }

    /// 查库存
    pub async fn stock(&self) -> Result<KiroappStock, KiroappServiceError> {
        self.client()?
            .stock()
            .await
            .map_err(KiroappServiceError::Upstream)
    }

    /// 查余额
    pub async fn balance(&self) -> Result<KiroappBalance, KiroappServiceError> {
        self.client()?
            .balance()
            .await
            .map_err(KiroappServiceError::Upstream)
    }

    /// 提取一个 Key 并入库。会真实扣费，调用方须已做二次确认。
    pub async fn claim(&self) -> Result<KiroappClaimResult, KiroappServiceError> {
        let client = self.client()?;
        let cfg = self
            .config
            .as_ref()
            .ok_or(KiroappServiceError::NotConfigured)?;

        let outcome = client.claim().await.map_err(KiroappServiceError::Upstream)?;
        let claimed = outcome.keys.len() as u32;

        // 一个都没捞到：接口成功返回但结构不认识，可能已扣费。原样回传原文，
        // 让面板显式提示人工去卖家侧核对，不要当成正常的「0 个」。
        if outcome.keys.is_empty() {
            tracing::warn!(
                "kiroapp claim 返回中未识别出 ksk_ Key，可能已扣费: {}",
                outcome.raw
            );
            return Ok(KiroappClaimResult {
                claimed: 0,
                imported: 0,
                duplicated: 0,
                failed: 0,
                error: Some("响应中未识别出 Key，请到卖家侧核对是否已扣费".to_string()),
                raw: Some(outcome.raw),
            });
        }

        let result = super::import::import_keys(
            &self.admin,
            outcome.keys,
            "kiroapp",
            cfg.default_groups.clone(),
            cfg.default_rpm_limit,
        )
        .await;

        Ok(KiroappClaimResult {
            claimed,
            imported: result.imported,
            duplicated: result.duplicated,
            failed: result.failed,
            error: result.last_error,
            raw: None,
        })
    }
}
