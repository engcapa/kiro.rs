//! `/grok/api/admin` 管理接口。
//!
//! 路径和原有 `/api/admin` 尽量保持对称，但凭据字段改为 xAI token / OAuth
//! 语义；两套凭据文件、故障计数和负载均衡状态完全独立。

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::admin::types::{
    AddApiKeyRequest, AdminErrorResponse, ApiKeyListResponse, SetDisabledRequest,
    SetLoadBalancingModeRequest, SetNameRequest, SetPoolsRequest, SetPriorityRequest,
    SuccessResponse, UpdateApiKeyRequest,
};
use crate::common::auth;
use crate::model::api_key_manager::ApiKeyManager;

use super::credentials::GrokCredentials;
use super::model_catalog::{GrokModelCatalog, ReasoningEffort};
use super::oauth::{GrokOAuthService, OAuthStartResponse, OAuthStatusResponse};
use super::provider::SharedGrokProvider;
use super::token_manager::{GrokManagerSnapshot, SharedGrokTokenManager};

#[derive(Clone)]
pub struct GrokAdminState {
    admin_api_key: String,
    service: Arc<GrokAdminService>,
}

impl GrokAdminState {
    pub fn new(admin_api_key: impl Into<String>, service: GrokAdminService) -> Self {
        Self {
            admin_api_key: admin_api_key.into(),
            service: Arc::new(service),
        }
    }
}

pub struct GrokAdminService {
    token_manager: SharedGrokTokenManager,
    provider: SharedGrokProvider,
    api_key_manager: Arc<ApiKeyManager>,
    oauth: GrokOAuthService,
}

impl GrokAdminService {
    pub fn new(
        token_manager: SharedGrokTokenManager,
        provider: SharedGrokProvider,
        api_key_manager: Arc<ApiKeyManager>,
        oauth: GrokOAuthService,
    ) -> Self {
        Self {
            token_manager,
            provider,
            api_key_manager,
            oauth,
        }
    }

    pub fn api_key_manager(&self) -> &Arc<ApiKeyManager> {
        &self.api_key_manager
    }

    fn credentials(&self) -> GrokManagerSnapshot {
        self.token_manager.snapshot()
    }

    fn credentials_status(&self) -> GrokCredentialsStatusResponse {
        let snapshot = self.credentials();
        GrokCredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials: snapshot.entries,
        }
    }

    async fn add_credential(
        &self,
        request: AddGrokCredentialRequest,
    ) -> anyhow::Result<AddGrokCredentialResponse> {
        let mut credential = GrokCredentials {
            name: non_empty(request.name),
            access_token: non_empty(request.access_token),
            refresh_token: non_empty(request.refresh_token),
            id_token: non_empty(request.id_token),
            token_type: non_empty(request.token_type),
            expires_at: non_empty(request.expires_at),
            auth_method: non_empty(request.auth_method),
            email: non_empty(request.email),
            subject: non_empty(request.subject),
            user_id: non_empty(request.user_id),
            team_id: non_empty(request.team_id),
            base_url: non_empty(request.base_url),
            token_endpoint: non_empty(request.token_endpoint),
            priority: request.priority,
            proxy_url: non_empty(request.proxy_url),
            proxy_username: non_empty(request.proxy_username),
            proxy_password: non_empty(request.proxy_password),
            pools: (!request.pools.is_empty()).then_some(request.pools),
            ..Default::default()
        };
        credential.canonicalize();
        let id = self.token_manager.add_credential(credential)?;
        // 只导入 refreshToken 的 AIClient-2-API 格式也可直接使用；这里主动
        // 刷新一次取得 accessToken。失败时保留凭据，管理员可修正后重试刷新。
        if self.token_manager.credential(id).is_some_and(|credential| {
            credential.access_token.is_none() && credential.refresh_token.is_some()
        }) {
            self.token_manager.force_refresh_token_for(id).await?;
        }
        // 新导入的凭据立即尝试拉取其真实 catalog；失败不影响凭据保存，避免
        // `/models` 控制平面短暂不可达时让管理员重复导入。
        if let Err(error) = self.provider.get_model_catalog_for(id, true).await {
            tracing::warn!(credential_id = id, %error, "新增 Grok 凭据后拉取模型目录失败");
        }
        let snapshot = self.token_manager.snapshot();
        let credential = snapshot
            .entries
            .iter()
            .find(|credential| credential.id == id)
            .ok_or_else(|| anyhow::anyhow!("新增 Grok 凭据后未找到凭据 #{}", id))?;
        Ok(AddGrokCredentialResponse {
            success: true,
            message: format!("Grok 凭据添加成功，ID: {id}"),
            credential_id: id,
            name: credential.name.clone(),
            email: credential.email.clone(),
            user_name: credential.user_name.clone(),
            imported_at: credential.imported_at.clone(),
        })
    }

    async fn balance(&self, id: u64) -> anyhow::Result<GrokBalanceResponse> {
        let credential = self
            .token_manager
            .credential(id)
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
        // xAI API Token 不能调用 Grok CLI 的订阅 billing endpoint。对这类
        // 凭据复用 `/models` 验活结果，使镜像 Admin UI 的“查询/验活”操作
        // 仍可正常完成；OAuth 凭据则返回真实 CLI billing 数据。
        let upstream = if credential.is_oauth() {
            self.provider.billing_for(id).await?
        } else {
            self.provider.verify_credential(id).await?
        };
        let config = upstream.get("config").unwrap_or(&upstream);
        let usage_percent = config
            .get("creditUsagePercent")
            .or_else(|| config.get("usage_percent"))
            .and_then(number_value)
            .unwrap_or(0.0)
            .clamp(0.0, 100.0);
        let subscription_title = config
            .get("subscription_tier")
            .or_else(|| config.get("subscriptionTier"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let next_reset_at = config
            .pointer("/currentPeriod/end")
            .or_else(|| config.get("billingPeriodEnd"))
            .and_then(Value::as_str)
            .and_then(parse_timestamp);
        Ok(GrokBalanceResponse {
            id,
            subscription_title,
            current_usage: usage_percent,
            usage_limit: 100.0,
            remaining: 100.0 - usage_percent,
            usage_percentage: usage_percent,
            next_reset_at,
            raw: upstream,
        })
    }

    async fn catalog(&self, id: u64, refresh: bool) -> anyhow::Result<GrokCatalogResponse> {
        let (catalog, from_cache) = self.provider.get_model_catalog_for(id, refresh).await?;
        Ok(GrokCatalogResponse {
            credential_id: id,
            source: if from_cache { "cache" } else { "upstream" }.to_string(),
            default_model: GrokDefaultModel {
                model_id: self.token_manager.config().grok_default_model.clone(),
            },
            models: catalog
                .models
                .into_iter()
                .map(|model| {
                    let default_reasoning_effort = model
                        .default_effort()
                        .map(ReasoningEffort::as_str)
                        .map(ToOwned::to_owned);
                    GrokCatalogModel {
                        model_id: model.model_id,
                        model_name: model.model_name,
                        description: model.description,
                        token_limits: GrokTokenLimits {
                            max_input_tokens: model.context_window,
                            max_output_tokens: model.max_completion_tokens,
                        },
                        api_backend: model.api_backend.as_str().to_string(),
                        supported_in_api: model.supported_in_api,
                        supports_reasoning_effort: model.supports_reasoning_effort,
                        default_reasoning_effort,
                        reasoning_efforts: model
                            .reasoning_efforts
                            .into_iter()
                            .map(|option| GrokReasoningEffortOptionResponse {
                                id: option.id,
                                value: option.value.as_str().to_string(),
                                label: option.label,
                                description: option.description,
                                default: option.default,
                            })
                            .collect(),
                    }
                })
                .collect(),
        })
    }

    fn export_catalog(&self) -> anyhow::Result<()> {
        let path = std::path::Path::new("docs/grok_model_catalog.json");
        let catalog = self
            .provider
            .model_catalog()
            .map(|catalog| (*catalog).clone())
            .unwrap_or_else(GrokModelCatalog::bootstrap);
        let body = json!({
            "defaultModel": self.token_manager.config().grok_default_model,
            "source": "merged-per-credential-catalog",
            "models": catalog.models,
        });
        std::fs::write(path, serde_json::to_string_pretty(&body)?)?;
        Ok(())
    }
}

/// 创建镜像的 Grok Admin API 路由。
pub fn create_admin_router(state: GrokAdminState) -> Router {
    Router::new()
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/name", post(set_credential_name))
        .route("/credentials/{id}/pools", post(set_credential_pools))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/verify", post(verify_credential))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/catalog", get(get_credential_catalog))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route("/catalog/export", post(export_model_catalog))
        .route("/api-keys", get(get_all_api_keys).post(add_api_key))
        .route("/api-keys/{id}", put(update_api_key).delete(delete_api_key))
        .route("/api_keys", get(get_all_api_keys).post(add_api_key))
        .route("/api_keys/{id}", put(update_api_key).delete(delete_api_key))
        .route("/pools", get(get_all_pools))
        .route("/oauth/start", post(start_oauth))
        .route("/oauth/status/{state}", get(get_oauth_status))
        .route("/oauth/cancel/{state}", post(cancel_oauth))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}

async fn admin_auth_middleware(
    State(state): State<GrokAdminState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match auth::extract_api_key(&request) {
        Some(key) if auth::constant_time_eq(&key, &state.admin_api_key) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(AdminErrorResponse::authentication_error()),
        )
            .into_response(),
    }
}

async fn get_all_credentials(State(state): State<GrokAdminState>) -> impl IntoResponse {
    Json(state.service.credentials_status())
}

async fn add_credential(
    State(state): State<GrokAdminState>,
    Json(request): Json<AddGrokCredentialRequest>,
) -> Response {
    match state.service.add_credential(request).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn delete_credential(State(state): State<GrokAdminState>, Path(id): Path<u64>) -> Response {
    match state.service.token_manager.delete_credential(id) {
        Ok(()) => Json(SuccessResponse::new(format!("Grok 凭据 #{id} 已删除"))).into_response(),
        Err(error) => admin_error(StatusCode::NOT_FOUND, error.to_string()),
    }
}

async fn set_credential_disabled(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Json(request): Json<SetDisabledRequest>,
) -> Response {
    match state
        .service
        .token_manager
        .set_disabled(id, request.disabled)
    {
        Ok(()) => Json(SuccessResponse::new(format!(
            "Grok 凭据 #{id} 已{}",
            if request.disabled { "禁用" } else { "启用" }
        )))
        .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn set_credential_name(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Json(request): Json<SetNameRequest>,
) -> Response {
    match state.service.token_manager.set_name(id, request.name) {
        Ok(()) => Json(SuccessResponse::new(format!("Grok 凭据 #{id} 名称已更新"))).into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn set_credential_pools(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Json(request): Json<SetPoolsRequest>,
) -> Response {
    match state.service.token_manager.set_pools(id, request.pools) {
        Ok(()) => Json(SuccessResponse::new(format!(
            "Grok 凭据 #{id} 资源池已更新"
        )))
        .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn set_credential_priority(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Json(request): Json<SetPriorityRequest>,
) -> Response {
    match state
        .service
        .token_manager
        .set_priority(id, request.priority)
    {
        Ok(()) => Json(SuccessResponse::new(format!(
            "Grok 凭据 #{id} 优先级已更新"
        )))
        .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn reset_failure_count(State(state): State<GrokAdminState>, Path(id): Path<u64>) -> Response {
    match state.service.token_manager.reset_and_enable(id) {
        Ok(()) => Json(SuccessResponse::new(format!(
            "Grok 凭据 #{id} 已重置并启用"
        )))
        .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn force_refresh_token(State(state): State<GrokAdminState>, Path(id): Path<u64>) -> Response {
    match state
        .service
        .token_manager
        .force_refresh_token_for(id)
        .await
    {
        Ok(_) => Json(SuccessResponse::new(format!(
            "Grok 凭据 #{id} Token 已刷新"
        )))
        .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn verify_credential(State(state): State<GrokAdminState>, Path(id): Path<u64>) -> Response {
    match state.service.provider.verify_credential(id).await {
        Ok(upstream) => Json(json!({ "success": true, "credentialId": id, "upstream": upstream }))
            .into_response(),
        Err(error) => admin_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn get_credential_balance(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
) -> Response {
    match state.service.balance(id).await {
        Ok(balance) => Json(balance).into_response(),
        Err(error) => admin_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

#[derive(Deserialize)]
struct CatalogQuery {
    #[serde(default)]
    refresh: bool,
}

async fn get_credential_catalog(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Query(query): Query<CatalogQuery>,
) -> Response {
    if state.service.token_manager.credential(id).is_none() {
        return admin_error(StatusCode::NOT_FOUND, format!("Grok 凭据 #{} 不存在", id));
    }
    match state.service.catalog(id, query.refresh).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => admin_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn get_load_balancing_mode(State(state): State<GrokAdminState>) -> impl IntoResponse {
    Json(json!({ "mode": state.service.token_manager.get_load_balancing_mode() }))
}

async fn set_load_balancing_mode(
    State(state): State<GrokAdminState>,
    Json(request): Json<SetLoadBalancingModeRequest>,
) -> Response {
    match state
        .service
        .token_manager
        .set_load_balancing_mode(request.mode)
    {
        Ok(()) => Json(json!({ "mode": state.service.token_manager.get_load_balancing_mode() }))
            .into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn export_model_catalog(State(state): State<GrokAdminState>) -> Response {
    match state.service.export_catalog() {
        Ok(()) => Json(SuccessResponse::new(
            "Grok 模型目录已导出到 docs/grok_model_catalog.json",
        ))
        .into_response(),
        Err(error) => admin_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn get_all_api_keys(State(state): State<GrokAdminState>) -> impl IntoResponse {
    Json(ApiKeyListResponse {
        keys: state.service.api_key_manager().list(),
    })
}

async fn add_api_key(
    State(state): State<GrokAdminState>,
    Json(request): Json<AddApiKeyRequest>,
) -> Response {
    match state
        .service
        .api_key_manager()
        .add(request.name, request.key, Some(request.pools), false)
    {
        Ok(entry) => Json(entry).into_response(),
        Err(error) => admin_error(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

async fn update_api_key(
    State(state): State<GrokAdminState>,
    Path(id): Path<u64>,
    Json(request): Json<UpdateApiKeyRequest>,
) -> Response {
    match state
        .service
        .api_key_manager()
        .update(id, request.name, request.pools, request.disabled)
    {
        Ok(entry) => Json(entry).into_response(),
        Err(error) => admin_error(StatusCode::NOT_FOUND, error.to_string()),
    }
}

async fn delete_api_key(State(state): State<GrokAdminState>, Path(id): Path<u64>) -> Response {
    match state.service.api_key_manager().delete(id) {
        Ok(()) => Json(SuccessResponse::new(format!("API Key #{id} 已删除"))).into_response(),
        Err(error) => admin_error(StatusCode::NOT_FOUND, error.to_string()),
    }
}

async fn get_all_pools(State(state): State<GrokAdminState>) -> impl IntoResponse {
    let credential_pools = state
        .service
        .credentials()
        .entries
        .into_iter()
        .flat_map(|credential| credential.pools)
        .collect::<Vec<_>>();
    Json(
        state
            .service
            .api_key_manager()
            .all_pool_names(&credential_pools),
    )
}

async fn start_oauth(State(state): State<GrokAdminState>) -> Response {
    match state.service.oauth.start().await {
        Ok(response) => Json(response).into_response(),
        Err(error) => admin_error(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn get_oauth_status(
    State(state): State<GrokAdminState>,
    Path(oauth_state): Path<String>,
) -> Response {
    match state.service.oauth.status(&oauth_state) {
        Ok(response) => Json(response).into_response(),
        Err(error) => admin_error(StatusCode::NOT_FOUND, error.to_string()),
    }
}

async fn cancel_oauth(
    State(state): State<GrokAdminState>,
    Path(oauth_state): Path<String>,
) -> Response {
    match state.service.oauth.cancel(&oauth_state) {
        Ok(()) => Json(SuccessResponse::new("Grok OAuth 授权已取消")).into_response(),
        Err(error) => admin_error(StatusCode::NOT_FOUND, error.to_string()),
    }
}

fn admin_error(status: StatusCode, message: String) -> Response {
    (status, Json(AdminErrorResponse::new("api_error", message))).into_response()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn number_value(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.get("val").and_then(Value::as_f64))
}

fn parse_timestamp(value: &str) -> Option<f64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp() as f64)
}

/// 手动导入 token 或 AIClient-2-API 保存的 OAuth 凭据字段。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddGrokCredentialRequest {
    name: Option<String>,
    #[serde(
        alias = "token",
        alias = "apiKey",
        alias = "kiroApiKey",
        alias = "access_token"
    )]
    access_token: Option<String>,
    #[serde(alias = "refresh_token")]
    refresh_token: Option<String>,
    #[serde(alias = "id_token")]
    id_token: Option<String>,
    #[serde(alias = "token_type")]
    token_type: Option<String>,
    #[serde(alias = "expires_at", alias = "expired")]
    expires_at: Option<String>,
    auth_method: Option<String>,
    email: Option<String>,
    #[serde(alias = "sub")]
    subject: Option<String>,
    #[serde(alias = "user_id")]
    user_id: Option<String>,
    #[serde(alias = "team_id")]
    team_id: Option<String>,
    #[serde(alias = "base_url")]
    base_url: Option<String>,
    #[serde(alias = "token_endpoint")]
    token_endpoint: Option<String>,
    #[serde(default)]
    priority: u32,
    proxy_url: Option<String>,
    proxy_username: Option<String>,
    proxy_password: Option<String>,
    #[serde(default)]
    pools: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddGrokCredentialResponse {
    success: bool,
    message: String,
    credential_id: u64,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported_at: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokCredentialsStatusResponse {
    total: usize,
    available: usize,
    current_id: u64,
    credentials: Vec<super::token_manager::GrokCredentialSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokBalanceResponse {
    id: u64,
    subscription_title: Option<String>,
    current_usage: f64,
    usage_limit: f64,
    remaining: f64,
    usage_percentage: f64,
    next_reset_at: Option<f64>,
    raw: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokCatalogResponse {
    credential_id: u64,
    source: String,
    default_model: GrokDefaultModel,
    models: Vec<GrokCatalogModel>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokDefaultModel {
    model_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokCatalogModel {
    model_id: String,
    model_name: String,
    description: Option<String>,
    token_limits: GrokTokenLimits,
    api_backend: String,
    supported_in_api: bool,
    supports_reasoning_effort: bool,
    default_reasoning_effort: Option<String>,
    reasoning_efforts: Vec<GrokReasoningEffortOptionResponse>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokTokenLimits {
    max_input_tokens: Option<i32>,
    max_output_tokens: Option<i32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrokReasoningEffortOptionResponse {
    id: String,
    value: String,
    label: String,
    description: Option<String>,
    default: bool,
}

// Keep these imports visible in generated rustdoc / API signatures.
#[allow(dead_code)]
fn _oauth_types(_: OAuthStartResponse, _: OAuthStatusResponse) {}
