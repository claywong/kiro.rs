//! 外部账号 `concurrency`（并发上限）的推送协议。
//!
//! 与 [`super::schedulable_client`] 共用认证头、退避与重试口径，区别在于目标端点：
//! 对方没有单独的改并发接口，只能走通用更新 `PUT /api/v1/admin/accounts/{id}`。
//!
//! 走通用更新有个坑值得记下：该请求体里 `name` / `type` / `status` 是裸 `string`
//! （非指针），字段缺省时反序列化成空串。所幸服务端对这三个都有 `if input.X != ""`
//! 守卫，空串等价于「不改」，所以只传 `{"concurrency": N}` 不会误清其他字段。
//! `concurrency` 自身是 `*int`，缺省即不改。

use std::time::Duration;

use reqwest::Client;

use super::schedulable_client::{RETRY_BACKOFF_SECS, is_retryable_status};

/// 一次并发推送的目标配置。
pub(crate) struct ConcurrencyTarget<'a> {
    pub label: &'static str,
    pub base_url: &'a str,
    pub token: &'a str,
    pub auth_header: &'a str,
    pub account_ids: &'a [u64],
    pub max_attempts: u32,
}

/// 给目标里的每个账号推一次并发上限。全部成功才返回 true。
///
/// 部分成功也返回 false：下次会对所有账号重推。同值重推无副作用。
pub(crate) async fn push_all(
    target: &ConcurrencyTarget<'_>,
    client: &Client,
    concurrency: u32,
) -> bool {
    let mut all_ok = true;
    for id in target.account_ids {
        if let Err(error) = push_one(target, client, *id, concurrency).await {
            tracing::warn!(
                control = target.label,
                account_id = id,
                concurrency,
                "外部并发上限推送失败，稍后重试: {}",
                error
            );
            all_ok = false;
        } else {
            tracing::info!(
                control = target.label,
                account_id = id,
                concurrency,
                "外部并发上限已更新"
            );
        }
    }
    all_ok
}

async fn push_one(
    target: &ConcurrencyTarget<'_>,
    client: &Client,
    account_id: u64,
    concurrency: u32,
) -> anyhow::Result<()> {
    let max_attempts = target.max_attempts.max(1);
    let mut last_error = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let index = (attempt as usize - 1).min(RETRY_BACKOFF_SECS.len() - 1);
            tokio::time::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[index])).await;
        }

        match try_push_one(target, client, account_id, concurrency).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if !error.retryable {
                    anyhow::bail!("{}（不重试）", error.message);
                }
                if attempt + 1 < max_attempts {
                    tracing::debug!(
                        control = target.label,
                        account_id,
                        attempt = attempt + 1,
                        "外部并发上限推送失败，将重试: {}",
                        error.message
                    );
                }
                last_error = Some(error.message);
            }
        }
    }

    anyhow::bail!(
        "{} 次尝试均失败，最后一次: {}",
        max_attempts,
        last_error.unwrap_or_else(|| "未知错误".into())
    )
}

struct PushError {
    message: String,
    retryable: bool,
}

async fn try_push_one(
    target: &ConcurrencyTarget<'_>,
    client: &Client,
    account_id: u64,
    concurrency: u32,
) -> Result<(), PushError> {
    let url = format!("{}/api/v1/admin/accounts/{}", target.base_url, account_id);
    let response = client
        .put(&url)
        .header(target.auth_header, target.token.trim())
        .json(&serde_json::json!({ "concurrency": concurrency }))
        .send()
        .await
        .map_err(|error| PushError {
            message: format!("请求失败: {}", error),
            retryable: true,
        })?;

    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(200).collect();
    Err(PushError {
        message: format!("HTTP {}: {}", status, snippet),
        retryable: is_retryable_status(status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只传 concurrency，不夹带 name/type/status —— 对方那三个字段是裸 string，
    /// 虽有空串守卫，但请求体里干脆不出现更稳。
    #[tokio::test]
    async fn 请求体只含concurrency字段() {
        use axum::{Router, routing::put};

        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new()
            .route(
                "/api/v1/admin/accounts/{id}",
                put(
                    |axum::extract::Path(id): axum::extract::Path<u64>,
                     axum::extract::State(tx): axum::extract::State<
                        tokio::sync::mpsc::UnboundedSender<(u64, serde_json::Value)>,
                    >,
                     axum::Json(body): axum::Json<serde_json::Value>| async move {
                        tx.send((id, body)).unwrap();
                        axum::http::StatusCode::OK
                    },
                ),
            )
            .with_state(sender);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let ids = vec![158u64];
        let target = ConcurrencyTarget {
            label: "并发联动",
            base_url: &format!("http://{address}"),
            token: "test-token",
            auth_header: "X-API-Key",
            account_ids: &ids,
            max_attempts: 1,
        };
        assert!(push_all(&target, &Client::new(), 11).await);

        let (id, body) = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, 158);
        assert_eq!(body, serde_json::json!({ "concurrency": 11 }));

        server.abort();
    }
}
