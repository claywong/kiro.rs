//! 计费事件
//!
//! 处理 meteringEvent 类型的事件

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// Kiro 计费事件
///
/// 表示本次请求实际消耗的 credit 数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位，当前固定为 `credit`
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数，当前固定为 `credits`
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求消耗量
    #[serde(default)]
    pub usage: f64,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unit.is_empty() {
            write!(f, "{:.6}", self.usage)
        } else {
            write!(f, "{:.6} {}", self.usage, self.unit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_metering_event() {
        let json = r#"{"unit":"credit","unitPlural":"credits","usage":0.02707833114427861}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();

        assert_eq!(event.unit, "credit");
        assert_eq!(event.unit_plural, "credits");
        assert!((event.usage - 0.02707833114427861).abs() < f64::EPSILON);
    }
}
