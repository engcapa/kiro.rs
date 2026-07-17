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

use super::handlers::{count_tokens, get_models, post_messages, post_messages_cc};
use super::provider::SharedGrokProvider;

const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

#[derive(Clone)]
pub struct GrokAppState {
    pub api_key: String,
    pub provider: SharedGrokProvider,
    pub default_model: String,
    pub extract_thinking: bool,
    pub api_key_manager: Option<Arc<ApiKeyManager>>,
}

impl GrokAppState {
    pub fn new(
        api_key: impl Into<String>,
        provider: SharedGrokProvider,
        default_model: impl Into<String>,
        extract_thinking: bool,
        api_key_manager: Option<Arc<ApiKeyManager>>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            provider,
            default_model: default_model.into(),
            extract_thinking,
            api_key_manager,
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
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
