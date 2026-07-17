//! xAI Grok Build OAuth（Authorization Code + PKCE）支持。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};

use super::credentials::{
    GrokCredentials, XAI_GROK_CLI_CLIENT_ID, XAI_GROK_CLI_REDIRECT_URI,
    default_oauth_base_url, jwt_identity,
};
use super::token_manager::SharedGrokTokenManager;

const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const OAUTH_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const CALLBACK_PORT: u16 = 56_121;
const FLOW_TTL_MINUTES: i64 = 10;

#[derive(Clone)]
pub struct GrokOAuthService {
    inner: Arc<OAuthInner>,
}

struct OAuthInner {
    token_manager: SharedGrokTokenManager,
    http_client: reqwest::Client,
    flows: Mutex<HashMap<String, OAuthFlow>>,
    callback_started: AtomicBool,
    callback_start_lock: AsyncMutex<()>,
}

#[derive(Debug, Clone)]
struct OAuthFlow {
    state: String,
    code_verifier: String,
    token_endpoint: String,
    authorization_url: String,
    status: OAuthFlowStatus,
    created_at: DateTime<Utc>,
    credential_id: Option<u64>,
    email: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlowStatus {
    Pending,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStartResponse {
    pub state: String,
    pub authorization_url: String,
    pub callback_url: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStatusResponse {
    pub state: String,
    pub status: OAuthFlowStatus,
    pub authorization_url: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    #[serde(alias = "userId")]
    user_id: Option<String>,
    #[serde(alias = "teamId")]
    team_id: Option<String>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl GrokOAuthService {
    pub fn new(
        token_manager: SharedGrokTokenManager,
        global_proxy: Option<ProxyConfig>,
    ) -> anyhow::Result<Self> {
        let client = build_client(
            global_proxy.as_ref(),
            60,
            token_manager.config().tls_backend,
        )?;
        Ok(Self {
            inner: Arc::new(OAuthInner {
                token_manager,
                http_client: client,
                flows: Mutex::new(HashMap::new()),
                callback_started: AtomicBool::new(false),
                callback_start_lock: AsyncMutex::new(()),
            }),
        })
    }

    pub async fn start(&self) -> anyhow::Result<OAuthStartResponse> {
        let discovery = self.discover().await?;
        self.ensure_callback_server().await?;
        self.inner
            .flows
            .lock()
            .retain(|_, flow| !flow_expired(flow));
        let state = Uuid::new_v4().simple().to_string();
        let nonce = Uuid::new_v4().simple().to_string();
        let code_verifier = format!(
            "{}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        let mut authorization_url = Url::parse(&discovery.authorization_endpoint)
            .context("解析 xAI authorization endpoint 失败")?;
        {
            let mut query = authorization_url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", XAI_GROK_CLI_CLIENT_ID);
            query.append_pair("redirect_uri", XAI_GROK_CLI_REDIRECT_URI);
            query.append_pair("scope", OAUTH_SCOPE);
            query.append_pair("code_challenge", &code_challenge);
            query.append_pair("code_challenge_method", "S256");
            query.append_pair("state", &state);
            query.append_pair("nonce", &nonce);
            query.append_pair("plan", "generic");
            query.append_pair("referrer", "grok-cli");
        }
        let authorization_url = authorization_url.to_string();
        let flow = OAuthFlow {
            state: state.clone(),
            code_verifier,
            token_endpoint: discovery.token_endpoint,
            authorization_url: authorization_url.clone(),
            status: OAuthFlowStatus::Pending,
            created_at: Utc::now(),
            credential_id: None,
            email: None,
            error: None,
        };
        self.inner.flows.lock().insert(state.clone(), flow);
        Ok(OAuthStartResponse {
            state,
            authorization_url,
            callback_url: XAI_GROK_CLI_REDIRECT_URI.to_string(),
            expires_in_seconds: 600,
        })
    }

    pub fn status(&self, state: &str) -> anyhow::Result<OAuthStatusResponse> {
        let mut flows = self.inner.flows.lock();
        let flow = flows
            .get_mut(state)
            .ok_or_else(|| anyhow::anyhow!("OAuth state 不存在或已过期"))?;
        if matches!(flow.status, OAuthFlowStatus::Pending) && flow_expired(flow) {
            flow.status = OAuthFlowStatus::Failed;
            flow.error = Some("OAuth state 已过期，请重新发起授权".to_string());
        }
        let flow = flow.clone();
        Ok(OAuthStatusResponse {
            state: flow.state,
            status: flow.status,
            authorization_url: flow.authorization_url,
            created_at: flow.created_at.to_rfc3339(),
            credential_id: flow.credential_id,
            email: flow.email,
            error: flow.error,
        })
    }

    pub fn cancel(&self, state: &str) -> anyhow::Result<()> {
        let mut flows = self.inner.flows.lock();
        let flow = flows
            .get_mut(state)
            .ok_or_else(|| anyhow::anyhow!("OAuth state 不存在或已过期"))?;
        flow.status = OAuthFlowStatus::Cancelled;
        Ok(())
    }

    async fn ensure_callback_server(&self) -> anyhow::Result<()> {
        if self.inner.callback_started.load(Ordering::Acquire) {
            return Ok(());
        }
        let _guard = self.inner.callback_start_lock.lock().await;
        if self.inner.callback_started.load(Ordering::Acquire) {
            return Ok(());
        }
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
            .await
            .with_context(|| {
                format!(
                    "无法监听 Grok OAuth 回调端口 {}；请释放该端口后重试",
                    CALLBACK_PORT
                )
            })?;
        let app = Router::new()
            .route("/callback", get(oauth_callback))
            .with_state(self.clone());
        self.inner.callback_started.store(true, Ordering::Release);
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                tracing::error!(%error, "Grok OAuth callback server 已停止");
            }
        });
        tracing::info!(
            callback = XAI_GROK_CLI_REDIRECT_URI,
            "Grok OAuth callback server 已启动"
        );
        Ok(())
    }

    async fn discover(&self) -> anyhow::Result<DiscoveryResponse> {
        let response = self
            .inner
            .http_client
            .get(XAI_DISCOVERY_URL)
            .header("accept", "application/json")
            .send()
            .await
            .context("请求 xAI OAuth discovery 失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("xAI OAuth discovery 失败: {} {}", status, body);
        }
        let discovery: DiscoveryResponse = response
            .json()
            .await
            .context("解析 xAI OAuth discovery 失败")?;
        validate_xai_url(&discovery.authorization_endpoint, "authorization_endpoint")?;
        validate_xai_url(&discovery.token_endpoint, "token_endpoint")?;
        Ok(discovery)
    }

    async fn complete(&self, state: &str, code: &str) -> anyhow::Result<(u64, Option<String>)> {
        let flow = {
            let flows = self.inner.flows.lock();
            let flow = flows
                .get(state)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("OAuth state 不存在或已过期"))?;
            if !matches!(flow.status, OAuthFlowStatus::Pending) {
                bail!("OAuth state 已完成、取消或失败");
            }
            if flow_expired(&flow) {
                bail!("OAuth state 已过期，请重新发起授权");
            }
            flow
        };
        let response = self
            .inner
            .http_client
            .post(&flow.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", XAI_GROK_CLI_REDIRECT_URI),
                ("client_id", XAI_GROK_CLI_CLIENT_ID),
                ("code_verifier", flow.code_verifier.as_str()),
            ])
            .send()
            .await
            .context("发送 xAI OAuth code exchange 请求失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("xAI OAuth code exchange 失败: {} {}", status, body);
        }
        let token: TokenResponse = response
            .json()
            .await
            .context("解析 xAI OAuth token 响应失败")?;
        let access_token = token
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("xAI OAuth token 响应缺少 access_token"))?;
        let identity_token = token.id_token.as_deref().unwrap_or(&access_token);
        let (email, subject) = jwt_identity(identity_token);
        let credential = GrokCredentials {
            access_token: Some(access_token),
            refresh_token: token.refresh_token,
            id_token: token.id_token,
            token_type: token.token_type.or_else(|| Some("Bearer".to_string())),
            expires_at: Some(
                (Utc::now() + Duration::seconds(token.expires_in.unwrap_or(3600).max(0)))
                    .to_rfc3339(),
            ),
            auth_method: Some("oauth".to_string()),
            email: email.clone(),
            subject,
            user_id: token.user_id,
            team_id: token.team_id,
            base_url: Some(default_oauth_base_url(self.inner.token_manager.config())),
            token_endpoint: Some(flow.token_endpoint),
            last_refresh: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };
        let credential_id = self.inner.token_manager.add_credential(credential)?;
        let mut flows = self.inner.flows.lock();
        if let Some(flow) = flows.get_mut(state) {
            flow.status = OAuthFlowStatus::Completed;
            flow.credential_id = Some(credential_id);
            flow.email = email.clone();
            flow.error = None;
        }
        Ok((credential_id, email))
    }

    fn mark_failed(&self, state: Option<&str>, error: impl Into<String>) {
        let Some(state) = state else {
            return;
        };
        if let Some(flow) = self.inner.flows.lock().get_mut(state) {
            flow.status = OAuthFlowStatus::Failed;
            flow.error = Some(error.into());
        }
    }
}

async fn oauth_callback(
    State(service): State<GrokOAuthService>,
    Query(query): Query<CallbackQuery>,
) -> Html<String> {
    if let Some(error) = query.error {
        let message = query.error_description.unwrap_or(error);
        service.mark_failed(query.state.as_deref(), message.clone());
        return Html(callback_page(false, &format!("授权被 xAI 拒绝：{message}")));
    }
    let (Some(code), Some(state)) = (query.code, query.state) else {
        return Html(callback_page(false, "回调缺少 code 或 state。"));
    };
    match service.complete(&state, &code).await {
        Ok((id, email)) => Html(callback_page(
            true,
            &format!(
                "Grok Build 授权成功，凭据 #{} 已保存{}。",
                id,
                email
                    .as_deref()
                    .map(|email| format!("（{}）", email))
                    .unwrap_or_default()
            ),
        )),
        Err(error) => {
            service.mark_failed(Some(&state), error.to_string());
            Html(callback_page(false, &format!("授权兑换失败：{error}")))
        }
    }
}

fn validate_xai_url(value: &str, field: &str) -> anyhow::Result<()> {
    let url = Url::parse(value).with_context(|| format!("xAI {field} 不是有效 URL"))?;
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("xAI {field} 缺少 host"))?;
    if url.scheme() != "https" || !(host == "x.ai" || host.ends_with(".x.ai")) {
        bail!("xAI {field} 不在 x.ai 域名下: {value}");
    }
    Ok(())
}

fn flow_expired(flow: &OAuthFlow) -> bool {
    flow.created_at + Duration::minutes(FLOW_TTL_MINUTES) < Utc::now()
}

fn callback_page(success: bool, message: &str) -> String {
    let title = if success {
        "Grok Build 授权成功"
    } else {
        "Grok Build 授权失败"
    };
    let color = if success { "#16803c" } else { "#b42318" };
    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>{title}</title><body style=\"font-family:system-ui;max-width:42rem;margin:5rem auto;line-height:1.6\"><h1 style=\"color:{color}\">{title}</h1><p>{}</p><p>可关闭此窗口并回到管理页面。</p></body></html>",
        html_escape(message)
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_non_xai_discovery_urls() {
        assert!(validate_xai_url("https://auth.x.ai/oauth/authorize", "test").is_ok());
        assert!(validate_xai_url("https://evil.example/token", "test").is_err());
        assert!(validate_xai_url("http://auth.x.ai/token", "test").is_err());
    }
}
