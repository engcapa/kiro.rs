//! Anthropic `thinking.signature` 中携带的 xAI Responses reasoning 回放包。
//!
//! Claude Code 会把上一轮 assistant 的 `thinking` 文本与 `signature` 原样带回。
//! 代理把该轮完整的 xAI `reasoning` items（含 `encrypted_content`）编码进
//! signature，下一轮再展开为 Responses `input` 中的 reasoning sibling，以逼近
//! Grok Build 的多轮 reasoning / prefix KV-cache 行为。
//!
//! Wire 形态：`xai-rs1.` + base64url(JSON)。非此前缀的 signature 不解析，
//! 以便与真·Anthropic signature 或损坏数据安全共存。

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 当前打包格式前缀与版本号。
pub const SIGNATURE_PREFIX: &str = "xai-rs1.";
pub const PACKAGE_VERSION: u32 = 1;

/// 写入 Anthropic `thinking.signature` 的完整包。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningSignaturePackage {
    pub v: u32,
    /// 签发时的 wire backend；目前仅 `responses` 会打包。
    pub backend: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_id: Option<u64>,
    /// 按上游 `output` 顺序保存的完整 reasoning item（含 encrypted_content）。
    pub items: Vec<Value>,
}

/// 从完整 reasoning item 列表编码 signature。items 为空时返回 `None`。
pub fn encode_signature(
    model: &str,
    credential_id: Option<u64>,
    items: &[Value],
) -> Option<String> {
    let items = items
        .iter()
        .filter_map(sanitize_reasoning_item_for_storage)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }
    let package = ReasoningSignaturePackage {
        v: PACKAGE_VERSION,
        backend: "responses".to_string(),
        model: model.to_string(),
        credential_id,
        items,
    };
    let json = serde_json::to_vec(&package).ok()?;
    Some(format!(
        "{SIGNATURE_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(json)
    ))
}

/// 解码 signature。无法识别的前缀、损坏 payload、错误版本均返回 `None`。
pub fn decode_signature(signature: &str) -> Option<ReasoningSignaturePackage> {
    let signature = signature.trim();
    let payload = signature.strip_prefix(SIGNATURE_PREFIX)?;
    if payload.is_empty() {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
    let package: ReasoningSignaturePackage = serde_json::from_slice(&bytes).ok()?;
    if package.v != PACKAGE_VERSION {
        return None;
    }
    if package.items.is_empty() {
        return None;
    }
    Some(package)
}

/// 若包内 `credential_id` 与当前路由凭据冲突则返回 `false`（应回退文本策略）。
pub fn package_matches_credential(
    package: &ReasoningSignaturePackage,
    replay_credential_id: Option<u64>,
) -> bool {
    match (package.credential_id, replay_credential_id) {
        (Some(expected), Some(actual)) => expected == actual,
        // 包未记录凭据或当前未知：允许尝试回放，由上游决定是否 400。
        (None, _) | (_, None) => true,
    }
}

/// 将包展开为 Responses `input` 可接受的 reasoning items（去掉 `status`）。
pub fn package_to_input_items(package: &ReasoningSignaturePackage) -> Vec<Value> {
    package
        .items
        .iter()
        .filter_map(sanitize_reasoning_item_for_input)
        .collect()
}

/// 从任意上游 JSON 中提取可保存的 reasoning item（要求 type=reasoning）。
pub fn extract_reasoning_item(item: &Value) -> Option<Value> {
    if item.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    sanitize_reasoning_item_for_storage(item)
}

fn sanitize_reasoning_item_for_storage(item: &Value) -> Option<Value> {
    let object = item.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        // 允许无 type 但有 id+encrypted 的残缺对象；补上 type。
        if object.get("encrypted_content").is_none() && object.get("id").is_none() {
            return None;
        }
    }
    let mut out = json!({ "type": "reasoning" });
    let target = out.as_object_mut()?;
    if let Some(id) = object
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        target.insert("id".to_string(), Value::String(id.to_string()));
    }
    if let Some(encrypted) = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        target.insert(
            "encrypted_content".to_string(),
            Value::String(encrypted.to_string()),
        );
    }
    if let Some(summary) = object.get("summary") {
        target.insert("summary".to_string(), summary.clone());
    }
    if let Some(content) = object.get("content") {
        target.insert("content".to_string(), content.clone());
    }
    // 至少要有 id 或 encrypted_content，否则回放无意义。
    if !target.contains_key("id") && !target.contains_key("encrypted_content") {
        return None;
    }
    Some(out)
}

fn sanitize_reasoning_item_for_input(item: &Value) -> Option<Value> {
    let mut item = sanitize_reasoning_item_for_storage(item)?;
    if let Some(object) = item.as_object_mut() {
        // status 为 output-only；Grok Build 在回放前会剥掉。
        object.remove("status");
    }
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(id: &str, enc: &str) -> Value {
        json!({
            "type": "reasoning",
            "id": id,
            "status": "completed",
            "summary": [{"type": "summary_text", "text": "plan"}],
            "encrypted_content": enc,
        })
    }

    #[test]
    fn round_trips_full_reasoning_items_and_strips_status_on_input() {
        let encoded = encode_signature(
            "grok-4.5",
            Some(7),
            &[sample_item("rs_1", "enc1"), sample_item("tco_1", "enc2")],
        )
        .expect("encode");
        assert!(encoded.starts_with(SIGNATURE_PREFIX));

        let package = decode_signature(&encoded).expect("decode");
        assert_eq!(package.v, PACKAGE_VERSION);
        assert_eq!(package.model, "grok-4.5");
        assert_eq!(package.credential_id, Some(7));
        assert_eq!(package.items.len(), 2);

        let input = package_to_input_items(&package);
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["id"], "rs_1");
        assert_eq!(input[0]["encrypted_content"], "enc1");
        assert!(input[0].get("status").is_none());
        assert_eq!(input[0]["summary"][0]["text"], "plan");
        assert_eq!(input[1]["id"], "tco_1");
    }

    #[test]
    fn rejects_foreign_or_corrupt_signatures() {
        assert!(decode_signature("anthropic-real-signature").is_none());
        assert!(decode_signature("xai-rs1.!!!not-base64!!!").is_none());
        assert!(decode_signature("xai-rs1.").is_none());
        assert!(encode_signature("grok-4.5", None, &[]).is_none());
    }

    #[test]
    fn credential_mismatch_detection() {
        let package = ReasoningSignaturePackage {
            v: PACKAGE_VERSION,
            backend: "responses".to_string(),
            model: "grok-4.5".to_string(),
            credential_id: Some(1),
            items: vec![sample_item("rs_1", "enc")],
        };
        assert!(package_matches_credential(&package, Some(1)));
        assert!(!package_matches_credential(&package, Some(2)));
        assert!(package_matches_credential(&package, None));
    }
}
