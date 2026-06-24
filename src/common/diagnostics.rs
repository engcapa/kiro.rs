//! 服务器端诊断文件落盘工具

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TypeErrorTraceFiles {
    pub request_path: PathBuf,
    pub response_path: PathBuf,
}

pub struct TypeErrorTrace<'a> {
    pub api_type: &'a str,
    pub request_body: &'a str,
    pub response_status: u16,
    pub response_body: &'a str,
    pub attempt: usize,
    pub max_retries: usize,
    pub credential_id: u64,
    pub endpoint: &'a str,
    pub model: Option<&'a str>,
    pub url: &'a str,
}

pub fn is_type_error_response(body: &str) -> bool {
    let lower = body.to_lowercase();

    if lower.contains("type_error")
        || lower.contains("type error")
        || lower.contains("invalid type")
        || lower.contains("expected type")
    {
        return true;
    }

    let mentions_signature = lower.contains("signature");
    let mentions_thinking = lower.contains("thinking") || lower.contains("reasoning");
    let validation_like = lower.contains("validation")
        || lower.contains("invalid")
        || lower.contains("expected")
        || lower.contains("missing")
        || lower.contains("type");

    mentions_signature && mentions_thinking && validation_like
}

pub fn type_error_summary(status: u16, body: &str) -> String {
    let message = match serde_json::from_str::<Value>(body) {
        Ok(value) => extract_error_message(&value)
            .map(str::to_string)
            .unwrap_or_else(|| "upstream type error; response body saved".to_string()),
        Err(_) => body.lines().next().unwrap_or("").to_string(),
    };

    let message = message.trim();
    let message = if message.is_empty() {
        "upstream type error".to_string()
    } else {
        truncate_for_log(message, 500)
    };

    format!("HTTP {} {}", status, message)
}

fn extract_error_message(value: &Value) -> Option<&str> {
    value
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("message").and_then(|v| v.as_str()))
        .or_else(|| value.get("Message").and_then(|v| v.as_str()))
        .or_else(|| value.get("errorMessage").and_then(|v| v.as_str()))
        .or_else(|| value.get("detail").and_then(|v| v.as_str()))
}

pub fn write_type_error_trace(trace: TypeErrorTrace<'_>) -> anyhow::Result<TypeErrorTraceFiles> {
    let dir = trace_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建类型错误诊断目录失败: {}", dir.display()))?;

    let stem = format!(
        "{}_{}",
        Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
        Uuid::new_v4().simple()
    );
    let request_path = dir.join(format!("{}_request.json", stem));
    let response_path = dir.join(format!("{}_response.json", stem));

    write_pretty_json(&request_path, trace.request_body)
        .with_context(|| format!("写入请求诊断文件失败: {}", request_path.display()))?;

    let response_body_json = serde_json::from_str::<Value>(trace.response_body).ok();
    let response = json!({
        "apiType": trace.api_type,
        "status": trace.response_status,
        "attempt": trace.attempt,
        "maxRetries": trace.max_retries,
        "credentialId": trace.credential_id,
        "endpoint": trace.endpoint,
        "model": trace.model,
        "url": trace.url,
        "bodyText": trace.response_body,
        "bodyJson": response_body_json,
    });

    let response_json = serde_json::to_string_pretty(&response)?;
    std::fs::write(&response_path, response_json)
        .with_context(|| format!("写入响应诊断文件失败: {}", response_path.display()))?;

    Ok(TypeErrorTraceFiles {
        request_path,
        response_path,
    })
}

fn trace_dir() -> PathBuf {
    std::env::var("KIRO_TYPE_ERROR_TRACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/type-errors"))
}

fn write_pretty_json(path: &Path, raw: &str) -> anyhow::Result<()> {
    let pretty = serde_json::from_str::<Value>(raw)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| raw.to_string());
    std::fs::write(path, pretty)?;
    Ok(())
}

fn truncate_for_log(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_thinking_signature_type_error() {
        let body = r#"{"message":"ValidationException: invalid type for thinking signature, expected string"}"#;
        assert!(is_type_error_response(body));
    }

    #[test]
    fn does_not_treat_plain_rate_limit_as_type_error() {
        let body = r#"{"message":"Too many requests, retry later"}"#;
        assert!(!is_type_error_response(body));
    }
}
