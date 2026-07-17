//! Anthropic Messages → Grok Build catalog 指定的 xAI API 请求转换。

use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::types::{ContentBlock, MessagesRequest, Tool};

use super::model_catalog::{GrokApiBackend, GrokModel, GrokModelCatalog, ReasoningEffort};

#[derive(Debug)]
pub struct ConvertedGrokRequest {
    pub body: Value,
    pub model: String,
    pub thinking_enabled: bool,
    pub backend: GrokApiBackend,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug)]
pub enum ConversionError {
    EmptyMessages,
    UnsupportedModel(String),
    InvalidReasoningEffort(String),
    UnsupportedReasoningEffort {
        model: String,
        effort: ReasoningEffort,
    },
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMessages => formatter.write_str("消息列表为空"),
            Self::UnsupportedModel(model) => {
                write!(formatter, "当前 Grok 凭据目录中没有可用模型 {model}")
            }
            Self::InvalidReasoningEffort(effort) => {
                write!(formatter, "不支持的 Grok reasoning effort: {effort}")
            }
            Self::UnsupportedReasoningEffort { model, effort } => {
                write!(formatter, "模型 {model} 不支持 reasoning effort={effort}")
            }
        }
    }
}

impl std::error::Error for ConversionError {}

pub fn convert_request(
    request: &MessagesRequest,
    default_model: &str,
    catalog: Option<&GrokModelCatalog>,
) -> Result<ConvertedGrokRequest, ConversionError> {
    if request.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }

    let model = resolve_model(&request.model, default_model, catalog)?;
    let model_entry = catalog.and_then(|catalog| catalog.model_by_id(&model));
    let backend = model_entry
        .map(|model| model.api_backend)
        // 真实 catalog 尚未拉取时沿用已有 `/responses` 兼容行为；真实
        // catalog 一到位后则严格按它的 apiBackend 分派。
        .unwrap_or(GrokApiBackend::Responses);
    let reasoning_effort = resolve_reasoning_effort(request, model_entry, &model)?;
    let thinking_enabled = match backend {
        GrokApiBackend::Messages => reasoning_effort
            .and_then(ReasoningEffort::to_messages_api)
            .is_some(),
        GrokApiBackend::Responses | GrokApiBackend::ChatCompletions => reasoning_effort.is_some(),
    };

    let body = match backend {
        GrokApiBackend::Responses => build_responses_body(request, &model, reasoning_effort),
        GrokApiBackend::ChatCompletions => {
            build_chat_completions_body(request, &model, reasoning_effort)
        }
        GrokApiBackend::Messages => build_messages_body(request, &model, reasoning_effort),
    };

    Ok(ConvertedGrokRequest {
        body,
        model,
        thinking_enabled,
        backend,
        reasoning_effort,
    })
}

/// 把 Claude/Kiro 别名落到配置默认模型；真实 catalog 存在时，普通模型名、
/// 显示名、以及唯一的简写都会被规范为 catalog 的实际 wire model id。
pub fn resolve_model(
    requested: &str,
    default_model: &str,
    catalog: Option<&GrokModelCatalog>,
) -> Result<String, ConversionError> {
    let requested = requested.trim();
    let use_default = requested.is_empty()
        || requested.eq_ignore_ascii_case("grok-build")
        || requested.eq_ignore_ascii_case("grok-build-latest")
        || requested.to_ascii_lowercase().starts_with("claude-");
    let candidate = if use_default {
        default_model.trim()
    } else {
        requested
    };
    if let Some(catalog) = catalog {
        return catalog
            .resolve_model_id(candidate)
            .ok_or_else(|| ConversionError::UnsupportedModel(candidate.to_string()));
    }
    if candidate.is_empty() {
        return Err(ConversionError::UnsupportedModel(default_model.to_string()));
    }
    if use_default || candidate.to_ascii_lowercase().starts_with("grok-") {
        return Ok(candidate.to_ascii_lowercase());
    }
    Ok(default_model.to_string())
}

fn resolve_reasoning_effort(
    request: &MessagesRequest,
    model: Option<&GrokModel>,
    model_id: &str,
) -> Result<Option<ReasoningEffort>, ConversionError> {
    let requested = request
        .output_config
        .as_ref()
        .map(|config| config.effort.trim())
        .filter(|effort| !effort.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            request
                .thinking
                .as_ref()
                .filter(|thinking| thinking.is_enabled())
                .map(|thinking| effort_from_budget(thinking.budget_tokens).to_string())
        });
    let Some(requested) = requested else {
        return Ok(None);
    };
    let effort = model
        .and_then(|model| model.resolve_effort(&requested))
        .or_else(|| ReasoningEffort::parse(&requested))
        .ok_or_else(|| ConversionError::InvalidReasoningEffort(requested.clone()))?;
    if let Some(model) = model {
        if !model.supports_effort(effort) {
            return Err(ConversionError::UnsupportedReasoningEffort {
                model: model_id.to_string(),
                effort,
            });
        }
    }
    Ok(Some(effort))
}

fn build_responses_body(
    request: &MessagesRequest,
    model: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Value {
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
    // 与 Grok Build Responses 适配器一致：始终请求 concise reasoning
    // summary；effort 未显式选择时只省略 `reasoning.effort`，不省略 summary。
    let mut reasoning = json!({ "summary": "concise" });
    if let Some(effort) = reasoning_effort {
        reasoning["effort"] = Value::String(effort.as_str().to_string());
    }
    body["reasoning"] = reasoning;
    body
}

fn build_chat_completions_body(
    request: &MessagesRequest,
    model: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": build_chat_messages(request),
        "max_completion_tokens": request.max_tokens.max(1),
        "stream_options": { "include_usage": true },
    });
    let tools = request
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .filter_map(convert_chat_tool)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if let Some(tool_choice) = request
        .tool_choice
        .as_ref()
        .and_then(convert_chat_tool_choice)
    {
        body["tool_choice"] = tool_choice;
    }
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = Value::String(effort.as_str().to_string());
    }
    body
}

fn build_messages_body(
    request: &MessagesRequest,
    model: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Value {
    let mut body = serde_json::to_value(request).unwrap_or_else(|_| json!({}));
    body["model"] = Value::String(model.to_string());
    body["max_tokens"] = Value::from(request.max_tokens.max(1));
    let object = body
        .as_object_mut()
        .expect("MessagesRequest serializes to object");
    match reasoning_effort.and_then(ReasoningEffort::to_messages_api) {
        Some(effort) => {
            // 对应 Grok Build `build_messages_request`：只有可转换到
            // Anthropic 的 effort 才同时设置 adaptive + summarized。
            object.insert(
                "thinking".to_string(),
                json!({ "type": "adaptive", "display": "summarized" }),
            );
            object.insert("output_config".to_string(), json!({ "effort": effort }));
        }
        None => {
            // `none` / `minimal` 在 Messages API 没有对应值；与 Grok Build
            // 一样不要伪造 `output_config.effort` 或 summarized thinking。
            object.remove("thinking");
            object.remove("output_config");
        }
    }
    body
}

fn build_chat_messages(request: &MessagesRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        let text = system
            .iter()
            .map(|message| message.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }
    for message in &request.messages {
        match message.role.as_str() {
            "system" | "developer" => {
                let text = value_to_text(&message.content);
                if !text.is_empty() {
                    messages.push(json!({ "role": message.role, "content": text }));
                }
            }
            "assistant" => append_chat_assistant_message(&mut messages, &message.content),
            _ => append_chat_user_message(&mut messages, &message.content),
        }
    }
    messages
}

fn append_chat_user_message(messages: &mut Vec<Value>, content: &Value) {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            messages.push(json!({ "role": "user", "content": text }));
        }
        return;
    }
    let mut content_parts = Vec::new();
    for block in blocks {
        match block.block_type.as_str() {
            "tool_result" => {
                flush_chat_user_message(messages, &mut content_parts);
                let tool_call_id = block
                    .tool_use_id
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": value_to_text(&block.content.unwrap_or(Value::Null)),
                }));
            }
            "image" => {
                if let Some(source) = block.source {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", source.media_type, source.data),
                        },
                    }));
                }
            }
            "text" => {
                if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                    content_parts.push(json!({ "type": "text", "text": text }));
                }
            }
            _ => {
                if let Some(text) = block
                    .text
                    .or(block.thinking)
                    .filter(|text| !text.is_empty())
                {
                    content_parts.push(json!({ "type": "text", "text": text }));
                }
            }
        }
    }
    flush_chat_user_message(messages, &mut content_parts);
}

fn flush_chat_user_message(messages: &mut Vec<Value>, content_parts: &mut Vec<Value>) {
    if !content_parts.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": std::mem::take(content_parts),
        }));
    }
}

fn append_chat_assistant_message(messages: &mut Vec<Value>, content: &Value) {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            messages.push(json!({ "role": "assistant", "content": text }));
        }
        return;
    }
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block.block_type.as_str() {
            "tool_use" => {
                let id = block
                    .id
                    .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
                let name = block.name.unwrap_or_else(|| "tool".to_string());
                let arguments = serde_json::to_string(&block.input.unwrap_or_else(|| json!({})))
                    .unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }));
            }
            "text" => {
                if let Some(value) = block.text.filter(|text| !text.is_empty()) {
                    text.push(value);
                }
            }
            "thinking" => {
                if let Some(value) = block
                    .thinking
                    .or(block.text)
                    .filter(|text| !text.is_empty())
                {
                    text.push(value);
                }
            }
            _ => {}
        }
    }
    if !text.is_empty() || !tool_calls.is_empty() {
        let mut message = json!({
            "role": "assistant",
            "content": if text.is_empty() { Value::Null } else { Value::String(text.join("\n")) },
        });
        if !tool_calls.is_empty() {
            message["tool_calls"] = Value::Array(tool_calls);
        }
        messages.push(message);
    }
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

fn convert_chat_tool(tool: &Tool) -> Option<Value> {
    let converted = convert_tool(tool)?;
    if converted.get("type").and_then(Value::as_str) == Some("web_search") {
        // OpenAI Chat Completions 没有与 Responses 相同的顶层 web_search
        // 工具形状；保留其 type，由支持该扩展的 xAI backend 自行识别。
        return Some(converted);
    }
    Some(json!({
        "type": "function",
        "function": {
            "name": converted.get("name").cloned().unwrap_or(Value::String("tool".to_string())),
            "description": converted.get("description").cloned().unwrap_or(Value::String(String::new())),
            "parameters": converted.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" })),
        },
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

fn convert_chat_tool_choice(value: &Value) -> Option<Value> {
    let tool_type = value.get("type").and_then(Value::as_str)?;
    match tool_type {
        "auto" => Some(Value::String("auto".to_string())),
        "any" | "required" => Some(Value::String("required".to_string())),
        "none" => Some(Value::String("none".to_string())),
        "tool" => value
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "function": { "name": name } })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::types::{Message, OutputConfig, Thinking};

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
        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        assert_eq!(converted.model, "grok-4.5");
        assert_eq!(converted.body["input"][0]["type"], "function_call");
        assert_eq!(converted.body["input"][1]["type"], "function_call_output");
        assert_eq!(converted.body["tools"][0]["type"], "function");
        assert!(converted.body.get("reasoning").is_some());
    }

    #[test]
    fn preserves_grok_model_and_maps_claude_alias() {
        assert_eq!(
            resolve_model("grok-4.5", "grok-3", None).unwrap(),
            "grok-4.5"
        );
        assert_eq!(
            resolve_model("claude-opus-4-6", "grok-4.5", None).unwrap(),
            "grok-4.5"
        );
    }

    #[test]
    fn responses_effort_requests_concise_summary_and_preserves_xhigh() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: true,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("hello"),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: Some(OutputConfig {
                effort: "max".to_string(),
            }),
            metadata: None,
        };
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"responses",
                "supportsReasoningEffort":true,
                "reasoningEfforts":["low","medium","high","xhigh"]
            }]}),
            "https://api.x.ai/v1",
        );
        let converted = convert_request(&request, "grok-4.5", Some(&catalog)).unwrap();
        assert_eq!(converted.backend, GrokApiBackend::Responses);
        assert_eq!(converted.reasoning_effort, Some(ReasoningEffort::Xhigh));
        assert!(converted.thinking_enabled);
        assert_eq!(converted.body["reasoning"]["effort"], "xhigh");
        assert_eq!(converted.body["reasoning"]["summary"], "concise");
    }

    #[test]
    fn messages_backend_pairs_adaptive_summarized_thinking_with_effort() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: true,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("hello"),
            }],
            tools: None,
            tool_choice: None,
            thinking: Some(Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 8_000,
            }),
            output_config: None,
            metadata: None,
        };
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"messages",
                "supportsReasoningEffort":true,
                "reasoningEfforts":["low","medium","high"]
            }]}),
            "https://api.x.ai/v1",
        );
        let converted = convert_request(&request, "grok-4.5", Some(&catalog)).unwrap();
        assert_eq!(converted.backend, GrokApiBackend::Messages);
        assert_eq!(converted.body["output_config"]["effort"], "medium");
        assert_eq!(converted.body["thinking"]["type"], "adaptive");
        assert_eq!(converted.body["thinking"]["display"], "summarized");
    }
}
