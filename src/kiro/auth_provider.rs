//! Infer OAuth / IdP display provider from access tokens and API payloads.

use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use serde_json::Value;

/// Normalize free-text provider names to a short slug (`github`, `google`, `idc`).
pub fn normalize_provider_slug(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_lowercase();
    if lower.contains("github") {
        return Some("github".to_string());
    }
    if lower.contains("google") {
        return Some("google".to_string());
    }
    if lower.contains("idc")
        || lower.contains("builder")
        || lower.contains("iamidentity")
        || lower.contains("sso")
        || lower.contains("oidc")
    {
        return Some("idc".to_string());
    }
    if lower.contains("amazon") && lower.contains("cognito") {
        return Some("idc".to_string());
    }
    Some(lower)
}

/// Parse JWT payload (second segment) and infer provider from common claim shapes.
pub fn infer_from_access_token(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    if let Some(p) = extract_provider_from_json_value(&payload) {
        return Some(p);
    }
    None
}

fn decode_jwt_payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let payload_b64 = parts.nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| URL_SAFE.decode(payload_b64))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn extract_provider_from_json_value(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(u) = map.get("username").and_then(|x| x.as_str()) {
                if let Some(p) = provider_from_username(u) {
                    return Some(p);
                }
            }
            if let Some(u) = map
                .get("cognito:username")
                .and_then(|x| x.as_str())
                .or_else(|| map.get("preferred_username").and_then(|x| x.as_str()))
            {
                if let Some(p) = provider_from_username(u) {
                    return Some(p);
                }
            }

            for (key, val) in map {
                let key_lc = key.to_lowercase();
                if matches!(
                    key_lc.as_str(),
                    "providername" | "identityprovider" | "federationprovider" | "loginwith"
                ) {
                    if let Some(s) = val.as_str().and_then(|s| normalize_provider_slug(s)) {
                        return Some(s);
                    }
                }
                if key_lc == "provider" && val.is_string() {
                    if let Some(s) = val.as_str().and_then(|s| normalize_provider_slug(s)) {
                        return Some(s);
                    }
                }
                if key_lc == "identities" {
                    if let Some(s) = extract_provider_from_identities(val) {
                        return Some(s);
                    }
                }
            }

            if let Some(iss) = map.get("iss").and_then(|x| x.as_str()) {
                let il = iss.to_lowercase();
                if il.contains("oidc.") && il.contains("amazonaws.com") {
                    return Some("idc".to_string());
                }
            }

            for val in map.values() {
                if let Some(p) = extract_provider_from_json_value(val) {
                    return Some(p);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(p) = extract_provider_from_json_value(item) {
                    return Some(p);
                }
            }
        }
        _ => {}
    }
    None
}

fn extract_provider_from_identities(v: &Value) -> Option<String> {
    let arr = v.as_array()?;
    for item in arr {
        if let Some(name) = item
            .get("providerName")
            .or_else(|| item.get("provider"))
            .and_then(|x| x.as_str())
        {
            if let Some(s) = normalize_provider_slug(name) {
                return Some(s);
            }
        }
        if let Some(p) = extract_provider_from_json_value(item) {
            return Some(p);
        }
    }
    None
}

pub(crate) fn provider_from_username(username: &str) -> Option<String> {
    let u = username;
    let ul = u.to_lowercase();
    if ul.starts_with("github_") || ul == "github" {
        return Some("github".to_string());
    }
    if ul.starts_with("google_") || ul == "google" {
        return Some("google".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_slugs() {
        assert_eq!(
            normalize_provider_slug("GitHub").as_deref(),
            Some("github")
        );
        assert_eq!(
            normalize_provider_slug("GOOGLE").as_deref(),
            Some("google")
        );
    }

    #[test]
    fn jwt_identities_array() {
        let payload = json!({
            "identities": [{"providerName": "GitHub", "userId": "x"}]
        });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap());
        let token = format!("xx.{b64}.yy");
        assert_eq!(infer_from_access_token(&token).as_deref(), Some("github"));
    }

    #[test]
    fn jwt_username_prefix() {
        let payload = json!({ "username": "github_12345" });
        let b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap());
        let token = format!("xx.{b64}.yy");
        assert_eq!(infer_from_access_token(&token).as_deref(), Some("github"));
    }
}
