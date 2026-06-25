//! Admin API HTTP 处理器

use axum::{
    Json,
    extract::{Path, State},
    response::IntoResponse,
};

use super::{
    middleware::AdminState,
    types::{
        AddApiKeyRequest, AddCredentialRequest, ApiKeyListResponse, SetDisabledRequest,
        SetLoadBalancingModeRequest, SetNameRequest, SetPoolsRequest, SetPriorityRequest,
        SuccessResponse, UpdateApiKeyRequest,
    },
};

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials().await;
    Json(response)
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/name
/// 设置凭据名称
pub async fn set_credential_name(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetNameRequest>,
) -> impl IntoResponse {
    match state.service.set_name(id, payload.name) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 名称已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/pools
/// 设置凭据所属的池列表
pub async fn set_credential_pools(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPoolsRequest>,
) -> impl IntoResponse {
    match state.service.set_pools(id, payload.pools) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 权限池已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/catalog/export
/// 导出模型目录到 docs 目录
pub async fn export_model_catalog(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.export_model_catalog_to_docs().await {
        Ok(_) => Json(SuccessResponse::new("模型目录已导出到 docs/ 目录".to_string())).into_response(),
        Err(e) => {
            tracing::error!("导出模型目录失败: {}", e);
            Json(SuccessResponse::new(format!("导出失败: {}", e))).into_response()
        }
    }
}

/// GET /api/admin/api-keys
pub async fn get_all_api_keys(State(state): State<AdminState>) -> impl IntoResponse {
    let keys = state.service.api_key_manager().list();
    Json(ApiKeyListResponse { keys })
}

/// POST /api/admin/api-keys
pub async fn add_api_key(
    State(state): State<AdminState>,
    Json(payload): Json<AddApiKeyRequest>,
) -> impl IntoResponse {
    match state
        .service
        .api_key_manager()
        .add(payload.name, payload.key, Some(payload.pools), false)
    {
        Ok(entry) => Json(entry).into_response(),
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(SuccessResponse::new(e.to_string())),
        )
            .into_response(),
    }
}

/// PUT /api/admin/api-keys/:id
pub async fn update_api_key(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<UpdateApiKeyRequest>,
) -> impl IntoResponse {
    match state
        .service
        .api_key_manager()
        .update(id, payload.name, payload.pools, payload.disabled)
    {
        Ok(entry) => Json(entry).into_response(),
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(SuccessResponse::new(e.to_string())),
        )
            .into_response(),
    }
}

/// DELETE /api/admin/api-keys/:id
pub async fn delete_api_key(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.api_key_manager().delete(id) {
        Ok(_) => Json(SuccessResponse::new(format!("API Key #{} 已删除", id))).into_response(),
        Err(e) => (
            axum::http::StatusCode::NOT_FOUND,
            Json(SuccessResponse::new(e.to_string())),
        )
            .into_response(),
    }
}

/// GET /api/admin/pools
pub async fn get_all_pools(State(state): State<AdminState>) -> impl IntoResponse {
    let credential_pools: Vec<String> = state
        .service
        .get_all_credentials()
        .await
        .credentials
        .into_iter()
        .flat_map(|c| c.pools)
        .collect();
    let pools = state
        .service
        .api_key_manager()
        .all_pool_names(&credential_pools);
    Json(pools)
}
