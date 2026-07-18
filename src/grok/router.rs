//! `/grok` 路由和客户端 API Key 认证。

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::{get, post},
};

use crate::anthropic::types::ErrorResponse;
use crate::common::auth;
use crate::model::api_key_manager::ApiKeyManager;

use super::files::{GrokFileStore, MAX_UPLOAD_BYTES};
use super::handlers::{
    count_tokens, delete_file, get_file, get_file_content, get_files, get_models,
    get_video_generation, post_files, post_image_edits, post_image_generations, post_messages,
    post_messages_cc, post_video_generations,
};
use super::provider::SharedGrokProvider;
use super::reasoning_sig::ReasoningSignatureCodec;

/// 包含 multipart boundary、文件名与表单字段后的总请求体上限。xAI 的文件
/// 本体可达 `MAX_UPLOAD_BYTES`，因此这里多留 2 MiB，避免合法 50 MiB 文件
/// 被代理层提前拒绝。
const MAX_BODY_SIZE: usize = MAX_UPLOAD_BYTES + 2 * 1024 * 1024;

#[derive(Clone)]
pub struct GrokAppState {
    pub api_key: String,
    pub provider: SharedGrokProvider,
    pub default_model: String,
    pub extract_thinking: bool,
    pub api_key_manager: Option<Arc<ApiKeyManager>>,
    /// 使用持久化 server-only 随机密钥的 HMAC codec；防止客户端篡改
    /// reasoning signature 中的凭据、模型和 encrypted content。
    pub reasoning_signatures: ReasoningSignatureCodec,
    /// Anthropic `file_id` 与创建它的 xAI credential 的持久化绑定。
    pub file_store: GrokFileStore,
}

impl GrokAppState {
    pub fn new(
        api_key: impl Into<String>,
        provider: SharedGrokProvider,
        default_model: impl Into<String>,
        extract_thinking: bool,
        api_key_manager: Option<Arc<ApiKeyManager>>,
    ) -> Self {
        let api_key = api_key.into();
        let reasoning_signatures = provider.token_manager().reasoning_signature_codec();
        Self {
            api_key,
            provider,
            default_model: default_model.into(),
            extract_thinking,
            api_key_manager,
            reasoning_signatures,
            file_store: GrokFileStore::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AllowedPools(pub Vec<String>);

#[derive(Debug, Clone)]
pub struct ApiKeyInfo {
    pub name: String,
    pub pools: Vec<String>,
}

pub fn create_router_with_provider(
    api_key: impl Into<String>,
    provider: SharedGrokProvider,
    default_model: impl Into<String>,
    extract_thinking: bool,
    api_key_manager: Option<Arc<ApiKeyManager>>,
) -> Router {
    let state = GrokAppState::new(
        api_key,
        provider,
        default_model,
        extract_thinking,
        api_key_manager,
    );
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/files", get(get_files).post(post_files))
        .route("/files/{file_id}/content", get(get_file_content))
        .route("/files/{file_id}", get(get_file).delete(delete_file))
        // Grok Build 的 Imagine 工具使用独立 xAI endpoint；这些路由保留
        // Anthropic `/messages` 兼容性，同时给需要媒体生成的调用方一个
        // 不伪造 content block 的 Build-style API。
        .route("/images/generations", post(post_image_generations))
        .route("/images/edits", post(post_image_edits))
        .route("/videos/generations", post(post_video_generations))
        .route("/videos/{request_id}", get(get_video_generation))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        // Claude Code 兼容的 base URL 有时会指向 `/grok/cc`，所以 Files API
        // 也作为同一存储的别名暴露，避免上传和 Messages 落在不同前缀。
        .route("/files", get(get_files).post(post_files))
        .route("/files/{file_id}/content", get(get_file_content))
        .route("/files/{file_id}", get(get_file).delete(delete_file))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

async fn auth_middleware(
    State(state): State<GrokAppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    if let Some(key) = auth::extract_api_key(&request) {
        if auth::constant_time_eq(&key, &state.api_key) {
            let info = ApiKeyInfo {
                name: "Master Key".to_string(),
                pools: vec!["default".to_string()],
            };
            request
                .extensions_mut()
                .insert(AllowedPools(info.pools.clone()));
            request.extensions_mut().insert(info);
            return next.run(request).await;
        }
        if let Some(manager) = &state.api_key_manager {
            if let Some(entry) = manager.find_active_entry(&key) {
                let info = ApiKeyInfo {
                    name: entry.name,
                    pools: entry.pools,
                };
                request
                    .extensions_mut()
                    .insert(AllowedPools(info.pools.clone()));
                request.extensions_mut().insert(info);
                return next.run(request).await;
            }
        }
    }
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse::authentication_error()),
    )
        .into_response()
}

fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
