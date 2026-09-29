//! conversationId 来源统计
//!
//! 发往 Kiro 上游的 `conversationState.conversationId` 优先取 `metadata.user_id`
//! 里的 session UUID，取不到时每次随机生成（见 `converter.rs`）。随机 ID 会让上游
//! 把每一轮都当成新对话，可能影响上游缓存 / 风控，所以这里统计各来源占比，
//! 每 10 分钟打一行 INFO 汇总，用于判断无 session 流量的规模。
//!
//! 只在进入上游对话的请求入口处计一次（纯 web_search 走本地 MCP，不计；
//! web_search agentic loop 的多轮只计首轮），避免多轮循环放大计数。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use super::converter::extract_session_id;
use super::types::MessagesRequest;

/// 汇总日志间隔
const REPORT_INTERVAL: Duration = Duration::from_secs(600);
/// 脱敏样例的最大长度
const SAMPLE_MAX_LEN: usize = 80;

/// conversationId 的来源分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConversationIdSource {
    /// user_id 是 JSON，取到 `session_id`（新版 Claude Code）
    SessionJson,
    /// user_id 是 `session_<uuid>`（OpenAI / Responses 路径合成）
    SessionOpenAi,
    /// user_id 是 `..._session_<uuid>` 字符串（旧版 Claude Code 等）
    SessionString,
    /// 有 user_id 但解析不出 session → 随机 ID
    Unparsed,
    /// 没有 metadata / user_id → 随机 ID
    Missing,
}

impl ConversationIdSource {
    const ALL: [Self; 5] = [
        Self::SessionJson,
        Self::SessionOpenAi,
        Self::SessionString,
        Self::Unparsed,
        Self::Missing,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Self::SessionJson => "session_json",
            Self::SessionOpenAi => "session_openai",
            Self::SessionString => "session_string",
            Self::Unparsed => "random_unparsed",
            Self::Missing => "random_missing",
        }
    }

    fn is_random(self) -> bool {
        matches!(self, Self::Unparsed | Self::Missing)
    }
}

/// 对请求分类（口径与 `converter` 生成 conversationId 的逻辑一致）
pub(crate) fn classify(req: &MessagesRequest) -> ConversationIdSource {
    let Some(user_id) = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.as_deref())
        .filter(|s| !s.is_empty())
    else {
        return ConversationIdSource::Missing;
    };

    if extract_session_id(user_id).is_none() {
        return ConversationIdSource::Unparsed;
    }
    if user_id.trim_start().starts_with('{') {
        ConversationIdSource::SessionJson
    } else if user_id.starts_with("session_") {
        ConversationIdSource::SessionOpenAi
    } else {
        ConversationIdSource::SessionString
    }
}

struct Stats {
    /// 本窗口计数（每次汇总后清零）
    window: [AtomicU64; 5],
    /// 进程启动以来累计
    total: [AtomicU64; 5],
    /// 本窗口首个解析失败 user_id 的脱敏样例
    unparsed_sample: Mutex<Option<String>>,
}

static STATS: Stats = Stats {
    window: [const { AtomicU64::new(0) }; 5],
    total: [const { AtomicU64::new(0) }; 5],
    unparsed_sample: Mutex::new(None),
};

/// 记录一次请求的 conversationId 来源
pub(crate) fn record(req: &MessagesRequest) {
    let source = classify(req);
    STATS.window[source.index()].fetch_add(1, Ordering::Relaxed);
    STATS.total[source.index()].fetch_add(1, Ordering::Relaxed);

    if source == ConversationIdSource::Unparsed {
        let mut sample = STATS.unparsed_sample.lock();
        if sample.is_none()
            && let Some(user_id) = req.metadata.as_ref().and_then(|m| m.user_id.as_deref())
        {
            *sample = Some(redact_shape(user_id));
        }
    }
    tracing::debug!(source = source.label(), "conversationId 来源");
}

/// 把 user_id 脱敏成「形状」：字母→a、数字→9，保留标点，截断到固定长度。
/// 只用于判断格式，不暴露设备 ID / session 内容。
fn redact_shape(user_id: &str) -> String {
    let mut out: String = user_id
        .chars()
        .take(SAMPLE_MAX_LEN)
        .map(|c| {
            if c.is_ascii_alphabetic() {
                'a'
            } else if c.is_ascii_digit() {
                '9'
            } else {
                c
            }
        })
        .collect();
    let len = user_id.chars().count();
    if len > SAMPLE_MAX_LEN {
        out.push_str(&format!("…(len={len})"));
    }
    out
}

fn format_counts(counts: &[u64; 5]) -> String {
    let sum: u64 = counts.iter().sum();
    let random: u64 = ConversationIdSource::ALL
        .iter()
        .filter(|s| s.is_random())
        .map(|s| counts[s.index()])
        .sum();
    let pct = |n: u64| if sum == 0 { 0.0 } else { n as f64 * 100.0 / sum as f64 };
    let parts: Vec<String> = ConversationIdSource::ALL
        .iter()
        .map(|s| format!("{}={}", s.label(), counts[s.index()]))
        .collect();
    format!("total={sum} random={random} ({:.1}%) {}", pct(random), parts.join(" "))
}

/// 输出一次汇总；本窗口无请求时不打日志
fn report() {
    let window: [u64; 5] = std::array::from_fn(|i| STATS.window[i].swap(0, Ordering::Relaxed));
    if window.iter().all(|&n| n == 0) {
        return;
    }
    let total: [u64; 5] = std::array::from_fn(|i| STATS.total[i].load(Ordering::Relaxed));
    let sample = STATS.unparsed_sample.lock().take();
    tracing::info!(
        unparsed_sample = sample.as_deref().unwrap_or("-"),
        "conversationId 来源（近 10 分钟）: {} | 累计: {}",
        format_counts(&window),
        format_counts(&total),
    );
}

/// 启动后台汇总任务
pub fn spawn_reporter() {
    tokio::spawn(async {
        loop {
            tokio::time::sleep(REPORT_INTERVAL).await;
            report();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::Metadata;

    fn req_with_user_id(user_id: Option<&str>) -> MessagesRequest {
        let mut req: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-sonnet-5",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        req.metadata = user_id.map(|u| Metadata {
            user_id: Some(u.to_string()),
        });
        req
    }

    const UUID: &str = "8bb5523b-ec7c-4540-a9ca-beb6d79f1552";

    #[test]
    fn classify_covers_all_sources() {
        let json = format!(r#"{{"device_id":"abc","account_uuid":"","session_id":"{UUID}"}}"#);
        assert_eq!(classify(&req_with_user_id(Some(&json))), ConversationIdSource::SessionJson);

        let openai = format!("session_{UUID}");
        assert_eq!(classify(&req_with_user_id(Some(&openai))), ConversationIdSource::SessionOpenAi);

        let legacy = format!("user_abc_account__session_{UUID}");
        assert_eq!(classify(&req_with_user_id(Some(&legacy))), ConversationIdSource::SessionString);

        assert_eq!(classify(&req_with_user_id(Some("some-user"))), ConversationIdSource::Unparsed);
        // JSON 但 session_id 不是 UUID：converter 会回退随机 ID
        assert_eq!(
            classify(&req_with_user_id(Some(r#"{"session_id":"short"}"#))),
            ConversationIdSource::Unparsed
        );

        assert_eq!(classify(&req_with_user_id(None)), ConversationIdSource::Missing);
        assert_eq!(classify(&req_with_user_id(Some(""))), ConversationIdSource::Missing);
    }

    #[test]
    fn redact_shape_hides_content() {
        assert_eq!(redact_shape("user_Ab1-x"), "aaaa_aa9-a");
        let long = "x".repeat(100);
        assert!(redact_shape(&long).ends_with("…(len=100)"));
    }

    #[test]
    fn format_counts_computes_random_share() {
        let s = format_counts(&[6, 1, 1, 1, 1]);
        assert!(s.starts_with("total=10 random=2 (20.0%)"), "{s}");
    }
}
