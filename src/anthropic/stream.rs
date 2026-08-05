//! 流式响应处理模块
//!
//! 实现 Kiro → Anthropic 流式响应转换和 SSE 状态管理

use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::kiro::model::events::Event;


/// SSE 事件
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

impl SseEvent {
    pub fn new(event: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }

    /// 格式化为 SSE 字符串
    pub fn to_sse_string(&self) -> String {
        format!(
            "event: {}\ndata: {}\n\n",
            self.event,
            serde_json::to_string(&self.data).unwrap_or_default()
        )
    }
}

/// 内容块状态
#[derive(Debug, Clone)]
struct BlockState {
    block_type: String,
    started: bool,
    stopped: bool,
}

impl BlockState {
    fn new(block_type: impl Into<String>) -> Self {
        Self {
            block_type: block_type.into(),
            started: false,
            stopped: false,
        }
    }
}

/// SSE 状态管理器
///
/// 确保 SSE 事件序列符合 Claude API 规范：
/// 1. message_start 只能出现一次
/// 2. content_block 必须先 start 再 delta 再 stop
/// 3. message_delta 只能出现一次，且在所有 content_block_stop 之后
/// 4. message_stop 在最后
#[derive(Debug)]
pub struct SseStateManager {
    /// message_start 是否已发送
    message_started: bool,
    /// message_delta 是否已发送
    message_delta_sent: bool,
    /// 活跃的内容块状态
    active_blocks: HashMap<i32, BlockState>,
    /// 消息是否已结束
    message_ended: bool,
    /// 下一个块索引
    next_block_index: i32,
    /// 当前 stop_reason
    stop_reason: Option<String>,
    /// 是否有工具调用
    has_tool_use: bool,
}

impl Default for SseStateManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SseStateManager {
    pub fn new() -> Self {
        Self {
            message_started: false,
            message_delta_sent: false,
            active_blocks: HashMap::new(),
            message_ended: false,
            next_block_index: 0,
            stop_reason: None,
            has_tool_use: false,
        }
    }

    /// 判断指定块是否处于可接收 delta 的打开状态
    fn is_block_open_of_type(&self, index: i32, expected_type: &str) -> bool {
        self.active_blocks
            .get(&index)
            .is_some_and(|b| b.started && !b.stopped && b.block_type == expected_type)
    }

    /// 获取下一个块索引
    pub fn next_block_index(&mut self) -> i32 {
        let index = self.next_block_index;
        self.next_block_index += 1;
        index
    }

    /// 记录工具调用
    pub fn set_has_tool_use(&mut self, has: bool) {
        self.has_tool_use = has;
    }

    /// 设置 stop_reason
    pub fn set_stop_reason(&mut self, reason: impl Into<String>) {
        self.stop_reason = Some(reason.into());
    }

    /// 检查是否存在非 thinking 类型的内容块（如 text 或 tool_use）
    fn has_non_thinking_blocks(&self) -> bool {
        self.active_blocks
            .values()
            .any(|b| b.block_type != "thinking")
    }

    /// 获取最终的 stop_reason
    pub fn get_stop_reason(&self) -> String {
        if let Some(ref reason) = self.stop_reason {
            reason.clone()
        } else if self.has_tool_use {
            "tool_use".to_string()
        } else {
            "end_turn".to_string()
        }
    }

    /// 处理 message_start 事件
    pub fn handle_message_start(&mut self, event: serde_json::Value) -> Option<SseEvent> {
        if self.message_started {
            tracing::debug!("跳过重复的 message_start 事件");
            return None;
        }
        self.message_started = true;
        Some(SseEvent::new("message_start", event))
    }

    /// 处理 content_block_start 事件
    pub fn handle_content_block_start(
        &mut self,
        index: i32,
        block_type: &str,
        data: serde_json::Value,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果是 tool_use 块，先关闭之前的文本块
        if block_type == "tool_use" {
            self.has_tool_use = true;
            for (block_index, block) in self.active_blocks.iter_mut() {
                if block.block_type == "text" && block.started && !block.stopped {
                    // 自动发送 content_block_stop 关闭文本块
                    events.push(SseEvent::new(
                        "content_block_stop",
                        json!({
                            "type": "content_block_stop",
                            "index": block_index
                        }),
                    ));
                    block.stopped = true;
                }
            }
        }

        // 检查块是否已存在
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.started {
                tracing::debug!("块 {} 已启动，跳过重复的 content_block_start", index);
                return events;
            }
            block.started = true;
        } else {
            let mut block = BlockState::new(block_type);
            block.started = true;
            self.active_blocks.insert(index, block);
        }

        events.push(SseEvent::new("content_block_start", data));
        events
    }

    /// 处理 content_block_delta 事件
    pub fn handle_content_block_delta(
        &mut self,
        index: i32,
        data: serde_json::Value,
    ) -> Option<SseEvent> {
        // 确保块已启动
        if let Some(block) = self.active_blocks.get(&index) {
            if !block.started || block.stopped {
                tracing::warn!(
                    "块 {} 状态异常: started={}, stopped={}",
                    index,
                    block.started,
                    block.stopped
                );
                return None;
            }
        } else {
            // 块不存在，可能需要先创建
            tracing::warn!("收到未知块 {} 的 delta 事件", index);
            return None;
        }

        Some(SseEvent::new("content_block_delta", data))
    }

    /// 处理 content_block_stop 事件
    pub fn handle_content_block_stop(&mut self, index: i32) -> Option<SseEvent> {
        if let Some(block) = self.active_blocks.get_mut(&index) {
            if block.stopped {
                tracing::debug!("块 {} 已停止，跳过重复的 content_block_stop", index);
                return None;
            }
            block.stopped = true;
            return Some(SseEvent::new(
                "content_block_stop",
                json!({
                    "type": "content_block_stop",
                    "index": index
                }),
            ));
        }
        None
    }

    /// 生成最终事件序列
    pub fn generate_final_events(
        &mut self,
        input_tokens: i32,
        output_tokens: i32,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 关闭所有未关闭的块
        for (index, block) in self.active_blocks.iter_mut() {
            if block.started && !block.stopped {
                events.push(SseEvent::new(
                    "content_block_stop",
                    json!({
                        "type": "content_block_stop",
                        "index": index
                    }),
                ));
                block.stopped = true;
            }
        }

        // 发送 message_delta
        if !self.message_delta_sent {
            self.message_delta_sent = true;
            events.push(SseEvent::new(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": self.get_stop_reason(),
                        "stop_sequence": null
                    },
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens
                    }
                }),
            ));
        }

        // 发送 message_stop
        if !self.message_ended {
            self.message_ended = true;
            events.push(SseEvent::new(
                "message_stop",
                json!({ "type": "message_stop" }),
            ));
        }

        events
    }
}

use super::converter::get_context_window_size;

/// 流处理上下文
pub struct StreamContext {
    /// SSE 状态管理器
    pub state_manager: SseStateManager,
    /// 请求的模型名称
    pub model: String,
    /// 消息 ID
    pub message_id: String,
    /// 输入 tokens（估算值）
    pub input_tokens: i32,
    /// 从 contextUsageEvent 计算的实际输入 tokens
    pub context_input_tokens: Option<i32>,
    /// 输出 tokens 累计
    pub output_tokens: i32,
    /// 工具块索引映射 (tool_id -> block_index)
    pub tool_block_indices: HashMap<String, i32>,
    /// 工具名称反向映射（短名称 → 原始名称），用于响应时还原
    pub tool_name_map: HashMap<String, String>,
    /// thinking 是否启用
    pub thinking_enabled: bool,
    /// 是否在 thinking 块内
    pub in_thinking_block: bool,
    /// thinking 块是否已提取完成
    pub thinking_extracted: bool,
    /// thinking 块索引
    pub thinking_block_index: Option<i32>,
    /// 文本块索引（thinking 启用时动态分配）
    pub text_block_index: Option<i32>,
    /// 捕获到的推理签名
    pub signature: Option<String>,
    /// 上游 meteringEvent 下发的本次请求扣费（`None` 表示未收到）
    pub credits: Option<f64>,
    /// 扣费单位（如 "credits"）
    pub credits_unit: Option<String>,
    /// 上游实际服务的模型 ID（来自 assistantResponseEvent.modelId）
    pub served_model: Option<String>,
    /// 回答字符数累计（用于成本口径分析）
    pub answer_chars: u64,
    /// 思考字符数累计
    pub reasoning_chars: u64,
    /// 工具调用字符数累计（toolUseEvent 的 name + input）
    pub tool_chars: u64,
    /// 上游下发的上下文使用百分比原值
    pub context_usage_percentage: Option<f64>,
}

impl StreamContext {
    /// 创建启用thinking的StreamContext
    pub fn new_with_thinking(
        model: impl Into<String>,
        input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        Self {
            state_manager: SseStateManager::new(),
            model: model.into(),
            message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
            input_tokens,
            context_input_tokens: None,
            output_tokens: 0,
            tool_block_indices: HashMap::new(),
            tool_name_map,
            thinking_enabled,
            in_thinking_block: false,
            thinking_extracted: false,
            thinking_block_index: None,
            text_block_index: None,
            signature: None,
            credits: None,
            credits_unit: None,
            served_model: None,
            answer_chars: 0,
            reasoning_chars: 0,
            tool_chars: 0,
            context_usage_percentage: None,
        }
    }

    /// 汇总本次请求的用量，供计量账本记账
    ///
    /// `credential_id` 是实际服务本次请求的凭据（故障转移后可能不是首选那张），
    /// `requested_model` 是下发给上游的 modelId（映射后的值，而非客户端模型名）。
    pub fn to_request_usage(
        &self,
        credential_id: Option<u64>,
        conversation_id: Option<String>,
        requested_model: impl Into<String>,
        stream: bool,
    ) -> crate::kiro::usage::RequestUsage {
        crate::kiro::usage::RequestUsage {
            credential_id,
            conversation_id,
            requested_model: requested_model.into(),
            served_model: self.served_model.clone(),
            credits: self.credits,
            unit: self.credits_unit.clone(),
            answer_chars: self.answer_chars,
            reasoning_chars: self.reasoning_chars,
            tool_chars: self.tool_chars,
            context_usage_percentage: self.context_usage_percentage,
            stream,
        }
    }

    /// 生成 message_start 事件
    pub fn create_message_start_event(&self) -> serde_json::Value {
        json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": 1
                }
            }
        })
    }

    /// 生成初始事件序列 (message_start + 文本块 start)
    ///
    /// 当 thinking 启用时，不在初始化时创建文本块，而是等到实际收到内容时再创建。
    /// 这样可以确保 thinking 块（索引 0）在文本块（索引 1）之前。
    pub fn generate_initial_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // message_start
        let msg_start = self.create_message_start_event();
        if let Some(event) = self.state_manager.handle_message_start(msg_start) {
            events.push(event);
        }

        // 如果启用了 thinking，不在这里创建文本块
        // thinking 块和文本块会在 process_content_with_thinking 中按正确顺序创建
        if self.thinking_enabled {
            return events;
        }

        // 创建初始文本块（仅在未启用 thinking 时）
        let text_block_index = self.state_manager.next_block_index();
        self.text_block_index = Some(text_block_index);
        let text_block_events = self.state_manager.handle_content_block_start(
            text_block_index,
            "text",
            json!({
                "type": "content_block_start",
                "index": text_block_index,
                "content_block": {
                    "type": "text",
                    "text": ""
                }
            }),
        );
        events.extend(text_block_events);

        events
    }

    /// Process Kiro events and convert to Anthropic SSE events
    pub fn process_kiro_event(&mut self, event: &Event) -> Vec<SseEvent> {
        match event {
            Event::ReasoningContent(resp) => {
                if let Some(ref sig) = resp.signature {
                    self.signature = Some(sig.clone());
                }
                self.reasoning_chars += resp.text.chars().count() as u64;
                self.process_reasoning_content(&resp.text)
            }
            Event::AssistantResponse(resp) => {
                self.answer_chars += resp.content.chars().count() as u64;
                // 上游可能静默替换模型。modelId 每个 delta 都会带，只在首次取值，
                // 避免整条响应期间反复分配 String。
                if self.served_model.is_none() {
                    self.served_model = resp.model_id.clone().filter(|m| !m.is_empty());
                }
                self.process_assistant_response(&resp.content)
            }
            Event::ToolUse(tool_use) => {
                // 工具调用的 input JSON 同样是模型生成的输出，必须计入成本口径。
                // name 只在收尾帧记一次，避免每个 delta 重复累加。
                self.tool_chars += tool_use.input.chars().count() as u64;
                if tool_use.stop {
                    self.tool_chars += tool_use.name.chars().count() as u64;
                }
                self.process_tool_use(tool_use)
            }
            Event::Metering(metering) => {
                self.credits = Some(metering.usage);
                self.credits_unit = Some(metering.unit_label().to_string());
                tracing::debug!("收到 meteringEvent: {}", metering);
                Vec::new()
            }
            Event::ContextUsage(context_usage) => {
                self.context_usage_percentage = Some(context_usage.context_usage_percentage);
                // 从上下文使用百分比计算实际的 input_tokens
                let window_size = get_context_window_size(&self.model);
                let actual_input_tokens = (context_usage.context_usage_percentage
                    * (window_size as f64)
                    / 100.0) as i32;
                self.context_input_tokens = Some(actual_input_tokens);
                // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                if context_usage.context_usage_percentage >= 100.0 {
                    self.state_manager
                        .set_stop_reason("model_context_window_exceeded");
                }
                tracing::debug!(
                    "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                    context_usage.context_usage_percentage,
                    actual_input_tokens
                );
                Vec::new()
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                tracing::error!("收到错误事件: {} - {}", error_code, error_message);
                Vec::new()
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                // 处理 ContentLengthExceededException
                if exception_type == "ContentLengthExceededException" {
                    self.state_manager.set_stop_reason("max_tokens");
                }
                tracing::warn!("收到异常事件: {} - {}", exception_type, message);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Process reasoning content event
    fn process_reasoning_content(&mut self, text: &str) -> Vec<SseEvent> {
        // Even if text is empty, we may still need to create the thinking block
        // to send the signature later. Check if we have a signature waiting.
        let has_pending_signature = self.signature.is_some() && text.is_empty();

        if text.is_empty() && !has_pending_signature {
            return Vec::new();
        }

        // Estimate output tokens
        self.output_tokens += estimate_tokens(text);

        let mut events = Vec::new();

        // Ensure thinking block is started
        let thinking_index = if let Some(idx) = self.thinking_block_index {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.thinking_block_index = Some(idx);
            self.in_thinking_block = true;

            // Send content_block_start event for thinking
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "thinking",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "thinking",
                        "thinking": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // Send thinking_delta event
        events.push(self.create_thinking_delta_event(thinking_index, text));

        events
    }

    /// Process assistant response event
    fn process_assistant_response(&mut self, content: &str) -> Vec<SseEvent> {
        if content.is_empty() {
            return Vec::new();
        }

        // Estimate output tokens
        self.output_tokens += estimate_tokens(content);

        let mut events = Vec::new();

        // If we are still in thinking block, close it first
        events.extend(self.close_thinking_block());

        // Emit text_delta events
        events.extend(self.create_text_delta_events(content));
        events
    }


    /// 创建 text_delta 事件
    ///
    /// 如果文本块尚未创建，会先创建文本块。
    /// 当发生 tool_use 时，状态机会自动关闭当前文本块；后续文本会自动创建新的文本块继续输出。
    ///
    /// 返回值包含可能的 content_block_start 事件和 content_block_delta 事件。
    fn create_text_delta_events(&mut self, text: &str) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // 如果当前 text_block_index 指向的块已经被关闭（例如 tool_use 开始时自动 stop），
        // 则丢弃该索引并创建新的文本块继续输出，避免 delta 被状态机拒绝导致“吞字”。
        if let Some(idx) = self.text_block_index {
            if !self.state_manager.is_block_open_of_type(idx, "text") {
                self.text_block_index = None;
            }
        }

        // 获取或创建文本块索引
        let text_index = if let Some(idx) = self.text_block_index {
            idx
        } else {
            // 文本块尚未创建，需要先创建
            let idx = self.state_manager.next_block_index();
            self.text_block_index = Some(idx);

            // 发送 content_block_start 事件
            let start_events = self.state_manager.handle_content_block_start(
                idx,
                "text",
                json!({
                    "type": "content_block_start",
                    "index": idx,
                    "content_block": {
                        "type": "text",
                        "text": ""
                    }
                }),
            );
            events.extend(start_events);
            idx
        };

        // 发送 content_block_delta 事件
        if let Some(delta_event) = self.state_manager.handle_content_block_delta(
            text_index,
            json!({
                "type": "content_block_delta",
                "index": text_index,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ) {
            events.push(delta_event);
        }

        events
    }

    /// 创建 thinking_delta 事件
    fn create_thinking_delta_event(&self, index: i32, thinking: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "thinking_delta",
                    "thinking": thinking
                }
            }),
        )
    }

    /// 创建 signature_delta 事件
    fn create_signature_delta_event(&self, index: i32, signature: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {
                    "type": "signature_delta",
                    "signature": signature
                }
            }),
        )
    }

    /// 关闭当前 thinking 块并生成相应的事件（包括 signature_delta 和 thinking_delta 的收尾）
    fn close_thinking_block(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.in_thinking_block {
            self.in_thinking_block = false;
            self.thinking_extracted = true;

            if let Some(thinking_index) = self.thinking_block_index {
                // 如果存在加密签名，先发送 signature_delta 事件
                if let Some(ref sig) = self.signature {
                    events.push(self.create_signature_delta_event(thinking_index, sig));
                }
                // 发送空的 thinking_delta 并停止该块
                events.push(self.create_thinking_delta_event(thinking_index, ""));
                if let Some(stop_event) = self.state_manager.handle_content_block_stop(thinking_index) {
                    events.push(stop_event);
                }
            }
        }
        events
    }

    /// Process tool use event
    fn process_tool_use(
        &mut self,
        tool_use: &crate::kiro::model::events::ToolUseEvent,
    ) -> Vec<SseEvent> {
        let mut events = Vec::new();

        self.state_manager.set_has_tool_use(true);

        // 若仍处于 thinking 块中，先关闭它。必须复用 close_thinking_block()，
        // 它会在收尾前发出 signature_delta；否则当模型「思考后直接调用工具」（中间无正文）
        // 时，thinking 块会缺失签名，导致 Claude Code 回传该块时上游返回
        // 400 THINKING_SIGNATURE_INVALID。
        events.extend(self.close_thinking_block());

        // 获取或分配块索引
        let block_index = if let Some(&idx) = self.tool_block_indices.get(&tool_use.tool_use_id) {
            idx
        } else {
            let idx = self.state_manager.next_block_index();
            self.tool_block_indices
                .insert(tool_use.tool_use_id.clone(), idx);
            idx
        };

        // 还原工具名称（如果有映射）
        let original_name = self
            .tool_name_map
            .get(&tool_use.name)
            .cloned()
            .unwrap_or_else(|| tool_use.name.clone());

        // 发送 content_block_start
        let start_events = self.state_manager.handle_content_block_start(
            block_index,
            "tool_use",
            json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": tool_use.tool_use_id,
                    "name": original_name,
                    "input": {}
                }
            }),
        );
        events.extend(start_events);

        // 发送参数增量 (ToolUseEvent.input 是 String 类型)
        if !tool_use.input.is_empty() {
            self.output_tokens += (tool_use.input.len() as i32 + 3) / 4; // 估算 token

            if let Some(delta_event) = self.state_manager.handle_content_block_delta(
                block_index,
                json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": tool_use.input
                    }
                }),
            ) {
                events.push(delta_event);
            }
        }

        // 如果是完整的工具调用（stop=true），发送 content_block_stop
        if tool_use.stop {
            if let Some(stop_event) = self.state_manager.handle_content_block_stop(block_index) {
                events.push(stop_event);
            }
        }

        events
    }

    /// Generate final events sequence
    pub fn generate_final_events(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // If we are still in thinking block, close it first
        events.extend(self.close_thinking_block());

        // 如果整个流中只产生了 thinking 块，没有 text 也没有 tool_use，
        // 则设置 stop_reason 为 max_tokens（表示模型耗尽了 token 预算在思考上），
        // 并补发一套完整的 text 事件（内容为一个空格），确保 content 数组中有 text 块
        if self.thinking_enabled
            && self.thinking_block_index.is_some()
            && !self.state_manager.has_non_thinking_blocks()
        {
            self.state_manager.set_stop_reason("max_tokens");
            events.extend(self.create_text_delta_events(" "));
        }

        // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
        let final_input_tokens = self.context_input_tokens.unwrap_or(self.input_tokens);

        // 生成最终事件
        events.extend(
            self.state_manager
                .generate_final_events(final_input_tokens, self.output_tokens),
        );
        events
    }
}

/// 缓冲流处理上下文 - 用于 /cc/v1/messages 流式请求
///
/// 与 `StreamContext` 不同，此上下文会缓冲所有事件直到流结束，
/// 然后用从 `contextUsageEvent` 计算的正确 `input_tokens` 更正 `message_start` 事件。
///
/// 工作流程：
/// 1. 使用 `StreamContext` 正常处理所有 Kiro 事件
/// 2. 把生成的 SSE 事件缓存起来（而不是立即发送）
/// 3. 流结束时，找到 `message_start` 事件并更新其 `input_tokens`
/// 4. 一次性返回所有事件
pub struct BufferedStreamContext {
    /// 内部流处理上下文（复用现有的事件处理逻辑）
    inner: StreamContext,
    /// 缓冲的所有事件（包括 message_start、content_block_start 等）
    event_buffer: Vec<SseEvent>,
    /// 估算的 input_tokens（用于回退）
    estimated_input_tokens: i32,
    /// 是否已经生成了初始事件
    initial_events_generated: bool,
}

impl BufferedStreamContext {
    /// 创建缓冲流上下文
    pub fn new(
        model: impl Into<String>,
        estimated_input_tokens: i32,
        thinking_enabled: bool,
        tool_name_map: HashMap<String, String>,
    ) -> Self {
        let inner =
            StreamContext::new_with_thinking(model, estimated_input_tokens, thinking_enabled, tool_name_map);
        Self {
            inner,
            event_buffer: Vec::new(),
            estimated_input_tokens,
            initial_events_generated: false,
        }
    }

    /// 内部流上下文（用于读取累计的计量数据）
    pub fn inner(&self) -> &StreamContext {
        &self.inner
    }

    /// 处理 Kiro 事件并缓冲结果
    ///
    /// 复用 StreamContext 的事件处理逻辑，但把结果缓存而不是立即发送。
    pub fn process_and_buffer(&mut self, event: &crate::kiro::model::events::Event) {
        // 首次处理事件时，先生成初始事件（message_start 等）
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 处理事件并缓冲结果
        let events = self.inner.process_kiro_event(event);
        self.event_buffer.extend(events);
    }

    /// 完成流处理并返回所有事件
    ///
    /// 此方法会：
    /// 1. 生成最终事件（message_delta, message_stop）
    /// 2. 用正确的 input_tokens 更正 message_start 事件
    /// 3. 返回所有缓冲的事件
    pub fn finish_and_get_all_events(&mut self) -> Vec<SseEvent> {
        // 如果从未处理过事件，也要生成初始事件
        if !self.initial_events_generated {
            let initial_events = self.inner.generate_initial_events();
            self.event_buffer.extend(initial_events);
            self.initial_events_generated = true;
        }

        // 生成最终事件
        let final_events = self.inner.generate_final_events();
        self.event_buffer.extend(final_events);

        // 获取正确的 input_tokens
        let final_input_tokens = self
            .inner
            .context_input_tokens
            .unwrap_or(self.estimated_input_tokens);

        // 更正 message_start 事件中的 input_tokens
        for event in &mut self.event_buffer {
            if event.event == "message_start" {
                if let Some(message) = event.data.get_mut("message") {
                    if let Some(usage) = message.get_mut("usage") {
                        usage["input_tokens"] = serde_json::json!(final_input_tokens);
                    }
                }
            }
        }

        std::mem::take(&mut self.event_buffer)
    }
}

/// 简单的 token 估算
fn estimate_tokens(text: &str) -> i32 {
    let chars: Vec<char> = text.chars().collect();
    let mut chinese_count = 0;
    let mut other_count = 0;

    for c in &chars {
        if *c >= '\u{4E00}' && *c <= '\u{9FFF}' {
            chinese_count += 1;
        } else {
            other_count += 1;
        }
    }

    // 中文约 1.5 字符/token，英文约 4 字符/token
    let chinese_tokens = (chinese_count * 2 + 2) / 3;
    let other_tokens = (other_count + 3) / 4;

    (chinese_tokens + other_tokens).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_event_format() {
        let event = SseEvent::new("message_start", json!({"type": "message_start"}));
        let sse_str = event.to_sse_string();

        assert!(sse_str.starts_with("event: message_start\n"));
        assert!(sse_str.contains("data: "));
        assert!(sse_str.ends_with("\n\n"));
    }

    #[test]
    fn test_sse_state_manager_message_start() {
        let mut manager = SseStateManager::new();

        // 第一次应该成功
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_some());

        // 第二次应该被跳过
        let event = manager.handle_message_start(json!({"type": "message_start"}));
        assert!(event.is_none());
    }

    #[test]
    fn test_sse_state_manager_block_lifecycle() {
        let mut manager = SseStateManager::new();

        // 创建块
        let events = manager.handle_content_block_start(0, "text", json!({}));
        assert_eq!(events.len(), 1);

        // delta
        let event = manager.handle_content_block_delta(0, json!({}));
        assert!(event.is_some());

        // stop
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_some());

        // 重复 stop 应该被跳过
        let event = manager.handle_content_block_stop(0);
        assert!(event.is_none());
    }

    #[test]
    fn test_tool_name_reverse_mapping_in_stream() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut map = HashMap::new();
        map.insert("short_abc12345".to_string(), "mcp__very_long_original_tool_name".to_string());

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, map);
        let _ = ctx.generate_initial_events();

        // 模拟 Kiro 返回短名称的 tool_use
        let tool_event = Event::ToolUse(ToolUseEvent {
            name: "short_abc12345".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"key":"value"}"#.to_string(),
            stop: true,
        });

        let events = ctx.process_kiro_event(&tool_event);

        // content_block_start 中的 name 应该是原始长名称
        let start_event = events.iter().find(|e| e.event == "content_block_start").unwrap();
        assert_eq!(
            start_event.data["content_block"]["name"],
            "mcp__very_long_original_tool_name",
            "应还原为原始工具名称"
        );
    }

    #[test]
    fn test_text_delta_after_tool_use_restarts_text_block() {
        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());

        let initial_events = ctx.generate_initial_events();
        assert!(
            initial_events
                .iter()
                .any(|e| e.event == "content_block_start"
                    && e.data["content_block"]["type"] == "text")
        );

        let initial_text_index = ctx
            .text_block_index
            .expect("initial text block index should exist");

        // tool_use 开始会自动关闭现有 text block
        let tool_events = ctx.process_tool_use(&crate::kiro::model::events::ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: false,
        });
        assert!(
            tool_events.iter().any(|e| {
                e.event == "content_block_stop"
                    && e.data["index"].as_i64() == Some(initial_text_index as i64)
            }),
            "tool_use should stop the previous text block"
        );

        // 之后再来文本增量，应自动创建新的 text block 而不是往已 stop 的块里写 delta
        let text_events = ctx.process_assistant_response("hello");
        let new_text_start_index = text_events.iter().find_map(|e| {
            if e.event == "content_block_start" && e.data["content_block"]["type"] == "text" {
                e.data["index"].as_i64()
            } else {
                None
            }
        });
        assert!(
            new_text_start_index.is_some(),
            "should start a new text block"
        );
        assert_ne!(
            new_text_start_index.unwrap(),
            initial_text_index as i64,
            "new text block index should differ from the stopped one"
        );
        assert!(
            text_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == "hello"
            }),
            "should emit text_delta after restarting text block"
        );
    }

    #[test]
    fn test_estimate_tokens() {
        assert!(estimate_tokens("Hello") > 0);
        assert!(estimate_tokens("你好") > 0);
        assert!(estimate_tokens("Hello 你好") > 0);
    }

    #[test]
    fn test_reasoning_content_stream_flow() {
        use crate::kiro::model::events::ReasoningContentEvent;
        use crate::kiro::model::events::AssistantResponseEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let initial_events = ctx.generate_initial_events();
        assert_eq!(initial_events.len(), 1);
        assert_eq!(initial_events[0].event, "message_start");

        // 1. Process reasoning content
        let event1 = Event::ReasoningContent(ReasoningContentEvent::new("Thinking process..."));
        let events1 = ctx.process_kiro_event(&event1);
        
        // Should start thinking block and emit thinking_delta
        assert_eq!(events1.len(), 2);
        assert_eq!(events1[0].event, "content_block_start");
        assert_eq!(events1[0].data["content_block"]["type"], "thinking");
        assert_eq!(events1[1].event, "content_block_delta");
        assert_eq!(events1[1].data["delta"]["type"], "thinking_delta");
        assert_eq!(events1[1].data["delta"]["thinking"], "Thinking process...");

        // 2. Process assistant response (transition from thinking to text)
        let event2 = Event::AssistantResponse(AssistantResponseEvent::new("Final answer"));
        let events2 = ctx.process_kiro_event(&event2);

        // Should:
        // - Close thinking block (empty thinking_delta + content_block_stop)
        // - Start text block (content_block_start)
        // - Emit text_delta (content_block_delta)
        assert_eq!(events2.len(), 4);
        assert_eq!(events2[0].event, "content_block_delta");
        assert_eq!(events2[0].data["delta"]["type"], "thinking_delta");
        assert_eq!(events2[0].data["delta"]["thinking"], "");
        assert_eq!(events2[1].event, "content_block_stop");
        assert_eq!(events2[1].data["index"], 0);

        assert_eq!(events2[2].event, "content_block_start");
        assert_eq!(events2[2].data["content_block"]["type"], "text");
        assert_eq!(events2[3].event, "content_block_delta");
        assert_eq!(events2[3].data["delta"]["type"], "text_delta");
        assert_eq!(events2[3].data["delta"]["text"], "Final answer");

        // 3. Finalize stream
        let final_events = ctx.generate_final_events();
        // Should close text block and send message_delta, message_stop
        assert!(final_events.iter().any(|e| e.event == "content_block_stop" && e.data["index"] == 1));
        assert!(final_events.iter().any(|e| e.event == "message_delta"));
        assert!(final_events.iter().any(|e| e.event == "message_stop"));
    }

    #[test]
    fn test_thinking_only_sets_max_tokens_stop_reason() {
        use crate::kiro::model::events::ReasoningContentEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        let event1 = Event::ReasoningContent(ReasoningContentEvent::new("Thinking process..."));
        let _ = ctx.process_kiro_event(&event1);

        let final_events = ctx.generate_final_events();

        // Should set stop_reason as max_tokens
        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "max_tokens",
            "stop_reason should be max_tokens when only thinking is produced"
        );

        // Should have emitted a text block with a single space
        assert!(
            final_events.iter().any(|e| {
                e.event == "content_block_start" && e.data["content_block"]["type"] == "text"
            }),
            "should emit text content_block_start"
        );
        assert!(
            final_events.iter().any(|e| {
                e.event == "content_block_delta"
                    && e.data["delta"]["type"] == "text_delta"
                    && e.data["delta"]["text"] == " "
            }),
            "should emit text_delta with a single space"
        );
    }

    #[test]
    fn test_thinking_with_text_keeps_end_turn_stop_reason() {
        use crate::kiro::model::events::ReasoningContentEvent;
        use crate::kiro::model::events::AssistantResponseEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        let event1 = Event::ReasoningContent(ReasoningContentEvent::new("Thinking process..."));
        let _ = ctx.process_kiro_event(&event1);

        let event2 = Event::AssistantResponse(AssistantResponseEvent::new("Final answer"));
        let _ = ctx.process_kiro_event(&event2);

        let final_events = ctx.generate_final_events();

        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "end_turn",
            "stop_reason should be end_turn when text is also produced"
        );
    }

    #[test]
    fn test_thinking_with_tool_use_keeps_tool_use_stop_reason() {
        use crate::kiro::model::events::ReasoningContentEvent;
        use crate::kiro::model::events::ToolUseEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, true, HashMap::new());
        let _ = ctx.generate_initial_events();

        let event1 = Event::ReasoningContent(ReasoningContentEvent::new("Thinking process..."));
        let _ = ctx.process_kiro_event(&event1);

        let event2 = Event::ToolUse(ToolUseEvent {
            name: "test_tool".to_string(),
            tool_use_id: "tool_1".to_string(),
            input: "{}".to_string(),
            stop: true,
        });
        let _ = ctx.process_kiro_event(&event2);

        let final_events = ctx.generate_final_events();

        let message_delta = final_events
            .iter()
            .find(|e| e.event == "message_delta")
            .expect("should have message_delta event");

        assert_eq!(
            message_delta.data["delta"]["stop_reason"], "tool_use",
            "stop_reason should be tool_use when tool_use is present"
        );
    }

    #[test]
    fn test_metering_event_accumulated() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-6", 0, false, HashMap::new());
        assert_eq!(ctx.credits, None);

        let events = ctx.process_kiro_event(&Event::Metering(
            crate::kiro::model::events::MeteringEvent::new(2.5),
        ));
        // 计费事件不产生任何面向客户端的 SSE
        assert!(events.is_empty());
        assert_eq!(ctx.credits, Some(2.5));
        assert_eq!(ctx.credits_unit.as_deref(), Some("credits"));
    }

    #[test]
    fn test_served_model_and_output_chars_accumulated() {
        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-6", 0, true, HashMap::new());

        ctx.process_kiro_event(&Event::ReasoningContent(
            crate::kiro::model::events::ReasoningContentEvent::new("思考"),
        ));
        let mut answer = crate::kiro::model::events::AssistantResponseEvent::new("hello");
        answer.model_id = Some("claude-opus-4.7".to_string());
        ctx.process_kiro_event(&Event::AssistantResponse(answer));

        assert_eq!(ctx.reasoning_chars, 2, "按字符计数而非字节");
        assert_eq!(ctx.answer_chars, 5);
        assert_eq!(ctx.served_model.as_deref(), Some("claude-opus-4.7"));
    }

    /// 工具调用的 input 也是模型输出，必须计入成本口径
    ///
    /// 回归用例：实测一次「28 字符回答 + 一个大 tool_use」的请求扣了 0.114 credits，
    /// 只算回答文本会把单位成本高估约 6.7 倍。
    #[test]
    fn test_tool_use_chars_counted_as_output() {
        use crate::kiro::model::events::ToolUseEvent;

        let mut ctx = StreamContext::new_with_thinking("test-model", 1, false, HashMap::new());
        let _ = ctx.generate_initial_events();

        // input 分两个 delta 到达，name 只在收尾帧计一次
        ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "Bash".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#"{"command":"#.to_string(),
            stop: false,
        }));
        ctx.process_kiro_event(&Event::ToolUse(ToolUseEvent {
            name: "Bash".to_string(),
            tool_use_id: "toolu_01".to_string(),
            input: r#""ls -la"}"#.to_string(),
            stop: true,
        }));

        // 11 + 9 个 input 字符 + 一次 "Bash"(4)
        assert_eq!(ctx.tool_chars, 24, "name 不应随每个 delta 重复累加");

        let mut answer = crate::kiro::model::events::AssistantResponseEvent::new("ok");
        answer.model_id = None;
        ctx.process_kiro_event(&Event::AssistantResponse(answer));

        let usage = ctx.to_request_usage(Some(1), None, "m", true);
        assert_eq!(usage.answer_chars, 2);
        assert_eq!(usage.tool_chars, 24);
        // 成本分母必须含工具输出，否则带工具轮次的单位成本被系统性高估
        assert_eq!(usage.output_chars(), 26);
    }

    /// 用真实抓包的上游响应验证整条计量提取链路
    ///
    /// `docs/kiro_rs_aws_turn1_res.txt` 是一次真实 `generateAssistantResponse`
    /// 的原始 event-stream 字节流，末尾带 `meteringEvent`。
    #[test]
    fn test_extract_usage_from_real_capture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/kiro_rs_aws_turn1_res.txt"
        );
        let bytes = std::fs::read(path).expect("真实抓包文件缺失，无法验证计量提取");

        let mut decoder = crate::kiro::parser::decoder::EventStreamDecoder::new();
        decoder.feed(&bytes).unwrap();

        let mut ctx = StreamContext::new_with_thinking("claude-opus-4-8", 0, true, HashMap::new());
        for result in decoder.decode_iter() {
            if let Ok(frame) = result {
                if let Ok(event) = Event::from_frame(frame) {
                    ctx.process_kiro_event(&event);
                }
            }
        }

        // 扣费取自 meteringEvent，必须精确等于上游下发的值
        let credits = ctx.credits.expect("未从真实抓包中解析出 meteringEvent");
        assert!(
            (credits - 2.3283256603316747).abs() < f64::EPSILON,
            "credits = {}",
            credits
        );
        assert_eq!(ctx.credits_unit.as_deref(), Some("credits"));
        // 请求发的是 opus-4.8，上游实际用 opus-4.7 服务
        assert_eq!(ctx.served_model.as_deref(), Some("claude-opus-4.7"));
        assert!(ctx.answer_chars > 0, "回答字符数应被累计");
        assert!(ctx.reasoning_chars > 0, "思考字符数应被累计");
        assert!(ctx.context_usage_percentage.is_some());

        // 账本口径：由 StreamContext 汇总出的 RequestUsage 应保留同样的值
        let usage = ctx.to_request_usage(Some(7), Some("conv-x".to_string()), "claude-opus-4.8", true);
        assert_eq!(usage.credits, ctx.credits);
        assert_eq!(usage.effective_model(), "claude-opus-4.7");
        assert!(usage.is_substituted(), "请求与实际服务模型不一致应被标记");
    }
}
