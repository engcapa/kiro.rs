use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::time::Duration;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenTestFile {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    region: String,
    provider: String,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KiroConfig {
    proxy_url: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Kiro OIDC Token & API Test ===");

    // 1. 加载 config.json 获取代理
    let mut proxy_str = None;
    if let Ok(mut file) = File::open("config.json") {
        let mut content = String::new();
        if file.read_to_string(&mut content).is_ok() {
            if let Ok(config) = serde_json::from_str::<KiroConfig>(&content) {
                proxy_str = config.proxy_url;
            }
        }
    }
    println!("Loaded proxy from config.json: {:?}", proxy_str);

    // 2. 加载 token 测试文件
    let token_file_path = "/home/zhyhang/文档/kiro-token-test.json";
    println!("Loading token file from: {}", token_file_path);
    let mut file = File::open(token_file_path)?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let token_info: TokenTestFile = serde_json::from_str(&content)?;

    println!("Token Info:");
    println!("  Provider: {}", token_info.provider);
    println!("  Client ID: {}", token_info.client_id);
    println!("  Region: {}", token_info.region);
    println!("  Email: {:?}", token_info.email);

    // 3. 构建 HTTP 客户端
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30));
    if let Some(ref p) = proxy_str {
        builder = builder.proxy(reqwest::Proxy::all(p)?);
    }
    let client = builder.build()?;

    // 4. 刷新 OIDC Token
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", token_info.region);
    println!("\n[1] Refreshing token via AWS OIDC: {} ...", refresh_url);
    let refresh_body = json!({
        "clientId": token_info.client_id,
        "clientSecret": token_info.client_secret,
        "refreshToken": token_info.refresh_token,
        "grantType": "refresh_token"
    });

    let res = client.post(&refresh_url)
        .header("content-type", "application/json")
        .json(&refresh_body)
        .send()
        .await?;

    let status = res.status();
    println!("  Refresh Response Status: {}", status);
    let res_text = res.text().await?;
    if !status.is_success() {
        println!("  Failed to refresh token: {}", res_text);
        return Err("Token refresh failed".into());
    }

    let refresh_data: serde_json::Value = serde_json::from_str(&res_text)?;
    let access_token = refresh_data["accessToken"].as_str().ok_or("No accessToken in response")?;
    let returned_profile_arn = refresh_data.get("profileArn").and_then(|v| v.as_str());
    println!("  Token refresh successful!");
    println!("  Returned profileArn: {:?}", returned_profile_arn);

    // 5. 测试 kiro.dev 接口
    let mgmt_host = format!("management.{}.kiro.dev", token_info.region);
    let runtime_host = format!("runtime.{}.kiro.dev", token_info.region);
    
    // 关键发现：从 kiro-account-manager 源码中提取的官方 Builder ID 全局共享 profileArn。
    // AWS 为所有个人 Builder ID 账户分配了相同的全局虚拟 Profile。如果使用非此特定的 Profile ARN，
    // AWS 会认为 Token 与 Profile 不匹配并报 403 Forbidden。
    let dummy_profile_arn = "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

    println!("\n==================================================");
    println!("Testing management API: getUsageLimits");
    println!("==================================================");

    // Test 1a: getUsageLimits without profileArn
    {
        let url = format!("https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST", mgmt_host);
        println!("\nTest 1a: getUsageLimits WITHOUT profileArn");
        let res = client.get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    // Test 1b: getUsageLimits WITH dummy profileArn
    {
        let url = format!(
            "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST&profileArn={}",
            mgmt_host,
            urlencoding::encode(dummy_profile_arn)
        );
        println!("\nTest 1b: getUsageLimits WITH dummy profileArn (regex-compliant)");
        let res = client.get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    println!("\n==================================================");
    println!("Testing management API: ListAvailableModels");
    println!("==================================================");

    // Test 2a: ListAvailableModels without profileArn
    {
        let url = format!("https://{}/", mgmt_host);
        let body = json!({ "origin": "AI_EDITOR" });
        println!("\nTest 2a: ListAvailableModels WITHOUT profileArn");
        let res = client.post(&url)
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amz-target", "KiroControlPlaneBearerService.ListAvailableModels")
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&body)
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    // Test 2b: ListAvailableModels WITH dummy profileArn
    {
        let url = format!("https://{}/", mgmt_host);
        let body = json!({
            "origin": "AI_EDITOR",
            "profileArn": dummy_profile_arn
        });
        println!("\nTest 2b: ListAvailableModels WITH dummy profileArn (regex-compliant)");
        let res = client.post(&url)
            .header("content-type", "application/x-amz-json-1.0")
            .header("x-amz-target", "KiroControlPlaneBearerService.ListAvailableModels")
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&body)
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    println!("\n==================================================");
    println!("Testing runtime API: generateAssistantResponse");
    println!("==================================================");

    let chat_prompt = "Hello! Please reply with a short greeting.";
    let chat_body_base = json!({
        "conversationState": {
            "conversationId": format!("conv-{}", uuid::Uuid::new_v4()),
            "currentMessage": {
                "userInputMessage": {
                    "content": chat_prompt,
                    "modelId": "claude-3-5-sonnet",
                    "userInputMessageContext": {
                        "tools": [],
                        "toolResults": []
                    },
                    "origin": "AI_EDITOR"
                }
            },
            "history": [],
            "agentTaskType": "vibe",
            "chatTriggerType": "MANUAL"
        }
    });

    // Test 3a: generateAssistantResponse WITHOUT profileArn
    {
        let url = format!("https://{}/generateAssistantResponse", runtime_host);
        println!("\nTest 3a: generateAssistantResponse WITHOUT profileArn");
        let res = client.post(&url)
            .header("content-type", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&chat_body_base)
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    // Test 3b: generateAssistantResponse WITH dummy profileArn
    {
        let url = format!("https://{}/generateAssistantResponse", runtime_host);
        let mut body = chat_body_base.clone();
        body["profileArn"] = json!(dummy_profile_arn);
        println!("\nTest 3b: generateAssistantResponse WITH dummy profileArn (regex-compliant)");
        let res = client.post(&url)
            .header("content-type", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("x-amzn-kiro-profile-arn", dummy_profile_arn)
            .json(&body)
            .send()
            .await?;
        println!("  Status: {}", res.status());
        println!("  Body: {}", res.text().await?);
    }

    // Test 4: 调用 AWS 官方原生终点 (AwsEndpoint 直连测试)
    {
        let url = "https://q.us-east-1.amazonaws.com/generateAssistantResponse";
        println!("\n==================================================");
        println!("Testing official AWS API: generateAssistantResponse (AwsEndpoint direct)");
        println!("==================================================");
        println!("Calling {} WITHOUT profileArn ...", url);
        
        let invocation_id = uuid::Uuid::new_v4().to_string();
        let x_amz_ua = "aws-sdk-js/1.0.34 KiroIDE-1.0.0-test-machine-id";
        let ua = "aws-sdk-js/1.0.34 ua/2.1 os/windows lang/js md/nodejs#18.0.0 api/codewhispererstreaming#1.0.34 m/E KiroIDE-1.0.0-test-machine-id";
        
        let res = client.post(url)
            .header("content-type", "application/json")
            .header("Connection", "close")
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", x_amz_ua)
            .header("user-agent", ua)
            .header("host", "q.us-east-1.amazonaws.com")
            .header("amz-sdk-invocation-id", &invocation_id)
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&chat_body_base) // 没有 profileArn!
            .send()
            .await?;
            
        let status = res.status();
        println!("  Status: {}", status);
        let body_bytes = res.bytes().await?;
        println!("  Received {} bytes", body_bytes.len());
        if status.is_success() {
            println!("  AwsEndpoint direct call SUCCEEDED!");
            let preview_len = std::cmp::min(body_bytes.len(), 200);
            let preview = String::from_utf8_lossy(&body_bytes[..preview_len]);
            println!("  Body preview (first 200 bytes):\n{}", preview);
        } else {
            let error_text = String::from_utf8_lossy(&body_bytes);
            println!("  Body: {}", error_text);
        }
    }

    println!("\n=== Tests completed successfully! ===");
    Ok(())
}
