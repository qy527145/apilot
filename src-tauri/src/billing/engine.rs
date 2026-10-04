//! 结算公式。
//!
//! 单位约定：**1 token × 倍率 1.0 = 1 quota**，即 `model_ratio = 1` 对应
//! 每 100 万 token 2 美元（与 new-api 一致）。
//!
//! ```text
//! prompt_units     = fresh_input + cache_read × cache_ratio + cache_create × cache_create_ratio
//! completion_units = output × completion_ratio
//! quota            = (prompt_units + completion_units) × model_ratio × group_ratio
//! quota           ×= Π other_ratios
//! quota           += tool_call_surcharge × 工具调用次数
//! if model_ratio ≠ 0 且 quota ≤ 0 → quota = 1     // 兜底，避免"用了却不计费"
//! ```
//!
//! 一切乘法都在单次结算内完成并立刻取整，误差不会跨请求累积。

use super::pricing::ModelPricing;
use super::quota::clamp_quota;
use crate::protocol::dto::UnifiedUsage;

/// 一次请求的计费明细，便于在监控页展示"钱花在哪了"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuotaBreakdown {
    /// 输入侧加权后的计费单位数（不是额度）。
    pub prompt_units: f64,
    /// 输出侧加权后的计费单位数。
    pub completion_units: f64,
    /// 输入部分的额度。
    pub prompt_quota: i64,
    /// 输出部分的额度。
    pub completion_quota: i64,
    /// 工具调用附加费。
    pub tool_quota: i64,
    /// 最终额度。
    pub total_quota: i64,
    /// 计入计费的 fresh 输入 token（不含缓存）。
    pub billable_input_tokens: u64,
    /// 因缓存读而少花的额度。
    pub cache_saved_quota: i64,
}

/// 无状态计费引擎。
#[derive(Debug, Default, Clone, Copy)]
pub struct BillingEngine;

impl BillingEngine {
    /// 按用量与单价算出额度。
    pub fn settle(
        usage: &UnifiedUsage,
        pricing: &ModelPricing,
        tool_call_count: u32,
    ) -> QuotaBreakdown {
        // 免费模型：直接归零，不参与任何倍率运算。
        if pricing.is_free() {
            return QuotaBreakdown {
                billable_input_tokens: usage.input_tokens,
                ..Default::default()
            };
        }

        let fresh = usage.input_tokens as f64;
        let cache_read = usage.cache_read_tokens as f64;
        let cache_create = usage.cache_creation_tokens as f64;
        let output = usage.output_tokens as f64;

        let prompt_units =
            fresh + cache_read * pricing.cache_ratio + cache_create * pricing.cache_create_ratio;
        let completion_units = output * pricing.completion_ratio;

        // 所有倍率连乘：模型 × 分组 × 其它。
        let mut multiplier = pricing.model_ratio * pricing.group_ratio;
        for r in pricing.other_ratios.values() {
            multiplier *= *r;
        }

        let base = (prompt_units + completion_units) * multiplier;

        let prompt_quota = clamp_quota(prompt_units * multiplier);
        let completion_quota = clamp_quota(completion_units * multiplier);
        let tool_quota = clamp_quota(pricing.tool_call_surcharge as f64 * tool_call_count as f64);

        let mut total = clamp_quota(base).saturating_add(tool_quota);

        // 有倍率却算出 0（例如极小的请求被取整掉了）时至少计 1，
        // 否则大量小额请求会完全不计费。
        if total <= 0 {
            total = 1;
        }

        // 缓存读带来的节省：相对于「同一批 token 按原价计费」少花的钱。
        // 只在缓存价比原价便宜时为正，否则记 0（不计"负节省"）。
        let saved_per_token = (1.0 - pricing.cache_ratio).max(0.0) * multiplier;
        let cache_saved_quota = clamp_quota(cache_read * saved_per_token);

        QuotaBreakdown {
            prompt_units,
            completion_units,
            prompt_quota,
            completion_quota,
            tool_quota,
            total_quota: total,
            billable_input_tokens: usage.input_tokens,
            cache_saved_quota,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{UnifiedUsage, UsageSource};

    fn usage(input: u64, output: u64) -> UnifiedUsage {
        UnifiedUsage {
            input_tokens: input,
            output_tokens: output,
            source: UsageSource::Upstream,
            ..Default::default()
        }
    }

    fn pricing(ratio: f64) -> ModelPricing {
        ModelPricing {
            model: "m".into(),
            model_ratio: ratio,
            ..Default::default()
        }
    }

    #[test]
    fn baseline_one_token_per_quota_at_ratio_one() {
        // ratio 1.0 → 每 100 万 token 2 美元；1000 输入 + 500 输出 = 1500 quota
        let b = BillingEngine::settle(&usage(1000, 500), &pricing(1.0), 0);
        assert_eq!(b.total_quota, 1500);
        assert_eq!(b.prompt_quota, 1000);
        assert_eq!(b.completion_quota, 500);
    }

    #[test]
    fn model_ratio_scales_linearly() {
        let b = BillingEngine::settle(&usage(1000, 0), &pricing(3.0), 0);
        assert_eq!(b.total_quota, 3000);
    }

    #[test]
    fn completion_ratio_applies_only_to_output() {
        let mut p = pricing(1.0);
        p.completion_ratio = 4.0;
        let b = BillingEngine::settle(&usage(1000, 500), &p, 0);
        assert_eq!(b.prompt_quota, 1000, "输出倍率不该影响输入");
        assert_eq!(b.completion_quota, 2000);
        assert_eq!(b.total_quota, 3000);
    }

    #[test]
    fn cache_read_is_cheaper_than_fresh_input() {
        let mut p = pricing(1.0);
        p.cache_ratio = 0.1;

        let u = UnifiedUsage {
            input_tokens: 1000,
            cache_read_tokens: 9000,
            output_tokens: 0,
            ..Default::default()
        };
        let b = BillingEngine::settle(&u, &p, 0);

        // 1000*1 + 9000*0.1 = 1900
        assert_eq!(b.total_quota, 1900);
        // 相对原价少花：(1-0.1)*9000 = 8100
        assert_eq!(b.cache_saved_quota, 8100);
        assert_eq!(b.billable_input_tokens, 1000, "只统计 fresh 输入");
    }

    #[test]
    fn cache_write_costs_more_than_fresh_input() {
        let mut p = pricing(1.0);
        p.cache_create_ratio = 1.25;
        let u = UnifiedUsage {
            input_tokens: 0,
            cache_creation_tokens: 1000,
            ..Default::default()
        };
        let b = BillingEngine::settle(&u, &p, 0);
        assert_eq!(b.total_quota, 1250);
    }

    /// 这条守住全项目最容易踩的坑：缓存 token 的服务商口径不同。
    #[test]
    fn no_double_charging_of_cached_tokens() {
        // IR 口径下 input_tokens 是 fresh；缓存单独计。两者不重叠。
        let mut p = pricing(1.0);
        p.cache_ratio = 0.25;

        let u = UnifiedUsage {
            input_tokens: 100,
            cache_read_tokens: 400,
            output_tokens: 0,
            ..Default::default()
        };
        let b = BillingEngine::settle(&u, &p, 0);

        // 若缓存被重复计入 fresh，这里会变成 500 + 100 = 600
        assert_eq!(b.total_quota, 100 + 100, "缓存 token 不得按原价再算一遍");
    }

    #[test]
    fn cache_saved_is_zero_when_cache_is_not_cheaper() {
        let mut p = pricing(1.0);
        p.cache_ratio = 1.0; // 缓存与原价一样
        let u = UnifiedUsage {
            input_tokens: 0,
            cache_read_tokens: 1000,
            ..Default::default()
        };
        assert_eq!(BillingEngine::settle(&u, &p, 0).cache_saved_quota, 0);

        p.cache_ratio = 2.0; // 缓存反而更贵
        assert_eq!(
            BillingEngine::settle(&u, &p, 0).cache_saved_quota,
            0,
            "不计负节省"
        );
    }

    #[test]
    fn group_ratio_multiplies_everything() {
        let mut p = pricing(2.0);
        p.group_ratio = 0.5;
        let b = BillingEngine::settle(&usage(1000, 0), &p, 0);
        assert_eq!(b.total_quota, 1000, "2.0 × 0.5 = 1.0");
    }

    #[test]
    fn other_ratios_multiply_together() {
        let mut p = pricing(1.0);
        p.other_ratios.insert("peak".into(), 2.0);
        p.other_ratios.insert("vip".into(), 0.5);
        let b = BillingEngine::settle(&usage(1000, 0), &p, 0);
        assert_eq!(b.total_quota, 1000, "×2 ×0.5 = ×1");
    }

    #[test]
    fn tool_call_surcharge_is_added_per_call() {
        let mut p = pricing(1.0);
        p.tool_call_surcharge = 100;
        let b = BillingEngine::settle(&usage(0, 0), &p, 3);
        assert_eq!(b.tool_quota, 300);
        assert_eq!(b.total_quota, 300);
    }

    #[test]
    fn tool_surcharge_adds_on_top_of_token_cost() {
        let mut p = pricing(1.0);
        p.tool_call_surcharge = 50;
        let b = BillingEngine::settle(&usage(100, 0), &p, 2);
        assert_eq!(b.total_quota, 200);
    }

    #[test]
    fn free_model_costs_nothing() {
        let p = pricing(0.0);
        let b = BillingEngine::settle(&usage(100_000, 50_000), &p, 5);
        assert_eq!(b.total_quota, 0);
        assert_eq!(b.cache_saved_quota, 0);
    }

    #[test]
    fn tiny_request_still_costs_at_least_one() {
        // 取整后归零的请求若不计费，大量小额调用会完全漏账
        let p = pricing(0.0001);
        let b = BillingEngine::settle(&usage(1, 0), &p, 0);
        assert_eq!(b.total_quota, 1);
    }

    #[test]
    fn zero_usage_on_paid_model_costs_one() {
        let b = BillingEngine::settle(&usage(0, 0), &pricing(1.0), 0);
        assert_eq!(b.total_quota, 1);
    }

    #[test]
    fn breakdown_parts_reconcile_with_total() {
        let mut p = pricing(2.0);
        p.completion_ratio = 3.0;
        p.cache_ratio = 0.5;
        p.tool_call_surcharge = 10;

        let u = UnifiedUsage {
            input_tokens: 100,
            cache_read_tokens: 200,
            cache_creation_tokens: 50,
            output_tokens: 80,
            ..Default::default()
        };
        let b = BillingEngine::settle(&u, &p, 2);

        assert_eq!(b.prompt_quota + b.completion_quota + b.tool_quota, b.total_quota);
        // 手工核算：prompt_units = 100 + 200*0.5 + 50*1.25 = 262.5
        // completion_units = 80*3 = 240
        // ×2.0 = 1005 → 四舍五入 1005；+ tool 20 = 1025
        assert_eq!(b.total_quota, 1025);
    }

    #[test]
    fn pathological_pricing_does_not_panic() {
        let mut p = pricing(1e300);
        p.group_ratio = 1e300;
        let b = BillingEngine::settle(&usage(u64::MAX, u64::MAX), &p, 1000);
        assert!(b.total_quota > 0, "极端倍率应被夹住而不是溢出崩溃");
    }
}
