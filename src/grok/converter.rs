//! Anthropic Messages → Grok Build catalog 指定的 xAI API 请求转换。

use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::types::{ContentBlock, MessagesRequest, Tool};

use super::model_catalog::{GrokApiBackend, GrokModel, GrokModelCatalog, ReasoningEffort};
use super::reasoning_sig::{ReasoningSignatureCodec, package_to_input_items};

#[derive(Debug)]
pub struct ConvertedGrokRequest {
    pub body: Value,
    pub model: String,
    pub thinking_enabled: bool,
    pub backend: GrokApiBackend,
    pub reasoning_effort: Option<ReasoningEffort>,
    /// 是否把 Anthropic Web Search 转换成 xAI Responses 的 hosted tool。
    /// 该标记会继续传给凭据选择器，保证在多账号场景中排除 catalog
    /// 明确声明 `supportsBackendSearch=false` 的凭据。
    pub uses_hosted_web_search: bool,
}

/// 选凭据前的轻量规划：解析模型、effort 与能力需求，但不构建上游 body。
///
/// 真正的 wire 转换必须使用**即将发送的那张凭据**的 catalog，避免合并目录的
/// `apiBackend` 与单凭据 backend 不一致时把 Chat/Responses 请求体投错。
#[derive(Debug, Clone)]
pub struct ConversionPlan {
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub needs_web_search: bool,
    pub needs_files: bool,
    /// 请求携带图片输入（vision）。用于在多账号异构时排除 catalog 明确声明
    /// 不支持图片的凭据；不强制 backend（base64 图片在 Responses/Chat 均可，
    /// 只有 `file_id` 图片才经 `needs_files` 强制 Responses）。
    pub needs_image: bool,
}

impl ConversionPlan {
    /// 选凭据时的 backend 约束：Files / hosted Web Search 只能走 Responses。
    pub fn backend_constraint(&self) -> Option<GrokApiBackend> {
        if self.needs_files || self.needs_web_search {
            Some(GrokApiBackend::Responses)
        } else {
            None
        }
    }
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
    WebSearchRequiresResponses {
        model: String,
        backend: GrokApiBackend,
    },
    WebSearchUnsupported(String),
    FilesRequireResponses(String),
    UnsupportedContentBlock(String),
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
            Self::WebSearchRequiresResponses { model, backend } => write!(
                formatter,
                "模型 {model} 使用 {} backend，无法承载 Grok Build 的 hosted web_search；请选择 Responses backend 模型",
                backend.as_str(),
            ),
            Self::WebSearchUnsupported(model) => write!(
                formatter,
                "模型 {model} 的 Grok Build catalog 明确声明 supportsBackendSearch=false，无法启用 hosted web_search"
            ),
            Self::FilesRequireResponses(model) => write!(
                formatter,
                "模型 {model} 使用非 Responses backend；Anthropic source.type=file 需要 xAI Responses backend"
            ),
            Self::UnsupportedContentBlock(detail) => write!(
                formatter,
                "不支持的 Anthropic 内容块，请先转换或移除: {detail}"
            ),
        }
    }
}

impl std::error::Error for ConversionError {}

/// 用并集/bootstrap catalog 做模型别名规范化与能力探测，供选凭据使用。
///
/// 注意：此处的 effort 校验基于传入 catalog；最终 wire 转换仍须用目标凭据
/// catalog 再走一遍 [`convert_request`]。
pub fn plan_request(
    request: &MessagesRequest,
    default_model: &str,
    catalog: Option<&GrokModelCatalog>,
) -> Result<ConversionPlan, ConversionError> {
    if request.messages.is_empty() {
        return Err(ConversionError::EmptyMessages);
    }
    let model = resolve_model(&request.model, default_model, catalog)?;
    let model_entry = catalog.and_then(|catalog| catalog.model_by_id(&model));
    let needs_web_search = request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_web_search_tool));
    let needs_files = request_has_file_inputs(request);
    let needs_image = request_has_image_inputs(request);
    // 规划阶段对 effort 做宽松解析：有 model entry 时校验菜单；无 entry 时仍
    // 解析 wire 值，留给目标凭据 catalog 做最终裁决。
    let reasoning_effort = resolve_reasoning_effort(request, model_entry, &model)?;
    Ok(ConversionPlan {
        model,
        reasoning_effort,
        needs_web_search,
        needs_files,
        needs_image,
    })
}

pub fn convert_request(
    request: &MessagesRequest,
    default_model: &str,
    catalog: Option<&GrokModelCatalog>,
) -> Result<ConvertedGrokRequest, ConversionError> {
    convert_request_for_credential(request, default_model, catalog, None)
}

/// 与 [`convert_request`] 相同，但携带 signature codec，用于把历史
/// `thinking.signature` 中打包的 xAI reasoning 展开回放（HMAC + model/backend
/// 校验；不再校验 credential，见 [`package_matches_route`]）。
pub fn convert_request_for_credential(
    request: &MessagesRequest,
    default_model: &str,
    catalog: Option<&GrokModelCatalog>,
    signature_codec: Option<&ReasoningSignatureCodec>,
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
    let requests_file_inputs = request_has_file_inputs(request);
    // xAI 公开文档把上传的 file_id 定义在 Responses 的 `input_file` item。
    // Chat Completions 没有等价稳定字段；Messages catalog backend 虽然形状接近
    // Anthropic，但文件实际存储在 xAI Files API，不能假定其会识别同一 source。
    if requests_file_inputs && backend != GrokApiBackend::Responses {
        return Err(ConversionError::FilesRequireResponses(model));
    }
    let requests_web_search = request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(is_web_search_tool));
    // Grok Build 只会把 backend-hosted tool 加入 Responses 请求。即使目标
    // catalog 暴露 Messages backend，也不能把 Claude Code 的普通 WebSearch
    // function 原样透传，否则 xAI 仍会生成客户端工具参数（例如 num_results）。
    if requests_web_search && backend != GrokApiBackend::Responses {
        return Err(ConversionError::WebSearchRequiresResponses { model, backend });
    }
    let uses_hosted_web_search = requests_web_search;
    // 未声明 supportsBackendSearch 时允许尝试，由 xAI Responses 最终裁决；
    // 只有 catalog 明确声明 false 时才前置拒绝。
    if uses_hosted_web_search
        && model_entry.is_some_and(|model| model.supports_backend_search == Some(false))
    {
        return Err(ConversionError::WebSearchUnsupported(model));
    }
    let reasoning_effort = resolve_reasoning_effort(request, model_entry, &model)?;
    let thinking_enabled = match backend {
        GrokApiBackend::Messages => reasoning_effort
            .and_then(ReasoningEffort::to_messages_api)
            .is_some(),
        GrokApiBackend::Responses | GrokApiBackend::ChatCompletions => reasoning_effort.is_some(),
    };

    let body = match backend {
        GrokApiBackend::Responses => {
            build_responses_body(request, &model, reasoning_effort, signature_codec)?
        }
        GrokApiBackend::ChatCompletions => {
            build_chat_completions_body(request, &model, reasoning_effort)?
        }
        GrokApiBackend::Messages => build_messages_body(request, &model, reasoning_effort),
    };

    Ok(ConvertedGrokRequest {
        body,
        model,
        thinking_enabled,
        backend,
        reasoning_effort,
        uses_hosted_web_search,
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
    signature_codec: Option<&ReasoningSignatureCodec>,
) -> Result<Value, ConversionError> {
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
            "assistant" => {
                append_assistant_message(&mut input, &message.content, signature_codec)?;
            }
            _ => append_user_message(&mut input, &message.content)?,
        }
    }

    let mut body = json!({
        "model": model,
        "input": input,
        "max_output_tokens": request.max_tokens.max(1),
        "store": false,
        // 与 Grok Build sampler 一致：请求 encrypted reasoning，便于 Claude Code
        // 通过 thinking.signature 原样回传后做多轮 reasoning 回放。
        "include": ["reasoning.encrypted_content"],
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
        .map(|tools| convert_tools(tools, convert_tool))
        .unwrap_or_default();
    if !converted_tools.is_empty() {
        body["tools"] = Value::Array(converted_tools);
    }
    if let Some(tool_choice) = request.tool_choice.as_ref().and_then(convert_tool_choice) {
        body["tool_choice"] = tool_choice;
    }
    apply_sampling_params(&mut body, request);
    // 与 Grok Build Responses 适配器一致：始终请求 concise reasoning
    // summary；effort 未显式选择时只省略 `reasoning.effort`，不省略 summary。
    // 客户端未声明 thinking/effort 时 summary 不会转成 Anthropic thinking 块
    // （见 README「reasoning summary 语义」）。
    let mut reasoning = json!({ "summary": "concise" });
    if let Some(effort) = reasoning_effort {
        reasoning["effort"] = Value::String(effort.as_str().to_string());
    }
    body["reasoning"] = reasoning;
    Ok(body)
}

fn build_chat_completions_body(
    request: &MessagesRequest,
    model: &str,
    reasoning_effort: Option<ReasoningEffort>,
) -> Result<Value, ConversionError> {
    let mut body = json!({
        "model": model,
        "messages": build_chat_messages(request)?,
        // 与 Grok Build ChatCompletionRequest / xAI Chat 对照一致：使用 max_tokens。
        "max_tokens": request.max_tokens.max(1),
        "stream_options": { "include_usage": true },
    });
    let tools = request
        .tools
        .as_ref()
        .map(|tools| convert_tools(tools, convert_chat_tool))
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
    apply_sampling_params(&mut body, request);
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = Value::String(effort.as_str().to_string());
    }
    Ok(body)
}

/// 将 Anthropic 可选采样参数原样写入 Responses / Chat Completions body。
/// `None` 不写入字段，交给上游默认值。
fn apply_sampling_params(body: &mut Value, request: &MessagesRequest) {
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = request.top_p {
        body["top_p"] = json!(top_p);
    }
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

fn build_chat_messages(request: &MessagesRequest) -> Result<Vec<Value>, ConversionError> {
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
            "assistant" => append_chat_assistant_message(&mut messages, &message.content)?,
            _ => append_chat_user_message(&mut messages, &message.content)?,
        }
    }
    Ok(messages)
}

fn append_chat_user_message(
    messages: &mut Vec<Value>,
    content: &Value,
) -> Result<(), ConversionError> {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            messages.push(json!({ "role": "user", "content": text }));
        }
        return Ok(());
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
            "image" | "image_url" => {
                if let Some(url) = image_url_from_block(&block) {
                    validate_image_url(&url)?;
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": url,
                        },
                    }));
                }
            }
            "document" => {
                return Err(ConversionError::UnsupportedContentBlock(
                    "Chat Completions 不支持 document 块；请使用 Responses backend 与 Files API"
                        .to_string(),
                ));
            }
            "text" => {
                if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                    content_parts.push(json!({ "type": "text", "text": text }));
                }
            }
            other => {
                if let Some(text) = block
                    .text
                    .or(block.thinking)
                    .filter(|text| !text.is_empty())
                {
                    content_parts.push(json!({ "type": "text", "text": text }));
                } else if !matches!(other, "thinking") {
                    return Err(ConversionError::UnsupportedContentBlock(format!(
                        "Chat 路径不支持内容类型 {other}"
                    )));
                }
            }
        }
    }
    flush_chat_user_message(messages, &mut content_parts);
    Ok(())
}

fn flush_chat_user_message(messages: &mut Vec<Value>, content_parts: &mut Vec<Value>) {
    if !content_parts.is_empty() {
        messages.push(json!({
            "role": "user",
            "content": std::mem::take(content_parts),
        }));
    }
}

fn append_chat_assistant_message(
    messages: &mut Vec<Value>,
    content: &Value,
) -> Result<(), ConversionError> {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            messages.push(json!({ "role": "assistant", "content": text }));
        }
        return Ok(());
    }
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block.block_type.as_str() {
            "server_tool_use" | "web_search_tool_result" => {
                return Err(ConversionError::UnsupportedContentBlock(
                    "Chat Completions 不支持历史 server_tool_use / web_search_tool_result".to_string(),
                ));
            }
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
            other => {
                return Err(ConversionError::UnsupportedContentBlock(format!(
                    "Chat assistant 历史不支持内容类型 {other}"
                )));
            }
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
    Ok(())
}

fn append_user_message(input: &mut Vec<Value>, content: &Value) -> Result<(), ConversionError> {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            input.push(message_item(
                "user",
                vec![json!({ "type": "input_text", "text": text })],
            ));
        }
        return Ok(());
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
                if let Some(file_id) = file_id_from_block(&block) {
                    content_parts.push(json!({
                        "type": "input_file",
                        "file_id": file_id,
                    }));
                } else if let Some(url) = image_url_from_block(&block) {
                    validate_image_url(&url)?;
                    content_parts.push(json!({
                        "type": "input_image",
                        "image_url": url,
                        // xAI Responses 的 input_image 要求 detail 字段（缺省时
                        // 部分上游会拒绝）。Anthropic image 块没有 detail 概念，
                        // 与 Grok Build 参考实现一致固定用 auto，交上游自行取舍。
                        "detail": "auto",
                    }));
                } else {
                    return Err(ConversionError::UnsupportedContentBlock(
                        "image 块缺少 file_id 或可识别的 image_url/base64 source".to_string(),
                    ));
                }
            }
            "image_url" => {
                if let Some(url) = image_url_from_block(&block) {
                    validate_image_url(&url)?;
                    content_parts.push(json!({
                        "type": "input_image",
                        "image_url": url,
                        "detail": "auto",
                    }));
                } else {
                    return Err(ConversionError::UnsupportedContentBlock(
                        "image_url 块缺少可用 URL".to_string(),
                    ));
                }
            }
            "document" => {
                if let Some(file_id) = file_id_from_block(&block) {
                    content_parts.push(json!({
                        "type": "input_file",
                        "file_id": file_id,
                    }));
                } else {
                    return Err(ConversionError::UnsupportedContentBlock(
                        "document 仅支持 source.type=file（请先上传 Files API）；base64/URL document 不被静默丢弃"
                            .to_string(),
                    ));
                }
            }
            "text" => {
                if let Some(text) = block.text {
                    if !text.is_empty() {
                        content_parts.push(json!({ "type": "input_text", "text": text }));
                    }
                }
            }
            "web_search_tool_result" => {
                // 历史 server web search 结果：转成可读上下文，避免静默丢弃。
                let summary = value_to_text(&block.content.unwrap_or(Value::Null));
                if !summary.is_empty() {
                    content_parts.push(json!({
                        "type": "input_text",
                        "text": format!("[web_search_tool_result]\n{summary}"),
                    }));
                }
            }
            other => {
                let text = block.text.or(block.thinking).unwrap_or_default();
                if !text.is_empty() {
                    content_parts.push(json!({ "type": "input_text", "text": text }));
                } else {
                    return Err(ConversionError::UnsupportedContentBlock(format!(
                        "user 消息不支持内容类型 {other}"
                    )));
                }
            }
        }
    }
    flush_message(input, "user", &mut content_parts);
    Ok(())
}

fn append_assistant_message(
    input: &mut Vec<Value>,
    content: &Value,
    signature_codec: Option<&ReasoningSignatureCodec>,
) -> Result<(), ConversionError> {
    let blocks = parse_content_blocks(content);
    if blocks.is_empty() {
        let text = value_to_text(content);
        if !text.is_empty() {
            input.push(message_item(
                "assistant",
                vec![json!({ "type": "output_text", "text": text })],
            ));
        }
        return Ok(());
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
            // Claude Code 会原样回传 thinking + signature。只要 xai-rs2 包通过
            // HMAC 解码（decode 内部已强制 backend==responses、结构与大小合法）
            // 即展开回放；旧格式/篡改包 decode 失败，回退为可见文本。
            //
            // 不再按 model 或 credential 过滤：encrypted_content 非账户作用域，
            // 跨凭据 failover 可安全回放；model 亦放宽（同一会话通常同模型，
            // 换模型时上游若拒会以 400+encrypted_content 触发既有重试）。这样
            // 多轮 reasoning / KV-cache 在 failover 与目录漂移下都能保住。
            "thinking" => {
                if let (Some(codec), Some(signature)) =
                    (signature_codec, block.signature.as_deref())
                {
                    if let Some(package) = codec.decode(signature) {
                        flush_message(input, "assistant", &mut content_parts);
                        for item in package_to_input_items(&package) {
                            input.push(item);
                        }
                        continue;
                    }
                }
                if let Some(text) = block.thinking.or(block.text) {
                    if !text.is_empty() {
                        content_parts.push(json!({ "type": "output_text", "text": text }));
                    }
                }
            }
            "server_tool_use" => {
                // 回放为可读文本，避免多轮历史静默丢失。
                flush_message(input, "assistant", &mut content_parts);
                let name = block.name.unwrap_or_else(|| "server_tool".to_string());
                let input_json =
                    serde_json::to_string(&block.input.unwrap_or_else(|| json!({})))
                        .unwrap_or_else(|_| "{}".to_string());
                content_parts.push(json!({
                    "type": "output_text",
                    "text": format!("[server_tool_use name={name}] {input_json}"),
                }));
            }
            "web_search_tool_result" => {
                let summary = value_to_text(&block.content.unwrap_or(Value::Null));
                content_parts.push(json!({
                    "type": "output_text",
                    "text": format!("[web_search_tool_result]\n{summary}"),
                }));
            }
            other => {
                return Err(ConversionError::UnsupportedContentBlock(format!(
                    "assistant 历史不支持内容类型 {other}"
                )));
            }
        }
    }
    flush_message(input, "assistant", &mut content_parts);
    Ok(())
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

/// 将 Anthropic `image` block（base64 或 URL）以及常见 OpenAI 兼容
/// `image_url` block 规范为 xAI 可以接受的 image URL。Grok Build 会把本地
/// 附件归一化成 data URL；代理端没有调用方的本地文件系统，因此只接受已经
/// 可传输的 data URL 或远程 URL。
fn image_url_from_block(block: &ContentBlock) -> Option<String> {
    if let Some(source) = &block.source {
        if let Some(url) = source
            .url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            return Some(url.to_string());
        }
        let data = source.data.trim();
        if !data.is_empty() {
            if data.starts_with("data:") {
                return Some(data.to_string());
            }
            let media_type = source.media_type.trim();
            if !media_type.is_empty() {
                return Some(format!("data:{media_type};base64,{data}"));
            }
        }
    }
    let value = block.image_url.as_ref()?;
    match value {
        Value::String(url) => non_empty_trimmed(url),
        Value::Object(object) => object
            .get("url")
            .and_then(Value::as_str)
            .and_then(non_empty_trimmed),
        _ => None,
    }
}

/// 解码后图片字节的上限（20 MiB）。与 Grok Build 持久化历史的
/// `MAX_LOADED_IMAGE_BYTES` 一致，仅作 sanity 上限，拦掉明显过大的 data URL，
/// 避免把巨型 base64 原样打给上游再被拒/超时。
const MAX_IMAGE_DECODED_BYTES: usize = 20 * 1024 * 1024;

/// 校验一个即将作为 input_image / image_url 发送的图片 URL。
///
/// 仅对**可本地检查**的 `data:` URL 生效：
/// * 拒绝 xAI vision 明确不接受的格式（gif/bmp/tiff）——Grok Build 对这些
///   格式要么转码要么判为 "API-rejected format"；代理无法转码，故给出清晰
///   400，而不是把注定失败的请求打给上游。
/// * 拒绝解码后超过 [`MAX_IMAGE_DECODED_BYTES`] 的负载。
///
/// 远程 http(s) URL 无法本地检查，原样放行、交上游裁决。
fn validate_image_url(url: &str) -> Result<(), ConversionError> {
    let Some(after_data) = url.strip_prefix("data:") else {
        return Ok(());
    };
    let Some(comma) = after_data.find(',') else {
        return Err(ConversionError::UnsupportedContentBlock(
            "data URL 图片缺少 base64 负载分隔符".to_string(),
        ));
    };
    let header = &after_data[..comma];
    let mime = header.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if matches!(mime.as_str(), "image/gif" | "image/bmp" | "image/tiff") {
        return Err(ConversionError::UnsupportedContentBlock(format!(
            "xAI vision 不接受 {mime} 图片，请转换为 PNG/JPEG/WebP"
        )));
    }
    // base64 解码后约为原长度的 3/4；直接用长度估算避免真的解码大字符串。
    let payload_len = after_data[comma + 1..].len();
    if payload_len / 4 * 3 > MAX_IMAGE_DECODED_BYTES {
        return Err(ConversionError::UnsupportedContentBlock(format!(
            "图片超过 {} MiB 上限，请压缩后再发送",
            MAX_IMAGE_DECODED_BYTES / (1024 * 1024)
        )));
    }
    Ok(())
}

/// Anthropic Files API 的 image/document source。
fn file_id_from_block(block: &ContentBlock) -> Option<String> {
    let source = block.source.as_ref()?;
    if source.source_type.trim() != "file" {
        return None;
    }
    source.file_id.as_deref().and_then(non_empty_trimmed)
}

fn request_has_file_inputs(request: &MessagesRequest) -> bool {
    request.messages.iter().any(|message| {
        message.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("source")
                    .and_then(Value::as_object)
                    .and_then(|source| source.get("type"))
                    .and_then(Value::as_str)
                    .is_some_and(|source_type| source_type == "file")
            })
        })
    })
}

/// 请求是否携带图片输入。任何 `image` / `image_url` block（含 base64、远程
/// URL 或 Files API `file_id`）都算，用于在路由阶段排除明确不支持 vision 的
/// 凭据。仅探测存在性，不校验单块内容合法性——那由转换阶段负责。
fn request_has_image_inputs(request: &MessagesRequest) -> bool {
    request.messages.iter().any(|message| {
        message.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("image") | Some("image_url")
                )
            })
        })
    })
}

fn non_empty_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
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
    if is_web_search_tool(tool) {
        let mut converted = json!({ "type": "web_search" });
        if let Some(domains) = tool
            .allowed_domains
            .as_ref()
            .map(|domains| {
                domains
                    .iter()
                    .map(|domain| domain.trim())
                    .filter(|domain| !domain.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|domains| !domains.is_empty())
        {
            // 与 Grok Build `HostedTool::WebSearch` 完全相同的 wire shape。
            converted["filters"] = json!({ "allowed_domains": domains });
        }
        return Some(converted);
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

/// Grok Build 会让 hosted `web_search` 胜过同名 function，避免 xAI
/// Responses 拒绝 `Duplicate tool names: web_search`。显式声明
/// `type:web_search_*` 的 hosted tool 始终优先；没有显式声明时，Claude Code
/// 的普通 `WebSearch` / `web_search` function 会提升为一个 hosted tool。
fn convert_tools(tools: &[Tool], converter: fn(&Tool) -> Option<Value>) -> Vec<Value> {
    let preferred_web_search = tools
        .iter()
        .position(is_declared_web_search_tool)
        .or_else(|| tools.iter().position(is_named_web_search_tool));
    tools
        .iter()
        .enumerate()
        .filter_map(|(index, tool)| {
            if is_web_search_tool(tool) && Some(index) != preferred_web_search {
                return None;
            }
            converter(tool)
        })
        .collect()
}

fn is_web_search_tool(tool: &Tool) -> bool {
    is_declared_web_search_tool(tool) || is_named_web_search_tool(tool)
}

fn is_declared_web_search_tool(tool: &Tool) -> bool {
    tool.tool_type
        .as_deref()
        .is_some_and(|tool_type| tool_type.starts_with("web_search"))
}

fn is_named_web_search_tool(tool: &Tool) -> bool {
    is_web_search_tool_name(&tool.name)
}

fn is_web_search_tool_name(name: &str) -> bool {
    matches!(name, "WebSearch" | "web_search")
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
            .map(|name| {
                if is_web_search_tool_name(name) {
                    json!({ "type": "web_search" })
                } else {
                    json!({ "type": "function", "name": name })
                }
            }),
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
                allowed_domains: None,
            }]),
            tool_choice: None,
            thinking: Some(Thinking {
                thinking_type: "enabled".to_string(),
                budget_tokens: 10_000,
            }),
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        assert_eq!(converted.model, "grok-4.5");
        assert_eq!(converted.body["input"][0]["type"], "function_call");
        assert_eq!(converted.body["input"][1]["type"], "function_call_output");
        assert_eq!(converted.body["tools"][0]["type"], "function");
        assert!(converted.body.get("reasoning").is_some());
    }

    #[test]
    fn converts_web_search_to_build_hosted_tool_with_domain_filters() {
        let request = web_search_request();

        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        let tools = converted.body["tools"].as_array().unwrap();
        assert!(converted.uses_hosted_web_search);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(
            tools[0]["filters"]["allowed_domains"],
            json!(["docs.rs", "example.com"])
        );
    }

    #[test]
    fn promotes_claude_code_web_search_function_names_to_hosted_tool() {
        for name in ["WebSearch", "web_search"] {
            let request = named_web_search_request(name);
            let plan = plan_request(&request, "grok-4.5", None).unwrap();
            assert!(
                plan.needs_web_search,
                "{name} must constrain routing to Responses"
            );
            assert_eq!(plan.backend_constraint(), Some(GrokApiBackend::Responses));

            let converted = convert_request(&request, "grok-4.5", None).unwrap();
            assert_eq!(converted.backend, GrokApiBackend::Responses);
            assert!(converted.uses_hosted_web_search);
            assert_eq!(
                converted.body["tools"],
                json!([{ "type": "web_search" }])
            );
            assert_eq!(
                converted.body["tool_choice"],
                json!({ "type": "web_search" })
            );
        }
    }

    #[test]
    fn web_search_function_name_matching_is_exact() {
        let request = named_web_search_request("websearch");
        let plan = plan_request(&request, "grok-4.5", None).unwrap();
        assert!(!plan.needs_web_search);

        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        assert!(!converted.uses_hosted_web_search);
        assert_eq!(converted.body["tools"][0]["type"], "function");
        assert_eq!(converted.body["tools"][0]["name"], "websearch");
        assert_eq!(
            converted.body["tool_choice"],
            json!({ "type": "function", "name": "websearch" })
        );
    }

    #[test]
    fn plan_request_detects_web_search_and_files_without_building_body() {
        let mut request = web_search_request();
        request.messages[0].content = json!([
            {"type":"document","source":{"type":"file","file_id":"file_1"}},
            {"type":"text","text":"summarize"}
        ]);
        let plan = plan_request(&request, "grok-4.5", None).unwrap();
        assert_eq!(plan.model, "grok-4.5");
        assert!(plan.needs_web_search);
        assert!(plan.needs_files);
        assert_eq!(plan.backend_constraint(), Some(GrokApiBackend::Responses));
    }

    #[test]
    fn convert_uses_credential_catalog_backend_not_bootstrap_default() {
        // 公共 api.x.ai 目录常缺 apiBackend → Chat Completions；若误用
        // bootstrap/并集的 Responses 默认会把 body 建错。
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"chat_completions",
                "supportsReasoningEffort":true,
                "reasoningEfforts":["low","medium","high"]
            }]}),
            "https://api.x.ai/v1",
        );
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 128,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("hi"),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let converted = convert_request(&request, "grok-4.5", Some(&catalog)).unwrap();
        assert_eq!(converted.backend, GrokApiBackend::ChatCompletions);
        assert!(converted.body.get("messages").is_some());
        assert!(converted.body.get("input").is_none());
        assert_eq!(converted.body["max_tokens"], 128);
        assert!(converted.body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn rejects_base64_document_instead_of_silent_drop() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 64,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([{
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": "AAAA"
                    }
                }]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let error = convert_request(&request, "grok-4.5", None)
            .expect_err("base64 document must not be dropped")
            .to_string();
        assert!(error.contains("document") || error.contains("不支持"));
    }

    #[test]
    fn rejects_hosted_web_search_when_catalog_does_not_support_it() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"responses",
                "supportsBackendSearch":false
            }]}),
            "https://api.x.ai/v1",
        );
        let error = convert_request(&web_search_request(), "grok-4.5", Some(&catalog))
            .unwrap_err()
            .to_string();
        assert!(error.contains("supportsBackendSearch"));
    }

    #[test]
    fn allows_hosted_web_search_when_catalog_capability_is_missing() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"responses"
            }]}),
            "https://api.x.ai/v1",
        );
        let converted =
            convert_request(&web_search_request(), "grok-4.5", Some(&catalog)).unwrap();
        assert_eq!(converted.backend, GrokApiBackend::Responses);
        assert!(converted.uses_hosted_web_search);
        assert_eq!(converted.body["tools"][0]["type"], "web_search");
    }

    #[test]
    fn rejects_hosted_web_search_for_chat_completions_backend() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"chat_completions",
                "supportsBackendSearch":true
            }]}),
            "https://api.x.ai/v1",
        );
        let error = convert_request(&web_search_request(), "grok-4.5", Some(&catalog))
            .unwrap_err()
            .to_string();
        assert!(error.contains("chat_completions"));
    }

    #[test]
    fn rejects_named_web_search_for_messages_backend() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"messages",
                "supportsBackendSearch":true
            }]}),
            "https://api.x.ai/v1",
        );
        let error = convert_request(
            &named_web_search_request("WebSearch"),
            "grok-4.5",
            Some(&catalog),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("messages"));
        assert!(error.contains("Responses"));
    }

    #[test]
    fn keeps_only_one_hosted_web_search_definition() {
        let mut request = web_search_request();
        request.tools.as_mut().unwrap().push(Tool {
            tool_type: Some("web_search_20250305".to_string()),
            name: "web_search".to_string(),
            description: String::new(),
            input_schema: Default::default(),
            max_uses: None,
            allowed_domains: Some(vec!["second.example".to_string()]),
        });
        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        let tools = converted.body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0]["filters"]["allowed_domains"],
            json!(["docs.rs", "example.com"])
        );
    }

    fn web_search_request() -> MessagesRequest {
        MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: true,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("search the docs"),
            }],
            tools: Some(vec![
                // 即使普通 function 排在前面，显式 hosted tool 仍必须胜出。
                Tool {
                    tool_type: None,
                    name: "web_search".to_string(),
                    description: "incorrect function duplicate".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    allowed_domains: None,
                },
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                    allowed_domains: Some(vec![
                        "docs.rs".to_string(),
                        " ".to_string(),
                        "example.com".to_string(),
                    ]),
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        }
    }

    fn named_web_search_request(name: &str) -> MessagesRequest {
        MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: true,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("search the docs"),
            }],
            tools: Some(vec![Tool {
                tool_type: None,
                name: name.to_string(),
                description: "Search the web".to_string(),
                input_schema: serde_json::from_value(json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "allowed_domains": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "blocked_domains": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }))
                .unwrap(),
                max_uses: None,
                allowed_domains: None,
            }]),
            tool_choice: Some(json!({ "type": "tool", "name": name })),
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        }
    }

    #[test]
    fn converts_anthropic_base64_and_url_images_without_rewriting_urls() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {"type":"text","text":"describe these"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}},
                    {"type":"image","source":{"type":"url","url":"https://example.com/photo.png"}},
                    {"type":"image_url","image_url":{"url":"https://example.com/openai-shape.jpg"}}
                ]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };

        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        let content = converted.body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AA==");
        assert_eq!(content[2]["image_url"], "https://example.com/photo.png");
        assert_eq!(
            content[3]["image_url"],
            "https://example.com/openai-shape.jpg"
        );
    }

    #[test]
    fn converts_anthropic_file_sources_to_xai_input_files() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {"type":"text","text":"compare these files"},
                    {"type":"image","source":{"type":"file","file_id":"file_image"}},
                    {"type":"document","source":{"type":"file","file_id":"file_document"}}
                ]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };

        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        let content = converted.body["input"][0]["content"].as_array().unwrap();
        assert_eq!(
            content[1],
            json!({"type":"input_file","file_id":"file_image"})
        );
        assert_eq!(
            content[2],
            json!({"type":"input_file","file_id":"file_document"})
        );
    }

    #[test]
    fn input_image_carries_detail_auto_for_base64_and_url_sources() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {"type":"text","text":"describe"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}},
                    {"type":"image_url","image_url":{"url":"https://example.com/a.png"}}
                ]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };

        let converted = convert_request(&request, "grok-4.5", None).unwrap();
        let content = converted.body["input"][0]["content"].as_array().unwrap();
        // base64 source → data URL input_image，必须带 detail=auto。
        assert_eq!(
            content[1],
            json!({
                "type":"input_image",
                "image_url":"data:image/png;base64,iVBOR",
                "detail":"auto"
            })
        );
        // image_url 兼容块同样带 detail=auto。
        assert_eq!(
            content[2],
            json!({
                "type":"input_image",
                "image_url":"https://example.com/a.png",
                "detail":"auto"
            })
        );
    }

    #[test]
    fn validate_image_url_rejects_gif_and_oversize_but_allows_png_and_remote() {
        // xAI vision 不接受 gif → 明确拒绝。
        assert!(validate_image_url("data:image/gif;base64,R0lGODdh").is_err());
        // bmp/tiff 同样拒绝。
        assert!(validate_image_url("data:image/bmp;base64,Qk0=").is_err());
        // png data URL 放行。
        assert!(validate_image_url("data:image/png;base64,iVBORw0KGgo=").is_ok());
        // 远程 URL 无法本地检查，放行交上游裁决。
        assert!(validate_image_url("https://example.com/a.gif").is_ok());
        // 超过 20 MiB 解码上限 → 拒绝（base64 长度约为解码后的 4/3）。
        let huge_payload = "A".repeat(28 * 1024 * 1024);
        let huge = format!("data:image/png;base64,{huge_payload}");
        assert!(validate_image_url(&huge).is_err());
        // 刚好在上限内的小图放行。
        assert!(validate_image_url("data:image/jpeg;base64,/9j/4AAQ").is_ok());
    }

    #[test]
    fn converts_rejects_gif_image_block_with_clear_error() {
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 128,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {"type":"image","source":{"type":"base64","media_type":"image/gif","data":"R0lGODdh"}}
                ]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let err = convert_request(&request, "grok-4.5", None).unwrap_err();
        assert!(matches!(err, ConversionError::UnsupportedContentBlock(_)));
    }

    #[test]
    fn rejects_file_sources_for_non_responses_catalog_models() {
        let request = MessagesRequest {
            model: "grok-chat".to_string(),
            max_tokens: 1024,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!([
                    {"type":"document","source":{"type":"file","file_id":"file_document"}}
                ]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{"model":"grok-chat","apiBackend":"chat_completions"}]}),
            "https://api.x.ai/v1",
        );

        assert!(matches!(
            convert_request(&request, "grok-4.5", Some(&catalog)),
            Err(ConversionError::FilesRequireResponses(model)) if model == "grok-chat"
        ));
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
    fn responses_body_includes_encrypted_reasoning_and_replays_signature_items() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let signature = codec
            .encode(
                "grok-4.5",
                Some(3),
                &[json!({
                "type": "reasoning",
                "id": "rs_1",
                "status": "completed",
                "summary": [{"type":"summary_text","text":"plan"}],
                "encrypted_content": "enc_blob",
                })],
            )
            .unwrap();
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 128,
            stream: false,
            system: None,
            messages: vec![
                Message {
                    role: "assistant".to_string(),
                    content: json!([{
                        "type": "thinking",
                        "thinking": "plan",
                        "signature": signature,
                    }, {
                        "type": "text",
                        "text": "done",
                    }]),
                },
                Message {
                    role: "user".to_string(),
                    content: json!("continue"),
                },
            ],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let converted =
            convert_request_for_credential(&request, "grok-4.5", None, Some(&codec)).unwrap();
        assert_eq!(
            converted.body["include"],
            json!(["reasoning.encrypted_content"])
        );
        let input = converted.body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "enc_blob");
        assert!(input[0].get("status").is_none());
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "done");
        assert_eq!(input[2]["role"], "user");
    }

    #[test]
    fn cross_model_reasoning_signature_still_replays() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        // 签发时 model=grok-4.5，本轮换到 grok-4.6：model gate 已放宽，只要
        // HMAC 解码通过就回放 reasoning（不再降级成纯文本）。若上游因跨模型
        // 拒收，会以 400+encrypted_content 触发既有重试，而不是这里前置降级。
        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let signature = codec
            .encode(
                "grok-4.5",
                Some(1),
                &[json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "enc_blob",
                })],
            )
            .unwrap();
        let request = MessagesRequest {
            model: "grok-4.6".to_string(),
            max_tokens: 64,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: json!([{
                    "type": "thinking",
                    "thinking": "visible plan",
                    "signature": signature,
                }]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let converted =
            convert_request_for_credential(&request, "grok-4.5", None, Some(&codec)).unwrap();
        let input = converted.body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "enc_blob");
    }

    #[test]
    fn tampered_reasoning_signature_falls_back_to_thinking_text() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        // 用不同密钥签发 → 本 codec HMAC 校验失败，decode 返回 None，回退为
        // 可见文本（旧格式 / 篡改包同理）。
        let signer = ReasoningSignatureCodec::new(b"other-secret");
        let signature = signer
            .encode(
                "grok-4.5",
                Some(1),
                &[json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "enc_blob",
                })],
            )
            .unwrap();
        let verifier = ReasoningSignatureCodec::new(b"server-secret");
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 64,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: json!([{
                    "type": "thinking",
                    "thinking": "visible plan",
                    "signature": signature,
                }]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        let converted =
            convert_request_for_credential(&request, "grok-4.5", None, Some(&verifier)).unwrap();
        let input = converted.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "visible plan");
    }

    #[test]
    fn cross_credential_reasoning_signature_still_replays_after_failover() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        // 签发时 credential=1，但本轮 failover 到别的账号：credential 不再是
        // 门槛（encrypted_content 非账户作用域），reasoning 仍原样回放，保住
        // 多轮 KV-cache，而不是降级成纯文本。
        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let signature = codec
            .encode(
                "grok-4.5",
                Some(1),
                &[json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "enc_blob",
                })],
            )
            .unwrap();
        let request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 64,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "assistant".to_string(),
                content: json!([{
                    "type": "thinking",
                    "thinking": "visible plan",
                    "signature": signature,
                }]),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: None,
            top_p: None,
        };
        // signature_codec 是服务端单例；同一 codec 解码即视为通过 HMAC。这里
        // 不传任何 credential，等价于 failover 到与签发账号不同的凭据。
        let converted =
            convert_request_for_credential(&request, "grok-4.5", None, Some(&codec)).unwrap();
        let input = converted.body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "enc_blob");
    }

    #[test]
    fn multiple_reasoning_signatures_replay_around_hosted_tool_history_in_order() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let first = codec
            .encode(
                "grok-4.5",
                Some(3),
                &[json!({
                    "type":"reasoning",
                    "id":"rs_1",
                    "encrypted_content":"enc_1"
                })],
            )
            .unwrap();
        let second = codec
            .encode(
                "grok-4.5",
                Some(3),
                &[json!({
                    "type":"reasoning",
                    "id":"tco_2",
                    "encrypted_content":"enc_2"
                })],
            )
            .unwrap();
        let request: MessagesRequest = serde_json::from_value(json!({
            "model":"grok-4.5",
            "max_tokens":128,
            "messages":[{
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"first","signature":first},
                    {"type":"server_tool_use","id":"ws_1","name":"web_search","input":{"query":"rust"}},
                    {"type":"web_search_tool_result","content":[]},
                    {"type":"thinking","thinking":"second","signature":second},
                    {"type":"text","text":"answer"}
                ]
            }]
        }))
        .unwrap();
        let converted =
            convert_request_for_credential(&request, "grok-4.5", None, Some(&codec)).unwrap();
        let input = converted.body["input"].as_array().unwrap();
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[1]["type"], "message");
        assert!(input[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("server_tool_use"));
        assert_eq!(input[2]["id"], "tco_2");
        assert_eq!(input[3]["content"][0]["text"], "answer");
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
            temperature: None,
            top_p: None,
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
            temperature: None,
            top_p: None,
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

    #[test]
    fn responses_and_chat_forward_temperature_and_top_p() {
        let mut request = MessagesRequest {
            model: "grok-4.5".to_string(),
            max_tokens: 128,
            stream: false,
            system: None,
            messages: vec![Message {
                role: "user".to_string(),
                content: json!("hi"),
            }],
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
            temperature: Some(0.4),
            top_p: Some(0.9),
        };
        let responses = convert_request(&request, "grok-4.5", None).unwrap();
        assert_eq!(responses.body["temperature"], 0.4);
        assert_eq!(responses.body["top_p"], 0.9);

        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{
                "model":"grok-4.5",
                "apiBackend":"chat_completions"
            }]}),
            "https://api.x.ai/v1",
        );
        let chat = convert_request(&request, "grok-4.5", Some(&catalog)).unwrap();
        assert_eq!(chat.backend, GrokApiBackend::ChatCompletions);
        assert_eq!(chat.body["temperature"], 0.4);
        assert_eq!(chat.body["top_p"], 0.9);

        request.temperature = None;
        request.top_p = None;
        let omitted = convert_request(&request, "grok-4.5", None).unwrap();
        assert!(omitted.body.get("temperature").is_none());
        assert!(omitted.body.get("top_p").is_none());
    }
}
