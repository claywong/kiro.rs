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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use reqwest::Client;

use crate::model::config::HealthGateConfig;

use super::trace_db::SharedTraceStore;
// 本地新增判据依赖单独成行，避免与上游对 use 块的重排相撞。
use crate::kiro::token_manager::MultiTokenManager;

use super::health_probe::SharedProbeState;

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

/// 一轮判定的输入读数。
///
/// 抽成结构体是为了让判定逻辑能脱离运行中的 token manager / 探测器单独测试——
/// 判定规则是这个特性的核心，必须能被测试锁住。
#[derive(Debug, Clone, Copy)]
struct Readings {
    /// 近 60 秒报错条数。**依赖流量**：零流量时恒为 0。
    errors: u64,
    /// 可用凭据数 / 总凭据数。存量指标，不依赖流量。
    available: usize,
    total: usize,
    /// 探测侧是否已连续失败到阈值。不依赖流量。
    probe_failing: bool,
}

/// 判定为不稳定的具体原因，用于日志归因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnstableReason {
    /// 凭据池可用比例过低（含全部不可用）
    CredentialsExhausted,
    /// 主动探测连续失败：凭据看着好，但链路出不了货
    ProbeFailing,
    /// 报错数达阈值
    ErrorsOverThreshold,
}

impl UnstableReason {
    fn label(self) -> &'static str {
        match self {
            UnstableReason::CredentialsExhausted => "凭据池可用比例过低",
            UnstableReason::ProbeFailing => "主动探测连续失败",
            UnstableReason::ErrorsOverThreshold => "报错数达阈值",
        }
    }
}

/// 三路判据合一。返回不稳定的原因，`None` 表示稳定。
///
/// # 为什么必须是「或」而不是投票
///
/// 三路各覆盖一类互不重叠的故障，任一路报警都是真故障：
///
/// | 判据 | 覆盖的故障 | 零流量下有效 |
/// |---|---|---|
/// | 可用凭据比例 | 凭据耗尽、token 失效 | 有效 |
/// | 主动探测 | 凭据都好但推理接口坏了 | 有效 |
/// | 报错数 | 兜底，抓前两者漏的 | **无效** |
///
/// 关键在于：**报错数这一路只能用来判不稳定，绝不能单独用来判稳定**。它是绝对条数
/// （= 错误率 × 流量），兜底池一开、流量被分走，读数必然掉到阈值以下，那时「没量」
/// 与「健康」无法区分。前两路不从请求派生，所以「兜底开了导致本地没量」不会让它们
/// 的读数变好——这才是打破震荡环的地方。
///
/// 判定为稳定要求三路同时无异常，其中前两路是实打实的正面证据。
fn judge(r: Readings, config: &HealthGateConfig) -> Option<UnstableReason> {
    // 底线：一张可用的都没有 → 请求必然失败或排队，无可争辩的不可用。
    // `total == 0` 是「一张凭据都没配」，同样不叫健康。
    // 这条不受 min_available_ratio 影响，永远生效。
    if r.total == 0 || r.available == 0 {
        return Some(UnstableReason::CredentialsExhausted);
    }

    // 可选的余量判据，**默认关闭**。
    //
    // 为什么默认关：`available_count()` 把限流冷却中（`throttled_until` 未到期）的
    // 凭据也算作不可用，而账号级 429 冷却是正常运行中的预期行为，不是故障。
    // 流量一大就有大批凭据在冷却里轮转，比例天然很低——此时系统完全健康。
    // 而且方向是反的：流量越大 → 冷却的越多 → 比例越低 → 越倾向判不稳定，
    // 会在系统最正常忙碌的时候误报。
    //
    // 10 张里只有 1 张可用也可能完全正常：这 1 张能不能扛住取决于当前流量和它的
    // 剩余配额，与「另外 9 张在冷却」没有直接关系。
    //
    // 保留配置项是给「想要余量预警」的运维口味用的，默认 0 表示不参与判定。
    if config.min_available_ratio > 0.0 {
        let ratio = r.available as f64 / r.total as f64;
        if ratio < config.min_available_ratio {
            return Some(UnstableReason::CredentialsExhausted);
        }
    }

    if r.probe_failing {
        return Some(UnstableReason::ProbeFailing);
    }

    if r.errors >= config.error_threshold {
        return Some(UnstableReason::ErrorsOverThreshold);
    }

    None
}

/// 看门狗的运行时状态，面板读写走这里。
///
/// 为什么需要它：`enabled` 原本只在 spawn 时读一次，改配置要重启才生效。面板要
/// 能随时停掉自动联动（例如手工接管期间），故把它提成原子位，循环每轮重新读。
///
/// 同时暴露 `applied`（已推给对方的值）。这一项是刻意加的：关掉开关时我们**不动
/// 对方状态**（单向推送、不读对方），于是对方残留的是上次推过去的值。不显示的话
/// 你关掉之后无从知道现在停在哪一档 —— 兜底池永久开着（计费）和永久关着（无兜底）
/// 是两个后果完全不同的残留。把它显示出来，不确定性至少是可见的。
/// 用原子量而非 `Mutex`：与 [`super::health_probe::ProbeState`] 同理，读侧（面板
/// 按需查）与写侧（每轮一次）都是低频单值，没有需要保持一致的字段组合。
///
/// 两个三态字段编码成 `u8`：0 = 未知 / 尚无，1 / 2 见各自常量。
pub struct GateState {
    /// 运行时总开关。false = 停止周期判定与推送，不改对方状态。
    enabled: AtomicBool,
    /// 最近一次**成功推给对方**的 `schedulable`。
    /// [`APPLIED_NONE`] = 本进程还没推过。
    applied: AtomicU8,
    /// 最近一轮判定结论。[`VERDICT_UNKNOWN`] = 还没判过。
    verdict: AtomicU8,
}

/// 本进程尚未成功推送过
const APPLIED_NONE: u8 = 0;
const APPLIED_FALSE: u8 = 1;
const APPLIED_TRUE: u8 = 2;

/// 尚无判定（刚启动，或开关关着没在判）
const VERDICT_UNKNOWN: u8 = 0;
const VERDICT_STABLE: u8 = 1;
const VERDICT_UNSTABLE: u8 = 2;

impl GateState {
    fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled: AtomicBool::new(enabled),
            applied: AtomicU8::new(APPLIED_NONE),
            verdict: AtomicU8::new(VERDICT_UNKNOWN),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// 已推给对方的 `schedulable`。`None` = 本进程还没推过 —— 此时对方可能残留
    /// 上次运行留下的值，面板要照实说「未知」而不是猜一个。
    pub fn applied(&self) -> Option<bool> {
        match self.applied.load(Ordering::Relaxed) {
            APPLIED_FALSE => Some(false),
            APPLIED_TRUE => Some(true),
            _ => None,
        }
    }

    fn set_applied(&self, schedulable: bool) {
        self.applied.store(
            if schedulable { APPLIED_TRUE } else { APPLIED_FALSE },
            Ordering::Relaxed,
        );
    }

    /// 最近一轮判定。`None` = 还没判过（刚启动或开关关着）
    pub fn verdict(&self) -> Option<&'static str> {
        match self.verdict.load(Ordering::Relaxed) {
            VERDICT_STABLE => Some("稳定"),
            VERDICT_UNSTABLE => Some("不稳定"),
            _ => None,
        }
    }

    fn set_verdict(&self, health: Health) {
        self.verdict.store(
            match health {
                Health::Stable => VERDICT_STABLE,
                Health::Unstable => VERDICT_UNSTABLE,
                Health::Unknown => VERDICT_UNKNOWN,
            },
            Ordering::Relaxed,
        );
    }
}

/// 共享句柄
pub type SharedGateState = Arc<GateState>;

/// 启动看门狗后台任务。配置不完整时直接返回，不起任务。
/// 返回运行时状态句柄供面板读写；`None` 表示配置不全、没起任务。
///
/// 判据是 [`HealthGateConfig::is_configured`] 而非 `is_usable` —— 只要地址、token、
/// 账号列表填全了就把循环起起来，`enabled` 交给循环每轮读。若按 `is_usable` 起，
/// 启动时开关是关的就没有任何循环在跑，面板打开后要等重启才生效。
pub fn spawn(
    config: HealthGateConfig,
    trace_store: SharedTraceStore,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
    probe_state: Option<SharedProbeState>,
) -> Option<SharedGateState> {
    if !config.is_configured() {
        if config.enabled {
            tracing::warn!(
                "健康联动已开启但配置不完整（baseUrl / token / accountIds 需全部填写），本次不启动"
            );
        }
        return None;
    }

    let state = GateState::new(config.enabled);

    tracing::info!(
        base_url = %config.normalized_base_url(),
        accounts = ?config.account_ids,
        threshold = config.error_threshold,
        interval_secs = config.check_interval_secs,
        confirmations = config.confirmations,
        reaffirm_secs = config.reaffirm_interval_secs,
        max_attempts = config.max_attempts,
        min_available_ratio = config.min_available_ratio,
        probe = probe_state.is_some(),
        probe_failures = config.probe_failures,
        enabled = config.enabled,
        "健康联动看门狗已就绪：本地稳则关闭外部调度，不稳则打开（总开关可在面板切换）"
    );

    tokio::spawn(run(
        Arc::new(config),
        trace_store,
        client,
        token_manager,
        probe_state,
        Arc::clone(&state),
    ));
    Some(state)
}

async fn run(
    config: Arc<HealthGateConfig>,
    trace_store: SharedTraceStore,
    client: Client,
    token_manager: Arc<MultiTokenManager>,
    probe_state: Option<SharedProbeState>,
    state: SharedGateState,
) {
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
    // 总开关的上一轮状态，用于只在切换时打日志。
    let mut was_enabled = state.enabled();

    loop {
        tokio::time::sleep(interval).await;

        // 总开关：关着就什么都不做 —— 不判定、不推送，**也不改对方状态**。
        // 对方会保持上次推过去的值，这是有意的（面板会把该值显示出来）：
        // 关开关的动机通常是「我要手工接管」，此时替用户决定对方该开还是该关，
        // 比留在原处更容易出错。
        //
        // 防抖计数一并清掉：关掉期间本地状态可能已经变了，重新打开时应从头
        // 观察 confirmations 轮再推，而不是拿关闭前的半截 streak 直接翻转。
        if !state.enabled() {
            if was_enabled {
                tracing::info!(
                    applied = ?state.applied(),
                    "健康联动：总开关已关闭，停止判定与推送（外部调度保持当前值不变）"
                );
                was_enabled = false;
            }
            candidate = Health::Unknown;
            streak = 0;
            state.set_verdict(Health::Unknown);
            continue;
        }
        if !was_enabled {
            tracing::info!("健康联动：总开关已开启，恢复周期判定");
            was_enabled = true;
        }

        // trace 关闭时只失去「报错数」这一路判据，凭据池存量与主动探测都不依赖它，
        // 所以整体联动继续跑，不再像早期版本那样整个暂停。报错数按 0 计入，
        // 也就是这一路不再指控——但它本来就只有指控权、没有放行权（见 judge 的说明），
        // 所以按 0 处理不会造成「因为读不到数据而误判为健康」。
        let trace_on = trace_store.is_enabled();
        if !trace_on && !warned_trace_off {
            tracing::warn!(
                "健康联动：trace 已关闭，报错数判据失效，改由凭据池存量与主动探测判定"
            );
            warned_trace_off = true;
        }
        if trace_on && warned_trace_off {
            tracing::info!("健康联动：trace 已恢复，报错数判据重新生效");
            warned_trace_off = false;
        }

        let readings = Readings {
            errors: if trace_on {
                trace_store.recent_counters(WINDOW_SECS).errors
            } else {
                0
            },
            available: token_manager.available_count(),
            total: token_manager.total_count_in_group(None),
            probe_failing: probe_state
                .as_ref()
                .map(|s| s.is_failing(config.probe_failures))
                .unwrap_or(false),
        };

        let reason = judge(readings, &config);
        let observed = match reason {
            Some(_) => Health::Unstable,
            None => Health::Stable,
        };
        // 判定结论每轮都记，与推不推送无关 —— 面板要显示「它现在认为本地稳不稳」，
        // 而防抖期间（streak 未满）是不推送的，那时也该有结论可看。
        state.set_verdict(observed);

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
                errors_1m = readings.errors,
                threshold = config.error_threshold,
                available = readings.available,
                total = readings.total,
                probe_failing = readings.probe_failing,
                reason = reason.map(|r| r.label()).unwrap_or("三路判据均正常"),
                from = applied.label(),
                to = observed.label(),
                schedulable,
                "健康联动：本地状态翻转，推送外部调度开关"
            );
        } else {
            // 重推是低频动作（默认 5 分钟一次），在这里带上探测器的累计计数，
            // 用来观测「有成功就跳过」实际省下了多少次付费调用。
            let (probes, skipped) = probe_state
                .as_ref()
                .map(|s| s.counters())
                .unwrap_or((0, 0));
            tracing::debug!(
                errors_1m = readings.errors,
                available = readings.available,
                total = readings.total,
                probe_failing = readings.probe_failing,
                probes_sent = probes,
                probes_skipped = skipped,
                probe_last_success_secs = probe_state
                    .as_ref()
                    .and_then(|s| s.secs_since_success()),
                state = observed.label(),
                schedulable,
                "健康联动：定期重推当前判定"
            );
        }

        if push_all(&config, &client, schedulable).await {
            applied = observed;
            last_push = std::time::Instant::now();
            state.set_applied(schedulable);
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

    /// `is_configured` 不看 `enabled` —— 看门狗靠它决定要不要起循环，
    /// 若跟着 `enabled` 走，启动时关着就没循环在跑，面板打开后要等重启才生效。
    #[test]
    fn 配置齐全与当前启用是两件事() {
        let mut c = HealthGateConfig {
            enabled: false,
            base_url: "https://4code.us".into(),
            token: "t".into(),
            account_ids: vec![1],
            ..Default::default()
        };
        assert!(c.is_configured(), "填全了就算已配置，与开关无关");
        assert!(!c.is_usable(), "但当前没启用");

        c.enabled = true;
        assert!(c.is_usable());

        // 缺任一项都不算已配置
        c.account_ids.clear();
        assert!(!c.is_configured());
    }

    #[test]
    fn 总开关可运行时切换() {
        let s = GateState::new(false);
        assert!(!s.enabled());
        s.set_enabled(true);
        assert!(s.enabled());
        s.set_enabled(false);
        assert!(!s.enabled());
    }

    /// 未推送过要照实返回 None：此时对方可能残留上次运行留下的值，
    /// 面板得显示「未知」而不是猜一个，否则会让人以为对方停在某个确定档位。
    #[test]
    fn 未推送过的已应用值为未知() {
        let s = GateState::new(true);
        assert_eq!(s.applied(), None);
        s.set_applied(false);
        assert_eq!(s.applied(), Some(false));
        s.set_applied(true);
        assert_eq!(s.applied(), Some(true));
    }

    #[test]
    fn 判定结论可读回且未知态为空() {
        let s = GateState::new(true);
        assert_eq!(s.verdict(), None);
        s.set_verdict(Health::Stable);
        assert_eq!(s.verdict(), Some("稳定"));
        s.set_verdict(Health::Unstable);
        assert_eq!(s.verdict(), Some("不稳定"));
        // 关掉开关时循环会写回 Unknown，面板据此显示「未判定」
        s.set_verdict(Health::Unknown);
        assert_eq!(s.verdict(), None);
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

    // ── 三路判据的测试。这是本特性的核心规则，必须被锁住 ──────────────────
    // 尤其是「报错数只有指控权、没有放行权」那条：它是打破震荡环的关键，
    // 一旦被改成可以单独判稳定，环就回来了。

    /// 默认判据配置：比例维度关闭（与线上默认一致），只有 available==0 是底线。
    fn 判据配置() -> HealthGateConfig {
        HealthGateConfig {
            error_threshold: 10,
            probe_failures: 2,
            ..Default::default()
        }
    }

    /// 健康基线：凭据充足、探测正常、无报错。
    fn 健康读数() -> Readings {
        Readings {
            errors: 0,
            available: 10,
            total: 10,
            probe_failing: false,
        }
    }

    #[test]
    fn 三路均正常才判稳定() {
        assert_eq!(judge(健康读数(), &判据配置()), None);
    }

    #[test]
    fn 零流量不能推出稳定_这是震荡环的根源() {
        // 场景复现：兜底池已打开，本地流量被分走 → 报错数为 0。
        // 若只看报错数会判"稳定"→ 关兜底 → 流量涌回 → 全报错 → 再开，循环往复。
        // 凭据池存量与探测都不依赖流量，因此仍能正确判为不稳定。
        let c = 判据配置();

        // 凭据耗尽，但因为没量所以报错数是 0
        let 凭据耗尽 = Readings {
            errors: 0,
            available: 0,
            total: 10,
            probe_failing: false,
        };
        assert_eq!(
            judge(凭据耗尽, &c),
            Some(UnstableReason::CredentialsExhausted),
            "零流量下报错数为 0，但凭据耗尽仍须判为不稳定"
        );

        // 凭据全好、推理接口坏了：这是存量信号的盲区，只有探测能发现
        let 探测失败 = Readings {
            errors: 0,
            available: 10,
            total: 10,
            probe_failing: true,
        };
        assert_eq!(
            judge(探测失败, &c),
            Some(UnstableReason::ProbeFailing),
            "凭据看着全好，但链路出不了货，须由探测判为不稳定"
        );
    }

    #[test]
    fn 报错数达阈值单独也能判不稳定() {
        // 报错数有指控权：前两路漏掉的故障靠它兜底
        let r = Readings {
            errors: 10,
            ..健康读数()
        };
        assert_eq!(
            judge(r, &判据配置()),
            Some(UnstableReason::ErrorsOverThreshold)
        );
    }

    #[test]
    fn 默认不按比例判定_少量可用凭据算正常() {
        // 关键：10 张里只剩 1 张可用是正常状态，不该判不稳定。
        // available_count() 把限流冷却中的凭据算作不可用，而账号级 429 冷却是
        // 正常运行中的预期行为。流量一大就有大批凭据在冷却里轮转，若按比例判，
        // 会在系统最正常忙碌的时候误报——方向恰好是反的。
        let c = 判据配置();
        for available in 1..=10 {
            let r = Readings {
                available,
                total: 10,
                ..健康读数()
            };
            assert_eq!(
                judge(r, &c),
                None,
                "10 张里有 {} 张可用，应判稳定",
                available
            );
        }
    }

    #[test]
    fn 一张可用的都没有是底线_不受比例配置影响() {
        let 全不可用 = Readings {
            available: 0,
            total: 10,
            ..健康读数()
        };
        // 比例判据关着（默认）也要拦
        assert_eq!(
            judge(全不可用, &判据配置()),
            Some(UnstableReason::CredentialsExhausted)
        );
        // 比例判据开着当然也拦
        let 开比例 = HealthGateConfig {
            min_available_ratio: 0.2,
            ..判据配置()
        };
        assert_eq!(
            judge(全不可用, &开比例),
            Some(UnstableReason::CredentialsExhausted)
        );
    }

    #[test]
    fn 一张凭据都没配不算健康() {
        // total=0 时 available/total 无意义，不能当"比例达标"放行
        let r = Readings {
            available: 0,
            total: 0,
            ..健康读数()
        };
        assert_eq!(
            judge(r, &判据配置()),
            Some(UnstableReason::CredentialsExhausted)
        );
    }

    #[test]
    fn 显式开启比例判据后按阈值生效() {
        // 保留该配置项是给「想要余量预警」的口味用，默认关闭。
        let c = HealthGateConfig {
            min_available_ratio: 0.2,
            ..判据配置()
        };
        let 比例达标 = Readings {
            available: 2,
            total: 10,
            ..健康读数()
        };
        assert_eq!(judge(比例达标, &c), None, "0.2 不低于 0.2，达标");

        let 比例不足 = Readings {
            available: 1,
            total: 10,
            ..健康读数()
        };
        assert_eq!(
            judge(比例不足, &c),
            Some(UnstableReason::CredentialsExhausted),
            "0.1 低于 0.2"
        );
    }

    #[test]
    fn 判据优先级_凭据耗尽先于探测与报错() {
        // 三路同时异常时按归因价值排序：凭据耗尽是最根本的原因
        let 全异常 = Readings {
            errors: 100,
            available: 0,
            total: 10,
            probe_failing: true,
        };
        assert_eq!(
            judge(全异常, &判据配置()),
            Some(UnstableReason::CredentialsExhausted)
        );
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
