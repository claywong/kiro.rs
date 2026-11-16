//! Anthropic API Handler 函数

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceRoute, TraceSink, outcome,
    usage_source,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::error::NoAvailableCredentialsError;
use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
use crate::kiro::model::events::{Event, TokenUsage};
use crate::kiro::model::requests::kiro::{AdditionalModelRequestFields, KiroRequest};
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::ModelDiscoveryError;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request_with_mode, get_context_window_size};
use super::middleware::{AppState, KeyContext};
use super::stream::{SseEvent, StreamContext};
use super::structured_rewrite::{
    StructuredCollector, append_schema_instruction, inject_system_instruction,
    rewrite_message_json, split_initial_events,
};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;
// 本地新增导入单独成行，避免上游按字母重排 use 块时反复冲突。
use super::thinking_mode::backend_requires_adaptive_thinking;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub(crate) credit_token_manager: Option<std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>>,
    pub key_id: u64,
    pub model: String,
    /// 本次请求最终下发的推理思考级别（None 表示未请求 effort）
    pub effort: Option<String>,
    pub started_at: Instant,
    /// 可选链路追踪器。挂上后 [`Self::record`] 会顺带 [`RequestTracer::finalize`]，
    /// 供那些「提前返回、拿不到主链路 tracer」的分支（web_search）补齐请求日志。
    ///
    /// 主链路（handle_stream_request / handle_non_stream_request）**不要**挂：
    /// 它们自己显式调 finalize，挂上会重复落库。
    pub(crate) tracer: Option<std::sync::Arc<RequestTracer>>,
    pub count_call: bool,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            credit_token_manager: state.kiro_provider.as_ref().map(|p| p.token_manager().clone()),
            key_id,
            model,
            effort: None,
            started_at: Instant::now(),
            tracer: None,
            count_call: true,
        }
    }

    /// 设置本次请求最终下发的 effort（在完成请求转换、拿到归一化 effort 后调用）
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.effort = effort;
    }

    /// 挂上链路追踪器，让 [`Self::record`] 兼任 trace 落库。
    ///
    /// 只给 web_search 这类在主链路 tracer 构造之前就 `return` 的分支用。
    pub(crate) fn attach_tracer(&mut self, tracer: std::sync::Arc<RequestTracer>) {
        self.tracer = Some(tracer);
    }

    /// 丢弃挂载的 tracer（不落 trace）。无可用凭据等「不应留日志」的早退分支用。
    pub(crate) fn discard_tracer(&self) {
        if let Some(tracer) = &self.tracer {
            tracer.discard();
        }
    }

    /// 取出挂载的 tracer 作为 provider 的 trace sink（未挂载时为 None，零开销）。
    #[allow(dead_code)]
    pub(crate) fn trace_sink(&self) -> Option<&dyn TraceSink> {
        self.tracer.as_ref().map(|t| t.as_ref() as &dyn TraceSink)
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
            effort: self.effort.clone(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        // 本地新增：喂给近 1 分钟消耗窗口（凭证列表展示用，与 trace 开关无关）
        crate::admin::recent_spend::tracker().record(credential_id, rec.credits);
        if let Some(manager) = &self.credit_token_manager {
            manager.record_local_credits(credential_id, rec.credits);
        }
        // 上游 cba46ea：系统 Key（key_id == 0）也计入用量总计，不再按 key_id 过滤。
        if status == "success" {
            if let Some(m) = &self.client_keys {
                m.record_usage(&rec, self.count_call);
            }
        }
        // 挂了 tracer 的分支（web_search）在这里补齐 trace 落库。
        if let Some(tracer) = &self.tracer {
            let error_type = if status == "success" {
                None
            } else {
                last_attempt_outcome(tracer)
            };
            tracer.finalize(
                status,
                error_type,
                None,
                None,
                TraceUsage {
                    input_tokens: rec.input_tokens,
                    output_tokens: rec.output_tokens,
                    cache_creation_tokens: rec.cache_creation_tokens,
                    cache_read_tokens: rec.cache_read_tokens,
                    credits: rec.credits,
                    // web_search 直连 MCP 路径没有上游 metadata 真值可区分，来源记为未知。
                    ..TraceUsage::zero()
                },
            );
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    client_ip: Option<String>,
    model: String,
    is_stream: bool,
    /// 本次请求最终下发的推理思考级别（None 表示未请求 effort）
    effort: Option<String>,
    started_at: Instant,
    /// 首个上游 chunk 到达时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    /// 首个 reasoningContentEvent 到达时刻（思考开始；取第一次）
    ///
    /// 与 [`Self::first_token_at`] 分开记：`first_token_at` 是「首个上游 chunk」，
    /// 语义不变（TTFT EWMA 调度依赖它）；带思考的请求里首个 chunk 往往就是思考，
    /// 但非思考请求、或上游先发 contextUsageEvent 时二者并不相等。
    first_reasoning_at: parking_lot::Mutex<Option<Instant>>,
    /// 首个产出帧到达时刻（开始产出正文 **或** 工具调用；取第一次）
    ///
    /// 「产出」含 assistantResponseEvent 与 toolUseEvent 两种 —— 详见
    /// [`Self::observe_event`] 里的说明。
    first_answer_at: parking_lot::Mutex<Option<Instant>>,
    /// 思考文本累计字符数（reasoningContentEvent 的 text 字段）
    ///
    /// 用字符数而非 token：token 只能按 `字符/4` 估，落库存原始字符数，
    /// 换算留给展示层，避免把估算误差固化进历史数据。
    thinking_chars: std::sync::atomic::AtomicU64,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
    /// Token 管理器句柄，用于结束时上报 TTFT EWMA（可选）
    token_manager: Option<std::sync::Arc<crate::kiro::token_manager::MultiTokenManager>>,
    /// 会话亲和键（metadata.user_id 里的 session_id；无 session 为 None）
    session_key: Option<String>,
    /// 最后一跳的会话亲和结果（provider 经 TraceSink::on_affinity 上报）
    affinity: parking_lot::Mutex<Option<&'static str>>,
    /// 首次选号的路由决策。web_search 一条 trace 内多次 provider 调用，
    /// 「是否沿用了上一轮账号」只看第一次。
    route: parking_lot::Mutex<Option<TraceRoute>>,
    /// 是否已落库。[`finalize`](Self::finalize) 首次进入即置位，保证同一条 trace
    /// 只写一行；[`Drop`] 兜底时据此判断「本请求从未正常收尾」。
    finalized: std::sync::atomic::AtomicBool,
    /// provider 成功返回 `reqwest::Response`，即已收到上游 HTTP 响应头。
    /// 不能用「尚未 finalize」代替这个状态：下游可能在读取 body 前取消。
    upstream_headers_received: std::sync::atomic::AtomicBool,
}

/// usage 三项的来源，落到 trace 行便于区分「上游真值」与「本地估算」。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum UsageSource {
    /// 错误早退等无用量场景
    #[default]
    Unknown,
    /// 上游 metadataEvent.tokenUsage
    Provider,
    /// 本地 CacheMeter 按断点估算
    Simulated,
    /// 无断点 / 计量关闭
    None,
}

impl UsageSource {
    fn as_db(self) -> Option<&'static str> {
        match self {
            Self::Unknown => Option::None,
            Self::Provider => Some(usage_source::PROVIDER),
            Self::Simulated => Some(usage_source::SIMULATED),
            Self::None => Some(usage_source::NONE),
        }
    }

    /// 由「上游是否给了精确用量」与「本地模拟是否覆盖到前缀」推断来源。
    pub fn resolve(has_provider_usage: bool, cache_usage: &super::cache_metering::CacheUsage) -> Self {
        if has_provider_usage {
            Self::Provider
        } else if cache_usage.cache_covered_est > 0 {
            Self::Simulated
        } else {
            Self::None
        }
    }
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
    pub source: UsageSource,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }
}

struct RequestTraceOptions {
    key_ctx: KeyContext,
    model: String,
    is_stream: bool,
    /// 本次请求最终下发的推理思考级别（None 表示未请求 effort）
    effort: Option<String>,
    /// 会话亲和键，见 [`session_affinity_key`]
    session_key: Option<String>,
}

/// 会话亲和键：`metadata.user_id` 里的 session_id，与发往上游的 conversationId 同源。
/// 取不到 session 时返回 None（不做亲和）。
fn session_affinity_key(payload: &MessagesRequest) -> Option<String> {
    payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .and_then(super::converter::extract_session_id)
}

/// 把本地会话亲和结果（none / bind / hit / rebind / overflow）映射为上游 trace 的
/// `sticky_outcome` 取值。bind = 该会话首次绑定（上游称 miss_first）；
/// rebind / overflow = 原绑定凭据不可用或触顶（上游称 miss_unavailable）。
fn sticky_from_affinity(affinity: &str) -> &'static str {
    use crate::admin::trace_db::sticky;
    match affinity {
        "hit" => sticky::HIT,
        "bind" => sticky::MISS_FIRST,
        "rebind" | "overflow" => sticky::MISS_UNAVAILABLE,
        _ => sticky::OFF,
    }
}

impl RequestTracer {
    fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            client_ip: options.key_ctx.client_ip,
            model: options.model,
            is_stream: options.is_stream,
            effort: options.effort,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            first_reasoning_at: parking_lot::Mutex::new(None),
            first_answer_at: parking_lot::Mutex::new(None),
            thinking_chars: std::sync::atomic::AtomicU64::new(0),
            attempts: parking_lot::Mutex::new(Vec::new()),
            token_manager: state.kiro_provider.as_ref().map(|p| p.token_manager().clone()),
            session_key: options.session_key,
            affinity: parking_lot::Mutex::new(None),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 会话亲和键（传给 provider 选号）
    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    /// 标记 provider 已成功返回响应头。应在取得 `reqwest::Response` 后立刻调用，
    /// 在创建/轮询下游 response body 之前完成。
    pub(super) fn mark_upstream_headers_received(&self) {
        self.upstream_headers_received
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 标记首个可发送的内容到达（幂等，仅记录第一次）。
    pub fn mark_first_token(&self) {
        if !self.is_stream {
            return;
        }
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// 非流式路径的思考观测：只累加字符数，不记时间戳
    ///
    /// 非流式是拿到完整响应体后一次性喂入解码器再遍历，所有帧的"到达时刻"都是
    /// 同一瞬间，思考时长无从测量。但思考的**文本量**依然是真实的，照常累加。
    ///
    /// 与 [`Self::observe_event`] 分开命名，避免调用方误以为非流式也能拿到时长。
    pub fn observe_reasoning_chars(&self, text: &str) {
        self.thinking_chars.fetch_add(
            text.chars().count() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// 观测一个已解码的上游事件：记录思考 / 正文的首帧时刻与思考文本量
    ///
    /// 只读事件、不改写，与 `process_kiro_event` 的转换逻辑解耦；trace 未启用时
    /// 由调用方跳过。仅用于流式路径——非流式整体喂入解码器，时间戳无意义，
    /// 那条路走 [`Self::observe_reasoning_chars`]。
    pub fn observe_event(&self, event: &Event) {
        match event {
            Event::ReasoningContent(reasoning) => {
                let mut slot = self.first_reasoning_at.lock();
                if slot.is_none() {
                    *slot = Some(Instant::now());
                }
                drop(slot);
                if let Some(text) = reasoning.text.as_deref() {
                    self.thinking_chars.fetch_add(
                        text.chars().count() as u64,
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
            }
            // 正文与工具调用都算「思考结束、开始产出」。
            //
            // Kiro 上游的产出终点有两种：assistantResponseEvent（文本）和
            // toolUseEvent（工具调用）。agent 类流量大量是「想一会儿 → 直接发工具
            // 调用」，整条流里没有一个 assistantResponseEvent。只认前者会让
            // first_answer_at 永远为 None，thinking_ms 退化成 None —— 思考数据在
            // 最需要它的那批请求上恰好全部缺失。
            Event::AssistantResponse(_) | Event::ToolUse(_) => {
                let mut slot = self.first_answer_at.lock();
                if slot.is_none() {
                    *slot = Some(Instant::now());
                }
            }
            _ => {}
        }
    }

    /// 构造自定义响应头插值上下文。
    pub fn header_context<'a>(
        &'a self,
        credential_id: Option<u64>,
        group: Option<&'a str>,
    ) -> crate::model::custom_headers::HeaderInterpolationContext<'a> {
        crate::model::custom_headers::HeaderInterpolationContext {
            trace_id: &self.trace_id,
            model: &self.model,
            key_id: Some(self.key_id),
            group,
            credential_id,
            client_ip: self.client_ip.as_deref(),
        }
    }

    fn mark_first_content(&self, events: &[SseEvent]) {
        if events.iter().any(|event| match event.event.as_str() {
            "content_block_start" => {
                event.data.pointer("/content_block/type")
                    .and_then(serde_json::Value::as_str) == Some("tool_use")
            }
            "content_block_delta" => ["text", "thinking", "partial_json"].iter().any(|field| {
                event.data.get("delta")
                    .and_then(|delta| delta.get(*field))
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            }),
            _ => false,
        }) {
            self.mark_first_token();
        }
    }

    pub(crate) fn discard(&self) {
        self.finalized.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        // 幂等：首次进入置位，防止显式收尾后 Drop 兜底再写一次（insert 按 trace_id 覆盖）。
        if self
            .finalized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        // 上报 TTFT EWMA：用于 priority 模式同层按响应最快调度
        if let (Some(tm), Some(ttft), true) =
            (&self.token_manager, first_token_ms, final_credential_id != 0)
        {
            tm.report_ttft(final_credential_id, &self.model, ttft);
        }
        // 思考观测：首个 reasoning 帧 → 首个产出帧（正文或工具调用）的间隔即思考耗时。
        // 只在两个时刻都有、且顺序正常时给值；上游若不发思考（或只发思考没发
        // 正文，例如中途断流）则为 None，展示层显示占位符。
        let first_reasoning_at = *self.first_reasoning_at.lock();
        let first_answer_at = *self.first_answer_at.lock();
        let first_answer_ms =
            first_answer_at.map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let thinking_ms = match (first_reasoning_at, first_answer_at) {
            (Some(r), Some(a)) if a >= r => Some(a.duration_since(r).as_millis() as u64),
            _ => None,
        };
        let thinking_chars = self.thinking_chars.load(std::sync::atomic::Ordering::Relaxed);
        let route = self.route.lock().take();
        // 本地会话亲和结果；上游 trace 列（sticky_outcome 等）在 provider 未经 on_route
        // 上报时，由它映射得到，保证上游的链路页与缓存命中率统计仍有数据。
        let affinity = self.affinity.lock().map(str::to_string);
        let (session_id, sticky_outcome, previous_credential_id) = match &route {
            Some(r) => (
                r.session_id.clone(),
                Some(r.sticky_outcome.to_string()),
                r.previous_credential_id,
            ),
            None => (
                self.session_key.clone(),
                affinity.as_deref().map(|a| sticky_from_affinity(a).to_string()),
                None,
            ),
        };
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            credits: usage.credits,
            first_token_ms,
            first_answer_ms,
            thinking_ms,
            thinking_chars,
            effort: self.effort.clone(),
            affinity,
            session_id,
            sticky_outcome,
            previous_credential_id,
            usage_source: usage.source.as_db().map(|s| s.to_string()),
            client_ip: self.client_ip.clone(),
            attempts,
        };
        store.insert(&rec);
    }
}

/// 兜底落库：请求被取消、走不到任何显式 finalize 时补一行。
///
/// `upstream_headers_received=false` 才表示请求仍在等待上游响应头；若 headers 已到
/// 但 body 尚未被读取/收尾，则应归为流中断，不能误报为等头超时。
///
/// 只记录可观测到的事实（头未到达即被取消），不推断原因：此处无法区分前置 SLB 等头超时、
/// 客户端读超时、用户主动中断、客户端进程退出等——它们在这一层长得完全一样。原因需结合
/// `duration_ms`（如贴近 SLB 的等头超时值）与下游日志判断。
///
/// 已正常收尾的请求已置位 `finalized`，幂等守卫使此处成为空操作，不会双写或覆盖。
impl Drop for RequestTracer {
    fn drop(&mut self) {
        let headers_received = self
            .upstream_headers_received
            .load(std::sync::atomic::Ordering::SeqCst);
        let (error_type, message) = if headers_received {
            (
                outcome::STREAM_INTERRUPTED,
                "request was cancelled after upstream response headers arrived",
            )
        } else {
            (
                outcome::ABANDONED_BEFORE_HEADERS,
                "request was cancelled while awaiting upstream response headers",
            )
        };
        // final_status 复用 "interrupted"（前端归入「中断」徽章），靠 error_type 区分具体类型。
        self.finalize(
            "interrupted",
            Some(error_type),
            Some(message),
            None,
            TraceUsage::zero(),
        );
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, mut attempt: TraceAttempt) {
        // 换跳即丢弃上一跳的思考观测。
        //
        // provider 在拿到响应头时就上报本跳（见 `emit_attempt` 的 SUCCESS 分支：
        // 上报后才 return response，流尚未消费），所以这里重置只会清掉**失败跳**
        // 的残留，成功跳的 reasoning / assistant 帧都发生在重置之后。
        //
        // 不重置的后果：first_reasoning_at 幂等只记第一次，thinking_ms 会变成
        // 「第 0 跳思考开始 → 最后一跳正文开始」，把失败、退避、换号全算进思考时长。
        // 口径与 final_credential_id 一致 —— 都只描述最后一跳。
        *self.first_reasoning_at.lock() = None;
        *self.first_answer_at.lock() = None;
        self.thinking_chars
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let mut attempts = self.attempts.lock();
        // Each provider call numbers retries from zero. A web-search request can make
        // several provider calls under one trace, so assign a request-wide sequence
        // before persisting to the (trace_id, attempt) primary key.
        attempt.attempt = attempts.len() as u32;
        attempts.push(attempt);
    }

    fn on_affinity(&self, affinity: &'static str) {
        *self.affinity.lock() = Some(affinity);
    }

    fn on_route(&self, route: TraceRoute) {
        let mut slot = self.route.lock();
        if slot.is_none() {
            *slot = Some(route);
        }
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
pub(crate) fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(canonical_attempt_outcome(&last))
}

fn canonical_attempt_outcome(value: &str) -> &'static str {
    match value {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::ACCOUNT_SUSPENDED => outcome::ACCOUNT_SUSPENDED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::LOCAL_RATE_LIMITED => outcome::LOCAL_RATE_LIMITED,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    }
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

pub(super) fn is_no_available_credentials_error(err: &Error) -> bool {
    if err.downcast_ref::<NoAvailableCredentialsError>().is_some() {
        return true;
    }
    let s = err.to_string();
    s.contains("所有凭据均已禁用")
        || s.contains("所有凭据均无法获取有效 Token")
        || s.contains("所有凭据已用尽")
        || s.contains("No available credentials")
}

fn request_has_no_credentials(
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    group: Option<&str>,
) -> bool {
    if websearch::has_web_search_tool(payload) {
        return !provider.token_manager().has_available_credentials(None, group);
    }
    super::converter::map_model(&payload.model).is_some_and(|model| {
        !provider.token_manager().has_available_credentials(Some(&model), group)
    })
}

/// 构造没有 `RequestTracer` 时的插值上下文（早退错误、WebSearch 等路径）。
///
/// 这些路径不会落 trace，`trace_id` 仅作为本次响应的请求 ID 下发。
pub(crate) fn early_header_context<'a>(
    trace_id: &'a str,
    model: &'a str,
    key_ctx: &'a KeyContext,
    credential_id: Option<u64>,
) -> crate::model::custom_headers::HeaderInterpolationContext<'a> {
    crate::model::custom_headers::HeaderInterpolationContext {
        trace_id,
        model,
        key_id: Some(key_ctx.key_id),
        group: key_ctx.group.as_deref(),
        credential_id,
        client_ip: key_ctx.client_ip.as_deref(),
    }
}

/// 向响应头追加自定义响应头（供下游 NewAPI / OneAPI 提取 upstream_request_id，及用户自定义头部）
pub(crate) fn attach_custom_headers(
    resp: &mut Response,
    custom_headers: Option<&crate::model::custom_headers::CustomHeadersManager>,
    ctx: &crate::model::custom_headers::HeaderInterpolationContext,
) {
    if let Some(mgr) = custom_headers {
        mgr.attach_to_response(resp, ctx);
    } else {
        attach_trace_headers(resp, ctx.trace_id);
    }
}

/// 向响应头追加 Trace ID 追踪头（供下游 NewAPI / OneAPI 提取 upstream_request_id）
pub(crate) fn attach_trace_headers(resp: &mut Response, trace_id: &str) {
    if let Ok(val) = header::HeaderValue::from_str(trace_id) {
        resp.headers_mut().insert(
            header::HeaderName::from_static("x-oneapi-request-id"),
            val.clone(),
        );
        resp.headers_mut()
            .insert(header::HeaderName::from_static("x-request-id"), val);
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    if is_no_available_credentials_error(&err) {
        tracing::warn!(error = %err, "无可用凭据（映射为 401）");
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "authentication_error",
                "No available credentials for this request",
            )),
        )
            .into_response();
    }

    if err
        .downcast_ref::<crate::kiro::error::UpstreamTimeoutError>()
        .is_some()
    {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            Json(ErrorResponse::new(
                "api_error",
                "Upstream timed out before returning a response.",
            )),
        )
            .into_response();
    }
    if let Some(unavailable) =
        err.downcast_ref::<crate::kiro::error::UpstreamModelUnavailableError>()
    {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "overloaded_error",
                "Upstream model is temporarily overloaded. Retry later.",
            )),
        )
            .into_response();
        if let Ok(value) = unavailable.retry_after.parse() {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }
    if err
        .downcast_ref::<crate::kiro::error::UpstreamContextOverflowError>()
        .is_some()
    {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    if let Some(rate_limit) = err.downcast_ref::<crate::kiro::error::UpstreamRateLimitError>() {
        let message = if rate_limit.is_local() {
            tracing::warn!(error = %err, "本地单账号 RPM 限流（映射为 429）");
            "Gateway per-account rate limit reached. Retry later."
        } else {
            tracing::warn!(error = %err, "上游限流（映射为 429）");
            "Upstream rate limit exceeded. Retry later."
        };
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse::new("rate_limit_error", message)),
        )
            .into_response();
        if let Some(value) = rate_limit
            .retry_after()
            .and_then(|value| value.parse::<header::HeaderValue>().ok())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        return response;
    }

    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }

    // Bedrock client-side validation errors (tool_use <-> tool_result mismatch, invalid message sequence, etc.)
    // The root cause is the client's own messages array, not an upstream failure, so it must not map to 5xx
    // otherwise it triggers an upstream cooldown that amplifies one client error into a 30+ burst of 503s.
    // Detection is centralized in the endpoint layer (single source of truth for the markers); the provider
    // already bails out without retry on these, and this mapping is the client-facing safety net.
    if crate::kiro::endpoint::default_is_client_validation_error(&err_str) {
        tracing::warn!(
            error = %err,
            "client messages array violates the protocol (Bedrock validation; mapped to 400 to avoid a false cooldown)"
        );
        // Return a stable, client-facing message and avoid echoing the raw upstream
        // error string (which can carry request IDs or internal validation details).
        // The full error is already logged above for diagnostics.
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.".to_string(),
            )),
        )
            .into_response();
    }

    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            "Upstream API request failed.",
        )),
    )
        .into_response()
}

/// 解析普通非流式响应的最终 Anthropic usage。
///
/// 返回 `(uncached_input, output, cache_write, cache_read)`。精确 provider 快照优先；
/// 缺失时才使用 contextUsage/输入估算和本地 CacheMeter 分摊。
fn resolve_non_stream_usage(
    fallback_total_input_tokens: i32,
    context_total_input_tokens: Option<i32>,
    fallback_output_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    provider_usage: Option<TokenUsage>,
) -> (i32, i32, i32, i32) {
    if let Some(usage) = provider_usage {
        let usage = usage.sanitized();
        return (
            usage.uncached_input_tokens,
            usage.output_tokens,
            usage.cache_write_input_tokens,
            usage.cache_read_input_tokens,
        );
    }

    // contextUsage 覆盖生成结束后的整个上下文（含本轮输出），分摊前先扣掉输出。
    let total_input = token::input_tokens_excluding_output(
        context_total_input_tokens,
        fallback_total_input_tokens,
        fallback_output_tokens,
    );
    let (input, cache_write, cache_read) = cache_usage.split_against_total(total_input);
    (
        input,
        fallback_output_tokens.max(0),
        cache_write,
        cache_read,
    )
}

fn validate_max_tokens(max_tokens: i32) -> Result<(), ErrorResponse> {
    if max_tokens <= 0 {
        Err(ErrorResponse::new(
            "invalid_request_error",
            "max_tokens must be greater than 0",
        ))
    } else {
        Ok(())
    }
}

fn merge_token_limits(target: &mut Option<TokenLimits>, incoming: Option<TokenLimits>) {
    let Some(incoming) = incoming else {
        return;
    };
    match target {
        Some(target) => {
            target.max_input_tokens = target.max_input_tokens.max(incoming.max_input_tokens);
            target.max_output_tokens = target.max_output_tokens.max(incoming.max_output_tokens);
        }
        None => *target = Some(incoming),
    }
}

fn infer_model_owner(model_id: &str) -> &'static str {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("claude-") {
        "anthropic"
    } else if id.starts_with("gpt-")
        || id.starts_with("chatgpt-")
        || id.starts_with("o1-")
        || id.starts_with("o3-")
        || id.starts_with("o4-")
    {
        "openai"
    } else {
        "kiro"
    }
}

fn context_window_from_upstream(model_id: &str, token_limits: Option<&TokenLimits>) -> i32 {
    token_limits
        .and_then(|limits| limits.max_input_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or_else(|| get_context_window_size(model_id))
}

fn model_from_upstream(upstream: UpstreamModel) -> Model {
    let max_tokens = upstream
        .token_limits
        .as_ref()
        .and_then(|limits| limits.max_output_tokens)
        .and_then(|limit| i32::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(64_000);
    let context_window =
        context_window_from_upstream(&upstream.model_id, upstream.token_limits.as_ref());
    Model {
        display_name: upstream
            .model_name
            .clone()
            .unwrap_or_else(|| upstream.model_id.clone()),
        owned_by: infer_model_owner(&upstream.model_id).to_string(),
        id: upstream.model_id,
        object: "model".to_string(),
        created: 0,
        model_type: "chat".to_string(),
        context_window,
        max_tokens,
    }
}

fn aggregate_available_models_with_custom(
    upstream_models: Vec<UpstreamModel>,
    custom_models: &[crate::model::config::CustomModel],
) -> Vec<Model> {
    let mut merged_upstream: BTreeMap<String, UpstreamModel> = BTreeMap::new();
    for incoming in upstream_models {
        match merged_upstream.get_mut(&incoming.model_id) {
            Some(existing) => {
                if existing.model_name.is_none() {
                    existing.model_name = incoming.model_name;
                }
                if existing.description.is_none() {
                    existing.description = incoming.description;
                }
                merge_token_limits(&mut existing.token_limits, incoming.token_limits);
            }
            None => {
                merged_upstream.insert(incoming.model_id.clone(), incoming);
            }
        }
    }

    let mut models: BTreeMap<String, Model> = BTreeMap::new();
    for upstream in merged_upstream.into_values() {
        let model = model_from_upstream(upstream);
        models.insert(model.id.clone(), model);
    }

    // 自定义别名最后写入，同名时其展示元数据优先于动态条目。
    for custom in custom_models {
        let model = Model {
            id: custom.id.clone(),
            object: "model".to_string(),
            created: 0,
            owned_by: custom
                .owned_by
                .clone()
                .unwrap_or_else(|| "custom".to_string()),
            display_name: custom
                .display_name
                .clone()
                .unwrap_or_else(|| custom.id.clone()),
            model_type: "chat".to_string(),
            context_window: custom
                .context_window
                .unwrap_or_else(|| get_context_window_size(&custom.id)),
            max_tokens: custom.max_tokens.unwrap_or(64_000),
        };
        models.insert(model.id.clone(), model);
    }

    models.into_values().collect()
}

fn aggregate_available_models(upstream_models: Vec<UpstreamModel>) -> Vec<Model> {
    aggregate_available_models_with_custom(upstream_models, &crate::model::custom_models::all())
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!("Received GET /v1/models request");

    let Some(provider) = &state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "service_unavailable",
                "Kiro API provider not configured",
            )),
        )
            .into_response();
    };

    let upstream = match provider
        .token_manager()
        .discover_models_for_group(key_ctx.group.as_deref())
        .await
    {
        Ok(models) => models,
        Err(ModelDiscoveryError::NoAvailableCredentials) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse::new(
                    "authentication_error",
                    "No available credentials for this API key",
                )),
            )
                .into_response();
        }
        Err(error @ ModelDiscoveryError::ColdStartFailed { .. }) => {
            tracing::warn!("动态模型列表加载失败: {}", error);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "Unable to load available models from upstream",
                )),
            )
                .into_response();
        }
    };

    let models = aggregate_available_models(upstream);

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
    .into_response()
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    let state = state.snapshot_tool_compatibility_mode();
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let mut hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检查是否有可用凭据（无可用账号直接返回 401，避免创建 tracer 和写入日志）
    if request_has_no_credentials(&provider, &payload, key_ctx.group.as_deref()) {
        tracing::warn!(
            model = %payload.model,
            group = ?key_ctx.group,
            "无可用凭据，直接返回 401"
        );
        let mut resp = (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "authentication_error",
                "No available credentials for this request",
            )),
        ).into_response();
        let trace_id = Uuid::new_v4().to_string();
        attach_custom_headers(&mut resp, state.custom_headers.as_ref(), &early_header_context(&trace_id, &payload.model, &key_ctx, None));
        return resp;
    }

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);
    warn_structured_output_with_web_search(&payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        // 这里在主链路 tracer 构造之前就返回，挂 tracer 到 hook 上补齐请求日志
        hook.attach_tracer(std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload.stream,
                effort: None,
                session_key: session_affinity_key(&payload),
            },
        )));
        let mut resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
        )
        .await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        if resp.status() != StatusCode::UNAUTHORIZED {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        } else {
            // 无可用凭据的 401 不计用量也不落 trace；上游的 Drop 兜底会补「中断」行，这里显式丢弃。
            hook.discard_tracer();
        }
        let trace_id = Uuid::new_v4().to_string();
        attach_custom_headers(
            &mut resp,
            state.custom_headers.as_ref(),
            &early_header_context(&trace_id, &payload.model, &key_ctx, None),
        );
        return resp;
    }

    // 以下都会进入上游对话，按请求统计一次 conversationId 来源
    super::conversation_id_stats::record(&payload);

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        // 同上：loop 内每个终止点都会调 hook.record，由它兼任 trace 落库；
        // tracer 同时作为 provider 的 trace sink，逐跳 attempt 也会记下来。
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
                effort: None,
                session_key: session_affinity_key(&payload),
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            state.cache_meter.clone(),
            key_ctx.key_id,
            state.custom_headers.clone(),
        )
        .await;
    }

    // output_config.format=json_schema：Kiro 无原生字段，注入 schema 指令，响应侧再提取改写。
    let structured_schema = inject_system_instruction(&mut payload);

    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::InvalidMessageSequence(reason) => (
                    "invalid_request_error",
                    format!("消息序列无效: {}", reason),
                ),
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let mut resp = (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
            let trace_id = Uuid::new_v4().to_string();
            attach_custom_headers(
                &mut resp,
                state.custom_headers.as_ref(),
                &early_header_context(&trace_id, &payload.model, &key_ctx, None),
            );
            return resp;
        }
    };

    // 最终下发的推理思考级别，落入用量日志与链路追踪
    let effort = extract_effort(conversion_result.additional_model_request_fields.as_ref());
    hook.set_effort(effort.clone());

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    // H2：发给上游的工具 schema，用于 `<invoke>` 文本合成校验（conversation_state 随后被移走）。
    let invoke_tool_schemas = super::stream::invoke_tool_schemas_from_state(
        &conversion_result.conversation_state,
        &conversion_result.known_tool_names,
    );
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let mut resp = (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
            let trace_id = Uuid::new_v4().to_string();
            attach_custom_headers(
                &mut resp,
                state.custom_headers.as_ref(),
                &early_header_context(&trace_id, &payload.model, &key_ctx, None),
            );
            return resp;
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存。
    // 返回 estimate 口径的覆盖量；真实 input/cache 互斥分摊在拿到 total 真值时进行。
    let cache_usage = match state.cache_meter.as_ref() {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    if payload.stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
                effort: effort.clone(),
                session_key: session_affinity_key(&payload),
            },
        ));
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            // L7：流式与非流式一致，遵守 extractThinking 配置。
            (state.extract_thinking || payload.output_config.is_some()) && thinking_enabled,
            tool_name_map,
            known_tool_names,
            invoke_tool_schemas,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            structured_schema.clone(),
            state.custom_headers.clone(),
        )
        .await
    } else {
        // Responses reasoning requests must expose native reasoning even when the
        // global Anthropic compatibility flag is disabled; the Responses adapter
        // explicitly opted into it via `thinking`/`output_config`.
        let extract_thinking =
            (state.extract_thinking || payload.output_config.is_some()) && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
                effort: effort.clone(),
                session_key: session_affinity_key(&payload),
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            invoke_tool_schemas,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            structured_schema.clone(),
            state.custom_headers.clone(),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    invoke_tool_schemas: super::stream::InvokeToolSchemas,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    structured_schema: Option<serde_json::Value>,
    custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(
            request_body,
            Some(input_tokens.max(0) as u64),
            Some(tracer.as_ref()),
            group.as_deref(),
            tracer.session_key(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            if is_no_available_credentials_error(&e) {
                tracer.discard();
            } else {
                hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
                // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
                tracer.finalize(
                    "error",
                    last_attempt_outcome(&tracer),
                    Some(&format!("{e:#}")),
                    None,
                    TraceUsage::zero(),
                );
            }
            let mut resp = map_provider_error(e);
            attach_custom_headers(
                &mut resp,
                custom_headers.as_ref(),
                &tracer.header_context(None, group.as_deref()),
            );
            return resp;
        }
    };
    tracer.mark_upstream_headers_received();
    let response = call_result.response;
    let credential_id = call_result.credential_id;
    let in_flight = call_result._in_flight;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
    );
    ctx.set_invoke_tool_schemas(invoke_tool_schemas);
    ctx.cache_usage = cache_usage;

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(
        response,
        ctx,
        initial_events,
        hook,
        credential_id,
        tracer.clone(),
        structured_schema.map(|schema| StructuredCollector::new(schema, model)),
        in_flight,
    );

    // 返回 SSE 响应
    // `x-accel-buffering: no` 让前置 nginx 对本次响应关闭 proxy_buffering。
    // 若反代开启了响应缓冲，SSE 分片会先攒在缓冲区再转发，拉长增量到达延迟；
    // 该头只作用于当前响应，无需在反代侧改全局配置。
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap();
    attach_custom_headers(
        &mut resp,
        custom_headers.as_ref(),
        &tracer.header_context(Some(credential_id), group.as_deref()),
    );
    resp
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    mut structured: Option<StructuredCollector>,
    in_flight: Option<std::sync::Arc<crate::kiro::token_manager::InFlightGuard>>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 结构化输出模式：只先发 message_start，其余初始事件与后续内容缓冲到流结束再改写发出。
    let initial_events = match structured.as_mut() {
        Some(collector) => {
            let (sent, held) = split_initial_events(initial_events);
            collector.hold(held);
            sent
        }
        None => initial_events,
    };
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();
    let mut settlement = StreamSettlement::new(hook, credential_id, tracer, &ctx, in_flight);
    settlement.structured = structured;

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), settlement, 0u64),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, mut settlement, mut sent_bytes)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            sent_bytes += chunk.len() as u64;
                            // 解码事件。缓冲区溢出（>16MB）后 feed 会持续失败、后续字节
                            // 被丢弃，继续 pump 只会让客户端收到永久截断的响应，故立即收尾为中断。
                            if let Err(e) = decoder.feed(&chunk) {
                                let bytes = settlement.interrupt(&mut ctx, Vec::new(), &StreamFailure::interrupted("malformed event frame from upstream"), &e.to_string(), sent_bytes);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        match Event::from_frame(frame) {
                                            Ok(event) => {
                                                if let Some((error, failure)) = upstream_event_error(&event) {
                                                    let bytes = settlement.interrupt(&mut ctx, events, &failure, &error, sent_bytes);
                                                    return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                                                }
                                                // 本地：思考观测（首个 reasoning / 产出帧时刻与思考字符数，供 trace 展示）
                                                settlement.tracer.observe_event(&event);
                                                let generated = ctx.process_kiro_event(&event);
                                                // 结构化模式下内容被缓冲，首字时间在流末真正发出时再记
                                                if settlement.structured.is_none() {
                                                    settlement.tracer.mark_first_content(&generated);
                                                }
                                                events.extend(generated);
                                            }
                                            Err(error) => {
                                                let bytes = settlement.interrupt(&mut ctx, events, &StreamFailure::interrupted("malformed event frame from upstream"), &error.to_string(), sent_bytes);
                                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let bytes = settlement.interrupt(&mut ctx, events, &StreamFailure::interrupted("malformed event frame from upstream"), &e.to_string(), sent_bytes);
                                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                                    }
                                }
                            }

                            let events = settlement.hold_if_structured(events);
                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            settlement.update(&ctx, sent_bytes);

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement, sent_bytes)))
                        }
                        Some(Err(e)) => {
                            let failure = body_read_failure(&e);
                            let bytes = settlement.interrupt(&mut ctx, Vec::new(), &failure, &format!("{:#}", anyhow::Error::new(e)), sent_bytes);
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)))
                        }
                        None => {
                            if sent_bytes == 0 {
                                let bytes = settlement.interrupt(&mut ctx, Vec::new(), &StreamFailure::interrupted("upstream returned an empty event stream"), "Upstream returned an empty event stream", sent_bytes);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                            }
                            if decoder.has_pending_data() {
                                let bytes = settlement.interrupt(&mut ctx, Vec::new(), &StreamFailure::interrupted("upstream ended with an incomplete event frame"), "Upstream ended with an incomplete event frame", sent_bytes);
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)));
                            }
                            // 流结束，发送最终事件（generate_final_events 内部会 finish()
                            // 累积器，据此判定是否有半截 / 非法工具调用 JSON）。
                            let final_events = ctx.generate_final_events();
                            let final_events = settlement.finish_structured(final_events);
                            settlement.tracer.mark_first_content(&final_events);
                            settlement.update(&ctx, sent_bytes);
                            if let Some(message) = ctx.tool_stream_refusal_message() {
                                settlement.finish(
                                    "error", "interrupted", Some("upstream_refusal"), Some(message), None,
                                );
                            } else if let Some(message) = ctx.tool_json_error_message() {
                                // 工具调用 JSON 半截 / 非法：实时流已回 200，无法改状态码，
                                // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
                                settlement.finish(
                                    "error",
                                    "error",
                                    Some(outcome::BAD_REQUEST),
                                    Some(&message),
                                    None,
                                );
                            } else if let Some(message) = ctx.repeat_guard_error_message() {
                                settlement.finish(
                                    "error",
                                    "error",
                                    Some(outcome::STREAM_INTERRUPTED),
                                    Some(message),
                                    None,
                                );
                            } else {
                                settlement.finish("success", "success", None, None, None);
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, settlement, sent_bytes)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, settlement, sent_bytes)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// Exactly-once settlement for a live Messages stream.
///
/// Responses consumes this stream as an inner body. If the outer client goes
/// away, dropping that body also drops the in-flight unfold future; `Drop`
/// records the latest usage snapshot and closes the trace instead of losing
/// already incurred provider usage.
struct StreamSettlement {
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    usage: TraceUsage,
    sent_bytes: u64,
    settled: bool,
    /// 结构化输出模式下的事件缓冲；None 表示按原样实时转发。
    structured: Option<StructuredCollector>,
    /// 在途请求守卫：随流处理上下文存活，流结束 / 中断后自动递减并发计数
    _in_flight: Option<std::sync::Arc<crate::kiro::token_manager::InFlightGuard>>,
}

impl StreamSettlement {
    /// 结构化模式下缓冲事件（返回空）；否则原样返回。
    fn hold_if_structured(&mut self, events: Vec<SseEvent>) -> Vec<SseEvent> {
        match self.structured.as_mut() {
            Some(collector) => {
                collector.hold(events);
                Vec::new()
            }
            None => events,
        }
    }

    /// 流正常结束：结构化模式下把缓冲 + 结尾事件改写后一次性返回。
    fn finish_structured(&mut self, final_events: Vec<SseEvent>) -> Vec<SseEvent> {
        match self.structured.as_mut() {
            Some(collector) => collector.finish(final_events),
            None => final_events,
        }
    }

    fn interrupt(
        &mut self,
        ctx: &mut StreamContext,
        mut events: Vec<SseEvent>,
        failure: &StreamFailure,
        error: &str,
        bytes: u64,
    ) -> Vec<Result<Bytes, Infallible>> {
        if let Some(collector) = self.structured.as_mut() {
            // 流被中断：已缓冲的事件按原顺序放在 error 事件之前，不做改写。
            let mut held = collector.take_unchanged();
            held.extend(events);
            events = held;
        }
        tracing::error!(error, error_type = failure.error_type, "Upstream response stream was interrupted");
        // M3：把上游原因归类后告诉客户端（限流 / 过载 / 输入过长 / 权限 / 可重试中断）。
        events.extend(ctx.generate_error_events(failure.error_type, &failure.message));
        self.update(ctx, bytes);
        self.finish(
            "error",
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some(error),
            Some(bytes),
        );
        events
            .into_iter()
            .map(|event| Ok(Bytes::from(event.to_sse_string())))
            .collect()
    }

    fn new(
        hook: UsageRecordHook,
        credential_id: u64,
        tracer: std::sync::Arc<RequestTracer>,
        ctx: &StreamContext,
        in_flight: Option<std::sync::Arc<crate::kiro::token_manager::InFlightGuard>>,
    ) -> Self {
        Self {
            hook,
            credential_id,
            tracer,
            usage: stream_trace_usage(ctx),
            sent_bytes: 0,
            settled: false,
            structured: None,
            _in_flight: in_flight,
        }
    }

    fn update(&mut self, ctx: &StreamContext, sent_bytes: u64) {
        self.usage = stream_trace_usage(ctx);
        self.sent_bytes = sent_bytes;
    }

    fn finish(
        &mut self,
        usage_status: &str,
        trace_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
    ) {
        if self.settled {
            return;
        }
        self.record_usage(usage_status);
        self.tracer.finalize(
            trace_status,
            error_type,
            error_message,
            interrupted_after_bytes,
            self.usage,
        );
        self.settled = true;
    }

    fn record_usage(&self, status: &str) {
        self.hook.record(
            self.credential_id,
            self.usage.input_tokens.min(i32::MAX as u64) as i32,
            self.usage.output_tokens.min(i32::MAX as u64) as i32,
            self.usage.cache_creation_tokens.min(i32::MAX as u64) as i32,
            self.usage.cache_read_tokens.min(i32::MAX as u64) as i32,
            self.usage.credits,
            status,
        );
    }
}

impl Drop for StreamSettlement {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        self.record_usage("error");
        self.tracer.finalize(
            "interrupted",
            Some(outcome::STREAM_INTERRUPTED),
            Some("response stream was cancelled before completion"),
            Some(self.sent_bytes),
            self.usage,
        );
        self.settled = true;
    }
}

/// 从 StreamContext 提取用量，转成 trace 行用量（与 record_stream_usage 同源）
fn stream_trace_usage(ctx: &StreamContext) -> TraceUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: ctx.resolved_output_tokens() as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        source: UsageSource::resolve(ctx.provider_token_usage.is_some(), &ctx.cache_usage),
        credits: if ctx.credits.is_finite() && ctx.credits > 0.0 {
            ctx.credits
        } else {
            0.0
        },
    }
}

pub(crate) enum NonStreamExecutionError {
    Provider(Error),
    Response(Response),
}

pub(crate) fn new_non_stream_request_tracer(
    state: &AppState,
    key_ctx: KeyContext,
    model: String,
) -> std::sync::Arc<RequestTracer> {
    std::sync::Arc::new(RequestTracer::new(
        state,
        RequestTraceOptions {
            key_ctx,
            model,
            is_stream: false,
            effort: None,
            session_key: None,
        },
    ))
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    // 非流式路径直接处理结构化 Event::ToolUse，不经过 <invoke> 文本嗅探，
    // 因此这里不需要工具表校验；保留参数以对齐调用方签名。
    _known_tool_names: std::collections::HashSet<String>,
    invoke_tool_schemas: super::stream::InvokeToolSchemas,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    structured_schema: Option<serde_json::Value>,
    custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
) -> Response {
    // L11：与流式同一套 `<invoke>` 文本识别规则（H2）。
    let reclaim_name_map = tool_name_map.clone();
    let (mut resp, credential_id) = match execute_non_stream_request(
        provider,
        request_body,
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        hook,
        cache_usage,
        tracer.clone(),
        group.clone(),
    )
    .await
    {
        Ok((mut response_body, credential_id)) => {
            reclaim_trailing_invoke_text(&mut response_body, &invoke_tool_schemas, &reclaim_name_map);
            if let Some(schema) = &structured_schema {
                rewrite_message_json(&mut response_body, schema, model);
            }
            ((StatusCode::OK, Json(response_body)).into_response(), Some(credential_id))
        }
        Err(NonStreamExecutionError::Provider(error)) => (map_provider_error(error), None),
        Err(NonStreamExecutionError::Response(response)) => (response, None),
    };
    attach_custom_headers(
        &mut resp,
        custom_headers.as_ref(),
        &tracer.header_context(credential_id, group.as_deref()),
    );
    resp
}

/// 非流式响应：本轮没有原生 tool_use 时，把末尾文本块里合格的 `<invoke>` 合成为 tool_use（H2 / L11）。
/// 规则与流式完全相同（见 `stream::InvokeSniffer`）；拒答等非正常终止不处理。
fn reclaim_trailing_invoke_text(
    response_body: &mut serde_json::Value,
    schemas: &super::stream::InvokeToolSchemas,
    tool_name_map: &std::collections::HashMap<String, String>,
) {
    if schemas.is_empty() {
        return;
    }
    let stop_reason = response_body.get("stop_reason").and_then(|v| v.as_str());
    if stop_reason != Some("end_turn") {
        return;
    }
    let Some(content) = response_body.get_mut("content").and_then(|v| v.as_array_mut()) else {
        return;
    };
    if content.iter().any(|b| b["type"] == "tool_use") {
        return;
    }
    let Some(text) = content
        .last()
        .filter(|b| b["type"] == "text")
        .and_then(|b| b["text"].as_str())
        .map(str::to_string)
    else {
        return;
    };
    let blocks = super::stream::extract_invoke_content_blocks(&text, schemas, tool_name_map);
    if !blocks.iter().any(|b| b["type"] == "tool_use") {
        return;
    }
    content.pop();
    content.extend(
        blocks
            .into_iter()
            .filter(|b| b["type"] != "text" || b["text"].as_str().is_some_and(|t| !t.trim().is_empty())),
    );
    response_body["stop_reason"] = json!("tool_use");
}

/// 上游流中断时返回给客户端的错误（M3）：Anthropic 错误类型 + 脱敏后的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamFailure {
    error_type: &'static str,
    message: String,
}

impl StreamFailure {
    /// 传输或帧层面的中断：可重试。
    fn interrupted(reason: &str) -> Self {
        Self {
            error_type: "api_error",
            message: format!("Upstream response stream was interrupted: {reason}. Retry the request."),
        }
    }

    /// 非流式路径的 HTTP 状态码，与 Anthropic 错误类型对应。
    fn status(&self) -> StatusCode {
        match self.error_type {
            "rate_limit_error" => StatusCode::TOO_MANY_REQUESTS,
            "overloaded_error" => StatusCode::SERVICE_UNAVAILABLE,
            "invalid_request_error" => StatusCode::BAD_REQUEST,
            "permission_error" => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_GATEWAY,
        }
    }
}

/// 只保留异常类型名（去掉 `com.amazon...#` 之类前缀，限制字符和长度），不回传上游原文。
fn sanitize_upstream_code(code: &str) -> String {
    let short = code.rsplit(['#', '.', ':']).next().unwrap_or(code);
    short
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

/// 按上游 error / exception 帧的类型和说明归类（M3）。说明只用于归类，不出现在返回给客户端的消息里。
fn classify_upstream_failure(code: &str, message: &str) -> StreamFailure {
    let name = sanitize_upstream_code(code);
    let label = if name.is_empty() { "unknown upstream error".to_string() } else { name };
    let code_lower = code.to_ascii_lowercase();
    let lower = format!("{code_lower} {}", message.to_ascii_lowercase());
    let has = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    let failure = |error_type: &'static str, message: String| StreamFailure { error_type, message };
    if has(&["throttl", "rate exceeded", "rate limit", "too many requests", "toomanyrequests"]) {
        return failure("rate_limit_error", format!("Upstream rate limit reached ({label}). Retry after a short wait."));
    }
    if has(&["serviceunavailable", "service unavailable", "overloaded", "high load", "insufficient capacity", "temporarily unavailable", "temporarily_unavailable"]) {
        return failure("overloaded_error", format!("Upstream model is overloaded ({label}). Retry later."));
    }
    if has(&["input is too long", "prompt is too long", "too many tokens", "context window", "context length", "exceeds the maximum"]) {
        return failure("invalid_request_error", format!("prompt is too long: the upstream rejected the input as exceeding the model context ({label})."));
    }
    if has(&["accessdenied", "access denied", "unauthorized", "forbidden"]) {
        return failure("permission_error", format!("Upstream denied access ({label})."));
    }
    if code_lower.contains("validation") {
        return failure("invalid_request_error", format!("Upstream rejected the request ({label})."));
    }
    StreamFailure::interrupted(&format!("upstream reported {label}"))
}

/// body 读取失败（连接断开或读超时）。
fn body_read_failure(error: &reqwest::Error) -> StreamFailure {
    if error.is_timeout() {
        StreamFailure::interrupted("no data arrived from upstream before the read deadline")
    } else {
        StreamFailure::interrupted("the connection to upstream was lost")
    }
}

/// 上游 error / exception 帧：返回 (日志用的完整原因, 返回给客户端的分类错误)。
fn upstream_event_error(event: &Event) -> Option<(String, StreamFailure)> {
    match event {
        Event::Error {
            error_code,
            error_message,
        } => Some((
            format!("{error_code}: {error_message}"),
            classify_upstream_failure(error_code, error_message),
        )),
        Event::Exception {
            exception_type,
            message,
        } if exception_type != "ContentLengthExceededException" => Some((
            format!("{exception_type}: {message}"),
            classify_upstream_failure(exception_type, message),
        )),
        _ => None,
    }
}

fn non_stream_read_failure(
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
    credential_id: u64,
    input_tokens: i32,
    error: &str,
    failure: StreamFailure,
) -> NonStreamExecutionError {
    hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
    tracer.finalize(
        "interrupted",
        Some(outcome::STREAM_INTERRUPTED),
        Some(error),
        None,
        TraceUsage::zero(),
    );
    NonStreamExecutionError::Response(
        (
            failure.status(),
            Json(ErrorResponse::new(failure.error_type, failure.message)),
        )
            .into_response(),
    )
}

pub(crate) async fn execute_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Result<(serde_json::Value, u64), NonStreamExecutionError> {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api(
            request_body,
            Some(input_tokens.max(0) as u64),
            Some(tracer.as_ref()),
            group.as_deref(),
            tracer.session_key(),
        )
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            if is_no_available_credentials_error(&e) {
                tracer.discard();
            } else {
                hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
                tracer.finalize(
                    "error",
                    last_attempt_outcome(&tracer),
                    Some(&format!("{e:#}")),
                    None,
                    TraceUsage::zero(),
                );
            }
            return Err(NonStreamExecutionError::Provider(e));
        }
    };
    tracer.mark_upstream_headers_received();
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    let mut body_stream = response.bytes_stream();
    let mut decoder = EventStreamDecoder::new();
    let mut received_data = false;

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature: Option<String> = None;
    let mut native_redacted_thinking: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    let mut native_stop_reason: Option<String> = None;
    let mut context_window_full = false;
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // metadataEvent.tokenUsage 是本次 provider 调用的精确最终快照。
    let mut provider_token_usage: Option<TokenUsage> = None;
    // meteringEvent 上报的 credit 计费量（上游真实下发）；
    // input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    let mut credits: f64 = 0.0;
    // 最近一次 meteringEvent 的完整 payload，用于在响应体 usage 中透传
    // credit_usage / credit_unit / credit_unit_plural 字段，与 /v1/messages
    // 流式（message_delta）行为一致；如果上游多次下发则取最后一次。
    let mut metering: Option<crate::kiro::model::events::MeteringEvent> = None;

    // 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    // 半截 / 非法 JSON 显式暴露为错误（返回 502），不再静默回退 {} 或丢弃。
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::new();
    let mut tool_json_error: Option<super::stream::ToolJsonAccumulatorError> = None;
    // 上游内容过滤拒答：拦截后只剩 reasoning，必须显式标成 refusal，
    // 否则会被当成正常 end_turn 的空响应返回。
    let mut refused = false;

    while let Some(chunk) = body_stream.next().await {
        let body_bytes = match chunk {
            Ok(bytes) => bytes,
            Err(e) => {
                let failure = body_read_failure(&e);
                return Err(non_stream_read_failure(
                    &hook,
                    &tracer,
                    credential_id,
                    input_tokens,
                    &format!("{:#}", anyhow::Error::new(e)),
                    failure,
                ));
            }
        };
        received_data |= !body_bytes.is_empty();
        if let Err(e) = decoder.feed(&body_bytes) {
            return Err(non_stream_read_failure(
                &hook,
                &tracer,
                credential_id,
                input_tokens,
                &e.to_string(),
                StreamFailure::interrupted("malformed event frame from upstream"),
            ));
        }
        for result in decoder.decode_iter() {
            match result {
                Ok(frame) => {
                    let event = match Event::from_frame(frame) {
                        Ok(event) => event,
                        Err(e) => {
                            return Err(non_stream_read_failure(
                                &hook,
                                &tracer,
                                credential_id,
                                input_tokens,
                                &e.to_string(),
                                StreamFailure::interrupted("malformed event frame from upstream"),
                            ));
                        }
                    };
                    if let Some((error, failure)) = upstream_event_error(&event) {
                        return Err(non_stream_read_failure(
                            &hook,
                            &tracer,
                            credential_id,
                            input_tokens,
                            &error,
                            failure,
                        ));
                    }
                    {
                        match event {
                            Event::AssistantResponse(resp) => {
                                text_content.push_str(&resp.content);
                            }
                            Event::ReasoningContent(reasoning) => {
                                if let Some(text) = reasoning.text
                                    && !text.is_empty()
                                {
                                    // 本地：非流式只累加思考字符数（无从测量时长）
                                    tracer.observe_reasoning_chars(&text);
                                    native_thinking.push_str(&text);
                                }
                                if let Some(signature) = reasoning.signature
                                    && !signature.is_empty()
                                {
                                    native_thinking_signature = Some(signature);
                                }
                                if let Some(redacted) = reasoning.redacted_content
                                    && !redacted.is_empty()
                                {
                                    native_redacted_thinking.push(redacted);
                                }
                            }
                            Event::ToolUse(tool_use) => {
                                has_tool_use = true;
                                match tool_accumulator.push(&tool_use, &tool_name_map) {
                                    Ok(Some(completed)) => {
                                        tool_uses.push(completed.to_anthropic_block());
                                    }
                                    Ok(None) => {}
                                    Err(e) => {
                                        tracing::error!("{}", e);
                                        tool_json_error = Some(e);
                                    }
                                }
                            }
                            Event::Metadata(metadata) => {
                                if let Some(reason) = metadata.stop_reason.as_ref() {
                                    native_stop_reason = Some(reason.to_ascii_uppercase());
                                }
                                if let Some(usage) = metadata.token_usage {
                                    let usage = usage.sanitized();
                                    tracing::debug!(
                                        uncached_input_tokens = usage.uncached_input_tokens,
                                        cache_write_input_tokens = usage.cache_write_input_tokens,
                                        cache_read_input_tokens = usage.cache_read_input_tokens,
                                        output_tokens = usage.output_tokens,
                                        "收到 metadataEvent.tokenUsage 精确用量"
                                    );
                                    // 单条 provider 流内是最终快照，重复事件取最后一份。
                                    provider_token_usage = Some(usage);
                                }
                                if let Some(refusal) = metadata.content_filter_refusal() {
                                    tracing::warn!("上游内容过滤拒答: {}", refusal);
                                    refused = true;
                                }
                            }
                            Event::ContextUsage(context_usage) => {
                                // 从上下文使用百分比计算实际的 input_tokens
                                let window_size = get_context_window_size(model);
                                let actual_input_tokens =
                                    (context_usage.context_usage_percentage * (window_size as f64)
                                        / 100.0) as i32;
                                context_input_tokens = Some(actual_input_tokens);
                                if context_usage.context_usage_percentage >= 100.0 {
                                    context_window_full = true;
                                }
                                tracing::debug!(
                                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                    context_usage.context_usage_percentage,
                                    actual_input_tokens
                                );
                            }
                            Event::Metering(event_metering) => {
                                // 上游只下发 credit；token / cache 字段不存在
                                credits += event_metering.usage;
                                tracing::debug!(
                                    usage = event_metering.usage,
                                    unit = %event_metering.unit,
                                    unit_plural = %event_metering.unit_plural,
                                    "metering credits +{:.6}", event_metering.usage
                                );
                                metering = Some(event_metering);
                            }
                            Event::Exception { exception_type, .. } => {
                                if exception_type == "ContentLengthExceededException" {
                                    stop_reason = "max_tokens".to_string();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    return Err(non_stream_read_failure(
                        &hook,
                        &tracer,
                        credential_id,
                        input_tokens,
                        &e.to_string(),
                        StreamFailure::interrupted("malformed event frame from upstream"),
                    ));
                }
            }
        }
    }

    if !received_data || decoder.has_pending_data() {
        return Err(non_stream_read_failure(
            &hook,
            &tracer,
            credential_id,
            input_tokens,
            if received_data { "Upstream ended with an incomplete event frame" } else { "Upstream returned an empty event stream" },
            StreamFailure::interrupted(if received_data { "upstream ended with an incomplete event frame" } else { "upstream returned an empty event stream" }),
        ));
    }

    // 收尾：无参工具的 0 字节滞留缓冲在此还原为 {} 补进 content；累积了非空内容
    // 却从未收到 stop=true 的（上游在参数写到一半时截断）返回 IncompleteJson。
    // 已有错误则保持不变。
    if refused {
        // A filter refusal can cut off tool arguments. Do not recover pending
        // calls or replace the authoritative refusal with their JSON errors.
        tool_json_error = None;
    } else if tool_json_error.is_none() {
        match tool_accumulator.finish(&tool_name_map) {
            Ok(recovered) => {
                for completed in recovered {
                    tool_uses.push(completed.to_anthropic_block());
                }
            }
            Err(e) => {
                tracing::error!("{}", e);
                tool_json_error = Some(e);
            }
        }
    }

    // 工具调用 JSON 半截 / 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        if let Some(usage) = provider_token_usage {
            let usage = usage.sanitized();
            let trace_usage = TraceUsage {
                input_tokens: usage.uncached_input_tokens as u64,
                output_tokens: usage.output_tokens as u64,
                cache_creation_tokens: usage.cache_write_input_tokens as u64,
                cache_read_tokens: usage.cache_read_input_tokens as u64,
                credits: if credits.is_finite() && credits > 0.0 {
                    credits
                } else {
                    0.0
                },
                source: UsageSource::Provider,
            };
            hook.record(
                credential_id,
                usage.uncached_input_tokens,
                usage.output_tokens,
                usage.cache_write_input_tokens,
                usage.cache_read_input_tokens,
                credits,
                "error",
            );
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                trace_usage,
            );
        } else {
            // metadata 缺失时保留原有错误口径，不把不完整工具输出估算成已消费量。
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(&message),
                None,
                TraceUsage::zero(),
            );
        }
        return Err(NonStreamExecutionError::Response(
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("upstream_tool_json_error", message)),
            )
                .into_response(),
        ));
    }

    // 确定 stop_reason：拒答是终态，优先于同流内的 max_tokens / 上下文溢出 / tool_use。
    if refused {
        stop_reason = super::stream::REFUSAL_STOP_REASON.to_string();
    } else {
        match native_stop_reason.as_deref() {
            Some("MAX_TOKENS") => stop_reason = "max_tokens".to_string(),
            Some("MODEL_CONTEXT_WINDOW_EXCEEDED") => stop_reason = "model_context_window_exceeded".to_string(),
            Some("STOP_SEQUENCE") => stop_reason = "stop_sequence".to_string(),
            None if context_window_full && stop_reason == "end_turn" => {
                stop_reason = "model_context_window_exceeded".to_string();
            }
            _ => {}
        }
        if has_tool_use && stop_reason == "end_turn" {
            stop_reason = "tool_use".to_string();
        }
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（非流式：整段文本已就绪，一次性剥离）。
    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 构建响应内容
    let mut content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    content.extend(tool_uses);

    // provider 未下发 metadataEvent 时才使用本地输出估算。
    let fallback_output_tokens = token::estimate_output_tokens(&content);
    let (final_input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens) =
        resolve_non_stream_usage(
            input_tokens,
            context_input_tokens,
            fallback_output_tokens,
            cache_usage,
            provider_token_usage,
        );

    // 构建 Anthropic 响应
    let mut usage_json = json!({
        "input_tokens": final_input_tokens,
        "output_tokens": output_tokens,
        "cache_creation_input_tokens": cache_creation_tokens,
        "cache_read_input_tokens": cache_read_tokens
    });
    // 透传上游 meteringEvent 的 credit_* 字段，让客户端拿到与 Kiro
    // 后端口径一致的计费元数据；只在收到过 meteringEvent 时才追加。
    if let Some(m) = &metering {
        usage_json["credit_usage"] = json!(m.usage);
        usage_json["credit_unit"] = json!(m.unit);
        usage_json["credit_unit_plural"] = json!(m.unit_plural);
    }
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_json
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        credits,
        "success",
    );
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage {
            input_tokens: final_input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            source: UsageSource::resolve(provider_token_usage.is_some(), &cache_usage),
        },
    );
    Ok((response_body, credential_id))
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
                "signature": native_thinking_signature
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string()),
            }));
        }

        // redacted_thinking 源自原生 reasoning 事件流，按上游顺序排在正文之前
        // （与流式版 thinking → redacted → text 的顺序一致）。
        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        // 从完整文本中提取**全部** thinking 块，兼容旧的 <thinking> 文本路径。
        // 一条回复可能含多段（交错思考 / 模型模仿历史回传格式），
        // 每段独立成块，正文按出现位置穿插。原生 thinking 已存在时同样执行：
        // 混流场景（原生事件 + 文本标签）下文本里的思考段不能漏进正文。
        let segments = super::stream::extract_thinking_from_complete_text(&text_content);

        for (is_thinking, segment) in segments {
            if segment.is_empty() {
                continue;
            }
            if is_thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": segment,
                    "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
                }));
            } else {
                content.push(json!({
                    "type": "text",
                    "text": segment
                }));
            }
        }
    } else if !text_content.trim().is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }
    // Native reasoning must never be relabeled as ordinary text. When thinking
    // is disabled and there is no visible output, preserve the empty content
    // response (tool blocks are appended by the caller).
    content
}

/// 从转换结果里取出最终下发的推理思考级别（effort）。
///
/// 取的是 converter 归一化后、真正写进 `additionalModelRequestFields` 的值，与实际
/// 发往上游的请求一致；未请求 effort 时为 None。
///
/// 两个键都要读：Claude 系落在 `output_config`，GPT-5.6 系落在 `reasoning`。二者
/// 互斥，只会命中一个；漏读会让 gpt 请求的 effort 在用量统计和 trace 里记成空。
fn extract_effort(fields: Option<&AdditionalModelRequestFields>) -> Option<String> {
    let fields = fields?;
    fields
        .output_config
        .as_ref()
        .map(|oc| oc.effort.clone())
        .or_else(|| fields.reasoning.as_ref().map(|r| r.effort.clone()))
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - 仅支持 adaptive 的模型（Opus 4.6+ / Sonnet 4.6+ / Fable 5 / Mythos 5 / Haiku 5+，判定见
///   [`backend_requires_adaptive_thinking`]）与非 Claude 的原生 reasoning 模型：覆写为 adaptive
/// - Opus 4.5 / Sonnet 4.5 等仍保留固定预算模式的模型：覆写为 enabled 类型
/// - 保留客户端给的 `budget_tokens` 与显式 `output_config`；没有 budget 时取 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    // 先映射成 backend_id 再判定：客户端原始名形态太多（4-6/4.6/-thinking 后缀/
    // 自定义别名），直接在原始名上匹配会漏模型。映射失败时回退原始名，保持旧行为。
    let mapped = super::converter::map_model(&payload.model)
        .unwrap_or_else(|| payload.model.clone())
        .to_ascii_lowercase();
    // Claude 系按本地 thinking_mode 的枚举判定（4.6 起 adaptive，4.5 及更早固定预算）；
    // 非 Claude 的原生 reasoning 模型（gpt-5.6 / grok-4.7 / 自定义 supportsReasoning）
    // 沿用上游口径一律 adaptive。
    let is_adaptive_thinking = backend_requires_adaptive_thinking(&mapped)
        || (!mapped.starts_with("claude-")
            && super::converter::model_supports_native_reasoning(&mapped));

    let thinking_type = if is_adaptive_thinking {
        "adaptive"
    } else {
        "enabled"
    };
    // 上游口径：保留客户端给的 budget_tokens（effort 由它推导），没有才用默认 20000。
    let budget_tokens = payload
        .thinking
        .as_ref()
        .map(|t| t.budget_tokens)
        .filter(|b| *b > 0)
        .unwrap_or(20000);

    tracing::info!(
        model = %payload.model,
        mapped_model = %mapped,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens,
    });

    // 客户端已显式给了 output_config（含 effort / format）时不覆写，避免丢掉 format。
    if is_adaptive_thinking && payload.output_config.is_none() {
        payload.output_config = Some(OutputConfig {
            effort: super::converter::effort_from_budget_tokens(budget_tokens).to_string(),
            format: None,
        });
    }
}

/// web_search 路径（纯 web_search 与混合工具循环）暂不支持结构化输出：请求按普通方式处理，
/// 这里只记一条 warn，避免调用方拿到自由文本却无从排查。
fn warn_structured_output_with_web_search(payload: &MessagesRequest) {
    let wants_schema = payload
        .output_config
        .as_ref()
        .and_then(|config| config.json_schema())
        .is_some();
    if wants_schema
        && (websearch::has_web_search_tool(payload)
            || websearch::has_web_search_among_tools(payload))
    {
        tracing::warn!(
            model = %payload.model,
            "structured output: output_config.format is not supported together with web_search; the reply is returned unchanged"
        );
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    // 与 /v1/messages 一致：结构化输出注入的 schema 指令也要计入
    append_schema_instruction(payload.output_config.as_ref(), &mut payload.system);

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    let state = state.snapshot_tool_compatibility_mode();
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );
    if let Err(error) = validate_max_tokens(payload.max_tokens) {
        return (StatusCode::BAD_REQUEST, Json(error)).into_response();
    }
    let mut hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检查是否有可用凭据（无可用账号直接返回 401，避免创建 tracer 和写入日志）
    if request_has_no_credentials(&provider, &payload, key_ctx.group.as_deref()) {
        tracing::warn!(
            model = %payload.model,
            group = ?key_ctx.group,
            "无可用凭据，直接返回 401"
        );
        let mut resp = (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse::new(
                "authentication_error",
                "No available credentials for this request",
            )),
        ).into_response();
        let trace_id = Uuid::new_v4().to_string();
        attach_custom_headers(&mut resp, state.custom_headers.as_ref(), &early_header_context(&trace_id, &payload.model, &key_ctx, None));
        return resp;
    }

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);
    warn_structured_output_with_web_search(&payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        // 与 /v1/messages 同理：提前返回路径挂 tracer 补齐请求日志
        hook.attach_tracer(std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload.stream,
                effort: None,
                session_key: session_affinity_key(&payload),
            },
        )));
        let mut resp = websearch::handle_websearch_request(
            provider,
            &payload,
            input_tokens,
            key_ctx.group.as_deref(),
        )
        .await;
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        if resp.status() != StatusCode::UNAUTHORIZED {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        } else {
            // 无可用凭据的 401 不计用量也不落 trace；上游的 Drop 兜底会补「中断」行，这里显式丢弃。
            hook.discard_tracer();
        }
        let trace_id = Uuid::new_v4().to_string();
        attach_custom_headers(&mut resp, state.custom_headers.as_ref(), &early_header_context(&trace_id, &payload.model, &key_ctx, None));
        return resp;
    }

    // 以下都会进入上游对话，按请求统计一次 conversationId 来源
    super::conversation_id_stats::record(&payload);

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: payload_stream,
                effort: None,
                session_key: session_affinity_key(&payload),
            },
        ));
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            tracer,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            state.cache_meter.clone(),
            key_ctx.key_id,
            state.custom_headers.clone(),
        )
        .await;
    }

    // output_config.format=json_schema：Kiro 无原生字段，注入 schema 指令，响应侧再提取改写。
    let structured_schema = inject_system_instruction(&mut payload);

    // 转换请求
    let conversion_result = match convert_request_with_mode(&payload, state.tool_compatibility_mode)
    {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::InvalidModel(reason) => {
                    ("invalid_request_error", format!("无效模型 ID: {}", reason))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::InvalidMessageSequence(reason) => (
                    "invalid_request_error",
                    format!("消息序列无效: {}", reason),
                ),
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let mut resp = (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
            let trace_id = Uuid::new_v4().to_string();
            attach_custom_headers(
                &mut resp,
                state.custom_headers.as_ref(),
                &early_header_context(&trace_id, &payload.model, &key_ctx, None),
            );
            return resp;
        }
    };

    // 最终下发的推理思考级别，落入用量日志与链路追踪
    let effort = extract_effort(conversion_result.additional_model_request_fields.as_ref());
    hook.set_effort(effort.clone());

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    // H2：发给上游的工具 schema，用于 `<invoke>` 文本合成校验（conversation_state 随后被移走）。
    let invoke_tool_schemas = super::stream::invoke_tool_schemas_from_state(
        &conversion_result.conversation_state,
        &conversion_result.known_tool_names,
    );
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let mut resp = (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
            let trace_id = Uuid::new_v4().to_string();
            attach_custom_headers(
                &mut resp,
                state.custom_headers.as_ref(),
                &early_header_context(&trace_id, &payload.model, &key_ctx, None),
            );
            return resp;
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 计算总 input tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存（estimate 口径）。
    let cache_usage = match state.cache_meter.as_ref() {
        Some(cache) => {
            super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id).await
        }
        None => super::cache_metering::CacheUsage::default(),
    };

    if payload.stream {
        // Stream content immediately; final usage arrives in message_delta.
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
                effort: effort.clone(),
                session_key: session_affinity_key(&payload),
            },
        ));
        handle_cc_stream_request(
            provider,
            &request_body,
            &payload.model,
            // L7：流式与非流式一致，遵守 extractThinking 配置。
            (state.extract_thinking || payload.output_config.is_some()) && thinking_enabled,
            tool_name_map,
            known_tool_names,
            invoke_tool_schemas,
            hook,
            total_input_tokens,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            structured_schema.clone(),
            state.custom_headers.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = (state.extract_thinking || payload.output_config.is_some()) && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
                effort: effort.clone(),
                session_key: session_affinity_key(&payload),
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            tool_name_map,
            known_tool_names,
            invoke_tool_schemas,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            structured_schema.clone(),
            state.custom_headers.clone(),
        )
        .await
    }
}

/// Claude Code uses the live stream too. Initial usage is an estimate;
/// message_delta carries the final provider usage without delaying content.
async fn handle_cc_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    invoke_tool_schemas: super::stream::InvokeToolSchemas,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    structured_schema: Option<serde_json::Value>,
    custom_headers: Option<crate::model::custom_headers::CustomHeadersManager>,
) -> Response {
    handle_stream_request(
        provider,
        request_body,
        model,
        fallback_input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        invoke_tool_schemas,
        hook,
        cache_usage,
        tracer,
        group,
        structured_schema,
        custom_headers,
    )
    .await
}

/// 请求体没带 metadata.user_id 时，用会话亲和请求头（x-session-affinity 等）补上。
/// pi-ai 系客户端（例如 DeepSeek Harness）不发 metadata.user_id，只在请求头里带会话 id。不补的话：缓存计量拿不到
/// 会话隔离种子，缓存读写恒为 0；conversationId 每次随机，按会话固定账号的路由不命中。
/// 客户端自己带了 user_id 时不覆盖。
fn fill_session_metadata_from_headers(payload: &mut MessagesRequest, headers: &HeaderMap) {
    let has_user_id = payload
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .is_some_and(|s| !s.trim().is_empty());
    if has_user_id {
        return;
    }
    if let Some(metadata) = super::openai::resolve_session_metadata(None, headers) {
        payload.metadata = Some(metadata);
    }
}

/// `POST /v1/messages` 路由入口：先按请求头补会话 metadata，再交给 post_messages。
pub async fn post_messages_route(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    fill_session_metadata_from_headers(&mut payload, &headers);
    post_messages(State(state), Extension(key_ctx), JsonExtractor(payload)).await
}

/// `POST /cc/v1/messages` 路由入口：先按请求头补会话 metadata，再交给 post_messages_cc。
pub async fn post_messages_cc_route(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    fill_session_metadata_from_headers(&mut payload, &headers);
    post_messages_cc(State(state), Extension(key_ctx), JsonExtractor(payload)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_affinity_header_scopes_cache_metering() {
        use crate::anthropic::cache_metering::{CacheMeter, compute_cache_usage_sync};
        use crate::anthropic::types::{CacheControl, Message as AMessage, Metadata};
        let body = "conversation prefix that stays stable ".repeat(20);
        let make = |headers: &HeaderMap| {
            let mut req = MessagesRequest {
                model: "claude-opus-4-8".to_string(),
                max_tokens: 64,
                messages: vec![
                    AMessage { role: "user".into(), content: serde_json::json!([{"type": "text", "text": body}]) },
                    AMessage { role: "assistant".into(), content: serde_json::json!([{"type": "text", "text": body}]) },
                    AMessage { role: "user".into(), content: serde_json::json!([{"type": "text", "text": body}]) },
                ],
                stream: false,
                system: None,
                tools: None,
                tool_choice: None,
                thinking: None,
                output_config: None,
                metadata: None,
                cache_control: Some(CacheControl { cache_type: "ephemeral".to_string(), ttl: None }),
            };
            fill_session_metadata_from_headers(&mut req, headers);
            req
        };
        let dsh = |uuid: &str| {
            let mut h = HeaderMap::new();
            h.insert("x-session-affinity", format!("session-{uuid}").parse().unwrap());
            h
        };
        let a = dsh("0b4445e1-f5be-49e1-87ce-62bbc28ad705");
        let b = dsh("67e55044-10b1-426f-9247-bb680e5fe0c8");
        let none = HeaderMap::new();
        let cache = CacheMeter::new(None);
        assert_eq!(compute_cache_usage_sync(&cache, &make(&a), 0).cache_read, 0, "首轮只写不读");
        assert_eq!(compute_cache_usage_sync(&cache, &make(&b), 0).cache_read, 0, "不同会话不串");
        assert!(compute_cache_usage_sync(&cache, &make(&a), 0).cache_read > 0, "同一会话第二轮应如实上报命中");
        assert_eq!(compute_cache_usage_sync(&cache, &make(&none), 0).cache_read, 0, "没有会话头时仍不模拟跨用户命中");

        let mut own = make(&none);
        own.metadata = Some(Metadata { user_id: Some("user_x_account__session_123e4567-e89b-12d3-a456-426614174000".into()) });
        fill_session_metadata_from_headers(&mut own, &a);
        assert_eq!(
            own.metadata.unwrap().user_id.as_deref(),
            Some("user_x_account__session_123e4567-e89b-12d3-a456-426614174000"),
            "客户端自带的 user_id 不被请求头覆盖"
        );
    }
    use crate::model::config::ToolCompatibilityMode;

    #[test]
    fn count_tokens_includes_structured_output_instruction() {
        let request = |output_config: serde_json::Value| -> CountTokensRequest {
            serde_json::from_value(json!({
                "model": "claude-sonnet-4-6",
                "messages": [{"role": "user", "content": "Is the goal done?"}],
                "output_config": output_config
            }))
            .unwrap()
        };
        let schema = json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]});
        let mut with_format = request(json!({"format": {"type": "json_schema", "schema": schema}}));
        assert!(append_schema_instruction(with_format.output_config.as_ref(), &mut with_format.system).is_some());
        let system = with_format.system.as_ref().unwrap();
        assert_eq!(system.len(), 1);
        assert!(system[0].text.contains("\"ok\""));
        // 不带 format：不注入
        let mut plain = request(json!({"effort": "high"}));
        assert!(append_schema_instruction(plain.output_config.as_ref(), &mut plain.system).is_none());
        assert!(plain.system.is_none());
        // output_config 不会被转发给外部 count_tokens API
        assert!(serde_json::to_value(&with_format).unwrap().get("output_config").is_none());
    }

    #[test]
    fn opus_4_6_thinking_override_keeps_output_format() {
        let mut payload: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6-thinking",
            "messages": [{"role": "user", "content": "hi"}],
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}}
        }))
        .unwrap();
        override_thinking_from_model_name(&mut payload);
        let config = payload.output_config.as_ref().unwrap();
        assert_eq!(config.effort, "high");
        assert!(config.json_schema().is_some());
        assert!(inject_system_instruction(&mut payload).is_some());
    }

    #[test]
    fn thinking_suffix_preserves_budget_and_explicit_effort() {
        for (model, effort) in [("claude-sonnet-5.5-thinking", "low"), ("gpt-5.6-sol-thinking", "low")] {
            let mut payload: MessagesRequest = serde_json::from_value(json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello"}],
                "thinking": {"type": "enabled", "budget_tokens": 3000}
            })).unwrap();
            override_thinking_from_model_name(&mut payload);
            assert_eq!(payload.thinking.as_ref().unwrap().thinking_type, "adaptive");
            let conversion = convert_request_with_mode(&payload, ToolCompatibilityMode::ClaudeCode).unwrap();
            let fields = conversion.additional_model_request_fields.unwrap();
            let actual = if model.starts_with("gpt-") {
                fields.reasoning.unwrap().effort
            } else {
                fields.output_config.unwrap().effort
            };
            assert_eq!(actual, effort);
        }

        let mut payload: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-6-thinking",
            "messages": [{"role": "user", "content": "hello"}],
            "thinking": {"type": "enabled", "budget_tokens": 3000},
            "output_config": {"effort": "medium", "format": {"type": "json_schema", "schema": {"type": "object"}}}
        })).unwrap();
        override_thinking_from_model_name(&mut payload);
        let output = payload.output_config.unwrap();
        assert_eq!(output.effort, "medium");
        assert!(output.json_schema().is_some());
    }

    #[test]
    fn dropped_stream_settles_latest_usage_exactly_once() {
        let aggregator = std::sync::Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(false, ToolCompatibilityMode::Raw).with_usage(
            None,
            None,
            Some(aggregator.clone()),
        );
        let hook = UsageRecordHook::from_state(&state, 0, "test-model".to_string());
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                },
                model: "test-model".to_string(),
                is_stream: true,
                effort: None,
                session_key: None,
            },
        ));
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            11,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.output_tokens = 7;
        ctx.credits = 0.5;

        let mut settlement = StreamSettlement::new(hook, 42, tracer, &ctx, None);
        settlement.update(&ctx, 123);
        drop(settlement);

        let overview = aggregator.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);
        assert_eq!(overview.today_input_tokens, 11);
        assert_eq!(overview.today_output_tokens, 7);
        assert_eq!(overview.today_credits, 0.5);
    }

    /// CC 实时流在客户端断开时：Drop 必须记账 + 落 trace，
    /// 且已产生的用量不能丢（与 /v1 的 StreamSettlement 行为一致）。
    #[test]
    fn dropped_cc_live_stream_settles_usage_and_trace() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let aggregator = std::sync::Arc::new(crate::admin::usage_stats::UsageAggregator::new());
        let state = AppState::new(false, ToolCompatibilityMode::Raw)
            .with_usage(None, None, Some(aggregator.clone()))
            .with_trace_store(Some(store.clone()));
        let hook = UsageRecordHook::from_state(&state, 0, "test-model".to_string());
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                },
                model: "test-model".to_string(),
                is_stream: true,
                effort: None,
                session_key: None,
            },
        ));
        let mut ctx = StreamContext::new_with_thinking(
            "test-model",
            11,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        );
        ctx.cache_usage = super::super::cache_metering::CacheUsage::default();

        let mut settlement = StreamSettlement::new(hook, 42, tracer, &ctx, None);
        settlement.update(&ctx, 456);
        drop(settlement);

        // 用量必须落账（不能因取消而丢失）
        let overview = aggregator.overview();
        assert_eq!(overview.today_calls, 1);
        assert_eq!(overview.today_errors, 1);

        // trace 必须留痕，且归类为流中断（而非 abandoned_before_headers）
        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        assert_eq!(records[0].final_status, "interrupted");
        assert_eq!(
            records[0].error_type.as_deref(),
            Some(outcome::STREAM_INTERRUPTED)
        );
        assert_eq!(records[0].interrupted_after_bytes, Some(456));
    }

    #[test]
    fn tracer_renumbers_attempts_across_provider_rounds_before_persisting() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            store: Some(store.clone()),
            trace_id: "gpt-websearch-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 7,
            key_source: TraceKeySource::ClientKey,
            client_ip: None,
            model: "gpt-5.6-luna".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            effort: None,
            first_reasoning_at: parking_lot::Mutex::new(None),
            first_answer_at: parking_lot::Mutex::new(None),
            thinking_chars: std::sync::atomic::AtomicU64::new(0),
            token_manager: None,
            attempts: parking_lot::Mutex::new(Vec::new()),
            session_key: None,
            affinity: parking_lot::Mutex::new(None),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };

        let attempt = |attempt, credential_id, outcome: &str| TraceAttempt {
            attempt,
            credential_id,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        // First provider round reports local attempts 0,1; the next round starts at 0 again.
        tracer.on_attempt(attempt(0, 11, outcome::TRANSIENT));
        tracer.on_attempt(attempt(1, 12, outcome::SUCCESS));
        tracer.on_attempt(attempt(0, 13, outcome::SUCCESS));
        tracer.finalize(
            "success",
            None,
            None,
            None,
            TraceUsage {
                input_tokens: 101,
                output_tokens: 23,
                cache_creation_tokens: 7,
                cache_read_tokens: 89,
                credits: 0.25,
                source: UsageSource::Simulated,
            },
        );

        let (records, total) = store.query_paged(&TraceQuery {
            model: Some("gpt-5.6-luna".to_string()),
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 13);
        assert_eq!(record.total_attempts, 3);
        assert_eq!(
            record
                .attempts
                .iter()
                .map(|a| a.attempt)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(record.input_tokens, 101);
        assert_eq!(record.output_tokens, 23);
        assert_eq!(record.cache_creation_tokens, 7);
        assert_eq!(record.cache_read_tokens, 89);
        assert_eq!(record.credits, 0.25);
    }

    #[test]
    fn tracer_only_marks_first_token_for_streaming_requests() {
        let mut tracer = RequestTracer {
            store: None,
            trace_id: "first-token-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: false,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            effort: None,
            first_reasoning_at: parking_lot::Mutex::new(None),
            first_answer_at: parking_lot::Mutex::new(None),
            thinking_chars: std::sync::atomic::AtomicU64::new(0),
            token_manager: None,
            attempts: parking_lot::Mutex::new(Vec::new()),
            session_key: None,
            affinity: parking_lot::Mutex::new(None),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };

        tracer.mark_first_token();
        assert!(tracer.first_token_at.lock().is_none());

        tracer.is_stream = true;
        tracer.mark_first_token();
        let first = *tracer.first_token_at.lock();
        assert!(first.is_some());

        tracer.mark_first_token();
        assert_eq!(*tracer.first_token_at.lock(), first);
    }

    #[test]
    fn tracer_uses_terminal_mcp_attempt_for_failure_fields() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        let tracer = RequestTracer {
            store: Some(store.clone()),
            trace_id: "mcp-failure-trace".to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 0,
            key_source: TraceKeySource::MasterApiKey,
            client_ip: None,
            model: "claude-sonnet-4".to_string(),
            is_stream: true,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            effort: None,
            first_reasoning_at: parking_lot::Mutex::new(None),
            first_answer_at: parking_lot::Mutex::new(None),
            thinking_chars: std::sync::atomic::AtomicU64::new(0),
            token_manager: None,
            attempts: parking_lot::Mutex::new(Vec::new()),
            session_key: None,
            affinity: parking_lot::Mutex::new(None),
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        };
        let attempt = |credential_id, endpoint: &str, status, attempt_outcome: &str| TraceAttempt {
            attempt: 0,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status: Some(status),
            outcome: attempt_outcome.to_string(),
            error_snippet: None,
            duration_ms: 10,
        };

        tracer.on_attempt(attempt(11, "ide", 200, outcome::SUCCESS));
        tracer.on_attempt(attempt(29, "cli", 503, outcome::TRANSIENT));
        tracer.finalize(
            "error",
            last_attempt_outcome(&tracer),
            Some("MCP request failed"),
            None,
            TraceUsage::zero(),
        );

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1);
        let record = &records[0];
        assert_eq!(record.final_credential_id, 29);
        assert_eq!(record.error_type.as_deref(), Some(outcome::TRANSIENT));
        assert_eq!(record.total_attempts, 2);
        assert_eq!(record.attempts[0].endpoint, "ide");
        assert_eq!(record.attempts[1].endpoint, "cli");
    }

    /// 造一个只带 trace store 的 hook + tracer，模拟 web_search 分支的接线。
    fn hook_with_tracer(
        store: SharedTraceStore,
        is_stream: bool,
    ) -> (UsageRecordHook, std::sync::Arc<RequestTracer>) {
        let tracer = std::sync::Arc::new(RequestTracer {
            store: Some(store),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 7,
            key_source: TraceKeySource::ClientKey,
            model: "gpt-5.6-sol".to_string(),
            is_stream,
            effort: None,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            first_reasoning_at: parking_lot::Mutex::new(None),
            first_answer_at: parking_lot::Mutex::new(None),
            thinking_chars: std::sync::atomic::AtomicU64::new(0),
            attempts: parking_lot::Mutex::new(Vec::new()),
            token_manager: None,
            session_key: None,
            affinity: parking_lot::Mutex::new(None),
            client_ip: None,
            route: parking_lot::Mutex::new(None),
            finalized: std::sync::atomic::AtomicBool::new(false),
            upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
        });
        let mut hook = UsageRecordHook {
            recorder: None,
            aggregator: None,
            client_keys: None,
            credit_token_manager: None,
            key_id: 7,
            model: "gpt-5.6-sol".to_string(),
            effort: None,
            started_at: Instant::now(),
            tracer: None,
            count_call: true,
        };
        hook.attach_tracer(tracer.clone());
        (hook, tracer)
    }

    /// web_search 分支在主链路 tracer 之前就 return，历史上导致「有次数有用量、
    /// 请求日志为空」。契约：hook 挂上 tracer 后，record 必须兼任 trace 落库。
    #[test]
    fn attached_tracer_finalizes_on_record() {
        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, _tracer) = hook_with_tracer(store.clone(), false);

        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1, "record 应落一条 trace");
        assert_eq!(rows[0].key_id, 7);
        assert_eq!(rows[0].final_status, "success");
        assert_eq!(rows[0].input_tokens, 1000);
        assert_eq!(rows[0].output_tokens, 200);
        assert!(rows[0].error_type.is_none());
    }

    /// 老库（缺思考三列）迁移后要能正常读写，且历史行不丢。
    ///
    /// 用真实文件库而非内存库：内存库每次都按最新 SCHEMA 建表，走不到
    /// `migrate` 的 ALTER 分支，正是这个分支决定现网 db 能否平滑升级。
    #[test]
    fn legacy_db_without_thinking_columns_migrates_and_keeps_rows() {
        let dir = std::env::temp_dir().join(format!("kiro-trace-mig-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("traces.db");

        // 造一个"老版本"库：字段停在加思考列之前
        {
            let conn = rusqlite::Connection::open(&path).expect("建老库");
            conn.execute_batch(
                "CREATE TABLE traces (
                    trace_id TEXT PRIMARY KEY, ts TEXT NOT NULL, ts_epoch INTEGER NOT NULL,
                    key_id INTEGER NOT NULL, key_source TEXT, model TEXT NOT NULL,
                    is_stream INTEGER NOT NULL, final_status TEXT NOT NULL,
                    final_credential_id INTEGER NOT NULL, error_type TEXT, error_message TEXT,
                    total_attempts INTEGER NOT NULL, duration_ms INTEGER NOT NULL,
                    interrupted_after_bytes INTEGER,
                    input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                    credits REAL NOT NULL DEFAULT 0, first_token_ms INTEGER, effort TEXT
                 );
                 CREATE TABLE trace_attempts (
                    trace_id TEXT NOT NULL, attempt INTEGER NOT NULL, credential_id INTEGER NOT NULL,
                    endpoint TEXT NOT NULL, http_status INTEGER, outcome TEXT NOT NULL,
                    error_snippet TEXT, duration_ms INTEGER NOT NULL,
                    PRIMARY KEY (trace_id, attempt)
                 );
                 INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, is_stream,
                    final_status, final_credential_id, total_attempts, duration_ms)
                 VALUES ('legacy-1', '2026-08-01T00:00:00+00:00', 1785500000, 3, 'clientKey',
                    'claude-opus-4-6', 1, 'success', 9, 1, 4200);",
            )
            .expect("建老表并插历史行");
        }

        let store = crate::admin::trace_db::TraceStore::open(path.clone(), true, 7)
            .expect("打开老库应触发迁移");
        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1, "历史行不能丢");
        assert_eq!(rows[0].trace_id, "legacy-1");
        // 老行没有思考数据：可空列为 None，NOT NULL 列取默认 0
        assert!(rows[0].thinking_ms.is_none());
        assert!(rows[0].first_answer_ms.is_none());
        assert_eq!(rows[0].thinking_chars, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 思考观测：reasoning → assistant 的间隔落成 thinking_ms，字符数累计，
    /// 且 first_token_ms 语义不受影响（仍是首个上游 chunk，由 mark_first_token 决定）。
    #[test]
    fn observe_event_records_thinking_span_and_chars() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, tracer) = hook_with_tracer(store.clone(), true);

        // 首个上游 chunk 与首个 reasoning 是两个独立标记
        tracer.mark_first_token();
        for frag in ["推演中", "continue"] {
            tracer.observe_event(&Event::ReasoningContent(ReasoningContentEvent {
                text: Some(frag.to_string()),
                ..Default::default()
            }));
        }
        // 思考与正文之间留出可测量的间隔
        std::thread::sleep(std::time::Duration::from_millis(12));
        tracer.observe_event(&Event::AssistantResponse(
            serde_json::from_value(serde_json::json!({ "content": "answer" }))
                .expect("构造 assistantResponseEvent"),
        ));

        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        let rec = &rows[0];
        // "推演中" 3 字符 + "continue" 8 字符，按字符计不按字节
        assert_eq!(rec.thinking_chars, 11);
        let thinking_ms = rec.thinking_ms.expect("有 reasoning 和正文时应有思考耗时");
        assert!(thinking_ms >= 10, "思考耗时应覆盖两帧间隔，实际 {thinking_ms}ms");
        assert!(rec.first_answer_ms.is_some(), "应记录首个产出帧时刻");
        assert!(rec.first_token_ms.is_some(), "首 Token 语义不变，仍应有值");
    }

    /// 思考 → 工具调用（无正文）：toolUseEvent 同样是产出起点，thinking_ms 要有值。
    ///
    /// agent 类流量大量是这个形状（想一会儿直接发工具调用，整条流没有一个
    /// assistantResponseEvent）。曾经只认 assistantResponseEvent，导致思考数据
    /// 在最需要它的那批请求上恰好全部缺失。
    #[test]
    fn observe_event_treats_tool_use_as_answer_start() {
        use crate::kiro::model::events::{ReasoningContentEvent, ToolUseEvent};

        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, tracer) = hook_with_tracer(store.clone(), true);

        tracer.mark_first_token();
        tracer.observe_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("先看下文件".to_string()),
            ..Default::default()
        }));
        std::thread::sleep(std::time::Duration::from_millis(12));
        tracer.observe_event(&Event::ToolUse(ToolUseEvent {
            name: "read_file".to_string(),
            tool_use_id: "tu_1".to_string(),
            input: r#"{"path":"a.rs"}"#.to_string(),
            stop: true,
        }));

        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        let rec = &rows[0];
        assert_eq!(rec.thinking_chars, 5);
        let thinking_ms = rec
            .thinking_ms
            .expect("思考后接工具调用也应有思考耗时（工具调用即产出起点）");
        assert!(thinking_ms >= 10, "思考耗时应覆盖两帧间隔，实际 {thinking_ms}ms");
        assert!(
            rec.first_answer_ms.is_some(),
            "工具调用应置位首个产出帧时刻"
        );
    }

    /// 非流式：字符数照常累加，但不产生时长（帧全在同一瞬间到达）。
    ///
    /// 展示层靠「有字符数但没时长」把非流式和「真的没思考」区分开。
    #[test]
    fn non_stream_collects_chars_without_duration() {
        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        // is_stream = false
        let (hook, tracer) = hook_with_tracer(store.clone(), false);

        tracer.observe_reasoning_chars("非流式的思考文本");
        tracer.observe_reasoning_chars("continue");
        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        let rec = &rows[0];
        assert!(!rec.is_stream);
        // 8 字 + 8 字母
        assert_eq!(rec.thinking_chars, 16, "非流式也要累加思考字符数");
        assert!(rec.thinking_ms.is_none(), "非流式测不出时长");
        assert!(rec.first_answer_ms.is_none(), "非流式没有首帧概念");
    }

    /// 重试污染：失败跳的思考观测不能算进最终结果。
    ///
    /// 模拟第 0 跳思考后断流、第 1 跳换号重来的时序。若不在 on_attempt 里重置，
    /// thinking_ms 会把第 0 跳的思考起点当基准，把断流 + 退避 + 换号全算进思考时长。
    #[test]
    fn retry_discards_failed_hop_thinking_observation() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, tracer) = hook_with_tracer(store.clone(), true);

        // 第 0 跳：provider 拿到响应头即上报，随后流里出现思考，然后断流
        tracer.on_attempt(TraceAttempt {
            attempt: 0,
            credential_id: 11,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome::SUCCESS.to_string(),
            error_snippet: None,
            duration_ms: 30,
        });
        tracer.mark_first_token();
        tracer.observe_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("第 0 跳的思考，共十二字".to_string()),
            ..Default::default()
        }));
        // 断流 + 退避：这段时间不该被算作思考
        std::thread::sleep(std::time::Duration::from_millis(25));

        // 第 1 跳：换号重来
        tracer.on_attempt(TraceAttempt {
            attempt: 1,
            credential_id: 22,
            endpoint: "ide".to_string(),
            http_status: Some(200),
            outcome: outcome::SUCCESS.to_string(),
            error_snippet: None,
            duration_ms: 40,
        });
        tracer.observe_event(&Event::ReasoningContent(ReasoningContentEvent {
            text: Some("重来".to_string()),
            ..Default::default()
        }));
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracer.observe_event(&Event::AssistantResponse(
            serde_json::from_value(serde_json::json!({ "content": "answer" }))
                .expect("构造 assistantResponseEvent"),
        ));

        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        let rec = &rows[0];
        // 只剩第 1 跳的 "重来" 两字，第 0 跳那十二字被丢弃
        assert_eq!(rec.thinking_chars, 2, "失败跳的思考字符数不该累积");
        let thinking_ms = rec.thinking_ms.expect("最后一跳有思考和正文");
        assert!(
            thinking_ms < 25,
            "思考时长不该包含第 0 跳的断流与退避，实际 {thinking_ms}ms"
        );
        // 最终凭据取最后一跳，与思考观测同口径
        assert_eq!(rec.final_credential_id, 22);
    }

    /// 没有 reasoning 帧时 thinking_ms 为 None（而不是 0），避免把"无思考"
    /// 和"思考了 0 毫秒"混为一谈。
    #[test]
    fn observe_event_without_reasoning_leaves_thinking_none() {

        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, tracer) = hook_with_tracer(store.clone(), true);

        tracer.mark_first_token();
        tracer.observe_event(&Event::AssistantResponse(
            serde_json::from_value(serde_json::json!({ "content": "answer" }))
                .expect("构造 assistantResponseEvent"),
        ));
        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert!(rows[0].thinking_ms.is_none(), "无思考时不应有思考耗时");
        assert_eq!(rows[0].thinking_chars, 0);
        assert!(rows[0].first_answer_ms.is_some());
    }

    /// 失败时 error_type 取最后一跳的 outcome，且 attempt 要跟着落库
    /// （provider 拿 hook.trace_sink() 当 sink 上报的那些跳）。
    #[test]
    fn attached_tracer_carries_attempts_and_error_type() {
        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let (hook, tracer) = hook_with_tracer(store.clone(), true);

        // 模拟 provider 逐跳上报：第 0 跳配额耗尽，第 1 跳限流
        tracer.on_attempt(TraceAttempt {
            attempt: 0,
            credential_id: 11,
            endpoint: "ide".to_string(),
            http_status: Some(429),
            outcome: outcome::QUOTA_EXHAUSTED.to_string(),
            error_snippet: None,
            duration_ms: 5,
        });
        tracer.on_attempt(TraceAttempt {
            attempt: 1,
            credential_id: 12,
            endpoint: "ide".to_string(),
            http_status: Some(429),
            outcome: outcome::ACCOUNT_THROTTLED.to_string(),
            error_snippet: Some("slow down".to_string()),
            duration_ms: 7,
        });

        hook.record(12, 300, 0, 0, 0, 0.0, "error");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].final_status, "error");
        assert_eq!(rows[0].error_type.as_deref(), Some(outcome::ACCOUNT_THROTTLED));
        assert_eq!(rows[0].final_credential_id, 12);
        assert_eq!(rows[0].attempts.len(), 2, "逐跳 attempt 应一并落库");
    }

    /// 主链路不挂 tracer（它自己显式 finalize），否则会重复落库。
    #[test]
    fn hook_without_tracer_writes_no_trace() {
        let store = std::sync::Arc::new(
            crate::admin::trace_db::TraceStore::open_in_memory().expect("open in-memory trace db"),
        );
        let hook = UsageRecordHook {
            recorder: None,
            aggregator: None,
            client_keys: None,
            credit_token_manager: None,
            key_id: 7,
            model: "claude-opus-5".to_string(),
            effort: None,
            started_at: Instant::now(),
            tracer: None,
            count_call: true,
        };

        hook.record(42, 1000, 200, 0, 0, 1.5, "success");

        let (rows, _total) = store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert!(rows.is_empty());
    }

    /// effort 记账要覆盖两族包装键，否则 GPT-5.6 的 effort 在统计/trace 里丢成 None。
    #[test]
    fn extract_effort_reads_both_output_config_and_reasoning() {
        use crate::kiro::model::requests::kiro::{KiroOutputConfig, KiroReasoningConfig};

        // Claude 系：output_config
        let claude = AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "high".to_string(),
            }),
            reasoning: None,
        };
        assert_eq!(extract_effort(Some(&claude)).as_deref(), Some("high"));

        // GPT-5.6 系：reasoning
        let gpt = AdditionalModelRequestFields {
            output_config: None,
            reasoning: Some(KiroReasoningConfig {
                effort: "xhigh".to_string(),
            }),
        };
        assert_eq!(extract_effort(Some(&gpt)).as_deref(), Some("xhigh"));

        // 未请求 effort
        assert_eq!(extract_effort(None), None);
    }

    /// 从未显式 finalize → Drop 兜底补一条 abandoned 记录（attempts=0 / first_token=None）。
    #[test]
    fn tracer_drop_records_abandoned_when_never_finalized() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let _tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "abandoned-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: true,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                effort: None,
                first_reasoning_at: parking_lot::Mutex::new(None),
                first_answer_at: parking_lot::Mutex::new(None),
                thinking_chars: std::sync::atomic::AtomicU64::new(0),
                token_manager: None,
                session_key: None,
                affinity: parking_lot::Mutex::new(None),
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            // 不调用任何 finalize，直接离开作用域触发 Drop
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "Drop 应兜底落一行");
        let record = &records[0];
        assert_eq!(record.final_status, "interrupted");
        assert_eq!(
            record.error_type.as_deref(),
            Some(outcome::ABANDONED_BEFORE_HEADERS)
        );
        assert_eq!(record.total_attempts, 0);
        assert_eq!(record.first_token_ms, None);
        assert_eq!(record.interrupted_after_bytes, None);
    }

    /// provider 已返回 response headers 后、body 收尾前请求被取消：不能误报为等头阶段
    /// 中断。这个覆盖非流式 body 读取，以及未来忘记安装 stream settlement 的新路径。
    #[test]
    fn tracer_drop_records_stream_interrupted_after_headers_arrive() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "headers-arrived-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: true,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                effort: None,
                first_reasoning_at: parking_lot::Mutex::new(None),
                first_answer_at: parking_lot::Mutex::new(None),
                thinking_chars: std::sync::atomic::AtomicU64::new(0),
                token_manager: None,
                session_key: None,
                affinity: parking_lot::Mutex::new(None),
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            tracer.mark_upstream_headers_received();
            // 不调用 finalize，直接离开作用域模拟 headers 已到后 handler/body 被取消。
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "Drop 应兜底落一行");
        assert_eq!(records[0].final_status, "interrupted");
        assert_eq!(
            records[0].error_type.as_deref(),
            Some(outcome::STREAM_INTERRUPTED)
        );
        assert_eq!(records[0].first_token_ms, None);
    }

    /// 已显式 finalize 的请求：Drop 走幂等守卫，不得再写第二行、也不得覆盖原记录。
    #[test]
    fn tracer_drop_is_noop_when_already_finalized() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};

        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "finalized-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                model: "claude-sonnet-5".to_string(),
                is_stream: false,
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
                client_ip: None,
                effort: None,
                first_reasoning_at: parking_lot::Mutex::new(None),
                first_answer_at: parking_lot::Mutex::new(None),
                thinking_chars: std::sync::atomic::AtomicU64::new(0),
                token_manager: None,
                session_key: None,
                affinity: parking_lot::Mutex::new(None),
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            tracer.finalize("success", None, None, None, TraceUsage::zero());
            // 离开作用域触发 Drop —— 应被幂等守卫拦下
        }

        let (records, total) = store.query_paged(&TraceQuery {
            limit: 10,
            ..Default::default()
        });
        assert_eq!(total, 1, "已 finalize 后 Drop 不得再写一行");
        assert_eq!(records[0].final_status, "success");
        assert_eq!(records[0].error_type, None);
    }

    #[test]
    fn discarded_tracer_does_not_record_cancellation() {
        use crate::admin::trace_db::{TraceQuery, TraceStore};
        let store = std::sync::Arc::new(TraceStore::open_in_memory().unwrap());
        {
            let tracer = RequestTracer {
                store: Some(store.clone()),
                trace_id: "discarded-trace".to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: 0,
                key_source: TraceKeySource::MasterApiKey,
                client_ip: None,
                model: "test".to_string(),
                is_stream: false,
                started_at: Instant::now(),
                effort: None,
                first_token_at: parking_lot::Mutex::new(None),
                first_reasoning_at: parking_lot::Mutex::new(None),
                first_answer_at: parking_lot::Mutex::new(None),
                thinking_chars: std::sync::atomic::AtomicU64::new(0),
                attempts: parking_lot::Mutex::new(Vec::new()),
                token_manager: None,
                session_key: None,
                affinity: parking_lot::Mutex::new(None),
                route: parking_lot::Mutex::new(None),
                finalized: std::sync::atomic::AtomicBool::new(false),
                upstream_headers_received: std::sync::atomic::AtomicBool::new(false),
            };
            tracer.discard();
        }
        let (_, total) = store.query_paged(&TraceQuery { limit: 10, ..Default::default() });
        assert_eq!(total, 0);
    }

    #[test]
    fn account_suspended_attempt_is_preserved_as_request_error_type() {
        assert_eq!(
            canonical_attempt_outcome(outcome::ACCOUNT_SUSPENDED),
            outcome::ACCOUNT_SUSPENDED
        );
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn upstream_rate_limit_maps_to_429_with_retry_after() {
        let err = crate::kiro::error::UpstreamRateLimitError::new(Some("1800".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1800");
    }

    #[test]
    fn upstream_rate_limit_drops_invalid_retry_after() {
        let err =
            crate::kiro::error::UpstreamRateLimitError::new(Some("not-a-retry-delay".to_string()));
        let resp = map_provider_error(err.into());

        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn typed_context_overflow_maps_to_stable_400() {
        let resp = map_provider_error(crate::kiro::error::UpstreamContextOverflowError.into());
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Context window is full"));
    }

    #[tokio::test]
    async fn generic_upstream_error_does_not_expose_raw_body() {
        let secret = "aws-account=123456789012 request-id=private-request";
        let resp = map_provider_error(anyhow::anyhow!(secret));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(!body.contains(secret));
        assert!(body.contains("Upstream API request failed"));
    }

    /// 非流式混流：原生 thinking 事件 + 文本流里的 `<thinking>` 标签并存时，
    /// 文本里的思考段必须切出为独立 thinking 块，不能带标签漏进正文。
    #[test]
    fn non_stream_mixed_native_and_text_thinking_extracts_both() {
        let content = build_non_stream_content(
            true,
            "<thinking>文本思考</thinking>\n\n正文".to_string(),
            "原生思考".to_string(),
            None,
            vec![],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "原生思考");
        assert_eq!(content[1]["type"], "thinking");
        assert_eq!(content[1]["thinking"], "文本思考");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "正文");
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    #[test]
    fn non_stream_legacy_thinking_extraction_still_works_without_native_reasoning() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "legacy thinking");
        assert_eq!(
            content[0]["signature"],
            crate::anthropic::stream::THINKING_SIGNATURE_PLACEHOLDER
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_native_thinking_is_not_text_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert!(content.is_empty());
        // A client replaying these content blocks cannot reintroduce the
        // suppressed reasoning as text on the next turn.
        assert!(!serde_json::to_string(&content).unwrap().contains("native thinking"));
    }

    #[test]
    fn non_stream_disabled_thinking_keeps_only_visible_answer() {
        let content = build_non_stream_content(
            false,
            "answer".into(),
            "private reasoning".into(),
            Some("private-signature".into()),
            vec!["private-redacted".into()],
        );
        assert_eq!(content, vec![json!({"type": "text", "text": "answer"})]);
    }

    #[test]
    fn non_stream_thinking_only_preserves_type_and_signature_when_enabled() {
        let content = build_non_stream_content(
            true,
            String::new(),
            "native reasoning".into(),
            Some("real-signature".into()),
            Vec::new(),
        );
        assert_eq!(content, vec![json!({
            "type": "thinking", "thinking": "native reasoning",
            "signature": "real-signature"
        })]);
    }

    #[test]
    fn dynamic_models_do_not_synthesize_claude_thinking_alias() {
        let models = aggregate_available_models(vec![UpstreamModel {
            model_id: "claude-opus-5".to_string(),
            model_name: Some("Claude Opus 5".to_string()),
            description: None,
            token_limits: None,
        }]);
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-5"));
        assert!(!ids.contains(&"claude-opus-5-thinking"));
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn dynamic_models_merge_metadata_and_do_not_use_input_limit_as_output_limit() {
        // 显式传空自定义模型表：`custom_models` 是进程级 OnceLock，别的用例可能已装载，
        // 这里只验证上游条目的合并/去重，不应受全局注册表影响。
        let models = aggregate_available_models_with_custom(
            vec![
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: None,
                description: Some("first".to_string()),
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(200_000),
                    max_output_tokens: None,
                }),
            },
            UpstreamModel {
                model_id: "glm-5".to_string(),
                model_name: Some("GLM 5".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(32_000),
                }),
            },
            ],
            &[],
        );

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "GLM 5");
        assert_eq!(models[0].owned_by, "kiro");
        assert_eq!(models[0].context_window, 1_000_000);
        assert_eq!(models[0].max_tokens, 32_000);
    }

    #[test]
    fn custom_model_metadata_overrides_dynamic_collision() {
        let custom = crate::model::config::CustomModel {
            id: "gpt-next".to_string(),
            backend_id: "gpt-next".to_string(),
            display_name: Some("Configured GPT".to_string()),
            context_window: Some(500_000),
            max_tokens: Some(12_345),
            supports_reasoning: Some(true),
            owned_by: Some("configured-owner".to_string()),
        };
        let models = aggregate_available_models_with_custom(
            vec![UpstreamModel {
                model_id: "gpt-next".to_string(),
                model_name: Some("Upstream GPT".to_string()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(300_000),
                    max_output_tokens: Some(64_000),
                }),
            }],
            &[custom],
        );

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Configured GPT");
        assert_eq!(models[0].owned_by, "configured-owner");
        assert_eq!(models[0].context_window, 500_000);
        assert_eq!(models[0].max_tokens, 12_345);
    }

    #[test]
    fn non_stream_usage_prefers_sanitized_provider_snapshot() {
        let fallback_cache = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };
        let provider = TokenUsage {
            uncached_input_tokens: 3,
            output_tokens: 11,
            cache_read_input_tokens: 7,
            cache_write_input_tokens: 4,
        };

        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, fallback_cache, Some(provider)),
            (3, 11, 4, 7)
        );
    }

    #[test]
    fn non_stream_usage_falls_back_to_context_and_cache_split() {
        let cache_usage = super::super::cache_metering::CacheUsage {
            cache_read: 25,
            cache_covered_est: 50,
            prompt_total_est: 100,
        };

        // context 80 - output 9 = 71，再按 50/100 分摊：35 / 18 / 18。
        // output_tokens 字段本身不受扣减影响，仍是 9。
        assert_eq!(
            resolve_non_stream_usage(100, Some(80), 9, cache_usage, None),
            (35, 9, 18, 18)
        );
        let (input, _, creation, read) =
            resolve_non_stream_usage(100, Some(80), 9, cache_usage, None);
        assert_eq!(
            input + creation + read,
            71,
            "分摊必须发生在扣减之后，三份之和等于修正后的 total"
        );
        // output 必须取正数：若传 -9，错实现（对 fallback 分支也扣减）因
        // output.max(0) == 0 而照样得到 100，这条断言就失去鉴别力。
        // 取 9 后，错实现会得到 91，与期望的 100 不符，立刻暴露。
        assert_eq!(
            resolve_non_stream_usage(100, None, 9, Default::default(), None),
            (100, 9, 0, 0)
        );
        // 负输出的 clamp 另行单独覆盖（口径与上一条互不干扰）。
        assert_eq!(
            resolve_non_stream_usage(100, None, -9, Default::default(), None),
            (100, 0, 0, 0)
        );
    }

    #[test]
    fn max_tokens_must_be_positive() {
        assert!(validate_max_tokens(1).is_ok());
        assert!(validate_max_tokens(0).is_err());
        assert!(validate_max_tokens(-1).is_err());
    }

    /// 每个 SSE 响应都要带 `x-accel-buffering: no`。
    ///
    /// 走源码扫描而非构造响应：SSE 端点散落在 7 个模块且构造成本高，而风险点
    /// 恰是"新增端点时忘了带"。
    #[test]
    fn every_sse_response_disables_proxy_buffering() {
        const SSE_CONTENT_TYPE: &str = r#".header(header::CONTENT_TYPE, "text/event-stream")"#;
        const NO_BUFFERING: &str = r#".header("x-accel-buffering", "no")"#;

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        let mut missing = Vec::new();

        let mut stack = vec![root];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).expect("read src dir") {
                let entry = entry.expect("dir entry");
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&p).expect("read rs file");
                for (idx, _) in src.match_indices(SSE_CONTENT_TYPE) {
                    checked += 1;
                    // 头部块紧邻 content-type；按字符截取，避免切断中文注释的 UTF-8 编码。
                    let tail: String = src[idx..].chars().take(400).collect();
                    if !tail.contains(NO_BUFFERING) {
                        let line = src[..idx].lines().count();
                        missing.push(format!("{}:{}", p.display(), line));
                    }
                }
            }
        }

        assert!(checked > 0, "未扫描到任何 SSE 响应，断言失效");
        assert!(
            missing.is_empty(),
            "以下 SSE 响应缺少 `x-accel-buffering: no`：{missing:?}"
        );
    }

    #[test]
    fn fixresp_l11_non_stream_reclaims_trailing_invoke_like_stream() {
        let mut schemas = super::super::stream::InvokeToolSchemas::new();
        schemas.insert(
            "exec_command".to_string(),
            json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}, "yield_time_ms": {"type": "integer"}},
                "required": ["cmd"]
            }),
        );
        let text = "我来跑一下：\n<invoke name=\"exec_command\">\n<parameter name=\"cmd\">ls</parameter>\n<parameter name=\"yield_time_ms\">500</parameter>\n</invoke>";
        let mut body = json!({"type": "message", "stop_reason": "end_turn", "content": [{"type": "text", "text": text}]});
        reclaim_trailing_invoke_text(&mut body, &schemas, &std::collections::HashMap::new());
        assert_eq!(body["stop_reason"], json!("tool_use"));
        let content = body["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type": "text", "text": "我来跑一下：\n"}));
        assert_eq!(content[1]["type"], json!("tool_use"));
        assert_eq!(content[1]["input"], json!({"cmd": "ls", "yield_time_ms": 500}));

        // 末尾还有正文：是示例，不合成。
        let example = format!("{text}\n这是示例。");
        let mut body = json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": example}]});
        let before = body.clone();
        reclaim_trailing_invoke_text(&mut body, &schemas, &std::collections::HashMap::new());
        assert_eq!(body, before);
    }

    #[test]
    fn fixresp_m3_upstream_failures_keep_classified_reason() {
        let exc = |t: &str, m: &str| Event::Exception { exception_type: t.into(), message: m.into() };
        let cases = [
            (exc("ThrottlingException", "Rate exceeded for account 123456789012"), "rate_limit_error", StatusCode::TOO_MANY_REQUESTS),
            (exc("com.amazon.aws.codewhisperer#ServiceUnavailableException", "Encountered unexpectedly high load"), "overloaded_error", StatusCode::SERVICE_UNAVAILABLE),
            (Event::Error { error_code: "ValidationException".into(), error_message: "Input is too long for requested model.".into() }, "invalid_request_error", StatusCode::BAD_REQUEST),
            (exc("AccessDeniedException", "User is not authorized for 123456789012"), "permission_error", StatusCode::FORBIDDEN),
            (exc("InternalServerException", "secret-request-id abc"), "api_error", StatusCode::BAD_GATEWAY),
        ];
        for (event, ty, status) in cases {
            let (detail, failure) = upstream_event_error(&event).expect("error event");
            assert_eq!(failure.error_type, ty, "{detail}");
            assert_eq!(failure.status(), status, "{detail}");
            for secret in ["123456789012", "secret-request-id", "com.amazon"] {
                assert!(!failure.message.contains(secret), "{}", failure.message);
            }
        }
        let (_, too_long) = upstream_event_error(&Event::Error {
            error_code: "ValidationException".into(),
            error_message: "Input is too long for requested model.".into(),
        })
        .unwrap();
        assert!(too_long.message.starts_with("prompt is too long"), "{}", too_long.message);
        assert!(upstream_event_error(&exc("ContentLengthExceededException", "x")).is_none());
    }

    #[tokio::test]
    async fn map_provider_error_returns_401_on_no_available_credentials() {
        use axum::body::to_bytes;

        let err = anyhow::Error::new(NoAvailableCredentialsError::new("所有凭据均已禁用（0/2）"));
        let resp = map_provider_error(err);
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["type"], "authentication_error");
        assert_eq!(
            json["error"]["message"],
            "No available credentials for this request"
        );
    }

    #[test]
    fn attach_trace_headers_sets_expected_headers() {
        let mut resp = Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap();
        let trace_id = "0fc99995-b8cc-46b9-bf25-c44d0e3b1447";
        attach_trace_headers(&mut resp, trace_id);

        assert_eq!(resp.headers().get("x-oneapi-request-id").unwrap(), trace_id);
        assert_eq!(resp.headers().get("x-request-id").unwrap(), trace_id);
    }
}

/// 本地测试单独成块，不插进上游 `mod tests` 中间（见 CLAUDE.md 第二条）。
///
/// 覆盖 `-thinking` 后缀模型的 thinking 模式覆写：判定改为基于 backend_id 后，
/// Opus 4.7+ / 5 代模型应走 adaptive，而非旧逻辑漏判成 enabled。
#[cfg(test)]
mod thinking_override_tests {
    use super::*;

    fn request_with_model(model: &str) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("构造 MessagesRequest 失败")
    }

    #[test]
    fn adaptive_only_models_get_adaptive_with_effort() {
        // 旧逻辑只认 opus 4.6 / sonnet-5，这批会被漏判成 enabled。
        for model in [
            // 4.6 两款按 Kiro per-model 表只列 adaptive/disabled；opus 4.6 另有
            // Kiro 侧约束（只在 adaptive 下接受 output_config）。
            "claude-opus-4-6-thinking",
            "claude-sonnet-4-6-thinking",
            "claude-opus-4-7-thinking",
            "claude-opus-4-8-thinking",
            "claude-opus-5-thinking",
            "claude-sonnet-5-thinking",
            "claude-fable-5-thinking",
        ] {
            let mut payload = request_with_model(model);
            override_thinking_from_model_name(&mut payload);

            let thinking = payload.thinking.as_ref().expect("应写入 thinking");
            assert_eq!(
                thinking.thinking_type, "adaptive",
                "{model} 应覆写为 adaptive"
            );
            assert_eq!(
                payload.output_config.as_ref().map(|oc| oc.effort.as_str()),
                Some("high"),
                "{model} 应附带 effort=high"
            );
        }
    }

    #[test]
    fn fixed_budget_models_stay_enabled() {
        // 4.5 及更早只支持固定预算，维持 enabled + budget_tokens。
        for model in ["claude-sonnet-4-5-thinking", "claude-opus-4-5-thinking"] {
            let mut payload = request_with_model(model);
            override_thinking_from_model_name(&mut payload);

            let thinking = payload.thinking.as_ref().expect("应写入 thinking");
            assert_eq!(thinking.thinking_type, "enabled", "{model} 应保持 enabled");
            assert_eq!(thinking.budget_tokens, 20000);
            assert!(
                payload.output_config.is_none(),
                "{model} 不应附带 output_config"
            );
        }
    }

    #[test]
    fn models_without_thinking_suffix_are_untouched() {
        let mut payload = request_with_model("claude-opus-4-7");
        override_thinking_from_model_name(&mut payload);
        assert!(payload.thinking.is_none(), "无 thinking 后缀不应覆写");
        assert!(payload.output_config.is_none());
    }
}

#[cfg(test)]
mod credit_stats_tests;
