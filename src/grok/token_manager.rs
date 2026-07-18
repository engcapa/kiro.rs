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
use super::reasoning_sig::ReasoningSignatureCodec;

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
    /// 串行化 snapshot -> temp write -> rename 整个事务，防止较旧 snapshot
    /// 在较新写入完成后才 rename，令磁盘状态倒退。
    persist_lock: Mutex<()>,
    credentials_path: PathBuf,
    load_balancing_mode: Mutex<String>,
    reasoning_signature_codec: ReasoningSignatureCodec,
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
        let signature_key_path = credentials_path.with_extension("reasoning.key");
        let reasoning_signature_codec =
            ReasoningSignatureCodec::load_or_create(&signature_key_path)?;
        let manager = Self {
            tls_backend: config.tls_backend,
            config,
            global_proxy,
            entries: Mutex::new(entries),
            current_id: Mutex::new(current_id),
            refresh_lock: AsyncMutex::new(()),
            persist_lock: Mutex::new(()),
            credentials_path,
            load_balancing_mode: Mutex::new(mode),
            reasoning_signature_codec,
        };
        if requires_persist {
            manager.persist_credentials()?;
        }
        Ok(manager)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn reasoning_signature_codec(&self) -> ReasoningSignatureCodec {
        self.reasoning_signature_codec.clone()
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
        let previous = self.get_load_balancing_mode();
        if previous == mode {
            return Ok(());
        }
        *self.load_balancing_mode.lock() = mode.to_string();
        if let Err(error) = self.persist_load_balancing_mode(mode) {
            *self.load_balancing_mode.lock() = previous;
            return Err(error);
        }
        self.repair_current_cursor();
        Ok(())
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!(
                    "配置文件路径未知，Grok 负载均衡模式仅在当前进程生效: {}",
                    mode
                );
                return Ok(());
            }
        };
        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化 Grok 负载均衡模式失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 预览下一张可用凭据的显示名，**不推进** round-robin 游标。
    ///
    /// 过滤参数应与真正的 `acquire_context` / `call_api` 一致，否则日志会
    /// 把“并集里随便一张”误报成即将使用的账号。
    pub fn peek_next_credential_name(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> Option<String> {
        let id = self
            .find_credential_id(
                model_id,
                reasoning_effort,
                backend,
                requires_backend_search,
                allowed_pools,
            )
            .ok()?;
        self.credential_display_name(id)
    }

    /// 返回凭据显示名（若存在）。
    pub fn credential_display_name(&self, id: u64) -> Option<String> {
        self.credential(id)
            .map(|credential| credential.display_name(id))
    }

    /// 为路由/转换选择一张凭据，**不推进** round-robin 游标。
    ///
    /// 仅用于预览；真正请求应使用 [`Self::claim_routing_credential_id`] 锁定
    /// 游标，再 pin 到 `call_api`。
    pub fn find_routing_credential_id(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<u64> {
        self.find_credential_id(
            model_id,
            reasoning_effort,
            backend,
            requires_backend_search,
            allowed_pools,
        )
    }

    /// 锁定路由凭据：查找并推进 round-robin / balanced 游标。
    ///
    /// 调用方应使用返回的 id 做 catalog 转换，并作为 `call_api` 的
    /// `pinned_credential_id`，避免 convert 用 A、send 用 B。
    pub fn claim_routing_credential_id(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<u64> {
        self.choose_credential_id(
            model_id,
            reasoning_effort,
            backend,
            requires_backend_search,
            allowed_pools,
        )
    }

    /// 选择会话路由凭据。已验证 reasoning signature 指向的凭据优先；否则
    /// metadata session key 在当前 eligible 集合上做稳定散列；两者均不存在时
    /// 回退到配置的 round-robin/priority/balanced 策略。
    ///
    /// preferred id 仍会重新检查 disabled、pool、model/backend/search 能力，
    /// 因而历史 signature 不能恢复已撤权或已禁用账号。
    pub fn claim_routing_credential_id_with_affinity(
        &self,
        preferred_id: Option<u64>,
        affinity_key: Option<&str>,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<u64> {
        let affinity_key = affinity_key.map(str::trim).filter(|key| !key.is_empty());
        let selected = {
            let entries = self.entries.lock();
            let eligible = entries
                .iter()
                .filter(|entry| {
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
            preferred_id
                .and_then(|preferred_id| {
                    eligible
                        .iter()
                        .find(|entry| entry.id == preferred_id)
                        .map(|entry| entry.id)
                })
                .or_else(|| {
                    affinity_key.and_then(|key| {
                        if eligible.is_empty() {
                            return None;
                        }
                        let digest = Sha256::digest(key.as_bytes());
                        let bucket = u64::from_be_bytes(digest[..8].try_into().ok()?);
                        Some(eligible[bucket as usize % eligible.len()].id)
                    })
                })
        };
        if let Some(id) = selected {
            *self.current_id.lock() = id;
            return Ok(id);
        }
        self.choose_credential_id(
            model_id,
            reasoning_effort,
            backend,
            requires_backend_search,
            allowed_pools,
        )
    }

    /// 为一次上游调用生成不会重复的故障转移候选列表。首选凭据在仍 eligible
    /// 时排首位，其余账号按当前负载均衡策略排序；required_id 用于 Files 等
    /// 绑定资源，只允许返回指定账号。
    pub fn routing_candidate_ids(
        &self,
        preferred_id: Option<u64>,
        required_id: Option<u64>,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<Vec<u64>> {
        let entries = self.entries.lock();
        if entries.is_empty() {
            bail!("未配置 Grok 凭据");
        }
        let is_eligible = |entry: &&CredentialEntry| {
            !entry.disabled
                && pool_matches(entry, allowed_pools)
                && credential_supports(
                    entry,
                    model_id,
                    reasoning_effort,
                    backend,
                    requires_backend_search,
                )
        };
        if let Some(required_id) = required_id {
            if entries
                .iter()
                .find(|entry| entry.id == required_id)
                .is_some_and(|entry| is_eligible(&entry))
            {
                return Ok(vec![required_id]);
            }
            bail!(
                "绑定资源所用的 Grok 凭据 #{} 已禁用、无权访问当前资源池或不支持模型/backend",
                required_id
            );
        }

        let mode = self.get_load_balancing_mode();
        let mut candidates: Vec<u64> = match mode.as_str() {
            LOAD_BALANCING_MODE_PRIORITY => {
                let mut eligible = entries.iter().filter(is_eligible).collect::<Vec<_>>();
                eligible.sort_by_key(|entry| entry.credentials.priority);
                eligible.into_iter().map(|entry| entry.id).collect()
            }
            LOAD_BALANCING_MODE_BALANCED => {
                let mut eligible = entries.iter().filter(is_eligible).collect::<Vec<_>>();
                eligible.sort_by_key(|entry| (entry.success_count, entry.credentials.priority));
                eligible.into_iter().map(|entry| entry.id).collect()
            }
            _ => {
                let current_id = *self.current_id.lock();
                let start = entries
                    .iter()
                    .position(|entry| entry.id == current_id)
                    .map(|index| (index + 1) % entries.len())
                    .unwrap_or(0);
                (0..entries.len())
                    .map(|offset| &entries[(start + offset) % entries.len()])
                    .filter(is_eligible)
                    .map(|entry| entry.id)
                    .collect()
            }
        };
        if let Some(preferred_id) = preferred_id {
            if let Some(position) = candidates.iter().position(|id| *id == preferred_id) {
                candidates.rotate_left(position);
            }
        }
        if candidates.is_empty() {
            bail!("没有支持当前模型/backend 且属于当前 API Key 资源池的 Grok 凭据");
        }
        *self.current_id.lock() = candidates[0];
        Ok(candidates)
    }

    /// 在候选列表生成后、真正发送请求前重新检查凭据的实时资格。管理面可以
    /// 并发禁用凭据或修改 pools/catalog；provider 不应继续信任较早的候选
    /// 快照。
    pub fn ensure_credential_eligible(
        &self,
        id: u64,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<()> {
        let entries = self.entries.lock();
        let entry = entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 不存在", id))?;
        if entry.disabled {
            bail!("Grok 凭据 #{} 已禁用", id);
        }
        if !pool_matches(entry, allowed_pools) {
            bail!("Grok 凭据 #{} 已无权访问当前 API Key 资源池", id);
        }
        if !credential_supports(
            entry,
            model_id,
            reasoning_effort,
            backend,
            requires_backend_search,
        ) {
            bail!("Grok 凭据 #{} 已不再支持当前模型/backend", id);
        }
        Ok(())
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
        let refresh_token = refreshed
            .refresh_token
            .filter(|token| !token.trim().is_empty());
        let id_token = refreshed.id_token.filter(|token| !token.trim().is_empty());
        let token_type = refreshed
            .token_type
            .filter(|token| !token.trim().is_empty());
        let user_id = refreshed.user_id.filter(|value| !value.trim().is_empty());
        let team_id = refreshed.team_id.filter(|value| !value.trim().is_empty());
        let expires_in = refreshed.expires_in.unwrap_or(3600).max(0);
        let refreshed_at = Utc::now();
        let updated = {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("Grok 凭据 #{} 在刷新期间被删除", id))?;
            // 网络请求期间管理面可能已经修改名称、资源池、优先级、代理或禁用
            // 状态。只合并 OAuth 服务拥有的字段，绝不把请求前的完整快照写回。
            let credentials = &mut entry.credentials;
            credentials.access_token = Some(access_token.clone());
            if let Some(refresh_token) = refresh_token {
                credentials.refresh_token = Some(refresh_token);
            }
            if let Some(id_token) = id_token {
                credentials.id_token = Some(id_token);
            }
            if let Some(token_type) = token_type {
                credentials.token_type = Some(token_type);
            }
            if let Some(user_id) = user_id {
                credentials.user_id = Some(user_id);
            }
            if let Some(team_id) = team_id {
                credentials.team_id = Some(team_id);
            }
            credentials.expires_at =
                Some((refreshed_at + Duration::seconds(expires_in)).to_rfc3339());
            credentials.last_refresh = Some(refreshed_at.to_rfc3339());
            credentials.auth_method = Some("oauth".to_string());
            let (email, subject) = jwt_identity(
                credentials
                    .id_token
                    .as_deref()
                    .unwrap_or(access_token.as_str()),
            );
            if credentials.email.is_none() {
                credentials.email = email;
            }
            if credentials.subject.is_none() {
                credentials.subject = subject;
            }
            credentials.canonicalize();
            credentials.disabled = entry.disabled;
            entry.refresh_failure_count = 0;
            credentials.clone()
        };
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

    /// 按负载均衡策略选出一张可用凭据，并推进 round-robin 游标。
    fn choose_credential_id(
        &self,
        model_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
        allowed_pools: Option<&[String]>,
    ) -> anyhow::Result<u64> {
        let id = self.find_credential_id(
            model_id,
            reasoning_effort,
            backend,
            requires_backend_search,
            allowed_pools,
        )?;
        *self.current_id.lock() = id;
        Ok(id)
    }

    /// 按负载均衡策略选出一张可用凭据，**不**修改 `current_id`。
    fn find_credential_id(
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
        let _persist_guard = self.persist_lock.lock();
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
        // 每次写入使用唯一临时文件，避免并发 refresh/admin 共用固定 .json.tmp 互覆盖。
        let unique = format!(
            "{}.tmp.{}-{}",
            self.credentials_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("grok_credentials.json"),
            std::process::id(),
            uuid::Uuid::new_v4().simple(),
        );
        let temporary_path = self
            .credentials_path
            .parent()
            .map(|parent| parent.join(&unique))
            .unwrap_or_else(|| PathBuf::from(&unique));
        std::fs::write(&temporary_path, json)
            .with_context(|| format!("写入临时 Grok 凭据文件失败: {}", temporary_path.display()))?;
        if let Err(error) = std::fs::rename(&temporary_path, &self.credentials_path) {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error).with_context(|| {
                format!(
                    "替换 Grok 凭据文件失败: {}",
                    self.credentials_path.display()
                )
            });
        }
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
    entry
        .model_index
        .as_ref()
        .is_none_or(|index| match model_id {
            Some(model_id) => {
                index.supports(model_id, reasoning_effort, backend, requires_backend_search)
            }
            None => backend.is_none_or(|backend| index.supports_backend(backend)),
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
    use axum::{Json, Router, extract::State, routing::post};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct RefreshGate {
        arrived: Arc<Notify>,
        release: Arc<Notify>,
    }

    async fn gated_refresh(State(gate): State<RefreshGate>) -> Json<serde_json::Value> {
        gate.arrived.notify_one();
        gate.release.notified().await;
        Json(json!({
            "access_token": "refreshed-access",
            "refresh_token": "refreshed-refresh",
            "token_type": "Bearer",
            "expires_in": 7200,
            "user_id": "refreshed-user"
        }))
    }

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
    fn peek_and_find_do_not_advance_round_robin_cursor() {
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
        let before = manager.snapshot().current_id;
        let first = manager
            .peek_next_credential_name(None, None, None, false, None)
            .unwrap();
        let second = manager
            .peek_next_credential_name(None, None, None, false, None)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(manager.snapshot().current_id, before);
        let routing = manager
            .find_routing_credential_id(None, None, None, false, None)
            .unwrap();
        assert_eq!(manager.snapshot().current_id, before);
        // choose/acquire 才会推进游标
        let _ = manager.choose_credential_id(None, None, None, false, None);
        assert_ne!(manager.snapshot().current_id, before);
        assert_eq!(
            manager.credential_display_name(routing).as_deref(),
            Some(first.as_str())
        );
    }

    #[tokio::test]
    async fn claim_routing_advances_and_pins_same_id_for_convert_and_send() {
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
        let claimed_a = manager
            .claim_routing_credential_id(None, None, None, false, None)
            .unwrap();
        let claimed_b = manager
            .claim_routing_credential_id(None, None, None, false, None)
            .unwrap();
        assert_ne!(claimed_a, claimed_b);
        // pin 路径：acquire_context_for 使用 claim 得到的 id，不重新 round-robin。
        assert_eq!(
            manager.acquire_context_for(claimed_a).await.unwrap().id,
            claimed_a
        );
    }

    #[test]
    fn session_affinity_is_stable_and_verified_preference_wins() {
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

        let affinity_a = manager
            .claim_routing_credential_id_with_affinity(
                None,
                Some("session-stable"),
                None,
                None,
                None,
                false,
                None,
            )
            .unwrap();
        let affinity_b = manager
            .claim_routing_credential_id_with_affinity(
                None,
                Some("session-stable"),
                None,
                None,
                None,
                false,
                None,
            )
            .unwrap();
        assert_eq!(affinity_a, affinity_b);

        let preferred = if affinity_a == 1 { 2 } else { 1 };
        assert_eq!(
            manager
                .claim_routing_credential_id_with_affinity(
                    Some(preferred),
                    Some("session-stable"),
                    None,
                    None,
                    None,
                    false,
                    None,
                )
                .unwrap(),
            preferred
        );
        manager.set_disabled(preferred, true).unwrap();
        assert_ne!(
            manager
                .claim_routing_credential_id_with_affinity(
                    Some(preferred),
                    Some("session-stable"),
                    None,
                    None,
                    None,
                    false,
                    None,
                )
                .unwrap(),
            preferred
        );
    }

    #[test]
    fn routing_candidates_prefer_session_account_without_duplicates_and_honor_required() {
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
            GrokCredentials {
                id: Some(3),
                access_token: Some("token-three".to_string()),
                ..Default::default()
            },
        ]);
        let candidates = manager
            .routing_candidate_ids(Some(2), None, None, None, None, false, None)
            .unwrap();
        assert_eq!(candidates[0], 2);
        assert_eq!(candidates.len(), 3);
        let unique = candidates
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), candidates.len());
        assert_eq!(
            manager
                .routing_candidate_ids(None, Some(1), None, None, None, false, None)
                .unwrap(),
            vec![1]
        );
    }

    #[test]
    fn concurrent_mutations_persist_the_latest_complete_snapshot() {
        let credentials_path = temp_path("grok-concurrent-persist");
        let credentials = (1..=8)
            .map(|id| GrokCredentials {
                id: Some(id),
                access_token: Some(format!("token-{id}")),
                name: Some(format!("initial-{id}")),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let manager = Arc::new(
            GrokTokenManager::new(
                Config::default(),
                credentials,
                None,
                credentials_path.clone(),
            )
            .unwrap(),
        );
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let handles = (1..=8)
            .map(|id| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    manager.set_name(id, format!("final-{id}")).unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        let persisted: Vec<GrokCredentials> =
            serde_json::from_str(&std::fs::read_to_string(&credentials_path).unwrap()).unwrap();
        for id in 1..=8 {
            let credential = persisted
                .iter()
                .find(|credential| credential.id == Some(id))
                .unwrap();
            let expected_name = format!("final-{id}");
            assert_eq!(credential.name.as_deref(), Some(expected_name.as_str()));
        }
        let _ = std::fs::remove_file(&credentials_path);
        let _ = std::fs::remove_file(credentials_path.with_extension("reasoning.key"));
    }

    #[tokio::test]
    async fn oauth_refresh_merges_into_live_admin_state() {
        let gate = RefreshGate {
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_gate = gate.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/token", post(gated_refresh))
                    .with_state(server_gate),
            )
            .await
            .unwrap();
        });

        let credentials_path = temp_path("grok-refresh-merge");
        let manager = Arc::new(
            GrokTokenManager::new(
                Config::default(),
                vec![GrokCredentials {
                    id: Some(1),
                    name: Some("before-refresh".to_string()),
                    access_token: Some("stale-access".to_string()),
                    refresh_token: Some("stale-refresh".to_string()),
                    token_endpoint: Some(format!("http://{address}/token")),
                    pools: Some(vec!["old-pool".to_string()]),
                    priority: 1,
                    ..Default::default()
                }],
                None,
                credentials_path.clone(),
            )
            .unwrap(),
        );
        let refresh_manager = manager.clone();
        let refresh = tokio::spawn(async move { refresh_manager.force_refresh_token_for(1).await });

        gate.arrived.notified().await;
        manager
            .set_name(1, "edited-during-refresh".to_string())
            .unwrap();
        manager.set_pools(1, vec!["new-pool".to_string()]).unwrap();
        manager.set_priority(1, 42).unwrap();
        manager.set_disabled(1, true).unwrap();
        gate.release.notify_one();

        let refreshed = refresh.await.unwrap().unwrap();
        assert_eq!(refreshed.access_token.as_deref(), Some("refreshed-access"));
        assert_eq!(
            refreshed.refresh_token.as_deref(),
            Some("refreshed-refresh")
        );
        assert_eq!(refreshed.name.as_deref(), Some("edited-during-refresh"));
        assert_eq!(refreshed.pools, Some(vec!["new-pool".to_string()]));
        assert_eq!(refreshed.priority, 42);
        assert!(refreshed.disabled);

        let persisted: Vec<GrokCredentials> =
            serde_json::from_str(&std::fs::read_to_string(&credentials_path).unwrap()).unwrap();
        assert_eq!(
            persisted[0].access_token.as_deref(),
            Some("refreshed-access")
        );
        assert_eq!(persisted[0].name.as_deref(), Some("edited-during-refresh"));
        assert_eq!(persisted[0].pools, Some(vec!["new-pool".to_string()]));
        assert_eq!(persisted[0].priority, 42);
        assert!(persisted[0].disabled);
        assert_eq!(
            manager.snapshot().entries[0].disabled_reason.as_deref(),
            Some("manual")
        );

        server.abort();
        let _ = std::fs::remove_file(&credentials_path);
        let _ = std::fs::remove_file(credentials_path.with_extension("reasoning.key"));
    }

    #[test]
    fn set_disabled_manual_reason_is_recorded() {
        let manager = manager(vec![GrokCredentials {
            id: Some(1),
            access_token: Some("token-one".to_string()),
            ..Default::default()
        }]);
        manager.set_disabled(1, true).unwrap();
        let snap = manager.snapshot();
        assert!(snap.entries[0].disabled);
        assert_eq!(
            snap.entries[0].disabled_reason.as_deref(),
            Some("manual")
        );
    }

    #[test]
    fn load_balancing_mode_persists_when_config_path_set() {
        let config_path = temp_path("grok-lb-config");
        let mut config = Config::default();
        // 写入初始配置文件并绑定 path。
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        config = Config::load(&config_path).unwrap();
        let manager = GrokTokenManager::new(
            config,
            vec![GrokCredentials {
                id: Some(1),
                access_token: Some("token-one".to_string()),
                ..Default::default()
            }],
            None,
            temp_path("grok-creds-lb"),
        )
        .unwrap();
        manager
            .set_load_balancing_mode("priority".to_string())
            .unwrap();
        let reloaded = Config::load(&config_path).unwrap();
        assert_eq!(reloaded.load_balancing_mode, "priority");
        let _ = std::fs::remove_file(&config_path);
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
    async fn backend_only_routing_rejects_chat_only_catalog_for_file_uploads() {
        let manager = manager(vec![
            GrokCredentials {
                id: Some(1),
                access_token: Some("chat-token".to_string()),
                ..Default::default()
            },
            GrokCredentials {
                id: Some(2),
                access_token: Some("responses-token".to_string()),
                ..Default::default()
            },
        ]);
        manager
            .set_model_catalog(
                1,
                GrokModelCatalog::from_upstream(
                    &json!({"data":[{
                        "model":"grok-chat-only",
                        "apiBackend":"chat_completions"
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
                        "model":"grok-responses",
                        "apiBackend":"responses"
                    }]}),
                    "https://api.x.ai/v1",
                ),
            )
            .unwrap();

        let context = manager
            .acquire_context(None, None, Some(GrokApiBackend::Responses), false, None)
            .await
            .unwrap();
        assert_eq!(context.id, 2);
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
