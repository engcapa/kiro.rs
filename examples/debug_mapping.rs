use kiro_rs::anthropic::converter::map_model;
use kiro_rs::kiro::model::model_catalog::{GLOBAL_MODEL_CATALOG, KiroModelCatalog, KiroModel, TokenLimits};

fn main() {
    let catalog = KiroModelCatalog {
        default_model: None,
        models: vec![
            KiroModel {
                model_id: "claude-opus-4.8".to_string(),
                model_name: "Claude Opus 4.8".to_string(),
                description: None,
                rate_multiplier: None,
                rate_unit: None,
                supported_input_types: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(64000),
                }),
                prompt_caching: None,
                additional_model_request_fields_schema: None,
            },
        ],
    };

    {
        let mut guard = GLOBAL_MODEL_CATALOG.write().unwrap();
        *guard = Some(catalog);
    }

    println!("Mapping claude-opus-4.8: {:?}", map_model("claude-opus-4.8"));
    println!("Mapping CLAUDE OPUS 4.8: {:?}", map_model("CLAUDE OPUS 4.8"));
}
