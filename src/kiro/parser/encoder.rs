//! AWS Event Stream 帧编码
//!
//! 解析器只负责解码。CreateResponse 回退路径把上游 SSE 转写成同样的二进制帧，
//! 让下游沿用现有的 `EventStreamDecoder`，不必为第二种协议改动每一个消费点。

use super::crc::crc32;
use super::header::HeaderValueType;

/// 把一组字符串头和 payload 编码成一个完整的 event-stream 消息。
///
/// 布局：`total_len(4) | headers_len(4) | prelude_crc(4) | headers | payload | message_crc(4)`。
pub fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        // 头名长度 1 字节、字符串值长度 2 字节；本地构造的头都远小于该上限。
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(HeaderValueType::String as u8);
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }

    // prelude(12) + headers + payload + message_crc(4)
    let total_len = 12 + header_bytes.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u32).to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&out[..8]);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);
    let message_crc = crc32(&out);
    out.extend_from_slice(&message_crc.to_be_bytes());
    out
}

/// 编码一个 `:message-type = event` 的事件帧。
pub fn encode_event(event_type: &str, payload: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    encode_frame(
        &[
            (":message-type", "event"),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ],
        &body,
    )
}

/// 编码一个 `:message-type = exception` 的异常帧。
pub fn encode_exception(exception_type: &str, message: &str) -> Vec<u8> {
    encode_frame(
        &[
            (":message-type", "exception"),
            (":exception-type", exception_type),
            (":content-type", "application/json"),
        ],
        message.as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::parser::frame::parse_frame;
    use serde_json::json;

    #[test]
    fn event_frame_roundtrips_through_parser() {
        let bytes = encode_event("assistantResponseEvent", &json!({"content": "你好"}));
        let (frame, used) = parse_frame(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(frame.message_type(), Some("event"));
        assert_eq!(frame.event_type(), Some("assistantResponseEvent"));
        let payload: serde_json::Value = frame.payload_as_json().unwrap();
        assert_eq!(payload["content"], "你好");
    }

    #[test]
    fn exception_frame_roundtrips_through_parser() {
        let bytes = encode_exception("ThrottlingException", "slow down");
        let (frame, _) = parse_frame(&bytes).unwrap().unwrap();
        assert_eq!(frame.message_type(), Some("exception"));
        assert_eq!(frame.payload_as_str(), "slow down");
    }
}
