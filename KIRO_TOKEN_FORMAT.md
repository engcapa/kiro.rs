# Kiro Token 格式分析与用户信息显示方案

## 问题发现

在尝试通过 JWT 解析获取用户信息时，发现 Kiro 的 `access_token` **不是标准 JWT 格式**。

## Kiro Token 格式

### 实际格式
```
aoaAAAAAGnWHZMeulUcfH26igWiLNa7aCwy4GDEcab3M_F6C9NJWkwoQlRsDZBiBsm2JjRHkXD8xFQ87-sziG8Bt0Bkc0:MGQCMEc3PkTqDAWICJQG6ikjnhi68jQATCcTkL9azUJhJog6atH+zN8y6dReegTPtJ467gIwW86/9qkbdfKQI9QlMSbIWU3rVwBJJfif0YwLoLO6uyUXhU/7PO3UdznfDgJByMGk
```

### 格式特征
- **分隔符**: 使用 `:` (冒号)，不是 `.` (点号)
- **部分数量**: 2 个部分，不是标准 JWT 的 3 个部分
- **结构**: `prefix:signature`
  - 第一部分: `aoa` 前缀 + base64 编码数据
  - 第二部分: base64 编码数据（可能是签名）

### 标准 JWT 格式对比
```
标准 JWT: header.payload.signature (3 部分，用点号分隔)
Kiro Token: prefix:signature (2 部分，用冒号分隔)
```

## Token 类型

Kiro token 是 **Opaque Token**（不透明令牌）：
- 服务端通过数据库查询验证
- Token 本身不包含用户信息
- 无法通过解析获取 email/provider 等信息

## 正确的用户信息获取方案

### 当前实现（已完成且正确）

#### 1. 从 API 响应获取
```rust
// getUsageLimits API 返回
{
  "userInfo": {
    "userId": "d-9067c98495...",
    "email": null  // 可能为 null
  }
}
```

#### 2. 字段解析兼容性
```rust
// src/kiro/model/usage_limits.rs
pub fn resolve_account_info(&self) -> Option<AccountInfo> {
    // 支持多种字段名: userInfo, accountInfo, user_info 等
}
```

#### 3. Email 回退机制
```rust
// src/admin/service.rs
let email = usage.email()  // 1. 优先从 API 获取
    .or_else(|| credential.and_then(|c| c.email.clone()));  // 2. 回退到凭据配置
```

### 数据获取优先级

| 字段 | 来源 1 | 来源 2 | 来源 3 |
|------|--------|--------|--------|
| **userId** | API 响应 ✓ | - | - |
| **email** | API 响应 | 凭据配置 ✓ | None |
| **provider** | API 响应 | - | None |

## 用户配置 Email

### credentials.json 配置
```json
[
  {
    "id": 1,
    "accessToken": "...",
    "refreshToken": "...",
    "email": "user@example.com",  // ← 手动配置
    "name": "我的账户"
  }
]
```

### Admin API 配置
```bash
# 通过 API 设置 email
curl -X POST http://localhost:8990/admin/credentials/1/email \
  -H "Content-Type: application/json" \
  -d '{"email": "user@example.com"}'
```

## 前端显示策略

### 推荐实现
```typescript
interface BalanceResponse {
  id: number;
  email?: string;
  userId?: string;
  provider?: string;
}

function getUserDisplay(balance: BalanceResponse): string {
  // 1. 优先显示 email（如果配置了）
  if (balance.email) {
    return balance.email;
  }
  
  // 2. 显示 userId（总是可用）
  if (balance.userId) {
    const shortId = balance.userId.substring(0, 20);
    return `User ${shortId}...`;
  }
  
  // 3. 默认显示
  return "未知用户";
}

function getUserBadge(balance: BalanceResponse): React.ReactNode {
  return (
    <div className="user-info">
      {balance.provider && (
        <span className="provider-badge">{balance.provider}</span>
      )}
      <span className="user-id">{getUserDisplay(balance)}</span>
    </div>
  );
}
```

### UI 示例
```
┌─────────────────────────────────────┐
│ 凭据 #1                              │
│ ┌─────────────────────────────────┐ │
│ │ 👤 user@example.com             │ │  ← 如果配置了 email
│ │ 🔑 User d-9067c98495...         │ │  ← 或显示 userId
│ │ 📊 KIRO FREE                    │ │
│ │ 💰 133.48 / 550.0 Credits       │ │
│ └─────────────────────────────────┘ │
└─────────────────────────────────────┘
```

## API 响应示例

### getUsageLimits 响应
```json
{
  "id": 1,
  "subscriptionTitle": "KIRO FREE",
  "currentUsage": 133.48,
  "usageLimit": 550.0,
  "remaining": 416.52,
  "usagePercentage": 24.27,
  "nextResetAt": 1777593600.0,
  "email": "user@example.com",  // 如果配置了
  "userId": "d-9067c98495.94a81478-9081-704e-0722-8a7cbcb6b16f",
  "provider": null,
  "profileArn": "arn:aws:...",
  "authRegion": "us-east-1",
  "apiRegion": "us-east-1"
}
```

## 验证步骤

### 1. 启动服务
```bash
cargo run
```

**预期**：无 JWT 警告日志

### 2. 查询余额
```bash
curl http://localhost:8990/admin/balance/1 | jq '.'
```

**预期**：返回包含 `userId` 的响应

### 3. 配置 Email
在 `credentials.json` 中添加：
```json
{
  "email": "user@example.com"
}
```

**预期**：API 响应包含配置的 email

## 总结

### ✅ 已实现且正确
- userId 显示（从 API 获取）
- Email 回退机制（API → 配置）
- 字段名兼容性（userInfo/accountInfo）
- 完整的错误处理

### ❌ 不可行的方案
- JWT 解析（Kiro token 不是 JWT）
- Token 本地解析（Opaque token）

### 💡 推荐方案
1. **显示 userId** 作为主要标识（总是可用）
2. **手动配置 email** 在 credentials.json 中
3. **前端优雅降级** email → userId → "未知用户"

## 相关文件

### 核心实现
- `src/kiro/model/usage_limits.rs` - API 响应解析
- `src/admin/service.rs` - Email 回退逻辑
- `src/admin/types.rs` - API 类型定义

### 配置文件
- `credentials.json` - 凭据配置（包含 email）
- `config.json` - 服务配置

### 文档
- `README.md` - 项目文档
- `KIRO_TOKEN_FORMAT.md` - 本文档
