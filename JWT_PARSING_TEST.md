# JWT Token 解析功能测试

## 测试步骤

1. 启动服务
2. 查看日志中是否有 JWT 解析的信息
3. 检查 API 返回的用户信息

## 预期行为

### Token 刷新时
```
INFO kiro_rs::kiro::token_manager: 从 JWT token 中提取到 email: user@example.com
```

### 启动时（如果 access_token 存在）
```
INFO kiro_rs::kiro::token_manager: 凭据 #1 从 JWT token 中提取到 email: user@example.com
```

### JWT Payload 调试
```
DEBUG kiro_rs::kiro::jwt_parser: JWT payload 原始内容: {"sub":"123","email":"user@example.com","provider":"GitHub"}
INFO kiro_rs::kiro::jwt_parser: 成功从 JWT token 解析用户信息: email=Some("user@example.com"), provider=Some("GitHub"), sub=Some("123")
```

## 测试用例

### 1. 测试 JWT 解析
```bash
cargo test jwt_parser -- --nocapture
```

### 2. 测试完整流程
```bash
# 设置日志级别为 debug
export RUST_LOG=debug
cargo run
```

### 3. 查看 API 响应
```bash
curl http://localhost:8990/admin/balance/1
```

预期响应包含：
```json
{
  "id": 1,
  "email": "user@example.com",
  "userId": "d-9067c98495...",
  "provider": "GitHub"
}
```

## JWT Token 格式说明

JWT Token 由三部分组成，用 `.` 分隔：
```
header.payload.signature
```

我们只解析 payload 部分，它是 base64url 编码的 JSON。

### 示例 Payload
```json
{
  "sub": "user-id-123",
  "email": "user@example.com",
  "provider": "GitHub",
  "iat": 1234567890,
  "exp": 1234567890
}
```

## 支持的字段

- `email`: 用户邮箱
- `provider`: 认证供应商 (GitHub, Google, etc.)
- `sub`: 用户 ID (subject)
- `userId` / `user_id`: 用户 ID (备用字段名)
- `username`: 用户名
- `name`: 显示名称
- `providerUserId` / `provider_user_id`: 供应商用户 ID

## 故障排查

### 如果没有解析到 email

1. 检查日志中的 JWT payload 内容
2. 确认 payload 中是否包含 email 字段
3. 检查字段名是否匹配（可能是 `email`, `mail`, `emailAddress` 等）

### 如果解析失败

1. 检查 token 格式是否正确（应该有 3 个部分）
2. 查看 base64 解码是否成功
3. 检查 JSON 解析错误信息
