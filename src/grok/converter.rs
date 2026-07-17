//! Anthropic Messages → xAI Responses API 请求转换。

use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::types::{ContentBlock, MessagesRequest, Tool};

/// Grok Build 可用的文本模型清单。模型端点不依赖 Kiro catalog，避免两个
/// provider 的模型能力相互污染。
pub const GROK_BUILD_MODELS: &[&str] = &[
    "grok-4.5",
    "grok-build-0.1",
    "grok-4.3",
    "grok-4.20-0309-reasoning",
    "grok-4.20-0309-non-reasoning",
    "grok-4.20-multi-agent-0309",
    "grok-4",
    "grok-4-fast",
    "grok-3-mini",
    "grok-3-mini-fast",
    "grok-3",
];

#[derive(Debug)]
pub struct ConvertedGrokRequest {
    pub body: Value,
    pub model: String,
    pub thinking_enabled: bool,
}

#[derive(Debug)]
pub enum ConversionError {
    EmptyMessages,
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMessages => formatter.write_str("消息列表为空"),
        }
    }
}

impl std::error::Error for ConversionError {}

pub fn convert_request(
    request: &MessagesRequest,
    default_model: &str,
) -> Result<ConvertedGrokRequest, ConversionError> {
    if request.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    let model = resolve_model(&request.model, default_model);
    let mut input = Vec::new();
    let mut instructions = Vec::new();

    if let Some(system) = &request.system {
        for message in system {
            let text = message.text.trim();
            if !text.is_empty() {
                instructions.push(text.to_string());
            }
        }
    }

    for message in &request.messages {
        match message.role.as_str() {
            "system" | "developer" => {
                let text = value_to_text(&message.content);
                if !text.is_empty() {
                    instructions.push(text);
                }
            }
            "assistant" => append_assistant_message(&mut input, &message.content),
            _ => append_user_message(&mut input, &message.content),
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "max_output_tokens": request.max_tokens.max(1),
        "store": false,
    });
    if !instructions.is_empty() {
        body["instructions"] = Value::String(instructions.join("\n\n"));
    }

    if let Some(metadata) = &request.metadata {
        if let Some(session_id) = metadata
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            // xAI Grok CLI 将该字段用于连续对话/提示缓存；保留客户端传来的
            // session id 便于和 Claude Code 的会话保持一致。
            body["prompt_cache_key"] = Value::String(session_id.to_string());
        }
    }

    let converted_tools = request
        .tools
        .as_ref()
        .map(|tools| tools.iter().filter_map(convert_tool).collect::<Vec<_>>())
        .unwrap_or_default();
    if !converted_tools.is_empty() {
        body["tools"] = Value::Array(converted_tools);
    }
    if let Some(tool_choice) = request.tool_choice.as_ref().and_then(convert_tool_choice) {
        body["tool_choice"] = tool_choice;
    }

    let thinking_enabled = request
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.is_enabled());
    if thinking_enabled || request.output_config.is_some() {
        let effort = request
            .output_config
            .as_ref()
            .map(|config| config.effort.as_str())
            .unwrap_or_else(|| {
                request
                    .thinking
                    .as_ref()
                    .map(|thinking| effort_from_budget(thinking.budget_tokens))
                    .unwrap_or("high")
            });
        body["reasoning"] = json!({ "effort": normalize_effort(effort) });
    }

    Ok(ConvertedGrokRequest {
        body,
        model,
        thinking_enabled,
    })
}

/// 把 Claude / Kiro 模型别名温和地落到配置的 Grok 默认模型；已经是 Grok
/// 模型时保留原值，使新模型无需服务端升级即可调用。
pub fn resolve_model(requested: &str, default_model: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty()
        || requested.eq_ignore_ascii_case("grok-build")
        || requested.eq_ignore_ascii_case("grok-build-latest")
        || requested.to_ascii_lowercase().starts_with("claude-")
    {
        return default_model.to_string();
    }
    if requested.to_ascii_lowercase().starts_with("grok-") {
        return requested.to_ascii_lowercase();
    }
    default_model.to_string()
}

fn append_user_message(input: &mut Vec<Value>, content: &Value) {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            input.push(message_item(
                "user",
                vec![json!({ "type": "input_text", "text": text })],
            ));
        }
        return;
    }

    let mut content_parts = Vec::new();
    for block in blocks {
        match block.block_type.as_str() {
            "tool_result" => {
                flush_message(input, "user", &mut content_parts);
                let call_id = block
                    .tool_use_id
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": value_to_text(&block.content.unwrap_or(Value::Null)),
                }));
            }
            "image" => {
                if let Some(source) = block.source {
                    content_parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", source.media_type, source.data),
                    }));
                }
            }
            "text" => {
                if let Some(text) = block.text {
                    if !text.is_empty() {
                        content_parts.push(json!({ "type": "input_text", "text": text }));
                    }
                }
            }
            _ => {
                let text = block.text.or(block.thinking).unwrap_or_default();
                if !text.is_empty() {
                    content_parts.push(json!({ "type": "input_text", "text": text }));
                }
            }
        }
    }
    flush_message(input, "user", &mut content_parts);
}

fn append_assistant_message(input: &mut Vec<Value>, content: &Value) {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            input.push(message_item(
                "assistant",
                vec![json!({ "type": "output_text", "text": text })],
            ));
        }
        return;
    }

    let mut content_parts = Vec::new();
    for block in blocks {
        match block.block_type.as_str() {
            "tool_use" => {
                flush_message(input, "assistant", &mut content_parts);
                let call_id = block
                    .id
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let name = block.name.unwrap_or_else(|| "tool".to_string());
                let arguments = serde_json::to_string(&block.input.unwrap_or_else(|| json!({})))
                    .unwrap_or_else(|_| "{}".to_string());
                input.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            "text" => {
                if let Some(text) = block.text {
                    if !text.is_empty() {
                        content_parts.push(json!({ "type": "output_text", "text": text }));
                    }
                }
            }
            // Anthropic thinking 没有可重放的 xAI reasoning item；保留其文本到
            // assistant 输出，避免历史上下文突然丢失，同时不伪造 xAI 签名。
            "thinking" => {
                if let Some(text) = block.thinking.or(block.text) {
                    if !text.is_empty() {
                        content_parts.push(json!({ "type": "output_text", "text": text }));
                    }
                }
            }
            _ => {}
        }
    }
    flush_message(input, "assistant", &mut content_parts);
}

fn flush_message(input: &mut Vec<Value>, role: &str, content: &mut Vec<Value>) {
    if !content.is_empty() {
        input.push(message_item(role, std::mem::take(content)));
    }
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    json!({ "type": "message", "role": role, "content": content })
}

fn parse_content_blocks(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::Array(_) => serde_json::from_value(content.clone()).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .or_else(|| item.get("thinking"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| item.get("content").map(value_to_text).unwrap_or_default())
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("thinking"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| object.get("content").map(value_to_text))
            .unwrap_or_else(|| value.to_string()),
        Value::Null => String::new(),
        _ => value.to_string(),
    }
}

fn convert_tool(tool: &Tool) -> Option<Value> {
    if tool
        .tool_type
        .as_deref()
        .is_some_and(|tool_type| tool_type.starts_with("web_search"))
    {
        return Some(json!({ "type": "web_search" }));
    }
    if tool.name.trim().is_empty() {
        return None;
    }
    let mut parameters = serde_json::to_value(&tool.input_schema).unwrap_or_else(|_| json!({}));
    if !parameters.is_object()
        || parameters
            .as_object()
            .is_some_and(|object| object.is_empty())
    {
        parameters = json!({ "type": "object", "properties": {}, "additionalProperties": true });
    } else if parameters.get("type").is_none() {
        parameters["type"] = Value::String("object".to_string());
    }
    Some(json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": parameters,
    }))
}

fn convert_tool_choice(value: &Value) -> Option<Value> {
    let tool_type = value.get("type").and_then(Value::as_str)?;
    match tool_type {
        "auto" => Some(Value::String("auto".to_string())),
        "any" | "required" => Some(Value::String("required".to_string())),
        "none" => Some(Value::String("none".to_string())),
        "tool" => value
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "name": name })),
        _ => None,
    }
}

fn effort_from_budget(budget: i32) -> &'static str {
    match budget {
        ..=4_000 => "low",
        ..=12_000 => "medium",
        _ => "high",
    }
}

fn normalize_effort(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "low" => "low",
        "medium" => "medium",
        "high" | "max" | "xhigh" => "high",
        _ => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{Message, Thinking};

    #[test]
    fn converts_tool_history_and_tool_definition() {
        let request = MessagesRequest {
            model: "claude-sonnet-4-5".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![
                Message {
                    role: "assistant".to_string(),
                    content: json!([{"type":"tool_use","id":"tool_1","name":"read_file","input":{"path":"a"}}]),
                },
                Message {
                    role: "user".to_string(),
                    content: json!([{"type":"tool_result","tool_use_id":"tool_1","content":"ok"}]),
                },
            ],
            tools: Some(vec![Tool {
                tool_type: None,
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                input_schema: serde_json::from_value(
                    json!({"type":"object","properties":{"path":{"type":"string"}}}),
                )
                .unwrap(),
                max_uses: None,
            }]),
            tool_choice: None,
            thinking: Some(Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 10_000,
            }),
            output_config: None,
            metadata: None,
        };
        let converted = convert_request(&request, "grok-4.5").unwrap();
        assert_eq!(converted.model, "grok-4.5");
        assert_eq!(converted.body["input"][0]["type"], "function_call");
        assert_eq!(converted.body["input"][1]["type"], "function_call_output");
        assert_eq!(converted.body["tools"][0]["type"], "function");
        assert!(converted.body.get("reasoning").is_some());
    }

    #[test]
    fn preserves_grok_model_and_maps_claude_alias() {
        assert_eq!(resolve_model("grok-4.5", "grok-3"), "grok-4.5");
        assert_eq!(resolve_model("claude-opus-4-6", "grok-4.5"), "grok-4.5");
    }
}
