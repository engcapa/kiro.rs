//! 向后兼容性测试
//!
//! 验证系统能够正确处理不包含 name 和 email 字段的旧凭据数据
//! Requirements: 7.1, 7.2, 7.3, 7.4

#[cfg(test)]
mod tests {
    use crate::admin::service::AdminService;
    use crate::admin::types::CredentialStatusItem;
    use crate::kiro::model::credentials::{CredentialsConfig, KiroCredentials};
    use crate::kiro::token_manager::MultiTokenManager;
    use crate::model::config::Config;
    use std::sync::Arc;

    // ============ Requirement 7.1: 加载不包含 name 字段的旧凭据数据 ============

    #[test]
    fn test_load_old_credential_without_name_field() {
        // 模拟旧格式 JSON：不包含 name 字段
        let old_json = r#"{
            "id": 1,
            "refreshToken": "old_refresh_token",
            "authMethod": "social",
            "priority": 0
        }"#;

        let creds = KiroCredentials::from_json(old_json).unwrap();

        // 验证：name 字段应该为 None
        assert_eq!(creds.name, None, "旧凭据的 name 应该为 None");
        assert_eq!(creds.id, Some(1));
        assert_eq!(
            creds.refresh_token,
            Some("old_refresh_token".to_string())
        );
        assert_eq!(creds.auth_method, Some("social".to_string()));
    }

    #[test]
    fn test_load_multiple_old_credentials_without_name() {
        // 模拟旧格式多凭据 JSON：不包含 name 字段
        let old_json = r#"[
            {
                "id": 1,
                "refreshToken": "token1",
                "authMethod": "social"
            },
            {
                "id": 2,
                "refreshToken": "token2",
                "authMethod": "idc",
                "clientId": "client123"
            }
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(old_json).unwrap();
        let creds_list = config.into_sorted_credentials();

        assert_eq!(creds_list.len(), 2);

        // 验证：所有旧凭据的 name 都应该为 None
        for cred in &creds_list {
            assert_eq!(cred.name, None, "旧凭据的 name 应该为 None");
        }

        assert_eq!(creds_list[0].id, Some(1));
        assert_eq!(creds_list[1].id, Some(2));
    }

    // ============ Requirement 7.2: 加载不包含 email 字段的旧凭据数据 ============

    #[test]
    fn test_load_old_credential_without_email_field() {
        // 模拟旧格式 JSON：不包含 email 字段
        let old_json = r#"{
            "id": 1,
            "refreshToken": "old_refresh_token",
            "authMethod": "social"
        }"#;

        let creds = KiroCredentials::from_json(old_json).unwrap();

        // 验证：email 字段应该为 None
        assert_eq!(creds.email, None, "旧凭据的 email 应该为 None");
        assert_eq!(creds.id, Some(1));
    }

    #[test]
    fn test_load_old_credential_without_name_and_email() {
        // 模拟最旧格式 JSON：既不包含 name 也不包含 email
        let old_json = r#"{
            "id": 42,
            "accessToken": "access_token_value",
            "refreshToken": "refresh_token_value",
            "expiresAt": "2025-12-31T00:00:00Z",
            "authMethod": "social",
            "priority": 5
        }"#;

        let creds = KiroCredentials::from_json(old_json).unwrap();

        // 验证：name 和 email 都应该为 None
        assert_eq!(creds.name, None, "旧凭据的 name 应该为 None");
        assert_eq!(creds.email, None, "旧凭据的 email 应该为 None");

        // 验证：其他字段正常解析
        assert_eq!(creds.id, Some(42));
        assert_eq!(creds.access_token, Some("access_token_value".to_string()));
        assert_eq!(
            creds.refresh_token,
            Some("refresh_token_value".to_string())
        );
        assert_eq!(creds.priority, 5);
    }

    // ============ Requirement 7.3: Admin UI 正确显示 Display_Name ============

    #[test]
    fn test_admin_service_returns_none_for_old_credentials() {
        // 创建不包含 name 和 email 的旧凭据
        let mut old_cred = KiroCredentials::default();
        old_cred.id = Some(1);
        old_cred.refresh_token = Some("old_token".to_string());
        old_cred.name = None;
        old_cred.email = None;

        let config = Config::default();
        let token_manager = Arc::new(
            MultiTokenManager::new(config, vec![old_cred], None, None, false)
                .expect("Failed to create token manager"),
        );
        let service = AdminService::new(token_manager);

        // 获取凭据状态
        let response = service.get_all_credentials();

        assert_eq!(response.credentials.len(), 1);
        let cred_status = &response.credentials[0];

        // 验证：name 和 email 都应该为 None
        assert_eq!(cred_status.name, None, "旧凭据的 name 应该为 None");
        assert_eq!(cred_status.email, None, "旧凭据的 email 应该为 None");
        assert_eq!(cred_status.id, 1);
    }

    #[test]
    fn test_display_name_fallback_for_old_credentials() {
        // 模拟前端 Display Name 解析逻辑
        fn resolve_display_name(cred: &CredentialStatusItem) -> String {
            cred.name
                .clone()
                .or_else(|| cred.email.clone())
                .unwrap_or_else(|| format!("凭据 #{}", cred.id))
        }

        // 测试场景 1：既没有 name 也没有 email（最旧格式）
        let old_cred = CredentialStatusItem {
            id: 1,
            priority: 0,
            disabled: false,
            failure_count: 0,
            is_current: true,
            expires_at: None,
            auth_method: Some("social".to_string()),
            has_profile_arn: false,
            refresh_token_hash: None,
            email: None,
            success_count: 0,
            last_used_at: None,
            has_proxy: false,
            proxy_url: None,
            refresh_failure_count: 0,
            disabled_reason: None,
            name: None,
        };

        let display_name = resolve_display_name(&old_cred);
        assert_eq!(
            display_name, "凭据 #1",
            "没有 name 和 email 时应该显示默认格式"
        );

        // 测试场景 2：有 email 但没有 name（旧格式）
        let cred_with_email = CredentialStatusItem {
            id: 2,
            priority: 0,
            disabled: false,
            failure_count: 0,
            is_current: true,
            expires_at: None,
            auth_method: Some("social".to_string()),
            has_profile_arn: false,
            refresh_token_hash: None,
            email: Some("user@example.com".to_string()),
            success_count: 0,
            last_used_at: None,
            has_proxy: false,
            proxy_url: None,
            refresh_failure_count: 0,
            disabled_reason: None,
            name: None,
        };

        let display_name = resolve_display_name(&cred_with_email);
        assert_eq!(
            display_name, "user@example.com",
            "有 email 但没有 name 时应该显示 email"
        );

        // 测试场景 3：有 name（新格式）
        let cred_with_name = CredentialStatusItem {
            id: 3,
            priority: 0,
            disabled: false,
            failure_count: 0,
            is_current: true,
            expires_at: None,
            auth_method: Some("social".to_string()),
            has_profile_arn: false,
            refresh_token_hash: None,
            email: Some("user@example.com".to_string()),
            success_count: 0,
            last_used_at: None,
            has_proxy: false,
            proxy_url: None,
            refresh_failure_count: 0,
            disabled_reason: None,
            name: Some("My Credential".to_string()),
        };

        let display_name = resolve_display_name(&cred_with_name);
        assert_eq!(
            display_name, "My Credential",
            "有 name 时应该优先显示 name"
        );
    }

    // ============ Requirement 7.4: skip_serializing_if 处理 null 值 ============

    #[test]
    fn test_serialization_skips_none_name_field() {
        // 创建不包含 name 的凭据
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.name = None;

        let json = cred.to_pretty_json().unwrap();

        // 验证：name 字段不应该出现在 JSON 中
        assert!(
            !json.contains("\"name\""),
            "name 为 None 时不应该序列化"
        );
        assert!(json.contains("refreshToken"), "其他字段应该正常序列化");
    }

    #[test]
    fn test_serialization_skips_none_email_field() {
        // 创建不包含 email 的凭据
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.email = None;

        let json = cred.to_pretty_json().unwrap();

        // 验证：email 字段不应该出现在 JSON 中
        assert!(
            !json.contains("\"email\""),
            "email 为 None 时不应该序列化"
        );
    }

    #[test]
    fn test_serialization_includes_name_when_present() {
        // 创建包含 name 的凭据
        let mut cred = KiroCredentials::default();
        cred.id = Some(1);
        cred.refresh_token = Some("test_token".to_string());
        cred.name = Some("Test Credential".to_string());

        let json = cred.to_pretty_json().unwrap();

        // 验证：name 字段应该出现在 JSON 中
        assert!(json.contains("\"name\""), "name 有值时应该序列化");
        assert!(
            json.contains("Test Credential"),
            "name 值应该正确序列化"
        );
    }

    #[test]
    fn test_credential_status_item_serialization_skips_none() {
        // 测试 CredentialStatusItem 的序列化行为
        let status = CredentialStatusItem {
            id: 1,
            priority: 0,
            disabled: false,
            failure_count: 0,
            is_current: true,
            expires_at: None,
            auth_method: Some("social".to_string()),
            has_profile_arn: false,
            refresh_token_hash: None,
            email: None,
            success_count: 0,
            last_used_at: None,
            has_proxy: false,
            proxy_url: None,
            refresh_failure_count: 0,
            disabled_reason: None,
            name: None,
        };

        let json = serde_json::to_string(&status).unwrap();

        // 验证：name 为 None 时不应该序列化
        assert!(
            !json.contains("\"name\""),
            "CredentialStatusItem 的 name 为 None 时不应该序列化"
        );
    }

    // ============ 混合场景测试 ============

    #[test]
    fn test_mixed_old_and_new_credentials() {
        // 模拟混合场景：同时存在旧格式和新格式凭据
        let json = r#"[
            {
                "id": 1,
                "refreshToken": "old_token",
                "authMethod": "social"
            },
            {
                "id": 2,
                "refreshToken": "new_token",
                "authMethod": "social",
                "email": "user@example.com"
            },
            {
                "id": 3,
                "refreshToken": "newest_token",
                "authMethod": "social",
                "email": "admin@example.com",
                "name": "Production Credential"
            }
        ]"#;

        let config: CredentialsConfig = serde_json::from_str(json).unwrap();
        let creds_list = config.into_sorted_credentials();

        assert_eq!(creds_list.len(), 3);

        // 验证旧格式凭据（无 name 和 email）
        assert_eq!(creds_list[0].name, None);
        assert_eq!(creds_list[0].email, None);

        // 验证中间格式凭据（有 email 无 name）
        assert_eq!(creds_list[1].name, None);
        assert_eq!(creds_list[1].email, Some("user@example.com".to_string()));

        // 验证新格式凭据（有 name 和 email）
        assert_eq!(
            creds_list[2].name,
            Some("Production Credential".to_string())
        );
        assert_eq!(
            creds_list[2].email,
            Some("admin@example.com".to_string())
        );
    }

    #[test]
    fn test_roundtrip_preserves_backward_compatibility() {
        // 测试序列化和反序列化往返后保持向后兼容性
        let mut old_cred = KiroCredentials::default();
        old_cred.id = Some(1);
        old_cred.refresh_token = Some("old_token".to_string());
        old_cred.auth_method = Some("social".to_string());
        old_cred.name = None;
        old_cred.email = None;

        // 序列化
        let json = old_cred.to_pretty_json().unwrap();

        // 验证：JSON 不包含 name 和 email 字段
        assert!(!json.contains("\"name\""));
        assert!(!json.contains("\"email\""));

        // 反序列化
        let parsed = KiroCredentials::from_json(&json).unwrap();

        // 验证：往返后字段值保持一致
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.email, None);
        assert_eq!(parsed.id, old_cred.id);
        assert_eq!(parsed.refresh_token, old_cred.refresh_token);
    }

    // ============ 边界条件测试 ============

    #[test]
    fn test_empty_string_vs_none_for_name() {
        // 验证：空字符串和 None 的区别
        let json_with_empty = r#"{"refreshToken": "test", "name": ""}"#;
        let json_with_none = r#"{"refreshToken": "test"}"#;

        let cred_empty = KiroCredentials::from_json(json_with_empty).unwrap();
        let cred_none = KiroCredentials::from_json(json_with_none).unwrap();

        // 空字符串会被解析为 Some("")
        assert_eq!(cred_empty.name, Some("".to_string()));
        // 缺失字段会被解析为 None
        assert_eq!(cred_none.name, None);
    }

    #[test]
    fn test_null_value_in_json() {
        // 验证：JSON 中的 null 值
        let json = r#"{"refreshToken": "test", "name": null, "email": null}"#;
        let cred = KiroCredentials::from_json(json).unwrap();

        // null 值应该被解析为 None
        assert_eq!(cred.name, None);
        assert_eq!(cred.email, None);
    }
}
