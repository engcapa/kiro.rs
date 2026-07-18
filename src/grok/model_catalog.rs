//! Grok Build / xAI `/v1/models` 凭据级模型目录。
//!
//! Grok Build 不把模型清单视为静态常量：OAuth session 与 API token 看到的
//! 模型、推理 effort 菜单、以及所使用的 API backend 都可能不同。本模块保留
//! 上游目录中的这些能力信息，供路由选择和请求转换共同使用。

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `/v1/models` 不可用时用于启动兼容的最小清单。
///
/// 这不是授权来源；一旦任意凭据的真实目录加载成功，HTTP 模型列表和路由都会
/// 优先使用真实目录。Composer 的完整 model id 保留在这里，使旧的 API token
/// 部署在目录尚未加载时仍可显式请求它。
pub const BOOTSTRAP_GROK_BUILD_MODELS: &[&str] = &[
    "grok-4.5",
    "grok-composer-2.5-fast",
    "grok-build-0.1",
    "grok-4.3",
    "grok-4.20-0309-reasoning",
    "grok-4.20-0309-non-reasoning",
    "grok-4.20-multi-agent-0309",
    "grok-4",
    "grok-4-fast",
    "grok-3-mini",
    "grok-3-mini-fast",
    "grok-3",
];

/// Grok Build 从模型目录读取并按模型分派的上游协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrokApiBackend {
    ChatCompletions,
    Responses,
    Messages,
}

impl Default for GrokApiBackend {
    /// 与 Grok Build 的 `ApiBackend::default()` 保持一致。公开 OpenAI 格式
    /// `/models` 响应通常不带 `apiBackend`，Grok Build 会将它们视为 Chat
    /// Completions 模型。
    fn default() -> Self {
        Self::ChatCompletions
    }
}

impl GrokApiBackend {
    pub fn from_wire(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat_completions" | "chat-completions" | "chat/completions" => {
                Some(Self::ChatCompletions)
            }
            "responses" => Some(Self::Responses),
            "messages" => Some(Self::Messages),
            _ => None,
        }
    }

    pub fn endpoint_path(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat/completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
        }
    }
}

/// Grok Build 的 reasoning effort 规范值。
///
/// `max` 是 Anthropic Messages API 与部分 TUI 配置中的别名，线上 Responses
/// / Chat Completions 请求应使用 `xhigh`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "max" => Some(Self::Xhigh),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Anthropic Messages upstream 的对应 wire value。该后端不支持把
    /// `none` / `minimal` 作为 `output_config.effort` 发送。
    pub fn to_messages_api(self) -> Option<&'static str> {
        match self {
            Self::None | Self::Minimal => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("max"),
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 服务端可为一个模型给出带展示文案的 effort 菜单。`id` 可不同于实际 wire
/// value，例如 UI 的 “Deep” 选项可以映射到 `xhigh`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokReasoningEffortOption {
    pub id: String,
    pub value: ReasoningEffort,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

/// 单个凭据可见的模型及能力。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokModel {
    /// 服务端展示/配置中的 id；实际请求始终使用 `model_id`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model_id: String,
    pub model_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub api_backend: GrokApiBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<i32>,
    #[serde(default = "default_true")]
    pub supported_in_api: bool,
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Grok Build 仅在目录明确声明该能力时，才将 Responses hosted
    /// Web Search 注入该模型的请求。这个字段也参与每凭据路由，避免合并
    /// catalog 后把搜索请求负载到不支持它的账号。
    #[serde(default)]
    pub supports_backend_search: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub reasoning_efforts: Vec<GrokReasoningEffortOption>,
}

fn default_true() -> bool {
    true
}

impl GrokModel {
    /// 用服务端菜单解析输入。先接受 canonical wire 值，再接受菜单 id，避免
    /// 把 “Deep” 一类 UI id 错当成未知 effort。
    pub fn resolve_effort(&self, value: &str) -> Option<ReasoningEffort> {
        ReasoningEffort::parse(value).or_else(|| {
            self.reasoning_efforts
                .iter()
                .find(|option| option.id.eq_ignore_ascii_case(value.trim()))
                .map(|option| option.value)
        })
    }

    /// 与 Grok Build `model_offers_reasoning_effort` 的策略一致：服务端提供
    /// 菜单时严格使用菜单；未提供菜单但声明 support 时回退 legacy 的四档。
    pub fn supports_effort(&self, effort: ReasoningEffort) -> bool {
        if !self.supports_reasoning_effort {
            return false;
        }
        if self.reasoning_efforts.is_empty() {
            return matches!(
                effort,
                ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::Xhigh
            );
        }
        self.reasoning_efforts
            .iter()
            .any(|option| option.value == effort)
    }

    pub fn default_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort.or_else(|| {
            self.reasoning_efforts
                .iter()
                .find(|option| option.default)
                .map(|option| option.value)
        })
    }

    fn aliases(&self) -> Vec<String> {
        let mut aliases = vec![self.model_id.clone(), self.model_name.clone()];
        if let Some(id) = &self.id {
            aliases.push(id.clone());
        }
        let model = self.model_id.trim();
        if let Some(without_prefix) = model.strip_prefix("grok-") {
            aliases.push(without_prefix.to_string());
        }
        // `composer2.5` 是人类常用的简写；只有唯一命中时 catalog resolver
        // 才会采用这个别名，避免 fast / non-fast 同时存在时错误猜测。
        if let Some(without_fast) = model.strip_suffix("-fast") {
            aliases.push(without_fast.to_string());
            if let Some(without_prefix) = without_fast.strip_prefix("grok-") {
                aliases.push(without_prefix.to_string());
            }
        }
        aliases
    }
}

/// 单凭据或合并后的模型目录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrokModelCatalog {
    #[serde(default)]
    pub models: Vec<GrokModel>,
}

impl GrokModelCatalog {
    /// 将 Grok Build 所读取的 OpenAI-compatible `/v1/models` 响应解析为
    /// 本地能力目录。字段同时兼容 camelCase、snake_case 以及 `_meta` 回退。
    pub fn from_upstream(value: &Value, default_base_url: &str) -> Self {
        let entries = value
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())
            .cloned()
            .unwrap_or_default();
        let models = entries
            .iter()
            .filter_map(|entry| GrokModel::from_upstream(entry, default_base_url))
            .collect();
        Self { models }
    }

    /// 静态 bootstrap 仅用于尚未取得真实目录时的兼容性，不表示凭据一定有权
    /// 使用其中的模型。
    pub fn bootstrap() -> Self {
        Self {
            models: BOOTSTRAP_GROK_BUILD_MODELS
                .iter()
                .map(|model_id| GrokModel {
                    id: Some((*model_id).to_string()),
                    model_id: (*model_id).to_string(),
                    model_name: (*model_id).to_string(),
                    description: Some("Grok Build bootstrap model".to_string()),
                    api_backend: GrokApiBackend::Responses,
                    base_url: None,
                    context_window: Some(131_072),
                    max_completion_tokens: Some(if *model_id == "grok-4.5" {
                        32_768
                    } else {
                        16_384
                    }),
                    supported_in_api: true,
                    supports_reasoning_effort: true,
                    // bootstrap 用于真实 catalog 还未加载的兼容窗口；保持
                    // `/grok` 既有 Responses Web Search 可用性。真实 catalog
                    // 到位后会严格以 supportsBackendSearch 为准。
                    supports_backend_search: true,
                    reasoning_effort: None,
                    // 缺少服务端菜单时 Grok Build 使用 legacy 的
                    // low/medium/high/xhigh fallback，而不是把 xhigh 压缩。
                    reasoning_efforts: Vec::new(),
                })
                .collect(),
        }
    }

    pub fn model_by_id(&self, id: &str) -> Option<&GrokModel> {
        self.models
            .iter()
            .find(|model| model.supported_in_api && model.model_id.eq_ignore_ascii_case(id.trim()))
    }

    /// 返回唯一匹配的规范 model id。显示名/配置 id/简写若发生歧义则拒绝猜测。
    pub fn resolve_model_id(&self, requested: &str) -> Option<String> {
        if let Some(model) = self.model_by_id(requested) {
            return Some(model.model_id.clone());
        }
        let key = normalize_model_key(requested);
        if key.is_empty() {
            return None;
        }
        let mut matches = self
            .models
            .iter()
            .filter(|model| model.supported_in_api)
            .filter(|model| {
                model
                    .aliases()
                    .iter()
                    .any(|alias| normalize_model_key(alias) == key)
            });
        let first = matches.next()?;
        matches.next().is_none().then(|| first.model_id.clone())
    }
}

impl GrokModel {
    fn from_upstream(value: &Value, default_base_url: &str) -> Option<Self> {
        let object = value.as_object()?;
        let meta = object.get("_meta").and_then(Value::as_object);
        let model_id = string_from(object, meta, &["model", "modelId"])
            .or_else(|| string_from(object, meta, &["id"]))?;
        let id = string_from(object, meta, &["id"]);
        let model_name = string_from(object, meta, &["name"]).unwrap_or_else(|| model_id.clone());
        let api_backend = string_from(object, meta, &["apiBackend", "api_backend"])
            .as_deref()
            .and_then(GrokApiBackend::from_wire)
            .unwrap_or_default();
        let base_url = string_from(object, meta, &["baseUrl", "base_url"]).or_else(|| {
            (!default_base_url.trim().is_empty()).then(|| default_base_url.to_string())
        });
        let reasoning_effort = string_from(object, meta, &["reasoningEffort", "reasoning_effort"])
            .as_deref()
            .and_then(ReasoningEffort::parse);
        let reasoning_efforts: Vec<GrokReasoningEffortOption> =
            value_from(object, meta, &["reasoningEfforts", "reasoning_efforts"])
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(parse_effort_option).collect())
                .unwrap_or_default();
        let supports_reasoning_effort = value_from(
            object,
            meta,
            &["supportsReasoningEffort", "supports_reasoning_effort"],
        )
        .and_then(Value::as_bool)
        .unwrap_or(false)
            || reasoning_effort.is_some()
            || !reasoning_efforts.is_empty();

        Some(Self {
            id,
            model_id,
            model_name,
            description: string_from(object, meta, &["description"]),
            api_backend,
            base_url,
            context_window: integer_from(
                object,
                meta,
                &["contextWindow", "context_window", "totalContextTokens"],
            ),
            max_completion_tokens: integer_from(
                object,
                meta,
                &["maxCompletionTokens", "max_completion_tokens"],
            ),
            supported_in_api: value_from(object, meta, &["supportedInApi", "supported_in_api"])
                .and_then(Value::as_bool)
                .unwrap_or(true),
            supports_reasoning_effort,
            supports_backend_search: value_from(
                object,
                meta,
                &["supportsBackendSearch", "supports_backend_search"],
            )
            .and_then(Value::as_bool)
            .unwrap_or(false),
            reasoning_effort,
            reasoning_efforts,
        })
    }
}

/// 为每个凭据预计算的 O(1) 路由能力索引。
#[derive(Debug, Clone, Default)]
pub struct GrokCredentialModelIndex {
    models: HashMap<String, GrokModelCapability>,
}

#[derive(Debug, Clone)]
struct GrokModelCapability {
    backend: GrokApiBackend,
    supports_reasoning_effort: bool,
    supports_backend_search: bool,
    efforts: HashSet<ReasoningEffort>,
    uses_legacy_effort_menu: bool,
}

impl GrokCredentialModelIndex {
    pub fn from_catalog(catalog: &GrokModelCatalog) -> Self {
        let models = catalog
            .models
            .iter()
            .filter(|model| model.supported_in_api)
            .map(|model| {
                (
                    model.model_id.to_ascii_lowercase(),
                    GrokModelCapability {
                        backend: model.api_backend,
                        supports_reasoning_effort: model.supports_reasoning_effort,
                        supports_backend_search: model.supports_backend_search,
                        efforts: model
                            .reasoning_efforts
                            .iter()
                            .map(|option| option.value)
                            .collect(),
                        uses_legacy_effort_menu: model.reasoning_efforts.is_empty(),
                    },
                )
            })
            .collect();
        Self { models }
    }

    pub fn supports(
        &self,
        model_id: &str,
        effort: Option<ReasoningEffort>,
        backend: Option<GrokApiBackend>,
        requires_backend_search: bool,
    ) -> bool {
        let Some(model) = self.models.get(&model_id.to_ascii_lowercase()) else {
            return false;
        };
        if backend.is_some_and(|backend| model.backend != backend) {
            return false;
        }
        if requires_backend_search && !model.supports_backend_search {
            return false;
        }
        let Some(effort) = effort else {
            return true;
        };
        if !model.supports_reasoning_effort {
            return false;
        }
        if model.uses_legacy_effort_menu {
            matches!(
                effort,
                ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::Xhigh
            )
        } else {
            model.efforts.contains(&effort)
        }
    }
}

/// 将各凭据目录合成 handler 侧的只读“并集”视图。真正发送时仍会根据单凭据
/// index 过滤，避免把并集里存在的模型误投到没有授权的凭据。
pub fn merge_catalogs(catalogs: &[GrokModelCatalog]) -> GrokModelCatalog {
    let mut order = Vec::new();
    let mut models: HashMap<String, GrokModel> = HashMap::new();
    for catalog in catalogs {
        for model in &catalog.models {
            let key = model.model_id.to_ascii_lowercase();
            match models.get_mut(&key) {
                None => {
                    order.push(key.clone());
                    models.insert(key, model.clone());
                }
                Some(existing) => merge_model(existing, model),
            }
        }
    }
    GrokModelCatalog {
        models: order
            .into_iter()
            .filter_map(|id| models.remove(&id))
            .collect(),
    }
}

fn merge_model(existing: &mut GrokModel, incoming: &GrokModel) {
    existing.supported_in_api |= incoming.supported_in_api;
    existing.supports_reasoning_effort |= incoming.supports_reasoning_effort;
    existing.supports_backend_search |= incoming.supports_backend_search;
    if existing.description.is_none() {
        existing.description = incoming.description.clone();
    }
    if existing.base_url.is_none() {
        existing.base_url = incoming.base_url.clone();
    }
    existing.context_window = max_option(existing.context_window, incoming.context_window);
    existing.max_completion_tokens = max_option(
        existing.max_completion_tokens,
        incoming.max_completion_tokens,
    );
    if existing.reasoning_effort.is_none() {
        existing.reasoning_effort = incoming.reasoning_effort;
    }
    for option in &incoming.reasoning_efforts {
        if !existing
            .reasoning_efforts
            .iter()
            .any(|current| current.value == option.value && current.id == option.id)
        {
            existing.reasoning_efforts.push(option.clone());
        }
    }
}

fn max_option(left: Option<i32>, right: Option<i32>) -> Option<i32> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, right) => right,
    }
}

fn effort_option(value: ReasoningEffort) -> GrokReasoningEffortOption {
    let id = value.as_str().to_string();
    let mut chars = id.chars();
    let label = chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default();
    GrokReasoningEffortOption {
        id,
        value,
        label,
        description: None,
        default: false,
    }
}

fn parse_effort_option(value: &Value) -> Option<GrokReasoningEffortOption> {
    match value {
        Value::String(value) => {
            let effort = ReasoningEffort::parse(value)?;
            Some(effort_option(effort))
        }
        Value::Object(object) => {
            let effort = object
                .get("value")
                .and_then(Value::as_str)
                .and_then(ReasoningEffort::parse)?;
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(effort.as_str())
                .to_string();
            let label = object
                .get("label")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| humanize(&id));
            Some(GrokReasoningEffortOption {
                id,
                value: effort,
                label,
                description: object
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                default: object
                    .get("default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        }
        _ => None,
    }
}

fn humanize(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn string_from(
    object: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    keys: &[&str],
) -> Option<String> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .chain(
            meta.into_iter()
                .flat_map(|meta| keys.iter().filter_map(move |key| meta.get(*key))),
        )
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn integer_from(
    object: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    keys: &[&str],
) -> Option<i32> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .chain(
            meta.into_iter()
                .flat_map(|meta| keys.iter().filter_map(move |key| meta.get(*key))),
        )
        .filter_map(Value::as_i64)
        .next()
        .and_then(|value| i32::try_from(value).ok())
}

fn value_from<'a>(
    object: &'a Map<String, Value>,
    meta: Option<&'a Map<String, Value>>,
    keys: &[&str],
) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .or_else(|| meta.and_then(|meta| keys.iter().find_map(|key| meta.get(*key))))
}

fn normalize_model_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_grok_build_model_capabilities_and_aliases() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({
                "data": [{
                    "id": "composer-display",
                    "model": "grok-composer-2.5-fast",
                    "name": "Composer 2.5",
                    "baseUrl": "https://cli-chat-proxy.grok.com/v1",
                    "apiBackend": "responses",
                    "contextWindow": 256000,
                    "supportsBackendSearch": true,
                    "supportsReasoningEffort": true,
                    "reasoningEfforts": ["low", {"id":"deep","value":"xhigh","label":"Deep"}]
                }]
            }),
            "https://api.x.ai/v1",
        );
        let model = catalog.model_by_id("grok-composer-2.5-fast").unwrap();
        assert_eq!(model.api_backend, GrokApiBackend::Responses);
        assert!(model.supports_backend_search);
        assert!(model.supports_effort(ReasoningEffort::Xhigh));
        assert_eq!(model.resolve_effort("deep"), Some(ReasoningEffort::Xhigh));
        assert_eq!(
            catalog.resolve_model_id("composer2.5").as_deref(),
            Some("grok-composer-2.5-fast")
        );
    }

    #[test]
    fn catalog_index_filters_model_backend_and_effort() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({
                "data": [{
                    "model": "grok-4.5",
                    "apiBackend": "responses",
                    "supportsBackendSearch": true,
                    "supportsReasoningEffort": true,
                    "reasoningEfforts": ["low", "medium", "high"]
                }]
            }),
            "https://api.x.ai/v1",
        );
        let index = GrokCredentialModelIndex::from_catalog(&catalog);
        assert!(index.supports(
            "grok-4.5",
            Some(ReasoningEffort::High),
            Some(GrokApiBackend::Responses),
            true,
        ));
        assert!(!index.supports(
            "grok-4.5",
            Some(ReasoningEffort::Xhigh),
            Some(GrokApiBackend::Responses),
            true,
        ));
        assert!(!index.supports(
            "grok-4.5",
            None,
            Some(GrokApiBackend::ChatCompletions),
            true,
        ));
    }

    #[test]
    fn source_default_backend_is_chat_completions() {
        let catalog = GrokModelCatalog::from_upstream(
            &json!({"data":[{"id":"grok-4.5"}]}),
            "https://api.x.ai/v1",
        );
        assert_eq!(
            catalog.models[0].api_backend,
            GrokApiBackend::ChatCompletions
        );
    }
}
