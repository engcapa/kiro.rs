//! JWT Token 解析模块
//!
//! 用于从 access_token 中提取用户信息（email、provider 等）

use base64::{Engine as _, engine::general_purpose};
use serde::Deserialize;

/// JWT Token Payload 中的用户信息
#[derive(Debug, Clone, Deserialize)]
pub struct TokenUserInfo {
    /// 用户邮箱
    #[serde(default)]
    pub email: Option<String>,

    /// 认证供应商 (GitHub, Google 等)
    #[serde(default)]
    pub provider: Option<String>,

    /// 用户 ID
    #[serde(default)]
    pub sub: Option<String>,

    /// 用户 ID (备用字段名)
    #[serde(default, alias = "userId")]
    pub user_id: Option<String>,

    /// 供应商用户 ID
    #[serde(default, alias = "providerUserId")]
    pub provider_user_id: Option<String>,

    /// 用户名
    #[serde(default)]
    pub username: Option<String>,

    /// 显示名称
    #[serde(default)]
    pub name: Option<String>,
}

/// 从 JWT token 中解析用户信息
///
/// JWT 格式: header.payload.signature
/// 我们只需要解码 payload 部分（不验证签名）
pub fn parse_token_user_info(access_token: &str) -> Option<TokenUserInfo> {
    // 分割 JWT token
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        tracing::warn!("JWT token 格式不正确，应该有 3 个部分，实际有 {} 个", parts.len());
        return None;
    }

    // 解码 payload (第二部分)
    let payload_base64 = parts[1];

    // JWT 使用 base64url 编码（无填充）
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(payload_base64)
        .or_else(|_| {
            // 如果失败，尝试标准 base64（有些实现可能使用标准 base64）
            general_purpose::STANDARD_NO_PAD.decode(payload_base64)
        })
        .ok()?;

    let payload_str = String::from_utf8(decoded).ok()?;

    tracing::debug!("JWT payload 原始内容: {}", payload_str);

    // 解析 JSON
    match serde_json::from_str::<TokenUserInfo>(&payload_str) {
        Ok(user_info) => {
            tracing::info!(
                "成功从 JWT token 解析用户信息: email={:?}, provider={:?}, sub={:?}",
                user_info.email,
                user_info.provider,
                user_info.sub
            );
            Some(user_info)
        }
        Err(e) => {
            tracing::warn!("解析 JWT payload 失败: {}", e);
            // 尝试解析为通用 JSON 以便调试
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&payload_str) {
                tracing::debug!("JWT payload 包含的字段: {:?}", json.as_object().map(|o| o.keys().collect::<Vec<_>>()));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jwt_token() {
        // 这是一个示例 JWT token (payload: {"sub":"123","email":"user@example.com","provider":"GitHub"})
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjMiLCJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJwcm92aWRlciI6IkdpdEh1YiJ9.dummy_signature";

        let user_info = parse_token_user_info(token);
        assert!(user_info.is_some());

        let info = user_info.unwrap();
        assert_eq!(info.email, Some("user@example.com".to_string()));
        assert_eq!(info.provider, Some("GitHub".to_string()));
        assert_eq!(info.sub, Some("123".to_string()));
    }

    #[test]
    fn test_parse_invalid_token() {
        let invalid_token = "not.a.valid.jwt.token";
        let user_info = parse_token_user_info(invalid_token);
        assert!(user_info.is_none());
    }

    #[test]
    fn test_parse_token_with_missing_fields() {
        // payload: {"sub":"456"}
        let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiI0NTYifQ.dummy";

        let user_info = parse_token_user_info(token);
        assert!(user_info.is_some());

        let info = user_info.unwrap();
        assert_eq!(info.sub, Some("456".to_string()));
        assert_eq!(info.email, None);
        assert_eq!(info.provider, None);
    }
}
