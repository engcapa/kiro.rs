# profileArn 400 错误分析文档

## 错误描述

AWS API 返回 400 错误，错误信息：
```json
{
  "message": "profileArn is required for this request.",
  "reason": null
}
```

## 已添加的诊断日志

### 1. 普通 API 调用 (generateAssistantResponse)
当发生 400 错误时，现在会记录：
- 请求 URL
- 凭据 ID（Option<u64>）
- profile_arn 的值（Option<String>）
- 请求的模型（Option<String>）
- 请求体长度
- 请求体前 500 字符预览

**日志示例：**
```
2026-04-05T12:15:34.913616Z ERROR kiro_rs::kiro::provider: API 400 错误详情 - URL: https://q.us-east-1.amazonaws.com/generateAssistantResponse, 凭据ID: Some(1), profile_arn: None, 模型: Some("claude-opus-4-6"), 请求体长度: 1234 bytes
2026-04-05T12:15:34.913620Z ERROR kiro_rs::kiro::provider: 请求体预览: {"conversationState":{"currentMessage":{"userInputMessage":{"content":"test","modelId":"claude-opus-4-6"}}}}
2026-04-05T12:15:34.913625Z ERROR kiro_rs::anthropic::handlers: Kiro API 调用失败: 流式 API 请求失败: 400 Bad Request {"message":"profileArn is required for this request.","reason":null}
```

### 2. MCP API 调用 (WebSearch 等工具)
当发生 400 错误时，现在会记录：
- 请求 URL
- 凭据 ID（Option<u64>）
- profile_arn 的值（Option<String>）
- 请求体长度
- 请求体前 500 字符预览

**日志示例：**
```
2026-04-05T12:15:34.913616Z ERROR kiro_rs::kiro::provider: MCP API 400 错误详情 - URL: https://q.us-east-1.amazonaws.com/mcp, 凭据ID: Some(2), profile_arn: None, 请求体长度: 567 bytes
2026-04-05T12:15:34.913620Z ERROR kiro_rs::kiro::provider: 请求体预览: {"method":"search","params":{"query":"test"}}
```

## 可能的原因分析

### 1. 凭据配置问题
- **某些凭据缺少 profile_arn**：多凭据场景下，部分凭据可能没有配置 profile_arn
- **检查方法**：查看日志中的 `profile_arn: None` 记录

### 2. 请求体构建问题
- **profile_arn 字段被错误移除**：在 `patch_profile_arn` 函数中，如果凭据没有 profile_arn，会移除该字段
- **代码位置**：`src/kiro/provider.rs:683-701`
- **逻辑**：
  ```rust
  None => {
      // 当前凭据没有 profile_arn，移除该字段
      if let Some(obj) = value.as_object_mut() {
          obj.remove("profileArn");
      }
  }
  ```

### 3. API 区域差异
- **不同区域的 API 要求不同**：某些区域可能强制要求 profile_arn
- **检查方法**：对比日志中的 URL，查看是否特定区域出现此问题

### 4. 凭据轮换触发
- **故障转移时切换到无 profile_arn 的凭据**：当主凭据失败后，切换到备用凭据可能缺少 profile_arn
- **检查方法**：查看错误前是否有凭据切换的日志

### 5. 特定模型要求
- **某些模型强制要求 profile_arn**：不同模型可能有不同的权限要求
- **检查方法**：对比日志中的模型字段

## 排查步骤

1. **收集日志样本**
   - 记录至少 5-10 次 400 错误的完整日志
   - 包含错误前后的上下文信息

2. **分析共同特征**
   - 是否都是同一个凭据？
   - 是否都是同一个模型？
   - 是否都是同一个 API 区域？
   - profile_arn 是否都为 None？

3. **对比成功案例**
   - 找到成功的请求日志
   - 对比请求体结构差异
   - 对比凭据配置差异

4. **验证修复方案**
   - 如果是凭据问题：为所有凭据配置 profile_arn
   - 如果是代码问题：修改 patch_profile_arn 逻辑，保留默认值而非移除
   - 如果是区域问题：针对特定区域使用特定凭据

## 代码改进建议

### 选项 1：保留默认 profile_arn
```rust
fn patch_profile_arn(request_body: &str, profile_arn: Option<&str>) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(request_body) else {
        return request_body.to_string();
    };

    if let Some(arn) = profile_arn {
        value["profileArn"] = serde_json::Value::String(arn.to_string());
    }
    // 不移除字段，保留原始请求体中的值

    serde_json::to_string(&value).unwrap_or_else(|_| request_body.to_string())
}
```

### 选项 2：使用配置级默认值
在 `Config` 中添加 `default_profile_arn` 字段，当凭据没有 profile_arn 时使用默认值。

### 选项 3：强制验证
在启动时验证所有凭据都有 profile_arn，如果缺少则报错或警告。

## 监控指标

建议添加以下监控：
1. **400 错误率**：按凭据、模型、区域分组
2. **profile_arn 缺失率**：统计有多少请求使用了无 profile_arn 的凭据
3. **凭据切换频率**：监控故障转移的触发频率

## 相关代码位置

- 错误处理：`src/anthropic/handlers.rs:31-68`
- API 调用：`src/kiro/provider.rs:450-676`
- profile_arn 替换：`src/kiro/provider.rs:678-701`
- 请求构建：`src/anthropic/handlers.rs:289-298` 和 `787-796`
