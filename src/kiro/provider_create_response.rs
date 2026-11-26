//! CreateResponse 调用辅助（本地扩展）
//!
//! 把「构造 CreateResponse 请求」和「429 回退发送」从 `provider.rs` 的重试主循环里
//! 拆出来，主循环只保留几行分支。协议细节见 [`crate::kiro::create_response`]。
//!
//! @author wangzhong

use std::time::Duration;

use reqwest::Client;

use crate::kiro::create_response;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::provider::KiroProvider;
use crate::model::config::CreateResponseMode;

/// CreateResponse 上游对同一账号有严格的并发窗口，连发会 429；等一下再试一次即可吸收。
const CONCURRENCY_BACKOFF: Duration = Duration::from_millis(1500);

/// 回退日志与链路追踪里使用的端点名。
pub(super) const CREATE_RESPONSE_ENDPOINT: &str = "create-response";

impl KiroProvider {
    pub fn create_response_mode(&self) -> CreateResponseMode {
        self.token_manager().create_response_mode()
    }

    fn create_response_url(&self, credentials: &KiroCredentials) -> String {
        #[cfg(test)]
        if let Some(url) = self.create_response_url_override.lock().clone() {
            return url;
        }
        let config = self.token_manager().config();
        create_response::url_for_region(credentials.effective_api_region(config))
    }

    /// 构造发往 CreateResponse 的请求。`kiro_body` 是端点转换前的 Kiro 请求体。
    pub(super) fn create_response_request(
        &self,
        client: &Client,
        credentials: &KiroCredentials,
        token: &str,
        kiro_body: &str,
    ) -> anyhow::Result<reqwest::Request> {
        let config = self.token_manager().config();
        let arn = credentials.streaming_profile_arn();
        let body = create_response::convert_request(kiro_body, arn.as_deref())?;
        let url = self.create_response_url(credentials);
        let mut builder = client.post(url).body(body);
        for (name, value) in create_response::headers(credentials, token, config) {
            builder = builder.header(name, value);
        }
        builder
            .build()
            .map_err(|e| anyhow::anyhow!("构建 CreateResponse 请求失败: {}", e))
    }

    /// 在同一凭据上改用 CreateResponse 重发。成功返回已转写成 event-stream 的响应；
    /// 任何失败返回 `None` 并记日志，由调用方继续走原有的 429 处理。
    pub(super) async fn send_create_response_fallback(
        &self,
        client: &Client,
        credentials: &KiroCredentials,
        token: &str,
        kiro_body: &str,
        credential_id: u64,
    ) -> Option<reqwest::Response> {
        for attempt in 0..2 {
            let request = match self.create_response_request(client, credentials, token, kiro_body) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!("凭据 #{} CreateResponse 请求构造失败: {}", credential_id, error);
                    return None;
                }
            };
            let response = match client.execute(request).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!("凭据 #{} CreateResponse 请求发送失败: {}", credential_id, error);
                    return None;
                }
            };
            let status = response.status();
            if status.is_success() {
                return Some(create_response::transcode_response(response));
            }
            tracing::warn!("凭据 #{} CreateResponse 返回 {}", credential_id, status);
            if status.as_u16() == 429 && attempt == 0 {
                tokio::time::sleep(CONCURRENCY_BACKOFF).await;
                continue;
            }
            return None;
        }
        None
    }
}
