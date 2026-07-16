//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 建连超时（DNS + TCP + TLS 握手）。所有 Client 统一应用：连不上的上游 / 挂掉的
/// 代理会在此时限内快速失败，而不是卡在总超时里干等。连上后即失效。
const CONNECT_TIMEOUT_SECS: u64 = 50;

/// 流式 / API Client 的读超时（相邻两次读数据之间的最大间隔）。既是首字节（TTFB）
/// 保护，也是流中途的空闲保护：上游连上后长时间不吐数据即报错，避免僵死连接拖满总
/// 超时。只要上游持续吐字节就不触发，因此不会误杀正常的长响应。
const STREAM_READ_TIMEOUT_SECS: u64 = 120;

/// 构建基础 ClientBuilder（统一 TLS 后端、代理、建连超时、总超时）。
fn base_builder(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS));

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder)
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 总超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client（含统一的建连超时）
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    Ok(base_builder(proxy, timeout_secs, tls_backend)?.build()?)
}

/// 构建流式 / API 用 Client：在 [`build_client`] 基础上额外加读超时（空闲 / 首字节
/// 保护）。供上游 API（流式 / 非流式）与 MCP 调用使用——这些场景都要读上游响应体，
/// 需要空闲超时兜住僵死连接。
pub fn build_streaming_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let builder = base_builder(proxy, timeout_secs, tls_backend)?
        .read_timeout(Duration::from_secs(STREAM_READ_TIMEOUT_SECS));
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_streaming_client() {
        // 流式 Client（含读超时）应能正常构建，代理有无均可
        let client = build_streaming_client(None, 720, TlsBackend::Rustls);
        assert!(client.is_ok());

        let config = ProxyConfig::new("socks5://127.0.0.1:1080");
        let client = build_streaming_client(Some(&config), 720, TlsBackend::Rustls);
        assert!(client.is_ok());
    }
}
