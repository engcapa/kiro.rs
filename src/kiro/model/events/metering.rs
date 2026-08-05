//! 计费事件
//!
//! 处理 meteringEvent 类型的事件

use serde::Deserialize;

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 计费事件
///
/// 上游在响应流末尾下发本次请求的真实扣费，是唯一可信的成本来源。
/// 实测 payload 形如：
/// `{"unit":"credit","unitPlural":"credits","usage":2.3283256603316747}`
///
/// # 示例
///
/// ```rust
/// use kiro_rs::kiro::model::events::MeteringEvent;
///
/// let json = r#"{"unit":"credit","unitPlural":"credits","usage":2.5}"#;
/// let event: MeteringEvent = serde_json::from_str(json).unwrap();
/// assert_eq!(event.usage, 2.5);
/// assert_eq!(event.unit_label(), "credits");
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteringEvent {
    /// 计费单位（如 "credit"）
    #[serde(default)]
    pub unit: String,
    /// 计费单位复数形式（如 "credits"）
    #[serde(default)]
    pub unit_plural: String,
    /// 本次请求的消耗量
    #[serde(default)]
    pub usage: f64,
}

impl EventPayload for MeteringEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

impl MeteringEvent {
    /// 创建计费事件
    pub fn new(usage: f64) -> Self {
        Self {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage,
        }
    }

    /// 用于展示的单位名称：usage 不等于 1 时优先取复数形式
    pub fn unit_label(&self) -> &str {
        let plural = (self.usage - 1.0).abs() > f64::EPSILON;
        if plural && !self.unit_plural.is_empty() {
            &self.unit_plural
        } else if !self.unit.is_empty() {
            &self.unit
        } else {
            "credits"
        }
    }
}

impl std::fmt::Display for MeteringEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.6} {}", self.usage, self.unit_label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_real_payload() {
        // 取自真实抓包 docs/kiro_rs_aws_turn1_res.txt
        let json = r#"{"unit":"credit","unitPlural":"credits","usage":2.3283256603316747}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.unit, "credit");
        assert_eq!(event.unit_plural, "credits");
        assert!((event.usage - 2.3283256603316747).abs() < f64::EPSILON);
    }

    #[test]
    fn test_deserialize_missing_fields() {
        // 字段缺失时不应反序列化失败
        let event: MeteringEvent = serde_json::from_str("{}").unwrap();
        assert_eq!(event.usage, 0.0);
        assert_eq!(event.unit_label(), "credits");
    }

    #[test]
    fn test_deserialize_ignores_unknown_fields() {
        let json = r#"{"usage":1.5,"unit":"credit","somethingNew":true}"#;
        let event: MeteringEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.usage, 1.5);
    }

    #[test]
    fn test_unit_label_singular() {
        let event = MeteringEvent {
            unit: "credit".to_string(),
            unit_plural: "credits".to_string(),
            usage: 1.0,
        };
        assert_eq!(event.unit_label(), "credit");
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", MeteringEvent::new(2.5)), "2.500000 credits");
    }
}
