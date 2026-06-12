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
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
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
            // 基础模型
            list.push(Model {
                id: m.model_id.clone(),
                object: "model".to_string(),
                created: 1776276000,
                owned_by: "anthropic".to_string(),
                display_name: m.model_name.clone(),
                model_type: "chat".to_string(),
                max_tokens: m.token_limits.as_ref().and_then(|l| l.max_output_tokens).unwrap_or(64000),
            });

            // 检查 schema properties 中是否包含 thinking 或 output_config 相关的配置
            let supports_thinking = m.additional_model_request_fields_schema
                .as_ref()
                .and_then(|s| s.as_object())
                .and_then(|obj| obj.get("properties"))
                .and_then(|props| props.as_object())
                .map(|props| props.contains_key("thinking") || props.contains_key("output_config"))
                .unwrap_or(false);

            if supports_thinking {
                list.push(Model {
                    id: format!("{}-thinking", m.model_id),
                    object: "model".to_string(),
                    created: 1776276000,
                    owned_by: "anthropic".to_string(),
                    display_name: format!("{} (Thinking)", m.model_name),
                    model_type: "chat".to_string(),
                    max_tokens: m.token_limits.as_ref().and_then(|l| l.max_output_tokens).unwrap_or(64000),
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
            },
            Model {
                id: "claude-opus-4-7-thinking".to_string(),
                object: "model".to_string(),
                created: 1776276000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.7 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-opus-4-6".to_string(),
                object: "model".to_string(),
                created: 1770163200,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.6".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-opus-4-6-thinking".to_string(),
                object: "model".to_string(),
                created: 1770163200,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.6 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-sonnet-4-6".to_string(),
                object: "model".to_string(),
                created: 1771286400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.6".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-sonnet-4-6-thinking".to_string(),
                object: "model".to_string(),
                created: 1771286400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-opus-4-5-20251101".to_string(),
                object: "model".to_string(),
                created: 1763942400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-opus-4-5-20251101-thinking".to_string(),
                object: "model".to_string(),
                created: 1763942400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Opus 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-sonnet-4-5-20250929".to_string(),
                object: "model".to_string(),
                created: 1759104000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-sonnet-4-5-20250929-thinking".to_string(),
                object: "model".to_string(),
                created: 1759104000,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-haiku-4-5-20251001".to_string(),
                object: "model".to_string(),
                created: 1760486400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Haiku 4.5".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
            Model {
                id: "claude-haiku-4-5-20251001-thinking".to_string(),
                object: "model".to_string(),
                created: 1760486400,
                owned_by: "anthropic".to_string(),
                display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
                model_type: "chat".to_string(),
                max_tokens: 64000,
            },
        ]
    };

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
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

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
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
    let _ = std::fs::create_dir_all("test-output");
    if let Ok(cc_req_json) = serde_json::to_string_pretty(&payload) {
        let _ = std::fs::write(format!("test-output/kiro_rs_cc_turn{}_req.json", turn), cc_req_json);
    }
    if let Ok(aws_req_json) = serde_json::to_string_pretty(&kiro_request) {
        let _ = std::fs::write(format!("test-output/kiro_rs_aws_turn{}_req.json", turn), aws_req_json);
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
        )
        .await
    } else {
        // 非流式响应
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, thinking_enabled, tool_name_map).await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let stream = create_sse_stream(response, ctx, initial_events);

    // 返回 SSE 响应
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

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS))),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }

            // 使用 select! 同时等待数据和 ping 定时器
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                        None => {
                            // 流结束，发送最终事件
                            let final_events = ctx.generate_final_events();
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
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
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 读取响应体
    let body_bytes = match response.bytes().await {
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

    let _ = std::fs::create_dir_all("test-output");
    let _ = std::fs::write(format!("test-output/kiro_rs_aws_turn{}_res.txt", turn), &body_bytes);

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

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::ReasoningContent(resp) => {
                            thinking_content.push_str(&resp.text);
                            if let Some(ref sig) = resp.signature {
                                signature = Some(sig.clone());
                            }
                        }
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ToolUse(tool_use) => {
                            has_tool_use = true;

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



    let _ = std::fs::create_dir_all("test-output");
    if let Ok(cc_res_json) = serde_json::to_string_pretty(&response_body) {
        let _ = std::fs::write(format!("test-output/kiro_rs_cc_turn{}_res.json", turn), cc_res_json);
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
        
        // 判断请求中是否启用了 thinking
        if let Some(ref t) = payload.thinking {
            if t.is_enabled() {
                let mut fields = serde_json::Map::new();
                
                // 如果 schema properties 中支持 thinking
                if let Some(thinking_prop) = properties.get("thinking") {
                    let mut thinking_obj = serde_json::Map::new();
                    let thinking_type = if let Some(enum_vals) = thinking_prop.get("properties").and_then(|p| p.get("type")).and_then(|e| e.get("enum")).and_then(|ev| ev.as_array()) {
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
                    };
                    thinking_obj.insert("type".to_string(), serde_json::Value::String(thinking_type.to_string()));
                    fields.insert("thinking".to_string(), serde_json::Value::Object(thinking_obj));
                }
                
                // 如果 schema properties 中支持 output_config (effort)
                if let Some(output_config_prop) = properties.get("output_config") {
                    let effort = if t.thinking_type == "adaptive" {
                        payload
                            .output_config
                            .as_ref()
                            .map(|c| c.effort.clone())
                            .unwrap_or_else(|| "high".to_string())
                    } else {
                        if t.budget_tokens >= 4000 {
                            "max".to_string()
                        } else {
                            "high".to_string()
                        }
                    };
                    
                    // 校验 effort 是否在 schema 允许的 enum 中
                    let effort_valid = if let Some(effort_prop) = output_config_prop.get("properties").and_then(|p| p.get("effort")) {
                        if let Some(enum_vals) = effort_prop.get("enum").and_then(|e| e.as_array()) {
                            let mut matched = false;
                            for val in enum_vals {
                                if let Some(val_str) = val.as_str() {
                                    if val_str == effort {
                                        matched = true;
                                        break;
                                    }
                                }
                            }
                            if matched {
                                effort
                            } else {
                                effort_prop.get("default")
                                    .and_then(|d| d.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "high".to_string())
                            }
                        } else {
                            effort
                        }
                    } else {
                        effort
                    };
                    
                    let mut output_config_obj = serde_json::Map::new();
                    output_config_obj.insert("effort".to_string(), serde_json::Value::String(effort_valid));
                    fields.insert("output_config".to_string(), serde_json::Value::Object(output_config_obj));
                }
                
                if !fields.is_empty() {
                    return Some(serde_json::Value::Object(fields));
                }
            }
        }
        return None;
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

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
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
        )
        .await
    } else {
        // 非流式响应
        handle_non_stream_request(provider, &request_body, &payload.model, input_tokens, thinking_enabled, tool_name_map).await
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
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(model, estimated_input_tokens, thinking_enabled, tool_name_map);

    // 创建缓冲 SSE 流
    let stream = create_buffered_sse_stream(response, ctx);

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
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval)| async move {
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
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval)));
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
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）
                                let all_events = ctx.finish_and_get_all_events();
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}
