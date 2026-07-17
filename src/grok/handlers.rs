//! `/grok` Anthropic-compatible HTTP handlers。

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::time::interval;

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    Thinking,
};
use crate::token;

use super::converter::{ConversionError, GROK_BUILD_MODELS, convert_request};
use super::router::{AllowedPools, ApiKeyInfo, GrokAppState};
use super::stream::{GrokStreamContext, XaiSseDecoder};

const PING_INTERVAL_SECS: u64 = 25;

/// GET /grok/v1/models
pub async fn get_models(State(state): State<GrokAppState>) -> impl IntoResponse {
    let model_list = GROK_BUILD_MODELS
        .iter()
        .map(|id| Model {
            id: (*id).to_string(),
            object: "model".to_string(),
            created: 1_772_000_000,
            owned_by: "xai".to_string(),
            display_name: if *id == "grok-4.5" {
                "Grok 4.5 (Grok Build)".to_string()
            } else {
                (*id).to_string()
            },
            model_type: "chat".to_string(),
            max_tokens: if *id == "grok-4.5" { 32_768 } else { 16_384 },
        })
        .collect::<Vec<_>>();
    tracing::debug!(default_model = %state.default_model, "返回 Grok Build 模型列表");
    Json(ModelsResponse {
        object: "list".to_string(),
        data: model_list,
    })
}

/// POST /grok/v1/messages/count_tokens
pub async fn count_tokens(
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    let input_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;
    Json(CountTokensResponse { input_tokens })
}

/// POST /grok/v1/messages
pub async fn post_messages(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    axum::extract::Extension(api_key_info): axum::extract::Extension<ApiKeyInfo>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    override_thinking_from_model_name(&mut payload);
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let next_credential = state
        .provider
        .token_manager()
        .peek_next_credential_name(Some(&allowed_pools.0))
        .unwrap_or_else(|| "None".to_string());
    tracing::info!(
        model = %payload.model,
        stream = payload.stream,
        message_count = payload.messages.len(),
        api_key_name = %api_key_info.name,
        credential_name = %next_credential,
        pools = ?allowed_pools.0,
        "Received POST /grok/v1/messages request"
    );

    let converted = match convert_request(&payload, &state.default_model) {
        Ok(converted) => converted,
        Err(ConversionError::EmptyMessages) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new("invalid_request_error", "消息列表为空")),
            )
                .into_response();
        }
    };

    let mut body = converted.body;
    // Grok CLI / xAI Responses API 的非流式也按 SSE 聚合，避免不同模型在
    // `/responses` 上返回格式不一致；本服务再按客户端 `stream` 返回。
    body["stream"] = Value::Bool(true);

    if payload.stream {
        stream_response(
            state,
            body,
            converted.model,
            input_tokens,
            converted.thinking_enabled,
            allowed_pools.0,
        )
        .await
    } else {
        non_stream_response(
            state,
            body,
            converted.model,
            input_tokens,
            converted.thinking_enabled,
            &allowed_pools.0,
        )
        .await
    }
}

/// POST /grok/cc/v1/messages
///
/// Grok 没有 Kiro 的 contextUsageEvent，因此 Claude Code 兼容路由与标准
/// 路由共享同一转换器；仍保留独立路径以保持原工程的对外接口对称性。
pub async fn post_messages_cc(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    axum::extract::Extension(api_key_info): axum::extract::Extension<ApiKeyInfo>,
    JsonExtractor(payload): JsonExtractor<MessagesRequest>,
) -> Response {
    post_messages(
        State(state),
        axum::extract::Extension(allowed_pools),
        axum::extract::Extension(api_key_info),
        JsonExtractor(payload),
    )
    .await
}

async fn stream_response(
    state: GrokAppState,
    body: Value,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    allowed_pools: Vec<String>,
) -> Response {
    let stream = create_sse_stream(
        state,
        body,
        model,
        input_tokens,
        thinking_enabled,
        allowed_pools,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|error| {
            tracing::error!(%error, "构建 Grok SSE 响应失败");
            Response::new(Body::empty())
        })
}

fn create_sse_stream(
    state: GrokAppState,
    body: Value,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    allowed_pools: Vec<String>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut context = GrokStreamContext::new(model, input_tokens, thinking_enabled);
        for event in context.initial_events() {
            yield Ok(Bytes::from(event.to_sse_string()));
        }

        let mut ping = interval(Duration::from_secs(PING_INTERVAL_SECS));
        ping.tick().await;
        let connect = state.provider.call_responses(&body, Some(&allowed_pools));
        tokio::pin!(connect);
        let response = loop {
            tokio::select! {
                result = &mut connect => match result {
                    Ok(response) => break Some(response.response),
                    Err(error) => {
                        yield Ok(provider_error_sse(&error));
                        break None;
                    }
                },
                _ = ping.tick() => yield Ok(ping_sse()),
            }
        };
        let Some(response) = response else { return };

        let mut decoder = XaiSseDecoder::default();
        let body_stream = response.bytes_stream();
        tokio::pin!(body_stream);
        loop {
            tokio::select! {
                chunk = body_stream.next() => match chunk {
                    Some(Ok(chunk)) => {
                        for upstream_event in decoder.feed(&chunk) {
                            if let Some(message) = upstream_error_message(&upstream_event) {
                                yield Ok(upstream_error_sse(&message));
                                return;
                            }
                            for event in context.process_event(&upstream_event) {
                                yield Ok(Bytes::from(event.to_sse_string()));
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "读取 xAI SSE 响应失败");
                        break;
                    }
                    None => break,
                },
                _ = ping.tick() => yield Ok(ping_sse()),
            }
        }
        for upstream_event in decoder.finish() {
            for event in context.process_event(&upstream_event) {
                yield Ok(Bytes::from(event.to_sse_string()));
            }
        }
        for event in context.finish_events() {
            yield Ok(Bytes::from(event.to_sse_string()));
        }
    }
}

async fn non_stream_response(
    state: GrokAppState,
    body: Value,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    allowed_pools: &[String],
) -> Response {
    let upstream = match state
        .provider
        .call_responses(&body, Some(allowed_pools))
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    let bytes = match upstream.response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取 xAI 响应失败: {error}"),
                )),
            )
                .into_response();
        }
    };

    let mut context = GrokStreamContext::new(model, input_tokens, thinking_enabled);
    let mut decoder = XaiSseDecoder::default();
    let events = decoder.feed(&bytes);
    for event in events.into_iter().chain(decoder.finish()) {
        if let Some(message) = upstream_error_message(&event) {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("api_error", message)),
            )
                .into_response();
        }
        context.process_event(&event);
    }
    if !context.completed() {
        if let Ok(response) = serde_json::from_slice::<Value>(&bytes) {
            let response = response.get("response").unwrap_or(&response);
            context.ingest_completed_response(response);
        }
    }
    if !context.completed() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                "xAI 响应在收到 response.completed 前结束",
            )),
        )
            .into_response();
    }
    Json(context.to_anthropic_response()).into_response()
}

fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    if payload.model.to_ascii_lowercase().ends_with("-thinking") && payload.thinking.is_none() {
        payload.thinking = Some(Thinking {
            thinking_type: "enabled".to_string(),
            budget_tokens: 20_000,
        });
    }
}

fn map_provider_error(error: anyhow::Error) -> Response {
    let message = error.to_string();
    let status = if message.contains("没有可用") || message.contains("未配置 Grok 凭据") {
        StatusCode::SERVICE_UNAVAILABLE
    } else if message.contains("请求失败: 400") || message.contains("请求失败: 422") {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, Json(ErrorResponse::new("api_error", message))).into_response()
}

fn ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\":\"ping\"}\n\n")
}

fn provider_error_sse(error: &anyhow::Error) -> Bytes {
    upstream_error_sse(&format!("xAI 上游调用失败: {error}"))
}

fn upstream_error_sse(message: &str) -> Bytes {
    Bytes::from(format!(
        "event: error\ndata: {}\n\n",
        json!({ "type": "error", "error": { "type": "api_error", "message": message } })
    ))
}

fn upstream_error_message(event: &Value) -> Option<String> {
    let event_type = event.get("type").and_then(Value::as_str);
    let is_error = event_type == Some("error")
        || event_type == Some("response.failed")
        || event.get("error").is_some()
        || event.pointer("/response/error").is_some();
    if !is_error {
        return None;
    }
    let error = event
        .get("error")
        .or_else(|| event.pointer("/response/error"))
        .unwrap_or(event);
    Some(
        error
            .get("message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_nested_responses_failure() {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": { "error": { "message": "upstream failed" } }
        });
        assert_eq!(upstream_error_message(&event).as_deref(), Some("upstream failed"));
    }
}
