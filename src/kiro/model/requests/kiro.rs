//! Kiro 请求类型定义
//!
//! 定义 Kiro API 的主请求结构

use serde::{Deserialize, Serialize};

use super::conversation::ConversationState;

/// Kiro API 请求
///
/// 用于构建发送给 Kiro API 的请求
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::requests::{
///     KiroRequest, ConversationState, CurrentMessage, UserInputMessage, Tool
/// };
///
/// // 创建简单请求
/// let state = ConversationState::new("conv-123")
///     .with_agent_task_type("vibe")
///     .with_current_message(CurrentMessage::new(
///         UserInputMessage::new("Hello", "claude-3-5-sonnet")
///     ));
///
/// let request = KiroRequest::new(state);
/// let json = request.to_json().unwrap();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroRequest {
    /// 对话状态
    pub conversation_state: ConversationState,
    /// Profile ARN（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// Additional model request fields (a real Kiro CLI wire field carrying control switches such as `output_config.effort`)
    ///
    /// Real wire sample (captured from real Kiro CLI traffic):
    /// ```json
    /// "additionalModelRequestFields": {
    ///     "output_config": { "effort": "max" }
    /// }
    /// ```
    /// Effort tiers are model-dependent. Older 4.5/4.6 models accept
    /// `low / medium / high / max`; newer effort-capable models may also
    /// accept `xhigh`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<AdditionalModelRequestFields>,
}

/// Top-level container for the AWS Q CodeWhisperer `additionalModelRequestFields`
///
/// Note: in the real wire format the inner `output_config` field is `snake_case`,
/// unlike the outer `additionalModelRequestFields` (camelCase),
/// so this struct **must not** inherit `rename_all = "camelCase"`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AdditionalModelRequestFields {
    /// Output configuration (including reasoning effort)
    ///
    /// Used by the Claude family (Opus 4.6/4.7/4.8, Sonnet 4.6, Claude 5 line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<KiroOutputConfig>,
    /// Reasoning configuration, the GPT-5.6 family's equivalent of `output_config`
    ///
    /// The GPT-5.6 line (sol / terra / luna) rejects `output_config` outright
    /// (`property 'output_config' is not defined in the schema`) but does accept
    /// the effort tier under a `reasoning` key. Real wire sample:
    /// ```json
    /// "additionalModelRequestFields": { "reasoning": { "effort": "xhigh" } }
    /// ```
    /// The two keys are mutually exclusive: pick per model, never send both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<KiroReasoningConfig>,
}

/// The effort control field recognized by the AWS Q backend
///
/// Accepted tiers are model-dependent. Older 4.5/4.6 models accept
/// `low / medium / high / max`; newer effort-capable models may also accept
/// `xhigh`.
///
/// Measured (via a ladder experiment), the same prompt between `low` and `max` differs
/// by roughly 5x in response time and output length, so this **is a protocol field that genuinely takes effect**,
/// completely unlike the "pseudo-protocol" of stuffing a `<thinking_effort>` XML tag into the system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroOutputConfig {
    pub effort: String,
}

/// The GPT-5.6 family's effort control field
///
/// Same tier vocabulary as [`KiroOutputConfig`] (`low / medium / high / xhigh / max`),
/// only the wrapping key differs. See [`AdditionalModelRequestFields::reasoning`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroReasoningConfig {
    pub effort: String,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_kiro_request_deserialize() {
        let json = r#"{
            "conversationState": {
                "conversationId": "conv-456",
                "currentMessage": {
                    "userInputMessage": {
                        "content": "Test message",
                        "modelId": "claude-3-5-sonnet",
                        "userInputMessageContext": {
                            "envState": {
                                "operatingSystem": "macos",
                                "currentWorkingDirectory": "/workspace"
                            }
                        }
                    }
                }
            }
        }"#;

        let request: KiroRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.conversation_state.conversation_id, "conv-456");
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "Test message"
        );
    }

    #[test]
    fn test_additional_model_request_fields_wire_format() {
        // The wire format requires the outer key to be camelCase
        // (`additionalModelRequestFields`) while the inner key stays snake_case
        // (`output_config`), matching real Kiro CLI traffic.
        let fields = AdditionalModelRequestFields {
            output_config: Some(KiroOutputConfig {
                effort: "max".to_string(),
            }),
            reasoning: None,
        };
        let v = serde_json::to_value(&fields).unwrap();
        assert_eq!(v["output_config"]["effort"], "max");
        assert!(
            v.get("outputConfig").is_none(),
            "inner key must stay snake_case output_config, got {v}"
        );
        assert!(
            v.get("reasoning").is_none(),
            "unset reasoning must be omitted entirely, got {v}"
        );
    }

    /// The GPT-5.6 family's wire shape: `reasoning` only, no `output_config` leaking in.
    ///
    /// Sending `output_config` to these models returns 400
    /// (`property 'output_config' is not defined in the schema`), so the absence
    /// assertion is the one that actually guards production traffic.
    #[test]
    fn test_additional_model_request_fields_reasoning_wire_format() {
        let fields = AdditionalModelRequestFields {
            output_config: None,
            reasoning: Some(KiroReasoningConfig {
                effort: "xhigh".to_string(),
            }),
        };
        let json = serde_json::to_string(&fields).unwrap();
        assert_eq!(json, r#"{"reasoning":{"effort":"xhigh"}}"#);
    }
}
