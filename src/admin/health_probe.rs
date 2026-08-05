//! 健康探测器（本地运维便利特性）
//!
//! 周期性发一次**真实推理请求**，判断「本地链路当前还能不能出货」，结果供
//! [`super::health_gate`] 做调度联动判据。
//!
//! # 为什么需要它
//!
//! 看门狗原来的判据是 traces.db 近 1 分钟的报错**绝对条数**，那个判据在闭环里会失效：
//! 兜底池一开，流量被分走，分子塌了，于是「没量」和「健康」在读数上完全一样——
//! 系统越坏，判据越倾向于说它好，进而关掉兜底，流量涌回来又全报错，如此往复。
//!
//! 凭据池存量信号（`available/total`、`refresh_failure_count`）能破这个环，因为它们
//! 不从请求派生、零流量下读数依然有效。但存量信号有个盲区：**凭据全好、推理接口坏了**
//! 的时候 `available` 是满的。想覆盖那类故障，只能真的出一次货。这就是本模块。
//!
//! # 设计要点
//!
//! - **有真实成功就不探测**。窗口内已有 `final_status='success'` 的请求，说明链路
//!   已被真实流量证明，探测纯属浪费一次付费调用。于是探测频率天然与流量成反比：
//!   忙时一次不发，闲时才发——而闲时正是存量信号覆盖不到、真正需要探测的时刻。
//! - **不写 trace，判据不自污染**。探测走 `KiroProvider::call_api` 且 `sink` 传 `None`，
//!   trace 的唯一生产写入点在 anthropic handler 层，因此探测失败不会被
//!   `recent_counters().errors` 统计进去。否则「探测失败 → 报错数升高 → 判据变差」
//!   会形成自我强化的环。
//! - **串行循环天然防重入**。单任务顺序 await，且每轮结束后才 sleep（而非固定
//!   ticker），所以探测耗时超过间隔时只会自然拉开间距，不会堆积并发的挂起请求。
//!   这点很要紧：不健康时请求往往是挂着而非快速失败，超时 [`PROBE_TIMEOUT_SECS`]
//!   远大于 30 秒的间隔。
//! - **连续失败才算不稳定**。单次网络抖动不下结论，阈值由配置的 `probeFailures` 决定。
//! - 全流程失败只 warn，绝不影响主服务。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use uuid::Uuid;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::conversation::{
    ConversationState, CurrentMessage, UserInputMessage,
};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;

use super::trace_db::SharedTraceStore;

/// 探测请求的提示词。刻意极短：探测只关心「链路能否出货」，不关心内容质量，
/// 输入输出 token 越少越省钱。与 Admin 手工模型测试用的是同一句。
const PROBE_PROMPT: &str = "Reply with exactly: OK";

/// 单次探测的超时（秒）。
///
/// 取 60 而非手工测试那边的 90：探测是周期性的自动动作，挂太久没有意义——
/// 一个要 60 秒还没出第一个字节的链路，对用户来说已经等同于坏了。
const PROBE_TIMEOUT_SECS: u64 = 60;

/// 探测结果的共享状态。看门狗读它，探测器写它。
///
/// 用原子量而非 `Mutex`：读侧（看门狗）每 30 秒读一次，写侧每轮写一次，
/// 没有需要保证多字段一致性的场景，原子量足够且不会让读侧阻塞在写侧的 IO 上。
#[derive(Debug, Default)]
pub struct ProbeState {
    /// 连续失败次数。任何一次成功归零。
    consecutive_failures: AtomicU32,
    /// 是否已经至少完成过一轮判定（成功或失败）。
    ///
    /// 用于区分「还没探测过」和「探测过且没失败」：两者的 `consecutive_failures`
    /// 都是 0，但前者不该被当作健康证据。
    has_verdict: AtomicBool,
    /// 最近一次探测成功的 Unix 秒（未成功过为 0）。
    last_success_epoch: AtomicU64,
    /// 累计探测次数与跳过次数，仅用于观测成本。
    total_probes: AtomicU64,
    total_skipped: AtomicU64,
}

impl ProbeState {
    /// 连续失败次数。
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// 是否已产生过至少一次判定。未探测过时看门狗不应把它当健康证据。
    pub fn has_verdict(&self) -> bool {
        self.has_verdict.load(Ordering::Relaxed)
    }

    /// 按阈值判断探测侧是否认为链路已经不可用。
    ///
    /// 没有判定时返回 `false`（不主动指控），把结论交给其他判据。
    pub fn is_failing(&self, threshold: u32) -> bool {
        self.has_verdict() && self.consecutive_failures() >= threshold.max(1)
    }

    /// 最近一次探测成功距今的秒数；从未成功过返回 `None`。
    pub fn secs_since_success(&self) -> Option<u64> {
        let ts = self.last_success_epoch.load(Ordering::Relaxed);
        if ts == 0 {
            return None;
        }
        let now = chrono::Utc::now().timestamp().max(0) as u64;
        Some(now.saturating_sub(ts))
    }

    /// 累计探测次数 / 跳过次数，供日志观测实际成本。
    pub fn counters(&self) -> (u64, u64) {
        (
            self.total_probes.load(Ordering::Relaxed),
            self.total_skipped.load(Ordering::Relaxed),
        )
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.has_verdict.store(true, Ordering::Relaxed);
        self.last_success_epoch
            .store(chrono::Utc::now().timestamp().max(0) as u64, Ordering::Relaxed);
    }

    fn record_failure(&self) -> u32 {
        self.has_verdict.store(true, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// 共享探测状态句柄。
pub type SharedProbeState = Arc<ProbeState>;

/// 探测器运行参数。
#[derive(Debug, Clone)]
pub struct ProbeOptions {
    /// 探测间隔（秒）。也是「窗口内有成功就跳过」的窗口长度。
    pub interval_secs: u64,
    /// 探测用的模型 ID。
    pub model_id: String,
}

/// 启动探测器后台任务，返回共享状态供看门狗读取。
pub fn spawn(
    options: ProbeOptions,
    provider: Arc<KiroProvider>,
    trace_store: SharedTraceStore,
) -> SharedProbeState {
    let state: SharedProbeState = Arc::new(ProbeState::default());
    let interval = options.interval_secs.max(5);

    tracing::info!(
        interval_secs = interval,
        model = %options.model_id,
        timeout_secs = PROBE_TIMEOUT_SECS,
        "健康探测器已启动：窗口内有成功请求则跳过本轮探测"
    );

    tokio::spawn(run(options, provider, trace_store, state.clone(), interval));
    state
}

async fn run(
    options: ProbeOptions,
    provider: Arc<KiroProvider>,
    trace_store: SharedTraceStore,
    state: SharedProbeState,
    interval: u64,
) {
    // 每轮结束后才 sleep，而非固定 ticker：探测耗时超过间隔时自然拉开间距，
    // 不会堆积并发请求。
    let gap = Duration::from_secs(interval);
    // trace 关闭的告警只打一次，避免每轮刷屏。
    let mut warned_trace_off = false;

    loop {
        tokio::time::sleep(gap).await;

        // 窗口取一个探测间隔：这一段时间里有真实成功，就不必自己再发一次。
        // trace 关闭时读不到成功记录，只能每轮都探测——这是安全的降级方向。
        let skip = if trace_store.is_enabled() {
            if warned_trace_off {
                tracing::info!("健康探测：trace 已恢复，重新按成功记录跳过探测");
                warned_trace_off = false;
            }
            trace_store.recent_success_count(interval as i64) > 0
        } else {
            if !warned_trace_off {
                tracing::warn!(
                    "健康探测：trace 已关闭，读不到成功记录，之后每轮都会实际发探测请求"
                );
                warned_trace_off = true;
            }
            false
        };

        if skip {
            state.total_skipped.fetch_add(1, Ordering::Relaxed);
            // 真实流量的成功与探测成功等价，都证明链路能出货，所以照样归零失败计数。
            state.record_success();
            continue;
        }

        state.total_probes.fetch_add(1, Ordering::Relaxed);
        match probe_once(&provider, &options.model_id).await {
            Ok(latency_ms) => {
                let had_failures = state.consecutive_failures() > 0;
                state.record_success();
                if had_failures {
                    tracing::info!(latency_ms, model = %options.model_id, "健康探测：已恢复");
                } else {
                    tracing::debug!(latency_ms, model = %options.model_id, "健康探测：成功");
                }
            }
            Err(e) => {
                let streak = state.record_failure();
                tracing::warn!(
                    consecutive_failures = streak,
                    model = %options.model_id,
                    "健康探测失败: {}",
                    e
                );
            }
        }
    }
}

/// 发一次真实推理请求。成功返回耗时毫秒。
///
/// `sink` 传 `None`，因此不写 trace、不污染看门狗的报错数判据。
/// 也不指定凭据：这里要判断的是「系统整体还能不能出货」而非单张凭据健康度，
/// 走账号池调度与故障转移正合适，而且探测成本与凭据数量解耦——池子里 3 张还是
/// 30 张都只发一次请求。
async fn probe_once(provider: &KiroProvider, model_id: &str) -> anyhow::Result<u64> {
    let conversation_state = ConversationState::new(Uuid::new_v4().to_string())
        .with_agent_continuation_id(Uuid::new_v4().to_string())
        .with_agent_task_type("vibe")
        .with_chat_trigger_type("MANUAL")
        .with_current_message(CurrentMessage::new(
            UserInputMessage::new(PROBE_PROMPT, model_id).with_origin("AI_EDITOR"),
        ));
    let body = serde_json::to_string(&KiroRequest {
        conversation_state,
        profile_arn: None,
        additional_model_request_fields: None,
    })?;

    let started = std::time::Instant::now();
    let bytes = tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECS), async {
        let call = provider.call_api(&body, None, None).await?;
        let bytes = call.response.bytes().await?;
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .map_err(|_| anyhow::anyhow!("探测请求超时（{}s）", PROBE_TIMEOUT_SECS))??;

    // 解析事件流，确认真的出了内容。只要能拿到非空文本就算通——
    // 探测不校验回答内容是否等于 "OK"，模型偶尔多说几个字不是故障。
    let mut decoder = EventStreamDecoder::new();
    decoder.feed(&bytes)?;
    let mut text = String::new();
    for frame in decoder.decode_iter() {
        let event = Event::from_frame(frame?)?;
        match event {
            Event::AssistantResponse(response) => text.push_str(&response.content),
            Event::Error {
                error_code,
                error_message,
            } => anyhow::bail!("{error_code}: {error_message}"),
            Event::Exception {
                exception_type,
                message,
            } => anyhow::bail!("{exception_type}: {message}"),
            _ => {}
        }
    }
    if text.trim().is_empty() {
        anyhow::bail!("模型返回了空响应");
    }
    Ok(started.elapsed().as_millis().min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未探测过时不指控链路不可用() {
        let s = ProbeState::default();
        assert!(!s.has_verdict());
        // 连续失败数是 0，但「没探测过」不等于「健康」，也不该反过来被判为失败
        assert!(!s.is_failing(1));
        assert_eq!(s.secs_since_success(), None);
    }

    #[test]
    fn 连续失败达阈值才算不可用() {
        let s = ProbeState::default();
        assert_eq!(s.record_failure(), 1);
        assert!(!s.is_failing(2), "1 次失败未达阈值 2，不下结论");
        assert_eq!(s.record_failure(), 2);
        assert!(s.is_failing(2));
    }

    #[test]
    fn 成功归零失败计数() {
        let s = ProbeState::default();
        s.record_failure();
        s.record_failure();
        assert_eq!(s.consecutive_failures(), 2);
        s.record_success();
        assert_eq!(s.consecutive_failures(), 0);
        assert!(!s.is_failing(1));
        assert!(s.secs_since_success().is_some());
    }

    #[test]
    fn 阈值零被抬为一_不做无条件指控() {
        let s = ProbeState::default();
        // threshold=0 若直接比较会让「0 >= 0」在未失败时也成立，必须抬到 1
        assert!(!s.is_failing(0));
        s.record_failure();
        assert!(s.is_failing(0));
    }

    #[test]
    fn 探测超时短于手工测试() {
        // 周期性自动探测不该像手工测试那样等 90 秒：60 秒还没出字节，
        // 对用户而言已等同于坏了。
        assert_eq!(PROBE_TIMEOUT_SECS, 60);
        assert!(PROBE_TIMEOUT_SECS < 90);
    }
}
