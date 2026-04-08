use base64::Engine;
use serde::{Deserialize, Serialize};

/// 刷新 Token 的请求体 (Social 认证)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// 刷新 Token 的响应体 (Social 认证)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// IdC Token 刷新请求体 (AWS SSO OIDC)
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshRequest {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub grant_type: String,
}

/// IdC Token 刷新响应体 (AWS SSO OIDC)
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdcRefreshResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// JWT claims extracted from id_token
#[derive(Debug, Deserialize)]
pub struct JwtClaims {
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, alias = "identities")]
    pub identities: Option<serde_json::Value>,
}

/// Decode JWT payload without signature verification (for user info extraction only)
pub fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(parts[1]))
        .ok()?;

    serde_json::from_slice(&payload).ok()
}

/// Extract user info from refresh response extra fields
pub fn extract_user_info_from_extra(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> (Option<String>, Option<String>, Option<String>) {
    let mut email: Option<String> = None;
    let mut user_id: Option<String> = None;
    let mut provider: Option<String> = None;

    for key in ["userInfo", "user_info", "identity", "user", "profile"] {
        if let Some(val) = extra.get(key) {
            if let Some(obj) = val.as_object() {
                if email.is_none() {
                    email = obj
                        .get("email")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if user_id.is_none() {
                    user_id = obj
                        .get("userId")
                        .or_else(|| obj.get("user_id"))
                        .or_else(|| obj.get("sub"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
                if provider.is_none() {
                    provider = obj
                        .get("provider")
                        .or_else(|| obj.get("identityProvider"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
        }
    }

    // Also check top-level extra fields
    if email.is_none() {
        email = extra
            .get("email")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if user_id.is_none() {
        user_id = extra
            .get("userId")
            .or_else(|| extra.get("user_id"))
            .or_else(|| extra.get("sub"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }
    if provider.is_none() {
        provider = extra
            .get("provider")
            .or_else(|| extra.get("identityProvider"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
    }

    (email, user_id, provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_jwt_claims_valid() {
        // Build a fake JWT: header.payload.signature
        let payload = r#"{"sub":"user-123","email":"test@example.com"}"#;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.as_bytes());
        let token = format!("eyJhbGciOiJSUzI1NiJ9.{}.fake_signature", encoded);

        let claims = decode_jwt_claims(&token);
        assert!(claims.is_some());
        let c = claims.unwrap();
        assert_eq!(c.sub, Some("user-123".to_string()));
        assert_eq!(c.email, Some("test@example.com".to_string()));
    }

    #[test]
    fn test_decode_jwt_claims_no_email() {
        let payload = r#"{"sub":"user-456"}"#;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.as_bytes());
        let token = format!("header.{}.sig", encoded);

        let claims = decode_jwt_claims(&token).unwrap();
        assert_eq!(claims.sub, Some("user-456".to_string()));
        assert_eq!(claims.email, None);
    }

    #[test]
    fn test_decode_jwt_claims_not_jwt() {
        assert!(decode_jwt_claims("not-a-jwt-token").is_none());
        assert!(decode_jwt_claims("only.two").is_none());
        assert!(decode_jwt_claims("").is_none());
    }

    #[test]
    fn test_decode_jwt_claims_opaque_kiro_token() {
        // Kiro opaque tokens use ':' separator, not '.', should return None
        let token = "aoaAAAAAGnWHZMe:MGQCMEc3PkTqDAWICJQG";
        assert!(decode_jwt_claims(token).is_none());
    }

    #[test]
    fn test_extract_user_info_from_extra_user_info_field() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "userInfo".to_string(),
            serde_json::json!({"email": "a@b.com", "userId": "uid-1", "provider": "GitHub"}),
        );

        let (email, uid, prov) = extract_user_info_from_extra(&extra);
        assert_eq!(email, Some("a@b.com".to_string()));
        assert_eq!(uid, Some("uid-1".to_string()));
        assert_eq!(prov, Some("GitHub".to_string()));
    }

    #[test]
    fn test_extract_user_info_from_extra_top_level() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("email".to_string(), serde_json::json!("direct@test.com"));
        extra.insert("userId".to_string(), serde_json::json!("uid-top"));

        let (email, uid, _) = extract_user_info_from_extra(&extra);
        assert_eq!(email, Some("direct@test.com".to_string()));
        assert_eq!(uid, Some("uid-top".to_string()));
    }

    #[test]
    fn test_refresh_response_with_extra_fields() {
        let json = r#"{
            "accessToken": "tok",
            "expiresIn": 3600,
            "userInfo": {"email": "test@test.com", "userId": "u1"}
        }"#;
        let resp: RefreshResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.access_token, "tok");
        assert!(resp.extra.contains_key("userInfo"));

        let (email, uid, _) = extract_user_info_from_extra(&resp.extra);
        assert_eq!(email, Some("test@test.com".to_string()));
        assert_eq!(uid, Some("u1".to_string()));
    }

    #[test]
    fn test_refresh_response_with_id_token() {
        let payload = r#"{"sub":"jwt-user","email":"jwt@test.com"}"#;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.as_bytes());
        let id_token = format!("header.{}.sig", encoded);

        let json = format!(
            r#"{{"accessToken":"tok","idToken":"{}"}}"#,
            id_token
        );
        let resp: RefreshResponse = serde_json::from_str(&json).unwrap();
        assert!(resp.id_token.is_some());

        let claims = decode_jwt_claims(resp.id_token.as_ref().unwrap()).unwrap();
        assert_eq!(claims.sub, Some("jwt-user".to_string()));
        assert_eq!(claims.email, Some("jwt@test.com".to_string()));
    }

    #[test]
    fn test_idc_refresh_response_captures_id_token() {
        let payload = r#"{"sub":"idc-user","email":"idc@test.com"}"#;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(payload.as_bytes());
        let id_token = format!("h.{}.s", encoded);

        let json = format!(
            r#"{{"accessToken":"tok","idToken":"{}"}}"#,
            id_token
        );
        let resp: IdcRefreshResponse = serde_json::from_str(&json).unwrap();
        assert!(resp.id_token.is_some());

        let claims = decode_jwt_claims(resp.id_token.as_ref().unwrap()).unwrap();
        assert_eq!(claims.sub, Some("idc-user".to_string()));
        assert_eq!(claims.email, Some("idc@test.com".to_string()));
    }
}
