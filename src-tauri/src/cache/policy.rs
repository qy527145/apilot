//! 缓存策略：哪些请求可以缓存。
//!
//! 这里的取舍是**宁可不缓存，也不要返回错误的结果**。响应缓存会改变语义 ——
//! 同一个问题第二次问会拿到第一次的答案。只有当"同样输入必然同样输出"成立时，
//! 这个替换才是安全的。

use serde::{Deserialize, Serialize};

use crate::protocol::dto::UnifiedRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachePolicy {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_entries: u32,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            // 默认关闭：缓存会改变语义，必须由用户显式开启。
            enabled: false,
            ttl_secs: 3600,
            max_entries: 1000,
        }
    }
}

impl CachePolicy {
    pub fn normalized(mut self) -> Self {
        // TTL 为 0 会让所有条目立刻过期，等同于关闭，容易让人困惑 —— 抬到 1 秒。
        self.ttl_secs = self.ttl_secs.clamp(1, 30 * 24 * 3600);
        self.max_entries = self.max_entries.clamp(1, 100_000);
        self
    }
}

/// 清空范围。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheScope {
    All,
    Expired,
    Model { model: String },
}

/// 该请求是否可缓存。
///
/// 放行条件（全部满足）：
/// 1. 策略已启用；
/// 2. **temperature 显式为 0**。
///
/// 第 2 条是关键。`temperature` 为 `None` 时用的是服务商默认值（通常是 1.0），
/// 输出是随机的；这时返回旧答案会让用户以为模型"卡住了"。要求显式设为 0，
/// 是把「我要确定性」这个意图变成一个可检验的条件 —— 用户没写就是没打算要确定性。
///
/// 这一点无法通过配置放宽，因为放宽就意味着接受错误结果。
pub fn is_cacheable(req: &UnifiedRequest, policy: &CachePolicy) -> bool {
    policy.enabled && req.temperature == Some(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::UnifiedRequest;

    fn req(temperature: Option<f64>) -> UnifiedRequest {
        let mut r = UnifiedRequest::new("m");
        r.temperature = temperature;
        r
    }

    fn enabled() -> CachePolicy {
        CachePolicy {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn default_policy_is_disabled() {
        assert!(!CachePolicy::default().enabled, "缓存必须显式开启");
        assert!(!is_cacheable(&req(Some(0.0)), &CachePolicy::default()));
    }

    #[test]
    fn temperature_zero_is_cacheable_when_enabled() {
        assert!(is_cacheable(&req(Some(0.0)), &enabled()));
    }

    #[test]
    fn missing_temperature_is_not_cacheable() {
        // 未设置 temperature = 用服务商默认值 = 非确定性输出
        assert!(
            !is_cacheable(&req(None), &enabled()),
            "temperature 缺失时输出是随机的，缓存会返回错误结果"
        );
    }

    #[test]
    fn positive_temperature_is_not_cacheable() {
        assert!(!is_cacheable(&req(Some(0.7)), &enabled()));
        assert!(!is_cacheable(&req(Some(1.0)), &enabled()));
        assert!(!is_cacheable(&req(Some(0.0001)), &enabled()));
    }

    #[test]
    fn disabled_policy_blocks_caching_regardless_of_temperature() {
        let p = CachePolicy {
            enabled: false,
            ..Default::default()
        };
        assert!(!is_cacheable(&req(Some(0.0)), &p));
    }

    #[test]
    fn normalize_clamps_ttl_and_entries() {
        let p = CachePolicy {
            enabled: true,
            ttl_secs: 0,
            max_entries: 0,
        }
        .normalized();
        assert_eq!(p.ttl_secs, 1, "TTL=0 等于永远过期，没有意义");
        assert_eq!(p.max_entries, 1);

        let p = CachePolicy {
            enabled: true,
            ttl_secs: u64::MAX,
            max_entries: u32::MAX,
        }
        .normalized();
        assert_eq!(p.ttl_secs, 30 * 24 * 3600);
        assert_eq!(p.max_entries, 100_000);
    }

    #[test]
    fn scope_json_matches_frontend_contract() {
        assert_eq!(
            serde_json::to_value(CacheScope::All).unwrap()["kind"],
            "all"
        );
        assert_eq!(
            serde_json::to_value(CacheScope::Expired).unwrap()["kind"],
            "expired"
        );
        let v = serde_json::to_value(CacheScope::Model {
            model: "m".into(),
        })
        .unwrap();
        assert_eq!(v["kind"], "model");
        assert_eq!(v["model"], "m");
    }
}
