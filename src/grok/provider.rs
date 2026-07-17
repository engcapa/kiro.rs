//! xAI Responses API Provider。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use parking_lot::Mutex;
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use tokio::time::sleep;
use url::Url;
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

use super::credentials::GrokCredentials;
use super::token_manager::SharedGrokTokenManager;

const MAX_RETRIES_PER_CREDENTIAL: usize = 2;
const MAX_TOTAL_RETRIES: usize = 9;
const GROK_CLI_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// 与参考的 grok-build 版本保持一致。xAI 的代理会用该字段做版本识别，
/// 公共 `api.x.ai` 端点会忽略它。
const GROK_BUILD_CLIENT_VERSION: &str = "0.2.101";
/// `grok-build` 的 sampler 默认使用 `grok-shell` 作为 client identifier。
/// CLI chat proxy 会根据这一族标识做版本/产品路由。
const GROK_BUILD_CLIENT_IDENTIFIER: &str = "grok-shell";

pub struct GrokUpstreamResponse {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

/// 与 xAI 的 `/responses` 交互并负责凭据故障转移。
pub struct GrokProvider {
    token_manager: SharedGrokTokenManager,
    global_proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
}

impl GrokProvider {
    pub fn new(
        token_manager: SharedGrokTokenManager,
        global_proxy: Option<ProxyConfig>,
    ) -> anyhow::Result<Self> {
        let tls_backend = token_manager.config().tls_backend;
        let initial_client = build_client(global_proxy.as_ref(), 300, tls_backend)?;
        let mut client_cache = HashMap::new();
        client_cache.insert(global_proxy.clone(), initial_client);
        Ok(Self {
            token_manager,
            global_proxy,
            tls_backend,
            client_cache: Mutex::new(client_cache),
        })
    }

    pub fn token_manager(&self) -> &SharedGrokTokenManager {
        &self.token_manager
    }

    fn client_for(&self, credentials: &GrokCredentials) -> anyhow::Result<Client> {
        let effective_proxy = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective_proxy) {
            return Ok(client.clone());
        }
        let client = build_client(effective_proxy.as_ref(), 300, self.tls_backend)?;
        cache.insert(effective_proxy, client.clone());
        Ok(client)
    }

    /// 注入 Grok Build 参考客户端使用的认证和标识头。
    ///
    /// API token 与 OAuth access token 都是 Bearer token；后者还需要
    /// `x-xai-token-auth: xai-grok-cli` 来标识为 Grok CLI OAuth 会话。
    fn authenticated_request(
        request: RequestBuilder,
        credentials: &GrokCredentials,
        token: &str,
        cli_chat_proxy: bool,
    ) -> RequestBuilder {
        let mut request = request
            .header(
                "authorization",
                format!("{} {}", credentials.effective_token_type(), token),
            )
            .header("x-grok-client-version", GROK_BUILD_CLIENT_VERSION)
            .header("x-grok-client-identifier", GROK_BUILD_CLIENT_IDENTIFIER)
            .header(
                "user-agent",
                format!("grok-shell/{GROK_BUILD_CLIENT_VERSION}"),
            );
        if credentials.is_oauth() {
            request = request.header("x-xai-token-auth", "xai-grok-cli");
            if let Some(user_id) = credentials
                .user_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                request = request.header("x-userid", user_id);
            }
            if let Some(email) = credentials
                .email
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                request = request.header("x-email", email);
            }
            // Grok Build adds these URL-derived headers for the OAuth-only
            // CLI chat proxy. Public api.x.ai API-token requests do not need
            // them and must retain their normal xAI API semantics.
            if cli_chat_proxy {
                request = request
                    .header("x-authenticateresponse", "authenticate-response")
                    .header("x-grok-client-mode", "headless");
            }
        }
        request
    }

    /// 发送 xAI Responses API 请求。Responses API 的非流式请求在 Grok CLI
    /// 流程中同样以 SSE 返回，因此调用方总是接收原始 response stream。
    pub async fn call_responses(
        &self,
        body: &Value,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<GrokUpstreamResponse> {
        let total = self.token_manager.total_count();
        let max_retries = (total * MAX_RETRIES_PER_CREDENTIAL).clamp(1, MAX_TOTAL_RETRIES);
        let mut last_error = None;
        let mut forced_refresh = HashSet::new();

        for attempt in 0..max_retries {
            let context = match self.token_manager.acquire_context(allowed_pools).await {
                Ok(context) => context,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            };
            let url = format!(
                "{}/responses",
                context
                    .credentials
                    .effective_base_url(self.token_manager.config())
                    .trim_end_matches('/')
            );
            let cli_chat_proxy = is_cli_chat_proxy_url(&url);
            let session_id = body
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let request_id = Uuid::new_v4().to_string();
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut request = self
                .client_for(&context.credentials)?
                .post(&url)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("connection", "keep-alive")
                .header("x-grok-req-id", request_id)
                .header("x-grok-model-override", model)
                .header("x-grok-agent-id", GROK_BUILD_CLIENT_IDENTIFIER)
                .json(body);
            if let Some(session_id) = session_id {
                request = request
                    .header("x-grok-conv-id", session_id)
                    .header("x-grok-session-id", session_id);
            }
            let request = Self::authenticated_request(
                request,
                &context.credentials,
                &context.token,
                cli_chat_proxy,
            );

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error.into());
                    if attempt + 1 < max_retries {
                        sleep(retry_delay(attempt)).await;
                    }
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                self.token_manager.report_success(context.id);
                return Ok(GrokUpstreamResponse {
                    response,
                    credential_id: context.id,
                });
            }

            let response_body = response.text().await.unwrap_or_default();
            if is_quota_exhausted(status.as_u16(), &response_body) {
                let available = self.token_manager.report_quota_exhausted(context.id);
                last_error = Some(anyhow::anyhow!(
                    "xAI Responses API 配额已用尽: {} {}",
                    status,
                    response_body
                ));
                if !available {
                    break;
                }
                continue;
            }

            if matches!(status.as_u16(), 401 | 403)
                && context.credentials.is_oauth()
                && forced_refresh.insert(context.id)
            {
                tracing::info!(
                    credential_id = context.id,
                    "xAI 返回认证错误，尝试强制刷新 OAuth token"
                );
                if self
                    .token_manager
                    .force_refresh_token_for(context.id)
                    .await
                    .is_ok()
                {
                    continue;
                }
            }

            if matches!(status.as_u16(), 401 | 403 | 408 | 429) || status.is_server_error() {
                let available = self.token_manager.report_failure(context.id);
                last_error = Some(anyhow::anyhow!(
                    "xAI Responses API 请求失败: {} {}",
                    status,
                    response_body
                ));
                if !available {
                    break;
                }
                if attempt + 1 < max_retries {
                    sleep(retry_delay(attempt)).await;
                }
                continue;
            }

            return Err(anyhow::anyhow!(
                "xAI Responses API 请求失败: {} {}",
                status,
                response_body
            ));
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("xAI Responses API 请求失败")))
    }

    /// 使用对应凭据调用 xAI models endpoint，用于管理接口的 token 校验。
    pub async fn verify_credential(&self, id: u64) -> anyhow::Result<Value> {
        let context = self.token_manager.acquire_context_for(id).await?;
        let url = format!(
            "{}/models",
            context
                .credentials
                .effective_base_url(self.token_manager.config())
                .trim_end_matches('/')
        );
        let cli_chat_proxy = is_cli_chat_proxy_url(&url);
        let request = self
            .client_for(&context.credentials)?
            .get(url)
            .header("accept", "application/json");
        let response = Self::authenticated_request(
            request,
            &context.credentials,
            &context.token,
            cli_chat_proxy,
        )
        .send()
        .await
        .context("发送 xAI token 校验请求失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            self.token_manager.report_failure(context.id);
            anyhow::bail!("xAI token 校验失败: {} {}", status, body);
        }
        self.token_manager.report_success(context.id);
        Ok(response
            .json()
            .await
            .context("解析 xAI token 校验响应失败")?)
    }

    /// 查询 Grok CLI billing 数据。该接口不是 xAI 公共 API 的稳定契约，
    /// 因此保留原始 JSON 供管理端展示，并由调用方做兼容性提取。
    pub async fn billing_for(&self, id: u64) -> anyhow::Result<Value> {
        let context = self.token_manager.acquire_context_for(id).await?;
        if !context.credentials.is_oauth() {
            anyhow::bail!("Grok CLI billing 仅支持 OAuth session 凭据");
        }
        let request = self
            .client_for(&context.credentials)?
            .get(GROK_CLI_BILLING_URL)
            .header("accept", "application/json")
            .header("x-xai-token-auth", "xai-grok-cli")
            .header("x-grok-cli-version", GROK_BUILD_CLIENT_VERSION);
        let response = Self::authenticated_request(
            request,
            &context.credentials,
            &context.token,
            true,
        )
        .send()
        .await
        .context("发送 xAI billing 请求失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            self.token_manager.report_failure(context.id);
            anyhow::bail!("xAI billing 请求失败: {} {}", status, body);
        }
        self.token_manager.report_success(context.id);
        Ok(response.json().await.context("解析 xAI billing 响应失败")?)
    }
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis((500_u64.saturating_mul(1_u64 << attempt.min(4))).min(8_000))
}

fn is_quota_exhausted(status: u16, body: &str) -> bool {
    status == 402
        || body.contains("insufficient_quota")
        || body.contains("quota_exceeded")
        || body.contains("credit balance")
}

fn is_cli_chat_proxy_url(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "cli-chat-proxy.grok.com")
}

pub type SharedGrokProvider = Arc<GrokProvider>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_quota_errors() {
        assert!(is_quota_exhausted(402, "anything"));
        assert!(is_quota_exhausted(429, r#"{"code":"insufficient_quota"}"#));
        assert!(!is_quota_exhausted(400, "bad request"));
    }

    #[test]
    fn recognizes_the_grok_build_cli_proxy() {
        assert!(is_cli_chat_proxy_url(
            "https://cli-chat-proxy.grok.com/v1/responses"
        ));
        assert!(!is_cli_chat_proxy_url("https://api.x.ai/v1/responses"));
    }
}
