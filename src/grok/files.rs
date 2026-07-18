//! Anthropic Files API 与 xAI Files API 之间的绑定。
//!
//! 文件字节始终由 xAI 的 `/v1/files` 保存，代理只持久化
//! `file_id -> 创建凭据` 的小型注册表。这样 `source.type = "file"` 在
//! 多 Grok 凭据轮询时仍能固定回创建文件的账号，而不必把私有文件重新下载
//! 成 data URL。

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::anthropic::types::Message;

/// Anthropic 当前 Files API 所使用的 beta 标识。代理不会拒绝未携带该头的
/// 调用，以便兼容没有暴露 beta 选项的客户端；标准 Anthropic SDK 会自动发送。
pub const FILES_API_BETA: &str = "files-api-2025-04-14";

/// xAI Files API 当前单文件上限为 50 MiB。路由的总 body 上限会额外为
/// multipart boundary 和表单字段预留空间。
pub const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

const DEFAULT_REGISTRY_PATH: &str = "grok_file_bindings.json";
const REGISTRY_VERSION: u32 = 1;

/// 对外保持 Anthropic `FileMetadata` 字段形状。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMetadata {
    pub id: String,
    #[serde(rename = "type", default = "file_object_type")]
    pub object_type: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: String,
    #[serde(default)]
    pub downloadable: bool,
}

impl FileMetadata {
    /// xAI 的 Files API 与 Anthropic 的字段名并不完全一致（例如 OpenAI
    /// 兼容返回常用 `bytes`），在边界统一成 Anthropic 的 metadata 形状。
    pub fn from_xai(
        value: &Value,
        fallback_filename: &str,
        fallback_mime_type: &str,
        fallback_size_bytes: usize,
    ) -> Result<Self, FileStoreError> {
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                FileStoreError::UpstreamResponse("xAI Files API 响应缺少 file id".to_string())
            })?
            .to_string();
        let filename = first_string(value, &["filename", "name"])
            .unwrap_or_else(|| fallback_filename.to_string());
        let mime_type = first_string(value, &["mime_type", "content_type", "mimeType"])
            .unwrap_or_else(|| fallback_mime_type.to_string());
        let size_bytes = first_u64(value, &["size_bytes", "bytes", "size"])
            .unwrap_or(fallback_size_bytes as u64);
        let created_at = timestamp_from_value(value.get("created_at")).unwrap_or_else(now_rfc3339);

        Ok(Self {
            id,
            object_type: file_object_type(),
            filename,
            mime_type,
            size_bytes,
            created_at,
            // Anthropic 约定：调用方上传的文件不可通过 Files content endpoint
            // 下载；代理没有把模型生成物注册为 Files，因此始终为 false。
            downloadable: false,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileListResponse {
    pub data: Vec<FileMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
}

/// `GET /v1/files` 支持的 Anthropic 分页参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FileListQuery {
    pub after_id: Option<String>,
    pub before_id: Option<String>,
    pub limit: Option<usize>,
    pub scope_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileBinding {
    metadata: FileMetadata,
    credential_id: u64,
    /// 仅保留创建时审计/向后兼容信息。授权必须查询 credential 当前 pools，
    /// 否则管理员移出资源池后旧绑定仍会永久可见。
    pools: Vec<String>,
}

#[derive(Debug, Default)]
struct RegistryState {
    loaded: bool,
    bindings: HashMap<String, FileBinding>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedRegistry {
    #[serde(default = "registry_version")]
    version: u32,
    #[serde(default)]
    files: Vec<FileBinding>,
}

#[derive(Debug)]
struct GrokFileStoreInner {
    path: PathBuf,
    state: Mutex<RegistryState>,
}

/// xAI 文件的本地绑定注册表。
///
/// 文件本体不落地；只有绑定注册表需要跨重启保存。可用
/// `GROK_FILE_BINDINGS_PATH` 改变其位置。
#[derive(Debug, Clone)]
pub struct GrokFileStore {
    inner: Arc<GrokFileStoreInner>,
}

impl Default for GrokFileStore {
    fn default() -> Self {
        Self::new(default_registry_path())
    }
}

impl GrokFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(GrokFileStoreInner {
                path: path.into(),
                state: Mutex::new(RegistryState::default()),
            }),
        }
    }

    pub fn registry_path(&self) -> &Path {
        &self.inner.path
    }

    /// 注册一个刚由 xAI `/files` 成功创建的文件，并固定它所用的凭据。
    pub fn register(
        &self,
        metadata: FileMetadata,
        credential_id: u64,
        pools: Vec<String>,
    ) -> Result<(), FileStoreError> {
        let pools = normalize_pools(pools);
        if pools.is_empty() {
            return Err(FileStoreError::Registry(
                "创建 xAI 文件的凭据没有可访问资源池".to_string(),
            ));
        }
        let id = non_empty_id(&metadata.id)?;
        let binding = FileBinding {
            metadata,
            credential_id,
            pools,
        };

        let mut state = self.inner.state.lock();
        self.ensure_loaded(&mut state)?;
        if let Some(existing) = state.bindings.get(&id) {
            if existing.credential_id != binding.credential_id {
                return Err(FileStoreError::Registry(format!(
                    "file_id {id} 已绑定到其他 Grok 凭据"
                )));
            }
        }
        let previous = state.bindings.insert(id.clone(), binding);
        if let Err(error) = self.persist(&state) {
            match previous {
                Some(previous) => {
                    state.bindings.insert(id, previous);
                }
                None => {
                    state.bindings.remove(&id);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    /// 返回指定文件绑定的上游凭据。访问控制查询凭据**当前** pools；注册表
    /// 中的创建时快照只作审计。不存在、凭据已删除或无权访问均返回 not
    /// found，避免暴露其他租户的 file id。
    pub fn binding_for(
        &self,
        file_id: &str,
        allowed_pools: &[String],
        current_pools: &dyn Fn(u64) -> Option<Vec<String>>,
    ) -> Result<FileBindingInfo, FileStoreError> {
        let file_id = non_empty_id(file_id)?;
        let mut state = self.inner.state.lock();
        self.ensure_loaded(&mut state)?;
        let binding = state
            .bindings
            .get(&file_id)
            .cloned()
            .ok_or_else(|| FileStoreError::NotFound(file_id.clone()))?;
        if !current_pools(binding.credential_id)
            .is_some_and(|pools| pools_overlap(&pools, allowed_pools))
        {
            return Err(FileStoreError::NotFound(file_id));
        }
        Ok(FileBindingInfo {
            metadata: binding.metadata,
            credential_id: binding.credential_id,
        })
    }

    pub fn metadata_for(
        &self,
        file_id: &str,
        allowed_pools: &[String],
        current_pools: &dyn Fn(u64) -> Option<Vec<String>>,
    ) -> Result<FileMetadata, FileStoreError> {
        Ok(self
            .binding_for(file_id, allowed_pools, current_pools)?
            .metadata)
    }

    /// 从本代理上传过的、当前 API Key 可访问的文件生成分页列表。无法可靠地
    /// 把多个 xAI 账号的整个远端文件空间合并成一个 Anthropic workspace，故
    /// 不枚举绕过本代理直接上传的文件。
    pub fn list(
        &self,
        query: &FileListQuery,
        allowed_pools: &[String],
        current_pools: &dyn Fn(u64) -> Option<Vec<String>>,
    ) -> Result<FileListResponse, FileStoreError> {
        if query
            .scope_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(FileStoreError::UnsupportedScope);
        }
        if query.after_id.is_some() && query.before_id.is_some() {
            return Err(FileStoreError::InvalidRequest(
                "after_id 与 before_id 不能同时使用".to_string(),
            ));
        }
        let limit = query.limit.unwrap_or(20);
        if !(1..=1000).contains(&limit) {
            return Err(FileStoreError::InvalidRequest(
                "limit 必须在 1 到 1000 之间".to_string(),
            ));
        }

        let mut state = self.inner.state.lock();
        self.ensure_loaded(&mut state)?;
        let mut files = state
            .bindings
            .values()
            .filter(|binding| {
                current_pools(binding.credential_id)
                    .is_some_and(|pools| pools_overlap(&pools, allowed_pools))
            })
            .map(|binding| binding.metadata.clone())
            .collect::<Vec<_>>();
        // RFC3339 字符串可按字典序排序；同一时刻的文件再用 id 打破平局。
        files.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });

        if let Some(after_id) = query
            .after_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            let position = files
                .iter()
                .position(|file| file.id == after_id)
                .ok_or_else(|| FileStoreError::NotFound(after_id.to_string()))?;
            files = files.split_off(position + 1);
        } else if let Some(before_id) = query
            .before_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            let position = files
                .iter()
                .position(|file| file.id == before_id)
                .ok_or_else(|| FileStoreError::NotFound(before_id.to_string()))?;
            files.truncate(position);
        }

        let has_more = files.len() > limit;
        files.truncate(limit);
        let first_id = files.first().map(|file| file.id.clone());
        let last_id = files.last().map(|file| file.id.clone());
        Ok(FileListResponse {
            data: files,
            first_id,
            has_more,
            last_id,
        })
    }

    /// 在上游文件已成功删除后移除本地绑定。
    pub fn remove(&self, file_id: &str, credential_id: u64) -> Result<(), FileStoreError> {
        let file_id = non_empty_id(file_id)?;
        let mut state = self.inner.state.lock();
        self.ensure_loaded(&mut state)?;
        let binding = state
            .bindings
            .get(&file_id)
            .cloned()
            .ok_or_else(|| FileStoreError::NotFound(file_id.clone()))?;
        if binding.credential_id != credential_id {
            return Err(FileStoreError::NotFound(file_id));
        }
        state.bindings.remove(&file_id);
        if let Err(error) = self.persist(&state) {
            state.bindings.insert(file_id, binding);
            return Err(error);
        }
        Ok(())
    }

    /// 收集 Messages 中 `source.type = file` 的绑定凭据。一个 xAI 请求不能
    /// 混用多个账号私有文件，因此多个 file_id 必须属于同一 credential。
    pub fn credential_for_messages(
        &self,
        messages: &[Message],
        allowed_pools: &[String],
        current_pools: &dyn Fn(u64) -> Option<Vec<String>>,
    ) -> Result<Option<u64>, FileStoreError> {
        let mut credential_id = None;
        for message in messages {
            let Some(blocks) = message.content.as_array() else {
                continue;
            };
            for block in blocks {
                let block_type = block.get("type").and_then(Value::as_str);
                let source = block.get("source").and_then(Value::as_object);
                let is_file_source = source
                    .and_then(|source| source.get("type"))
                    .and_then(Value::as_str)
                    .is_some_and(|source_type| source_type == "file");
                if !is_file_source {
                    if block_type == Some("container_upload") && block.get("file_id").is_some() {
                        return Err(FileStoreError::UnsupportedContentBlock(
                            "container_upload 依赖 Anthropic code execution，/grok 尚未提供该工具"
                                .to_string(),
                        ));
                    }
                    continue;
                }
                match block_type {
                    Some("image") | Some("document") => {}
                    Some(other) => {
                        return Err(FileStoreError::UnsupportedContentBlock(format!(
                            "source.type=file 只支持 image 或 document content block，收到 {other}"
                        )));
                    }
                    None => {
                        return Err(FileStoreError::InvalidRequest(
                            "file source 缺少 content block type".to_string(),
                        ));
                    }
                }
                let file_id = source
                    .and_then(|source| source.get("file_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        FileStoreError::InvalidRequest(
                            "source.type=file 必须提供非空 file_id".to_string(),
                        )
                    })?;
                let binding = self.binding_for(file_id, allowed_pools, current_pools)?;
                match credential_id {
                    Some(expected) if expected != binding.credential_id => {
                        return Err(FileStoreError::MixedCredentialFiles);
                    }
                    Some(_) => {}
                    None => credential_id = Some(binding.credential_id),
                }
            }
        }
        Ok(credential_id)
    }

    fn ensure_loaded(&self, state: &mut RegistryState) -> Result<(), FileStoreError> {
        if state.loaded {
            return Ok(());
        }
        if let Some(parent) = self
            .inner
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(FileStoreError::storage)?;
        }
        if self.inner.path.exists() {
            let raw = fs::read_to_string(&self.inner.path).map_err(FileStoreError::storage)?;
            if !raw.trim().is_empty() {
                let persisted: PersistedRegistry =
                    serde_json::from_str(&raw).map_err(FileStoreError::storage)?;
                if persisted.version != REGISTRY_VERSION {
                    return Err(FileStoreError::Registry(format!(
                        "不支持的 Grok 文件绑定注册表版本 {}",
                        persisted.version
                    )));
                }
                state.bindings = persisted
                    .files
                    .into_iter()
                    .map(|binding| (binding.metadata.id.clone(), binding))
                    .collect();
            }
        }
        state.loaded = true;
        Ok(())
    }

    fn persist(&self, state: &RegistryState) -> Result<(), FileStoreError> {
        if let Some(parent) = self
            .inner
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(FileStoreError::storage)?;
        }
        let mut files = state.bindings.values().cloned().collect::<Vec<_>>();
        files.sort_by(|left, right| left.metadata.id.cmp(&right.metadata.id));
        let raw = serde_json::to_vec_pretty(&PersistedRegistry {
            version: REGISTRY_VERSION,
            files,
        })
        .map_err(FileStoreError::storage)?;
        let temporary_path = self
            .inner
            .path
            .with_extension(format!("json.tmp.{}", Uuid::new_v4().simple()));
        fs::write(&temporary_path, raw).map_err(FileStoreError::storage)?;
        fs::rename(&temporary_path, &self.inner.path).map_err(|error| {
            let _ = fs::remove_file(&temporary_path);
            FileStoreError::storage(error)
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileBindingInfo {
    pub metadata: FileMetadata,
    pub credential_id: u64,
}

#[derive(Debug)]
pub enum FileStoreError {
    NotFound(String),
    InvalidRequest(String),
    UnsupportedContentBlock(String),
    UnsupportedScope,
    MixedCredentialFiles,
    UpstreamResponse(String),
    Registry(String),
}

impl FileStoreError {
    fn storage(error: impl fmt::Display) -> Self {
        Self::Registry(format!("Grok 文件绑定注册表读写失败: {error}"))
    }

    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

impl fmt::Display for FileStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(file_id) => write!(
                formatter,
                "file_id {file_id} 不存在、未通过 /grok/v1/files 上传，或当前 API Key 无权访问"
            ),
            Self::InvalidRequest(message)
            | Self::UnsupportedContentBlock(message)
            | Self::UpstreamResponse(message)
            | Self::Registry(message) => formatter.write_str(message),
            Self::UnsupportedScope => {
                formatter.write_str("/grok Files API 暂不支持 scope_id 过滤")
            }
            Self::MixedCredentialFiles => formatter.write_str(
                "同一 Messages 请求中的 file_id 必须由同一个 Grok 凭据创建；请分开请求或使用同一凭据重新上传",
            ),
        }
    }
}

impl std::error::Error for FileStoreError {}

fn default_registry_path() -> PathBuf {
    std::env::var_os("GROK_FILE_BINDINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REGISTRY_PATH))
}

fn registry_version() -> u32 {
    REGISTRY_VERSION
}

fn file_object_type() -> String {
    "file".to_string()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn timestamp_from_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.to_string()),
        Some(Value::Number(value)) => value
            .as_i64()
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .map(|value: DateTime<Utc>| value.to_rfc3339_opts(SecondsFormat::Millis, true)),
        _ => None,
    }
}

fn first_string(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn first_u64(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(Value::as_u64))
}

fn normalize_pools(pools: Vec<String>) -> Vec<String> {
    let mut normalized = pools
        .into_iter()
        .map(|pool| pool.trim().to_string())
        .filter(|pool| !pool.is_empty())
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn pools_overlap(left: &[String], right: &[String]) -> bool {
    left.iter()
        .any(|pool| right.iter().any(|allowed| pool == allowed))
}

fn non_empty_id(file_id: &str) -> Result<String, FileStoreError> {
    let file_id = file_id.trim();
    if file_id.is_empty() {
        return Err(FileStoreError::InvalidRequest(
            "file_id 不能为空".to_string(),
        ));
    }
    Ok(file_id.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn metadata(id: &str) -> FileMetadata {
        FileMetadata {
            id: id.to_string(),
            object_type: "file".to_string(),
            filename: "diagram.png".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 12,
            created_at: "2026-07-18T00:00:00.000Z".to_string(),
            downloadable: false,
        }
    }

    fn temp_registry_path() -> PathBuf {
        std::env::temp_dir().join(format!("grok-file-bindings-{}.json", Uuid::new_v4()))
    }

    #[test]
    fn stores_bindings_by_pool_and_persists_them() {
        let path = temp_registry_path();
        let store = GrokFileStore::new(&path);
        store
            .register(metadata("file_image"), 7, vec!["team-a".to_string()])
            .unwrap();
        let current_pools =
            |credential_id| (credential_id == 7).then(|| vec!["team-a".to_string()]);
        let binding = store
            .binding_for("file_image", &["team-a".to_string()], &current_pools)
            .unwrap();
        assert_eq!(binding.credential_id, 7);
        assert!(
            store
                .binding_for("file_image", &["team-b".to_string()], &current_pools)
                .unwrap_err()
                .is_not_found()
        );

        let reloaded = GrokFileStore::new(&path);
        assert_eq!(
            reloaded
                .metadata_for("file_image", &["team-a".to_string()], &current_pools)
                .unwrap()
                .filename,
            "diagram.png"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn resolves_all_message_file_ids_to_one_credential() {
        let path = temp_registry_path();
        let store = GrokFileStore::new(&path);
        store
            .register(metadata("file_image"), 7, vec!["default".to_string()])
            .unwrap();
        let messages = vec![Message {
            role: "user".to_string(),
            content: json!([
                {"type":"image","source":{"type":"file","file_id":"file_image"}},
                {"type":"document","source":{"type":"file","file_id":"file_image"}}
            ]),
        }];
        let current_pools =
            |credential_id| (credential_id == 7).then(|| vec!["default".to_string()]);
        assert_eq!(
            store
                .credential_for_messages(&messages, &["default".to_string()], &current_pools,)
                .unwrap(),
            Some(7)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_files_from_different_credentials_in_one_request() {
        let path = temp_registry_path();
        let store = GrokFileStore::new(&path);
        store
            .register(metadata("file_one"), 7, vec!["default".to_string()])
            .unwrap();
        store
            .register(metadata("file_two"), 8, vec!["default".to_string()])
            .unwrap();
        let messages = vec![Message {
            role: "user".to_string(),
            content: json!([
                {"type":"image","source":{"type":"file","file_id":"file_one"}},
                {"type":"image","source":{"type":"file","file_id":"file_two"}}
            ]),
        }];
        let current_pools =
            |credential_id| matches!(credential_id, 7 | 8).then(|| vec!["default".to_string()]);
        assert!(matches!(
            store.credential_for_messages(&messages, &["default".to_string()], &current_pools,),
            Err(FileStoreError::MixedCredentialFiles)
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn current_credential_pools_revoke_old_file_access_immediately() {
        let path = temp_registry_path();
        let store = GrokFileStore::new(&path);
        store
            .register(metadata("file_moved"), 7, vec!["team-a".to_string()])
            .unwrap();
        let pools = Arc::new(Mutex::new(vec!["team-a".to_string()]));
        let lookup_pools = pools.clone();
        let current_pools =
            move |credential_id| (credential_id == 7).then(|| lookup_pools.lock().clone());

        assert!(
            store
                .binding_for("file_moved", &["team-a".to_string()], &current_pools)
                .is_ok()
        );
        *pools.lock() = vec!["team-b".to_string()];
        assert!(
            store
                .binding_for("file_moved", &["team-a".to_string()], &current_pools)
                .unwrap_err()
                .is_not_found()
        );
        assert!(
            store
                .binding_for("file_moved", &["team-b".to_string()], &current_pools)
                .is_ok()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn normalizes_xai_file_metadata() {
        let metadata = FileMetadata::from_xai(
            &json!({
                "id":"file_xai",
                "filename":"report.pdf",
                "bytes":42,
                "created_at":1_700_000_000
            }),
            "upload.bin",
            "application/octet-stream",
            1,
        )
        .unwrap();
        assert_eq!(metadata.mime_type, "application/octet-stream");
        assert_eq!(metadata.size_bytes, 42);
        assert!(metadata.created_at.starts_with("2023-"));
    }
}
