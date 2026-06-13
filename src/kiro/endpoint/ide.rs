//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的端点：
//! - API: `https://runtime.{api_region}.kiro.dev/generateAssistantResponse`
//! - MCP: `https://runtime.{api_region}.kiro.dev/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("runtime.{}.kiro.dev", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://runtime.{}.kiro.dev/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://runtime.{}.kiro.dev/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if !ctx.credentials.is_api_key_credential() {
            if let Some(ref arn) = ctx.credentials.profile_arn {
                req = req.header("x-amzn-kiro-profile-arn", arn);
            }
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        // 1. 注入 profileArn（API Key 凭据不携带）
        let body = if ctx.credentials.is_api_key_credential() {
            body.to_string()
        } else {
            inject_profile_arn(body, &ctx.credentials.profile_arn)
        };
        // 2. 按选中凭据的真实 schema 收紧 thinking/effort（per-credential clamp）
        match &ctx.catalog {
            Some(catalog) => clamp_additional_fields_in_body(body, catalog),
            None => body,
        }
    }
}

/// 按指定凭据 catalog 的 schema 收紧请求体根部的 `additionalModelRequestFields`。
///
/// merged 目录是各凭据并集（超集），故按单凭据 schema 只会收紧、不会丢意图：
/// - enum 越界值回退到 schema default；
/// - 该凭据对此模型不支持的扩展字段被移除（整体移除该键）。
/// 解析失败或无该字段时原样返回。
fn clamp_additional_fields_in_body(
    body: String,
    catalog: &crate::kiro::model::model_catalog::KiroModelCatalog,
) -> String {
    let mut json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            // 请求体由本服务构建，理论上必为合法 JSON；解析失败属异常，记 warn 并原样发送
            tracing::warn!("clamp: 请求体 JSON 解析失败，跳过 per-credential 收紧: {}", e);
            return body;
        }
    };
    // 无扩展字段 => 无需收紧
    if json.get("additionalModelRequestFields").is_none() {
        return body;
    }
    let mapped_id = json
        .get("conversationState")
        .and_then(|c| c.get("currentMessage"))
        .and_then(|m| m.get("userInputMessage"))
        .and_then(|u| u.get("modelId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let Some(mapped_id) = mapped_id else {
        tracing::debug!("clamp: 请求体含 additionalModelRequestFields 但缺少 modelId，跳过收紧");
        return body;
    };

    let existing = json.get("additionalModelRequestFields").cloned().unwrap_or(serde_json::Value::Null);
    match crate::anthropic::converter::clamp_additional_fields(&existing, catalog, &mapped_id) {
        Some(clamped) => {
            json["additionalModelRequestFields"] = clamped;
        }
        None => {
            if let Some(obj) = json.as_object_mut() {
                obj.remove("additionalModelRequestFields");
            }
        }
    }
    serde_json::to_string(&json).unwrap_or(body)
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Some(arn) = profile_arn {
        if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body) {
            json["profileArn"] = serde_json::Value::String(arn.clone());
            if let Ok(body) = serde_json::to_string(&json) {
                return body;
            }
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::{clamp_additional_fields_in_body, inject_profile_arn};
    use serde_json::Value;

    fn catalog_opus46_high_only() -> crate::kiro::model::model_catalog::KiroModelCatalog {
        use crate::kiro::model::model_catalog::{KiroModel, KiroModelCatalog};
        let schema = serde_json::json!({
            "properties": {
                "thinking": {"properties": {"type": {"enum": ["adaptive"]}}},
                "output_config": {"properties": {"effort": {"enum": ["high"], "default": "high"}}}
            }
        });
        KiroModelCatalog {
            default_model: None,
            models: vec![KiroModel {
                model_id: "claude-opus-4.6".to_string(),
                model_name: "Claude Opus 4.6".to_string(),
                description: None,
                rate_multiplier: None,
                rate_unit: None,
                supported_input_types: None,
                token_limits: None,
                prompt_caching: None,
                additional_model_request_fields_schema: Some(schema),
            }],
        }
    }

    #[test]
    fn test_clamp_in_body_clamps_effort_per_credential() {
        // body：modelId=opus-4.6，effort=max（来自并集）。该凭据 schema 仅允许 high。
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-opus-4.6"}}},"additionalModelRequestFields":{"thinking":{"type":"adaptive"},"output_config":{"effort":"max"}}}"#;
        let out = clamp_additional_fields_in_body(body.to_string(), &catalog_opus46_high_only());
        let json: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(json["additionalModelRequestFields"]["output_config"]["effort"], "high");
        // conversationState 保持不变
        assert_eq!(json["conversationState"]["currentMessage"]["userInputMessage"]["modelId"], "claude-opus-4.6");
    }

    #[test]
    fn test_clamp_in_body_noop_without_fields() {
        // 无 additionalModelRequestFields → 原样返回
        let body = r#"{"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-opus-4.6"}}}}"#;
        let out = clamp_additional_fields_in_body(body.to_string(), &catalog_opus46_high_only());
        let json: Value = serde_json::from_str(&out).unwrap();
        assert!(json.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, &None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, &arn);
        assert_eq!(result, "not-valid-json");
    }
}
