//! xAI Responses SSE → Anthropic SSE 转换。

use std::collections::HashMap;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::anthropic::{SseEvent, SseStateManager};

#[derive(Default)]
pub struct XaiSseDecoder {
    buffer: String,
}

impl XaiSseDecoder {
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Value> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut events = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let block = self.buffer[..index].replace('\r', "");
            self.buffer.drain(..index + 2);
            if let Some(event) = parse_sse_block(&block) {
                events.push(event);
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<Value> {
        let block = std::mem::take(&mut self.buffer).replace('\r', "");
        parse_sse_block(&block).into_iter().collect()
    }
}

fn parse_sse_block(block: &str) -> Option<Value> {
    let data = block
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return None;
    }
    // Chat Completions 以 `[DONE]` 结束，而 Responses 以
    // `response.completed` 结束。保留一个内部完成标记，非流式聚合可以同时
    // 验证两种 backend 的终止语义。
    if data == "[DONE]" {
        return Some(json!({ "type": "done" }));
    }
    match serde_json::from_str(&data) {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::debug!(%error, data = %truncate(&data), "忽略无法解析的 xAI SSE 事件");
            None
        }
    }
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

/// 同时驱动流式事件和非流式聚合的状态机。
pub struct GrokStreamContext {
    state: SseStateManager,
    model: String,
    message_id: String,
    requested_input_tokens: i32,
    input_tokens: Option<i32>,
    output_tokens: Option<i32>,
    thinking_enabled: bool,
    text_block_index: Option<i32>,
    thinking_block_index: Option<i32>,
    text: String,
    thinking: String,
    tool_blocks: HashMap<String, ToolBlock>,
    tool_aliases: HashMap<String, String>,
    tool_order: Vec<String>,
    web_search_blocks: HashMap<String, WebSearchBlock>,
    web_search_order: Vec<String>,
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
            text_block_index: None,
            thinking_block_index: None,
            text: String::new(),
            thinking: String::new(),
            tool_blocks: HashMap::new(),
            tool_aliases: HashMap::new(),
            tool_order: Vec::new(),
            web_search_blocks: HashMap::new(),
            web_search_order: Vec::new(),
            stop_reason: None,
            completed: false,
        }
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
                self.ingest_completed_response(event.get("response").unwrap_or(event))
            }
            "done" => {
                self.completed = true;
                Vec::new()
            }
            "response.incomplete" => {
                let events = self.ingest_completed_response(event.get("response").unwrap_or(event));
                self.stop_reason = Some("max_tokens".to_string());
                events
            }
            "response.failed" | "error" => {
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
        let (index, mut events) = self.ensure_text_block();
        self.text.push_str(delta);
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
        let suffix = suffix_after(&self.text, text);
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
        let (index, mut events) = self.ensure_thinking_block();
        self.thinking.push_str(delta);
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
        let suffix = suffix_after(&self.thinking, text);
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
        match item.get("type").and_then(Value::as_str) {
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
                events
            }
            Some("web_search_call") => self.finish_web_search(item, event),
            _ => Vec::new(),
        }
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

    fn ensure_text_block(&mut self) -> (i32, Vec<SseEvent>) {
        if let Some(index) = self.text_block_index {
            return (index, Vec::new());
        }
        let mut events = Vec::new();
        // Grok Build 的 reasoning summary 在普通文本前也可能仍处于打开
        // 状态。Anthropic 要求 thinking 块先结束，随后才能开始 text 块。
        if let Some(index) = self.thinking_block_index.take() {
            events.extend(self.state.handle_content_block_stop(index));
        }
        let index = self.state.next_block_index();
        self.text_block_index = Some(index);
        events.extend(self.state.handle_content_block_start(
            index,
            "text",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" }
            }),
        ));
        (index, events)
    }

    fn ensure_thinking_block(&mut self) -> (i32, Vec<SseEvent>) {
        if let Some(index) = self.thinking_block_index {
            return (index, Vec::new());
        }
        let index = self.state.next_block_index();
        self.thinking_block_index = Some(index);
        let events = self.state.handle_content_block_start(
            index,
            "thinking",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "thinking", "thinking": "" }
            }),
        );
        (index, events)
    }

    fn ensure_tool_block(&mut self, raw_key: &str, id: &str, name: &str) -> Vec<SseEvent> {
        let key = self.canonical_tool_key(raw_key);
        if self.tool_blocks.contains_key(&key) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // Responses API 的 reasoning summary 可位于 tool call 之前。Anthropic
        // 要求在开启 tool_use 前关闭 thinking 块。
        if let Some(index) = self.thinking_block_index.take() {
            events.extend(self.state.handle_content_block_stop(index));
        }
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
        // `SseStateManager` 会在上方自动关闭 text 块；清空本地索引以允许
        // 后续 Responses text item 创建新的 Anthropic text 块。
        self.text_block_index = None;
        events
    }

    fn ensure_web_search_block(&mut self, key: &str, id: &str, input: Value) -> Vec<SseEvent> {
        if self.web_search_blocks.contains_key(key) {
            return Vec::new();
        }
        let mut events = Vec::new();
        // `server_tool_use` 不是 client tool_use，SseStateManager 不会替它
        // 自动关闭 text；显式收束前序文本/思考以满足 Anthropic 的块顺序。
        if let Some(index) = self.thinking_block_index.take() {
            events.extend(self.state.handle_content_block_stop(index));
        }
        if let Some(index) = self.text_block_index.take() {
            events.extend(self.state.handle_content_block_stop(index));
        }
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
                    "input": input,
                }
            }),
        ));
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
        let content = self
            .web_search_blocks
            .get(key)
            .map(|block| block.results.clone())
            .unwrap_or_default();
        let index = self.state.next_block_index();
        events.extend(self.state.handle_content_block_start(
            index,
            "web_search_tool_result",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "web_search_tool_result",
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
        for item in response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let item_type = item.get("type").and_then(Value::as_str);
            if item_type == Some("reasoning") {
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
            } else {
                events.extend(self.process_output_item_done(&json!({ "item": item })));
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
            crate::token::count_tokens(&(self.text.clone() + &self.thinking)) as i32
        })
    }

    pub fn to_anthropic_response(&self) -> Value {
        let mut content = Vec::new();
        if self.thinking_enabled && !self.thinking.is_empty() {
            content.push(json!({ "type": "thinking", "thinking": self.thinking }));
        }
        for key in &self.web_search_order {
            if let Some(web_search) = self.web_search_blocks.get(key) {
                content.push(json!({
                    "type": "server_tool_use",
                    "id": web_search.id,
                    "name": "web_search",
                    "input": web_search.input,
                }));
                content.push(json!({
                    "type": "web_search_tool_result",
                    "content": web_search.results,
                }));
            }
        }
        if !self.text.is_empty() {
            content.push(json!({ "type": "text", "text": self.text }));
        }
        for key in &self.tool_order {
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
    value.get(..500).unwrap_or(value)
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
                .is_empty()
        );
        let events = decoder.feed(b"\"response.output_text.delta\",\"delta\":\"hi\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["delta"], "hi");
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
                && event.data["content_block"]["input"]["query"] == "latest Rust release"
        }));
        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "web_search_tool_result"
                && event.data["content_block"]["content"][0]["url"] == "https://blog.rust-lang.org/"
        }));

        let response = context.to_anthropic_response();
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["content"][0]["type"], "server_tool_use");
        assert_eq!(response["content"][1]["type"], "web_search_tool_result");
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
