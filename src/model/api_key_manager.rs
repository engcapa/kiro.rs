//! API Key 管理器
//!
//! 提供线程安全的 API Key 管理功能，包括查找、增删改查和持久化。

use std::path::PathBuf;

use parking_lot::Mutex;

use super::api_key::{ApiKeyConfig, ApiKeyEntry};

/// 线程安全的 API Key 管理器
pub struct ApiKeyManager {
    config: Mutex<ApiKeyConfig>,
    path: PathBuf,
}

impl ApiKeyManager {
    /// 从文件加载并创建管理器
    pub fn new(path: PathBuf) -> anyhow::Result<Self> {
        let config = ApiKeyConfig::load(&path)?;
        validate_config(&config)?;
        Ok(Self {
            config: Mutex::new(config),
            path,
        })
    }

    /// 根据 key 值查找允许的凭据池列表
    ///
    /// 如果 key 存在且未被禁用，返回 Some(pools)；否则返回 None
    pub fn find_allowed_pools(&self, key: &str) -> Option<Vec<String>> {
        let config = self.config.lock();
        config
            .find_by_key(key)
            .filter(|entry| !entry.disabled)
            .map(|entry| entry.pools.clone())
    }

    /// 列出所有 API Key 条目
    pub fn list(&self) -> Vec<ApiKeyEntry> {
        let config = self.config.lock();
        config.keys.clone()
    }

    /// 添加新的 API Key
    pub fn add(
        &self,
        name: String,
        key: Option<String>,
        pools: Option<Vec<String>>,
        disabled: bool,
    ) -> anyhow::Result<ApiKeyEntry> {
        let mut config = self.config.lock();

        let actual_key = match key {
            Some(key) if !key.trim().is_empty() => key.trim().to_string(),
            Some(_) => anyhow::bail!("API Key 不能为空"),
            None => ApiKeyConfig::generate_key(),
        };
        if config.keys.iter().any(|entry| entry.key == actual_key) {
            anyhow::bail!("API Key 已存在");
        }
        let actual_pools = normalize_pools(pools);

        let entry = ApiKeyEntry {
            id: config.next_id(),
            name,
            key: actual_key,
            pools: actual_pools,
            disabled,
            created_at: Some(chrono::Utc::now().to_rfc3339()),
        };

        config.keys.push(entry.clone());
        self.save_locked(&config)?;

        Ok(entry)
    }

    /// 更新已有的 API Key
    pub fn update(
        &self,
        id: u64,
        name: Option<String>,
        pools: Option<Vec<String>>,
        disabled: Option<bool>,
    ) -> Result<ApiKeyEntry, String> {
        let mut config = self.config.lock();

        let entry = config
            .keys
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("API Key #{} 不存在", id))?;

        if let Some(name) = name {
            entry.name = name;
        }
        if let Some(pools) = pools {
            entry.pools = normalize_pools(Some(pools));
        }
        if let Some(disabled) = disabled {
            entry.disabled = disabled;
        }

        let updated = entry.clone();
        self.save_locked(&config)
            .map_err(|e| format!("保存失败: {}", e))?;

        Ok(updated)
    }

    /// 删除 API Key
    pub fn delete(&self, id: u64) -> Result<(), String> {
        let mut config = self.config.lock();

        let pos = config
            .keys
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| format!("API Key #{} 不存在", id))?;

        config.keys.remove(pos);
        self.save_locked(&config)
            .map_err(|e| format!("保存失败: {}", e))?;

        Ok(())
    }

    /// 收集所有池名称（从 API Key 和凭据池中合并去重）
    pub fn all_pool_names(&self, credential_pools: &[String]) -> Vec<String> {
        let config = self.config.lock();
        let mut names: Vec<String> = Vec::new();

        // 从 API Key 条目收集
        for entry in &config.keys {
            for pool in &entry.pools {
                if !names.contains(pool) {
                    names.push(pool.clone());
                }
            }
        }

        // 从凭据池收集
        for pool in credential_pools {
            if !names.contains(pool) {
                names.push(pool.clone());
            }
        }

        names.sort();
        names
    }

    /// 在持有锁的情况下保存配置到文件
    fn save_locked(&self, config: &ApiKeyConfig) -> anyhow::Result<()> {
        config.save(&self.path)
    }
}

fn normalize_pools(pools: Option<Vec<String>>) -> Vec<String> {
    let normalized: Vec<String> = pools
        .unwrap_or_default()
        .into_iter()
        .map(|pool| pool.trim().to_string())
        .filter(|pool| !pool.is_empty())
        .fold(Vec::new(), |mut acc, pool| {
            if !acc.contains(&pool) {
                acc.push(pool);
            }
            acc
        });

    if normalized.is_empty() {
        vec!["default".to_string()]
    } else {
        normalized
    }
}

fn validate_config(config: &ApiKeyConfig) -> anyhow::Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for entry in &config.keys {
        let key = entry.key.trim();
        if key.is_empty() {
            anyhow::bail!("API Key #{} 不能为空", entry.id);
        }
        if seen.iter().any(|seen_key| *seen_key == key) {
            anyhow::bail!("API Key 已存在: {}", key);
        }
        seen.push(key);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_temp_manager() -> (ApiKeyManager, PathBuf) {
        let path = std::env::temp_dir().join(format!("api_key_test_{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, "{}").unwrap();
        let manager = ApiKeyManager::new(path.clone()).unwrap();
        (manager, path)
    }

    #[test]
    fn test_add_and_list() {
        let (manager, _path) = create_temp_manager();

        let entry = manager
            .add("Test Key".to_string(), None, None, false)
            .unwrap();
        assert_eq!(entry.id, 1);
        assert_eq!(entry.name, "Test Key");
        assert!(entry.key.starts_with("ksk_"));
        assert_eq!(entry.pools, vec!["default"]);

        let list = manager.list();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_add_rejects_duplicate_key() {
        let (manager, _path) = create_temp_manager();

        manager
            .add(
                "Test Key".to_string(),
                Some("ksk_duplicate".to_string()),
                None,
                false,
            )
            .unwrap();

        let err = manager
            .add(
                "Another Key".to_string(),
                Some("ksk_duplicate".to_string()),
                None,
                false,
            )
            .err()
            .unwrap()
            .to_string();

        assert!(err.contains("API Key 已存在"));
    }

    #[test]
    fn test_load_rejects_duplicate_keys() {
        let path =
            std::env::temp_dir().join(format!("api_key_duplicate_{}.json", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"keys":[{"id":1,"name":"one","key":"ksk_duplicate"},{"id":2,"name":"two","key":"ksk_duplicate"}]}"#,
        )
        .unwrap();

        let err = ApiKeyManager::new(path.clone()).err().unwrap().to_string();

        assert!(err.contains("API Key 已存在"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_add_rejects_empty_custom_key() {
        let (manager, _path) = create_temp_manager();

        let err = manager
            .add("Test Key".to_string(), Some("   ".to_string()), None, false)
            .err()
            .unwrap()
            .to_string();

        assert!(err.contains("API Key 不能为空"));
    }

    #[test]
    fn test_normalizes_pools() {
        let (manager, _path) = create_temp_manager();

        let entry = manager
            .add(
                "Test Key".to_string(),
                None,
                Some(vec![
                    " pro ".to_string(),
                    "".to_string(),
                    "default".to_string(),
                    "pro".to_string(),
                ]),
                false,
            )
            .unwrap();

        assert_eq!(entry.pools, vec!["pro", "default"]);
    }

    #[test]
    fn test_find_allowed_pools() {
        let (manager, _path) = create_temp_manager();

        let entry = manager
            .add(
                "Test".to_string(),
                Some("ksk_test123".to_string()),
                Some(vec!["pool_a".to_string(), "pool_b".to_string()]),
                false,
            )
            .unwrap();

        let pools = manager.find_allowed_pools(&entry.key).unwrap();
        assert_eq!(pools, vec!["pool_a", "pool_b"]);

        assert!(manager.find_allowed_pools("ksk_nonexistent").is_none());
    }

    #[test]
    fn test_find_allowed_pools_disabled() {
        let (manager, _path) = create_temp_manager();

        manager
            .add(
                "Disabled".to_string(),
                Some("ksk_disabled".to_string()),
                None,
                true,
            )
            .unwrap();

        assert!(manager.find_allowed_pools("ksk_disabled").is_none());
    }

    #[test]
    fn test_update() {
        let (manager, _path) = create_temp_manager();

        manager
            .add("Original".to_string(), None, None, false)
            .unwrap();

        let updated = manager
            .update(
                1,
                Some("Updated".to_string()),
                Some(vec!["new_pool".to_string()]),
                Some(true),
            )
            .unwrap();

        assert_eq!(updated.name, "Updated");
        assert_eq!(updated.pools, vec!["new_pool"]);
        assert!(updated.disabled);
    }

    #[test]
    fn test_delete() {
        let (manager, _path) = create_temp_manager();

        manager
            .add("ToDelete".to_string(), None, None, false)
            .unwrap();
        assert_eq!(manager.list().len(), 1);

        manager.delete(1).unwrap();
        assert_eq!(manager.list().len(), 0);
    }

    #[test]
    fn test_delete_not_found() {
        let (manager, _path) = create_temp_manager();
        let result = manager.delete(999);
        assert!(result.is_err());
    }

    #[test]
    fn test_all_pool_names() {
        let (manager, _path) = create_temp_manager();

        manager
            .add(
                "Key1".to_string(),
                None,
                Some(vec!["pool_b".to_string(), "pool_a".to_string()]),
                false,
            )
            .unwrap();

        let names = manager.all_pool_names(&["pool_c".to_string(), "pool_a".to_string()]);
        assert_eq!(names, vec!["pool_a", "pool_b", "pool_c"]);
    }
}
