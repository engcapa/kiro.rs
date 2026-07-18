use kiro_rs::{admin, admin_ui, anthropic, grok, http_client, kiro, model, token};
use kiro_rs::model::api_key_manager::ApiKeyManager;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::{IdeEndpoint, KiroEndpoint};
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // Grok Build 凭据与 Kiro 凭据完全独立。文件不存在时按空池启动，
    // 之后可通过 /grok/api/admin/credentials 导入 token，或启动 OAuth 授权。
    let grok_credentials_path = args.grok_credentials.unwrap_or_else(|| {
        grok::credentials::GrokCredentials::default_credentials_path().to_string()
    });
    let grok_credentials_config = grok::credentials::GrokCredentialsConfig::load(
        &grok_credentials_path,
    )
    .unwrap_or_else(|e| {
        tracing::error!("加载 Grok 凭据失败: {}", e);
        std::process::exit(1);
    });
    let grok_credentials_list = grok_credentials_config.into_sorted_credentials();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 获取 API Key
    let api_key = config.api_key.clone().unwrap_or_else(|| {
        tracing::error!("配置文件中未设置 apiKey");
        std::process::exit(1);
    });

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred
            .endpoint
            .as_deref()
            .unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);

    // 启动时拉取模型目录元数据，即使失败也不退出
    tracing::info!("正在拉取 Kiro 模型目录元数据...");
    if let Err(e) = token_manager.refresh_model_catalog().await {
        tracing::error!("启动时拉取 Kiro 模型目录元数据失败: {}", e);
    } else {
        tracing::info!("模型元数据加载流程结束，继续启动流程");
    }

    // 开启 10 分钟定时刷新后台任务
    let tm_clone = token_manager.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
            tracing::info!("定时刷新 Kiro 模型目录元数据...");
            if let Err(e) = tm_clone.refresh_model_catalog().await {
                tracing::warn!("定时刷新 Kiro 模型目录元数据失败 (将沿用上次元数据): {}", e);
            }
        }
    });

    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config.clone(),
        tls_backend: config.tls_backend,
    });

    let api_keys_path = "api_keys.json"; // Default
    let api_key_manager = Arc::new(
        ApiKeyManager::new(api_keys_path.into()).unwrap_or_else(|e| {
            tracing::error!("加载 API Key 配置失败: {}", e);
            std::process::exit(1);
        }),
    );

    // 创建独立的 Grok Build/xAI 凭据池和 Provider。不得复用 Kiro 的
    // MultiTokenManager：两者的 token 刷新、请求协议和响应流格式不同。
    let grok_token_manager = Arc::new(
        grok::token_manager::GrokTokenManager::new(
            config.clone(),
            grok_credentials_list,
            proxy_config.clone(),
            grok_credentials_path.into(),
        )
        .unwrap_or_else(|e| {
            tracing::error!("创建 Grok Token 管理器失败: {}", e);
            std::process::exit(1);
        }),
    );
    let grok_provider = Arc::new(
        grok::provider::GrokProvider::new(grok_token_manager.clone(), proxy_config.clone())
            .unwrap_or_else(|e| {
                tracing::error!("创建 Grok Provider 失败: {}", e);
                std::process::exit(1);
            }),
    );

    // Grok Build 的 `/v1/models` 目录与凭据绑定：不同 OAuth 账号/API token
    // 可见的模型、effort 菜单和 API backend 都可能不同。目录拉取失败只保留
    // 旧值，不影响推理凭据本身。
    tracing::info!("正在拉取 Grok 凭据模型目录...");
    if let Err(error) = grok_provider.refresh_model_catalog(false).await {
        tracing::warn!(%error, "启动时拉取 Grok 模型目录失败");
    }
    let grok_catalog_provider = grok_provider.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
            if let Err(error) = grok_catalog_provider.refresh_model_catalog(false).await {
                tracing::warn!(%error, "定时刷新 Grok 模型目录失败（将沿用旧目录）");
            }
        }
    });

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let anthropic_app = anthropic::create_router_with_provider(
        &api_key,
        Some(kiro_provider),
        config.extract_thinking,
        Some(api_key_manager.clone()),
    );

    let grok_app = grok::create_router_with_provider(
        &api_key,
        grok_provider.clone(),
        config.grok_default_model.clone(),
        config.extract_thinking,
        Some(api_key_manager.clone()),
    );

    // 构建 Admin API 路由（如果配置了非空的 admin_api_key）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app.nest("/grok", grok_app)
        } else {
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                api_key_manager.clone(),
                endpoint_names.clone(),
            );
            let admin_state = admin::AdminState::new(admin_key, admin_service);
            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            // Grok Admin 和已有 Kiro Admin 共享管理员 API Key / client API
            // key 管理器，但各自操作独立的凭据池和凭据文件。
            let grok_oauth = grok::oauth::GrokOAuthService::new(
                grok_token_manager.clone(),
                proxy_config.clone(),
            )
            .unwrap_or_else(|e| {
                tracing::error!("创建 Grok OAuth 服务失败: {}", e);
                std::process::exit(1);
            });
            let grok_admin_service = grok::admin::GrokAdminService::new(
                grok_token_manager.clone(),
                grok_provider.clone(),
                api_key_manager.clone(),
                grok_oauth,
            );
            let grok_admin_state = grok::admin::GrokAdminState::new(admin_key, grok_admin_service);
            let grok_admin_app = grok::admin::create_admin_router(grok_admin_state);
            let grok_admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
                .nest(
                    "/grok",
                    grok_app
                        .nest("/api/admin", grok_admin_app)
                        .nest("/admin", grok_admin_ui_app),
                )
        }
    } else {
        anthropic_app.nest("/grok", grok_app)
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("API Key: {}***", &api_key[..(api_key.len() / 2)]);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("Grok Build / xAI API:");
    tracing::info!("  GET  /grok/v1/models");
    tracing::info!("  POST /grok/v1/messages");
    tracing::info!("  POST /grok/v1/messages/count_tokens");
    tracing::info!("  POST /grok/v1/images/generations");
    tracing::info!("  POST /grok/v1/images/edits");
    tracing::info!("  POST /grok/v1/videos/generations");
    tracing::info!("  GET  /grok/v1/videos/:request_id");
    tracing::info!("  POST /grok/cc/v1/messages");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
        tracing::info!("Grok Admin API:");
        tracing::info!("  GET  /grok/api/admin/credentials");
        tracing::info!("  POST /grok/api/admin/credentials");
        tracing::info!("  POST /grok/api/admin/oauth/start");
        tracing::info!("Grok Admin UI:");
        tracing::info!("  GET  /grok/admin");
    }

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
