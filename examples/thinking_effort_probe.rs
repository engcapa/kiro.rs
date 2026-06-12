//! thinking / effort 传递诊断探针（完整版）
//!
//! 复用 kiro-rs 真实代码路径，通过代理 http://127.0.0.1:3128 请求 kiro.dev：
//!   Phase 1  拉取真实模型目录（ListAvailableModels），验证 API Key 路径
//!   Phase 2  catalog=None（fallback）时 get_additional_model_request_fields 的真实输出
//!   Phase 3  加载真实 catalog 后 get_additional_model_request_fields 的真实输出
//!   Phase 4  真实 generateAssistantResponse：带/不带 thinking 字段对比上游是否返回推理内容
//!
//! 运行：cargo run --example thinking_effort_probe

use std::sync::Arc;

use kiro_rs::anthropic::converter::convert_request;
use kiro_rs::anthropic::handlers::get_additional_model_request_fields;
use kiro_rs::anthropic::types::{Message, MessagesRequest, OutputConfig, Thinking};
use kiro_rs::http_client::ProxyConfig;
use kiro_rs::kiro::model::credentials::KiroCredentials;
use kiro_rs::kiro::model::model_catalog::GLOBAL_MODEL_CATALOG;
use kiro_rs::kiro::model::requests::kiro::KiroRequest;
use kiro_rs::kiro::token_manager::MultiTokenManager;
use kiro_rs::model::config::Config;

// API Key 与代理从环境变量读取，避免把密钥写进源码：
//   PROBE_API_KEY=ksk_xxx  PROBE_PROXY=http://127.0.0.1:3128  cargo run --example thinking_effort_probe
const PROXY_DEFAULT: &str = "http://127.0.0.1:3128";
const REGION: &str = "us-east-1";

fn divider(t: &str) {
    println!("\n{}\n  {}\n{}", "=".repeat(78), t, "=".repeat(78));
}

fn sample_request(model: &str, thinking: Option<Thinking>, oc: Option<OutputConfig>) -> MessagesRequest {
    MessagesRequest {
        model: model.to_string(),
        max_tokens: 2048,
        messages: vec![Message { role: "user".to_string(), content: serde_json::json!("用一句话解释为什么天空是蓝色的。") }],
        stream: true,
        system: None,
        tools: None,
        tool_choice: None,
        thinking,
        output_config: oc,
        metadata: None,
    }
}

fn run_field_cases(label: &str, model: &str) {
    let cases: Vec<(&str, Option<Thinking>, Option<OutputConfig>)> = vec![
        ("thinking=enabled budget=20000", Some(Thinking { thinking_type: "enabled".into(), budget_tokens: 20000 }), None),
        ("thinking=adaptive + effort=max", Some(Thinking { thinking_type: "adaptive".into(), budget_tokens: 20000 }), Some(OutputConfig { effort: "max".into() })),
    ];
    println!("[{}] model={}", label, model);
    for (l, th, oc) in cases {
        let req = sample_request(model, th, oc);
        match get_additional_model_request_fields(&req) {
            Some(v) => println!("    {l:<34} => {}", serde_json::to_string(&v).unwrap_or_default()),
            None => println!("    {l:<34} => None（不下发 thinking/effort）"),
        }
    }
}

/// 直接向 runtime.{region}.kiro.dev/generateAssistantResponse 发送（复刻 ide.rs 头部）
async fn send_generate(client: &reqwest::Client, api_key: &str, body: &str, label: &str) -> anyhow::Result<()> {
    let url = format!("https://runtime.{}.kiro.dev/generateAssistantResponse", REGION);
    let host = format!("runtime.{}.kiro.dev", REGION);
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("Connection", "close")
        .header("x-amzn-codewhisperer-optout", "true")
        .header("x-amzn-kiro-agent-mode", "vibe")
        .header("x-amz-user-agent", "aws-sdk-js/1.0.34 KiroIDE-0.11.107-probe")
        .header("user-agent", "aws-sdk-js/1.0.34 ua/2.1 os/darwin#24.6.0 lang/js md/nodejs#22.22.0 api/codewhispererstreaming#1.0.34 m/E KiroIDE-0.11.107-probe")
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("tokentype", "API_KEY")
        .body(body.to_string())
        .send()
        .await?;

    let status = resp.status();
    let bytes = resp.bytes().await?;
    let raw = String::from_utf8_lossy(&bytes);
    let has_reasoning = raw.contains("reasoningContentEvent");
    let has_assistant = raw.contains("assistantResponseEvent");
    println!("  [{label}] HTTP {} | {} bytes | reasoningContentEvent={} | assistantResponseEvent={}",
        status.as_u16(), bytes.len(), has_reasoning, has_assistant);
    if !status.is_success() {
        println!("       上游错误体: {}", &raw[..raw.len().min(300)]);
    } else if has_reasoning {
        // 提取一小段 reasoning 文本证明确实在思考
        if let Some(p) = raw.find("reasoningContentEvent") {
            let seg = &raw[p..raw.len().min(p + 220)];
            let printable: String = seg.chars().filter(|c| !c.is_control() || *c == ' ').collect();
            println!("       ↳ 片段: {}", printable);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = std::env::var("PROBE_API_KEY")
        .or_else(|_| std::env::var("KIRO_API_KEY"))
        .map_err(|_| anyhow::anyhow!("请设置环境变量 PROBE_API_KEY=ksk_xxx"))?;
    let proxy_url = std::env::var("PROBE_PROXY").unwrap_or_else(|_| PROXY_DEFAULT.to_string());

    let mut config = Config::default();
    config.proxy_url = Some(proxy_url.clone());
    let proxy = ProxyConfig::new(&proxy_url);
    let api_cred = KiroCredentials {
        kiro_api_key: Some(api_key.clone()),
        auth_method: Some("api_key".to_string()),
        priority: 0,
        ..Default::default()
    };
    println!("API Key: {}***  代理: {}  region: {}", &api_key[..api_key.len().min(8)], proxy_url, REGION);

    let tm = Arc::new(MultiTokenManager::new(config.clone(), vec![api_cred.clone()], Some(proxy.clone()), None, true)?);
    let http = reqwest::Client::builder().proxy(reqwest::Proxy::all(&proxy_url)?).timeout(std::time::Duration::from_secs(120)).build()?;

    // ===== Phase 1 =====
    divider("Phase 1: 通过代理拉取真实模型目录（含 Bug #1 修复后的 tokentype 头）");
    let catalog = tm.fetch_model_catalog_for_credential(1, &api_cred).await?;
    println!("✅ 拉取成功，{} 个模型", catalog.models.len());
    let thinking_models: Vec<&str> = catalog.models.iter()
        .filter(|m| m.additional_model_request_fields_schema.as_ref()
            .and_then(|s| s.get("properties")).and_then(|p| p.as_object())
            .map(|p| p.contains_key("thinking")).unwrap_or(false))
        .map(|m| m.model_id.as_str()).collect();
    println!("schema 声明支持 thinking 的模型: {:?}", thinking_models);

    // ===== Phase 2: fallback（catalog = None）=====
    divider("Phase 2: catalog=None（API-Key-only 实际状态）下的真实字段构建");
    { *GLOBAL_MODEL_CATALOG.write().unwrap() = None; }
    run_field_cases("opus-4.8 / fallback", "claude-opus-4.8");
    run_field_cases("sonnet-4.5 / fallback", "claude-sonnet-4.5");

    // ===== Phase 3: 加载真实 catalog =====
    divider("Phase 3: 加载真实 catalog 后的真实字段构建");
    { *GLOBAL_MODEL_CATALOG.write().unwrap() = Some(catalog.clone()); }
    run_field_cases("opus-4.8 / catalog", "claude-opus-4.8");
    run_field_cases("sonnet-4.6 / catalog", "claude-sonnet-4.6");

    // ===== Phase 4: 真实 generateAssistantResponse 对比 =====
    divider("Phase 4: 真实 generateAssistantResponse —— 上游是否因 thinking 字段返回推理内容");
    let req = sample_request("claude-opus-4.8", Some(Thinking { thinking_type: "enabled".into(), budget_tokens: 20000 }), None);
    let conv = convert_request(&req)?;

    // 4a: 不带 additionalModelRequestFields（等价于 fallback 丢失 thinking 字段的效果）
    let body_no = serde_json::to_string(&KiroRequest {
        conversation_state: conv.conversation_state.clone(),
        profile_arn: None,
        additional_model_request_fields: None,
    })?;
    send_generate(&http, &api_key, &body_no, "无 thinking 字段").await?;

    // 4b: 带正确的 thinking + effort（Phase 3 catalog 路径产出的字段）
    let body_yes = serde_json::to_string(&KiroRequest {
        conversation_state: conv.conversation_state.clone(),
        profile_arn: None,
        additional_model_request_fields: Some(serde_json::json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "max"}
        })),
    })?;
    send_generate(&http, &api_key, &body_yes, "带 thinking=adaptive+effort=max").await?;

    println!("\n探针结束。");
    Ok(())
}
