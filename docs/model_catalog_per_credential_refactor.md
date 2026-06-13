# Kiro-RS 模型目录按凭据隔离重构方案（定稿）

> 适用前提：部署使用**异构凭据**（不同订阅等级 / 不同 AWS 账号 profileArn / 不同 region）。
> 若全部凭据同质，本方案收益有限，单全局目录即可。

## 1. 背景与问题

### 1.1 模型目录本就是凭据级资源

`fetch_model_catalog_for_credential`（`src/kiro/token_manager.rs:2046`）拉取目录时，下列输入随凭据变化：

- **region**：`credentials.effective_api_region()` → `host = management.{region}.kiro.dev`（`:2053-2054`）
- **profileArn**：非 API Key 凭据把 `profileArn` 写入请求体（`:2076-2080`），绑定具体 AWS 账号 / 订阅
- **订阅等级**：不同账号可见的模型集合本就不同

因此上游 `ListAvailableModels` 返回的是"**该凭据能用什么**"，天然 per-credential。

### 1.2 现状：单一全局，先到先得

- 存储：单个 `GLOBAL_MODEL_CATALOG`（`src/kiro/model/model_catalog.rs:46`）
- 刷新：`refresh_model_catalog_ext`（`token_manager.rs:2128`）按 entry 顺序遍历凭据，**第一个成功就 `break`**（`:2174`），全局目录 ≈ "首个刷新成功的凭据"的视角
- 读取：`map_model`（`src/anthropic/converter.rs:427`）、`get_additional_model_request_fields`（`src/anthropic/handlers.rs:753`）、`get_context_window_size`（`converter.rs:566`）全部读这同一份全局，**完全不知道本次请求将用哪个凭据**
- 凭据选择在 provider 层 `acquire_context`/`select_next_credential` 才发生，且 round_robin 每次重选、失败可换凭据重试

### 1.3 异构凭据下的错配 Bug 清单

1. **模型静默降级**：若全局目录来自 Free 凭据（无 opus），用户点名 `opus-4.8` 时 `map_model` 三级匹配失败 → 降级 `"auto"`（`converter.rs:561`），用户无感知。
2. **thinking 静默丢失**：降级后 `mapped_id="auto"`，auto 无 thinking schema → `get_additional_model_request_fields` 返回 `None`，thinking/effort 字段被丢弃。
3. **路由错配**：round_robin 模式（`token_manager.rs:733-737`）在模型过滤之前就 `return`，可能把 opus 请求轮到不支持的凭据。
4. **effort 错配**：同一 model_id 在不同账号 schema 可能不同（如 effort enum 是否含 `max`），单全局用错凭据的 schema。
5. **控制平面抖动误禁凭据**：catalog 拉取失败即禁用凭据（`:2176-2185`），但 catalog 走 `management.*`、推理走 `runtime.*`，前者抖动不应让一个能正常推理的凭据报废。

## 2. 设计总纲：两类 catalog 依赖，分而治之

当前有两处 catalog 依赖被烤进请求体字符串（均在凭据选择之前）：

| 依赖项 | 在 body 中的位置 | per-credential 化的代价 | 解决手段 |
|---|---|---|---|
| **(A) 模型映射** `map_model` | `conversationState` 里**每条 message** 的 `userInputMessage.modelId`（`src/kiro/model/requests/conversation.rs:101/239`，多处） | 高：要重建/多点改写整个会话；且静默降级语义危险 | **凭据选择阶段过滤**（P2） |
| **(B) thinking/effort 字段** `get_additional_model_request_fields` | body 根部**一处** `additionalModelRequestFields` | 低：与 `profileArn` 注入同一处理点 | **故障转移循环内按选中凭据重建**（P3） |

**核心决策：两者用不同手段，不要都"按凭据重映射"。**

- **(A) 不重映射，而是"按 catalog 过滤凭据"**。用户点名 `opus-4.8` 时，正确行为是只路由到目录含 `opus-4.8` 的凭据，而非把请求发给只有 `4.6` 的凭据时偷偷降级。`modelId` 仍在 handler 阶段用**并集目录**映射一次；选择阶段保证选中凭据确实支持它，单次烤进 body 的 id 永远有效，无需多点改写。
- **(B) 必须按选中凭据的真实 schema 重建**。即便两凭据都有该模型，schema（是否支持 thinking、effort 的 enum）仍可能不同。

## 3. 分批实施

### 3.1 P1 — 存储与刷新

**数据结构**：catalog 挂到凭据 entry 上（生命周期与凭据一致，避免独立 map 的 id↔catalog 一致性问题）：

```rust
// CredentialEntry 内新增
catalog: Option<Arc<KiroModelCatalog>>,   // 该凭据真实目录；Arc 让服务期读取为廉价 clone
index:   Option<CredentialModelIndex>,    // P2 预计算索引（见 3.2）
```

`GLOBAL_MODEL_CATALOG` 保留，但语义改为**并集视图（merged）**：所有 entry 的 `models` 按 `model_id` 去重合并。用途：handler 预选阶段 `map_model`、token 计数、`count_tokens` 端点、admin —— 这些不区分凭据，**签名不动**。

**刷新**（改 `refresh_model_catalog_ext`）：

- 去掉"首个成功就 `break`"（`:2174`），改为**并发拉取所有未禁用凭据**（`join_all` + 小并发上限），分别写回各 entry 的 `catalog` 与 `index`。
- 拉完重算并集写入 `GLOBAL_MODEL_CATALOG`，更新 `LAST_CATALOG_REFRESH`。
- 按需新鲜度：某凭据 catalog 缺失或过期才重拉。

**⚠️ 解耦"拉取失败 → 禁用凭据"**（`:2176-2185`）：catalog 拉取失败 → **保留凭据启用、`catalog/index` 置 `None`、记 warning**；仅明确鉴权失败（401/403）才走禁用。服务期 catalog 为 `None` 时回退并集 / fallback。

### 3.2 P2 — 凭据选择按真实 catalog 过滤（实现细节定稿）

#### 3.2.1 接入点

给 `select_round_robin_entry`（`token_manager.rs:781`）的判据加一个谓词，扫描结构不变（仍是从 `current_id` 下一个开始、最多扫 N、跳过 disabled 的 O(N) 轮转）：

```rust
// 现状（:798）
if !entry.disabled { return Some(entry); }
// 改后
if !entry.disabled && credential_supports(entry, &mapped_id, thinking) {
    return Some(entry);
}
```

`priority` / `balanced` 模式（`:744-757`）同样把判据从写死的 `is_opus && supports_opus()` 换成 `credential_supports`，并**修掉 round_robin 提前 return 跳过过滤的 bug**（`:733-737`）。

#### 3.2.2 性能成败点

`credential_supports` 是 O(1) 还是 O(M·L)，决定整体复杂度。

**天真做法（禁止）**：对每个候选凭据跑完整模糊 `map_model_with_catalog`（内部 3 个 O(M) 循环 + 每候选 `normalize_string` O(L)）→ 每请求选择 **O(N·M·L)**，且在 `entries.lock()` 持锁热路径上。

**定稿做法 —— 模糊映射与精确过滤分离**：

1. 模糊匹配（剥后缀、版本兼容、family 兜底）**只做一次**，在 handler 阶段对**并集目录**跑，得到规范 `mapped_id`（如 `"claude-opus-4.8"`，必为某真实 model_id）。
2. 刷新时（T1，非热路径）为每凭据预计算索引：

```rust
struct CredentialModelIndex {
    model_ids:    HashSet<String>,   // 该凭据所有 model_id
    thinking_ids: HashSet<String>,   // 其中 schema 支持 thinking 的 model_id
}
```

3. 热路径退化为精确集合查（`HashSet::contains` 实质 O(1)，仅哈希一次字符串 O(L)）：

```rust
fn credential_supports(entry, mapped_id, thinking) -> bool {
    match &entry.index {
        Some(ix) => ix.model_ids.contains(mapped_id)
                    && (!thinking || ix.thinking_ids.contains(mapped_id)),
        None => true,   // catalog 未加载 → 放行，交上游裁决（保可用性）
    }
}
```

→ **轮转整体回到 O(N)。**

#### 3.2.3 模型映射时间线（昂贵的模糊映射每请求仅一次）

| 时刻 | 触发 | 做什么 | 频率 |
|---|---|---|---|
| **T1 预计算** | 凭据刷新（~10min） | 遍历每凭据 catalog，建 `model_ids`/`thinking_ids` 索引 | 每次 sweep，**非热路径** |
| **T2 规范映射** | handler 收到请求 | 对**并集**跑一次模糊 `map_model` → `mapped_id`，烤进 body | 每请求 1 次 |
| **T3 选择过滤** | provider 选凭据（持 `entries.lock()`） | O(1) 集合查 ×N + O(N) 轮转 | 每请求 1 次 |
| **T4 字段重建** | 故障转移循环每次尝试 | 用选中凭据 catalog 按 `mapped_id` 精确取 schema，重建 thinking/effort（P3） | 每尝试（通常 1） |

T3 用 T2 产出的 `mapped_id`，**不重跑模糊匹配**；T4 是按 id 精确取，**不是映射**。全程模糊映射只 T2 一次（与今天同阶，仅目录由"单全局"换"并集"，规模 M≈14 不变）。

#### 3.2.4 时间复杂度

记号：**N**=凭据数（<50），**M**=单凭据模型数（实测 14），**L**=模型名长度（~20），**A**=故障转移尝试数（通常 1）。

**每请求热路径（常见：点名具体模型）**

| 阶段 | 复杂度 | 备注 |
|---|---|---|
| T2 规范映射 | O(M·L) | 与今天相同 |
| T3 过滤 + 轮转 | O(N) | 每凭据 O(1) 集合查 ×N |
| T4 字段重建 | O(M·A)；加 schema-by-id 索引可降 O(A) | A 通常=1 |
| **合计** | **O(M·L + N)** | 与今天同阶 |

净增成本仅 T3 的 O(N) 精确过滤（可忽略）+ T4 每尝试一次 JSON 序列化（相对网络忽略）。**无 per-request O(N·M·L) 爆炸**，前提是 T1 预计算把 per-credential 检查降到 O(1)。

**刷新 sweep（每 ~10min，后台）**：建索引 O(N·M) CPU 可忽略；真实成本是 **N 次控制平面网络请求**（今天 1 次 → N 次），靠 5 分钟冷却 + 并发 + 按需新鲜度兜住。

#### 3.2.5 语义取舍：含糊请求

对并集做一次映射会取**最高版本**（`map_model` 按版本降序）。所以 `"claude-opus"`（无版本）被钉到 `opus-4.8` → 只有含它的凭据合格；若该凭据不可用，即便他者有 `opus-4.6` 也不被选。

- 默认接受此行为（可预测；点名具体版本的客户端走 O(N) 快路，是实际主流）。
- 可选**罕见兜底**：当精确过滤命中为空，再退一步跑一次 per-credential 模糊（O(N·M·L)），仅在"含糊请求 + 最高版本凭据不可用"时触发，不污染常见路径。
- 全部凭据均不支持时：返回明确 4xx（`no credential supports model X`），**不静默降级**。

### 3.3 P3 — 按选中凭据重建字段（解决 B）

改动点在 `call_api_with_retry`（`src/kiro/provider.rs:279`）循环内、选完 `ctx` 之后。需把重建所需原始输入传到 provider（现 body 已是字符串，原始 model 名与 thinking/output_config 已丢失）。

**方案 5a（推荐）**：`call_api_stream/call_api` 入参由 `&str` 改为轻量结构：

```rust
struct KiroCall {
    conversation_state: ConversationState,   // modelId 已用并集映射；P2 保证选中凭据支持
    thinking: Option<Thinking>,
    output_config: Option<OutputConfig>,
}
```

provider 每次尝试：`let cat = tm.catalog_for(ctx.id).unwrap_or_else(merged);` → 调 `get_additional_model_request_fields_with_catalog(...)` → 序列化 `KiroRequest` → endpoint 注入 profileArn → 发送。`modelId` 不在此处碰（P2 已保证有效）。

**方案 5b（过渡，改动小）**：保留字符串入参，`RequestContext` 带上 `thinking/output_config` 原值，扩展 `transform_api_body`（`src/kiro/endpoint/ide.rs:110`）用选中凭据 catalog 重算根部 `additionalModelRequestFields`（JSON patch，同 `inject_profile_arn:120` 手法）。不动 provider 公开签名。

**调用方盘点**（5a 必做）：grep `call_api_stream` / `call_api` / `call_api_with_retry` 全部调用方——至少 handler 的 stream / non-stream 两路（`handlers.rs:356/367`）+ websearch 路径（`handlers.rs:914` 一带），统一从"传字符串"改"传结构"。

### 3.4 函数签名重构（供上层调用）

`converter.rs`/`handlers.rs` 加显式 catalog 版本，旧签名保留为薄包装（委托并集），控制改动面：

- `map_model(m)` → 包装 `map_model_with_catalog(m, &merged)`；新增 `_with_catalog` 供选择阶段（实际只在 T1 建索引时用到模糊匹配）。
- `get_additional_model_request_fields(p)` → 包装 `_with_catalog(p, &merged)`；provider 调 `_with_catalog` 传 per-credential catalog。
- `get_context_window_size` 继续用并集（token 估算容忍并集）。

## 4. Worked Example（3 异构凭据 × 4 模型 × round_robin）

负载均衡模式 = `round_robin`。三个异构凭据（非 superset）：

| 凭据 | 等级 / 类型 | region | 各自 catalog（★=支持 thinking） |
|---|---|---|---|
| **#1** | Free / API Key | us-east-1 | `auto, haiku-4.5, sonnet-4, sonnet-4.5` |
| **#2** | Pro / profileArn(账号A) | us-east-1 | `auto, haiku-4.5, sonnet-4.5, sonnet-4.6★, opus-4.6★` |
| **#3** | Max / profileArn(账号B) | us-west-2 | `auto, sonnet-4.6★, opus-4.8★, opus-4.7★, opus-4.6★` |

四种请求：**A**=`opus-4.8`+thinking，**B**=`sonnet-4.6`+thinking，**C**=`sonnet-4.5`，**D**=`opus-4.6`+thinking+effort=max。

### 4.1 启动刷新（P1 全量并发）

```
#1 ← us-east-1 OK    → entry#1.catalog/index 就绪
#2 ← us-east-1 OK    → entry#2.catalog/index 就绪
#3 ← us-west-2 超时  → entry#3.catalog = None（P1：保留启用，记 warning）
```

对照当前实现：此处会**禁用 #3**，Max 凭据报废需重启。P1 后 #3 仅 catalog 暂 None，推理照常。
并集（`GLOBAL_MODEL_CATALOG`）= `{auto, haiku-4.5, sonnet-4, sonnet-4.5, sonnet-4.6, opus-4.6, opus-4.7, opus-4.8}`。

### 4.2 请求路由（P2 过滤 + 合格子集内轮转）

此刻 #3.index=None（按"未知放行"）：

| 序 | 请求 | 合格凭据 | 选中 | 说明 |
|---|---|---|---|---|
| R1 | A opus-4.8★ | {#3}（#2 无 4.8） | **#3** | opus-4.8 只可能在 #3 |
| R2 | B sonnet-4.6★ | {#2, #3} | **#2** | 轮转 |
| R3 | C sonnet-4.5 | {#1, #2}（#3 无） | **#1** | |
| R4 | D opus-4.6★ | {#2, #3} | **#3** | |
| R5 | B sonnet-4.6★ | {#2, #3} | **#2** | 与 R2 错开分摊 |

opus-4.8 永远落 #3；sonnet-4.6 在 #2/#3 轮转；sonnet-4.5 永不发给 #3。**round_robin 在过滤后的子集内轮转**，修掉了当前"全体轮转把 opus 轮给 Free"的 bug。

### 4.3 字段重建示例（P3：按选中凭据 schema）

设 `opus-4.6` 在两账号 schema 不同：#2 的 effort enum 含 `max`，#3 的只含 `high`。R4（effort=max）若轮到 **#3**：

```jsonc
// 用 entry#3.catalog 重建：max 不在 #3 enum → 回退 schema default
"additionalModelRequestFields": { "thinking":{"type":"adaptive"},
                                   "output_config":{"effort":"high"} }  // ✅ 自动矫正
```

若轮到 #2 则保留 `effort:max`。单全局实现做不到——它用"首个刷新成功凭据"的 schema 给所有人。

### 4.4 运行期时间线（刷新 + 配额 + 故障转移联动）

```
t=0    启动扫描：#3 控制平面超时→catalog=None（仍启用）→ opus-4.8 仍路由 #3，runtime us-west-2 正常→成功
t=10m  定时全量扫描：#3 恢复→entry#3.catalog/index 填好→此后 opus-4.8 过滤基于 #3 真实目录（精确）
t=23m  #3 一条 opus-4.8 上游 402 配额用尽→report_quota_exhausted 禁用 #3→opus-4.8 合格子集变空
t=23m+ 新 opus-4.8 请求：无凭据支持→handler 返回明确 4xx（不静默降级）；sonnet/haiku 继续在 #1/#2 轮转，互不影响
```

### 4.5 对照：当前实现下同一 R1

刷新"首个成功 break"→`GLOBAL = #1(Free)` 贫瘠目录（无 opus、无 thinking）。R1（opus-4.8+thinking）在 handler：
1. `map_model` 在 #1 目录找不到 opus → 三级匹配失败 → **降级 `"auto"`**（`converter.rs:561`）。
2. `mapped_id="auto"` 无 thinking schema → `get_additional_model_request_fields` 返回 `None` → **thinking 字段丢弃**。
3. round_robin 再把"已写死 auto、无 thinking"的 body 随便轮给某凭据——即便轮到能跑 opus-4.8 的 #3 也已晚。
→ **模型降级 + 思考丢失，双重静默 bug。**

## 5. 测试计划

- **单元**：`map_model_with_catalog` / `get_additional_model_request_fields_with_catalog` 喂两份合成目录（A 有 opus-4.8+thinking schema，B 只到 4.6），断言输出不同。
- **单元**：`credential_supports` 对 `model_ids`/`thinking_ids` 索引的命中与未命中；`index=None` 时放行。
- **单元**：`select_round_robin_entry` 加过滤后——**专门加 round_robin 模式用例**验证 opus 请求不会轮到 Free 凭据（回归 §1.3 bug 3）。
- **单元**：catalog 拉取失败时凭据保持启用、`catalog/index=None`（回归 §3.1 解耦）。
- **集成**：在 send 处加 seam，模拟"尝试 1 用不支持凭据 → 尝试 2 用支持凭据成功"，断言两次 body 的 `additionalModelRequestFields` 不同。
- **回归**：现有 `inject_profile_arn`（`ide.rs` tests）、`map_model`（`converter.rs` tests）经包装层全部通过。
- **探针**：扩展 `examples/thinking_effort_probe.rs`，加载两凭据各自目录，打印 per-credential 字段差异。

## 6. 风险与权衡

- **provider 入参结构化（5a）**是最大扩散面，波及 websearch 等所有 `call_api*` 调用方；5b 改动小但靠 JSON patch，稍脏。正式做选 5a。
- **刷新网络成本**：1 次 → N 次控制平面调用；靠冷却 + 并发 + 按需新鲜度兜住。
- **含糊请求被钉最高版本**（§3.2.5）：默认接受，必要时启用罕见兜底路径。
- **锁粒度**：T3 过滤在 `entries.lock()` 下做 O(N) 集合查，N 小（≤数十）无虞；索引构建在刷新（写锁）完成，不在请求锁内。
- **收益依赖异构程度**：若实测凭据模型集合相同、仅偶发差异，P3 收益下降，可只做 P1+P2。

## 7. 实施顺序与里程碑

| 批次 | 内容 | 价值 | 风险 | 可独立合入 |
|---|---|---|---|---|
| **P1** | §3.1 per-credential 存储 + 全量扫描 + 解耦禁用 | 立即消除"控制平面抖动误禁凭据" | 低（不改请求路径） | ✅ |
| **P2** | §3.2 索引预计算 + 选择过滤 + 修 round_robin bug | 消除"模型路由错配 / 静默降级" | 中（改选择逻辑） | ✅ |
| **P3** | §3.3+§3.4 provider 结构化 + per-credential 字段重建 | 消除 thinking/effort 错配 | 高（改 provider 签名） | 建议独立 PR |

P1+P2 已解决异构错配的大部分（路由 + 降级），风险可控；P3 为 thinking 正确性收尾，改动最大，独立合入。

## 8. 一句话对照

| 维度 | 当前（单全局） | 重构后（per-credential） |
|---|---|---|
| catalog 来源 | 首个刷新成功者，一份打天下 | 每凭据各一份 + 并集兜底 |
| 控制平面抖动 | 禁用该凭据 | 保留启用，catalog 暂 None |
| 模型路由 | 全体轮转，可能发给不支持者 | 仅在真支持子集内轮转（O(N)） |
| 点名模型不可用 | 静默降级 auto | 明确报错，不降级 |
| thinking/effort | 用错凭据 schema，丢字段或被拒 | 按选中凭据 schema 收紧，自动矫正 |
| 选择过滤复杂度 | 写死 `supports_opus` 粗判 | 索引化 O(1)/凭据，整体 O(N) |

## 9. 实现状态（已落地）

P1+P2+P3 已实现并通过 `cargo build --all-targets` 与 `cargo test --lib`（205 通过），探针 `examples/thinking_effort_probe.rs` 对真实 API 端到端验证通过。落地时相对本文档做了几处更克制的取舍：

- **P3 用「收紧（clamp）」而非「重建（_with_catalog）」**：`transform_api_body` 调用 `converter::clamp_additional_fields` 把并集已构建的 `additionalModelRequestFields` 按选中凭据 schema 收紧（enum 越界回退 default、无 schema 则移除）。**provider 公开签名不变**（`call_api`/`call_api_stream` 仍收 `&str`），`test.rs`/`websearch` 无需改动。
- **无需 `map_model_with_catalog`**：选择阶段用 handler 已映射的规范 `model_id` 对各凭据索引做精确集合查，不在每凭据重跑模糊匹配。
- **`require_thinking` 从请求体推导**：provider 用 `request_requires_thinking`（检测 body 根部 `additionalModelRequestFields.thinking`）得出，无需把原始 thinking/output_config 透传到 provider。
- **per-credential catalog 经 `RequestContext.catalog` 传入**：provider 循环内由 `token_manager.catalog_for(ctx.id)` 填充。
- **catalog 拉取失败完全不再禁用凭据**（不止收窄到 401/403）：保留启用、`catalog=None`、沿用上次目录；坏凭据交由运行期 `report_failure` 路径处理。

落点：
- `src/kiro/model/model_catalog.rs`：`CredentialModelIndex`、`model_supports_thinking`、`merge_catalogs`。
- `src/anthropic/converter.rs`：`clamp_additional_fields`。
- `src/kiro/token_manager.rs`：`CredentialEntry.{catalog,index}`、`credential_supports`、全量并发 sweep、`catalog_for`、`acquire_context`/`select_*` 加 `require_thinking` 与过滤。
- `src/kiro/endpoint/{mod.rs,ide.rs}`：`RequestContext.catalog` + `transform_api_body` 收紧。
- `src/kiro/provider.rs`：`request_requires_thinking`、填充 `RequestContext.catalog`、`acquire_context(model, require_thinking)`。

新增/调整测试：模型索引与并集合并（model_catalog）、`clamp_additional_fields` 三例（converter）、body 级收紧两例（ide）、per-credential 选择与 thinking 闸门与无凭据报错三例（token_manager）；并修正两个旧断言（round_robin 现按模型过滤 → 期望跳过 FREE；catalog 拉取失败 → 凭据保持启用）。
