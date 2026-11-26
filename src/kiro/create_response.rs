//! CreateResponse 协议适配（`runtime.<region>.kiro.dev/v1/responses`）
//!
//! 官方 Kiro IDE 1.4 起主聊天改走 CreateResponse，`generateAssistantResponse` 只留给后台
//! 辅助调用，上游对后者单独限流（429 `USER_REQUEST_RATE_EXCEEDED`），与账号额度无关。
//!
//! 本模块做三件事，且都不碰现有的消费链路：
//! 1. [`convert_request`]：把 Kiro `conversationState` 请求体改写成 Responses 请求体；
//! 2. [`url_for_region`] / [`headers`]：URL 与请求头；
//! 3. [`transcode_response`]：把上游 SSE 转写成 AWS event-stream 帧，下游继续用
//!    `EventStreamDecoder`，无需感知第二种协议。
//!
//! 协议细节参考 decolua/9router 的 `kiroCreateResponse.js`，事件格式已用真实凭据核对。

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::kiro::kiro_version;
use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// CreateResponse 只在这些 region 提供服务；region 会拼进接收 Bearer token 的主机名，
/// 所以只放行白名单，其余一律回落 us-east-1。
const RUNTIME_REGIONS: [&str; 2] = ["us-east-1", "eu-central-1"];

/// 触发回退的上游限流原因码。
const RATE_EXCEEDED_REASON: &str = "USER_REQUEST_RATE_EXCEEDED";

/// 429 响应体是否为 CreateResponse 回退要处理的「用户级限流」。
pub fn is_user_rate_exceeded(status: u16, body: &str) -> bool {
    status == 429 && body.contains(RATE_EXCEEDED_REASON)
}

pub fn url_for_region(region: &str) -> String {
    let region = if RUNTIME_REGIONS.contains(&region) {
        region
    } else {
        "us-east-1"
    };
    format!("https://runtime.{region}.kiro.dev/v1/responses")
}

/// 构造请求头（名称，值）。token 同时放在 `Authorization` 与 `x-amz-sso-bearer`。
pub fn headers(
    credentials: &KiroCredentials,
    token: &str,
    config: &Config,
) -> Vec<(&'static str, String)> {
    let version = kiro_version::effective_ide(&config.kiro_version);
    let mut out = vec![
        ("content-type", "application/json".to_string()),
        ("authorization", format!("Bearer {token}")),
        ("x-amzn-kiro-origin", "AI_EDITOR".to_string()),
        ("x-amzn-codewhisperer-optout", "true".to_string()),
        ("user-agent", format!("KiroIDE-{version}-KiroIDE/acp-client")),
        ("x-amzn-kiro-attempt", "1;max=3".to_string()),
        ("x-amz-sso-bearer", token.to_string()),
    ];
    if let Some(arn) = credentials.streaming_profile_arn() {
        out.push(("x-amzn-kiro-profile", arn));
    }
    if let Some(token_type) = credentials.token_type_header() {
        out.push(("tokentype", token_type.to_string()));
    }
    out
}

/// tool_result 的 `content` 是 `[{text}]` 或 `[{json}]`，拼成一段文本。
fn tool_result_text(result: &Value) -> String {
    let parts = result.get("content").and_then(Value::as_array);
    parts
        .map(|parts| {
            parts
                .iter()
                .map(|part| match (part.get("text"), part.get("json")) {
                    (Some(Value::String(text)), _) => text.clone(),
                    (_, Some(json)) => json.to_string(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn convert_user_turn(user: &Value, input: &mut Vec<Value>) {
    let context = user.get("userInputMessageContext");
    let results = context
        .and_then(|c| c.get("toolResults"))
        .and_then(Value::as_array);
    for result in results.into_iter().flatten() {
        input.push(json!({
            "type": "function_call_output",
            "call_id": result.get("toolUseId").cloned().unwrap_or(Value::Null),
            "output": tool_result_text(result),
        }));
    }
    let images: Vec<Value> = user
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|image| {
            let bytes = image.pointer("/source/bytes")?.as_str()?;
            let format = image.get("format").and_then(Value::as_str).unwrap_or("png");
            Some(json!({
                "type": "input_image",
                "image_url": format!("data:image/{};base64,{bytes}", format.to_ascii_lowercase()),
            }))
        })
        .collect();
    let text = user.get("content").and_then(Value::as_str).unwrap_or("");
    if !images.is_empty() {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type": "input_text", "text": text}));
        }
        content.extend(images);
        input.push(json!({"role": "user", "content": content}));
    } else if !text.is_empty() {
        input.push(json!({"role": "user", "content": text}));
    }
}

fn convert_assistant_turn(assistant: &Value, input: &mut Vec<Value>) {
    if let Some(text) = assistant.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            input.push(json!({"role": "assistant", "content": text}));
        }
    }
    let uses = assistant.get("toolUses").and_then(Value::as_array);
    for call in uses.into_iter().flatten() {
        let arguments = match call.get("input") {
            Some(Value::String(raw)) => raw.clone(),
            Some(other) => other.to_string(),
            None => "{}".to_string(),
        };
        input.push(json!({
            "type": "function_call",
            "call_id": call.get("toolUseId").cloned().unwrap_or(Value::Null),
            "name": call.get("name").cloned().unwrap_or(Value::Null),
            "arguments": arguments,
        }));
    }
}

/// 把 Kiro `generateAssistantResponse` 请求体改写成 CreateResponse 请求体。
///
/// `profile_arn` 优先于请求体里自带的值（端点层会按凭据类型决定要不要带）。
pub fn convert_request(kiro_body: &str, profile_arn: Option<&str>) -> anyhow::Result<String> {
    let root: Value = serde_json::from_str(kiro_body)?;
    let state = root
        .get("conversationState")
        .ok_or_else(|| anyhow::anyhow!("请求体缺少 conversationState"))?;
    let current = state.get("currentMessage");

    let history = state.get("history").and_then(Value::as_array);
    let turns = history.into_iter().flatten().chain(current);
    let mut input = Vec::new();
    for turn in turns {
        if let Some(user) = turn.get("userInputMessage") {
            convert_user_turn(user, &mut input);
        }
        if let Some(assistant) = turn.get("assistantResponseMessage") {
            convert_assistant_turn(assistant, &mut input);
        }
    }

    let user = current.and_then(|c| c.get("userInputMessage"));
    let model = user
        .and_then(|u| u.get("modelId"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("请求体缺少 modelId"))?;
    let origin = user
        .and_then(|u| u.get("origin"))
        .and_then(Value::as_str)
        .unwrap_or("AI_EDITOR");

    let specs = user
        .and_then(|u| u.pointer("/userInputMessageContext/tools"))
        .and_then(Value::as_array);
    let tools: Vec<Value> = specs
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            let spec = tool.get("toolSpecification")?;
            Some(json!({
                "type": "function",
                "name": spec.get("name")?,
                "description": spec.get("description").and_then(Value::as_str).unwrap_or(""),
                "parameters": spec.pointer("/inputSchema/json").cloned().unwrap_or_else(|| json!({})),
            }))
        })
        .collect();

    let mut metadata = json!({"kiro:agent_mode": "vibe"});
    if let Some(id) = state.get("conversationId").and_then(Value::as_str) {
        metadata["kiro:conversation_id"] = json!(id);
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        "origin": origin,
        "metadata": metadata,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    let arn = profile_arn.or_else(|| root.get("profileArn").and_then(Value::as_str));
    if let Some(arn) = arn {
        body["profileArn"] = json!(arn);
    }
    let fields = root.get("additionalModelRequestFields");
    let effort = fields
        .and_then(|f| f.pointer("/output_config/effort").or_else(|| f.pointer("/reasoning/effort")))
        .and_then(Value::as_str);
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }
    Ok(serde_json::to_string(&body)?)
}

use crate::kiro::parser::encoder::{encode_event, encode_exception};
use std::collections::{HashMap, HashSet};

/// SSE → event-stream 的增量转写器。只在内存里保留一行未完成的数据和工具调用索引。
#[derive(Default)]
struct Transcoder {
    pending: Vec<u8>,
    /// item id（`fc_…`）→（call_id，工具名）。参数增量事件只带 item id。
    calls: HashMap<String, (String, String)>,
    /// 已经流式输出过参数的 item id，用来决定 done 事件里要不要补发完整参数。
    streamed: HashSet<String>,
    /// 上游拒答说明。旧接口把它放在 metadataEvent.stopDetails 里而不是正文，这里同样
    /// 先缓存，等 `response.incomplete(content_filter)` 到来时再一并发出。
    refusal: String,
}

impl Transcoder {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Value>(data) {
                self.handle(&event, &mut out);
            }
        }
        out
    }

    fn tool_event(&self, item_id: &str, input: &str, stop: bool, out: &mut Vec<u8>) {
        if let Some((call_id, name)) = self.calls.get(item_id) {
            out.extend(encode_event(
                "toolUseEvent",
                &json!({"name": name, "toolUseId": call_id, "input": input, "stop": stop}),
            ));
        }
    }

    fn handle(&mut self, event: &Value, out: &mut Vec<u8>) {
        let delta = event.get("delta").and_then(Value::as_str);
        let item = event.get("item");
        let item_id = |v: Option<&Value>| {
            v.and_then(|v| v.get("id").or_else(|| v.get("item_id")))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(text) = delta.filter(|t| !t.is_empty()) {
                    out.extend(encode_event("assistantResponseEvent", &json!({"content": text})));
                }
            }
            "response.refusal.delta" => {
                if let Some(text) = delta {
                    self.refusal.push_str(text);
                }
            }
            "response.refusal.done" => {
                // done 带完整文本，以它为准（delta 可能在中途断开）。
                if let Some(text) = event.get("refusal").and_then(Value::as_str) {
                    self.refusal = text.to_string();
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                if let Some(text) = delta.filter(|t| !t.is_empty()) {
                    out.extend(encode_event("reasoningContentEvent", &json!({"text": text})));
                }
            }
            "response.output_item.added"
                if item.and_then(|i| i.get("type")).and_then(Value::as_str) == Some("function_call") =>
            {
                let id = item_id(item);
                let call_id = item.and_then(|i| i.get("call_id")).and_then(Value::as_str);
                let name = item.and_then(|i| i.get("name")).and_then(Value::as_str);
                self.calls.insert(
                    id.clone(),
                    (call_id.unwrap_or(&id).to_string(), name.unwrap_or("").to_string()),
                );
                self.tool_event(&id, "", false, out);
            }
            "response.function_call_arguments.delta" => {
                let id = item_id(Some(event));
                if let Some(args) = delta.filter(|a| !a.is_empty()) {
                    self.streamed.insert(id.clone());
                    self.tool_event(&id, args, false, out);
                }
            }
            "response.output_item.done" => self.handle_item_done(item, out),
            "response.completed" | "response.incomplete" => self.handle_finished(event, out),
            "response.failed" | "error" => {
                let error = event
                    .get("error")
                    .or_else(|| event.pointer("/response/error"))
                    .cloned()
                    .unwrap_or(Value::Null);
                out.extend(encode_exception("CreateResponseFailed", &error.to_string()));
            }
            _ => {}
        }
    }

    fn handle_item_done(&mut self, item: Option<&Value>, out: &mut Vec<u8>) {
        let Some(item) = item else { return };
        let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                // 参数一次都没流式输出过时，用 done 里的完整参数补发。
                if !self.streamed.remove(&id) {
                    if let Some(args) = item.get("arguments").and_then(Value::as_str) {
                        self.tool_event(&id, args, false, out);
                    }
                }
                self.tool_event(&id, "", true, out);
                self.calls.remove(&id);
            }
            Some("reasoning") => {
                // 思考签名藏在 encrypted_content（一段 JSON 字符串）里。
                let signature = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|v| v.pointer("/reasoningText/signature")?.as_str().map(str::to_owned));
                if let Some(signature) = signature {
                    out.extend(encode_event("reasoningContentEvent", &json!({"signature": signature})));
                }
            }
            _ => {}
        }
    }

    fn handle_finished(&mut self, event: &Value, out: &mut Vec<u8>) {
        let response = event.get("response");
        let usage = response.and_then(|r| r.get("usage"));
        if let Some(credits) = usage.and_then(|u| u.get("kiro:credits")) {
            out.extend(encode_event(
                "meteringEvent",
                &json!({
                    "unit": credits.get("unit").and_then(Value::as_str).unwrap_or("credit"),
                    "unitPlural": credits.get("unit_plural").and_then(Value::as_str).unwrap_or("credits"),
                    "usage": credits.get("value").and_then(Value::as_f64).unwrap_or(0.0),
                }),
            ));
        }
        let percentage = usage
            .and_then(|u| u.get("kiro:context_usage_percentage"))
            .and_then(Value::as_f64);
        if let Some(percentage) = percentage {
            out.extend(encode_event("contextUsageEvent", &json!({"contextUsagePercentage": percentage})));
        }
        let reason = response
            .and_then(|r| r.pointer("/incomplete_details/reason"))
            .and_then(Value::as_str);
        match reason {
            Some("max_output_tokens") => {
                out.extend(encode_event("metadataEvent", &json!({"stopReason": "MAX_TOKENS"})));
            }
            Some("content_filter") => {
                // 与旧接口一致：stopReason=CONTENT_FILTERED，说明放进 stopDetails.refusal，
                // 下游据此把 stop_reason 设为 refusal 并丢弃未完成的工具调用。
                let explanation = std::mem::take(&mut self.refusal);
                let refusal = if explanation.is_empty() {
                    json!({})
                } else {
                    json!({"explanation": explanation})
                };
                out.extend(encode_event(
                    "metadataEvent",
                    &json!({"stopReason": "CONTENT_FILTERED", "stopDetails": {"refusal": refusal}}),
                ));
            }
            _ => {}
        }
    }
}

/// 把 CreateResponse 的 SSE 响应转成 event-stream 响应，状态码和响应头保持不变。
pub fn transcode_response(response: reqwest::Response) -> reqwest::Response {
    let status = response.status();
    let mut transcoder = Transcoder::default();
    let frames = response.bytes_stream().map(move |chunk| {
        chunk.map(|bytes| Bytes::from(transcoder.feed(&bytes)))
    });
    let body = reqwest::Body::wrap_stream(frames);
    let rebuilt = http::Response::builder()
        .status(status)
        .header("content-type", "application/vnd.amazon.eventstream")
        .body(body)
        .expect("固定的状态码与响应头必然合法");
    reqwest::Response::from(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::events::Event;
    use crate::kiro::parser::decoder::EventStreamDecoder;

    fn decode(bytes: &[u8]) -> Vec<Event> {
        let mut decoder = EventStreamDecoder::new();
        decoder.feed(bytes).unwrap();
        decoder
            .decode_iter()
            .map(|frame| Event::from_frame(frame.unwrap()).unwrap())
            .collect()
    }

    fn sse(events: &[Value]) -> Vec<u8> {
        let mut out = String::new();
        for event in events {
            out.push_str(&format!("event: x\ndata: {event}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        out.into_bytes()
    }

    #[test]
    fn rate_exceeded_requires_429_and_reason() {
        let body = r#"{"message":"Too many requests","reason":"USER_REQUEST_RATE_EXCEEDED"}"#;
        assert!(is_user_rate_exceeded(429, body));
        assert!(!is_user_rate_exceeded(429, r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#));
        assert!(!is_user_rate_exceeded(400, body));
    }

    #[test]
    fn url_only_accepts_known_regions() {
        assert_eq!(url_for_region("eu-central-1"), "https://runtime.eu-central-1.kiro.dev/v1/responses");
        assert_eq!(url_for_region("evil.com/x"), "https://runtime.us-east-1.kiro.dev/v1/responses");
    }

    #[test]
    fn converts_history_tools_images_and_effort() {
        let body = json!({
            "conversationState": {
                "conversationId": "c-1",
                "history": [
                    {"userInputMessage": {"content": "hi", "modelId": "m", "origin": "AI_EDITOR"}},
                    {"assistantResponseMessage": {"content": "calling", "toolUses": [
                        {"toolUseId": "t1", "name": "get_weather", "input": {"city": "Paris"}}]}}
                ],
                "currentMessage": {"userInputMessage": {
                    "content": "next", "modelId": "claude-sonnet-4.6", "origin": "AI_EDITOR",
                    "images": [{"format": "PNG", "source": {"bytes": "QUJD"}}],
                    "userInputMessageContext": {
                        "toolResults": [{"toolUseId": "t1", "content": [{"text": "sunny"}], "status": "success"}],
                        "tools": [{"toolSpecification": {"name": "get_weather", "description": "d",
                            "inputSchema": {"json": {"type": "object"}}}}]
                    }
                }}
            },
            "profileArn": "arn:body",
            "additionalModelRequestFields": {"output_config": {"effort": "high"}}
        });
        let out: Value =
            serde_json::from_str(&convert_request(&body.to_string(), Some("arn:cred")).unwrap()).unwrap();
        assert_eq!(out["model"], "claude-sonnet-4.6");
        assert_eq!(out["stream"], true);
        assert_eq!(out["profileArn"], "arn:cred");
        assert_eq!(out["reasoning"]["effort"], "high");
        assert_eq!(out["metadata"]["kiro:conversation_id"], "c-1");
        assert_eq!(out["tools"][0]["parameters"]["type"], "object");
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role": "user", "content": "hi"}));
        assert_eq!(input[1], json!({"role": "assistant", "content": "calling"}));
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["arguments"], "{\"city\":\"Paris\"}");
        assert_eq!(input[3], json!({"type": "function_call_output", "call_id": "t1", "output": "sunny"}));
        assert_eq!(input[4]["content"][1]["image_url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn rejects_body_without_conversation_state() {
        assert!(convert_request(r#"{"x":1}"#, None).is_err());
    }

    #[test]
    fn transcodes_text_reasoning_tool_and_usage() {
        let events = [
            json!({"type": "response.reasoning_text.delta", "delta": "think"}),
            json!({"type": "response.output_item.done", "item": {"type": "reasoning",
                "encrypted_content": "{\"reasoningText\":{\"text\":\"think\",\"signature\":\"SIG\"}}"}}),
            json!({"type": "response.output_text.delta", "delta": "Sure"}),
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc1", "call_id": "tooluse_1", "name": "get_weather"}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc1", "delta": ""}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc1", "delta": "{\"city\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc1", "delta": "\"Paris\"}"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc1", "arguments": "{\"city\":\"Paris\"}"}}),
            json!({"type": "response.completed", "response": {"usage": {
                "kiro:credits": {"value": 0.5, "unit": "credit", "unit_plural": "credits"},
                "kiro:context_usage_percentage": 1.25}}}),
        ];
        // 逐字节喂入，确认行缓冲在任意切分点都正确。
        let mut transcoder = Transcoder::default();
        let mut bytes = Vec::new();
        for byte in sse(&events) {
            bytes.extend(transcoder.feed(&[byte]));
        }
        let decoded = decode(&bytes);
        assert!(matches!(&decoded[0], Event::ReasoningContent(e) if e.text.as_deref() == Some("think")));
        assert!(matches!(&decoded[1], Event::ReasoningContent(e) if e.signature.as_deref() == Some("SIG")));
        assert!(matches!(&decoded[2], Event::AssistantResponse(e) if e.content == "Sure"));
        let tools: Vec<_> = decoded
            .iter()
            .filter_map(|e| if let Event::ToolUse(t) = e { Some(t) } else { None })
            .collect();
        assert_eq!(tools.len(), 4, "起始、两段参数、结束");
        assert!(tools.iter().all(|t| t.tool_use_id == "tooluse_1" && t.name == "get_weather"));
        assert_eq!(tools.iter().map(|t| t.input.as_str()).collect::<String>(), "{\"city\":\"Paris\"}");
        assert!(tools.last().unwrap().stop);
        assert!(matches!(&decoded[decoded.len() - 2], Event::Metering(m) if (m.usage - 0.5).abs() < 1e-9));
        assert!(matches!(&decoded[decoded.len() - 1], Event::ContextUsage(c) if (c.context_usage_percentage - 1.25).abs() < 1e-9));
    }

    #[test]
    fn tool_arguments_fall_back_to_done_payload_when_not_streamed() {
        let events = [
            json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc1", "call_id": "t", "name": "n"}}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc1", "arguments": "{\"a\":1}"}}),
        ];
        let mut transcoder = Transcoder::default();
        let decoded = decode(&transcoder.feed(&sse(&events)));
        let inputs: Vec<_> = decoded
            .iter()
            .filter_map(|e| if let Event::ToolUse(t) = e { Some(t.input.clone()) } else { None })
            .collect();
        assert_eq!(inputs.concat(), "{\"a\":1}");
    }

    /// 真实样本（2026-11-26 在 Enterprise 账号上对 claude-opus-5.5 实测）：
    /// refusal.delta → refusal.done → response.incomplete(content_filter)，usage 无 credit。
    #[test]
    fn content_filter_becomes_refusal_metadata_not_body_text() {
        let text = "The selected model cannot continue this conversation.";
        let events = [
            json!({"type": "response.refusal.delta", "delta": "The selected model "}),
            json!({"type": "response.refusal.delta", "delta": "cannot continue this conversation."}),
            json!({"type": "response.refusal.done", "refusal": text}),
            json!({"type": "response.incomplete", "response": {
                "status": "incomplete",
                "incomplete_details": {"reason": "content_filter"},
                "usage": {"kiro:context_usage_percentage": 0.19}}}),
        ];
        let mut transcoder = Transcoder::default();
        let decoded = decode(&transcoder.feed(&sse(&events)));

        assert!(
            !decoded.iter().any(|e| matches!(e, Event::AssistantResponse(_))),
            "拒答说明不能混进正文"
        );
        let metadata = decoded
            .iter()
            .find_map(|e| if let Event::Metadata(m) = e { Some(m) } else { None })
            .expect("应发出 metadataEvent");
        assert_eq!(metadata.content_filter_refusal().as_deref(), Some(text));
    }

    #[test]
    fn content_filter_without_refusal_text_still_reports_filtered() {
        let events = [json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "content_filter"}}})];
        let mut transcoder = Transcoder::default();
        let decoded = decode(&transcoder.feed(&sse(&events)));
        let metadata = decoded
            .iter()
            .find_map(|e| if let Event::Metadata(m) = e { Some(m) } else { None })
            .unwrap();
        assert_eq!(metadata.content_filter_refusal().as_deref(), Some("CONTENT_FILTERED"));
    }

    #[test]
    fn max_output_tokens_still_maps_to_max_tokens() {
        let events = [json!({"type": "response.incomplete", "response": {
            "incomplete_details": {"reason": "max_output_tokens"}}})];
        let mut transcoder = Transcoder::default();
        let decoded = decode(&transcoder.feed(&sse(&events)));
        let metadata = decoded
            .iter()
            .find_map(|e| if let Event::Metadata(m) = e { Some(m) } else { None })
            .unwrap();
        assert_eq!(metadata.truncation_stop_reason(), Some("max_tokens"));
        assert!(metadata.content_filter_refusal().is_none());
    }

    #[test]
    fn failure_event_becomes_exception_frame() {
        let events = [json!({"type": "response.failed", "response": {"error": {"code": "boom"}}})];
        let mut transcoder = Transcoder::default();
        let decoded = decode(&transcoder.feed(&sse(&events)));
        assert!(matches!(&decoded[0], Event::Exception { message, .. } if message.contains("boom")));
    }
}
