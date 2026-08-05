//! 请求计量账本
//!
//! 上游 `generateAssistantResponse` 在响应流末尾下发 `meteringEvent`，给出本次请求
//! 的真实扣费（credits）；`assistantResponseEvent` 则带有实际服务模型 `modelId`
//! （可能与请求的 modelId 不同，且不同模型 `rateMultiplier` 不同）。
//!
//! 本模块把这两个信号按凭据 / 模型 / 会话三个维度聚合，供日志与 Admin API 读取。
//! 不做任何采样或估算：credits 一律来自上游，缺失就记为「未计量」，避免把估算值
//! 混进成本口径里。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 最近请求明细的保留条数（仅内存）
const RECENT_CAPACITY: usize = 200;
/// 会话维度聚合的最大保留数量，超出后淘汰最久未活跃的会话（仅内存）
const CONVERSATION_CAPACITY: usize = 500;
/// 落盘去抖间隔，与统计文件保持一致的量级
const SAVE_DEBOUNCE: Duration = Duration::from_secs(30);

/// 单次请求的计量结果
///
/// 由 handler 在请求收尾时构造并提交给 [`UsageLedger::record`]。
#[derive(Debug, Clone, Default)]
pub struct RequestUsage {
    /// 实际服务本次请求的凭据 ID
    pub credential_id: Option<u64>,
    /// 会话 ID（即下发给上游的 `conversationState.conversationId`）
    pub conversation_id: Option<String>,
    /// 请求侧的模型 ID（映射后下发给上游的值）
    pub requested_model: String,
    /// 上游实际服务的模型 ID，来自 `assistantResponseEvent.modelId`
    pub served_model: Option<String>,
    /// 上游下发的扣费；`None` 表示本次请求没有收到 `meteringEvent`
    pub credits: Option<f64>,
    /// 计费单位（如 "credits"）
    pub unit: Option<String>,
    /// 回答字符数
    pub answer_chars: u64,
    /// 思考（reasoning）字符数
    pub reasoning_chars: u64,
    /// 工具调用字符数（`toolUseEvent` 的 name + input JSON）
    ///
    /// 这同样是模型生成的输出，漏掉它会让带工具的轮次单位成本被系统性高估，
    /// 而 Claude Code 的绝大多数轮次都带工具。
    pub tool_chars: u64,
    /// 上游下发的上下文使用百分比
    pub context_usage_percentage: Option<f64>,
    /// 是否流式请求
    pub stream: bool,
}

impl RequestUsage {
    /// 归属到的模型：优先用上游实际服务的模型
    pub fn effective_model(&self) -> &str {
        self.served_model
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.requested_model)
    }

    /// 上游是否静默替换了模型
    pub fn is_substituted(&self) -> bool {
        match self.served_model.as_deref() {
            Some(served) => !served.is_empty() && served != self.requested_model,
            None => false,
        }
    }

    /// 输出总字符数（回答 + 思考 + 工具调用）
    pub fn output_chars(&self) -> u64 {
        self.answer_chars + self.reasoning_chars + self.tool_chars
    }
}

/// 聚合桶
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageBucket {
    /// 收到 `meteringEvent` 的请求数
    pub metered_requests: u64,
    /// 未收到 `meteringEvent` 的请求数（上游报错、客户端中断等）
    pub unmetered_requests: u64,
    /// 上游实际服务模型与请求模型不一致的请求数
    pub substituted_requests: u64,
    /// 累计扣费
    pub credits: f64,
    /// 累计回答字符数
    pub answer_chars: u64,
    /// 累计思考字符数
    pub reasoning_chars: u64,
    /// 累计工具调用字符数
    pub tool_chars: u64,
    /// 最后一次记账时间（RFC3339）
    pub last_at: Option<String>,
}

impl UsageBucket {
    fn apply(&mut self, usage: &RequestUsage, at: &str) {
        match usage.credits {
            Some(credits) => {
                self.metered_requests += 1;
                self.credits += credits;
            }
            None => self.unmetered_requests += 1,
        }
        if usage.is_substituted() {
            self.substituted_requests += 1;
        }
        self.answer_chars += usage.answer_chars;
        self.reasoning_chars += usage.reasoning_chars;
        self.tool_chars += usage.tool_chars;
        self.last_at = Some(at.to_string());
    }

    /// 输出总字符数（回答 + 思考 + 工具调用）
    pub fn output_chars(&self) -> u64 {
        self.answer_chars + self.reasoning_chars + self.tool_chars
    }

    /// 每千输出字符的扣费
    ///
    /// 输出（含思考与工具调用）是 Kiro 扣费的主导项，这个比值用于横向比较
    /// 不同模型 / effort 档位的真实成本。无输出时返回 `None`。
    ///
    /// 三类输出必须都算进分母：只算回答文本会让带工具的轮次看起来贵得离谱
    /// （实测一次 28 字符文本 + 大 tool_use 的请求扣了 0.114 credits）。
    pub fn credits_per_1k_output_chars(&self) -> Option<f64> {
        let chars = self.output_chars();
        if chars == 0 || self.credits == 0.0 {
            return None;
        }
        Some(self.credits / (chars as f64) * 1000.0)
    }
}

/// 最近一次请求的明细
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    /// 记账时间（RFC3339）
    pub at: String,
    /// 服务凭据 ID
    pub credential_id: Option<u64>,
    /// 会话 ID
    pub conversation_id: Option<String>,
    /// 请求的模型
    pub requested_model: String,
    /// 实际服务的模型
    pub served_model: Option<String>,
    /// 上游扣费；`None` 表示未计量
    pub credits: Option<f64>,
    /// 计费单位
    pub unit: Option<String>,
    /// 回答字符数
    pub answer_chars: u64,
    /// 思考字符数
    pub reasoning_chars: u64,
    /// 工具调用字符数
    pub tool_chars: u64,
    /// 上下文使用百分比
    pub context_usage_percentage: Option<f64>,
    /// 是否流式
    pub stream: bool,
}

/// 落盘部分：仅持久化跨重启仍有意义的累计量
///
/// 会话聚合与最近明细是进程内的观测窗口，不落盘。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedLedger {
    /// 全局累计
    total: UsageBucket,
    /// 按凭据 ID（字符串化，兼容 JSON object key）累计
    by_credential: HashMap<String, UsageBucket>,
    /// 按实际服务模型累计
    by_model: HashMap<String, UsageBucket>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    total: UsageBucket,
    by_credential: HashMap<u64, UsageBucket>,
    by_model: HashMap<String, UsageBucket>,
    by_conversation: HashMap<String, UsageBucket>,
    recent: VecDeque<UsageRecord>,
}

/// 账本快照（Admin API 读取用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    /// 全局累计
    pub total: UsageBucketView,
    /// 按凭据聚合
    pub by_credential: Vec<KeyedUsage>,
    /// 按实际服务模型聚合
    pub by_model: Vec<KeyedUsage>,
    /// 按会话聚合（仅内存窗口，最多 500 个会话）
    pub by_conversation: Vec<KeyedUsage>,
    /// 最近请求明细（仅内存窗口，最多 200 条，新的在前）
    pub recent: Vec<UsageRecord>,
}

/// 带派生指标的聚合桶视图
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucketView {
    /// 原始累计量
    #[serde(flatten)]
    pub bucket: UsageBucket,
    /// 每千输出字符的扣费（输出是扣费主导项，用于横向比价）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits_per_1k_output_chars: Option<f64>,
}

impl From<UsageBucket> for UsageBucketView {
    fn from(bucket: UsageBucket) -> Self {
        let credits_per_1k_output_chars = bucket.credits_per_1k_output_chars();
        Self {
            bucket,
            credits_per_1k_output_chars,
        }
    }
}

/// 一个聚合维度上的单项
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyedUsage {
    /// 维度取值（凭据 ID / 模型名 / 会话 ID）
    pub key: String,
    /// 该取值下的累计量
    #[serde(flatten)]
    pub usage: UsageBucketView,
}

fn to_keyed<K: ToString>(map: &HashMap<K, UsageBucket>) -> Vec<KeyedUsage> {
    let mut items: Vec<KeyedUsage> = map
        .iter()
        .map(|(k, v)| KeyedUsage {
            key: k.to_string(),
            usage: v.clone().into(),
        })
        .collect();
    // 扣费高的排前面，便于一眼看出成本集中在哪
    items.sort_by(|a, b| {
        b.usage
            .bucket
            .credits
            .total_cmp(&a.usage.bucket.credits)
            .then_with(|| a.key.cmp(&b.key))
    });
    items
}

/// 会话数超出上限时，淘汰最久未活跃的若干个会话
fn evict_stale_conversations(map: &mut HashMap<String, UsageBucket>) {
    while map.len() > CONVERSATION_CAPACITY {
        let oldest = map
            .iter()
            .min_by(|a, b| a.1.last_at.cmp(&b.1.last_at))
            .map(|(k, _)| k.clone());
        match oldest {
            Some(key) => {
                map.remove(&key);
            }
            None => break,
        }
    }
}

/// 计量账本
///
/// 线程安全，可跨请求共享。写入走去抖落盘，避免高频请求打爆磁盘。
pub struct UsageLedger {
    inner: Mutex<LedgerInner>,
    /// 落盘路径；`None` 时只在内存中统计
    path: Option<PathBuf>,
    dirty: AtomicBool,
    last_save_at: Mutex<Option<Instant>>,
}

impl UsageLedger {
    /// 创建账本，并尝试从磁盘恢复累计量
    pub fn new(path: Option<PathBuf>) -> Self {
        let ledger = Self {
            inner: Mutex::new(LedgerInner::default()),
            path,
            dirty: AtomicBool::new(false),
            last_save_at: Mutex::new(None),
        };
        ledger.load();
        ledger
    }

    /// 落盘文件路径（供日志与测试使用）
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    fn load(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };
        let persisted: PersistedLedger = match serde_json::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("解析用量账本失败，将从零开始统计: {}", e);
                return;
            }
        };

        let mut inner = self.inner.lock();
        inner.total = persisted.total;
        inner.by_model = persisted.by_model;
        inner.by_credential = persisted
            .by_credential
            .into_iter()
            .filter_map(|(k, v)| k.parse::<u64>().ok().map(|id| (id, v)))
            .collect();
        *self.last_save_at.lock() = Some(Instant::now());
        self.dirty.store(false, Ordering::Relaxed);
        tracing::info!(
            "已从账本恢复累计用量: {:.4} credits / {} 次已计量请求",
            inner.total.credits,
            inner.total.metered_requests
        );
    }

    /// 记一次请求的用量
    pub fn record(&self, usage: &RequestUsage) {
        let at = Utc::now().to_rfc3339();
        let model_key = {
            let m = usage.effective_model();
            if m.is_empty() {
                "unknown".to_string()
            } else {
                m.to_string()
            }
        };

        {
            let mut inner = self.inner.lock();
            inner.total.apply(usage, &at);
            if let Some(id) = usage.credential_id {
                inner.by_credential.entry(id).or_default().apply(usage, &at);
            }
            inner.by_model.entry(model_key).or_default().apply(usage, &at);
            if let Some(conv) = usage.conversation_id.as_deref().filter(|c| !c.is_empty()) {
                inner
                    .by_conversation
                    .entry(conv.to_string())
                    .or_default()
                    .apply(usage, &at);
                evict_stale_conversations(&mut inner.by_conversation);
            }

            inner.recent.push_front(UsageRecord {
                at,
                credential_id: usage.credential_id,
                conversation_id: usage.conversation_id.clone(),
                requested_model: usage.requested_model.clone(),
                served_model: usage.served_model.clone(),
                credits: usage.credits,
                unit: usage.unit.clone(),
                answer_chars: usage.answer_chars,
                reasoning_chars: usage.reasoning_chars,
                tool_chars: usage.tool_chars,
                context_usage_percentage: usage.context_usage_percentage,
                stream: usage.stream,
            });
            while inner.recent.len() > RECENT_CAPACITY {
                inner.recent.pop_back();
            }
        }

        self.save_debounced();
    }

    /// 读取账本快照
    pub fn snapshot(&self) -> UsageSnapshot {
        let inner = self.inner.lock();
        UsageSnapshot {
            total: inner.total.clone().into(),
            by_credential: to_keyed(&inner.by_credential),
            by_model: to_keyed(&inner.by_model),
            by_conversation: to_keyed(&inner.by_conversation),
            recent: inner.recent.iter().cloned().collect(),
        }
    }

    /// 读取指定凭据的累计用量
    pub fn credential_bucket(&self, id: u64) -> Option<UsageBucket> {
        self.inner.lock().by_credential.get(&id).cloned()
    }

    fn save_debounced(&self) {
        self.dirty.store(true, Ordering::Relaxed);
        let should_flush = match *self.last_save_at.lock() {
            Some(last) => last.elapsed() >= SAVE_DEBOUNCE,
            None => true,
        };
        if should_flush {
            self.save();
        }
    }

    /// 强制落盘（进程退出前调用，避免丢掉去抖窗口内的记账）
    pub fn flush(&self) {
        if self.dirty.load(Ordering::Relaxed) {
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };

        let persisted = {
            let inner = self.inner.lock();
            PersistedLedger {
                total: inner.total.clone(),
                by_credential: inner
                    .by_credential
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
                by_model: inner.by_model.clone(),
            }
        };

        match serde_json::to_string_pretty(&persisted) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存用量账本失败: {}", e);
                } else {
                    *self.last_save_at.lock() = Some(Instant::now());
                    self.dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化用量账本失败: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(cred: u64, requested: &str, served: Option<&str>, credits: Option<f64>) -> RequestUsage {
        RequestUsage {
            credential_id: Some(cred),
            conversation_id: Some("conv-1".to_string()),
            requested_model: requested.to_string(),
            served_model: served.map(|s| s.to_string()),
            credits,
            unit: Some("credits".to_string()),
            answer_chars: 1000,
            reasoning_chars: 500,
            tool_chars: 0,
            context_usage_percentage: Some(1.5),
            stream: true,
        }
    }

    #[test]
    fn test_record_aggregates_three_dimensions() {
        let ledger = UsageLedger::new(None);
        ledger.record(&usage(17, "claude-opus-4.8", Some("claude-opus-4.7"), Some(2.0)));
        ledger.record(&usage(17, "claude-opus-4.8", Some("claude-opus-4.7"), Some(1.0)));

        let snap = ledger.snapshot();
        assert_eq!(snap.total.bucket.metered_requests, 2);
        assert_eq!(snap.total.bucket.credits, 3.0);
        // 归属到实际服务模型，而不是请求的模型
        assert_eq!(snap.by_model.len(), 1);
        assert_eq!(snap.by_model[0].key, "claude-opus-4.7");
        assert_eq!(snap.by_credential[0].key, "17");
        assert_eq!(snap.by_conversation[0].key, "conv-1");
        assert_eq!(snap.recent.len(), 2);
    }

    #[test]
    fn test_unmetered_request_does_not_pollute_credits() {
        let ledger = UsageLedger::new(None);
        ledger.record(&usage(1, "m", None, None));
        let snap = ledger.snapshot();
        assert_eq!(snap.total.bucket.metered_requests, 0);
        assert_eq!(snap.total.bucket.unmetered_requests, 1);
        assert_eq!(snap.total.bucket.credits, 0.0);
        // 未计量时不产生派生指标，避免误读
        assert_eq!(snap.total.credits_per_1k_output_chars, None);
    }

    #[test]
    fn test_substitution_counted() {
        let ledger = UsageLedger::new(None);
        ledger.record(&usage(1, "claude-opus-4.7", Some("claude-sonnet-4.6"), Some(1.0)));
        ledger.record(&usage(1, "claude-opus-4.7", Some("claude-opus-4.7"), Some(1.0)));
        let snap = ledger.snapshot();
        assert_eq!(snap.total.bucket.substituted_requests, 1);
    }

    #[test]
    fn test_credits_per_1k_output_chars() {
        let ledger = UsageLedger::new(None);
        // 1500 输出字符 / 3.0 credits => 2.0 credits per 1k chars
        ledger.record(&usage(1, "m", None, Some(3.0)));
        let snap = ledger.snapshot();
        let v = snap.total.credits_per_1k_output_chars.unwrap();
        assert!((v - 2.0).abs() < 1e-9, "got {}", v);
    }

    #[test]
    fn test_tool_chars_included_in_unit_cost() {
        // 复刻实测的离群请求：28 字符回答 + 大 tool_use，扣费 0.114 credits。
        // 漏算工具输出会得出 4.07 credits/1k 字符（虚高约 6.7 倍）。
        let mut u = usage(1, "m", None, Some(0.1140));
        u.answer_chars = 28;
        u.reasoning_chars = 0;
        u.tool_chars = 600;

        let ledger = UsageLedger::new(None);
        ledger.record(&u);
        let snap = ledger.snapshot();

        assert_eq!(snap.total.bucket.tool_chars, 600);
        let v = snap.total.credits_per_1k_output_chars.unwrap();
        // 0.1140 / 628 * 1000
        assert!((v - 0.18153).abs() < 1e-4, "got {}", v);
        assert!(v < 0.5, "单位成本不应因漏算工具输出而虚高");
    }

    #[test]
    fn test_recent_window_bounded() {
        let ledger = UsageLedger::new(None);
        for _ in 0..(RECENT_CAPACITY + 20) {
            ledger.record(&usage(1, "m", None, Some(0.1)));
        }
        assert_eq!(ledger.snapshot().recent.len(), RECENT_CAPACITY);
    }

    #[test]
    fn test_conversation_window_bounded() {
        let ledger = UsageLedger::new(None);
        for i in 0..(CONVERSATION_CAPACITY + 10) {
            let mut u = usage(1, "m", None, Some(0.1));
            u.conversation_id = Some(format!("conv-{}", i));
            ledger.record(&u);
        }
        assert!(ledger.snapshot().by_conversation.len() <= CONVERSATION_CAPACITY);
    }

    #[test]
    fn test_snapshot_json_shape() {
        // 锁定 Admin API 的响应契约：嵌套 flatten 必须摊平成同层字段
        let ledger = UsageLedger::new(None);
        ledger.record(&usage(17, "claude-opus-4.8", Some("claude-opus-4.7"), Some(3.0)));
        let json = serde_json::to_value(ledger.snapshot()).unwrap();

        assert_eq!(json["total"]["credits"], 3.0);
        assert_eq!(json["total"]["meteredRequests"], 1);
        assert_eq!(json["total"]["substitutedRequests"], 1);
        assert_eq!(json["total"]["creditsPer1kOutputChars"], 2.0);

        let model = &json["byModel"][0];
        assert_eq!(model["key"], "claude-opus-4.7");
        assert_eq!(model["credits"], 3.0);

        let recent = &json["recent"][0];
        assert_eq!(recent["requestedModel"], "claude-opus-4.8");
        assert_eq!(recent["servedModel"], "claude-opus-4.7");
        assert_eq!(recent["credits"], 3.0);
        assert_eq!(recent["stream"], true);
    }

    #[test]
    fn test_persist_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kiro-usage-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kiro_usage.json");
        let _ = std::fs::remove_file(&path);

        let ledger = UsageLedger::new(Some(path.clone()));
        ledger.record(&usage(42, "claude-opus-4.8", Some("claude-opus-4.7"), Some(2.5)));
        ledger.flush();

        let restored = UsageLedger::new(Some(path.clone()));
        let snap = restored.snapshot();
        assert_eq!(snap.total.bucket.credits, 2.5);
        assert_eq!(snap.by_credential[0].key, "42");
        assert_eq!(snap.by_model[0].key, "claude-opus-4.7");
        // 会话维度与最近明细不落盘
        assert!(snap.by_conversation.is_empty());
        assert!(snap.recent.is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
