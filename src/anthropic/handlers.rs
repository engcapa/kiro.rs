//! Anthropic API Handler 函数

use std::convert::Infallible;

use anyhow::Error;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, StreamContext};
use super::types::{CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse, OutputConfig, Thinking};
use super::websearch;

/// 将 KiroProvider 错误映射为 HTTP 响应
fn map_provider_error(err: Error) -> Response {
    let err_str = err.to_string();

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let guard = crate::kiro::model::model_catalog::GLOBAL_MODEL_CATALOG.read().unwrap();
    let models = if let Some(catalog) = &*guard {
        let mut list = Vec::new();
        for m in &catalog.models {
            let max_tokens = m
                .token_limits
                .as_ref()
                .and_then(|l| l.max_output_tokens)
                .unwrap_or(64000);
            let context_window = m
                .token_limits
                .as_ref()
                .and_then(|l| l.max_input_tokens);
            // 基础模型
            list.push(Model {
                id: m.model_id.clone(),
                object: "model".to_string(),
                created: 1776276000,
                owned_by: "anthropic".to_string(),
                display_name: m.model_name.clone(),
                model_type: "chat".to_string(),
                max_tokens,
                context_window,
            });

            // 检查 schema properties 中是否包含 thinking / output_config / reasoning 相关配置
            // （reasoning 为 gpt-5.x 系列的推理 effort 形状）
            let supports_thinking = m.additional_model_request_fields_schema
                .as_ref()
                .and_then(|s| s.as_object())
                .and_then(|obj| obj.get("properties"))
                .and_then(|props| props.as_object())
                .map(|props| {
                    props.contains_key("thinking")
                        || props.contains_key("output_config")
                        || props.contains_key("reasoning")
                })
                .unwrap_or(false);

            if supports_thinking {
                list.push(Model {
                    id: format!("{}-thinking", m.model_id),
                    object: "model".to_string(),
                    created: 1776276000,
                    owned_by: "anthropic".to_string(),
                    display_name: format!("{} (Thinking)", m.model_name),
                    model_type: "chat".to_string(),
                    max_tokens,
                    context_window,
                });
            }
        }
        list
    } else {
        // Fallback hardcoded list if catalog is not loaded yet
        vec![
            Model {
                id: "claude-opus-4-7".to_string(),
                object: "model".to_string(),
                created: 1776276000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.7".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-opus-4-7-thinking".to_string(),
                object: "model".to_string(),
                created: 1776276000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.7 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-opus-4-6".to_string(),
                object: "model".to_string(),
                created: 1770163200,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.6".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-opus-4-6-thinking".to_string(),
                object: "model".to_string(),
                created: 1770163200,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.6 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-sonnet-4-6".to_string(),
                object: "model".to_string(),
                created: 1771286400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.6".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-sonnet-4-6-thinking".to_string(),
                object: "model".to_string(),
                created: 1771286400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-opus-4-5-20251101".to_string(),
                object: "model".to_string(),
                created: 1763942400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-opus-4-5-20251101-thinking".to_string(),
                object: "model".to_string(),
                created: 1763942400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-sonnet-4-5-20250929".to_string(),
                object: "model".to_string(),
                created: 1759104000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-sonnet-4-5-20250929-thinking".to_string(),
                object: "model".to_string(),
                created: 1759104000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-haiku-4-5-20251001".to_string(),
                object: "model".to_string(),
                created: 1760486400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
            Model {
                id: "claude-haiku-4-5-20251001-thinking".to_string(),
                object: "model".to_string(),
                created: 1760486400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
                context_window: None,
            },
        ]
    };

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// 计量归属上下文
///
/// 随请求一路带到收尾记账。这里存的是**下发给上游的**模型 ID（映射后）和会话 ID，
/// 而不是客户端侧的模型名——扣费倍率按上游模型算，账本必须用同一口径。
#[derive(Debug, Clone)]
struct UsageContext {
    /// 下发给上游的 modelId
    upstream_model_id: String,
    /// conversationState.conversationId
    conversation_id: String,
}

/// 用量上报器
///
/// 把「实际服务凭据 + 上游模型 ID + 会话 ID」与账本绑在一起，供各条响应链路
/// 在收尾时统一记账，避免三处链路各写一遍口径不一致的上报代码。
struct UsageReporter {
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    credential_id: u64,
    usage_ctx: UsageContext,
}

impl UsageReporter {
    fn new(
        provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
        credential_id: u64,
        usage_ctx: UsageContext,
    ) -> Self {
        Self {
            provider,
            credential_id,
            usage_ctx,
        }
    }

    /// 上报一次流式请求的用量
    fn report_stream(&self, ctx: &StreamContext) {
        self.provider
            .token_manager()
            .report_usage(&ctx.to_request_usage(
                Some(self.credential_id),
                Some(self.usage_ctx.conversation_id.clone()),
                self.usage_ctx.upstream_model_id.clone(),
                true,
            ));
    }
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<crate::anthropic::middleware::AllowedPools>,
    axum::extract::Extension(api_key_info): axum::extract::Extension<crate::anthropic::middleware::ApiKeyInfo>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let next_credential_name = if let Some(provider) = &state.kiro_provider {
        provider
            .token_manager()
            .peek_next_credential_name(
                Some(&payload.model),
                thinking_enabled,
                Some(&allowed_pools.0),
            )
            .unwrap_or_else(|| "None".to_string())
    } else {
        "None".to_string()
    };

    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        api_key_name = %api_key_info.name,
        credential_name = %next_credential_name,
        pools = ?allowed_pools.0,
        "Received POST /v1/messages request"
    );
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };


    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens, Some(&allowed_pools.0)).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    let additional_model_fields = get_additional_model_request_fields(&payload);

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: additional_model_fields,
    };

    // 计量口径：取下发给上游的真实 modelId 与会话 ID
    let usage_ctx = UsageContext {
        upstream_model_id: kiro_request
            .conversation_state
            .current_message
            .user_input_message
            .model_id
            .clone(),
        conversation_id: kiro_request.conversation_state.conversation_id.clone(),
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let turn = (payload.messages.len() + 1) / 2;
    if tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG {
        let _ = std::fs::create_dir_all("test-output");
        if let Ok(cc_req_json) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(format!("test-output/kiro_rs_cc_turn{}_req.json", turn), cc_req_json);
        }
        if let Ok(aws_req_json) = serde_json::to_string_pretty(&kiro_request) {
            let _ = std::fs::write(format!("test-output/kiro_rs_aws_turn{}_req.json", turn), aws_req_json);
        }
    }

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            Some(&allowed_pools.0),
            usage_ctx,
        )
        .await
    } else {
        // 非流式响应
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, thinking_enabled, tool_name_map, Some(&allowed_pools.0), usage_ctx).await
    }
}

/// 处理流式请求
///
/// 关键点：**立即**返回 SSE 响应头并下发 `message_start`，随后在流体内部才去
/// `await` 上游连接，期间用 ping 保活。这样可避免高 effort（如 xhigh）下上游首字节
/// 延迟数十秒时，客户端在「零字节、无 message_start、无 ping」的静默窗口里超时断开。
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    allowed_pools: Option<&[String]>,
    usage_ctx: UsageContext,
) -> Response {
    let stream = create_sse_stream(
        provider,
        request_body.to_string(),
        model.to_string(),
        input_tokens,
        thinking_enabled,
        tool_name_map,
        allowed_pools.map(|s| s.to_vec()),
        usage_ctx,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

/// 将上游错误转换为 Anthropic 流式 `error` 事件字节。
/// 流式请求一旦发出响应头便无法再改 HTTP 状态码，故以 SSE `error` 事件下发；
/// 分类与 `map_provider_error`（非流式路径）保持一致。
fn provider_error_sse(err: &Error) -> Bytes {
    let err_str = err.to_string();
    let (etype, message) = if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        (
            "invalid_request_error",
            "Context window is full. Reduce conversation history, system prompt, or tools.".to_string(),
        )
    } else if err_str.contains("Input is too long") {
        (
            "invalid_request_error",
            "Input is too long. Reduce the size of your messages.".to_string(),
        )
    } else {
        ("api_error", format!("上游 API 调用失败: {}", err))
    };
    let payload = json!({
        "type": "error",
        "error": { "type": etype, "message": message }
    });
    Bytes::from(format!("event: error\ndata: {}\n\n", payload))
}

/// 创建 SSE 事件流
///
/// 流程：先发 `message_start` → 等待上游连接（期间 ping 保活）→ 处理上游事件流
/// （期间继续 ping 保活）。上游连接失败时，由于响应头已发出无法再改 HTTP 状态码，
/// 改为下发 SSE `error` 事件（符合 Anthropic 流式错误语义，Claude Code 能识别）。
fn create_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    allowed_pools: Option<Vec<String>>,
    usage_ctx: UsageContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut ctx = StreamContext::new_with_thinking(&model, input_tokens, thinking_enabled, tool_name_map);

        // 1) 立即下发初始事件（message_start 等），让客户端尽快拿到响应头
        for e in ctx.generate_initial_events() {
            yield Ok(Bytes::from(e.to_sse_string()));
        }

        // 2) 等待上游响应头期间用 ping 保活（覆盖长 TTFB 静默窗口）
        let mut ping = interval(Duration::from_secs(PING_INTERVAL_SECS));
        ping.tick().await; // 跳过立即触发的首个 tick，使首个 ping 延后一个周期
        let connect = provider.call_api_stream(&request_body, allowed_pools.as_deref());
        tokio::pin!(connect);
        let response = loop {
            tokio::select! {
                res = &mut connect => match res {
                    Ok(resp) => break Some(resp),
                    Err(e) => {
                        tracing::error!("上游连接失败（流式，已发响应头）: {}", e);
                        yield Ok(provider_error_sse(&e));
                        break None;
                    }
                },
                _ = ping.tick() => {
                    yield Ok(create_ping_sse());
                }
            }
        };
        let Some(api_call) = response else { return };
        let credential_id = api_call.credential_id;

        // 3) 处理上游事件流，同时继续 ping 保活
        let body = api_call.response.bytes_stream();
        tokio::pin!(body);
        let mut decoder = EventStreamDecoder::new();
        loop {
            tokio::select! {
                chunk = body.next() => {
                    match chunk {
                        Some(Ok(chunk)) => {
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            for se in ctx.process_kiro_event(&event) {
                                                yield Ok(Bytes::from(se.to_sse_string()));
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!("解码事件失败: {}", e),
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            for se in ctx.generate_final_events() {
                                yield Ok(Bytes::from(se.to_sse_string()));
                            }
                            break;
                        }
                        None => {
                            for se in ctx.generate_final_events() {
                                yield Ok(Bytes::from(se.to_sse_string()));
                            }
                            break;
                        }
                    }
                }
                _ = ping.tick() => {
                    yield Ok(create_ping_sse());
                }
            }
        }

        // 4) 流收尾：把上游下发的扣费与实际服务模型记入账本
        // provider 仍被 connect future 借用，这里克隆 Arc 而非移动
        UsageReporter::new(provider.clone(), credential_id, usage_ctx).report_stream(&ctx);
    }
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    allowed_pools: Option<&[String]>,
    usage_ctx: UsageContext,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_call = match provider.call_api(request_body, allowed_pools).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };
    let credential_id = api_call.credential_id;

    // 读取响应体
    let body_bytes = match api_call.response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    let turn = serde_json::from_str::<serde_json::Value>(request_body)
        .ok()
        .and_then(|v| v.get("conversationState").and_then(|cs| cs.get("history")).and_then(|h| h.as_array().map(|arr| arr.len())))
        .map(|len| (len + 2) / 2)
        .unwrap_or(1);

    if tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG {
        let _ = std::fs::create_dir_all("test-output");
        let _ = std::fs::write(format!("test-output/kiro_rs_aws_turn{}_res.txt", turn), &body_bytes);
    }

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut thinking_content = String::new();
    let mut signature = None;
    let mut text_content = String::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;
    // 计量：上游下发的扣费、实际服务模型与输出规模
    let mut usage = crate::kiro::usage::RequestUsage {
        credential_id: Some(credential_id),
        conversation_id: Some(usage_ctx.conversation_id.clone()),
        requested_model: usage_ctx.upstream_model_id.clone(),
        stream: false,
        ..Default::default()
    };

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::ReasoningContent(resp) => {
                            usage.reasoning_chars += resp.text.chars().count() as u64;
                            thinking_content.push_str(&resp.text);
                            if let Some(ref sig) = resp.signature {
                                signature = Some(sig.clone());
                            }
                        }
                        Event::AssistantResponse(resp) => {
                            usage.answer_chars += resp.content.chars().count() as u64;
                            // modelId 每个 delta 都会带，只在首次取值
                            if usage.served_model.is_none() {
                                usage.served_model =
                                    resp.model_id.clone().filter(|m| !m.is_empty());
                            }
                            text_content.push_str(&resp.content);
                        }
                        Event::Metering(metering) => {
                            usage.credits = Some(metering.usage);
                            usage.unit = Some(metering.unit_label().to_string());
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;
                            // 工具调用的 input JSON 也是模型生成的输出，计入成本口径
                            usage.tool_chars += tool_use.input.chars().count() as u64;
                            if tool_use.stop {
                                usage.tool_chars += tool_use.name.chars().count() as u64;
                            }

                            // 累积工具的 JSON 输入
                            let buffer = tool_json_buffers
                                .entry(tool_use.tool_use_id.clone())
                                .or_insert_with(String::new);
                            buffer.push_str(&tool_use.input);

                            // 如果是完整的工具调用，添加到列表
                            if tool_use.stop {
                                let input: serde_json::Value = if buffer.is_empty() {
                                    serde_json::json!({})
                                } else {
                                    serde_json::from_str(buffer)
                                        .unwrap_or_else(|e| {
                                            tracing::warn!(
                                                "工具输入 JSON 解析失败: {}, tool_use_id: {}",
                                                e, tool_use.tool_use_id
                                            );
                                            serde_json::json!({})
                                        })
                                };

                                let original_name = tool_name_map
                                    .get(&tool_use.name)
                                    .cloned()
                                    .unwrap_or_else(|| tool_use.name.clone());

                                tool_uses.push(json!({
                                    "type": "tool_use",
                                    "id": tool_use.tool_use_id,
                                    "name": original_name,
                                    "input": input
                                }));
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            usage.context_usage_percentage =
                                Some(context_usage.context_usage_percentage);
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * (window_size as f64)
                                / 100.0)
                                as i32;
                            context_input_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                                context_usage.context_usage_percentage,
                                actual_input_tokens
                            );
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 事件流已处理完，把上游下发的扣费与实际服务模型记入账本
    provider.token_manager().report_usage(&usage);

    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();

    if thinking_enabled {
        if !thinking_content.is_empty() {
            let mut block = json!({
                "type": "thinking",
                "thinking": thinking_content
            });
            if let Some(ref sig) = signature {
                block["signature"] = json!(sig);
            }
            content.push(block);
        }
        if !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "output_tokens": output_tokens
        }
    });



    if tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG {
        let _ = std::fs::create_dir_all("test-output");
        if let Ok(cc_res_json) = serde_json::to_string_pretty(&response_body) {
            let _ = std::fs::write(format!("test-output/kiro_rs_cc_turn{}_res.json", turn), cc_res_json);
        }
    }

    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 =
        model_lower.contains("opus") && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });
    
    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// 判断是否为 Claude 模型且版本 <= 4.7
fn is_claude_model_lte_47(model_id: &str) -> bool {
    let model_lower = model_id.to_lowercase();

    // 只匹配 claude 家族的 opus/sonnet/haiku
    if !model_lower.contains("claude") {
        return false;
    }
    if !model_lower.contains("opus") && !model_lower.contains("sonnet") && !model_lower.contains("haiku") {
        return false;
    }

    // 复用 converter 中的版本提取逻辑
    let (major, minor) = crate::anthropic::converter::extract_version(model_id);

    // Claude 4.x 版本 <= 4.7
    if (major - 4.0).abs() < 0.001 {
        return minor <= 7.0;
    }
    // 其他版本（5.x 等）按新逻辑处理（不传递 thinking）
    false
}

pub fn get_additional_model_request_fields(payload: &MessagesRequest) -> Option<serde_json::Value> {
    // 1. 获取映射后的模型 ID
    let mapped_id = crate::anthropic::converter::map_model(&payload.model)?;

    // 2. 取模型元数据目录：优先全局动态目录，未加载时回退到与 map_model 一致的静态目录，
    //    保证 headless / 启动初期目录未就绪时也能正确下发 thinking/effort
    //    （进入/退出 fallback 会打印 info 日志，便于观测降级）
    let catalog = crate::anthropic::converter::active_catalog();
    {
        let model_meta = catalog.models.iter().find(|m| m.model_id == mapped_id)?;

        // 获取对应的扩展字段 schema properties
        let schema = model_meta.additional_model_request_fields_schema.as_ref()?;
        let schema_obj = schema.as_object()?;
        let properties = schema_obj.get("properties")?.as_object()?;

        // 判断是否为 Claude 模型且版本 <= 4.7
        let use_thinking_for_legacy = is_claude_model_lte_47(&payload.model);

        // schema 既不支持 output_config（Claude effort）也不支持 reasoning（gpt-5.x effort）
        // => 该模型无 effort 类扩展字段，直接不下发
        let has_output_config = properties.contains_key("output_config");
        let has_reasoning = properties.contains_key("reasoning");
        if !has_output_config && !has_reasoning {
            return None;
        }

        let mut fields = serde_json::Map::new();

        // 客户端请求的 effort（如 Claude Code 的 /effort → output_config.effort），缺省 high
        let effort = payload
            .output_config
            .as_ref()
            .map(|c| c.effort.clone())
            .unwrap_or_else(|| "high".to_string());

        if has_output_config {
            // Claude 家族：effort 走 output_config，按 schema enum 收紧
            let effort_prop = properties
                .get("output_config")
                .and_then(|p| p.get("properties"))
                .and_then(|p| p.get("effort"));
            let effort_valid =
                crate::anthropic::converter::clamp_effort_value(&effort, effort_prop);
            let mut output_config_obj = serde_json::Map::new();
            output_config_obj
                .insert("effort".to_string(), serde_json::Value::String(effort_valid));
            fields.insert(
                "output_config".to_string(),
                serde_json::Value::Object(output_config_obj),
            );
        } else if has_reasoning {
            // gpt-5.x 系列：effort 走 reasoning.effort，按 schema enum 收紧。
            let reasoning_prop = properties.get("reasoning").and_then(|p| p.get("properties"));
            let effort_prop = reasoning_prop.and_then(|p| p.get("effort"));
            let effort_valid =
                crate::anthropic::converter::clamp_effort_value(&effort, effort_prop);
            let mut reasoning_obj = serde_json::Map::new();
            reasoning_obj.insert("effort".to_string(), serde_json::Value::String(effort_valid));

            // mode：仅当模型名带 `-mode-pro` 后缀且 schema 声明支持 mode 时才下发 pro，
            // 按 schema enum 收紧（越界回退 standard）。不带后缀则不下发，由上游默认 standard。
            if crate::anthropic::converter::model_requests_pro_mode(&payload.model) {
                if let Some(mode_prop) = reasoning_prop.and_then(|p| p.get("mode")) {
                    let mode_valid = crate::anthropic::converter::clamp_enum_value(
                        "pro",
                        Some(mode_prop),
                        "standard",
                    );
                    reasoning_obj
                        .insert("mode".to_string(), serde_json::Value::String(mode_valid));
                }
            }

            fields.insert(
                "reasoning".to_string(),
                serde_json::Value::Object(reasoning_obj),
            );
        }

        // 对于 Claude 模型 <= 4.7，同时传递 thinking type
        if use_thinking_for_legacy && properties.contains_key("thinking") {
            if let Some(ref t) = payload.thinking {
                if t.is_enabled() {
                    let mut thinking_obj = serde_json::Map::new();
                    // 根据 schema 确定 thinking type
                    let thinking_type = if let Some(thinking_prop) = properties.get("thinking") {
                        if let Some(enum_vals) = thinking_prop.get("properties").and_then(|p| p.get("type")).and_then(|e| e.get("enum")).and_then(|ev| ev.as_array()) {
                            let mut has_adaptive = false;
                            for v in enum_vals {
                                if v.as_str() == Some("adaptive") {
                                    has_adaptive = true;
                                    break;
                                }
                            }
                            if has_adaptive { "adaptive" } else { "disabled" }
                        } else {
                            "adaptive"
                        }
                    } else {
                        "adaptive"
                    };
                    thinking_obj.insert("type".to_string(), serde_json::Value::String(thinking_type.to_string()));
                    fields.insert("thinking".to_string(), serde_json::Value::Object(thinking_obj));
                }
            }
        }

        Some(serde_json::Value::Object(fields))
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<crate::anthropic::middleware::AllowedPools>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens, Some(&allowed_pools.0)).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    let additional_model_fields = get_additional_model_request_fields(&payload);

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: additional_model_fields,
    };

    // 计量口径：取下发给上游的真实 modelId 与会话 ID
    let usage_ctx = UsageContext {
        upstream_model_id: kiro_request
            .conversation_state
            .current_message
            .user_input_message
            .model_id
            .clone(),
        conversation_id: kiro_request.conversation_state.conversation_id.clone(),
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应（缓冲模式）
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
            Some(&allowed_pools.0),
            usage_ctx,
        )
        .await
    } else {
        // 非流式响应
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, thinking_enabled, tool_name_map, Some(&allowed_pools.0), usage_ctx).await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    allowed_pools: Option<&[String]>,
    usage_ctx: UsageContext,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let api_call = match provider.call_api_stream(request_body, allowed_pools).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };
    let credential_id = api_call.credential_id;

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(
        api_call.response,
        ctx,
        UsageReporter::new(provider, credential_id, usage_ctx),
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    reporter: UsageReporter,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            reporter,
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, reporter)| async move {
            if finished {
                return None;
            }

            loop {
                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, reporter)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                reporter.report_stream(ctx.inner());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, reporter)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                reporter.report_stream(ctx.inner());
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, reporter)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}
