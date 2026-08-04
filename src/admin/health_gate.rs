//! 健康联动看门狗（本地运维便利特性）
//!
//! 周期性读取 `traces.db` 的近 1 分钟报错数，把「本地是否稳定」反向推成外部系统
//! （如 4code.us）里若干账号的调度开关：
//!
//! | 本地状态 | 判据 | 推给外部的 `schedulable` |
//! |---|---|---|
//! | 稳定 | `errors1m < errorThreshold` | `false`（不让兜底池接量） |
//! | 不稳定 | `errors1m >= errorThreshold` | `true`（放兜底池进来接量） |
//!
//! 之所以是反的：外部账号在这里是兜底池，平时闲着更好，只在本地扛不住时顶上。
//!
//! # 设计要点
//! - **防抖**：连续 `confirmations` 个周期判定一致才真正推开关，避免报错数在阈值
//!   上下抖动时来回切、刷对方审计日志。
//! - **不读对方状态，只单向写**。开关接口幂等，重推同值无副作用，所以「先查再决定
//!   要不要写」省下的只是一次请求，却多一个失败点。对方那些运行时拦截字段
//!   （`temp_unschedulable_until` / `rate_limit_reset_at` / `overload_until` /
//!   `health_verdict`）属于它自己的调度决策，我们只负责把闸推到该在的位置。
//! - **翻转立刻推 + 定期重推兜底**。只在翻转时推会有个漏洞：本地记的「上次推成功的
//!   值」若被人在对方后台手动改掉，本地会因为状态"没变"而永远不再推，一直错到下次
//!   健康度翻转。故每 `reaffirmIntervalSecs` 按当前判定重推一次，让漂移自愈。
//! - **trace 关闭时跳过**：那时报错计数不再更新，按残留读数判定会得出错误结论，
//!   宁可不动。
//! - **单次推送内重试 `maxAttempts` 次**（退避 2s → 5s），只重试网络错误与 5xx；
//!   4xx 直接放弃。推不成也不改内部记录，下个周期继续。
//! - 全流程失败只 warn，绝不影响主服务。

use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;

use crate::model::config::HealthGateConfig;

use super::trace_db::SharedTraceStore;

/// 判据窗口：固定 60 秒，对齐概览页「报错 · 近 1 分钟」那张卡。
const WINDOW_SECS: i64 = 60;

/// 本地健康态。`Unknown` 是启动初值，用于强制第一次判定必然触发一次推送，
/// 让外部开关与本地状态在启动后尽快对齐（否则若外部残留的是上次崩溃前的值，
/// 会一直错到下次翻转）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Unknown,
    Stable,
    Unstable,
}

impl Health {
    /// 该健康态下外部账号应有的 `schedulable` 值。注意是反向映射。
    fn schedulable(self) -> Option<bool> {
        match self {
            Health::Unknown => None,
            // 本地稳 → 兜底池不接量
            Health::Stable => Some(false),
            // 本地不稳 → 放兜底池进来
            Health::Unstable => Some(true),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Health::Unknown => "未知",
            Health::Stable => "稳定",
            Health::Unstable => "不稳定",
        }
    }
}

/// 启动看门狗后台任务。配置不完整时直接返回，不起任务。
pub fn spawn(
    config: HealthGateConfig,
    trace_store: SharedTraceStore,
    client: Client,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.is_usable() {
        if config.enabled {
            tracing::warn!(
                "健康联动已开启但配置不完整（baseUrl / token / accountIds 需全部填写），本次不启动"
            );
        }
        return None;
    }

    tracing::info!(
        base_url = %config.normalized_base_url(),
        accounts = ?config.account_ids,
        threshold = config.error_threshold,
        interval_secs = config.check_interval_secs,
        confirmations = config.confirmations,
        reaffirm_secs = config.reaffirm_interval_secs,
        max_attempts = config.max_attempts,
        "健康联动看门狗已启动：本地稳则关闭外部调度，不稳则打开"
    );

    Some(tokio::spawn(run(Arc::new(config), trace_store, client)))
}

async fn run(config: Arc<HealthGateConfig>, trace_store: SharedTraceStore, client: Client) {
    let interval = Duration::from_secs(config.check_interval_secs.max(5));
    // 防抖计数：候选态 + 已连续观察到几次。
    let mut candidate = Health::Unknown;
    let mut streak: u32 = 0;
    // 已成功推给外部的状态。推送失败时保持不变，下周期重试。
    let mut applied = Health::Unknown;
    // 上次推送成功的时刻，用于定期重推兜底。
    let mut last_push = std::time::Instant::now();
    // trace 关闭的告警只打一次，避免每周期刷屏。
    let mut warned_trace_off = false;

    loop {
        tokio::time::sleep(interval).await;

        if !trace_store.is_enabled() {
            if !warned_trace_off {
                tracing::warn!("健康联动：trace 已关闭，近 1 分钟报错数不再更新，联动暂停");
                warned_trace_off = true;
            }
            continue;
        }
        if warned_trace_off {
            tracing::info!("健康联动：trace 已恢复，联动继续");
            warned_trace_off = false;
        }

        let errors = trace_store.recent_counters(WINDOW_SECS).errors;
        let observed = if errors >= config.error_threshold {
            Health::Unstable
        } else {
            Health::Stable
        };

        // 防抖：候选态变了就重新计数。
        if observed == candidate {
            streak = streak.saturating_add(1);
        } else {
            candidate = observed;
            streak = 1;
        }

        let need = config.confirmations.max(1);
        if streak < need {
            continue;
        }

        let flipped = observed != applied;
        // 状态没变也定期重推，兜住「对方被手动改掉而本地不知道」的漂移。
        let due_for_reaffirm = config.reaffirm_interval_secs > 0
            && last_push.elapsed() >= Duration::from_secs(config.reaffirm_interval_secs);
        if !flipped && !due_for_reaffirm {
            continue;
        }

        let Some(schedulable) = observed.schedulable() else {
            continue;
        };
        if flipped {
            tracing::info!(
                errors_1m = errors,
                threshold = config.error_threshold,
                from = applied.label(),
                to = observed.label(),
                schedulable,
                "健康联动：本地状态翻转，推送外部调度开关"
            );
        } else {
            tracing::debug!(
                errors_1m = errors,
                state = observed.label(),
                schedulable,
                "健康联动：定期重推当前判定"
            );
        }

        if push_all(&config, &client, schedulable).await {
            applied = observed;
            last_push = std::time::Instant::now();
        }
        // 推送失败时 applied / last_push 都不变，下个周期会再试。
    }
}

/// 给配置里的每个账号推一次开关。全部成功才返回 true。
///
/// 部分成功也返回 false：那样下周期会对所有账号重推一遍。开关接口是幂等的
/// （同值重推无副作用），用整体重试换实现简单，比逐账号记状态划算。
async fn push_all(config: &HealthGateConfig, client: &Client, schedulable: bool) -> bool {
    let mut all_ok = true;
    for id in &config.account_ids {
        if let Err(e) = push_one(config, client, *id, schedulable).await {
            tracing::warn!(
                account_id = id,
                schedulable,
                "健康联动：推送外部调度开关失败，下个周期重试: {}",
                e
            );
            all_ok = false;
        } else {
            tracing::info!(
                account_id = id,
                schedulable,
                "健康联动：外部调度开关已更新"
            );
        }
    }
    all_ok
}

/// 重试退避表。第 1 次失败等 2s，第 2 次等 5s。
///
/// 总等待 7s，压在 30s 轮询间隔内，不会让下个周期堆积。够覆盖对方重启 / 网络抖动
/// 这类几秒级故障；更长的故障交给下个周期，没必要在这里死等。
const RETRY_BACKOFF_SECS: [u64; 2] = [2, 5];

/// 推一个账号的开关，失败按退避表重试到 `max_attempts` 次。
///
/// 只重试网络错误与对方 5xx。4xx 是 token 失效 / 账号不存在 / 请求格式不对这类
/// 确定性错误，重试改变不了结果，只会白打三次。
async fn push_one(
    config: &HealthGateConfig,
    client: &Client,
    account_id: u64,
    schedulable: bool,
) -> anyhow::Result<()> {
    let max_attempts = config.max_attempts.max(1);
    let mut last_err = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let idx = (attempt as usize - 1).min(RETRY_BACKOFF_SECS.len() - 1);
            tokio::time::sleep(Duration::from_secs(RETRY_BACKOFF_SECS[idx])).await;
        }

        match try_push_one(config, client, account_id, schedulable).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if !e.retryable {
                    anyhow::bail!("{}（不重试）", e.message);
                }
                if attempt + 1 < max_attempts {
                    tracing::debug!(
                        account_id,
                        attempt = attempt + 1,
                        "健康联动：推送失败，将重试: {}",
                        e.message
                    );
                }
                last_err = Some(e.message);
            }
        }
    }

    anyhow::bail!(
        "{} 次尝试均失败，最后一次: {}",
        max_attempts,
        last_err.unwrap_or_else(|| "未知错误".into())
    )
}

/// 单次尝试的错误：带上「是否值得重试」的判断。
struct PushError {
    message: String,
    retryable: bool,
}

async fn try_push_one(
    config: &HealthGateConfig,
    client: &Client,
    account_id: u64,
    schedulable: bool,
) -> Result<(), PushError> {
    let url = format!(
        "{}/api/v1/admin/accounts/{}/schedulable",
        config.normalized_base_url(),
        account_id
    );
    let resp = client
        .post(&url)
        // 实测认证走 `X-API-Key`，不是 Bearer：同一个 token 用
        // `Authorization: Bearer` 会被回 401 `INVALID_TOKEN`（头识别了、token 被拒），
        // 而 `X-API-Key` 直接 200。头名做成可配，换法时不用改代码。
        .header(config.auth_header(), config.token.trim())
        .json(&serde_json::json!({ "schedulable": schedulable }))
        .send()
        .await
        .map_err(|e| PushError {
            // 建连 / 超时 / DNS 一类，值得重试
            message: format!("请求失败: {}", e),
            retryable: true,
        })?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // 带上响应体开头一段，便于区分鉴权失败 / 账号不存在 / 对方 5xx。
    let body = resp.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(200).collect();
    Err(PushError {
        message: format!("HTTP {}: {}", status, snippet),
        retryable: is_retryable_status(status),
    })
}

/// 5xx 与 429 值得重试，其余 4xx 不值得。
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 反向映射_稳定关调度_不稳开调度() {
        assert_eq!(Health::Stable.schedulable(), Some(false));
        assert_eq!(Health::Unstable.schedulable(), Some(true));
        assert_eq!(Health::Unknown.schedulable(), None);
    }

    #[test]
    fn 配置不完整视为未启用() {
        let mut c = HealthGateConfig {
            enabled: true,
            base_url: "https://4code.us".into(),
            token: "t".into(),
            account_ids: vec![1],
            ..Default::default()
        };
        assert!(c.is_usable());

        c.enabled = false;
        assert!(!c.is_usable());

        c.enabled = true;
        c.token = "  ".into();
        assert!(!c.is_usable());

        c.token = "t".into();
        c.account_ids.clear();
        assert!(!c.is_usable());
    }

    #[test]
    fn 认证头默认为_x_api_key_且空值有回落() {
        // 实测 4code.us 走 X-API-Key，Bearer 会被回 401 INVALID_TOKEN
        assert_eq!(HealthGateConfig::default().auth_header(), "X-API-Key");
        // 空头名会让 reqwest panic，必须回落
        let c = HealthGateConfig {
            auth_header: "   ".into(),
            ..Default::default()
        };
        assert_eq!(c.auth_header(), "X-API-Key");
    }

    #[test]
    fn 基址末尾斜杠被去掉() {
        let c = HealthGateConfig {
            base_url: "https://4code.us/".into(),
            ..Default::default()
        };
        assert_eq!(c.normalized_base_url(), "https://4code.us");
    }

    #[test]
    fn 只重试5xx与429_其余4xx不重试() {
        use reqwest::StatusCode;
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        // token 失效 / 账号不存在 / 请求格式不对：重试改变不了结果
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn 退避总时长压在轮询间隔内() {
        // 3 次尝试用掉 2 次退避，总等待须明显短于默认 30s 轮询间隔，
        // 否则重试会拖到下个周期、造成堆积。
        let total: u64 = RETRY_BACKOFF_SECS.iter().sum();
        assert_eq!(total, 7);
        assert!(total < default_check_interval_for_test());
    }

    fn default_check_interval_for_test() -> u64 {
        HealthGateConfig::default().check_interval_secs
    }

    #[test]
    fn 重推间隔默认开启() {
        let c = HealthGateConfig::default();
        assert_eq!(c.reaffirm_interval_secs, 300);
        assert_eq!(c.max_attempts, 3);
    }

    #[test]
    fn 阈值边界_等于阈值算不稳定() {
        let threshold = 10u64;
        let judge = |errors: u64| {
            if errors >= threshold {
                Health::Unstable
            } else {
                Health::Stable
            }
        };
        assert_eq!(judge(9), Health::Stable);
        assert_eq!(judge(10), Health::Unstable);
        assert_eq!(judge(11), Health::Unstable);
    }
}
