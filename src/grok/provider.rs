//! xAI Responses API Provider。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::future::join_all;
use parking_lot::Mutex;
use reqwest::{Client, Method, RequestBuilder};
use serde_json::Value;
use tokio::time::sleep;
use url::Url;
use uuid::Uuid;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

use super::credentials::{GrokCredentials, XAI_DEFAULT_BASE_URL};
use super::model_catalog::{GrokApiBackend, GrokModelCatalog, ReasoningEffort};
use super::token_manager::{GrokCallContext, SharedGrokTokenManager};

const MAX_RETRIES_PER_CREDENTIAL: usize = 2;
const MAX_TOTAL_RETRIES: usize = 9;
const GROK_CLI_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
/// 与参考的 grok-build 版本保持一致。xAI 的代理会用该字段做版本识别，
/// 公共 `api.x.ai` 端点会忽略它。
const GROK_BUILD_CLIENT_VERSION: &str = "0.2.101";
/// `grok-build` 的 sampler 默认使用 `grok-shell` 作为 client identifier。
/// CLI chat proxy 会根据这一族标识做版本/产品路由。
const GROK_BUILD_CLIENT_IDENTIFIER: &str = "grok-shell";
/// Grok Build 会在本地持续轮询直到视频生成完成。HTTP 代理无法把文件写入
/// 调用方的本地会话，因此保存短期 job 绑定以让调用方在同一资源池中轮询。
const VIDEO_JOB_TTL: Duration = Duration::from_secs(60 * 60);

pub struct GrokUpstreamResponse {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

#[derive(Clone)]
struct VideoJob {
    upstream_request_id: String,
    credential_id: u64,
    pools: Vec<String>,
    created_at: Instant,
}

/// 与 xAI 的 `/responses` 交互并负责凭据故障转移。
pub struct GrokProvider {
    token_manager: SharedGrokTokenManager,
    global_proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// `/v1/models` 定时刷新冷却。目录失败不会写入此时间，确保后续周期仍会
    /// 重试，同时不影响运行时凭据状态。
    catalog_refresh_at: Mutex<Option<std::time::Instant>>,
    /// 代理公开的 opaque video request id → 实际 xAI request/credential。
    /// job 只允许同一凭据资源池访问，且会自动过期。
    video_jobs: Mutex<HashMap<String, VideoJob>>,
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
            catalog_refresh_at: Mutex::new(None),
            video_jobs: Mutex::new(HashMap::new()),
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

    /// Grok Build 的 Imagine image/video client 与 sampler 不同：它直接向
    /// `xai_api_base_url` 发送 Bearer，而不会添加 OAuth 专属的 CLI chat
    /// proxy 头。保持这条链路独立，避免把 `x-xai-token-auth` 等 CLI 路由
    /// 标识错误地带到公共媒体 endpoint。
    fn authenticated_media_request(
        request: RequestBuilder,
        credentials: &GrokCredentials,
        token: &str,
    ) -> RequestBuilder {
        request
            .header(
                "authorization",
                format!("{} {}", credentials.effective_token_type(), token),
            )
            // Grok Build 为所有 Imagine 请求设置这一版本标识；它不是
            // OAuth CLI chat-proxy 的专属认证头。
            .header("x-grok-client-version", GROK_BUILD_CLIENT_VERSION)
            .header(
                "user-agent",
                format!("xai-grok-build/{GROK_BUILD_CLIENT_VERSION}"),
            )
    }

    /// 按 Grok Build 模型目录指定的 backend 发送请求。
    ///
    /// 模型 id 和 effort 在进入重试循环前已规范化；每次选到凭据后仍会按该
    /// 凭据自己的 catalog 再过滤，防止并集目录把 Composer/Grok 4.5 误路由
    /// 到没有对应授权的账号。
    pub async fn call_api(
        &self,
        body: &Value,
        backend: GrokApiBackend,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<GrokUpstreamResponse> {
        let total = self.token_manager.total_count();
        let max_retries = (total * MAX_RETRIES_PER_CREDENTIAL).clamp(1, MAX_TOTAL_RETRIES);
        let mut last_error = None;
        let mut forced_refresh = HashSet::new();

        for attempt in 0..max_retries {
            let context = match self
                .token_manager
                .acquire_context(
                    Some(model),
                    reasoning_effort,
                    Some(backend),
                    requires_backend_search,
                    allowed_pools,
                )
                .await
            {
                Ok(context) => context,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            };
            let base_url = self
                .token_manager
                .model_for(context.id, model)
                .and_then(|model| model.base_url)
                .unwrap_or_else(|| {
                    context
                        .credentials
                        .effective_base_url(self.token_manager.config())
                        .to_string()
                });
            let url = endpoint_url(&base_url, backend);
            let cli_chat_proxy = is_cli_chat_proxy_url(&url);
            let session_id = body
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let request_id = Uuid::new_v4().to_string();
            let accepts_sse = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
            let mut request = self
                .client_for(&context.credentials)?
                .post(&url)
                .header("content-type", "application/json")
                .header(
                    "accept",
                    if accepts_sse {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                )
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
                    "xAI {} API 配额已用尽: {} {}",
                    backend.as_str(),
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
                    "xAI {} API 请求失败: {} {}",
                    backend.as_str(),
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
                "xAI {} API 请求失败: {} {}",
                backend.as_str(),
                status,
                response_body
            ));
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("xAI {} API 请求失败", backend.as_str())))
    }

    /// 调用 Grok Build 的公共 Imagine API（`/images/*`、`/videos/*`）。
    ///
    /// Grok Build 对 API token 和 OAuth session 都直连 `xai_api_base_url`，
    /// 而不是 OAuth 推理所用的 CLI chat proxy。因此这里总以全局
    /// `grokBaseUrl` 为准；若它意外指向 CLI proxy，则退回 xAI public
    /// default，避免把 Imagine 请求发往错误的服务。
    pub async fn call_public_api(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<GrokUpstreamResponse> {
        let total = self.token_manager.total_count();
        let max_retries = (total * MAX_RETRIES_PER_CREDENTIAL).clamp(1, MAX_TOTAL_RETRIES);
        let mut last_error = None;
        let mut forced_refresh = HashSet::new();
        let can_retry_transport = public_method_is_idempotent(&method);

        for attempt in 0..max_retries {
            let context = match self
                .token_manager
                .acquire_context(None, None, None, false, allowed_pools)
                .await
            {
                Ok(context) => context,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            };
            let response = match self.public_request(&context, method.clone(), path, body) {
                Ok(request) => match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        last_error = Some(error.into());
                        // 图片/视频创建不是幂等操作；连接断开时服务端可能已
                        // 收到请求。Grok Build 不会盲目重发，避免产生重复收费
                        // 的 Imagine 任务。只有轮询等安全方法可自动重试。
                        if !can_retry_transport {
                            break;
                        }
                        if attempt + 1 < max_retries {
                            sleep(retry_delay(attempt)).await;
                        }
                        continue;
                    }
                },
                Err(error) => return Err(error),
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
                last_error = Some(public_api_error(path, status, &response_body));
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
                    path,
                    "xAI Imagine API 返回认证错误，尝试强制刷新 OAuth token"
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
                last_error = Some(public_api_error(path, status, &response_body));
                if !available {
                    break;
                }
                if !can_retry_transport {
                    break;
                }
                if attempt + 1 < max_retries {
                    sleep(retry_delay(attempt)).await;
                }
                continue;
            }
            return Err(public_api_error(path, status, &response_body));
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("xAI Imagine API 请求失败: {path}")))
    }

    /// 使用已绑定的凭据调用公共 Imagine API。视频生成任务必须固定到创建
    /// 它的 OAuth/API token，不能在轮询时被负载均衡到其他账号。
    pub async fn call_public_api_for_credential(
        &self,
        credential_id: u64,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<GrokUpstreamResponse> {
        let mut last_error = None;
        let mut forced_refresh = false;
        let can_retry_transport = public_method_is_idempotent(&method);
        for attempt in 0..MAX_RETRIES_PER_CREDENTIAL {
            let context = match self.token_manager.acquire_context_for(credential_id).await {
                Ok(context) => context,
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            };
            let response = match self.public_request(&context, method.clone(), path, body) {
                Ok(request) => match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        last_error = Some(error.into());
                        if !can_retry_transport {
                            break;
                        }
                        if attempt + 1 < MAX_RETRIES_PER_CREDENTIAL {
                            sleep(retry_delay(attempt)).await;
                        }
                        continue;
                    }
                },
                Err(error) => return Err(error),
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
                self.token_manager.report_quota_exhausted(context.id);
                return Err(public_api_error(path, status, &response_body));
            }
            if matches!(status.as_u16(), 401 | 403)
                && context.credentials.is_oauth()
                && !forced_refresh
            {
                forced_refresh = true;
                tracing::info!(
                    credential_id = context.id,
                    path,
                    "视频轮询认证失败，尝试刷新 OAuth token"
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
            last_error = Some(public_api_error(path, status, &response_body));
            if matches!(status.as_u16(), 401 | 403 | 408 | 429) || status.is_server_error() {
                self.token_manager.report_failure(context.id);
                if !can_retry_transport {
                    break;
                }
                if attempt + 1 < MAX_RETRIES_PER_CREDENTIAL {
                    sleep(retry_delay(attempt)).await;
                    continue;
                }
            }
            break;
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("xAI Imagine API 请求失败: {path}")))
    }

    /// 将 xAI 的视频 request id 包装成不可猜测的代理 id，并绑定创建任务的
    /// 实际凭据资源池。调用方只需轮询这个 opaque id。
    pub fn register_video_job(
        &self,
        upstream_request_id: &str,
        credential_id: u64,
    ) -> anyhow::Result<String> {
        let upstream_request_id = upstream_request_id.trim();
        if upstream_request_id.is_empty() {
            anyhow::bail!("xAI 视频生成响应缺少 request_id");
        }
        let pools = self
            .token_manager
            .credential(credential_id)
            .map(|credential| credential.effective_pools())
            .ok_or_else(|| anyhow::anyhow!("创建视频任务的 Grok 凭据已不存在"))?;
        let request_id = format!("video_{}", Uuid::new_v4().simple());
        let mut jobs = self.video_jobs.lock();
        cleanup_video_jobs(&mut jobs);
        jobs.insert(
            request_id.clone(),
            VideoJob {
                upstream_request_id: upstream_request_id.to_string(),
                credential_id,
                pools,
                created_at: Instant::now(),
            },
        );
        Ok(request_id)
    }

    /// 轮询已注册的视频任务。请求方必须仍然拥有该凭据所在的至少一个资源池。
    pub async fn poll_video_job(
        &self,
        request_id: &str,
        allowed_pools: &[String],
    ) -> anyhow::Result<GrokUpstreamResponse> {
        let job = {
            let mut jobs = self.video_jobs.lock();
            cleanup_video_jobs(&mut jobs);
            let job = jobs
                .get(request_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("视频任务不存在或已过期"))?;
            if !job.pools.iter().any(|pool| allowed_pools.contains(pool)) {
                anyhow::bail!("当前 API Key 无权访问该视频任务");
            }
            job
        };
        let encoded_request_id = urlencoding::encode(&job.upstream_request_id);
        let path = format!("/videos/{encoded_request_id}");
        self.call_public_api_for_credential(job.credential_id, Method::GET, &path, None)
            .await
    }

    fn public_request(
        &self,
        context: &GrokCallContext,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<RequestBuilder> {
        let timeout = public_request_timeout(&method, path);
        let url = self.public_api_url(path)?;
        let mut request = self
            .client_for(&context.credentials)?
            .request(method, url)
            .header("accept", "application/json");
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .json(body);
        }
        Ok(Self::authenticated_media_request(
            request,
            &context.credentials,
            &context.token,
        ))
    }

    fn public_api_url(&self, path: &str) -> anyhow::Result<String> {
        if !path.starts_with('/') || path.contains("..") {
            anyhow::bail!("非法 xAI 公共 API 路径");
        }
        let configured = self.token_manager.config().grok_base_url.trim();
        let base_url = if is_cli_chat_proxy_url(configured) {
            XAI_DEFAULT_BASE_URL
        } else {
            configured
        };
        let url = format!("{}{}", base_url.trim_end_matches('/'), path);
        Url::parse(&url).context("xAI 公共 API 地址无效")?;
        Ok(url)
    }

    /// 已加载凭据目录的并集视图。请求实际发送时不要依赖这个结果做授权判断，
    /// 应由 `call_api` 的 per-credential 选择再次过滤。
    pub fn model_catalog(&self) -> Option<Arc<GrokModelCatalog>> {
        self.token_manager.merged_catalog()
    }

    async fn fetch_models_value(
        &self,
        id: u64,
        report_runtime_result: bool,
    ) -> anyhow::Result<(Value, String)> {
        let context = self.token_manager.acquire_context_for(id).await?;
        let base_url = context
            .credentials
            .effective_base_url(self.token_manager.config())
            .trim_end_matches('/')
            .to_string();
        let url = format!("{}/models", base_url);
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
            if report_runtime_result {
                self.token_manager.report_failure(context.id);
            }
            anyhow::bail!("xAI /models 请求失败: {} {}", status, body);
        }
        if report_runtime_result {
            self.token_manager.report_success(context.id);
        }
        let value = response.json().await.context("解析 xAI /models 响应失败")?;
        Ok((value, base_url))
    }

    /// 拉取并解析单凭据目录。目录是控制平面数据，失败时不得把推理凭据记为
    /// 失败或禁用；调用方会保留上一次成功的目录。
    async fn fetch_model_catalog_for_credential(
        &self,
        id: u64,
    ) -> anyhow::Result<GrokModelCatalog> {
        let (value, base_url) = self.fetch_models_value(id, false).await?;
        Ok(GrokModelCatalog::from_upstream(&value, &base_url))
    }

    /// 获取指定凭据目录。未强制刷新时优先返回内存 cache。
    pub async fn get_model_catalog_for(
        &self,
        id: u64,
        force_refresh: bool,
    ) -> anyhow::Result<(GrokModelCatalog, bool)> {
        if !force_refresh {
            if let Some(catalog) = self.token_manager.catalog_for(id) {
                return Ok(((*catalog).clone(), true));
            }
        }
        let catalog = self.fetch_model_catalog_for_credential(id).await?;
        self.token_manager.set_model_catalog(id, catalog.clone())?;
        Ok((catalog, false))
    }

    /// 刷新全部启用凭据的真实模型目录。与 Kiro 的 per-credential catalog
    /// 策略一致：一个账户的 `/models` 不可达不会影响其他账户，也不会把该
    /// 账户的运行时推理能力判定为故障。
    pub async fn refresh_model_catalog(&self, force: bool) -> anyhow::Result<()> {
        if !force {
            if self
                .catalog_refresh_at
                .lock()
                .is_some_and(|last| last.elapsed() < Duration::from_secs(300))
            {
                tracing::debug!("Grok 模型目录最近已刷新，跳过本次刷新");
                return Ok(());
            }
        }
        let ids = self.token_manager.active_credential_ids();
        if ids.is_empty() {
            tracing::debug!("没有启用的 Grok 凭据，跳过模型目录刷新");
            return Ok(());
        }
        let results = join_all(
            ids.into_iter()
                .map(|id| async move { (id, self.fetch_model_catalog_for_credential(id).await) }),
        )
        .await;
        let mut got_any = false;
        for (id, result) in results {
            match result {
                Ok(catalog) => {
                    let count = catalog.models.len();
                    if let Err(error) = self.token_manager.set_model_catalog(id, catalog) {
                        tracing::warn!(credential_id = id, %error, "写入 Grok 模型目录失败");
                    } else {
                        got_any = true;
                        tracing::info!(
                            credential_id = id,
                            model_count = count,
                            "Grok 模型目录刷新成功"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(credential_id = id, %error, "Grok 模型目录拉取失败，保留凭据与旧目录");
                }
            }
        }
        if got_any {
            *self.catalog_refresh_at.lock() = Some(std::time::Instant::now());
        }
        Ok(())
    }

    /// 使用对应凭据调用 xAI models endpoint，用于管理接口 token 校验，并在
    /// 成功时顺手更新这张凭据的模型目录。
    pub async fn verify_credential(&self, id: u64) -> anyhow::Result<Value> {
        let (value, base_url) = self.fetch_models_value(id, true).await?;
        let catalog = GrokModelCatalog::from_upstream(&value, &base_url);
        self.token_manager.set_model_catalog(id, catalog)?;
        Ok(value)
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
        let response =
            Self::authenticated_request(request, &context.credentials, &context.token, true)
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

fn public_method_is_idempotent(method: &Method) -> bool {
    method == Method::GET || method == Method::HEAD || method == Method::OPTIONS
}

/// 对齐 Grok Build video client：创建任务最长等 60 秒；单次轮询最长等 30
/// 秒。图片生成沿用共享 HTTP client 的 300 秒 timeout。
fn public_request_timeout(method: &Method, path: &str) -> Option<Duration> {
    if method == Method::POST && path == "/videos/generations" {
        Some(Duration::from_secs(60))
    } else if method == Method::GET && path.starts_with("/videos/") {
        Some(Duration::from_secs(30))
    } else {
        None
    }
}

fn cleanup_video_jobs(jobs: &mut HashMap<String, VideoJob>) {
    jobs.retain(|_, job| job.created_at.elapsed() < VIDEO_JOB_TTL);
}

fn public_api_error(path: &str, status: reqwest::StatusCode, body: &str) -> anyhow::Error {
    anyhow::anyhow!("xAI Imagine API 请求失败: {status} {path} {body}")
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis((500_u64.saturating_mul(1_u64 << attempt.min(4))).min(8_000))
}

/// catalog 的 `baseUrl` 通常是 `/v1` 根路径，但为兼容私有网关也接受已经
/// 包含 endpoint 的地址，避免拼出 `/responses/responses`。
fn endpoint_url(base_url: &str, backend: GrokApiBackend) -> String {
    let base_url = base_url.trim_end_matches('/');
    let path = backend.endpoint_path();
    if base_url.ends_with(path) {
        base_url.to_string()
    } else {
        format!("{base_url}/{path}")
    }
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

    #[test]
    fn builds_backend_url_once() {
        assert_eq!(
            endpoint_url("https://api.x.ai/v1", GrokApiBackend::ChatCompletions),
            "https://api.x.ai/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.x.ai/v1/responses/", GrokApiBackend::Responses),
            "https://api.x.ai/v1/responses"
        );
    }
}
