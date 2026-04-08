//! 数据持久化测试
//!
//! 验证凭据的 name 和 email 字段能够正确持久化到 credentials.json 文件
//! 并在系统重启后正确加载（Requirements 6.1, 6.2, 6.3, 6.4）

#[cfg(test)]
mod tests {
    use crate::admin::service::AdminService;
    use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 创建临时凭据文件路径
    fn create_temp_credentials_path() -> PathBuf {
        std::env::temp_dir().join(format!("test_credentials_{}.json", uuid::Uuid::new_v4()))
    }

    /// 创建测试用的 TokenManager 和 AdminService
    fn create_test_service_with_path(
        credentials: Vec<KiroCredentials>,
        path: PathBuf,
    ) -> (Arc<MultiTokenManager>, AdminService) {
        let config = Config::default();
        let token_manager = Arc::new(
            MultiTokenManager::new(config, credentials, None, Some(path), true)
                .expect("Failed to create token manager"),
        );
        let service = AdminService::new(token_manager.clone());
        (token_manager, service)
    }

    // ============ Requirement 6.1: 名称更新持久化测试 ============

    #[test]
    fn test_set_name_persists_to_credentials_json() {
        // 验证：当凭据的 name 被更新时，更新会写入 credentials.json 文件
        let temp_path = create_temp_credentials_path();

        // 创建初始凭据文件
        let mut initial_cred = KiroCredentials::default();
        initial_cred.id = Some(1);
        initial_cred.refresh_token = Some("test_token".to_string());
        initial_cred.name = None; // 初始无名称
        initial_cred.access_token = Some("test_access".to_string());
        initial_cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let initial_json = serde_json::to_string_pretty(&vec![initial_cred.clone()]).unwrap();
        fs::write(&temp_path, initial_json).unwrap();

        // 创建 service
        let (_manager, service) = create_test_service_with_path(vec![initial_cred], temp_path.clone());

        // 设置名称
        let new_name = "Production Credential".to_string();
        service.set_name(1, new_name.clone()).unwrap();

        // 验证文件内容已更新
        let file_content = fs::read_to_string(&temp_path).unwrap();
        let persisted: Vec<KiroCredentials> = serde_json::from_str(&file_content).unwrap();

        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, Some(1));
        assert_eq!(persisted[0].name, Some(new_name));

        // 清理
        fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn test_set_name_updates_existing_name() {
        // 验证：更新已有名称的凭据时，新名称会正确持久化
        let temp_path = create_temp_credentials_path();

        let mut initial_cred = KiroCredentials::default();
        initial_cred.id = Some(1);
        initial_cred.refresh_token = Some("test_token".to_string());
        initial_cred.name = Some("Old Name".to_string());
        initial_cred.access_token = Some("test_access".to_string());
        initial_cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let initial_json = serde_json::to_string_pretty(&vec![initial_cred.clone()]).unwrap();
        fs::write(&temp_path, initial_json).unwrap();

        let (_manager, service) = create_test_service_with_path(vec![initial_cred], temp_path.clone());

        // 更新名称
        let new_name = "New Name".to_string();
        service.set_name(1, new_name.clone()).unwrap();

        // 验证文件内容
        let file_content = fs::read_to_string(&temp_path).unwrap();
        let persisted: Vec<KiroCredentials> = serde_json::from_str(&file_content).unwrap();

        assert_eq!(persisted[0].name, Some(new_name));

        fs::remove_file(&temp_path).unwrap();
    }

    // ============ Requirement 6.2: 添加凭据时持久化 name 和 email ============

    #[test]
    fn test_credentials_with_name_and_email_persist() {
        // 验证：包含 name 和 email 的凭据能够正确持久化
        let temp_path = create_temp_credentials_path();

        // 创建包含 name 和 email 的凭据
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.name = Some("Test Credential".to_string());
        cred.email = Some("test@example.com".to_string());
        cred.access_token = Some("test_access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 创建 service（这会触发加载）
        let (_manager, service) = create_test_service_with_path(vec![cred], temp_path.clone());

        // 验证数据通过 API 可访问
        let response = service.get_all_credentials();
        assert_eq!(response.credentials.len(), 1);
        assert_eq!(
            response.credentials[0].name,
            Some("Test Credential".to_string())
        );
        assert_eq!(
            response.credentials[0].email,
            Some("test@example.com".to_string())
        );

        // 验证文件内容
        let file_content = fs::read_to_string(&temp_path).unwrap();
        let persisted: Vec<KiroCredentials> = serde_json::from_str(&file_content).unwrap();

        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].name, Some("Test Credential".to_string()));
        assert_eq!(persisted[0].email, Some("test@example.com".to_string()));

        fs::remove_file(&temp_path).unwrap();
    }

    // ============ Requirement 6.3: 系统重启后加载 name 和 email ============

    #[test]
    fn test_load_credentials_with_name_and_email_after_restart() {
        // 验证：系统重启后，从 credentials.json 加载的凭据包含 name 和 email 字段
        let temp_path = create_temp_credentials_path();

        // 创建包含 name 和 email 的凭据文件
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.name = Some("Saved Credential".to_string());
        cred.email = Some("saved@example.com".to_string());
        cred.access_token = Some("test_access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 模拟系统重启：重新加载凭据
        let credentials = CredentialsConfig::load(&temp_path)
            .unwrap()
            .into_sorted_credentials();

        // 验证加载的凭据包含 name 和 email
        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].name, Some("Saved Credential".to_string()));
        assert_eq!(credentials[0].email, Some("saved@example.com".to_string()));

        // 创建 service 验证数据可用
        let (_manager, service) = create_test_service_with_path(credentials, temp_path.clone());

        let snapshot = service.get_all_credentials();
        assert_eq!(snapshot.credentials.len(), 1);
        assert_eq!(
            snapshot.credentials[0].name,
            Some("Saved Credential".to_string())
        );
        assert_eq!(
            snapshot.credentials[0].email,
            Some("saved@example.com".to_string())
        );

        fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn test_multiple_credentials_persist_and_reload() {
        // 验证：多个凭据的 name 和 email 都能正确持久化和重新加载
        let temp_path = create_temp_credentials_path();

        let mut cred1 = KiroCredentials::default();
        cred1.id = Some(1);
        cred1.refresh_token = Some("token1".to_string());
        cred1.name = Some("Credential 1".to_string());
        cred1.email = Some("user1@example.com".to_string());
        cred1.access_token = Some("access1".to_string());
        cred1.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let mut cred2 = KiroCredentials::default();
        cred2.id = Some(2);
        cred2.refresh_token = Some("token2".to_string());
        cred2.name = Some("Credential 2".to_string());
        cred2.email = Some("user2@example.com".to_string());
        cred2.access_token = Some("access2".to_string());
        cred2.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred1, cred2]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 重新加载
        let credentials = CredentialsConfig::load(&temp_path)
            .unwrap()
            .into_sorted_credentials();

        assert_eq!(credentials.len(), 2);
        assert_eq!(credentials[0].name, Some("Credential 1".to_string()));
        assert_eq!(credentials[0].email, Some("user1@example.com".to_string()));
        assert_eq!(credentials[1].name, Some("Credential 2".to_string()));
        assert_eq!(credentials[1].email, Some("user2@example.com".to_string()));

        fs::remove_file(&temp_path).unwrap();
    }

    // ============ Requirement 6.4: credentials.json 文件格式正确且可读 ============

    #[test]
    fn test_credentials_json_format_is_valid() {
        // 验证：持久化的 credentials.json 文件格式正确且可读
        let temp_path = create_temp_credentials_path();

        // 创建凭据
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.name = Some("Test".to_string());
        cred.email = Some("test@example.com".to_string());
        cred.access_token = Some("access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 创建 service（会触发持久化）
        let (_manager, service) = create_test_service_with_path(vec![cred], temp_path.clone());

        // 修改名称触发持久化
        service.set_name(1, "Updated Test".to_string()).unwrap();

        // 验证文件可读且格式正确
        let file_content = fs::read_to_string(&temp_path).unwrap();

        // 1. 验证是有效的 JSON
        let parse_result: Result<serde_json::Value, _> = serde_json::from_str(&file_content);
        assert!(
            parse_result.is_ok(),
            "credentials.json 应该是有效的 JSON"
        );

        // 2. 验证是数组格式
        let json_value = parse_result.unwrap();
        assert!(json_value.is_array(), "credentials.json 应该是数组格式");

        // 3. 验证可以反序列化为 KiroCredentials
        let credentials: Result<Vec<KiroCredentials>, _> = serde_json::from_str(&file_content);
        assert!(
            credentials.is_ok(),
            "credentials.json 应该可以反序列化为 KiroCredentials"
        );

        // 4. 验证包含必要字段
        let creds = credentials.unwrap();
        assert_eq!(creds.len(), 1);
        assert!(creds[0].id.is_some());
        assert!(creds[0].refresh_token.is_some());
        assert_eq!(creds[0].name, Some("Updated Test".to_string()));
        assert_eq!(creds[0].email, Some("test@example.com".to_string()));

        fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn test_credentials_json_uses_pretty_format() {
        // 验证：credentials.json 使用格式化的 JSON（便于人工阅读）
        let temp_path = create_temp_credentials_path();

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test".to_string());
        cred.name = Some("Test".to_string());
        cred.access_token = Some("access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        let (_manager, service) = create_test_service_with_path(vec![cred], temp_path.clone());

        // 触发持久化
        service.set_name(1, "Updated".to_string()).unwrap();

        let file_content = fs::read_to_string(&temp_path).unwrap();

        // 验证包含换行符和缩进（pretty format 的特征）
        assert!(
            file_content.contains('\n'),
            "文件应该包含换行符（pretty format）"
        );
        assert!(
            file_content.contains("  "),
            "文件应该包含缩进（pretty format）"
        );

        fs::remove_file(&temp_path).unwrap();
    }

    // ============ 边界情况测试 ============

    #[test]
    fn test_persist_name_with_special_characters() {
        // 验证：包含特殊字符的名称能正确持久化和加载
        let temp_path = create_temp_credentials_path();

        let special_name = "Test \"Name\" with 'quotes' & symbols: @#$%";
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test".to_string());
        cred.name = Some(special_name.to_string());
        cred.access_token = Some("access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 重新加载验证
        let credentials = CredentialsConfig::load(&temp_path)
            .unwrap()
            .into_sorted_credentials();

        assert_eq!(credentials[0].name, Some(special_name.to_string()));

        fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn test_persist_unicode_name() {
        // 验证：Unicode 字符（中文、emoji）能正确持久化
        let temp_path = create_temp_credentials_path();

        let unicode_name = "测试凭据 🔑 Test";
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test".to_string());
        cred.name = Some(unicode_name.to_string());
        cred.access_token = Some("access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        // 重新加载验证
        let credentials = CredentialsConfig::load(&temp_path)
            .unwrap()
            .into_sorted_credentials();

        assert_eq!(credentials[0].name, Some(unicode_name.to_string()));

        fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn test_persist_none_name_not_serialized() {
        // 验证：name 为 None 时不会序列化到 JSON（skip_serializing_if）
        let temp_path = create_temp_credentials_path();

        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test".to_string());
        cred.name = None; // 明确设置为 None
        cred.access_token = Some("access".to_string());
        cred.expires_at = Some(
            (chrono::Utc::now() + chrono::Duration::hours(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );

        let json = serde_json::to_string_pretty(&vec![cred.clone()]).unwrap();
        fs::write(&temp_path, json).unwrap();

        let file_content = fs::read_to_string(&temp_path).unwrap();

        // 验证 JSON 中不包含 "name" 字段
        assert!(
            !file_content.contains("\"name\""),
            "name 为 None 时不应序列化到 JSON"
        );

        fs::remove_file(&temp_path).unwrap();
    }
}
