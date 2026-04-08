//! 使用额度查询数据模型
//!
//! 包含 getUsageLimits API 的响应类型定义

use serde::Deserialize;

/// 使用额度查询响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitsResponse {
    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 订阅信息
    #[serde(default)]
    pub subscription_info: Option<SubscriptionInfo>,

    /// 使用量明细列表
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,

    /// 账户信息（包含 email、userId、provider 等）- 字段名 accountInfo
    #[serde(default)]
    pub account_info: Option<AccountInfo>,

    /// 捕获所有未知字段（用于调试）
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

impl UsageLimitsResponse {
    /// 从 extra 字段中尝试提取账户信息（处理不同字段名）
    pub fn resolve_account_info(&self) -> Option<AccountInfo> {
        if self.account_info.is_some() {
            return self.account_info.clone();
        }
        // 尝试其他可能的字段名
        for key in &["userInfo", "user_info", "identity", "user", "profile", "userProfile"] {
            if let Some(val) = self.extra.get(*key) {
                tracing::debug!("尝试从 {} 字段解析 AccountInfo: {}", key, val);
                match serde_json::from_value::<AccountInfo>(val.clone()) {
                    Ok(info) => {
                        tracing::info!("成功从 {} 字段解析 AccountInfo: {:?}", key, info);
                        return Some(info);
                    }
                    Err(e) => {
                        tracing::warn!("从 {} 字段解析 AccountInfo 失败: {}", key, e);
                    }
                }
            }
        }
        None
    }
}

/// 订阅信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionInfo {
    /// 订阅标题 (KIRO PRO+ / KIRO FREE 等)
    #[serde(default)]
    pub subscription_title: Option<String>,
}

/// 账户信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    /// 用户邮箱
    #[serde(default)]
    pub email: Option<String>,

    /// 用户 ID
    #[serde(default)]
    pub user_id: Option<String>,

    /// 认证供应商 (GitHub, Google 等)
    #[serde(default)]
    pub provider: Option<String>,

    /// 供应商用户 ID
    #[serde(default)]
    pub provider_user_id: Option<String>,
}

/// 使用量明细
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 奖励额度列表
    #[serde(default)]
    pub bonuses: Vec<Bonus>,

    /// 免费试用信息
    #[serde(default)]
    pub free_trial_info: Option<FreeTrialInfo>,

    /// 下次重置日期 (Unix 时间戳)
    #[serde(default)]
    pub next_date_reset: Option<f64>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

/// 奖励额度
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bonus {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: f64,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: f64,

    /// 状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub status: Option<String>,
}

impl Bonus {
    /// 检查 bonus 是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

/// 免费试用信息
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeTrialInfo {
    /// 当前使用量
    #[serde(default)]
    pub current_usage: i64,

    /// 当前使用量（精确值）
    #[serde(default)]
    pub current_usage_with_precision: f64,

    /// 免费试用过期时间 (Unix 时间戳)
    #[serde(default)]
    pub free_trial_expiry: Option<f64>,

    /// 免费试用状态 (ACTIVE / EXPIRED)
    #[serde(default)]
    pub free_trial_status: Option<String>,

    /// 使用限额
    #[serde(default)]
    pub usage_limit: i64,

    /// 使用限额（精确值）
    #[serde(default)]
    pub usage_limit_with_precision: f64,
}

// ============ 便捷方法实现 ============

impl FreeTrialInfo {
    /// 检查免费试用是否处于激活状态
    pub fn is_active(&self) -> bool {
        self.free_trial_status
            .as_deref()
            .map(|s| s == "ACTIVE")
            .unwrap_or(false)
    }
}

impl UsageLimitsResponse {
    /// 获取订阅标题
    pub fn subscription_title(&self) -> Option<&str> {
        self.subscription_info
            .as_ref()
            .and_then(|info| info.subscription_title.as_deref())
    }

    /// 获取用户邮箱
    pub fn email(&self) -> Option<String> {
        self.resolve_account_info().and_then(|info| info.email)
    }

    /// 获取用户 ID
    pub fn user_id(&self) -> Option<String> {
        self.resolve_account_info().and_then(|info| info.user_id)
    }

    /// 获取认证供应商
    pub fn provider(&self) -> Option<String> {
        self.resolve_account_info().and_then(|info| info.provider)
    }

    /// 获取第一个使用量明细
    fn primary_breakdown(&self) -> Option<&UsageBreakdown> {
        self.usage_breakdown_list.first()
    }

    /// 获取总使用限额（精确值）
    ///
    /// 累加基础额度、激活的免费试用额度和激活的奖励额度
    pub fn usage_limit(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.usage_limit_with_precision;

        // 累加激活的 free trial 额度
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.usage_limit_with_precision;
            }
        }

        // 累加激活的 bonus 额度
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.usage_limit;
            }
        }

        total
    }

    /// 获取总当前使用量（精确值）
    ///
    /// 累加基础使用量、激活的免费试用使用量和激活的奖励使用量
    pub fn current_usage(&self) -> f64 {
        let Some(breakdown) = self.primary_breakdown() else {
            return 0.0;
        };

        let mut total = breakdown.current_usage_with_precision;

        // 累加激活的 free trial 使用量
        if let Some(trial) = &breakdown.free_trial_info {
            if trial.is_active() {
                total += trial.current_usage_with_precision;
            }
        }

        // 累加激活的 bonus 使用量
        for bonus in &breakdown.bonuses {
            if bonus.is_active() {
                total += bonus.current_usage;
            }
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_userinfo_from_api() {
        let json = r#"{
            "daysUntilReset": 0,
            "limits": [],
            "nextDateReset": 1.7775936E9,
            "subscriptionInfo": {
                "subscriptionTitle": "KIRO FREE"
            },
            "usageBreakdownList": [],
            "userInfo": {
                "email": null,
                "userId": "d-9067c98495.94a81478-9081-704e-0722-8a7cbcb6b16f"
            }
        }"#;

        let result: Result<UsageLimitsResponse, _> = serde_json::from_str(json);
        println!("Parse result: {:?}", result);

        assert!(result.is_ok(), "JSON 解析应该成功");
        let response = result.unwrap();

        println!("Extra fields: {:?}", response.extra.keys().collect::<Vec<_>>());
        println!("account_info: {:?}", response.account_info);

        let resolved = response.resolve_account_info();
        println!("resolve_account_info(): {:?}", resolved);

        assert!(resolved.is_some(), "应该能从 userInfo 解析出 AccountInfo");
        let account_info = resolved.unwrap();

        assert_eq!(account_info.email, None, "email 应该是 None");
        assert_eq!(
            account_info.user_id,
            Some("d-9067c98495.94a81478-9081-704e-0722-8a7cbcb6b16f".to_string()),
            "user_id 应该被正确解析"
        );

        println!("email(): {:?}", response.email());
        println!("user_id(): {:?}", response.user_id());

        assert_eq!(response.email(), None);
        assert_eq!(
            response.user_id(),
            Some("d-9067c98495.94a81478-9081-704e-0722-8a7cbcb6b16f".to_string())
        );
    }
}
