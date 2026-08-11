//! 外部账号 `schedulable` 开关的共享推送协议。

use std::time::Duration;

use reqwest::Client;

/// 一次推送的目标配置。健康联动与流量入口使用同一协议，只是目标站点与账号不同。
pub(crate) struct SchedulableTarget<'a> {
    pub label: &'static str,
    pub base_url: &'a str,
    pub token: &'a str,
    pub auth_header: &'a str,
    pub account_ids: &'a [u64],
    pub max_attempts: u32,
}

/// 给目标里的每个账号推一次开关。全部成功才返回 true。
///
/// 部分成功也返回 false：下次会对所有账号重推。接口幂等，同值重推无副作用。
pub(crate) async fn push_all(
    target: &SchedulableTarget<'_>,
    client: &Client,
    schedulable: bool,
) -> bool {
    let mut all_ok = true;
    for id in target.account_ids {
        if let Err(error) = push_one(target, client, *id, schedulable).await {
            tracing::warn!(
                control = target.label,
                account_id = id,
                schedulable,
                "外部调度开关推送失败，稍后重试: {}",
                error
            );
            all_ok = false;
        } else {
            tracing::info!(
                control = target.label,
                account_id = id,
                schedulable,
                "外部调度开关已更新"
            );
        }
    }
    all_ok
}

/// 第 1 次失败等 2s，第 2 次等 5s；更多尝试沿用 5s。
pub(crate) const RETRY_BACKOFF_SECS: [u64; 2] = [2, 5];

async fn push_one(
    target: &SchedulableTarget<'_>,
    client: &Client,
    account_id: u64,
    schedulable: bool,
) -> anyhow::Result<()> {
    let max_attempts = target.max_attempts.max(1);
    let mut last_error = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let index = (attempt as usize - 1).min(RETRY_BACKOFF_SECS.len() - 1);
            tokio::time::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[index])).await;
        }

        match try_push_one(target, client, account_id, schedulable).await {
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
                        "外部调度开关推送失败，将重试: {}",
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
    target: &SchedulableTarget<'_>,
    client: &Client,
    account_id: u64,
    schedulable: bool,
) -> Result<(), PushError> {
    let url = format!(
        "{}/api/v1/admin/accounts/{}/schedulable",
        target.base_url, account_id
    );
    let response = client
        .post(&url)
        .header(target.auth_header, target.token.trim())
        .json(&serde_json::json!({ "schedulable": schedulable }))
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

/// 网络错误在调用处标为可重试；HTTP 仅重试 5xx 与 429。
pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只重试5xx与429_其余4xx不重试() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    }
}
