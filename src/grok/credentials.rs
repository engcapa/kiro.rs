//! Grok Build / xAI 凭据模型。

use std::fs;
use std::path::Path;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::http_client::ProxyConfig;
use crate::model::config::Config;

pub const XAI_DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
/// Grok Build 使用 OAuth session token 时在源码中选择的 CLI chat proxy。
/// 外部 xAI API token 则继续使用 `XAI_DEFAULT_BASE_URL`。
pub const XAI_GROK_CLI_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";
pub const XAI_DEFAULT_TOKEN_ENDPOINT: &str = "https://auth.x.ai/oauth/token";
pub const XAI_GROK_CLI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const XAI_GROK_CLI_REDIRECT_URI: &str = "http://127.0.0.1:56121/callback";

/// xAI Grok Build 凭据。
///
/// 同时兼容 AIClient-2-API 保存的 snake_case 文件以及本项目的 camelCase
/// 配置格式。`access_token` 可直接是 xAI API token；存在 `refresh_token`
/// 时则按 OAuth 凭据处理并会自动刷新。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GrokCredentials {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(
        alias = "access_token",
        alias = "token",
        alias = "api_key",
        alias = "apiKey",
        alias = "key",
        skip_serializing_if = "Option::is_none"
    )]
    pub access_token: Option<String>,

    #[serde(alias = "refresh_token", skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,

    #[serde(alias = "id_token", skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,

    #[serde(alias = "token_type", skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,

    /// OAuth token 过期时间。兼容 AIClient-2-API 的 `expired` 字段。
    #[serde(
        alias = "expires_at",
        alias = "expired",
        skip_serializing_if = "Option::is_none"
    )]
    pub expires_at: Option<String>,

    #[serde(alias = "auth_kind", skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    #[serde(alias = "sub", skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,

    /// Grok CLI OAuth 凭据中可能带有的账户 ID。导入 AIClient-2-API
    /// 文件时保留并在 CLI chat proxy 请求中转发为 `x-userid`。
    #[serde(alias = "user_id", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,

    #[serde(alias = "team_id", skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,

    #[serde(alias = "base_url", skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    #[serde(alias = "token_endpoint", skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,

    #[serde(alias = "last_refresh", skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<String>,

    #[serde(default, skip_serializing_if = "is_zero")]
    pub priority: u32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,

    #[serde(default)]
    pub disabled: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pools: Option<Vec<String>>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// 凭据文件支持单对象和数组，便于从旧工具迁移。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GrokCredentialsConfig {
    Single(GrokCredentials),
    Multiple(Vec<GrokCredentials>),
}

impl GrokCredentialsConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::Multiple(Vec::new()));
        }
        let content = fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::Multiple(Vec::new()));
        }
        Ok(serde_json::from_str(&content)?)
    }

    pub fn into_sorted_credentials(self) -> Vec<GrokCredentials> {
        let mut credentials = match self {
            Self::Single(credential) => vec![credential],
            Self::Multiple(credentials) => credentials,
        };
        for credential in &mut credentials {
            credential.canonicalize();
        }
        credentials.sort_by_key(|credential| credential.priority);
        credentials
    }
}

impl GrokCredentials {
    pub const PROXY_DIRECT: &'static str = "direct";

    pub fn default_credentials_path() -> &'static str {
        "grok_credentials.json"
    }

    pub fn canonicalize(&mut self) {
        let auth_method = self
            .auth_method
            .as_deref()
            .unwrap_or_else(|| {
                if self.refresh_token.is_some() {
                    "oauth"
                } else {
                    "token"
                }
            })
            .trim()
            .to_ascii_lowercase();

        self.auth_method = Some(match auth_method.as_str() {
            "api_key" | "apikey" | "token" | "api-token" => "token".to_string(),
            "social" | "idc" | "oauth" | "xai" => "oauth".to_string(),
            _ if self.refresh_token.is_some() => "oauth".to_string(),
            _ => "token".to_string(),
        });

        self.name = trim_option(self.name.take());
        self.access_token = trim_option(self.access_token.take());
        self.refresh_token = trim_option(self.refresh_token.take());
        self.id_token = trim_option(self.id_token.take());
        self.email = trim_option(self.email.take());
        self.subject = trim_option(self.subject.take());
        self.user_id = trim_option(self.user_id.take());
        self.team_id = trim_option(self.team_id.take());
        self.base_url = trim_option(self.base_url.take());
        self.token_endpoint = trim_option(self.token_endpoint.take());
        self.proxy_url = trim_option(self.proxy_url.take());
        self.proxy_username = trim_option(self.proxy_username.take());
        self.proxy_password = trim_option(self.proxy_password.take());

        if self
            .token_type
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            self.token_type = Some("Bearer".to_string());
        }
        if self.pools.as_ref().is_some_and(|pools| pools.is_empty()) {
            self.pools = None;
        }
    }

    pub fn is_oauth(&self) -> bool {
        self.refresh_token
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .auth_method
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case("oauth"))
    }

    pub fn effective_token_type(&self) -> &str {
        self.token_type.as_deref().unwrap_or("Bearer")
    }

    pub fn effective_base_url<'a>(&'a self, config: &'a Config) -> &'a str {
        if let Some(base_url) = self
            .base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            return base_url;
        }

        // Grok Build 将 OAuth session token 路由到 CLI chat proxy，而普通
        // xAI API token 路由到 api.x.ai。若管理员显式设置了非默认的
        // grokBaseUrl（例如私有网关），则尊重该全局覆盖。
        if self.is_oauth() && uses_default_public_base_url(&config.grok_base_url) {
            XAI_GROK_CLI_BASE_URL
        } else {
            &config.grok_base_url
        }
    }

    pub fn effective_token_endpoint(&self) -> &str {
        self.token_endpoint
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(XAI_DEFAULT_TOKEN_ENDPOINT)
    }

    pub fn effective_proxy(&self, global_proxy: Option<&ProxyConfig>) -> Option<ProxyConfig> {
        match self.proxy_url.as_deref() {
            Some(value) if value.eq_ignore_ascii_case(Self::PROXY_DIRECT) => None,
            Some(value) => {
                let mut proxy = ProxyConfig::new(value);
                if let (Some(username), Some(password)) =
                    (&self.proxy_username, &self.proxy_password)
                {
                    proxy = proxy.with_auth(username, password);
                }
                Some(proxy)
            }
            None => global_proxy.cloned(),
        }
    }

    pub fn effective_pools(&self) -> Vec<String> {
        self.pools
            .as_ref()
            .filter(|pools| !pools.is_empty())
            .cloned()
            .unwrap_or_else(|| vec!["default".to_string()])
    }

    pub fn display_name(&self, id: u64) -> String {
        self.name
            .clone()
            .or_else(|| self.email.clone())
            .or_else(|| self.subject.clone())
            .unwrap_or_else(|| format!("Grok 凭据 #{}", id))
    }
}

/// 返回新建 OAuth 凭据应持久化的上游地址。
///
/// 保留用户显式配置的 `grokBaseUrl`，但默认配置下与 Grok Build 一致地
/// 使用 CLI chat proxy，而不是将 session token 发往公共 API endpoint。
pub fn default_oauth_base_url(config: &Config) -> String {
    if uses_default_public_base_url(&config.grok_base_url) {
        XAI_GROK_CLI_BASE_URL.to_string()
    } else {
        config.grok_base_url.clone()
    }
}

fn uses_default_public_base_url(value: &str) -> bool {
    value
        .trim()
        .trim_end_matches('/')
        .eq_ignore_ascii_case(XAI_DEFAULT_BASE_URL)
}

fn trim_option(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 从未验证的 JWT payload 提取展示用 identity。
///
/// 这里不把 JWT 当作认证依据；认证仍由 xAI token endpoint / Responses API
/// 完成。解析结果只用于管理面板的邮箱和 subject 展示。
pub fn jwt_identity(token: &str) -> (Option<String>, Option<String>) {
    let Some(payload) = token.split('.').nth(1) else {
        return (None, None);
    };
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(payload) else {
        return (None, None);
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (None, None);
    };
    let email = value
        .get("email")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let subject = value
        .get("sub")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    (email, subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_aiclient_snake_case_credentials() {
        let credential: GrokCredentials = serde_json::from_str(
            r#"{"access_token":"token","refresh_token":"refresh","expired":"2030-01-01T00:00:00Z","sub":"subject","user_id":"user","team_id":"team"}"#,
        )
        .unwrap();
        assert_eq!(credential.access_token.as_deref(), Some("token"));
        assert_eq!(credential.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(credential.subject.as_deref(), Some("subject"));
        assert_eq!(credential.user_id.as_deref(), Some("user"));
        assert_eq!(credential.team_id.as_deref(), Some("team"));
        assert_eq!(
            credential.expires_at.as_deref(),
            Some("2030-01-01T00:00:00Z")
        );
    }

    #[test]
    fn canonicalizes_api_key_to_token() {
        let mut credential = GrokCredentials {
            auth_method: Some("api_key".to_string()),
            access_token: Some(" xai-key ".to_string()),
            ..Default::default()
        };
        credential.canonicalize();
        assert_eq!(credential.auth_method.as_deref(), Some("token"));
        assert_eq!(credential.access_token.as_deref(), Some("xai-key"));
    }

    #[test]
    fn accepts_plain_token_alias() {
        let credential: GrokCredentials = serde_json::from_str(r#"{"token":"xai-token"}"#).unwrap();
        assert_eq!(credential.access_token.as_deref(), Some("xai-token"));
    }

    #[test]
    fn oauth_defaults_to_grok_build_cli_proxy() {
        let credential = GrokCredentials {
            refresh_token: Some("refresh".to_string()),
            ..Default::default()
        };
        let config = Config::default();
        assert_eq!(
            credential.effective_base_url(&config),
            XAI_GROK_CLI_BASE_URL
        );

        let mut custom_config = Config::default();
        custom_config.grok_base_url = "https://grok-gateway.example/v1".to_string();
        assert_eq!(
            credential.effective_base_url(&custom_config),
            "https://grok-gateway.example/v1"
        );
    }
}
