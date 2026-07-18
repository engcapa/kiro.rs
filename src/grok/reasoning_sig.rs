//! Anthropic `thinking.signature` 中携带的 xAI Responses reasoning 回放包。
//!
//! Claude Code 会把上一轮 assistant 的 `thinking` 文本与 `signature` 原样带回。
//! 代理把该轮完整的 xAI `reasoning` items（含 `encrypted_content`）编码进
//! signature，下一轮再展开为 Responses `input` 中的 reasoning sibling，以逼近
//! Grok Build 的多轮 reasoning / prefix KV-cache 行为。
//!
//! Wire 形态：`xai-rs2.` + base64url(JSON) + `.` + base64url(HMAC-SHA256)。
//! HMAC 密钥从代理 API key 做域分离派生，使客户端不能篡改 credential/model/
//! encrypted content 后影响路由或上游回放。旧的未签名 `xai-rs1` 不再解析，
//! 会像真·Anthropic signature 一样安全降级成可见 thinking 文本。

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::Context;

use crate::anthropic::types::MessagesRequest;

/// 当前打包格式前缀与版本号。
pub const SIGNATURE_PREFIX: &str = "xai-rs2.";
pub const PACKAGE_VERSION: u32 = 2;
const MAX_SIGNATURE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REASONING_ITEMS: usize = 128;
const KEY_DOMAIN: &[u8] = b"kiro.rs/grok/reasoning-signature/v2\0";

type HmacSha256 = Hmac<Sha256>;

/// 服务端持有的 reasoning signature 编解码器。
#[derive(Clone)]
pub struct ReasoningSignatureCodec {
    key: [u8; 32],
}

impl std::fmt::Debug for ReasoningSignatureCodec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReasoningSignatureCodec")
            .finish_non_exhaustive()
    }
}

impl ReasoningSignatureCodec {
    pub fn new(server_secret: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(KEY_DOMAIN);
        hash.update(server_secret);
        Self {
            key: hash.finalize().into(),
        }
    }

    /// 读取或原子创建一个仅服务端持有的随机 key。不能复用客户端 API key：
    /// 调用方本来就知道 API key，使用它做 HMAC 无法阻止伪造 signature。
    pub fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(encoded) => return decode_key_material(&encoded),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("读取 Grok reasoning 签名密钥失败: {}", path.display())
                });
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("创建 Grok reasoning 签名密钥目录失败: {}", parent.display())
            })?;
        }
        let mut material = [0_u8; 32];
        getrandom::fill(&mut material).context("生成 Grok reasoning 签名密钥失败")?;
        let encoded = hex::encode(material);

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(mut file) => {
                file.write_all(encoded.as_bytes()).with_context(|| {
                    format!("写入 Grok reasoning 签名密钥失败: {}", path.display())
                })?;
                file.write_all(b"\n").with_context(|| {
                    format!("写入 Grok reasoning 签名密钥失败: {}", path.display())
                })?;
                file.sync_all().with_context(|| {
                    format!("同步 Grok reasoning 签名密钥失败: {}", path.display())
                })?;
                Ok(Self::new(&material))
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let encoded = std::fs::read_to_string(path).with_context(|| {
                    format!(
                        "读取并发创建的 Grok reasoning 签名密钥失败: {}",
                        path.display()
                    )
                })?;
                decode_key_material(&encoded)
            }
            Err(error) => Err(error)
                .with_context(|| format!("创建 Grok reasoning 签名密钥失败: {}", path.display())),
        }
    }

    /// 从完整 reasoning item 列表编码并签名。items 为空时返回 `None`。
    pub fn encode(
        &self,
        model: &str,
        credential_id: Option<u64>,
        items: &[Value],
    ) -> Option<String> {
        let items = items
            .iter()
            .filter_map(sanitize_reasoning_item_for_storage)
            .collect::<Vec<_>>();
        if items.is_empty()
            || items.len() > MAX_REASONING_ITEMS
            || items.iter().any(|item| {
                item.get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(|value| value.trim().is_empty())
            })
        {
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
        let payload = URL_SAFE_NO_PAD.encode(json);
        let mut mac = HmacSha256::new_from_slice(&self.key).ok()?;
        mac.update(SIGNATURE_PREFIX.as_bytes());
        mac.update(payload.as_bytes());
        let tag = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let signature = format!("{SIGNATURE_PREFIX}{payload}.{tag}");
        (signature.len() <= MAX_SIGNATURE_BYTES).then_some(signature)
    }

    /// 仅返回通过 HMAC、版本、大小和结构校验的包。
    pub fn decode(&self, signature: &str) -> Option<ReasoningSignaturePackage> {
        let signature = signature.trim();
        if signature.len() > MAX_SIGNATURE_BYTES {
            return None;
        }
        let signed = signature.strip_prefix(SIGNATURE_PREFIX)?;
        let (payload, encoded_tag) = signed.split_once('.')?;
        if payload.is_empty() || encoded_tag.is_empty() || encoded_tag.contains('.') {
            return None;
        }
        let tag = URL_SAFE_NO_PAD.decode(encoded_tag.as_bytes()).ok()?;
        let mut mac = HmacSha256::new_from_slice(&self.key).ok()?;
        mac.update(SIGNATURE_PREFIX.as_bytes());
        mac.update(payload.as_bytes());
        mac.verify_slice(&tag).ok()?;

        let bytes = URL_SAFE_NO_PAD.decode(payload.as_bytes()).ok()?;
        let package: ReasoningSignaturePackage = serde_json::from_slice(&bytes).ok()?;
        if package.v != PACKAGE_VERSION
            || package.backend != "responses"
            || package.items.is_empty()
            || package.items.len() > MAX_REASONING_ITEMS
            || package.items.iter().any(|item| {
                item.get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(|value| value.trim().is_empty())
            })
        {
            return None;
        }
        Some(package)
    }
}

fn decode_key_material(encoded: &str) -> anyhow::Result<ReasoningSignatureCodec> {
    let material = hex::decode(encoded.trim()).context("Grok reasoning 签名密钥不是有效 hex")?;
    if material.len() != 32 {
        anyhow::bail!("Grok reasoning 签名密钥长度无效，应为 32 bytes");
    }
    Ok(ReasoningSignatureCodec::new(&material))
}

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

/// 仅在签发时的 model/backend/credential 与当前真实路由完全一致时允许回放。
pub fn package_matches_route(
    package: &ReasoningSignaturePackage,
    replay_model: &str,
    replay_backend: &str,
    replay_credential_id: Option<u64>,
) -> bool {
    package.model.eq_ignore_ascii_case(replay_model)
        && package.backend == replay_backend
        && matches!(
            (package.credential_id, replay_credential_id),
            (Some(expected), Some(actual)) if expected == actual
        )
}

/// 将包展开为 Responses `input` 可接受的 reasoning items（去掉 `status`）。
pub fn package_to_input_items(package: &ReasoningSignaturePackage) -> Vec<Value> {
    package
        .items
        .iter()
        .filter_map(sanitize_reasoning_item_for_input)
        .collect()
}

/// 从最近的 assistant thinking 块提取一个已验证、且与本次模型一致的路由提示。
/// 返回值仍需由 token manager 按 disabled/pool/catalog 能力重新校验，不能绕过
/// 当前授权状态。
pub fn latest_verified_route_credential(
    codec: &ReasoningSignatureCodec,
    request: &MessagesRequest,
    expected_model: &str,
) -> Option<u64> {
    let signature = request
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == "assistant")
        .flat_map(|message| message.content.as_array().into_iter().flatten().rev())
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("thinking"))
        .filter_map(|block| block.get("signature").and_then(Value::as_str))
        .next()?;
    let package = codec.decode(signature)?;
    (package.backend == "responses" && package.model.eq_ignore_ascii_case(expected_model))
        .then_some(package.credential_id)
        .flatten()
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
        let codec = ReasoningSignatureCodec::new(b"server-secret");
        let encoded = codec
            .encode(
                "grok-4.5",
                Some(7),
                &[sample_item("rs_1", "enc1"), sample_item("tco_1", "enc2")],
            )
            .expect("encode");
        assert!(encoded.starts_with(SIGNATURE_PREFIX));

        let package = codec.decode(&encoded).expect("decode");
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
        let codec = ReasoningSignatureCodec::new(b"server-secret");
        let other = ReasoningSignatureCodec::new(b"other-secret");
        assert!(codec.decode("anthropic-real-signature").is_none());
        assert!(codec.decode("xai-rs1.legacy-unsigned").is_none());
        assert!(codec.decode("xai-rs2.!!!not-base64!!!.bad").is_none());
        assert!(codec.encode("grok-4.5", None, &[]).is_none());
        assert!(
            codec
                .encode(
                    "grok-4.5",
                    Some(7),
                    &[json!({"type":"reasoning","id":"id_only"})]
                )
                .is_none()
        );

        let encoded = codec
            .encode("grok-4.5", Some(7), &[sample_item("rs_1", "enc")])
            .unwrap();
        assert!(other.decode(&encoded).is_none());
        let mut tampered = encoded.into_bytes();
        let payload_byte = SIGNATURE_PREFIX.len() + 2;
        tampered[payload_byte] = if tampered[payload_byte] == b'A' {
            b'B'
        } else {
            b'A'
        };
        assert!(
            codec
                .decode(std::str::from_utf8(&tampered).unwrap())
                .is_none()
        );
    }

    #[test]
    fn route_mismatch_detection() {
        let package = ReasoningSignaturePackage {
            v: PACKAGE_VERSION,
            backend: "responses".to_string(),
            model: "grok-4.5".to_string(),
            credential_id: Some(1),
            items: vec![sample_item("rs_1", "enc")],
        };
        assert!(package_matches_route(
            &package,
            "grok-4.5",
            "responses",
            Some(1)
        ));
        assert!(!package_matches_route(
            &package,
            "grok-4.5",
            "responses",
            Some(2)
        ));
        assert!(!package_matches_route(
            &package,
            "grok-4.6",
            "responses",
            Some(1)
        ));
        assert!(!package_matches_route(
            &package,
            "grok-4.5",
            "chat_completions",
            Some(1)
        ));
    }

    #[test]
    fn extracts_only_verified_same_model_route_hint() {
        let codec = ReasoningSignatureCodec::new(b"server-secret");
        let signature = codec
            .encode("grok-4.5", Some(17), &[sample_item("rs_1", "enc")])
            .unwrap();
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "grok-4.5",
            "max_tokens": 64,
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "thinking",
                    "thinking": "plan",
                    "signature": signature
                }]
            }]
        }))
        .unwrap();

        assert_eq!(
            latest_verified_route_credential(&codec, &request, "grok-4.5"),
            Some(17)
        );
        assert_eq!(
            latest_verified_route_credential(&codec, &request, "grok-4.6"),
            None
        );
        assert_eq!(
            latest_verified_route_credential(
                &ReasoningSignatureCodec::new(b"different-secret"),
                &request,
                "grok-4.5"
            ),
            None
        );
    }

    #[test]
    fn persisted_server_key_survives_codec_reload() {
        let path = std::env::temp_dir().join(format!(
            "kiro-rs-grok-reasoning-key-{}",
            uuid::Uuid::new_v4()
        ));
        let first = ReasoningSignatureCodec::load_or_create(&path).unwrap();
        let signature = first
            .encode("grok-4.5", Some(9), &[sample_item("rs_1", "enc")])
            .unwrap();
        let reloaded = ReasoningSignatureCodec::load_or_create(&path).unwrap();
        assert_eq!(reloaded.decode(&signature).unwrap().credential_id, Some(9));
        let _ = std::fs::remove_file(path);
    }
}
