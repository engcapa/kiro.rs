//! Grok Build 多凭据管理与 xAI OAuth token 刷新。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::{Config, TlsBackend};

use super::credentials::{GrokCredentials, XAI_GROK_CLI_CLIENT_ID, jwt_identity};
use super::model_catalog::{
    GrokApiBackend, GrokCredentialModelIndex, GrokModel, GrokModelCatalog, ReasoningEffort,
    merge_catalogs,
};

const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
const LOAD_BALANCING_MODE_PRIORITY: &str = "priority";
const LOAD_BALANCING_MODE_BALANCED: &str = "balanced";
const LOAD_BALANCING_MODE_ROUND_ROBIN: &str = "round_robin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    Manual,
    TooManyFailures,
    TooManyRefreshFailures,
    QuotaExceeded,
    InvalidRefreshToken,
}

impl DisabledReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::TooManyFailures => "too_many_failures",
            Self::TooManyRefreshFailures => "too_many_refresh_failures",
            Self::QuotaExceeded => "quota_exhausted",
            Self::InvalidRefreshToken => "invalid_refresh_token",
        }
    }
}

struct CredentialEntry {
    id: u64,
    credentials: GrokCredentials,
    failure_count: u32,
    refresh_failure_count: u32,
    disabled: bool,
    disabled_reason: Option<DisabledReason>,
    success_count: u64,
    last_used_at: Option<String>,
    /// 每张凭据从 `/v1/models` 取得的真实模型目录。目录拉取失败不能影响
    /// 推理可用性，因此 `None` 代表未知而不是“不支持任何模型”。
    catalog: Option<Arc<GrokModelCatalog>>,
    /// `catalog` 的 O(1) 热路径索引，避免每次路由都线性扫描模型数组。
    model_index: Option<GrokCredentialModelIndex>,
}

/// 管理接口使用的安全凭据快照，不包含原始 token。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokCredentialSnapshot {
    pub id: u64,
    pub name: String,
    pub priority: u32,
    pub disabled: bool,
    pub failure_count: u32,
    pub refresh_failure_count: u32,
    pub is_current: bool,
    pub expires_at: Option<String>,
    pub auth_method: Option<String>,
    /// 为保持 `/grok/api/admin` 与原 Admin UI 的响应形状一致而保留；
    /// xAI 凭据不使用 AWS Profile ARN，恒为 false / None。
    pub has_profile_arn: bool,
    pub profile_arn: Option<String>,
    pub imported_at: Option<String>,
    pub refresh_token_hash: Option<String>,
    pub api_key_hash: Option<String>,
    pub masked_api_key: Option<String>,
    pub email: Option<String>,
    pub user_name: Option<String>,
    pub subject: Option<String>,
    pub success_count: u64,
    pub last_used_at: Option<String>,
    pub has_proxy: bool,
    pub proxy_url: Option<String>,
    pub disabled_reason: Option<String>,
    pub endpoint: String,
    pub pools: Vec<String>,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokManagerSnapshot {
    pub entries: Vec<GrokCredentialSnapshot>,
    pub current_id: u64,
    pub total: usize,
    pub available: usize,
}

/// 一次调用所绑定的凭据，避免并发请求在凭据切换时混用 token。
#[derive(Clone)]
pub struct GrokCallContext {
    pub id: u64,
    pub credentials: GrokCredentials,
    pub token: String,
}

#[derive(Deserialize)]
struct RefreshTokenResponse {
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

/// 仅管理 Grok 凭据；Kiro 的 `MultiTokenManager` 不会与其共享状态。
pub struct GrokTokenManager {
    config: Config,
    global_proxy: Option<ProxyConfig>,
    tls_backend: TlsBackend,
    entries: Mutex<Vec<CredentialEntry>>,
    current_id: Mutex<u64>,
    refresh_lock: AsyncMutex<()>,
    credentials_path: PathBuf,
    load_balancing_mode: Mutex<String>,
}

impl GrokTokenManager {
    pub fn new(
        config: Config,
        credentials: Vec<GrokCredentials>,
        global_proxy: Option<ProxyConfig>,
        credentials_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut next_id = credentials
            .iter()
            .filter_map(|credential| credential.id)
            .max()
            .unwrap_or(0)
            + 1;
        let mut requires_persist = false;
        let now = Utc::now().to_rfc3339();

        let entries = credentials
            .into_iter()
            .map(|mut credentials| {
                credentials.canonicalize();
                let id = credentials.id.unwrap_or_else(|| {
                    let id = next_id;
                    next_id += 1;
                    credentials.id = Some(id);
                    requires_persist = true;
                    id
                });
                if credentials.imported_at.is_none() {
                    credentials.imported_at = Some(now.clone());
                    requires_persist = true;
                }
                if credentials.name.is_none() {
                    credentials.name = Some(credentials.display_name(id));
                    requires_persist = true;
                }
                CredentialEntry {
                    id,
                    disabled: credentials.disabled,
                    credentials,
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled_reason: None,
                    success_count: 0,
                    last_used_at: None,
                    catalog: None,
                    model_index: None,
                }
            })
            .collect::<Vec<_>>();

        let current_id = entries
            .iter()
            .filter(|entry| !entry.disabled)
            .min_by_key(|entry| entry.credentials.priority)
            .map(|entry| entry.id)
            .unwrap_or(0);

        let mode = normalize_load_balancing_mode(&config.load_balancing_mode)
            .unwrap_or(LOAD_BALANCING_MODE_ROUND_ROBIN)
            .to_string();
        let manager = Self {
            tls_backend: config.tls_backend,
            config,
            global_proxy,
            entries: Mutex::new(entries),
            current_id: Mutex::new(current_id),
            refresh_lock: AsyncMutex::new(()),
            credentials_path,
            load_balancing_mode: Mutex::new(mode),
        };
        if requires_persist {
            manager.persist_credentials()?;
        }
        Ok(manager)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path.parent().map(Path::to_path_buf)
    }

    pub fn snapshot(&self) -> GrokManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let visible_current_id = entries
            .iter()
            .any(|entry| entry.id == current_id && !entry.disabled)
            .then_some(current_id)
            .unwrap_or(0);
        let credentials = entries
            .iter()
            .map(|entry| self.snapshot_entry(entry, visible_current_id))
            .collect::<Vec<_>>();
        let available = entries.iter().filter(|entry| !entry.disabled).count();
        GrokManagerSnapshot {
            total: entries.len(),
            available,
            current_id: visible_current_id,
            entries: credentials,
        }
    }

    fn snapshot_entry(&self, entry: &CredentialEntry, current_id: u64) -> GrokCredentialSnapshot {
        let token = entry.credentials.access_token.as_deref();
        GrokCredentialSnapshot {
            id: entry.id,
            name: entry.credentials.display_name(entry.id),
            priority: entry.credentials.priority,
            disabled: entry.disabled,
            failure_count: entry.failure_count,
            refresh_failure_count: entry.refresh_failure_count,
            is_current: entry.id == current_id,
            expires_at: entry.credentials.expires_at.clone(),
            auth_method: entry.credentials.auth_method.clone(),
            has_profile_arn: false,
            profile_arn: None,
            imported_at: entry.credentials.imported_at.clone(),
            refresh_token_hash: entry.credentials.refresh_token.as_deref().map(sha256_hex),
            api_key_hash: token.map(sha256_hex),
            masked_api_key: token.map(mask_token),
            email: entry.credentials.email.clone(),
            user_name: entry.credentials.email.clone(),
            subject: entry.credentials.subject.clone(),
            success_count: entry.success_count,
            last_used_at: entry.last_used_at.clone(),
            has_proxy: entry.credentials.proxy_url.is_some(),
            proxy_url: entry.credentials.proxy_url.clone(),
            disabled_reason: entry
                .disabled_reason
                .map(|reason| reason.as_str().to_string()),
            endpoint: "grok-build".to_string(),
            pools: entry.credentials.effective_pools(),
            base_url: normalized_base_url(entry.credentials.effective_base_url(&self.config)),
        }
    }

    pub fn credential(&self, id: u64) -> Option<GrokCredentials> {
        self.entries
            .lock()
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.credentials.clone())
    }

    /// 返回当前可参与目录刷新（未手动/故障禁用）的凭据 id。
    pub fn active_credential_ids(&self) -> Vec<u64> {
        self.entries
            .lock()
            .iter()
            .filter(|entry| !entry.disabled)
            .map(|entry| entry.id)
            .collect()
    }

    /// 读取单凭据目录。`None` 代表尚未加载或上次刷新失败，应由调用方按
    /// “未知放行”策略处理，而不是理解为该凭据没有模型。
    pub fn catalog_for(&self, id: u64) -> Option<Arc<GrokModelCatalog>> {
        self.entries
            .lock()
            .iter()
            .find(|entry| entry.id == id)
            .and_then(|entry| entry.catalog.clone())
    }

    /// 取得所有启用凭据的已加载目录合并后的只读视图，供 HTTP `/models` 和
    /// 请求模型名规范化使用。真正选凭据时仍必须调用 [`Self::acquire_context`]。
    pub fn merged_catalog(&self) -> Option<Arc<GrokModelCatalog>> {
        let catalogs = self
            .entries
            .lock()
            .iter()
            .filter(|entry| !entry.disabled)
            .filter_map(|entry| entry.catalog.as_ref().map(|catalog| (**catalog).clone()))
            .collect::<Vec<_>>();
        (!catalogs.is_empty()).then(|| Arc::new(merge_catalogs(&catalogs)))
    }

    /// 写入一张凭据最新的目录并同步重建热路径索引。
    pub fn set_model_catalog(&self, id: u64, catalog: GrokModelCatalog) -> anyhow::Result<()> {
        let index = GrokCredentialModelIndex::from_catalog(&catalog);
        let mut entries = self.entries.lock();
        let entry = entries
            .iter_mut()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
        entry.catalog = Some(Arc::new(catalog));
        entry.model_index = Some(index);
        Ok(())
    }

    /// 返回该凭据上某个已规范化模型的真实配置（含 API backend/baseUrl）。
    pub fn model_for(&self, id: u64, model_id: &str) -> Option<GrokModel> {
        self.catalog_for(id)
            .and_then(|catalog| catalog.model_by_id(model_id).cloned())
    }

    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
            entry.disabled = disabled;
            entry.credentials.disabled = disabled;
            entry.disabled_reason = if disabled {
                Some(DisabledReason::Manual)
            } else {
                None
            };
            if !disabled {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
            }
        }
        self.repair_current_cursor();
        self.persist_credentials()
    }

    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
            entry.credentials.priority = priority;
        }
        self.repair_current_cursor();
        self.persist_credentials()
    }

    pub fn set_name(&self, id: u64, name: String) -> anyhow::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("凭据名称不能为空");
        }
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
            entry.credentials.name = Some(name.to_string());
        }
        self.persist_credentials()
    }

    pub fn set_pools(&self, id: u64, pools: Vec<String>) -> anyhow::Result<()> {
        let pools = normalize_pools(pools);
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
            entry.credentials.pools = (!pools.is_empty()).then_some(pools);
        }
        self.persist_credentials()
    }

    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
            entry.disabled = false;
            entry.credentials.disabled = false;
            entry.disabled_reason = None;
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
        }
        self.repair_current_cursor();
        self.persist_credentials()
    }

    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let before = entries.len();
            entries.retain(|entry| entry.id != id);
            if entries.len() == before {
                bail!("Grok 凭据 #{} 不存在", id);
            }
        }
        self.repair_current_cursor();
        self.persist_credentials()
    }

    pub fn add_credential(&self, mut credentials: GrokCredentials) -> anyhow::Result<u64> {
        credentials.canonicalize();
        if credentials.access_token.is_none() && credentials.refresh_token.is_none() {
            bail!("需要提供 accessToken / token，或 OAuth refreshToken");
        }

        let id = {
            let mut entries = self.entries.lock();
            if entries.iter().any(|entry| {
                credentials
                    .access_token
                    .as_ref()
                    .is_some_and(|token| entry.credentials.access_token.as_ref() == Some(token))
                    || credentials.refresh_token.as_ref().is_some_and(|token| {
                        entry.credentials.refresh_token.as_ref() == Some(token)
                    })
            }) {
                bail!("重复的 Grok accessToken 或 refreshToken");
            }
            let id = entries.iter().map(|entry| entry.id).max().unwrap_or(0) + 1;
            credentials.id = Some(id);
            credentials
                .imported_at
                .get_or_insert_with(|| Utc::now().to_rfc3339());
            if credentials.name.is_none() {
                let display_name = credentials.display_name(id);
                credentials.name = Some(display_name);
            }
            entries.push(CredentialEntry {
                id,
                disabled: credentials.disabled,
                credentials,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                catalog: None,
                model_index: None,
            });
            id
        };
        self.repair_current_cursor();
        self.persist_credentials()?;
        Ok(id)
    }

    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        let mode = normalize_load_balancing_mode(&mode)
            .ok_or_else(|| anyhow::anyhow!("mode 必须是 round_robin、priority 或 balanced"))?;
        *self.load_balancing_mode.lock() = mode.to_string();
        self.repair_current_cursor();
        Ok(())
    }

    pub fn peek_next_credential_name(&self, allowed_pools: Option<&[String]>) -> Option<String> {
        let id = self
            .choose_credential_id(None, None, None, false, allowed_pools)
            .ok()?;
        self.credential(id)
            .map(|credential| credential.display_name(id))
    }

    /// 获得支持指定模型/effort/backend 的可用凭据，并在 OAuth token 接近过期
    /// 时自动刷新。目录未加载的凭据会被保守地放行，保持控制平面抖动时的服务
    /// 可用性；目录已加载的凭据则严格按其模型能力过滤。`requires_backend_search`
    /// 对应 Grok Build catalog 的 `supportsBackendSearch`，避免把 Responses
    /// hosted search 投递给不支持它的凭据。
    pub async fn acquire_context(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<GrokCallContext> {
        let attempts = self.total_count().max(1) * MAX_FAILURES_PER_CREDENTIAL as usize;
        let mut last_error = None;
        for _ in 0..attempts {
            let id = match self.choose_credential_id(
                model_id,
                reasoning_effort,
                backend,
                requires_backend_search,
                allowed_pools,
            ) {
                Ok(id) => id,
                Err(error) => return Err(error),
            };
            match self.acquire_context_for(id).await {
                Ok(context) => return Ok(context),
                Err(error) => {
                    last_error = Some(error);
                    if !self.report_refresh_failure(id) {
                        break;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("没有可用的 Grok 凭据")))
    }

    pub async fn acquire_context_for(&self, id: u64) -> anyhow::Result<GrokCallContext> {
        let credentials = self.refresh_credential_if_needed(id, false).await?;
        if credentials.disabled {
            bail!("Grok 凭据 #{} 已禁用", id);
        }
        let token = credentials
            .access_token
            .clone()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 没有可用 accessToken", id))?;
        Ok(GrokCallContext {
            id,
            credentials,
            token,
        })
    }

    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<GrokCredentials> {
        self.refresh_credential_if_needed(id, true).await
    }

    async fn refresh_credential_if_needed(
        &self,
        id: u64,
        force: bool,
    ) -> anyhow::Result<GrokCredentials> {
        let _guard = self.refresh_lock.lock().await;
        let current = self
            .credential(id)
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
        if current.disabled {
            bail!("Grok 凭据 #{} 已禁用", id);
        }

        let needs_refresh = force || current.access_token.is_none() || is_expiring_soon(&current);
        if !needs_refresh {
            return Ok(current);
        }
        let refresh_token = current
            .refresh_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 已过期且没有 refreshToken", id))?;

        let proxy = current.effective_proxy(self.global_proxy.as_ref());
        let client = build_client(proxy.as_ref(), 60, self.tls_backend)?;
        let response = client
            .post(current.effective_token_endpoint())
            .header("accept", "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", XAI_GROK_CLI_CLIENT_ID),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .context("发送 xAI OAuth 刷新请求失败")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if body.contains("invalid_grant") {
                self.disable_with_reason(id, DisabledReason::InvalidRefreshToken);
            }
            bail!("xAI OAuth token 刷新失败: {} {}", status, body);
        }
        let refreshed: RefreshTokenResponse = response
            .json()
            .await
            .context("解析 xAI OAuth token 刷新响应失败")?;
        let access_token = refreshed
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("xAI OAuth 刷新响应缺少 access_token"))?;

        let mut updated = current.clone();
        updated.access_token = Some(access_token.clone());
        if let Some(refresh_token) = refreshed
            .refresh_token
            .filter(|token| !token.trim().is_empty())
        {
            updated.refresh_token = Some(refresh_token);
        }
        if let Some(id_token) = refreshed.id_token.filter(|token| !token.trim().is_empty()) {
            updated.id_token = Some(id_token);
        }
        if let Some(token_type) = refreshed
            .token_type
            .filter(|token| !token.trim().is_empty())
        {
            updated.token_type = Some(token_type);
        }
        if let Some(user_id) = refreshed.user_id.filter(|value| !value.trim().is_empty()) {
            updated.user_id = Some(user_id);
        }
        if let Some(team_id) = refreshed.team_id.filter(|value| !value.trim().is_empty()) {
            updated.team_id = Some(team_id);
        }
        let expires_in = refreshed.expires_in.unwrap_or(3600).max(0);
        updated.expires_at = Some((Utc::now() + Duration::seconds(expires_in)).to_rfc3339());
        updated.last_refresh = Some(Utc::now().to_rfc3339());
        updated.auth_method = Some("oauth".to_string());
        let identity_token = updated.id_token.as_deref().unwrap_or(&access_token);
        let (email, subject) = jwt_identity(identity_token);
        if updated.email.is_none() {
            updated.email = email;
        }
        if updated.subject.is_none() {
            updated.subject = subject;
        }
        updated.canonicalize();

        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 在刷新期间被删除", id))?;
            entry.credentials = updated.clone();
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.credentials.disabled = false;
            entry.disabled_reason = None;
        }
        self.persist_credentials()?;
        Ok(updated)
    }

    pub fn report_success(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.success_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
        }
    }

    /// 报告可重试的上游调用错误。连续三次后自动禁用该凭据。
    pub fn report_failure(&self, id: u64) -> bool {
        let mut changed = false;
        let available = {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                return entries.iter().any(|entry| !entry.disabled);
            };
            if entry.disabled {
                return entries.iter().any(|entry| !entry.disabled);
            }
            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            if entry.failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.credentials.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                changed = true;
            }
            entries.iter().any(|entry| !entry.disabled)
        };
        if changed {
            self.repair_current_cursor();
            if let Err(error) = self.persist_credentials() {
                tracing::warn!(%error, "保存自动禁用的 Grok 凭据失败");
            }
        }
        available
    }

    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let mut changed = false;
        let available = {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                return entries.iter().any(|entry| !entry.disabled);
            };
            if entry.disabled {
                return entries.iter().any(|entry| !entry.disabled);
            }
            entry.refresh_failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            if entry.refresh_failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.credentials.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);
                changed = true;
            }
            entries.iter().any(|entry| !entry.disabled)
        };
        if changed {
            self.repair_current_cursor();
            if let Err(error) = self.persist_credentials() {
                tracing::warn!(%error, "保存自动禁用的 Grok 凭据失败");
            }
        }
        available
    }

    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        self.disable_with_reason(id, DisabledReason::QuotaExceeded);
        self.entries.lock().iter().any(|entry| !entry.disabled)
    }

    fn disable_with_reason(&self, id: u64, reason: DisabledReason) {
        let changed = {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
                return;
            };
            if entry.disabled {
                false
            } else {
                entry.disabled = true;
                entry.credentials.disabled = true;
                entry.disabled_reason = Some(reason);
                true
            }
        };
        if changed {
            self.repair_current_cursor();
            if let Err(error) = self.persist_credentials() {
                tracing::warn!(%error, "保存自动禁用的 Grok 凭据失败");
            }
        }
    }

    fn choose_credential_id(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<u64> {
        let entries = self.entries.lock();
        if entries.is_empty() {
            bail!(
                "未配置 Grok 凭据；请在 grok_credentials.json 中添加 token，或通过 /grok/api/admin/oauth/start 授权"
            );
        }
        let eligible = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                !entry.disabled
                    && pool_matches(entry, allowed_pools)
                    && credential_supports(
                        entry,
                        model_id,
                        reasoning_effort,
                        backend,
                        requires_backend_search,
                    )
            })
            .collect::<Vec<_>>();
        if eligible.is_empty() {
            if let Some(model_id) = model_id {
                let backend = backend.map(GrokApiBackend::as_str).unwrap_or("任意后端");
                let effort = reasoning_effort
                    .map(|effort| format!("、effort={effort}"))
                    .unwrap_or_default();
                let backend_search = requires_backend_search
                    .then_some("、supportsBackendSearch=true")
                    .unwrap_or_default();
                bail!(
                    "没有 Grok 凭据支持模型 {}（backend={}{}{}）或当前 API Key 资源池",
                    model_id,
                    backend,
                    effort,
                    backend_search,
                );
            }
            bail!("没有可用于当前 API Key 资源池的 Grok 凭据");
        }
        let mode = self.get_load_balancing_mode();
        let id = match mode.as_str() {
            LOAD_BALANCING_MODE_PRIORITY => eligible
                .iter()
                .min_by_key(|(_, entry)| entry.credentials.priority)
                .map(|(_, entry)| entry.id),
            LOAD_BALANCING_MODE_BALANCED => eligible
                .iter()
                .min_by_key(|(_, entry)| (entry.success_count, entry.credentials.priority))
                .map(|(_, entry)| entry.id),
            _ => {
                let current_id = *self.current_id.lock();
                let start = entries
                    .iter()
                    .position(|entry| entry.id == current_id)
                    .map(|index| (index + 1) % entries.len())
                    .unwrap_or(0);
                (0..entries.len())
                    .map(|offset| (start + offset) % entries.len())
                    .find_map(|index| {
                        let entry = &entries[index];
                        (!entry.disabled
                            && pool_matches(entry, allowed_pools)
                            && credential_supports(
                                entry,
                                model_id,
                                reasoning_effort,
                                backend,
                                requires_backend_search,
                            ))
                        .then_some(entry.id)
                    })
            }
        }
        .ok_or_else(|| anyhow::anyhow!("没有可用的 Grok 凭据"))?;
        drop(entries);
        *self.current_id.lock() = id;
        Ok(id)
    }

    fn repair_current_cursor(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();
        if entries
            .iter()
            .any(|entry| entry.id == *current_id && !entry.disabled)
        {
            return;
        }
        *current_id = entries
            .iter()
            .filter(|entry| !entry.disabled)
            .min_by_key(|entry| entry.credentials.priority)
            .map(|entry| entry.id)
            .unwrap_or(0);
    }

    fn persist_credentials(&self) -> anyhow::Result<()> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|entry| {
                    let mut credential = entry.credentials.clone();
                    credential.disabled = entry.disabled;
                    credential
                })
                .collect::<Vec<_>>()
        };
        let json = serde_json::to_string_pretty(&credentials).context("序列化 Grok 凭据失败")?;
        if let Some(parent) = self.credentials_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 Grok 凭据目录失败: {}", parent.display()))?;
        }
        let temporary_path = self.credentials_path.with_extension("json.tmp");
        std::fs::write(&temporary_path, json)
            .with_context(|| format!("写入临时 Grok 凭据文件失败: {}", temporary_path.display()))?;
        std::fs::rename(&temporary_path, &self.credentials_path).with_context(|| {
            format!(
                "替换 Grok 凭据文件失败: {}",
                self.credentials_path.display()
            )
        })?;
        Ok(())
    }
}

fn normalize_load_balancing_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        LOAD_BALANCING_MODE_PRIORITY => Some(LOAD_BALANCING_MODE_PRIORITY),
        LOAD_BALANCING_MODE_BALANCED => Some(LOAD_BALANCING_MODE_BALANCED),
        LOAD_BALANCING_MODE_ROUND_ROBIN | "round-robin" => Some(LOAD_BALANCING_MODE_ROUND_ROBIN),
        _ => None,
    }
}

fn is_expiring_soon(credentials: &GrokCredentials) -> bool {
    let Some(expires_at) = credentials.expires_at.as_deref() else {
        return false;
    };
    let Ok(expires_at) = DateTime::parse_from_rfc3339(expires_at) else {
        return true;
    };
    expires_at <= Utc::now() + Duration::minutes(5)
}

fn normalize_pools(pools: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for pool in pools {
        let pool = pool.trim();
        if !pool.is_empty() && !normalized.iter().any(|existing| existing == pool) {
            normalized.push(pool.to_string());
        }
    }
    normalized
}

fn pool_matches(entry: &CredentialEntry, allowed_pools: Option<&[String]>) -> bool {
    match allowed_pools {
        None => true,
        Some(allowed_pools) => entry
            .credentials
            .effective_pools()
            .iter()
            .any(|pool| allowed_pools.contains(pool)),
    }
}

/// 已加载目录时严格过滤；目录尚未取得时未知放行。这样模型控制平面短暂故障
/// 不会把本来可推理的 OAuth/API-token 凭据排除在外。
fn credential_supports(
    entry: &CredentialEntry,
    model_id: Option<&str>,
    reasoning_effort: Option<ReasoningEffort>,
    backend: Option<GrokApiBackend>,
    requires_backend_search: bool,
) -> bool {
    let Some(model_id) = model_id else {
        return true;
    };
    entry.model_index.as_ref().is_none_or(|index| {
        index.supports(model_id, reasoning_effort, backend, requires_backend_search)
    })
}

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn mask_token(value: &str) -> String {
    if value.is_ascii() && value.len() > 12 {
        format!("{}...{}", &value[..4], &value[value.len() - 4..])
    } else {
        "***".to_string()
    }
}

fn normalized_base_url(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

pub type SharedGrokTokenManager = Arc<GrokTokenManager>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("kiro-rs-{name}-{stamp}.json"))
    }

    fn manager(credentials: Vec<GrokCredentials>) -> GrokTokenManager {
        GrokTokenManager::new(
            Config::default(),
            credentials,
            None,
            temp_path("grok-creds"),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn chooses_credentials_by_pool() {
        let manager = manager(vec![
            GrokCredentials {
                id: Some(1),
                access_token: Some("token-one".to_string()),
                pools: Some(vec!["one".to_string()]),
                ..Default::default()
            },
            GrokCredentials {
                id: Some(2),
                access_token: Some("token-two".to_string()),
                pools: Some(vec!["two".to_string()]),
                ..Default::default()
            },
        ]);
        let pools = vec!["two".to_string()];
        let context = manager
            .acquire_context(None, None, None, false, Some(&pools))
            .await
            .unwrap();
        assert_eq!(context.id, 2);
    }

    #[test]
    fn failure_disables_after_threshold() {
        let manager = manager(vec![GrokCredentials {
            id: Some(1),
            access_token: Some("token-one".to_string()),
            ..Default::default()
        }]);
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert!(!manager.report_failure(1));
        assert!(manager.snapshot().entries[0].disabled);
    }

    #[tokio::test]
    async fn chooses_only_credential_whose_catalog_supports_model_and_effort() {
        let manager = manager(vec![
            GrokCredentials {
                id: Some(1),
                access_token: Some("token-one".to_string()),
                ..Default::default()
            },
            GrokCredentials {
                id: Some(2),
                access_token: Some("token-two".to_string()),
                ..Default::default()
            },
        ]);
        manager
            .set_model_catalog(
                1,
                GrokModelCatalog::from_upstream(
                    &json!({"data":[{
                        "model":"grok-4.5",
                        "apiBackend":"responses",
                        "supportsReasoningEffort":true,
                        "reasoningEfforts":["low","medium","high"]
                    }]}),
                    "https://api.x.ai/v1",
                ),
            )
            .unwrap();
        manager
            .set_model_catalog(
                2,
                GrokModelCatalog::from_upstream(
                    &json!({"data":[{
                        "model":"grok-composer-2.5-fast",
                        "apiBackend":"responses",
                        "supportsReasoningEffort":true,
                        "reasoningEfforts":["low","medium","high","xhigh"]
                    }]}),
                    "https://api.x.ai/v1",
                ),
            )
            .unwrap();

        let context = manager
            .acquire_context(
                Some("grok-composer-2.5-fast"),
                Some(ReasoningEffort::Xhigh),
                Some(GrokApiBackend::Responses),
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(context.id, 2);

        let error = match manager
            .acquire_context(
                Some("grok-4.5"),
                Some(ReasoningEffort::Xhigh),
                Some(GrokApiBackend::Responses),
                false,
                None,
            )
            .await
        {
            Ok(_) => panic!("unsupported effort must not select a credential"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("grok-4.5"));
    }

    #[tokio::test]
    async fn backend_search_only_uses_credential_whose_catalog_supports_it() {
        let manager = manager(vec![
            GrokCredentials {
                id: Some(1),
                access_token: Some("token-one".to_string()),
                ..Default::default()
            },
            GrokCredentials {
                id: Some(2),
                access_token: Some("token-two".to_string()),
                ..Default::default()
            },
        ]);
        manager
            .set_model_catalog(
                1,
                GrokModelCatalog::from_upstream(
                    &json!({"data":[{
                        "model":"grok-4.5",
                        "apiBackend":"responses",
                        "supportsBackendSearch":false
                    }]}),
                    "https://api.x.ai/v1",
                ),
            )
            .unwrap();
        manager
            .set_model_catalog(
                2,
                GrokModelCatalog::from_upstream(
                    &json!({"data":[{
                        "model":"grok-4.5",
                        "apiBackend":"responses",
                        "supportsBackendSearch":true
                    }]}),
                    "https://api.x.ai/v1",
                ),
            )
            .unwrap();

        let context = manager
            .acquire_context(
                Some("grok-4.5"),
                None,
                Some(GrokApiBackend::Responses),
                true,
                None,
            )
            .await
            .unwrap();
        assert_eq!(context.id, 2);
    }
}
