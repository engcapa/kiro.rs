# JWT Token 解析功能实现总结

## 🎯 实现目标

通过解析 JWT access_token 自动提取用户信息（email、provider 等），解决 API 返回 `email: null` 的问题。

## 📦 新增文件

### 1. `src/kiro/jwt_parser.rs`
JWT Token 解析模块，提供以下功能：
- 解码 JWT payload（base64url）
- 提取用户信息（email、provider、userId 等）
- 支持多种字段名变体
- 完整的单元测试

### 2. `JWT_PARSING_TEST.md`
测试文档，包含：
- 测试步骤
- 预期行为
- 故障排查指南

## 🔧 修改的文件

### 1. `Cargo.toml`
添加依赖：
```toml
base64 = "0.22"  # Base64 编解码（用于 JWT 解析）
```

### 2. `src/kiro/mod.rs`
添加 jwt_parser 模块导出

### 3. `src/kiro/token_manager.rs`
在两个地方添加 JWT 解析：

#### a) Token 刷新后（refresh_social_token 和 refresh_idc_token）
```rust
// 尝试从 access_token 中解析用户信息
if let Some(token_user_info) = jwt_parser::parse_token_user_info(&data.access_token) {
    if let Some(email) = token_user_info.email {
        if new_credentials.email.is_none() {
            tracing::info!("从 JWT token 中提取到 email: {}", email);
            new_credentials.email = Some(email);
        }
    }
}
```

#### b) 启动时（MultiTokenManager::new）
```rust
// 如果凭据中没有 email，尝试从 access_token 中解析
if cred.email.is_none() {
    if let Some(access_token) = &cred.access_token {
        if let Some(token_user_info) = jwt_parser::parse_token_user_info(access_token) {
            if let Some(email) = token_user_info.email {
                tracing::info!("凭据 #{} 从 JWT token 中提取到 email: {}", id, email);
                cred.email = Some(email);
            }
        }
    }
}
```

### 4. `src/kiro/model/usage_limits.rs`
- 清理未使用的 `Serialize` 导入
- 保留之前实现的 `resolve_account_info()` 逻辑

### 5. `src/admin/service.rs`
保持之前的实现：
```rust
// 优先使用 API 返回的 email，如果没有则使用凭据中保存的 email
let email = usage.email()
    .or_else(|| credential.and_then(|c| c.email.clone()));
```

## 🔄 数据流

### 完整的 Email 获取优先级

1. **API 返回的 email** (`getUsageLimits` 响应中的 `userInfo.email`)
2. **JWT Token 中的 email** (从 `access_token` 解析)
3. **凭据配置中的 email** (用户手动配置)
4. **None** (无法获取)

### 时机

1. **启动时**：从现有 `access_token` 解析
2. **Token 刷新后**：从新的 `access_token` 解析
3. **查询余额时**：从 API 响应或凭据中获取

## 📊 支持的 JWT 字段

```rust
pub struct TokenUserInfo {
    pub email: Option<String>,           // 用户邮箱
    pub provider: Option<String>,        // 认证供应商
    pub sub: Option<String>,             // 用户 ID (subject)
    pub user_id: Option<String>,         // 用户 ID (备用)
    pub provider_user_id: Option<String>, // 供应商用户 ID
    pub username: Option<String>,        // 用户名
    pub name: Option<String>,            // 显示名称
}
```

## ✅ 测试覆盖

### 单元测试
- ✅ `test_parse_jwt_token`: 正常解析
- ✅ `test_parse_invalid_token`: 无效 token
- ✅ `test_parse_token_with_missing_fields`: 缺失字段
- ✅ `test_parse_userinfo_from_api`: API 响应解析

### 集成测试
运行服务后查看日志：
```bash
RUST_LOG=debug cargo run
```

## 🎨 前端显示建议

```typescript
function getUserDisplay(balance: BalanceResponse) {
  // 1. 优先显示 email
  if (balance.email) {
    return balance.email;
  }
  
  // 2. 显示 provider + userId
  if (balance.userId) {
    const shortId = balance.userId.substring(0, 20);
    return balance.provider 
      ? `${balance.provider}:${shortId}...`
      : `${shortId}...`;
  }
  
  // 3. 默认显示
  return "未知用户";
}
```

## 🔍 调试日志

### 成功解析
```
DEBUG kiro_rs::kiro::jwt_parser: JWT payload 原始内容: {"sub":"123","email":"user@example.com"}
INFO kiro_rs::kiro::jwt_parser: 成功从 JWT token 解析用户信息: email=Some("user@example.com"), provider=None, sub=Some("123")
INFO kiro_rs::kiro::token_manager: 从 JWT token 中提取到 email: user@example.com
```

### 解析失败
```
WARN kiro_rs::kiro::jwt_parser: JWT token 格式不正确，应该有 3 个部分，实际有 2 个
WARN kiro_rs::kiro::jwt_parser: 解析 JWT payload 失败: missing field `sub`
DEBUG kiro_rs::kiro::jwt_parser: JWT payload 包含的字段: ["iat", "exp", "aud"]
```

## 🚀 优势

1. **自动化**：无需手动配置 email
2. **实时更新**：Token 刷新时自动更新
3. **健壮性**：多重回退机制
4. **调试友好**：详细的日志输出
5. **安全**：只读取 payload，不验证签名（不需要密钥）

## 📝 注意事项

1. JWT 解析不验证签名（因为我们只是读取信息）
2. 如果 JWT 中没有 email 字段，会回退到其他方式
3. 支持 base64url 和标准 base64 两种编码
4. 字段名支持 camelCase 和 snake_case

## 🎯 下一步

1. 测试实际的 Kiro JWT token 格式
2. 根据实际 payload 调整字段映射
3. 考虑添加 provider 字段到 `KiroCredentials`
4. 前端实现用户信息显示
