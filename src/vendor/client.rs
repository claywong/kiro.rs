//! 卖家（Key 供应商）出站 API 客户端
//!
//! 覆盖 `/api/my/*` 系列接口：提取 Key、查库存 / 余额 / 订单、兑换充值、
//! 维护 webhook URL。所有请求带 `X-API-Key: usr-xxx`。
//!
//! @author wangzhong

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::http_client::{self, ProxyConfig};
use crate::model::config::{TlsBackend, VendorConfig};

/// 出站请求超时（秒）。提取 Key 需要卖家侧现场生成，给足时间。
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// 单个 Key 条目。卖家在 `/api/my/purchase` 响应里只保证 `key` 字段，
/// 其余元数据（status / created_at）此处不需要，不做解析。
#[derive(Debug, Clone, Deserialize)]
pub struct VendorKey {
    pub key: String,
}

/// `POST /api/my/purchase` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct PurchaseResponse {
    #[serde(default)]
    pub purchased: u32,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub keys: Vec<VendorKey>,
}

/// `GET /api/my/stock` 响应
#[derive(Debug, Clone, Deserialize)]
pub struct StockResponse {
    #[serde(default)]
    pub max: u32,
}

/// `GET /api/my/profile` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProfileResponse {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub remaining: Option<f64>,
    #[serde(default)]
    pub used_quota: Option<f64>,
    #[serde(default)]
    pub webhook_url: Option<String>,
}

/// `POST /api/my/redeem` 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RedeemResponse {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub quota: Option<f64>,
    #[serde(default)]
    pub previous_quota: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub created_by_name: Option<String>,
    #[serde(default)]
    pub redeemed_at: Option<String>,
    /// true 表示这张码此前已兑换过，本次未改动余额
    #[serde(default)]
    pub replayed: bool,
}

/// `GET /api/my/purchase-orders` 单条订单
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PurchaseOrder {
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub requested: Option<u32>,
    #[serde(default)]
    pub purchased: Option<u32>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// 卖家返回的错误体：`{"error":"错误说明"}`
#[derive(Debug, Deserialize)]
struct VendorError {
    #[serde(default)]
    error: Option<String>,
}

/// 出站调用失败，携带 HTTP 状态码便于上层按 403/404/409 分别处理
#[derive(Debug)]
pub struct VendorApiError {
    pub status: Option<u16>,
    pub message: String,
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

/// 卖家 API 客户端。复用全局代理与 TLS 后端配置。
pub struct VendorClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl VendorClient {
    /// 按配置构建客户端。`base_url` / `api_key` 为空时返回 Err。
    pub fn new(
        vendor: &VendorConfig,
        proxy: Option<&ProxyConfig>,
        tls_backend: TlsBackend,
    ) -> anyhow::Result<Self> {
        if !vendor.outbound_enabled() {
            anyhow::bail!("卖家配置不完整（baseUrl / apiKey 为空）");
        }
        let http = http_client::build_client(proxy, REQUEST_TIMEOUT_SECS, tls_backend)
            .context("构建卖家 API 客户端失败")?;
        Ok(Self {
            http,
            base_url: vendor.normalized_base_url().to_string(),
            api_key: vendor.api_key.trim().to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// 统一处理响应：非 2xx 时解析 `{"error":...}` 并带上状态码。
    async fn parse<T: for<'de> Deserialize<'de>>(
        resp: reqwest::Response,
    ) -> Result<T, VendorApiError> {
        let status = resp.status();
        let body = resp.text().await.map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("读取响应体失败: {e}"),
        })?;

        if !status.is_success() {
            let message = serde_json::from_str::<VendorError>(&body)
                .ok()
                .and_then(|e| e.error)
                .unwrap_or_else(|| truncate(&body, 300));
            return Err(VendorApiError {
                status: Some(status.as_u16()),
                message,
            });
        }

        serde_json::from_str::<T>(&body).map_err(|e| VendorApiError {
            status: Some(status.as_u16()),
            message: format!("解析响应失败: {e}；原文片段: {}", truncate(&body, 200)),
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, VendorApiError> {
        let resp = self
            .http
            .get(self.url(path))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        Self::parse(resp).await
    }

    /// `POST /api/my/purchase` —— 提取 Key。
    ///
    /// `client_order_id` 必须是 32 位十六进制串，并且**同一订单号必须始终配同一个
    /// `count`**：卖家侧对「相同订单号 + 相同 count」幂等重放，改 count 会返回 409。
    /// 因此调用方需持久化首次决定的 count，重试时原样复用。
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<PurchaseResponse, VendorApiError> {
        let resp = self
            .http
            .post(self.url("/api/my/purchase"))
            .header("X-API-Key", &self.api_key)
            .json(&serde_json::json!({
                "count": count,
                "client_order_id": client_order_id,
            }))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        Self::parse(resp).await
    }

    /// `GET /api/my/stock` —— 本轮最大可提取数量
    pub async fn stock(&self) -> Result<StockResponse, VendorApiError> {
        self.get("/api/my/stock").await
    }

    /// `GET /api/my/profile` —— 余额与 webhook 配置
    pub async fn profile(&self) -> Result<ProfileResponse, VendorApiError> {
        self.get("/api/my/profile").await
    }

    /// `GET /api/my/purchase-orders` —— 最近 50 条提取订单，用于跟本地事件对账
    pub async fn purchase_orders(&self) -> Result<Vec<PurchaseOrder>, VendorApiError> {
        self.get("/api/my/purchase-orders").await
    }

    /// `POST /api/my/redeem` —— 兑换码充值。
    /// 卖家侧对「同账号 + 同码」幂等，超时重试原样重发即可。
    pub async fn redeem(&self, code: &str) -> Result<RedeemResponse, VendorApiError> {
        let resp = self
            .http
            .post(self.url("/api/my/redeem"))
            .header("X-API-Key", &self.api_key)
            .json(&serde_json::json!({ "code": code }))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        Self::parse(resp).await
    }

    /// `PUT /api/my/webhook` —— 更新卖家侧保存的 webhook URL
    pub async fn set_webhook_url(&self, webhook_url: &str) -> Result<(), VendorApiError> {
        let resp = self
            .http
            .put(self.url("/api/my/webhook"))
            .header("X-API-Key", &self.api_key)
            .json(&serde_json::json!({ "webhook_url": webhook_url }))
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        Self::parse::<serde_json::Value>(resp).await.map(|_| ())
    }

    /// `POST /api/my/webhook/test` —— 让卖家往已保存的 URL 推一条测试消息
    pub async fn test_webhook(&self) -> Result<serde_json::Value, VendorApiError> {
        let resp = self
            .http
            .post(self.url("/api/my/webhook/test"))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| VendorApiError {
                status: None,
                message: e.to_string(),
            })?;
        Self::parse(resp).await
    }
}

/// 按字符边界截断，避免把多字节 UTF-8 切坏
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(base: &str, key: &str, token: &str) -> VendorConfig {
        VendorConfig {
            base_url: base.to_string(),
            api_key: key.to_string(),
            webhook_path_token: token.to_string(),
            default_groups: vec![],
            default_purchase_cost: None,
            default_rpm_limit: 10,
        }
    }

    #[test]
    fn base_url_去掉末尾斜杠() {
        let c = cfg("https://v.example.com///", "usr-x", "t");
        assert_eq!(c.normalized_base_url(), "https://v.example.com");
    }

    #[test]
    fn 启用判定() {
        assert!(cfg("https://v", "usr-x", "t").inbound_enabled());
        // 缺 token：出站可用，入站不可用
        let no_token = cfg("https://v", "usr-x", "");
        assert!(no_token.outbound_enabled());
        assert!(!no_token.inbound_enabled());
        // 缺 key：两者都不可用
        assert!(!cfg("https://v", "  ", "t").outbound_enabled());
        assert!(!cfg("", "usr-x", "t").outbound_enabled());
    }

    #[test]
    fn truncate_不切坏多字节字符() {
        assert_eq!(truncate("中文测试", 2), "中文…");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn 客户端拒绝不完整配置() {
        let c = cfg("", "", "");
        assert!(VendorClient::new(&c, None, TlsBackend::Rustls).is_err());
    }
}
