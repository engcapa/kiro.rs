use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, RwLock};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    pub max_input_tokens: Option<i32>,
    pub max_output_tokens: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCaching {
    pub maximum_cache_checkpoints_per_request: Option<i32>,
    pub minimum_tokens_per_cache_checkpoint: Option<i32>,
    pub supports_prompt_caching: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroModel {
    pub model_id: String,
    pub model_name: String,
    pub description: Option<String>,
    pub rate_multiplier: Option<f64>,
    pub rate_unit: Option<String>,
    pub supported_input_types: Option<Vec<String>>,
    pub token_limits: Option<TokenLimits>,
    pub prompt_caching: Option<PromptCaching>,
    pub additional_model_request_fields_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroDefaultModel {
    pub model_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroModelCatalog {
    pub default_model: Option<KiroDefaultModel>,
    pub models: Vec<KiroModel>,
}

pub static GLOBAL_MODEL_CATALOG: LazyLock<RwLock<Option<KiroModelCatalog>>> =
    LazyLock::new(|| RwLock::new(None));

pub static LAST_CATALOG_REFRESH: LazyLock<RwLock<Option<std::time::Instant>>> =
    LazyLock::new(|| RwLock::new(None));

/// 判断某模型的 schema 是否声明支持 thinking
pub fn model_supports_thinking(model: &KiroModel) -> bool {
    model
        .additional_model_request_fields_schema
        .as_ref()
        .and_then(|s| s.get("properties"))
        .and_then(|p| p.as_object())
        .map(|p| p.contains_key("thinking"))
        .unwrap_or(false)
}

/// 单凭据的模型支持索引（刷新时预计算，供选择热路径 O(1) 查询）
///
/// `model_ids`：该凭据 catalog 内所有 model_id。
/// `thinking_ids`：其中 schema 声明支持 thinking 的 model_id 子集。
#[derive(Debug, Clone, Default)]
pub struct CredentialModelIndex {
    pub model_ids: HashSet<String>,
    pub thinking_ids: HashSet<String>,
}

impl CredentialModelIndex {
    pub fn from_catalog(catalog: &KiroModelCatalog) -> Self {
        let mut model_ids = HashSet::with_capacity(catalog.models.len());
        let mut thinking_ids = HashSet::new();
        for m in &catalog.models {
            model_ids.insert(m.model_id.clone());
            if model_supports_thinking(m) {
                thinking_ids.insert(m.model_id.clone());
            }
        }
        Self { model_ids, thinking_ids }
    }

    /// 该凭据是否拥有此 model_id（精确匹配；mapped_id 已是规范 id）
    pub fn supports(&self, model_id: &str) -> bool {
        model_id == "auto" || self.model_ids.contains(model_id)
    }

    /// 该凭据是否对此 model_id 支持 thinking
    pub fn supports_thinking(&self, model_id: &str) -> bool {
        model_id == "auto" || self.thinking_ids.contains(model_id)
    }
}

fn max_opt(a: Option<i32>, b: Option<i32>) -> Option<i32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    }
}

/// 将多个凭据各自的 catalog 合并为并集视图（merged）。
///
/// - 按 `model_id` 去重，保留首次出现顺序。
/// - 重复 model_id 时：若已存的 schema 缺失而新条目有 schema，则升级为新 schema；
///   token_limits 取各字段较大值。从而 merged 是各凭据的超集（superset），
///   供 handler 阶段 `map_model` / `get_additional_model_request_fields` 使用；
///   服务期再按单凭据 schema 收紧（clamp）。
/// - 实践中同一 model_id 的 schema 在各凭据间通常一致，本合并仅为防御性兜底。
pub fn merge_catalogs(catalogs: &[KiroModelCatalog]) -> KiroModelCatalog {
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, KiroModel> = HashMap::new();
    let mut default_model: Option<KiroDefaultModel> = None;

    for cat in catalogs {
        if default_model.is_none() {
            default_model = cat.default_model.clone();
        }
        for m in &cat.models {
            match by_id.get_mut(&m.model_id) {
                None => {
                    order.push(m.model_id.clone());
                    by_id.insert(m.model_id.clone(), m.clone());
                }
                Some(existing) => {
                    if existing.additional_model_request_fields_schema.is_none()
                        && m.additional_model_request_fields_schema.is_some()
                    {
                        existing.additional_model_request_fields_schema =
                            m.additional_model_request_fields_schema.clone();
                    }
                    if let Some(ml) = &m.token_limits {
                        let e = existing.token_limits.get_or_insert(TokenLimits {
                            max_input_tokens: None,
                            max_output_tokens: None,
                        });
                        e.max_input_tokens = max_opt(e.max_input_tokens, ml.max_input_tokens);
                        e.max_output_tokens = max_opt(e.max_output_tokens, ml.max_output_tokens);
                    }
                }
            }
        }
    }

    let models = order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect();
    KiroModelCatalog { default_model, models }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, thinking: bool, max_in: i32) -> KiroModel {
        KiroModel {
            model_id: id.to_string(),
            model_name: id.to_string(),
            description: None,
            rate_multiplier: None,
            rate_unit: None,
            supported_input_types: None,
            token_limits: Some(TokenLimits {
                max_input_tokens: Some(max_in),
                max_output_tokens: Some(64000),
            }),
            prompt_caching: None,
            additional_model_request_fields_schema: if thinking {
                Some(serde_json::json!({"properties": {"thinking": {}, "output_config": {}}}))
            } else {
                None
            },
        }
    }

    fn catalog(models: Vec<KiroModel>) -> KiroModelCatalog {
        KiroModelCatalog { default_model: None, models }
    }

    #[test]
    fn test_model_supports_thinking() {
        assert!(model_supports_thinking(&model("a", true, 1)));
        assert!(!model_supports_thinking(&model("b", false, 1)));
    }

    #[test]
    fn test_credential_model_index() {
        let cat = catalog(vec![
            model("auto", false, 1000),
            model("claude-opus-4.8", true, 1000),
            model("claude-sonnet-4.5", false, 1000),
        ]);
        let ix = CredentialModelIndex::from_catalog(&cat);
        assert!(ix.supports("claude-opus-4.8"));
        assert!(ix.supports("claude-sonnet-4.5"));
        assert!(!ix.supports("claude-opus-4.6"));
        assert!(ix.supports_thinking("claude-opus-4.8"));
        assert!(!ix.supports_thinking("claude-sonnet-4.5"));
    }

    #[test]
    fn test_merge_catalogs_union_and_schema_upgrade() {
        // #1 has the model WITHOUT schema; #2 has it WITH thinking schema → merged keeps schema.
        // Distinct ids are unioned; token limits take the max.
        let c1 = catalog(vec![model("claude-opus-4.6", false, 200_000), model("claude-sonnet-4.5", false, 200_000)]);
        let c2 = catalog(vec![model("claude-opus-4.6", true, 1_000_000), model("claude-opus-4.8", true, 1_000_000)]);
        let merged = merge_catalogs(&[c1, c2]);

        let ids: Vec<&str> = merged.models.iter().map(|m| m.model_id.as_str()).collect();
        assert!(ids.contains(&"claude-opus-4.6"));
        assert!(ids.contains(&"claude-sonnet-4.5"));
        assert!(ids.contains(&"claude-opus-4.8"));
        assert_eq!(merged.models.len(), 3); // opus-4.6 deduped

        let opus46 = merged.models.iter().find(|m| m.model_id == "claude-opus-4.6").unwrap();
        // schema upgraded from #2
        assert!(model_supports_thinking(opus46));
        // token limit took the max (1M from #2)
        assert_eq!(opus46.token_limits.as_ref().unwrap().max_input_tokens, Some(1_000_000));
    }
}
