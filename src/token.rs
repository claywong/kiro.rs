//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入）

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, Message, SystemMessage, Tool,
};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

/// Count Tokens API 配置
#[derive(Clone, Default)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
}

/// 全局配置存储
static COUNT_TOKENS_CONFIG: OnceLock<CountTokensConfig> = OnceLock::new();

/// 初始化 count_tokens 配置
///
/// 应在应用启动时调用一次
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_CONFIG.set(config);
}

/// 获取配置
fn get_config() -> Option<&'static CountTokensConfig> {
    COUNT_TOKENS_CONFIG.get()
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符：每个计 4.5 个字符单位
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    let acc_token = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    } as u64;

    // println!("tokens: {}, acc_tokens: {}", tokens, acc_token);
    acc_token
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) fn count_all_tokens(
    model: String,
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    // 检查是否配置了远程 API
    if let Some(config) = get_config() {
        if let Some(api_url) = &config.api_url {
            // 尝试调用远程 API
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(call_remote_count_tokens(
                    api_url, config, model, &system, &messages, &tools,
                ))
            });

            match result {
                Ok(tokens) => {
                    tracing::debug!("远程 count_tokens API 返回: {}", tokens);
                    return tokens;
                }
                Err(e) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {}", e);
                }
            }
        }
    }

    // 本地计算
    count_all_tokens_local(system, messages, tools)
}

/// 调用远程 count_tokens API
async fn call_remote_count_tokens(
    api_url: &str,
    config: &CountTokensConfig,
    model: String,
    system: &Option<Vec<SystemMessage>>,
    messages: &Vec<Message>,
    tools: &Option<Vec<Tool>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    let client = build_client(config.proxy.as_ref(), 300, config.tls_backend)?;

    // 构建请求体
    let request = CountTokensRequest {
        model: model, // 模型名称用于 token 计算
        messages: messages.clone(),
        system: system.clone(),
        tools: tools.clone(),
    };

    // 构建请求
    let mut req_builder = client.post(api_url);

    // 设置认证头
    if let Some(api_key) = &config.api_key {
        if config.auth_type == "bearer" {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", api_key));
        } else {
            req_builder = req_builder.header("x-api-key", api_key);
        }
    }

    // 发送请求
    let response = req_builder
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!("API 返回错误状态: {}", response.status()).into());
    }

    let result: CountTokensResponse = response.json().await?;
    Ok(result.input_tokens as u64)
}

/// 统计单个 content 块的 token。
///
/// Anthropic 消息 content 数组里除 `text` 块外，长会话/编码场景的大头是
/// `tool_use`（参数在 `input`）和 `tool_result`（返回在 `content`，常是整段文件）。
/// 旧实现只数 `text`，把这两类漏掉，导致 tool 密集请求估值虚低（够不到速刷号
/// token 门槛）。这里按块类型分别取对应字段计入，逼近真实上下文量级。
fn count_content_block(item: &serde_json::Value) -> u64 {
    let block_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match block_type {
        // 文本块：直接数 text。
        "text" => item
            .get("text")
            .and_then(|v| v.as_str())
            .map(count_tokens)
            .unwrap_or(0),
        // tool_use：参数 JSON（input）+ 工具名。
        "tool_use" => {
            let mut n = item
                .get("name")
                .and_then(|v| v.as_str())
                .map(count_tokens)
                .unwrap_or(0);
            if let Some(input) = item.get("input") {
                n += count_json_value(input);
            }
            n
        }
        // tool_result：返回内容（content 可能是字符串或块数组）。
        "tool_result" => item.get("content").map(count_json_value).unwrap_or(0),
        // 其它块（image 等无文本、或未知类型）：回退到通用 JSON 计数，
        // 至少把可能存在的 text 字段数进去，避免再次漏计。
        _ => {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                count_tokens(text)
            } else {
                0
            }
        }
    }
}

/// 递归统计任意 JSON 值里的文本量（字符串字面量 + 结构键名近似）。
///
/// 对 `tool_use.input` / `tool_result.content` 这类嵌套结构，按其序列化字符数
/// 估算——与上游把整个结构编码进请求体的实际发送量口径一致。
fn count_json_value(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::String(s) => count_tokens(s),
        serde_json::Value::Array(arr) => {
            // 数组常见于 tool_result.content = [{type:text,text:...}, ...]，
            // 逐块复用 content 块计数；非块结构则退回整体序列化。
            let mut n = 0;
            for item in arr {
                if item.is_object() && item.get("type").is_some() {
                    n += count_content_block(item);
                } else {
                    n += count_tokens(&item.to_string());
                }
            }
            n
        }
        serde_json::Value::Object(_) => count_tokens(&value.to_string()),
        // 数字 / 布尔 / null：贡献极小，按其字面量长度粗算即可。
        other => count_tokens(&other.to_string()),
    }
}

/// 本地计算请求的输入 tokens
fn count_all_tokens_local(
    system: Option<Vec<SystemMessage>>,
    messages: Vec<Message>,
    tools: Option<Vec<Tool>>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(ref system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 用户 / 助手消息
    for msg in &messages {
        if let serde_json::Value::String(s) = &msg.content {
            total += count_tokens(s);
        } else if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                total += count_content_block(item);
            }
        }
    }

    // 工具定义
    if let Some(ref tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total.max(1)
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if let Some(thinking) = block.get("thinking").and_then(|v| v.as_str()) {
            total += count_tokens(thinking) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("redacted_thinking") {
            total += 8;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimate_output_tokens_counts_thinking_blocks() {
        let with_thinking = estimate_output_tokens(&[json!({
            "type": "thinking",
            "thinking": "需要计入输出 token"
        })]);
        let text_only = estimate_output_tokens(&[json!({
            "type": "text",
            "text": ""
        })]);

        assert!(with_thinking > text_only);
    }

    #[test]
    fn estimate_output_tokens_counts_redacted_thinking() {
        let tokens = estimate_output_tokens(&[json!({
            "type": "redacted_thinking",
            "data": "encrypted"
        })]);

        assert!(tokens >= 8);
    }

    #[test]
    fn count_content_block_counts_tool_use_input() {
        // tool_use 的参数在 input 里，旧实现完全漏数。
        let block = json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "write_file",
            "input": {"path": "/a/b.rs", "content": "fn main() { println!(\"hello world\"); }"}
        });
        assert!(count_content_block(&block) > 0);
    }

    #[test]
    fn count_content_block_counts_tool_result_string_and_array() {
        let as_string = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": "这是一整段被当作工具返回的文件内容 ".repeat(50)
        });
        let as_array = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": [{"type": "text", "text": "同样长度的返回内容 ".repeat(50)}]
        });
        assert!(count_content_block(&as_string) > 0);
        assert!(count_content_block(&as_array) > 0);
    }

    #[test]
    fn tool_heavy_message_beats_text_only_estimate() {
        // 回归：tool 密集消息的估算应显著高于只数 text 块的旧口径。
        let long = "x".repeat(4000);
        let messages = vec![Message {
            role: "user".to_string(),
            content: json!([
                {"type": "text", "text": "run it"},
                {"type": "tool_result", "tool_use_id": "t1", "content": long}
            ]),
        }];
        let total = count_all_tokens_local(None, messages, None);
        // 旧实现只会数到 "run it"（≈个位数 token）；补全后应远大于它。
        assert!(total > 500, "tool_result 内容未被计入: total={}", total);
    }
}
