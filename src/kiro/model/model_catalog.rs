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
