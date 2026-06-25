//! API Key 配置模型
//!
//! 管理用于客户端认证的 API Key 列表。
//! 每个 API Key 关联一组凭据池，决定可以使用哪些凭据。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// 单个 API Key 条目
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyEntry {
    /// 唯一标识符（自增 ID）
    pub id: u64,

    /// 显示名称
    pub name: String,

    /// API Key 值（如 ksk_xxxxxxxx）
    pub key: String,

    /// 允许访问的凭据池列表
    #[serde(default = "default_pools")]
    pub pools: Vec<String>,

    /// 是否被禁用
    #[serde(default)]
    pub disabled: bool,

    /// 创建时间（RFC3339 格式）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

fn default_pools() -> Vec<String> {
    vec!["default".to_string()]
}

/// API Key 配置（包含所有 API Key 条目）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyConfig {
    /// API Key 条目列表
    #[serde(default)]
    pub keys: Vec<ApiKeyEntry>,
}

impl ApiKeyConfig {
    /// 从文件加载 API Key 配置
    ///
    /// - 如果文件不存在，返回空配置
    /// - 如果文件内容为空，返回空配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)?;

        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        let config = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// 保存 API Key 配置到文件
    pub fn save<P: AsRef<Path>>(&self, path: P) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    /// 通过 key 值查找 API Key 条目
    pub fn find_by_key(&self, key: &str) -> Option<&ApiKeyEntry> {
        self.keys.iter().find(|entry| entry.key == key)
    }

    /// 获取下一个可用的 ID
    pub fn next_id(&self) -> u64 {
        self.keys.iter().map(|e| e.id).max().unwrap_or(0) + 1
    }

    /// 生成一个新的 API Key 值
    ///
    /// 格式: ksk_<uuid-without-hyphens>
    pub fn generate_key() -> String {
        let uuid = uuid::Uuid::new_v4().to_string().replace('-', "");
        format!("ksk_{}", uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ApiKeyConfig::default();
        assert!(config.keys.is_empty());
    }

    #[test]
    fn test_generate_key_format() {
        let key = ApiKeyConfig::generate_key();
        assert!(key.starts_with("ksk_"));
        assert_eq!(key.len(), 4 + 32); // "ksk_" + 32 hex chars
    }

    #[test]
    fn test_next_id_empty() {
        let config = ApiKeyConfig::default();
        assert_eq!(config.next_id(), 1);
    }

    #[test]
    fn test_next_id_with_entries() {
        let config = ApiKeyConfig {
            keys: vec![
                ApiKeyEntry {
                    id: 1,
                    name: "test1".to_string(),
                    key: "ksk_abc".to_string(),
                    pools: vec!["default".to_string()],
                    disabled: false,
                    created_at: None,
                },
                ApiKeyEntry {
                    id: 5,
                    name: "test2".to_string(),
                    key: "ksk_def".to_string(),
                    pools: vec!["default".to_string()],
                    disabled: false,
                    created_at: None,
                },
            ],
        };
        assert_eq!(config.next_id(), 6);
    }

    #[test]
    fn test_find_by_key() {
        let config = ApiKeyConfig {
            keys: vec![ApiKeyEntry {
                id: 1,
                name: "test".to_string(),
                key: "ksk_abc123".to_string(),
                pools: vec!["default".to_string()],
                disabled: false,
                created_at: None,
            }],
        };

        assert!(config.find_by_key("ksk_abc123").is_some());
        assert!(config.find_by_key("ksk_nonexistent").is_none());
    }

    #[test]
    fn test_serialization_roundtrip() {
        let config = ApiKeyConfig {
            keys: vec![ApiKeyEntry {
                id: 1,
                name: "My Key".to_string(),
                key: "ksk_test123".to_string(),
                pools: vec!["default".to_string(), "premium".to_string()],
                disabled: false,
                created_at: Some("2024-01-01T00:00:00Z".to_string()),
            }],
        };

        let json = serde_json::to_string(&config).unwrap();
        let parsed: ApiKeyConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.keys.len(), 1);
        assert_eq!(parsed.keys[0].id, 1);
        assert_eq!(parsed.keys[0].name, "My Key");
        assert_eq!(parsed.keys[0].key, "ksk_test123");
        assert_eq!(parsed.keys[0].pools, vec!["default", "premium"]);
    }

    #[test]
    fn test_default_pools_deserialization() {
        let json = r#"{"id": 1, "name": "test", "key": "ksk_abc"}"#;
        let entry: ApiKeyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.pools, vec!["default"]);
    }
}
