//! xAI Responses SSE → Anthropic SSE 转换。

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::{SseEvent, SseStateManager};

use super::reasoning_sig::{ReasoningSignatureCodec, extract_reasoning_item};

const MAX_SSE_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_SEPARATOR_BYTES: usize = 4;

/// 按字节缓冲的严格 SSE 解码器：跨 chunk 保留不完整 UTF-8，兼容 LF、
/// CRLF、CR 及其混合空行，并对单个未终止 frame 设置硬上限。
#[derive(Default)]
pub struct XaiSseDecoder {
    buffer: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XaiSseDecodeError {
    FrameTooLarge { limit: usize },
    InvalidUtf8(String),
    InvalidJson { error: String, data: String },
}

impl std::fmt::Display for XaiSseDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameTooLarge { limit } => {
                write!(formatter, "xAI SSE 单个事件超过 {limit} 字节上限")
            }
            Self::InvalidUtf8(error) => write!(formatter, "xAI SSE 包含非法 UTF-8: {error}"),
            Self::InvalidJson { error, data } => {
                write!(formatter, "xAI SSE 事件 JSON 无效: {error}; data={data}")
            }
        }
    }
}

impl std::error::Error for XaiSseDecodeError {}

impl XaiSseDecoder {
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Value>, XaiSseDecodeError> {
        let mut events = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            let max_buffer = MAX_SSE_FRAME_BYTES + MAX_SSE_SEPARATOR_BYTES;
            let room = max_buffer.saturating_sub(self.buffer.len());
            if room == 0 {
                self.buffer.clear();
                return Err(XaiSseDecodeError::FrameTooLarge {
                    limit: MAX_SSE_FRAME_BYTES,
                });
            }
            let take = room.min(chunk.len() - offset);
            self.buffer.extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
            self.drain_complete_events(&mut events)?;
            if self.buffer.len() == max_buffer {
                self.buffer.clear();
                return Err(XaiSseDecodeError::FrameTooLarge {
                    limit: MAX_SSE_FRAME_BYTES,
                });
            }
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<Value>, XaiSseDecodeError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }
        if self.buffer.len() > MAX_SSE_FRAME_BYTES {
            self.buffer.clear();
            return Err(XaiSseDecodeError::FrameTooLarge {
                limit: MAX_SSE_FRAME_BYTES,
            });
        }
        let block_bytes = std::mem::take(&mut self.buffer);
        Ok(parse_sse_block(&block_bytes)?.into_iter().collect())
    }

    fn drain_complete_events(&mut self, events: &mut Vec<Value>) -> Result<(), XaiSseDecodeError> {
        while let Some((sep_at, sep_len)) = find_sse_event_separator(&self.buffer) {
            if sep_at > MAX_SSE_FRAME_BYTES {
                self.buffer.clear();
                return Err(XaiSseDecodeError::FrameTooLarge {
                    limit: MAX_SSE_FRAME_BYTES,
                });
            }
            let block_bytes = self.buffer[..sep_at].to_vec();
            self.buffer.drain(..sep_at + sep_len);
            match parse_sse_block(&block_bytes) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(error) => {
                    self.buffer.clear();
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

/// 返回 SSE 空行分隔符的起始下标与长度。两次 line ending 可以分别是
/// LF、CRLF 或 CR，因此也接受 `\r\n\n`、`\n\r\n` 等混合形式。
fn find_sse_event_separator(buffer: &[u8]) -> Option<(usize, usize)> {
    for index in 0..buffer.len() {
        let Some(first_len) = sse_line_ending_len(buffer, index) else {
            continue;
        };
        let second_at = index + first_len;
        if let Some(second_len) = sse_line_ending_len(buffer, second_at) {
            return Some((index, first_len + second_len));
        }
    }
    None
}

fn sse_line_ending_len(buffer: &[u8], index: usize) -> Option<usize> {
    match buffer.get(index).copied()? {
        b'\n' => Some(1),
        b'\r' if buffer.get(index + 1) == Some(&b'\n') => Some(2),
        // chunk 末尾的 CR 可能是下一 chunk 中 CRLF 的前半段；等待一个
        // 字节再决定，避免把 `\n\r\n` 过早拆成 `\n\r` + `\n`。
        b'\r' if index + 1 == buffer.len() => None,
        b'\r' => Some(1),
        _ => None,
    }
}

fn parse_sse_block(block: &[u8]) -> Result<Option<Value>, XaiSseDecodeError> {
    let block = std::str::from_utf8(block)
        .map_err(|error| XaiSseDecodeError::InvalidUtf8(error.to_string()))?;
    let normalized = block.replace("\r\n", "\n").replace('\r', "\n");
    let data = normalized
        .lines()
        .filter_map(|line| {
            if line == "data" {
                Some("")
            } else {
                line.strip_prefix("data:")
                    .map(|value| value.strip_prefix(' ').unwrap_or(value))
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Ok(None);
    }
    // Chat Completions 以 `[DONE]` 结束，而 Responses 以
    // `response.completed` 结束。保留一个内部完成标记，非流式聚合可以同时
    // 验证两种 backend 的终止语义。
    if data == "[DONE]" {
        return Ok(Some(json!({ "type": "done" })));
    }
    serde_json::from_str(&data)
        .map(Some)
        .map_err(|error| XaiSseDecodeError::InvalidJson {
            error: error.to_string(),
            data: truncate(&data).to_string(),
        })
}

/// Messages backend 已经返回 Anthropic wire protocol，因此 handler 会原样
/// 转发字节。这个观察器只旁路检查完整 SSE frame，用于判断凭据健康状态，
/// 不会重新编码或改变发给客户端的内容。
#[derive(Default)]
pub struct AnthropicSseObserver {
    buffer: Vec<u8>,
    terminal: bool,
    failure: Option<AnthropicSseFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicSseFailure {
    /// 上游明确发送了 Anthropic error event；原始事件已经透传给客户端。
    Upstream(String),
    /// SSE framing / UTF-8 / JSON 损坏，需要代理补发一个 error event。
    Protocol(String),
}

impl AnthropicSseObserver {
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.failure.is_some() || self.terminal {
            return;
        }
        let mut offset = 0;
        while offset < chunk.len() {
            let max_buffer = MAX_SSE_FRAME_BYTES + MAX_SSE_SEPARATOR_BYTES;
            let room = max_buffer.saturating_sub(self.buffer.len());
            if room == 0 {
                self.fail_protocol("xAI Messages SSE 单个事件超过 4 MiB");
                return;
            }
            let take = room.min(chunk.len() - offset);
            self.buffer.extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
            while let Some((sep_at, sep_len)) = find_sse_event_separator(&self.buffer) {
                if sep_at > MAX_SSE_FRAME_BYTES {
                    self.fail_protocol("xAI Messages SSE 单个事件超过 4 MiB");
                    return;
                }
                let block = self.buffer[..sep_at].to_vec();
                self.buffer.drain(..sep_at + sep_len);
                self.observe_block(&block);
                if self.failure.is_some() || self.terminal {
                    return;
                }
            }
            if self.buffer.len() == max_buffer {
                self.fail_protocol("xAI Messages SSE 未终止事件超过 4 MiB");
                return;
            }
        }
    }

    pub fn finish(&mut self) {
        if self.failure.is_some() || self.terminal || self.buffer.is_empty() {
            return;
        }
        if self.buffer.len() > MAX_SSE_FRAME_BYTES {
            self.fail_protocol("xAI Messages SSE 未终止事件超过 4 MiB");
            return;
        }
        let block = std::mem::take(&mut self.buffer);
        self.observe_block(&block);
    }

    pub fn terminal(&self) -> bool {
        self.terminal
    }

    pub fn failure(&self) -> Option<&AnthropicSseFailure> {
        self.failure.as_ref()
    }

    fn observe_block(&mut self, block: &[u8]) {
        let block = match std::str::from_utf8(block) {
            Ok(block) => block,
            Err(error) => {
                self.fail_protocol(format!("xAI Messages SSE 包含非法 UTF-8: {error}"));
                return;
            }
        };
        let normalized = block.replace("\r\n", "\n").replace('\r', "\n");
        let mut event_name = None;
        let mut data_lines = Vec::new();
        for line in normalized.lines() {
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event_name = Some(value),
                "data" => data_lines.push(value),
                _ => {}
            }
        }

        let data = data_lines.join("\n");
        if event_name == Some("error") {
            let message = serde_json::from_str::<Value>(&data)
                .ok()
                .and_then(|value| anthropic_error_message(&value))
                .unwrap_or_else(|| {
                    (!data.trim().is_empty())
                        .then(|| truncate(&data).to_string())
                        .unwrap_or_else(|| "xAI Messages 上游返回 event:error".to_string())
                });
            self.failure = Some(AnthropicSseFailure::Upstream(message));
            return;
        }
        if data.is_empty() {
            if event_name == Some("message_stop") {
                self.terminal = true;
            }
            return;
        }
        let value = match serde_json::from_str::<Value>(&data) {
            Ok(value) => value,
            Err(error) => {
                self.fail_protocol(format!(
                    "xAI Messages SSE 事件 JSON 无效: {error}; data={}",
                    truncate(&data)
                ));
                return;
            }
        };
        if value.get("type").and_then(Value::as_str) == Some("error") {
            self.failure = Some(AnthropicSseFailure::Upstream(
                anthropic_error_message(&value)
                    .unwrap_or_else(|| "xAI Messages 上游返回错误事件".to_string()),
            ));
            return;
        }
        if event_name == Some("message_stop")
            || value.get("type").and_then(Value::as_str) == Some("message_stop")
        {
            self.terminal = true;
        }
    }

    fn fail_protocol(&mut self, message: impl Into<String>) {
        self.buffer.clear();
        self.failure = Some(AnthropicSseFailure::Protocol(message.into()));
    }
}

fn anthropic_error_message(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error").filter(|error| error.is_string()))
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
struct ToolBlock {
    index: i32,
    id: String,
    name: String,
    arguments: String,
    stopped: bool,
}

/// xAI Responses 的 hosted `web_search` 调用。它由服务端执行，不能转换成
/// 需要客户端回填结果的 Anthropic `tool_use`；应使用 Anthropic 的
/// `server_tool_use` + `web_search_tool_result` 成对内容块。
#[derive(Debug)]
struct WebSearchBlock {
    index: i32,
    id: String,
    input: Value,
    results: Vec<Value>,
    stopped: bool,
    result_emitted: bool,
}

/// 非流式 Anthropic `content` 组装顺序，镜像上游 `response.output` 交错顺序。
#[derive(Debug, Clone, PartialEq, Eq)]
enum OrderedBlock {
    Thinking(usize),
    Text(usize),
    WebSearch(String),
    Tool(String),
}

#[derive(Debug)]
struct TextContentBlock {
    index: i32,
    text: String,
}

#[derive(Debug)]
struct ThinkingContentBlock {
    index: i32,
    thinking: String,
    reasoning_items: Vec<Value>,
    signature_emitted: bool,
}

/// 同时驱动流式事件和非流式聚合的状态机。
pub struct GrokStreamContext {
    state: SseStateManager,
    model: String,
    message_id: String,
    requested_input_tokens: i32,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    thinking_enabled: bool,
    /// 签发 xai-rs2 signature 时写入，供 Claude Code 多轮回传后做凭据校验。
    credential_id: Option<u64>,
    signature_codec: Option<ReasoningSignatureCodec>,
    active_text_block: Option<usize>,
    active_thinking_block: Option<usize>,
    text_blocks: Vec<TextContentBlock>,
    thinking_blocks: Vec<ThinkingContentBlock>,
    tool_blocks: HashMap<String, ToolBlock>,
    tool_aliases: HashMap<String, String>,
    tool_order: Vec<String>,
    web_search_blocks: HashMap<String, WebSearchBlock>,
    web_search_order: Vec<String>,
    /// 内容块出现顺序（thinking / web_search / text / tools 交错）。
    content_order: Vec<OrderedBlock>,
    /// 已完整消费的 Responses output item，防止 `response.completed` 权威快照
    /// 与先前 output_item.done 重复生成内容块。
    completed_item_keys: HashSet<String>,
    /// reasoning item 尚无 encrypted_content 时，暂缓后续输出，等待 done 或
    /// terminal response 补齐后再按原顺序发出。
    awaiting_reasoning_terminal: bool,
    deferred_events: Vec<Value>,
    stop_reason: Option<String>,
    completed: bool,
}

impl GrokStreamContext {
    pub fn new(model: impl Into<String>, input_tokens: i32, thinking_enabled: bool) -> Self {
        Self {
            state: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            requested_input_tokens: input_tokens,
            input_tokens: None,
            output_tokens: None,
            thinking_enabled,
            credential_id: None,
            signature_codec: None,
            active_text_block: None,
            active_thinking_block: None,
            text_blocks: Vec::new(),
            thinking_blocks: Vec::new(),
            tool_blocks: HashMap::new(),
            tool_aliases: HashMap::new(),
            tool_order: Vec::new(),
            web_search_blocks: HashMap::new(),
            web_search_order: Vec::new(),
            content_order: Vec::new(),
            completed_item_keys: HashSet::new(),
            awaiting_reasoning_terminal: false,
            deferred_events: Vec::new(),
            stop_reason: None,
            completed: false,
        }
    }

    fn note_content_order(&mut self, entry: OrderedBlock) {
        self.content_order.push(entry);
    }

    /// 记录实际上游凭据，用于打包 `thinking.signature`。
    pub fn set_credential_id(&mut self, credential_id: u64) {
        self.credential_id = Some(credential_id);
    }

    pub fn set_signature_codec(&mut self, codec: ReasoningSignatureCodec) {
        self.signature_codec = Some(codec);
    }

    pub fn initial_events(&mut self) -> Vec<SseEvent> {
        self.state
            .handle_message_start(json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": { "input_tokens": self.requested_input_tokens, "output_tokens": 0 }
                }
            }))
            .into_iter()
            .collect()
    }

    pub fn process_event(&mut self, event: &Value) -> Vec<SseEvent> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let supplies_reasoning_item = event_type == "response.output_item.done"
            && event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning");
        let terminal = matches!(
            event_type,
            "response.completed" | "response.incomplete" | "response.failed" | "error"
        );
        if self.awaiting_reasoning_terminal && !supplies_reasoning_item && !terminal {
            self.deferred_events.push(event.clone());
            return Vec::new();
        }

        let mut events = self.process_event_now(event);
        if !self.awaiting_reasoning_terminal && !self.deferred_events.is_empty() {
            let deferred = std::mem::take(&mut self.deferred_events);
            for event in deferred {
                events.extend(self.process_event(&event));
            }
        }
        events
    }

    fn process_event_now(&mut self, event: &Value) -> Vec<SseEvent> {
        if event.get("choices").is_some() {
            return self.process_chat_completion_event(event);
        }
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "response.output_text.delta" => self.process_text_delta(event),
            "response.output_text.done" => self.process_text_done(event),
            "response.output_item.added" => self.process_output_item_added(event),
            "response.output_item.done" => self.process_output_item_done(event),
            "response.function_call_arguments.delta" => self.process_tool_delta(event),
            "response.function_call_arguments.done" => self.process_tool_done(event),
            "response.completed" => {
                self.deferred_events.clear();
                self.ingest_completed_response(event.get("response").unwrap_or(event))
            }
            "done" => {
                self.completed = true;
                Vec::new()
            }
            "response.incomplete" => {
                self.deferred_events.clear();
                let events = self.ingest_completed_response(event.get("response").unwrap_or(event));
                self.stop_reason = Some("max_tokens".to_string());
                events
            }
            "response.failed" | "error" => {
                self.deferred_events.clear();
                self.stop_reason = Some("end_turn".to_string());
                Vec::new()
            }
            _ if event_type.contains("reasoning") && event_type.ends_with(".delta") => {
                self.process_thinking_delta(event)
            }
            _ if event_type.contains("reasoning") && event_type.ends_with(".done") => {
                self.process_thinking_done(event)
            }
            _ => Vec::new(),
        }
    }

    /// OpenAI Chat Completions SSE → 与 Responses 共用的 Anthropic 状态机。
    /// Grok Build 对 catalog 中未标 `apiBackend` 的模型会走这一分支；reasoning
    /// 内容在 xAI/OpenAI 兼容流中通常位于 `delta.reasoning_content`。
    fn process_chat_completion_event(&mut self, event: &Value) -> Vec<SseEvent> {
        if event.get("usage").is_some() {
            self.capture_response_metadata(event);
        }
        let mut events = Vec::new();
        for choice in event
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(delta) = choice.get("delta") {
                events.extend(self.process_chat_message_delta(delta, false));
            }
            if let Some(message) = choice.get("message") {
                events.extend(self.process_chat_message_delta(message, true));
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if reason == "length" {
                    self.stop_reason = Some("max_tokens".to_string());
                }
                events.extend(self.stop_open_tools());
                // 非流式 Chat Completions JSON 没有 `[DONE]`；有 finish_reason
                // 即足以代表一个完整的 assistant turn。
                self.completed = true;
            }
        }
        if event.get("object").and_then(Value::as_str) == Some("chat.completion") {
            self.completed = true;
            events.extend(self.stop_open_tools());
        }
        events
    }

    fn process_chat_message_delta(&mut self, delta: &Value, terminal: bool) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for reasoning in chat_text_values(delta, &["reasoning_content", "reasoning"]) {
            events.extend(self.process_thinking_delta(&json!({ "delta": reasoning })));
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details {
                if let Some(text) = detail
                    .get("text")
                    .or_else(|| detail.get("content"))
                    .and_then(Value::as_str)
                {
                    events.extend(self.process_thinking_delta(&json!({ "delta": text })));
                }
            }
        }
        for text in chat_text_values(delta, &["content"]) {
            events.extend(self.process_text_delta(&json!({ "delta": text })));
        }
        for (position, tool_call) in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let index = tool_call
                .get("index")
                .and_then(Value::as_i64)
                .unwrap_or(position as i64);
            let fallback_key = format!("chat_tool_{index}");
            let raw_key = tool_call
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(&fallback_key);
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(raw_key);
            let function = tool_call.get("function").unwrap_or(&Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !name.is_empty() {
                events.extend(self.ensure_tool_block(raw_key, id, name));
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                events.extend(self.append_tool_arguments(raw_key, arguments));
            }
            if terminal {
                events.extend(self.stop_tool(raw_key));
            }
        }
        events
    }

    fn process_text_delta(&mut self, event: &Value) -> Vec<SseEvent> {
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Vec::new();
        }
        let (block_position, index, mut events) = self.ensure_text_block();
        self.text_blocks[block_position].text.push_str(delta);
        events.extend(
            self.state
                .handle_content_block_delta(
                    index,
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": delta }
                    }),
                )
                .into_iter(),
        );
        events
    }

    fn process_text_done(&mut self, event: &Value) -> Vec<SseEvent> {
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let existing = self
            .active_text_block
            .and_then(|position| self.text_blocks.get(position))
            .map(|block| block.text.as_str())
            .unwrap_or_default();
        let suffix = suffix_after(existing, text);
        if suffix.is_empty() {
            return Vec::new();
        }
        self.process_text_delta(&json!({ "delta": suffix }))
    }

    fn process_thinking_delta(&mut self, event: &Value) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let delta = event
            .get("delta")
            .or_else(|| event.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Vec::new();
        }
        let (block_position, index, mut events) = self.ensure_thinking_block();
        self.thinking_blocks[block_position]
            .thinking
            .push_str(delta);
        events.extend(
            self.state
                .handle_content_block_delta(
                    index,
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "thinking_delta", "thinking": delta }
                    }),
                )
                .into_iter(),
        );
        events
    }

    fn process_thinking_done(&mut self, event: &Value) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let text = event
            .get("text")
            .or_else(|| event.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let existing = self
            .active_thinking_block
            .and_then(|position| self.thinking_blocks.get(position))
            .map(|block| block.thinking.as_str())
            .unwrap_or_default();
        let suffix = suffix_after(existing, text);
        if suffix.is_empty() {
            return Vec::new();
        }
        self.process_thinking_delta(&json!({ "delta": suffix }))
    }

    fn process_output_item_added(&mut self, event: &Value) -> Vec<SseEvent> {
        let item = event.get("item").unwrap_or(&Value::Null);
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let (key, id, name) = function_call_identity(item, event);
                self.ensure_tool_block(&key, &id, &name)
            }
            Some("reasoning") => {
                // 部分网关在 added 就带 encrypted_content；尽早入库以便关
                // thinking 前能发出完整 signature。
                self.record_reasoning_item(item)
            }
            Some("web_search_call") => {
                let (key, id, input) = web_search_identity(item, event);
                // `response.output_item.added` 通常只携带 in_progress 状态；
                // 此时等 done 事件拿到完整 query/来源后再发 block，避免向
                // Anthropic 客户端暴露空输入。少数网关会在 added 中给出 action，
                // 则可立即展示 server tool 开始事件。
                if input.as_object().is_some_and(|input| !input.is_empty()) {
                    self.ensure_web_search_block(&key, &id, input)
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn process_output_item_done(&mut self, event: &Value) -> Vec<SseEvent> {
        let item = event.get("item").unwrap_or(&Value::Null);
        let item_key = output_item_key(item, event);
        if item_key
            .as_ref()
            .is_some_and(|key| self.completed_item_keys.contains(key))
        {
            return Vec::new();
        }
        let mut completed = true;
        let events = match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let (key, id, name) = function_call_identity(item, event);
                let mut events = self.ensure_tool_block(&key, &id, &name);
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                events.extend(self.append_tool_arguments(&key, arguments));
                events.extend(self.stop_tool(&key));
                events
            }
            Some("message") => {
                let mut events = Vec::new();
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if content.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(text) = content.get("text").and_then(Value::as_str) {
                            events.extend(self.process_text_done(&json!({ "text": text })));
                        }
                    }
                }
                events.extend(self.close_text_block());
                events
            }
            Some("reasoning") => {
                let mut events = self.record_reasoning_item(item);
                // 若流式 summary delta 已发过，这里只补全 encrypted；否则用 summary 文本补展示。
                for summary in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(text) = summary.get("text").and_then(Value::as_str) {
                        events.extend(self.process_thinking_done(&json!({ "text": text })));
                    }
                }
                if reasoning_has_encrypted_content(item) {
                    self.awaiting_reasoning_terminal = false;
                    events.extend(self.close_thinking_block());
                } else {
                    // id-only reasoning 不能形成可回放签名。保持 thinking 打开并
                    // 暂缓后续块，等待 response.completed 中的权威 item。
                    self.awaiting_reasoning_terminal = true;
                    completed = false;
                }
                events
            }
            Some("web_search_call") => self.finish_web_search(item, event),
            _ => Vec::new(),
        };
        if completed {
            if let Some(key) = item_key {
                self.completed_item_keys.insert(key);
            }
        }
        events
    }

    fn record_reasoning_item(&mut self, item: &Value) -> Vec<SseEvent> {
        if !self.thinking_enabled {
            return Vec::new();
        }
        let Some(item) = extract_reasoning_item(item) else {
            return Vec::new();
        };
        let (block_position, _, events) = self.ensure_thinking_block();
        let items = &mut self.thinking_blocks[block_position].reasoning_items;
        let id = item.get("id").and_then(Value::as_str);
        if let Some(id) = id {
            if let Some(position) = items
                .iter()
                .position(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
            {
                items[position] = item;
                return events;
            }
        }
        items.push(item);
        events
    }

    fn build_reasoning_signature(&self, block_position: usize) -> Option<String> {
        let block = self.thinking_blocks.get(block_position)?;
        self.signature_codec.as_ref()?.encode(
            &self.model,
            self.credential_id,
            &block.reasoning_items,
        )
    }

    /// 关闭 thinking 块；若有可回放的 reasoning items，先发 signature_delta。
    fn close_thinking_block(&mut self) -> Vec<SseEvent> {
        let Some(block_position) = self.active_thinking_block.take() else {
            return Vec::new();
        };
        let index = self.thinking_blocks[block_position].index;
        let mut events = Vec::new();
        if !self.thinking_blocks[block_position].signature_emitted {
            if let Some(signature) = self.build_reasoning_signature(block_position) {
                events.push(SseEvent::new(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "signature_delta",
                            "signature": signature,
                        }
                    }),
                ));
                self.thinking_blocks[block_position].signature_emitted = true;
            }
        }
        if let Some(stop) = self.state.handle_content_block_stop(index) {
            events.push(stop);
        }
        events
    }

    fn close_text_block(&mut self) -> Vec<SseEvent> {
        let Some(block_position) = self.active_text_block.take() else {
            return Vec::new();
        };
        self.state
            .handle_content_block_stop(self.text_blocks[block_position].index)
            .into_iter()
            .collect()
    }

    fn process_tool_delta(&mut self, event: &Value) -> Vec<SseEvent> {
        let raw_key = event
            .get("item_id")
            .or_else(|| event.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let key = self.canonical_tool_key(raw_key);
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return Vec::new();
        }
        // Grok Build 通常先发送 `response.output_item.added`，其中带有
        // function name。少数响应只在 terminal response 中给出 name；此时
        // 先缓存到完成事件再输出，避免产生 name="tool" 的错误 Anthropic
        // tool_use 块。
        if !self.tool_blocks.contains_key(&key) {
            return Vec::new();
        }
        self.append_tool_arguments(&key, delta)
    }

    fn process_tool_done(&mut self, event: &Value) -> Vec<SseEvent> {
        let raw_key = event
            .get("item_id")
            .or_else(|| event.get("call_id"))
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let key = self.canonical_tool_key(raw_key);
        let mut events = self.append_tool_arguments(
            &key,
            event
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        events.extend(self.stop_tool(&key));
        events
    }

    fn ensure_text_block(&mut self) -> (usize, i32, Vec<SseEvent>) {
        if let Some(block_position) = self.active_text_block {
            return (
                block_position,
                self.text_blocks[block_position].index,
                Vec::new(),
            );
        }
        let mut events = Vec::new();
        // Grok Build 的 reasoning summary 在普通文本前也可能仍处于打开
        // 状态。Anthropic 要求 thinking 块先结束，随后才能开始 text 块。
        // 关闭时附带 signature_delta，供 Claude Code 多轮原样回传。
        events.extend(self.close_thinking_block());
        let index = self.state.next_block_index();
        let block_position = self.text_blocks.len();
        self.text_blocks.push(TextContentBlock {
            index,
            text: String::new(),
        });
        self.active_text_block = Some(block_position);
        self.note_content_order(OrderedBlock::Text(block_position));
        events.extend(self.state.handle_content_block_start(
            index,
            "text",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" }
            }),
        ));
        (block_position, index, events)
    }

    fn ensure_thinking_block(&mut self) -> (usize, i32, Vec<SseEvent>) {
        if let Some(block_position) = self.active_thinking_block {
            return (
                block_position,
                self.thinking_blocks[block_position].index,
                Vec::new(),
            );
        }
        let mut events = self.close_text_block();
        let index = self.state.next_block_index();
        let block_position = self.thinking_blocks.len();
        self.thinking_blocks.push(ThinkingContentBlock {
            index,
            thinking: String::new(),
            reasoning_items: Vec::new(),
            signature_emitted: false,
        });
        self.active_thinking_block = Some(block_position);
        self.note_content_order(OrderedBlock::Thinking(block_position));
        events.extend(self.state.handle_content_block_start(
            index,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "thinking", "thinking": "" }
            }),
        ));
        (block_position, index, events)
    }

    fn ensure_tool_block(&mut self, raw_key: &str, id: &str, name: &str) -> Vec<SseEvent> {
        let key = self.canonical_tool_key(raw_key);
        if self.tool_blocks.contains_key(&key) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // Responses API 的 reasoning summary 可位于 tool call 之前。Anthropic
        // 要求在开启 tool_use 前关闭 thinking 块（含 signature_delta）。
        events.extend(self.close_thinking_block());
        events.extend(self.close_text_block());
        let index = self.state.next_block_index();
        let id = if id.is_empty() {
            key.clone()
        } else {
            id.to_string()
        };
        self.tool_blocks.insert(
            key.clone(),
            ToolBlock {
                index,
                id: id.clone(),
                name: name.to_string(),
                arguments: String::new(),
                stopped: false,
            },
        );
        self.tool_order.push(key.clone());
        self.note_content_order(OrderedBlock::Tool(key.clone()));
        self.tool_aliases.insert(raw_key.to_string(), key.clone());
        self.tool_aliases.insert(id.clone(), key.clone());
        events.extend(self.state.handle_content_block_start(
            index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
            }),
        ));
        events
    }

    fn ensure_web_search_block(&mut self, key: &str, id: &str, input: Value) -> Vec<SseEvent> {
        if self.web_search_blocks.contains_key(key) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // `server_tool_use` 不是 client tool_use，SseStateManager 不会替它
        // 自动关闭 text；显式收束前序文本/思考以满足 Anthropic 的块顺序。
        events.extend(self.close_thinking_block());
        events.extend(self.close_text_block());
        let index = self.state.next_block_index();
        let id = if id.trim().is_empty() {
            key.to_string()
        } else {
            id.to_string()
        };
        self.web_search_blocks.insert(
            key.to_string(),
            WebSearchBlock {
                index,
                id: id.clone(),
                input: input.clone(),
                results: Vec::new(),
                stopped: false,
                result_emitted: false,
            },
        );
        self.web_search_order.push(key.to_string());
        self.note_content_order(OrderedBlock::WebSearch(key.to_string()));
        events.extend(self.state.handle_content_block_start(
            index,
            "server_tool_use",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "server_tool_use",
                    "id": id,
                    "name": "web_search",
                    "input": {},
                }
            }),
        ));
        if !input.as_object().is_some_and(|input| input.is_empty()) {
            let partial_json =
                serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
            if let Some(event) = self.state.handle_content_block_delta(
                index,
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": partial_json,
                    }
                }),
            ) {
                events.push(event);
            }
        }
        events
    }

    fn finish_web_search(&mut self, item: &Value, event: &Value) -> Vec<SseEvent> {
        let (key, id, input) = web_search_identity(item, event);
        let results = web_search_results(item);
        let mut events = self.ensure_web_search_block(&key, &id, input);
        events.extend(self.finish_web_search_block(&key, results));
        events
    }

    fn finish_web_search_block(&mut self, key: &str, results: Vec<Value>) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let stop_index = {
            let Some(block) = self.web_search_blocks.get_mut(key) else {
                return events;
            };
            if block.stopped {
                None
            } else {
                block.stopped = true;
                Some(block.index)
            }
        };
        if let Some(index) = stop_index {
            events.extend(self.state.handle_content_block_stop(index));
        }

        let emit_result = {
            let Some(block) = self.web_search_blocks.get_mut(key) else {
                return events;
            };
            if block.result_emitted {
                false
            } else {
                block.results = results;
                block.result_emitted = true;
                true
            }
        };
        if !emit_result {
            return events;
        }
        let (tool_use_id, content) = self
            .web_search_blocks
            .get(key)
            .map(|block| (block.id.clone(), block.results.clone()))
            .unwrap_or_else(|| (key.to_string(), Vec::new()));
        let index = self.state.next_block_index();
        events.extend(self.state.handle_content_block_start(
            index,
            "web_search_tool_result",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                }
            }),
        ));
        events.extend(self.state.handle_content_block_stop(index));
        events
    }

    fn append_tool_arguments(&mut self, raw_key: &str, arguments: &str) -> Vec<SseEvent> {
        if arguments.is_empty() {
            return Vec::new();
        }
        let key = self.canonical_tool_key(raw_key);
        let Some(tool) = self.tool_blocks.get_mut(&key) else {
            return Vec::new();
        };
        let suffix = suffix_after(&tool.arguments, arguments);
        if suffix.is_empty() {
            return Vec::new();
        }
        tool.arguments.push_str(&suffix);
        self.state
            .handle_content_block_delta(
                tool.index,
                json!({
                    "type": "content_block_delta",
                    "index": tool.index,
                    "delta": { "type": "input_json_delta", "partial_json": suffix }
                }),
            )
            .into_iter()
            .collect()
    }

    fn stop_tool(&mut self, raw_key: &str) -> Vec<SseEvent> {
        let key = self.canonical_tool_key(raw_key);
        let Some(tool) = self.tool_blocks.get_mut(&key) else {
            return Vec::new();
        };
        if tool.stopped {
            return Vec::new();
        }
        tool.stopped = true;
        self.state
            .handle_content_block_stop(tool.index)
            .into_iter()
            .collect()
    }

    fn stop_open_tools(&mut self) -> Vec<SseEvent> {
        let keys = self.tool_order.clone();
        let mut events = Vec::new();
        for key in keys {
            events.extend(self.stop_tool(&key));
        }
        events
    }

    fn canonical_tool_key(&self, raw_key: &str) -> String {
        self.tool_aliases
            .get(raw_key)
            .cloned()
            .unwrap_or_else(|| raw_key.to_string())
    }

    fn capture_response_metadata(&mut self, response: &Value) {
        let usage = response.get("usage").unwrap_or(response);
        self.input_tokens = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        self.output_tokens = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_i64)
            .map(|value| value as i32);
        if response.get("status").and_then(Value::as_str) == Some("incomplete") {
            self.stop_reason = Some("max_tokens".to_string());
        }
    }

    pub fn finish_events(&mut self) -> Vec<SseEvent> {
        // 某些兼容网关只发送 added 而没有 done。仍然给 Anthropic 客户端一个
        // 完整的 server-tool 生命周期（空来源结果），而不是留下半开的块。
        let pending_web_searches = self
            .web_search_order
            .iter()
            .filter(|key| {
                self.web_search_blocks
                    .get(*key)
                    .is_some_and(|block| !block.result_emitted)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut events = Vec::new();
        events.extend(self.close_thinking_block());
        events.extend(self.close_text_block());
        for key in pending_web_searches {
            events.extend(self.finish_web_search_block(&key, Vec::new()));
        }
        if let Some(reason) = self.stop_reason.clone() {
            self.state.set_stop_reason(reason);
        }
        let mut final_events = self
            .state
            .generate_final_events(self.input_tokens(), self.output_tokens());
        self.add_web_search_usage(&mut final_events);
        events.extend(final_events);
        events
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    /// 兼容少数网关把 `stream: true` 响应降级为单个 JSON response 的情况。
    /// 将其中的 output items 走同一状态机聚合，确保非流式调用仍能返回
    /// Anthropic Message 格式。
    pub fn ingest_completed_response(&mut self, response: &Value) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for (output_index, item) in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            events.extend(self.process_output_item_done(&json!({
                "item": item,
                "output_index": output_index,
            })));
            if self.awaiting_reasoning_terminal {
                // terminal 已是最后权威来源；若仍没有 encrypted content，只能
                // 以无 signature 的 thinking 块收束，不能继续阻塞后续 output。
                self.awaiting_reasoning_terminal = false;
                events.extend(self.close_thinking_block());
            }
        }
        self.completed = true;
        self.capture_response_metadata(response);
        events
    }

    pub fn input_tokens(&self) -> i32 {
        self.input_tokens.unwrap_or(self.requested_input_tokens)
    }

    pub fn output_tokens(&self) -> i32 {
        self.output_tokens.unwrap_or_else(|| {
            let text = self
                .content_order
                .iter()
                .filter_map(|entry| match entry {
                    OrderedBlock::Text(position) => self
                        .text_blocks
                        .get(*position)
                        .map(|block| block.text.as_str()),
                    OrderedBlock::Thinking(position) => self
                        .thinking_blocks
                        .get(*position)
                        .map(|block| block.thinking.as_str()),
                    _ => None,
                })
                .collect::<String>();
            crate::token::count_tokens(&text) as i32
        })
    }

    pub fn to_anthropic_response(&self) -> Value {
        let mut content = Vec::new();
        let push_thinking = |content: &mut Vec<Value>, block_position: usize| {
            let Some(block) = self.thinking_blocks.get(block_position) else {
                return;
            };
            let signature = self.build_reasoning_signature(block_position);
            if self.thinking_enabled && (!block.thinking.is_empty() || signature.is_some()) {
                let mut thinking = json!({
                    "type": "thinking",
                    "thinking": block.thinking,
                });
                if let Some(signature) = signature {
                    thinking["signature"] = Value::String(signature);
                }
                content.push(thinking);
            }
        };
        let push_text = |content: &mut Vec<Value>, block_position: usize| {
            let Some(block) = self.text_blocks.get(block_position) else {
                return;
            };
            if !block.text.is_empty() {
                content.push(json!({ "type": "text", "text": block.text }));
            }
        };
        let push_web_search = |content: &mut Vec<Value>, key: &str| {
            if let Some(web_search) = self.web_search_blocks.get(key) {
                content.push(json!({
                    "type": "server_tool_use",
                    "id": web_search.id,
                    "name": "web_search",
                    "input": web_search.input,
                }));
                content.push(json!({
                    "type": "web_search_tool_result",
                    "tool_use_id": web_search.id,
                    "content": web_search.results,
                }));
            }
        };
        let push_tool = |content: &mut Vec<Value>, key: &str| {
            if let Some(tool) = self.tool_blocks.get(key) {
                let input =
                    serde_json::from_str::<Value>(&tool.arguments).unwrap_or_else(|_| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": tool.id,
                    "name": tool.name,
                    "input": input,
                }));
            }
        };

        if self.content_order.is_empty() {
            // 兼容未走过 ensure_* 的聚合路径：回退到历史固定顺序。
            for position in 0..self.thinking_blocks.len() {
                push_thinking(&mut content, position);
            }
            for key in &self.web_search_order {
                push_web_search(&mut content, key);
            }
            for position in 0..self.text_blocks.len() {
                push_text(&mut content, position);
            }
            for key in &self.tool_order {
                push_tool(&mut content, key);
            }
        } else {
            for entry in &self.content_order {
                match entry {
                    OrderedBlock::Thinking(position) => push_thinking(&mut content, *position),
                    OrderedBlock::Text(position) => push_text(&mut content, *position),
                    OrderedBlock::WebSearch(key) => push_web_search(&mut content, key),
                    OrderedBlock::Tool(key) => push_tool(&mut content, key),
                }
            }
        }
        let stop_reason = self.stop_reason.clone().unwrap_or_else(|| {
            if self.tool_blocks.is_empty() {
                "end_turn".to_string()
            } else {
                "tool_use".to_string()
            }
        });
        let mut usage = json!({
            "input_tokens": self.input_tokens(),
            "output_tokens": self.output_tokens(),
        });
        if !self.web_search_order.is_empty() {
            usage["server_tool_use"] = json!({
                "web_search_requests": self.web_search_order.len(),
            });
        }
        json!({
            "id": self.message_id,
            "type": "message",
            "role": "assistant",
            "content": content,
            "model": self.model,
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": usage,
        })
    }

    /// Anthropic 将服务端 Web Search 的消耗单列在 `usage.server_tool_use`。
    /// 保留这个字段可让标准 Anthropic 客户端正确归因，而不把它误判为普通
    /// client tool_use。
    fn add_web_search_usage(&self, events: &mut [SseEvent]) {
        if self.web_search_order.is_empty() {
            return;
        }
        let count = self.web_search_order.len();
        for event in events {
            if event.event != "message_delta" {
                continue;
            }
            if let Some(usage) = event.data.get_mut("usage").and_then(Value::as_object_mut) {
                usage.insert(
                    "server_tool_use".to_string(),
                    json!({ "web_search_requests": count }),
                );
            }
        }
    }
}

fn output_item_key(item: &Value, event: &Value) -> Option<String> {
    let item_type = item.get("type").and_then(Value::as_str)?;
    let identity = item
        .get("id")
        .or_else(|| item.get("call_id"))
        .or_else(|| event.get("item_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            event
                .get("output_index")
                .and_then(Value::as_u64)
                .map(|index| index.to_string())
        })?;
    Some(format!("{item_type}:{identity}"))
}

fn reasoning_has_encrypted_content(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn function_call_identity(item: &Value, event: &Value) -> (String, String, String) {
    let item_id = item
        .get("id")
        .or_else(|| event.get("item_id"))
        .or_else(|| item.get("call_id"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or(item_id);
    let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
    (item_id.to_string(), call_id.to_string(), name.to_string())
}

fn web_search_identity(item: &Value, event: &Value) -> (String, String, Value) {
    let id = item
        .get("id")
        .or_else(|| event.get("item_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("web_search");
    (id.to_string(), id.to_string(), web_search_input(item))
}

/// xAI 的 action 在 search/open_page/find 等动作间变化。Anthropic web
/// search 的标准 input 是 query；对非 search 动作保留完整 action，避免将
/// xAI 的信息静默丢弃。
fn web_search_input(item: &Value) -> Value {
    let Some(action) = item.get("action").and_then(Value::as_object) else {
        return json!({});
    };
    if action.get("type").and_then(Value::as_str) == Some("search") {
        if let Some(query) = action.get("query").and_then(Value::as_str) {
            return json!({ "query": query });
        }
    }
    Value::Object(action.clone())
}

/// Build 将 web-search 结果保存在 `web_search_call.action.sources`。转换成
/// Anthropic `web_search_result` 时优先保留 URL、标题和可展示的摘要文本；
/// 服务端未提供来源时则发送空列表，和 Anthropic 的 server-tool 语义一致。
fn web_search_results(item: &Value) -> Vec<Value> {
    item.pointer("/action/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|source| {
            let url = source.get("url").and_then(Value::as_str)?.trim();
            if url.is_empty() {
                return None;
            }
            let title = source
                .get("title")
                .or_else(|| source.get("name"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(url);
            let content = source
                .get("encrypted_content")
                .or_else(|| source.get("snippet"))
                .or_else(|| source.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut result = json!({
                "type": "web_search_result",
                "title": title,
                "url": url,
                "encrypted_content": content,
            });
            if let Some(page_age) = source
                .get("page_age")
                .or_else(|| source.get("published_date"))
                .cloned()
            {
                result["page_age"] = page_age;
            }
            Some(result)
        })
        .collect()
}

fn chat_text_values(delta: &Value, keys: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    for key in keys {
        let Some(value) = delta.get(*key) else {
            continue;
        };
        if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
            values.push(text.to_string());
            continue;
        }
        if let Some(parts) = value.as_array() {
            for part in parts {
                if let Some(text) = part
                    .as_str()
                    .or_else(|| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.is_empty())
                {
                    values.push(text.to_string());
                }
            }
        }
    }
    values
}

fn suffix_after(existing: &str, complete_or_delta: &str) -> String {
    if complete_or_delta.is_empty() {
        return String::new();
    }
    if let Some(suffix) = complete_or_delta.strip_prefix(existing) {
        return suffix.to_string();
    }
    if existing.ends_with(complete_or_delta) {
        return String::new();
    }
    complete_or_delta.to_string()
}

fn truncate(value: &str) -> &str {
    if value.len() <= 500 {
        return value;
    }
    let mut end = 500;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fragmented_sse() {
        let mut decoder = XaiSseDecoder::default();
        assert!(
            decoder
                .feed(b"event: response.output_text.delta\ndata: {\"type\":")
                .unwrap()
                .is_empty()
        );
        let events = decoder
            .feed(b"\"response.output_text.delta\",\"delta\":\"hi\"}\n\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["delta"], "hi");
    }

    #[test]
    fn preserves_multibyte_utf8_split_across_chunks() {
        let mut decoder = XaiSseDecoder::default();
        // "你好" in UTF-8: E4 BD A0 E5 A5 BD — split after first byte of second char
        let prefix = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"";
        let chinese = "你好".as_bytes();
        let mut first = prefix.to_vec();
        first.extend_from_slice(&chinese[..4]); // incomplete second char
        assert!(decoder.feed(&first).unwrap().is_empty());
        let mut second = chinese[4..].to_vec();
        second.extend_from_slice(b"\"}\n\n");
        let events = decoder.feed(&second).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["delta"], "你好");
    }

    #[test]
    fn decodes_crlf_framed_sse_events() {
        let mut decoder = XaiSseDecoder::default();
        let frame = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"crlf\"}\r\n\r\n";
        let events = decoder.feed(frame).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["delta"], "crlf");
    }

    #[test]
    fn decodes_mixed_and_bare_cr_sse_separators() {
        let mut decoder = XaiSseDecoder::default();
        let events = decoder
            .feed(
                b"data: {\"type\":\"one\"}\r\n\ndata: {\"type\":\"two\"}\n\r\ndata: {\"type\":\"three\"}\r\r",
            )
            .unwrap();
        assert_eq!(events[0]["type"], "one");
        assert_eq!(events[1]["type"], "two");
        let final_events = decoder.finish().unwrap();
        assert_eq!(final_events[0]["type"], "three");

        let mut fragmented = XaiSseDecoder::default();
        assert!(
            fragmented
                .feed(b"data: {\"type\":\"before\"}\n\r")
                .unwrap()
                .is_empty()
        );
        let events = fragmented
            .feed(b"\ndata: {\"type\":\"after\"}\n\n")
            .unwrap();
        assert_eq!(events[0]["type"], "before");
        assert_eq!(events[1]["type"], "after");
    }

    #[test]
    fn rejects_invalid_utf8_and_malformed_json_instead_of_dropping_events() {
        let mut invalid_utf8 = XaiSseDecoder::default();
        let mut bytes = b"data: {\"type\":\"delta\",\"text\":\"".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"\"}\n\n");
        assert!(matches!(
            invalid_utf8.feed(&bytes),
            Err(XaiSseDecodeError::InvalidUtf8(_))
        ));

        let mut malformed = XaiSseDecoder::default();
        assert!(matches!(
            malformed.feed(b"data: {not-json}\n\n"),
            Err(XaiSseDecodeError::InvalidJson { .. })
        ));
        let mut malformed_at_eof = XaiSseDecoder::default();
        malformed_at_eof.feed(b"data: {").unwrap();
        assert!(matches!(
            malformed_at_eof.finish(),
            Err(XaiSseDecodeError::InvalidJson { .. })
        ));
    }

    #[test]
    fn rejects_unterminated_frame_over_the_memory_limit() {
        let mut decoder = XaiSseDecoder::default();
        let oversized = vec![b'x'; MAX_SSE_FRAME_BYTES + MAX_SSE_SEPARATOR_BYTES + 1];
        assert_eq!(
            decoder.feed(&oversized),
            Err(XaiSseDecodeError::FrameTooLarge {
                limit: MAX_SSE_FRAME_BYTES
            })
        );
        assert!(decoder.buffer.is_empty());
    }

    #[test]
    fn messages_observer_requires_and_accepts_fragmented_terminal_event() {
        let mut observer = AnthropicSseObserver::default();
        observer.feed(
            b"event: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\nevent: message_",
        );
        assert!(!observer.terminal());
        assert!(observer.failure().is_none());
        observer.feed(b"stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n");
        assert!(observer.terminal());
        assert!(observer.failure().is_none());
    }

    #[test]
    fn messages_observer_detects_event_name_and_json_error_forms() {
        let mut named = AnthropicSseObserver::default();
        named.feed(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"quota\"}}\n\n",
        );
        assert_eq!(
            named.failure(),
            Some(&AnthropicSseFailure::Upstream("quota".to_string()))
        );

        let mut data_only = AnthropicSseObserver::default();
        data_only.feed(b"data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n");
        assert_eq!(
            data_only.failure(),
            Some(&AnthropicSseFailure::Upstream("overloaded".to_string()))
        );
    }

    #[test]
    fn messages_observer_rejects_malformed_event_without_claiming_terminal() {
        let mut observer = AnthropicSseObserver::default();
        observer.feed(b"event: message_delta\ndata: {not-json}\n\n");
        assert!(!observer.terminal());
        assert!(matches!(
            observer.failure(),
            Some(AnthropicSseFailure::Protocol(message))
                if message.contains("JSON 无效")
        ));

        let mut truncated = AnthropicSseObserver::default();
        truncated.feed(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}");
        truncated.finish();
        assert!(!truncated.terminal());
        assert!(truncated.failure().is_none());

        let mut oversized = AnthropicSseObserver::default();
        oversized.feed(&vec![
            b'x';
            MAX_SSE_FRAME_BYTES + MAX_SSE_SEPARATOR_BYTES + 1
        ]);
        assert!(matches!(
            oversized.failure(),
            Some(AnthropicSseFailure::Protocol(message)) if message.contains("4 MiB")
        ));
        assert!(oversized.buffer.is_empty());
    }

    #[test]
    fn incomplete_stream_without_terminal_is_not_completed() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({
            "type": "response.output_text.delta",
            "delta": "partial"
        }));
        assert!(!context.completed());
        // 无 response.completed 时 finish_events 仍会生成收尾，但 completed 仍为 false，
        // 供 handlers 决定是否发成功 message_stop。
        let _ = context.finish_events();
        assert!(!context.completed());
    }

    #[test]
    fn interleaved_output_order_preserved_in_non_stream_response() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [
                    {
                        "type": "reasoning",
                        "id": "rs_1",
                        "summary": [{"type":"summary_text","text":"think"}],
                        "encrypted_content": "enc"
                    },
                    {
                        "type": "web_search_call",
                        "id": "ws_1",
                        "status": "completed",
                        "action": {
                            "type": "search",
                            "query": "rust",
                            "sources": [{"url":"https://example.com","title":"ex"}]
                        }
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type":"output_text","text":"answer"}]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "read",
                        "arguments": "{\"path\":\"a\"}"
                    }
                ],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        }));
        assert!(context.completed());
        let response = context.to_anthropic_response();
        let types: Vec<&str> = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect();
        // thinking → server_tool_use + result → text → tool_use
        assert_eq!(
            types,
            vec![
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "text",
                "tool_use"
            ]
        );
    }

    #[test]
    fn accumulates_text_and_tool_calls() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({"type":"response.output_text.delta","delta":"Hello"}));
        context.process_event(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read"}}));
        context.process_event(&json!({"type":"response.function_call_arguments.delta","item_id":"item_1","delta":"{\"path\":\"a\"}"}));
        context.process_event(&json!({"type":"response.output_item.done","item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read"}}));
        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0]["text"], "Hello");
        assert_eq!(response["content"][1]["id"], "call_1");
        assert_eq!(response["content"][1]["input"]["path"], "a");
    }

    #[test]
    fn terminal_response_supplies_tool_metadata_when_item_added_is_absent() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "plan "
        }));

        // Grok Build can send argument deltas keyed by call_id before it sends
        // the function name in the terminal response.
        assert!(
            context
                .process_event(&json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": "call_1",
                    "delta": "{\"path\":\"a\"}"
                }))
                .is_empty()
        );

        let events = context.process_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{\"path\":\"a\"}"
                }],
                "usage": { "input_tokens": 10, "output_tokens": 2 }
            }
        }));

        let thinking_stop = events
            .iter()
            .position(|event| event.event == "content_block_stop" && event.data["index"] == 0)
            .expect("thinking block must stop before tool use");
        let tool_start = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
                    && event.data["content_block"]["name"] == "read"
            })
            .expect("terminal function metadata must start a named tool block");
        assert!(thinking_stop < tool_start);

        let response = context.to_anthropic_response();
        assert_eq!(response["content"][1]["name"], "read");
        assert_eq!(response["content"][1]["input"]["path"], "a");
    }

    #[test]
    fn text_starts_after_reasoning_block_has_stopped() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "considering"
        }));

        let events = context.process_event(&json!({
            "type": "response.output_text.delta",
            "delta": "answer"
        }));
        let thinking_stop = events
            .iter()
            .position(|event| event.event == "content_block_stop" && event.data["index"] == 0)
            .expect("thinking block must stop before text");
        let text_start = events
            .iter()
            .position(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "text"
            })
            .expect("text block must start");
        assert!(thinking_stop < text_start);

        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0]["thinking"], "considering");
        assert_eq!(response["content"][1]["text"], "answer");
    }

    #[test]
    fn emits_xai_rs2_signature_before_thinking_stop_and_non_stream_carries_it() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        context.set_signature_codec(codec.clone());
        context.set_credential_id(42);
        context.process_event(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "plan "
        }));
        let events = context.process_event(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "reasoning",
                "id": "rs_1",
                "status": "completed",
                "summary": [{"type":"summary_text","text":"plan "}],
                "encrypted_content": "enc_secret"
            }
        }));

        let text_events = context.process_event(&json!({
            "type": "response.output_text.delta",
            "delta": "ok"
        }));
        let signature_pos = events
            .iter()
            .position(|event| {
                event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "signature_delta"
            })
            .expect("signature_delta before thinking stop");
        let thinking_stop = events
            .iter()
            .position(|event| event.event == "content_block_stop" && event.data["index"] == 0)
            .expect("thinking stop");
        assert!(signature_pos < thinking_stop);
        assert!(text_events.iter().any(|event| {
            event.event == "content_block_start" && event.data["content_block"]["type"] == "text"
        }));

        let signature = events[signature_pos].data["delta"]["signature"]
            .as_str()
            .unwrap();
        let package = codec.decode(signature).expect("xai-rs2 package");
        assert_eq!(package.credential_id, Some(42));
        assert_eq!(package.items[0]["id"], "rs_1");
        assert_eq!(package.items[0]["encrypted_content"], "enc_secret");

        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0]["type"], "thinking");
        assert_eq!(response["content"][0]["thinking"], "plan ");
        assert!(
            response["content"][0]["signature"]
                .as_str()
                .unwrap()
                .starts_with("xai-rs2.")
        );
    }

    #[test]
    fn preserves_multiple_reasoning_items_around_hosted_tools() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.set_signature_codec(codec.clone());
        context.set_credential_id(42);
        let events = context.process_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type":"summary_text","text":"first"}],
                    "encrypted_content": "enc_1"
                }, {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "completed",
                    "action": {"type":"search","query":"rust","sources":[]}
                }, {
                    "type": "reasoning",
                    "id": "tco_2",
                    "summary": [],
                    "encrypted_content": "enc_2"
                }, {
                    "type": "message",
                    "id": "msg_1",
                    "content": [{"type":"output_text","text":"answer"}]
                }]
            }
        }));

        let signatures = events
            .iter()
            .filter(|event| {
                event.data.pointer("/delta/type").and_then(Value::as_str) == Some("signature_delta")
            })
            .map(|event| event.data["delta"]["signature"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(signatures.len(), 2);
        assert_eq!(codec.decode(signatures[0]).unwrap().items[0]["id"], "rs_1");
        assert_eq!(codec.decode(signatures[1]).unwrap().items[0]["id"], "tco_2");

        let response = context.to_anthropic_response();
        let types = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|block| block["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            vec![
                "thinking",
                "server_tool_use",
                "web_search_tool_result",
                "thinking",
                "text"
            ]
        );
        let response_signatures = response["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block.get("signature").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(response_signatures, signatures);
    }

    #[test]
    fn terminal_encrypted_reasoning_unblocks_deferred_text_with_complete_signature() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let codec = ReasoningSignatureCodec::new(b"test-server-secret");
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.set_signature_codec(codec.clone());
        context.set_credential_id(42);
        context.process_event(&json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": "plan"
        }));
        let done_events = context.process_event(&json!({
            "type": "response.output_item.done",
            "item": {"type":"reasoning","id":"rs_terminal","summary":[]}
        }));
        assert!(done_events.iter().all(|event| {
            event.data.pointer("/delta/type").and_then(Value::as_str) != Some("signature_delta")
        }));
        assert!(
            context
                .process_event(&json!({
                    "type": "response.output_text.delta",
                    "delta": "must defer"
                }))
                .is_empty()
        );

        let terminal_events = context.process_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "reasoning",
                    "id": "rs_terminal",
                    "summary": [{"type":"summary_text","text":"plan"}],
                    "encrypted_content": "enc_terminal"
                }, {
                    "type": "message",
                    "id": "msg_terminal",
                    "content": [{"type":"output_text","text":"answer"}]
                }]
            }
        }));
        let signature = terminal_events
            .iter()
            .find(|event| {
                event.data.pointer("/delta/type").and_then(Value::as_str) == Some("signature_delta")
            })
            .and_then(|event| event.data.pointer("/delta/signature"))
            .and_then(Value::as_str)
            .expect("terminal must supply a complete signature");
        assert_eq!(
            codec.decode(signature).unwrap().items[0]["encrypted_content"],
            "enc_terminal"
        );
        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0]["thinking"], "plan");
        assert_eq!(response["content"][1]["text"], "answer");
    }

    #[test]
    fn repeated_text_items_remain_separate_around_web_search() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, false);
        context.process_event(&json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_a",
                    "content": [{"type":"output_text","text":"A"}]
                }, {
                    "type": "web_search_call",
                    "id": "ws_mid",
                    "action": {"type":"search","query":"q","sources":[]}
                }, {
                    "type": "message",
                    "id": "msg_b",
                    "content": [{"type":"output_text","text":"B"}]
                }]
            }
        }));
        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0], json!({"type":"text","text":"A"}));
        assert_eq!(response["content"][1]["type"], "server_tool_use");
        assert_eq!(response["content"][2]["type"], "web_search_tool_result");
        assert_eq!(response["content"][3], json!({"type":"text","text":"B"}));
    }

    #[test]
    fn terminal_snapshot_does_not_duplicate_completed_stream_items() {
        use super::super::reasoning_sig::ReasoningSignatureCodec;

        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.set_signature_codec(ReasoningSignatureCodec::new(b"test-server-secret"));
        context.set_credential_id(42);
        let reasoning = json!({
            "type":"reasoning",
            "id":"rs_done",
            "summary":[{"type":"summary_text","text":"plan"}],
            "encrypted_content":"enc_done"
        });
        let message = json!({
            "type":"message",
            "id":"msg_done",
            "content":[{"type":"output_text","text":"answer"}]
        });
        context.process_event(&json!({"type":"response.output_item.done","item":reasoning}));
        context.process_event(&json!({"type":"response.output_item.done","item":message}));
        context.process_event(&json!({
            "type":"response.completed",
            "response":{"status":"completed","output":[reasoning,message]}
        }));

        let response = context.to_anthropic_response();
        assert_eq!(response["content"].as_array().unwrap().len(), 2);
        assert_eq!(response["content"][0]["type"], "thinking");
        assert_eq!(response["content"][1]["text"], "answer");
    }

    #[test]
    fn converts_chat_completion_reasoning_content_and_tool_calls() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        context.process_event(&json!({
            "object": "chat.completion.chunk",
            "choices": [{
                "delta": {
                    "reasoning_content": "plan",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"a\"}" }
                    }]
                }
            }]
        }));
        context.process_event(&json!({
            "object": "chat.completion.chunk",
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
        }));
        context.process_event(&json!({ "type": "done" }));
        assert!(context.completed());
        let response = context.to_anthropic_response();
        assert_eq!(response["content"][0]["thinking"], "plan");
        assert_eq!(response["content"][1]["type"], "tool_use");
        assert_eq!(response["content"][1]["name"], "read");
        assert_eq!(response["content"][1]["input"]["path"], "a");
    }

    #[test]
    fn converts_xai_web_search_call_to_anthropic_server_tool_result() {
        let mut context = GrokStreamContext::new("grok-4.5", 10, true);
        let events = context.process_event(&json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "status": "completed",
                "action": {
                    "type": "search",
                    "query": "latest Rust release",
                    "sources": [{
                        "title": "Rust Blog",
                        "url": "https://blog.rust-lang.org/",
                        "snippet": "Release notes"
                    }]
                }
            }
        }));

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "server_tool_use"
                && event.data["content_block"]["input"] == json!({})
        }));
        assert!(events.iter().any(|event| {
            event.event == "content_block_delta"
                && event.data["delta"]["type"] == "input_json_delta"
                && event.data["delta"]["partial_json"]
                    .as_str()
                    .and_then(|value| serde_json::from_str::<Value>(value).ok())
                    .is_some_and(|input| input["query"] == "latest Rust release")
        }));
        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "web_search_tool_result"
                && event.data["content_block"]["tool_use_id"] == "ws_1"
                && event.data["content_block"]["content"][0]["url"] == "https://blog.rust-lang.org/"
        }));

        let response = context.to_anthropic_response();
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["content"][0]["type"], "server_tool_use");
        assert_eq!(response["content"][1]["type"], "web_search_tool_result");
        assert_eq!(response["content"][1]["tool_use_id"], "ws_1");
        assert_eq!(
            response["usage"]["server_tool_use"]["web_search_requests"],
            1
        );

        let final_events = context.finish_events();
        let delta = final_events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("final message delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "end_turn");
        assert_eq!(
            delta.data["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
    }
}
