# kiro-rs

一个用 Rust 编写的 Anthropic Claude API 兼容代理服务，将 Anthropic API 请求转换为 Kiro API 请求。

---

<table>
<tr>
<td>
<b>特别感谢</b>：<a href="https://co.yes.vg/register?ref=hank9999">YesCode</a> 为本项目提供了 AI API 额度赞助, YesCode 作为一家低调务实的 AI API 中转服务商 <br>
长期以来提供稳定高可用的服务, 如您有意体验, 请点击链接注册体验 → <a href="https://co.yes.vg/register?ref=hank9999">立即访问</a>
</td>
</tr>
</table>

---

#### [LINUX DO 讨论帖](https://linux.do/t/topic/1571986)

## 免责声明

本项目仅供研究使用, Use at your own risk, 使用本项目所导致的任何后果由使用人承担, 与本项目无关。
本项目与 AWS/KIRO/Anthropic/Claude 等官方无关, 本项目不代表官方立场。

## 注意！

因 TLS 默认从 native-tls 切换至 rustls，你可能需要专门安装证书后才能配置 HTTP 代理。可通过 `config.json` 的 `tlsBackend` 切回 `native-tls`。
如果遇到请求报错, 尤其是无法刷新 token, 或者是直接返回 error request, 请尝试切换 tls 后端为 `native-tls`, 一般即可解决。

**Write Failed/会话卡死**: 如果遇到持续的 Write File / Write Failed 并导致会话不可用，参考 Issue [#22](https://github.com/hank9999/kiro.rs/issues/22) 和 [#49](https://github.com/hank9999/kiro.rs/issues/49) 的说明与临时解决方案（通常与输出过长被截断有关，可尝试调低输出相关 token 上限）

## 功能特性

- **Anthropic API 兼容**: 完整支持 Anthropic Claude API 格式
- **流式响应**: 支持 SSE (Server-Sent Events) 流式输出
- **Token 自动刷新**: 自动管理和刷新 OAuth Token
- **多凭据支持**: 支持配置多个凭据，按优先级自动故障转移
- **负载均衡**: 支持 `round_robin`（轮询，默认）、`priority`（按优先级）和 `balanced`（按历史成功次数均衡）三种模式
- **智能重试**: 单凭据最多重试 3 次，单请求最多重试 9 次
- **凭据回写**: 多凭据格式下自动回写刷新后的 Token
- **Thinking 模式**: 支持 Claude 的 extended thinking 功能
- **工具调用**: 完整支持 function calling / tool use
- **WebSearch**: 内置 WebSearch 工具转换逻辑
- **多模型支持**: 支持 Sonnet、Opus、Haiku 系列模型
- **Grok Build / xAI**: 提供独立的 `/grok` Anthropic 兼容接口，可调用 Grok 4.5、xAI API Token 与 Grok CLI OAuth
- **Admin 管理**: 可选的 Web 管理界面和 API，支持凭据管理、余额查询等
- **多级 Region 配置**: 支持全局和凭据级别的 Auth Region / API Region 配置
- **凭据级代理**: 支持为每个凭据单独配置 HTTP/SOCKS5 代理，优先级：凭据代理 > 全局代理 > 无代理

---

- [开始](#开始)
  - [1. 编译](#1-编译)
  - [2. 最小配置](#2-最小配置)
  - [3. 启动](#3-启动)
  - [4. 验证](#4-验证)
  - [Docker](#docker)
- [配置详解](#配置详解)
  - [config.json](#configjson)
  - [credentials.json](#credentialsjson)
  - [grok_credentials.json](#grok_credentialsjson)
  - [Region 配置](#region-配置)
  - [代理配置](#代理配置)
  - [认证方式](#认证方式)
  - [环境变量](#环境变量)
- [API 端点](#api-端点)
  - [标准端点 (/v1)](#标准端点-v1)
  - [Claude Code 兼容端点 (/cc/v1)](#claude-code-兼容端点-ccv1)
  - [Grok Build 端点 (/grok)](#grok-build-端点-grok)
  - [Thinking 模式](#thinking-模式)
  - [工具调用](#工具调用)
- [模型映射](#模型映射)
- [Admin（可选）](#admin可选)
- [注意事项](#注意事项)
- [项目结构](#项目结构)
- [技术栈](#技术栈)
- [License](#license)
- [致谢](#致谢)

## 开始

### 1. 编译

> PS: 如果不想编辑可以直接前往 Release 下载二进制文件

> **前置步骤**：编译前需要先构建前端 Admin UI（用于嵌入到二进制中）：
> ```bash
> cd admin-ui && pnpm install && pnpm build
> ```

```bash
cargo build --release
```

### 2. 最小配置

创建 `config.json`：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1"
}
```
> PS: 如果你需要 Web 管理面板, 请注意配置 `adminApiKey`

创建 `credentials.json`（从 Kiro IDE 等中获取凭证信息）：
> PS: 可以前往 Web 管理面板配置跳过本步骤
> 如果你对凭据地域有疑惑, 请查看 [Region 配置](#region-配置)

Social 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social"
}
```

IdC 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "idc",
   "clientId": "你的clientId",
   "clientSecret": "你的clientSecret",
   "profileArn": "arn:aws:codewhisperer:us-east-1:111112222233:profile/QWER1QAZSDFGH"
}
```

### 3. 启动

```bash
./target/release/kiro-rs
```

或指定配置文件路径：

```bash
./target/release/kiro-rs -c /path/to/config.json \
  --credentials /path/to/credentials.json \
  --grok-credentials /path/to/grok_credentials.json
```

### 4. 验证

```bash
curl http://127.0.0.1:8990/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-qazWSXedcRFV123456" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      {"role": "user", "content": "Hello, Claude!"}
    ]
  }'
```

### Docker

也可以通过 Docker 启动：

```bash
docker-compose up
```

需要将 `config.json`、`credentials.json`（使用原 Kiro 路由时）以及
`grok_credentials.json`（使用 `/grok` 时）挂载到容器中，具体参见 `docker-compose.yml`。

## 配置详解

### config.json

| 字段 | 类型 | 默认值 | 描述 |
|------|------|--------|------|
| `host` | string | `127.0.0.1` | 服务监听地址 |
| `port` | number | `8080` | 服务监听端口 |
| `apiKey` | string | - | 自定义 API Key（用于客户端认证，必配） |
| `region` | string | `us-east-1` | AWS 区域 |
| `authRegion` | string | - | Auth Region（用于 Token 刷新），未配置时回退到 region |
| `apiRegion` | string | - | API Region（用于 API 请求），未配置时回退到 region |
| `kiroVersion` | string | `0.9.2` | Kiro 版本号 |
| `machineId` | string | - | 自定义机器码（64位十六进制），不定义则自动生成 |
| `systemVersion` | string | 随机 | 系统版本标识 |
| `nodeVersion` | string | `22.21.1` | Node.js 版本标识 |
| `tlsBackend` | string | `rustls` | TLS 后端：`rustls` 或 `native-tls` |
| `countTokensApiUrl` | string | - | 外部 count_tokens API 地址 |
| `countTokensApiKey` | string | - | 外部 count_tokens API 密钥 |
| `countTokensAuthType` | string | `x-api-key` | 外部 API 认证类型：`x-api-key` 或 `bearer` |
| `proxyUrl` | string | - | HTTP/SOCKS5 代理地址 |
| `proxyUsername` | string | - | 代理用户名 |
| `proxyPassword` | string | - | 代理密码 |
| `adminApiKey` | string | - | Admin API 密钥，配置后启用凭据管理 API 和 Web 管理界面 |
| `loadBalancingMode` | string | `round_robin` | 负载均衡模式：`round_robin`（轮询）、`priority`（按优先级）或 `balanced`（按历史成功次数均衡） |
| `extractThinking` | boolean | `true` | 非流式响应的 thinking 块提取。启用后 `<thinking>` 标签会被解析为独立的 `thinking` 内容块 |
| `grokDefaultModel` | string | `grok-4.5` | `/grok` 路由的默认 Grok Build 模型；请求使用 Claude 别名或 `grok-build` 时会映射到此模型 |
| `grokBaseUrl` | string | `https://api.x.ai/v1` | Grok Build / xAI 默认上游根地址；真实模型目录中的 `baseUrl` 可按模型覆盖它 |
| `defaultEndpoint` | string | `ide` | 默认 Kiro 端点。凭据未显式指定 `endpoint` 时使用。当前支持：`ide` |

完整配置示例：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1",
   "tlsBackend": "rustls",
   "kiroVersion": "0.9.2",
   "machineId": "64位十六进制机器码",
   "systemVersion": "darwin#24.6.0",
   "nodeVersion": "22.21.1",
   "authRegion": "us-east-1",
   "apiRegion": "us-east-1",
   "countTokensApiUrl": "https://api.example.com/v1/messages/count_tokens",
   "countTokensApiKey": "sk-your-count-tokens-api-key",
   "countTokensAuthType": "x-api-key",
   "proxyUrl": "http://127.0.0.1:7890",
   "proxyUsername": "user",
   "proxyPassword": "pass",
   "adminApiKey": "sk-admin-your-secret-key",
   "loadBalancingMode": "round_robin",
   "extractThinking": true,
   "grokDefaultModel": "grok-4.5",
   "grokBaseUrl": "https://api.x.ai/v1"
}
```

### credentials.json

支持单对象格式（向后兼容）或数组格式（多凭据）。

#### 字段说明

| 字段             | 类型     | 描述                                          |
|----------------|--------|---------------------------------------------|
| `id`           | number | 凭据唯一 ID（可选，仅用于 Admin API 管理；手写文件可不填）        |
| `name`         | string | 凭据显示名称（可选；导入时可自动由用户名/邮箱和 ID 生成）             |
| `accessToken`  | string | OAuth 访问令牌（可选，可自动刷新）                        |
| `refreshToken` | string | OAuth 刷新令牌                                  |
| `profileArn`   | string | AWS Profile ARN（OAuth 凭据会在刷新/导入查询时尽量自动保存）    |
| `importedAt`   | string | 凭据导入时间 (RFC3339)                            |
| `expiresAt`    | string | Token 过期时间 (RFC3339)                        |
| `authMethod`   | string | 认证方式：`social` 或 `idc`                       |
| `clientId`     | string | IdC 登录的客户端 ID（IdC 认证必填）                     |
| `clientSecret` | string | IdC 登录的客户端密钥（IdC 认证必填）                      |
| `priority`     | number | 凭据优先级，数字越小越优先，默认为 0                         |
| `region`       | string | 凭据级 Auth Region, 兼容字段                       |
| `authRegion`   | string | 凭据级 Auth Region，用于 Token 刷新, 未配置时回退到 region |
| `apiRegion`    | string | 凭据级 API Region，用于 API 请求                    |
| `machineId`    | string | 凭据级机器码（64位十六进制）                             |
| `email`        | string | 用户邮箱（可选，从 API 获取）                           |
| `userName`     | string | 上游用户名（可选，通常为邮箱或账号名）                         |
| `proxyUrl`     | string | 凭据级代理 URL（可选，特殊值 `direct` 表示不使用代理）       |
| `proxyUsername`| string | 凭据级代理用户名（可选）                                |
| `proxyPassword`| string | 凭据级代理密码（可选）                                 |
| `endpoint`     | string | 凭据级端点名称（可选，未配置时使用 `config.defaultEndpoint`）|

说明：
- IdC / Builder-ID / IAM 在本项目里属于同一种登录方式，配置时统一使用 `authMethod: "idc"`
- 为兼容旧配置，`builder-id` / `iam` 仍可被识别，但会按 `idc` 处理
- 如果 Token 刷新响应没有返回 `profileArn`，请在新增或导入凭据时手动提供；缺少 `profileArn` 的 OAuth 凭据不会被保存

#### 单凭据格式（旧格式，向后兼容）

```json
{
   "accessToken": "请求token，一般有效期一小时，可选",
   "refreshToken": "刷新token，一般有效期7-30天不等",
   "profileArn": "arn:aws:codewhisperer:us-east-1:111112222233:profile/QWER1QAZSDFGH",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social",
   "clientId": "IdC 登录需要",
   "clientSecret": "IdC 登录需要"
}
```

#### 多凭据格式（支持故障转移和自动回写）

```json
[
   {
      "refreshToken": "第一个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 0
   },
   {
      "refreshToken": "第二个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "idc",
      "clientId": "xxxxxxxxx",
      "clientSecret": "xxxxxxxxx",
      "region": "us-east-2",
      "priority": 1,
      "proxyUrl": "socks5://proxy.example.com:1080",
      "proxyUsername": "user",
      "proxyPassword": "pass"
   },
   {
      "refreshToken": "第三个凭据（显式不走代理）",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 2,
      "proxyUrl": "direct"
   }
]
```

多凭据特性：
- 按 `priority` 字段排序，数字越小优先级越高（默认为 0）
- 默认 `round_robin` 模式会按凭据列表顺序轮询所有未禁用凭据；请求失败后的下一次重试会继续轮询到后续凭据
- 单凭据最多重试 3 次，单请求最多重试 9 次
- 自动故障转移到下一个可用凭据
- 多凭据格式下 Token 刷新后自动回写到源文件

### grok_credentials.json

`/grok` 与原 Kiro 路由使用**完全独立**的凭据池，默认读取当前目录的
`grok_credentials.json`，也可通过 `--grok-credentials` 指定路径。支持单对象或数组格式。

直接使用 xAI API Token：

```json
[
  {
    "name": "xAI API Token",
    "accessToken": "xai-your-api-token",
    "authMethod": "token",
    "priority": 0,
    "pools": ["default"]
  }
]
```

也可直接导入 AIClient-2-API 保存的 Grok CLI OAuth 文件；`access_token`、
`refresh_token`、`id_token`、`expired`、`sub`、`base_url`、`token_endpoint`
等 snake_case 字段会自动识别，并在到期前自动刷新：

```json
{
  "access_token": "oauth-access-token",
  "refresh_token": "oauth-refresh-token",
  "id_token": "optional-id-token",
  "token_type": "Bearer",
  "expired": "2030-01-01T00:00:00Z",
  "auth_kind": "oauth",
  "base_url": "https://api.x.ai/v1",
  "token_endpoint": "https://auth.x.ai/oauth/token"
}
```

配置了 `adminApiKey` 后，可访问 `/grok/admin`，点击 **Grok OAuth 授权** 完成
xAI Grok CLI OAuth + PKCE 登录；成功的凭据会写入 `grok_credentials.json`。OAuth
回调固定为 `http://127.0.0.1:56121/callback`，因此授权浏览器必须和运行本服务的主机
处于同一网络命名空间。也可直接调用 `POST /grok/api/admin/oauth/start`，轮询
`GET /grok/api/admin/oauth/status/:state`。

按 Grok Build 的路由规则，直接 xAI API Token 默认请求
`https://api.x.ai/v1`；本服务创建的 OAuth session 凭据默认请求
`https://cli-chat-proxy.grok.com/v1` 并附带 CLI 所需的认证标识头。导入
AIClient-2-API 文件时若其中已有 `base_url`，会原样保留。

### Region 配置

支持多级 Region 配置，分别控制 Token 刷新和 API 请求使用的区域。

**Auth Region**（Token 刷新）优先级：
`凭据.authRegion` > `凭据.region` > `config.authRegion` > `config.region`

**API Region**（API 请求）优先级：
`凭据.apiRegion` > `config.apiRegion` > `config.region`

### 代理配置

支持全局代理和凭据级代理，凭据级代理会覆盖该凭据产生的所有出站连接（API 请求、Token 刷新、额度查询）。

**代理优先级**：`凭据.proxyUrl` > `config.proxyUrl` > 无代理

| 凭据 `proxyUrl` 值 | 行为 |
|---|---|
| 具体 URL（如 `http://proxy:8080`、`socks5://proxy:1080`） | 使用凭据指定的代理 |
| `direct` | 显式不使用代理（即使全局配置了代理） |
| 未配置（留空） | 回退到全局代理配置 |

凭据级代理示例：

```json
[
   {
      "refreshToken": "凭据A：使用自己的代理",
      "authMethod": "social",
      "proxyUrl": "socks5://proxy-a.example.com:1080",
      "proxyUsername": "user_a",
      "proxyPassword": "pass_a"
   },
   {
      "refreshToken": "凭据B：显式不走代理（直连）",
      "authMethod": "social",
      "proxyUrl": "direct"
   },
   {
      "refreshToken": "凭据C：使用全局代理（或直连，取决于 config.json）",
      "authMethod": "social"
   }
]
```

### 认证方式

客户端请求本服务时，支持两种认证方式：

1. **x-api-key Header**
   ```
   x-api-key: sk-your-api-key
   ```

2. **Authorization Bearer**
   ```
   Authorization: Bearer sk-your-api-key
   ```

### 环境变量

可通过环境变量配置日志级别：

```bash
RUST_LOG=debug ./target/release/kiro-rs
```

## API 端点

### 标准端点 (/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/v1/models` | GET | 获取可用模型列表 |
| `/v1/messages` | POST | 创建消息（对话） |
| `/v1/messages/count_tokens` | POST | 估算 Token 数量 |

### Claude Code 兼容端点 (/cc/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/cc/v1/messages` | POST | 创建消息（缓冲模式，确保 `input_tokens` 准确） |
| `/cc/v1/messages/count_tokens` | POST | 估算 Token 数量（与 `/v1` 相同） |

> **`/cc/v1/messages` 与 `/v1/messages` 的区别**：
> - `/v1/messages`：实时流式返回，`message_start` 中的 `input_tokens` 是估算值
> - `/cc/v1/messages`：缓冲模式，等待上游流完成后，用从 `contextUsageEvent` 计算的准确 `input_tokens` 更正 `message_start`，然后一次性返回所有事件
> - 等待期间会每 25 秒发送 `ping` 事件保活

### Grok Build 端点 (/grok)

`/grok` 下的接口保持与根路径相同的 Anthropic 兼容请求格式和客户端 `apiKey` 认证。
启动时及每 10 分钟会按 **每张 Grok 凭据** 拉取 xAI `/v1/models`；目录中的模型 ID、
`baseUrl`、`apiBackend`、以及 reasoning effort 菜单决定实际请求如何转换和路由。目录
暂不可达不会禁用该推理凭据，并会保留上一次成功的目录。

| 端点 | 方法 | 描述 |
|------|------|------|
| `/grok/v1/models` | GET | 返回所有已加载凭据目录的模型并集；目录未加载时返回 bootstrap 清单 |
| `/grok/v1/messages` | POST | Anthropic Messages → catalog 指定的 xAI Responses / Chat Completions / Messages，支持流式、工具调用、图片与 thinking |
| `/grok/v1/messages/count_tokens` | POST | 估算请求 Token 数量 |
| `/grok/v1/files` | GET, POST | Anthropic Files API：列出本代理上传的文件，或以 multipart 上传文件 |
| `/grok/v1/files/{file_id}` | GET, DELETE | 查询元数据或删除文件 |
| `/grok/v1/files/{file_id}/content` | GET | 路由已兼容；调用方上传的文件为 `downloadable=false`，因此返回 400 |
| `/grok/v1/images/generations` | POST | Grok Build Imagine 文生图扩展；返回 xAI `data[].b64_json` |
| `/grok/v1/images/edits` | POST | Grok Build Imagine 图像编辑扩展，支持一张或多张参考图 |
| `/grok/v1/videos/generations` | POST | Grok Build image-to-video / reference-to-video 扩展，返回可轮询任务 ID |
| `/grok/v1/videos/{request_id}` | GET | 轮询视频生成状态；完成时返回 xAI `video.url` |
| `/grok/cc/v1/messages` | POST | Claude Code 兼容路径 |
| `/grok/cc/v1/messages/count_tokens` | POST | Token 估算 |
| `/grok/cc/v1/files*` | GET, POST, DELETE | 与 `/grok/v1/files*` 使用同一存储的 Claude Code 路径别名 |

请求模型名为 `claude-*`、`grok-build` 或为空时，会使用 `grokDefaultModel`；其他模型会
按已加载 catalog 的实际 wire model ID、显示名或唯一简写规范化（例如 catalog 中唯一的
`grok-composer-2.5-fast` 可用 `composer2.5` 选择）。实际调用时还会再按单凭据目录过滤，
不会把并集里存在的模型发给没有授权的账号。`/grok` 与根路径共用客户端 API Key 和 API
Key 的资源池授权规则，但不会共享 Kiro 或 xAI 的实际凭据。

#### 模型目录生命周期与路由

| 阶段 | `/grok/v1/models` 展示 | Messages 转换用的 catalog | 说明 |
|------|------------------------|---------------------------|------|
| 启动且尚无成功拉取 | bootstrap 清单（默认 `apiBackend=responses`） | bootstrap / 无目录时按 Responses 兼容路径 | 仅过渡窗口，**不代表**账号一定有权 |
| 已拉取 ≥1 张凭据目录 | 各启用凭据目录的**并集** | **先选凭据，再按该凭据自己的目录 convert** | 并集只用于别名解析与列表；wire `apiBackend` 以单凭据为准 |
| 某凭据 `/models` 失败 | 保留该凭据旧目录（若有） | 同上；无目录的凭据在选号时“未知放行” | 控制平面故障不禁用推理凭据 |
| 定时刷新（约 10 分钟） | 更新并集 | 下次请求起生效 | 刷新失败沿用旧目录 |

上游 `/v1/models` 未声明 `apiBackend` 时，与 Grok Build 一致默认 **`chat_completions`**。
因此 catalog 就绪后，原先 bootstrap 下可用的 hosted Web Search / Files 路径，可能变为
需要显式选择 `apiBackend=responses` 的模型——这是预期行为，不是回归。

多账号异构时（例如 OAuth CLI 为 `responses`、API token 为 `chat_completions`）：

1. 用并集 catalog 解析模型别名与 effort；
2. 按模型 / effort / pool /（WebSearch→Responses）选出一张路由凭据；
3. **仅用该凭据 catalog** 构建 Responses / Chat Completions / Messages 请求体；
4. `call_api` 再按同样过滤条件选发送凭据（可 failover 到同 backend 的其它账号）。

并集目录在合并异构 `apiBackend` 时优先展示 `responses`（便于发现 Web Search / Files 能力），
但**不会**把 Chat-only 账号误建成 Responses body——最终 body 始终跟单凭据 catalog 走。

#### Messages 字段支持矩阵

| Anthropic / 客户端字段 | `/grok` 处理 |
|------------------------|--------------|
| `model`（含 `claude-*` / `grok-build` 别名） | 规范化后按 catalog 路由 |
| `messages` / `system` | 转为目标 backend 的 input / messages / 透传 |
| `max_tokens` | → `max_output_tokens` / `max_completion_tokens` / `max_tokens` |
| `stream` | 支持；Responses/Chat 上游统一拉 SSE 再聚合成 Anthropic 流或 JSON |
| `tools` / `tool_choice` | function 与 hosted `web_search`（见下） |
| `thinking` / `output_config.effort` | → Responses `reasoning.effort` / Chat `reasoning_effort` / Messages adaptive（见下节 summary 语义） |
| `metadata.user_id` | → Responses `prompt_cache_key`（会话/缓存键）；并参与多账号 session 亲和 |
| `temperature` / `top_p` | 有值时透传到 Responses / Chat Completions body，以及 Messages backend（`serde` 原样保留）；省略则用上游默认 |
| `top_k` / `stop_sequences` | **当前未透传** |
| `source.type=file`（image/document） | 仅 `responses` backend；须先 `/grok/v1/files` 上传 |
| 多轮 `thinking` + `signature` | Responses 请求 `include: reasoning.encrypted_content`；把完整 reasoning items 打包进 `thinking.signature`（`xai-rs2.*` HMAC）。Claude Code 原样回传后展开为 Responses reasoning sibling；无包/凭据不匹配时回退为 thinking 文本 |

#### reasoning summary 语义（与 Grok Build 对齐，保持现状）

对于 catalog 为 `responses` 的模型，代理**始终**按 Grok Build sampler 发送：

```json
"reasoning": { "summary": "concise" }
```

并把 Anthropic `thinking` 或 `output_config.effort` 映射为 `reasoning.effort`（未声明 effort
时只省略 `effort` 字段，**不**省略 `summary`）。因此：

1. **无需**再依赖模型名 `-thinking` 后缀；
2. 客户端**未**开启 `thinking` / `output_config.effort` 时，上游仍可能产生 reasoning
   summary 与对应 token 消耗；
3. 代理仅在客户端声明了 thinking/effort（`thinking_enabled=true`）时，才把上游
   reasoning 转为 Anthropic `thinking` 内容块并打包 `signature`；否则 summary
   **不会**出现在 Anthropic 响应中。

这是有意对齐 Grok Build 的 wire 行为，而不是按 Anthropic「未请求则不思考」语义做裁剪。
`xhigh` 会原样保留；若服务端给该模型提供了明确的 `reasoningEfforts` 菜单，则只接受菜单中声明的值。

#### Web Search

`/grok/v1/messages` 与 `/grok/cc/v1/messages` 接受 Anthropic 的 Web Search
tool，也会识别 Claude Code 注册的普通 `WebSearch` / `web_search` function。
它们都会按 Grok Build 的 Responses hosted-tool 形状转发，而不是继续作为
普通 function 交给 xAI 生成客户端工具参数：

```json
{
  "model": "grok-4.5",
  "max_tokens": 2048,
  "messages": [{"role": "user", "content": "查一下 Rust 最新稳定版"}],
  "tools": [{
    "type": "web_search_20250305",
    "name": "web_search",
    "allowed_domains": ["blog.rust-lang.org", "doc.rust-lang.org"]
  }]
}
```

上游实际得到 `{"type":"web_search","filters":{"allowed_domains":[...]}}`；若显式 hosted
tool 与普通 `WebSearch` / `web_search` function 同时存在，和 Grok Build 一样由显式 hosted
Web Search 优先，避免 xAI 的重复工具名错误。指定 `tool_choice` 为这两个普通函数名时，也会改写为
`{"type":"web_search"}`。

带 Web Search 的请求只允许使用 catalog 为 `responses` 的模型。流式与非流式响应都会将 xAI
`web_search_call` 转换为配对的 Anthropic `server_tool_use` 和 `web_search_tool_result`
内容块，使用 `tool_use_id` 关联二者，保留 query、来源 URL、标题和摘要，并在
`usage.server_tool_use.web_search_requests` 中计数。
真实 catalog 已加载时，明确声明 `supportsBackendSearch: false` 的凭据会被排除；声明为 `true`
或未返回该字段的 `responses` 凭据均可尝试，字段缺失时由 xAI 上游最终裁决。多个凭据的模型并集
即使包含该模型，也不会把带搜索的请求负载到明确不支持搜索的账号。`chat_completions`
与 `messages` backend 都没有本代理所需的 xAI Responses hosted-tools 通道，因此这些组合会明确
返回 400，需改用 catalog 标记为 `responses` 的模型，而不会悄悄降级成普通 function。
`max_uses` 会被接受以兼容 Anthropic 请求，但 xAI Responses 没有对应 wire 字段，实际调用次数由
xAI 的 hosted-tool sampler 决定。

#### Anthropic Files API（`source.type: "file"`）

`/grok/v1/files` 兼容 Anthropic Files API 的上传、列出、查询和删除流程；标准
Anthropic SDK 发送的 `anthropic-beta: files-api-2025-04-14` 会被接受。文件字节直接上传到
xAI `/v1/files`，代理只保存 `file_id → 创建 xAI 凭据` 的绑定，因此轮询凭据池时仍能回到
正确的 xAI 账号。`/grok/cc/v1/files` 是相同存储的别名，供 Claude Code 将 base URL 指向
`/grok/cc` 时使用。

```bash
# 上传：单个 multipart file 字段，最大 50 MiB
curl -X POST http://127.0.0.1:8080/grok/v1/files \
  -H 'x-api-key: <apiKey>' \
  -H 'anthropic-beta: files-api-2025-04-14' \
  -F 'file=@./architecture.pdf;type=application/pdf'

# 结果包含 Anthropic 风格的 {"id":"file_...","type":"file",...}
curl -H 'x-api-key: <apiKey>' \
  http://127.0.0.1:8080/grok/v1/files
```

把上传返回的 `id` 放入标准 Anthropic Messages content block 即可。`image` 和 `document`
均支持，代理会分别统一映射为 xAI Responses 的 `input_file.file_id`；xAI 会自动进行文件/文档
搜索和推理，不会把文件字节塞进 prompt。

```json
{
  "model": "grok-4.5",
  "max_tokens": 2048,
  "messages": [{
    "role": "user",
    "content": [
      {"type": "image", "source": {"type": "file", "file_id": "file_image"}},
      {"type": "document", "source": {"type": "file", "file_id": "file_report"}},
      {"type": "text", "text": "比较图片与报告中的结论"}
    ]
  }]
}
```

有几个与多凭据和上游协议相关的边界：

- 文件输入只适用于 catalog 标记为 `responses` 的模型；`chat_completions` 和 `messages` backend
  会在请求前返回 400，而不是发送无效 payload。
- 一个 Messages 请求中的全部 `file_id` 必须来自同一张 Grok 凭据；若需要跨账号文件，请分开请求或用
  同一凭据重新上传。
- 默认在进程工作目录保存 `grok_file_bindings.json`，其中只有文件元数据和凭据/资源池绑定、不含文件
  字节；可用 `GROK_FILE_BINDINGS_PATH` 改到持久卷。删除这个注册表或直接绕过代理上传的 xAI 文件，代理
  无法安全判断应使用哪张凭据，因而不能在 `/grok` Messages 中引用。
- `GET /files` 只列出经本代理上传、且当前 API Key 资源池可访问的文件。调用方上传的文件与 Anthropic
  语义一致为 `downloadable=false`，所以 `GET /files/{file_id}/content` 返回 400；模型生成文件的注册/下载
  尚未实现。

协议细节分别可参照 [Anthropic Files API](https://platform.claude.com/docs/en/build-with-claude/files)
与 [xAI Chat with Files](https://docs.x.ai/developers/model-capabilities/files/chat-with-files)。

#### 图片输入与 Imagine 图片/视频生成

标准 Anthropic 消息中的图片输入可直接使用，既支持 base64 source：

```json
{"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}
```

也支持 URL source `{"type":"image","source":{"type":"url","url":"https://..."}}`；同时兼容常见的
OpenAI 形状 `{"type":"image_url","image_url":{"url":"..."}}`。它们会被转换为 xAI Responses 的
`input_image`（或 Chat Completions 的 `image_url`），不会重写 HTTPS URL。

Anthropic Messages 本身没有统一的“生成图片/视频”输出块。Grok Build 也不是通过 Responses
生成媒体，而是以本地 `image_gen`、`image_edit`、`image_to_video`、`reference_to_video` 工具直连
xAI 的 `/images/*`、`/videos/*`。因此 `/grok` 保持 Messages 的 Anthropic 兼容性，同时提供下列明确的
Build-style 扩展端点：

```bash
# 文生图：固定使用 Grok Build 默认的 grok-imagine-image-quality、n=1、1k、b64_json
curl -X POST http://127.0.0.1:8080/grok/v1/images/generations \
  -H 'x-api-key: <apiKey>' -H 'content-type: application/json' \
  -d '{"prompt":"a capybara astronaut","aspect_ratio":"16:9"}'

# 图像编辑：image 是一张或多张参考图；单图映射 xAI image，多图映射 images
curl -X POST http://127.0.0.1:8080/grok/v1/images/edits \
  -H 'x-api-key: <apiKey>' -H 'content-type: application/json' \
  -d '{"prompt":"turn this into watercolor","image":["data:image/png;base64,..."]}'

# 单图生成视频：对应 Grok Build image_to_video
curl -X POST http://127.0.0.1:8080/grok/v1/videos/generations \
  -H 'x-api-key: <apiKey>' -H 'content-type: application/json' \
  -d '{"image":"https://example.com/frame.png","prompt":"slow camera push-in","duration":6,"resolution_name":"480p"}'

# 多参考图生成视频：对应 Grok Build reference_to_video（images 必须为 2 到 7 张）
curl -X POST http://127.0.0.1:8080/grok/v1/videos/generations \
  -H 'x-api-key: <apiKey>' -H 'content-type: application/json' \
  -d '{"prompt":"cinematic transition","images":["https://example.com/a.png","https://example.com/b.png"],"aspect_ratio":"16:9","duration":10,"resolution_name":"720p"}'
```

单图视频使用 `grok-imagine-video-1.5-preview`，多参考图视频使用
`grok-imagine-video`；`duration` 只允许 `6` 或 `10`，`resolution_name` 只允许 `480p` 或 `720p`。
视频创建响应中的 `request_id` 是代理生成的 opaque ID，需用 `GET /grok/v1/videos/{request_id}`
轮询；成功时其中的 `video.url` 是 xAI 返回的可下载 URL。Grok Build 在本地会把图片/视频保存到 session
目录，而 HTTP 代理不能写入远程调用方的文件系统，因此保留 base64 图片和视频 URL 原样返回。调用方应和
Grok Build 一样每 5 秒轮询一次、最长等待 300 秒；opaque ID 只保存在本进程内，服务重启或创建 1 小时后会
失效并返回 404。

`/images/edits` 的 `image` 只接受 `data:image/...;base64,...`：Grok Build 会把它的本地参考图解码、压缩后
转换为这种形式，代理不会错误读取调用方机器路径或下载任意 HTTPS URL。视频的 `image` / `images` 则接受
`https://` URL 或 `data:image/...;base64,...`。视频输入上传也未实现：Grok Build 的实际能力是**图片生成视频**
和**参考图生成视频**，不是上传视频给对话模型。

媒体端点和 Grok Build 一样直连公共 xAI API 基址（`grokBaseUrl`），即使推理使用 OAuth 且 Messages
走 CLI chat proxy 也是如此；凭据池、API Key 资源池限制和 OAuth 自动刷新仍与 `/grok/v1/messages` 共用。

### Thinking 模式

支持 Claude 的 extended thinking 功能：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 16000,
  "thinking": {
    "type": "enabled",
    "budget_tokens": 10000
  },
  "messages": [...]
}
```

### 工具调用

完整支持 Anthropic 的 tool use 功能：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 1024,
  "tools": [
    {
      "name": "get_weather",
      "description": "获取指定城市的天气",
      "input_schema": {
        "type": "object",
        "properties": {
          "city": {"type": "string"}
        },
        "required": ["city"]
      }
    }
  ],
  "messages": [...]
}
```

## 模型映射

| Anthropic 模型 | Kiro 模型 |
|----------------|-----------|
| `*sonnet*` | `claude-sonnet-4.5` |
| `*opus*`（含 4.5/4-5） | `claude-opus-4.5` |
| `*opus*`（其他） | `claude-opus-4.6` |
| `*haiku*` | `claude-haiku-4.5` |

## Admin（可选）

当 `config.json` 配置了非空 `adminApiKey` 时，会启用：

- **Admin API（认证同 API Key）**
  - `GET /api/admin/credentials` - 获取所有凭据状态
  - `POST /api/admin/credentials` - 添加新凭据
  - `DELETE /api/admin/credentials/:id` - 删除凭据
  - `POST /api/admin/credentials/:id/disabled` - 设置凭据禁用状态
  - `POST /api/admin/credentials/:id/priority` - 设置凭据优先级
  - `POST /api/admin/credentials/:id/reset` - 重置失败计数
  - `GET /api/admin/credentials/:id/balance` - 获取凭据余额

- **Admin UI**
  - `GET /admin` - 访问管理页面（需要在编译前构建 `admin-ui/dist`）

- **Grok Build Admin（使用相同的 `adminApiKey`，但管理独立 Grok 凭据池）**
  - `GET/POST /grok/api/admin/credentials` - 查询或导入 xAI Token / OAuth 凭据
  - `POST /grok/api/admin/credentials/:id/verify` - 调用 xAI `/models` 校验凭据
  - `GET /grok/api/admin/credentials/:id/catalog?refresh=true` - 查看或强制刷新该凭据的真实模型、backend 与 effort 菜单
  - `GET /grok/api/admin/credentials/:id/balance` - OAuth 凭据查询 Grok CLI billing；API Token 返回 `/models` 验活结果
  - `POST /grok/api/admin/oauth/start` - 发起 Grok CLI OAuth + PKCE
  - `GET /grok/api/admin/oauth/status/:state` - 查询授权状态
  - `GET /grok/admin` - Grok 管理页面（与 `/admin` 使用同一构建产物）

## 注意事项

1. **凭证安全**: 请妥善保管 `credentials.json` 文件，不要提交到版本控制
2. **Token 刷新**: 服务会自动刷新过期的 Token，无需手动干预
3. **WebSearch 工具**: 根路径 `/v1/messages` 保留原有的单 WebSearch 工具转换逻辑；`/grok/v1/messages`
   支持与普通工具并存的 xAI hosted Web Search，具体 wire 映射见上文

## 项目结构

```
kiro-rs/
├── src/
│   ├── main.rs                 # 程序入口
│   ├── http_client.rs          # HTTP 客户端构建
│   ├── token.rs                # Token 计算模块
│   ├── debug.rs                # 调试工具
│   ├── test.rs                 # 测试
│   ├── model/                  # 配置和参数模型
│   │   ├── config.rs           # 应用配置
│   │   └── arg.rs              # 命令行参数
│   ├── anthropic/              # Anthropic API 兼容层
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── middleware.rs       # 认证中间件
│   │   ├── types.rs            # 类型定义
│   │   ├── converter.rs        # 协议转换器
│   │   ├── stream.rs           # 流式响应处理
│   │   └── websearch.rs        # WebSearch 工具处理
│   ├── kiro/                   # Kiro API 客户端
│   │   ├── provider.rs         # API 提供者
│   │   ├── token_manager.rs    # Token 管理
│   │   ├── machine_id.rs       # 设备指纹生成
│   │   ├── model/              # 数据模型
│   │   │   ├── credentials.rs  # OAuth 凭证
│   │   │   ├── events/         # 响应事件类型
│   │   │   ├── requests/       # 请求类型
│   │   │   ├── common/         # 共享类型
│   │   │   ├── token_refresh.rs # Token 刷新模型
│   │   │   └── usage_limits.rs # 使用额度模型
│   │   └── parser/             # AWS Event Stream 解析器
│   │       ├── decoder.rs      # 流式解码器
│   │       ├── frame.rs        # 帧解析
│   │       ├── header.rs       # 头部解析
│   │       ├── error.rs        # 错误类型
│   │       └── crc.rs          # CRC 校验
│   ├── grok/                   # Grok Build / xAI catalog 与 OAuth
│   │   ├── model_catalog.rs     # per-credential xAI /v1/models 目录与能力索引
│   │   ├── converter.rs         # Anthropic → catalog 指定 backend 请求转换
│   │   ├── stream.rs            # Responses / Chat Completions SSE → Anthropic SSE
│   │   ├── token_manager.rs     # xAI Token/OAuth 凭据池
│   │   ├── provider.rs          # xAI 上游调用与故障转移
│   │   └── admin.rs             # /grok/api/admin 管理接口
│   ├── admin/                  # Admin API 模块
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── service.rs          # 业务逻辑服务
│   │   ├── types.rs            # 类型定义
│   │   ├── middleware.rs       # 认证中间件
│   │   └── error.rs            # 错误处理
│   ├── admin_ui/               # Admin UI 静态文件嵌入
│   │   └── router.rs           # 静态文件路由
│   └── common/                 # 公共模块
│       └── auth.rs             # 认证工具函数
├── admin-ui/                   # Admin UI 前端工程（构建产物会嵌入二进制）
├── tools/                      # 辅助工具
├── Cargo.toml                  # 项目配置
├── config.example.json         # 配置示例
├── docker-compose.yml          # Docker Compose 配置
└── Dockerfile                  # Docker 构建文件
```

## 技术栈

- **Web 框架**: [Axum](https://github.com/tokio-rs/axum) 0.8
- **异步运行时**: [Tokio](https://tokio.rs/)
- **HTTP 客户端**: [Reqwest](https://github.com/seanmonstar/reqwest)
- **序列化**: [Serde](https://serde.rs/)
- **日志**: [tracing](https://github.com/tokio-rs/tracing)
- **命令行**: [Clap](https://github.com/clap-rs/clap)

## License

MIT

## 致谢

本项目的实现离不开前辈的努力:  
 - [kiro2api](https://github.com/caidaoli/kiro2api)
 - [proxycast](https://github.com/aiclientproxy/proxycast)

本项目部分逻辑参考了以上的项目, 再次由衷的感谢!
