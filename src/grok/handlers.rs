//! `/grok` Anthropic-compatible HTTP handlers。

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::time::interval;

use crate::anthropic::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
};
use crate::token;

use super::converter::{convert_request_for_credential, plan_request};
use super::files::{FileListQuery, FileMetadata, FileStoreError, MAX_UPLOAD_BYTES};
use super::media::{
    build_image_edit_body, build_image_generation_body, build_video_generation_body,
};
use super::model_catalog::{GrokApiBackend, GrokModelCatalog, ReasoningEffort};
use super::provider::GrokUpstreamResponse;
use super::router::{AllowedPools, ApiKeyInfo, GrokAppState};
use super::stream::{GrokStreamContext, XaiSseDecoder};

const PING_INTERVAL_SECS: u64 = 25;

/// POST /grok/v1/files
///
/// Anthropic Files API 形状的 multipart 上传。文件本体直接进入 xAI Files
/// storage；代理只保存 file_id 与创建凭据的映射，以保持多凭据安全。
pub async fn post_files(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    multipart: Multipart,
) -> Response {
    let (filename, mime_type, bytes) = match read_file_upload(multipart).await {
        Ok(upload) => upload,
        Err(error) => return file_store_error(error),
    };
    let size_bytes = bytes.len();
    let upstream = match state
        .provider
        .upload_file(&filename, &mime_type, bytes, Some(&allowed_pools.0))
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    let status = upstream.response.status();
    let credential_id = upstream.credential_id;
    let response_bytes = match upstream.response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return media_response_read_error(error),
    };
    let upstream_file = match serde_json::from_slice::<Value>(&response_bytes) {
        Ok(value) => value,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("xAI Files 上传响应不是有效 JSON: {error}"),
                )),
            )
                .into_response();
        }
    };
    let metadata = match FileMetadata::from_xai(&upstream_file, &filename, &mime_type, size_bytes) {
        Ok(metadata) => metadata,
        Err(error) => return file_store_error(error),
    };
    let pools = match state.provider.credential_pools(credential_id) {
        Ok(pools) => pools,
        Err(error) => return map_provider_error(error),
    };
    if let Err(error) = state
        .file_store
        .register(metadata.clone(), credential_id, pools)
    {
        // 上游上传已经成功但绑定持久化失败时，尽力清理文件，避免产生调用方
        // 无法再引用的 orphan；清理失败只记录，不覆盖真正的注册表错误。
        let path = format!("/files/{}", urlencoding::encode(&metadata.id));
        if let Ok(cleanup) = state
            .provider
            .call_public_api_for_credential(credential_id, reqwest::Method::DELETE, &path, None)
            .await
        {
            let _ = cleanup.response.bytes().await;
        }
        return file_store_error(error);
    }

    let mut response = Json(metadata).into_response();
    *response.status_mut() = status;
    response
}

/// GET /grok/v1/files
pub async fn get_files(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    Query(query): Query<FileListQuery>,
) -> Response {
    match state.file_store.list(&query, &allowed_pools.0) {
        Ok(files) => Json(files).into_response(),
        Err(error) => file_store_error(error),
    }
}

/// GET /grok/v1/files/{file_id}
pub async fn get_file(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    Path(file_id): Path<String>,
) -> Response {
    match state.file_store.metadata_for(&file_id, &allowed_pools.0) {
        Ok(metadata) => Json(metadata).into_response(),
        Err(error) => file_store_error(error),
    }
}

/// DELETE /grok/v1/files/{file_id}
pub async fn delete_file(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    Path(file_id): Path<String>,
) -> Response {
    let binding = match state.file_store.binding_for(&file_id, &allowed_pools.0) {
        Ok(binding) => binding,
        Err(error) => return file_store_error(error),
    };
    let path = format!("/files/{}", urlencoding::encode(&binding.metadata.id));
    let upstream = match state
        .provider
        .call_public_api_for_credential(binding.credential_id, reqwest::Method::DELETE, &path, None)
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    if let Err(error) = upstream.response.bytes().await {
        return media_response_read_error(error);
    }
    if let Err(error) = state
        .file_store
        .remove(&binding.metadata.id, &allowed_pools.0)
    {
        return file_store_error(error);
    }
    Json(json!({"id": binding.metadata.id, "type": "file_deleted"})).into_response()
}

/// GET /grok/v1/files/{file_id}/content
///
/// Anthropic 的上传文件 `downloadable=false`，只有 Skills/code execution 产生
/// 的文件可以下载。`/grok` 目前没有把模型输出登记为 Files，因此与官方语义
/// 一样拒绝下载调用方上传的文件。
pub async fn get_file_content(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    Path(file_id): Path<String>,
) -> Response {
    if let Err(error) = state.file_store.binding_for(&file_id, &allowed_pools.0) {
        return file_store_error(error);
    }
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new(
            "invalid_request_error",
            "上传到 /grok/v1/files 的文件 downloadable=false，不能通过 content endpoint 下载",
        )),
    )
        .into_response()
}

async fn read_file_upload(
    mut multipart: Multipart,
) -> Result<(String, String, Bytes), FileStoreError> {
    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        FileStoreError::InvalidRequest(format!("无法读取 multipart 文件字段: {error}"))
    })? {
        if field.name() != Some("file") {
            continue;
        }
        if upload.is_some() {
            return Err(FileStoreError::InvalidRequest(
                "一次 Files API 上传只能包含一个 file 字段".to_string(),
            ));
        }
        let filename = field
            .file_name()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("upload")
            .to_string();
        let content_type = field
            .content_type()
            .map(ToString::to_string)
            .unwrap_or_default();
        let bytes = field.bytes().await.map_err(|error| {
            FileStoreError::InvalidRequest(format!("无法读取上传文件内容: {error}"))
        })?;
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(FileStoreError::InvalidRequest(format!(
                "单个文件不能超过 {} MB",
                MAX_UPLOAD_BYTES / (1024 * 1024)
            )));
        }
        let mime_type = normalize_upload_mime_type(&content_type, &filename);
        upload = Some((filename, mime_type, bytes));
    }
    upload.ok_or_else(|| {
        FileStoreError::InvalidRequest(
            "Files API 需要 multipart/form-data 中名为 file 的文件字段".to_string(),
        )
    })
}

fn normalize_upload_mime_type(content_type: &str, filename: &str) -> String {
    let content_type = content_type
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| value.contains('/'));
    content_type
        .map(ToOwned::to_owned)
        .or_else(|| {
            mime_guess::from_path(filename)
                .first_raw()
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "application/octet-stream".to_string())
}

/// POST /grok/v1/images/generations
///
/// Build-style input: `{prompt, aspect_ratio?}`. 返回的 `data[].b64_json`
/// 是 xAI 原始响应；HTTP 服务无法像本地 Grok Build 一样写入调用方磁盘。
pub async fn post_image_generations(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    JsonExtractor(payload): JsonExtractor<Value>,
) -> Response {
    let body = match build_image_generation_body(&payload) {
        Ok(body) => body,
        Err(error) => return invalid_media_request(error),
    };
    forward_media_request(state, allowed_pools.0, "/images/generations", body).await
}

/// POST /grok/v1/images/edits
///
/// Build-style input: `{prompt, image: [data-url, ...], aspect_ratio?}`。
/// 单图和多图会按 Grok Build 分别转换为 xAI `image` / `images`；代理不能
/// 读取调用方本地文件或下载任意远程图，因此编辑参考图必须已是 data URL。
pub async fn post_image_edits(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    JsonExtractor(payload): JsonExtractor<Value>,
) -> Response {
    let body = match build_image_edit_body(&payload) {
        Ok(body) => body,
        Err(error) => return invalid_media_request(error),
    };
    forward_media_request(state, allowed_pools.0, "/images/edits", body).await
}

/// POST /grok/v1/videos/generations
///
/// 支持 Grok Build 的 image-to-video 与 reference-to-video 输入。响应中的
/// `request_id` 为代理 opaque id；用 GET `/grok/v1/videos/{request_id}`
/// 轮询即可拿到 xAI 的 `status` 与完成后的 `video.url`。
pub async fn post_video_generations(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    JsonExtractor(payload): JsonExtractor<Value>,
) -> Response {
    let body = match build_video_generation_body(&payload) {
        Ok(body) => body,
        Err(error) => return invalid_media_request(error),
    };
    let upstream = match state
        .provider
        .call_public_api(
            reqwest::Method::POST,
            "/videos/generations",
            Some(&body),
            Some(&allowed_pools.0),
        )
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    let status = upstream.response.status();
    let credential_id = upstream.credential_id;
    let bytes = match upstream.response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return media_response_read_error(error),
    };
    let mut response = match serde_json::from_slice::<Value>(&bytes) {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("xAI 视频生成返回了无法解析的 JSON: {error}"),
                )),
            )
                .into_response();
        }
    };
    let upstream_request_id = match response
        .get("request_id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    {
        Some(request_id) => request_id,
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    "xAI 视频生成响应缺少 request_id",
                )),
            )
                .into_response();
        }
    };
    let request_id = match state
        .provider
        .register_video_job(&upstream_request_id, credential_id)
    {
        Ok(request_id) => request_id,
        Err(error) => return map_provider_error(error),
    };
    let Some(object) = response.as_object_mut() else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                "xAI 视频生成响应必须是 JSON 对象",
            )),
        )
            .into_response();
    };
    object.insert("request_id".to_string(), Value::String(request_id));
    let mut client_response = Json(response).into_response();
    *client_response.status_mut() = status;
    client_response
}

/// GET /grok/v1/videos/{request_id}
pub async fn get_video_generation(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    Path(request_id): Path<String>,
) -> Response {
    let upstream = match state
        .provider
        .poll_video_job(&request_id, &allowed_pools.0)
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    public_json_response(upstream).await
}

/// GET /grok/v1/models
pub async fn get_models(State(state): State<GrokAppState>) -> impl IntoResponse {
    let catalog = state
        .provider
        .model_catalog()
        .map(|catalog| (*catalog).clone())
        .unwrap_or_else(GrokModelCatalog::bootstrap);
    let model_list = catalog
        .models
        .into_iter()
        .filter(|model| model.supported_in_api)
        .map(|model| Model {
            id: model.model_id,
            object: "model".to_string(),
            created: 1_772_000_000,
            owned_by: "xai".to_string(),
            display_name: model.model_name,
            model_type: "chat".to_string(),
            max_tokens: model.max_completion_tokens.unwrap_or(16_384),
        })
        .collect::<Vec<_>>();
    tracing::debug!(
        default_model = %state.default_model,
        model_count = model_list.len(),
        "返回 Grok Build 凭据模型目录"
    );
    Json(ModelsResponse {
        object: "list".to_string(),
        data: model_list,
    })
}

/// POST /grok/v1/messages/count_tokens
pub async fn count_tokens(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> Response {
    // Token 估算器本身不展开文件字节，但仍检查 file_id 存在、可访问且来自
    // 同一凭据，确保 count_tokens 与真正 Messages 请求的权限语义一致。
    if let Err(error) = state
        .file_store
        .credential_for_messages(&payload.messages, &allowed_pools.0)
    {
        return file_store_error(error);
    }
    let input_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;
    Json(CountTokensResponse { input_tokens }).into_response()
}

/// POST /grok/v1/messages
pub async fn post_messages(
    State(state): State<GrokAppState>,
    axum::extract::Extension(allowed_pools): axum::extract::Extension<AllowedPools>,
    axum::extract::Extension(api_key_info): axum::extract::Extension<ApiKeyInfo>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    let file_credential_id = match state
        .file_store
        .credential_for_messages(&payload.messages, &allowed_pools.0)
    {
        Ok(credential_id) => credential_id,
        Err(error) => return file_store_error(error),
    };
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 1) 用并集/bootstrap catalog 做模型别名与能力规划（不构建上游 body）。
    let merged_catalog = state.provider.model_catalog();
    let plan = match plan_request(&payload, &state.default_model, merged_catalog.as_deref()) {
        Ok(plan) => plan,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    error.to_string(),
                )),
            )
                .into_response();
        }
    };

    // 2) 选定路由凭据，并用**该凭据**的 catalog 做 wire 转换，保证
    //    backend/body 与真正发送账号一致（修复合并目录 apiBackend 误导）。
    let routing_credential_id = match file_credential_id {
        Some(id) => Some(id),
        None => state
            .provider
            .token_manager()
            .find_routing_credential_id(
                Some(&plan.model),
                plan.reasoning_effort,
                plan.backend_constraint(),
                plan.needs_web_search,
                Some(&allowed_pools.0),
            )
            .ok(),
    };
    let routing_catalog = routing_credential_id
        .and_then(|id| state.provider.token_manager().catalog_for(id))
        .or_else(|| merged_catalog.clone());
    // 规范化为 wire model id，避免目标凭据 catalog 只有正式 id、没有别名。
    payload.model = plan.model.clone();

    let converted = match convert_request_for_credential(
        &payload,
        &state.default_model,
        routing_catalog.as_deref(),
        routing_credential_id,
    ) {
        Ok(converted) => converted,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    error.to_string(),
                )),
            )
                .into_response();
        }
    };

    let credential_name = routing_credential_id
        .and_then(|id| {
            state
                .provider
                .token_manager()
                .credential_display_name(id)
        })
        .or_else(|| {
            state.provider.token_manager().peek_next_credential_name(
                Some(&converted.model),
                converted.reasoning_effort,
                Some(converted.backend),
                converted.uses_hosted_web_search,
                Some(&allowed_pools.0),
            )
        })
        .unwrap_or_else(|| "None".to_string());
    tracing::info!(
        model = %payload.model,
        resolved_model = %converted.model,
        backend = %converted.backend.as_str(),
        stream = payload.stream,
        message_count = payload.messages.len(),
        api_key_name = %api_key_info.name,
        credential_name = %credential_name,
        routing_credential_id = ?routing_credential_id,
        pools = ?allowed_pools.0,
        "Received POST /grok/v1/messages request"
    );

    let mut body = converted.body;
    if converted.backend == GrokApiBackend::Messages {
        // Messages backend 本身就是 Anthropic 协议，直接透传其 SSE/JSON；请求
        // 体中的 thinking/output_config 已按 Grok Build 的 summarized 规则重建。
        body["stream"] = Value::Bool(payload.stream);
        return messages_backend_response(
            state,
            body,
            converted.model,
            converted.reasoning_effort,
            payload.stream,
            allowed_pools.0,
            file_credential_id,
        )
        .await;
    }
    // Responses 和 Chat Completions 均统一向上游请求 SSE，再聚合为调用方要
    // 求的 Anthropic 流式或非流式格式。
    body["stream"] = Value::Bool(true);

    if payload.stream {
        stream_response(
            state,
            body,
            converted.model,
            input_tokens,
            converted.thinking_enabled,
            converted.backend,
            converted.reasoning_effort,
            converted.uses_hosted_web_search,
            allowed_pools.0,
            file_credential_id,
        )
        .await
    } else {
        non_stream_response(
            state,
            body,
            converted.model,
            input_tokens,
            converted.thinking_enabled,
            converted.backend,
            converted.reasoning_effort,
            converted.uses_hosted_web_search,
            &allowed_pools.0,
            file_credential_id,
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
    backend: GrokApiBackend,
    reasoning_effort: Option<ReasoningEffort>,
    uses_hosted_web_search: bool,
    allowed_pools: Vec<String>,
    file_credential_id: Option<u64>,
) -> Response {
    let stream = create_sse_stream(
        state,
        body,
        model,
        input_tokens,
        thinking_enabled,
        backend,
        reasoning_effort,
        uses_hosted_web_search,
        allowed_pools,
        file_credential_id,
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
    backend: GrokApiBackend,
    reasoning_effort: Option<ReasoningEffort>,
    uses_hosted_web_search: bool,
    allowed_pools: Vec<String>,
    file_credential_id: Option<u64>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut context = GrokStreamContext::new(model.clone(), input_tokens, thinking_enabled);
        for event in context.initial_events() {
            yield Ok(Bytes::from(event.to_sse_string()));
        }

        let mut ping = interval(Duration::from_secs(PING_INTERVAL_SECS));
        ping.tick().await;
        let connect = state.provider.call_api(
            &body,
            backend,
            &model,
            reasoning_effort,
            uses_hosted_web_search,
            Some(&allowed_pools),
            file_credential_id,
        );
        tokio::pin!(connect);
        let response = loop {
            tokio::select! {
                result = &mut connect => match result {
                    Ok(upstream) => {
                        context.set_credential_id(upstream.credential_id);
                        break Some(upstream.response);
                    }
                    Err(error) => {
                        // 已发 message_start：先 error，再收尾未关闭块 / message_stop，
                        // 避免部分客户端挂在半开 message 上。
                        yield Ok(provider_error_sse(&error));
                        for event in context.finish_events() {
                            yield Ok(Bytes::from(event.to_sse_string()));
                        }
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
                                for event in context.finish_events() {
                                    yield Ok(Bytes::from(event.to_sse_string()));
                                }
                                return;
                            }
                            for event in context.process_event(&upstream_event) {
                                yield Ok(Bytes::from(event.to_sse_string()));
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%error, "读取 xAI SSE 响应失败");
                        yield Ok(upstream_error_sse(&format!(
                            "读取 xAI SSE 响应失败: {error}"
                        )));
                        for event in context.finish_events() {
                            yield Ok(Bytes::from(event.to_sse_string()));
                        }
                        return;
                    }
                    None => break,
                },
                _ = ping.tick() => yield Ok(ping_sse()),
            }
        }
        for upstream_event in decoder.finish() {
            if let Some(message) = upstream_error_message(&upstream_event) {
                yield Ok(upstream_error_sse(&message));
                for event in context.finish_events() {
                    yield Ok(Bytes::from(event.to_sse_string()));
                }
                return;
            }
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
    backend: GrokApiBackend,
    reasoning_effort: Option<ReasoningEffort>,
    uses_hosted_web_search: bool,
    allowed_pools: &[String],
    file_credential_id: Option<u64>,
) -> Response {
    let upstream = match state
        .provider
        .call_api(
            &body,
            backend,
            &model,
            reasoning_effort,
            uses_hosted_web_search,
            Some(allowed_pools),
            file_credential_id,
        )
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    let credential_id = upstream.credential_id;
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
    context.set_credential_id(credential_id);
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
            // Chat Completions 的单 JSON 响应直接带 `choices`；Responses
            // 则继续走 output-item 聚合。
            if response.get("choices").is_some() {
                context.process_event(&response);
            } else {
                let response = response.get("response").unwrap_or(&response);
                context.ingest_completed_response(response);
            }
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

/// catalog 标记为 `messages` 的模型已经使用 Anthropic wire protocol；请求/响应
/// 不应先绕一层 Responses 再反向转换。只在模型、effort、display 字段处做
/// Grok Build 语义适配，其余 SSE 原样转发给调用方。
async fn messages_backend_response(
    state: GrokAppState,
    body: Value,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
    stream_requested: bool,
    allowed_pools: Vec<String>,
    file_credential_id: Option<u64>,
) -> Response {
    if stream_requested {
        let stream = create_messages_sse_stream(
            state,
            body,
            model,
            reasoning_effort,
            allowed_pools,
            file_credential_id,
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|error| {
                tracing::error!(%error, "构建 Grok Messages SSE 响应失败");
                Response::new(Body::empty())
            });
    }
    let upstream = match state
        .provider
        .call_api(
            &body,
            GrokApiBackend::Messages,
            &model,
            reasoning_effort,
            false,
            Some(&allowed_pools),
            file_credential_id,
        )
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    match upstream.response.bytes().await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .unwrap_or_else(|error| {
                tracing::error!(%error, "构建 Grok Messages JSON 响应失败");
                Response::new(Body::empty())
            }),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                format!("读取 xAI Messages 响应失败: {error}"),
            )),
        )
            .into_response(),
    }
}

fn create_messages_sse_stream(
    state: GrokAppState,
    body: Value,
    model: String,
    reasoning_effort: Option<ReasoningEffort>,
    allowed_pools: Vec<String>,
    file_credential_id: Option<u64>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    async_stream::stream! {
        let mut ping = interval(Duration::from_secs(PING_INTERVAL_SECS));
        ping.tick().await;
        let connect = state.provider.call_api(
            &body,
            GrokApiBackend::Messages,
            &model,
            reasoning_effort,
            false,
            Some(&allowed_pools),
            file_credential_id,
        );
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
        let body_stream = response.bytes_stream();
        tokio::pin!(body_stream);
        loop {
            tokio::select! {
                chunk = body_stream.next() => match chunk {
                    Some(Ok(chunk)) => yield Ok(chunk),
                    Some(Err(error)) => {
                        tracing::warn!(%error, "读取 xAI Messages SSE 响应失败");
                        yield Ok(upstream_error_sse(&format!("读取 xAI Messages SSE 响应失败: {error}")));
                        return;
                    }
                    None => break,
                },
                _ = ping.tick() => yield Ok(ping_sse()),
            }
        }
    }
}

fn map_provider_error(error: anyhow::Error) -> Response {
    let message = error.to_string();
    let status = if message.contains("无权访问") {
        StatusCode::FORBIDDEN
    } else if message.contains("视频任务不存在或已过期") || message.contains("请求失败: 404")
    {
        StatusCode::NOT_FOUND
    } else if message.contains("没有可用") || message.contains("未配置 Grok 凭据") {
        StatusCode::SERVICE_UNAVAILABLE
    } else if message.contains("请求失败: 400")
        || message.contains("请求失败: 413")
        || message.contains("请求失败: 422")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, Json(ErrorResponse::new("api_error", message))).into_response()
}

fn file_store_error(error: FileStoreError) -> Response {
    let (status, error_type) = match &error {
        FileStoreError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found_error"),
        FileStoreError::Registry(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        FileStoreError::UpstreamResponse(_) => (StatusCode::BAD_GATEWAY, "api_error"),
        FileStoreError::InvalidRequest(_)
        | FileStoreError::UnsupportedContentBlock(_)
        | FileStoreError::UnsupportedScope
        | FileStoreError::MixedCredentialFiles => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
    };
    (
        status,
        Json(ErrorResponse::new(error_type, error.to_string())),
    )
        .into_response()
}

async fn forward_media_request(
    state: GrokAppState,
    allowed_pools: Vec<String>,
    path: &str,
    body: Value,
) -> Response {
    let upstream = match state
        .provider
        .call_public_api(
            reqwest::Method::POST,
            path,
            Some(&body),
            Some(&allowed_pools),
        )
        .await
    {
        Ok(upstream) => upstream,
        Err(error) => return map_provider_error(error),
    };
    public_json_response(upstream).await
}

async fn public_json_response(upstream: GrokUpstreamResponse) -> Response {
    let status = upstream.response.status();
    let content_type = upstream
        .response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    match upstream.response.bytes().await {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(bytes))
            .unwrap_or_else(|error| {
                tracing::error!(%error, "构建 Grok Imagine 响应失败");
                Response::new(Body::empty())
            }),
        Err(error) => media_response_read_error(error),
    }
}

fn invalid_media_request(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new(
            "invalid_request_error",
            error.to_string(),
        )),
    )
        .into_response()
}

fn media_response_read_error(error: reqwest::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("读取 xAI Imagine 响应失败: {error}"),
        )),
    )
        .into_response()
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
        assert_eq!(
            upstream_error_message(&event).as_deref(),
            Some("upstream failed")
        );
    }
}
