//! 按模型策略给候选渠道排序。
//!
//! 排序是纯函数：随机源（加权策略要用）与延迟都从外面传进来，所以三种策略
//! 都能脱离数据库与真实时钟测。

use crate::storage::models::{ModelStrategy, ModelPolicyRecord};

/// 一条待排序的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub tag: String,
    /// 这个模型在该渠道上的优先级 / 权重。
    pub priority: i64,
    pub weight: i64,
    /// 最近一次测速的延迟；没测过是 `None`。
    pub latency_ms: Option<i64>,
}

/// 给候选排序，返回渠道 tag 列表 —— **第一个是主渠道**，其余是故障转移候选。
///
/// `roll` 是 [0, 1) 的随机数，只有加权策略用得到；其余策略忽略它。
/// 直接从外面传是为了让测试能给定值，而不是去 mock 随机数发生器。
pub fn order(
    policy: Option<&ModelPolicyRecord>,
    mut candidates: Vec<Candidate>,
    roll: f64,
) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // 基准序：优先级降序、权重降序。这也是 `priority` 策略的最终结果。
    // 同分时保持调用方给进来的次序（`sort_by` 是稳定排序）—— 那个次序来自
    // `channels_for_model` 的 SQL，本身已经按渠道优先级排过，是合理的兜底。
    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(b.weight.cmp(&a.weight))
    });

    let strategy = policy.map(|p| p.strategy).unwrap_or(ModelStrategy::Priority);

    match strategy {
        ModelStrategy::Priority => {}

        ModelStrategy::Latency => {
            // 没测过的排到最后 —— 不能因为"还没测"就当它是最快的。
            candidates.sort_by_key(|c| (c.latency_ms.is_none(), c.latency_ms.unwrap_or(i64::MAX)));
        }

        ModelStrategy::Weight => {
            // 只把**主渠道**换成加权随机的结果，其余仍按优先级依次兜底：
            // 故障转移要的是"最靠得住的那个"，不是再随机一次。
            if let Some(idx) = weighted_pick(&candidates, roll) {
                let primary = candidates.remove(idx);
                candidates.insert(0, primary);
            }
        }
    }

    // 手动切换的渠道最后盖过策略结果 —— 那是用户的显式选择，比自动策略优先。
    if let Some(active) = policy.and_then(|p| p.active_provider.as_deref()) {
        if let Some(idx) = candidates.iter().position(|c| c.tag == active) {
            let chosen = candidates.remove(idx);
            candidates.insert(0, chosen);
        }
        // active 不在候选里（渠道被停用或删了）就按策略走，而不是失败。
    }

    candidates.into_iter().map(|c| c.tag).collect()
}

/// 按权重随机挑一个下标。权重全为 0 或负数时回落到第一个。
fn weighted_pick(candidates: &[Candidate], roll: f64) -> Option<usize> {
    let total: i64 = candidates.iter().map(|c| c.weight.max(0)).sum();
    if total <= 0 {
        return Some(0);
    }

    // 把 [0,1) 映射到 [0, total)，再沿线累加找到落点。
    let target = (roll.clamp(0.0, 0.999_999) * total as f64) as i64;
    let mut acc = 0i64;
    for (i, c) in candidates.iter().enumerate() {
        acc += c.weight.max(0);
        if target < acc {
            return Some(i);
        }
    }
    Some(candidates.len() - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(tag: &str, priority: i64, weight: i64, latency: Option<i64>) -> Candidate {
        Candidate {
            tag: tag.into(),
            priority,
            weight,
            latency_ms: latency,
        }
    }

    fn policy(strategy: ModelStrategy, active: Option<&str>) -> ModelPolicyRecord {
        ModelPolicyRecord {
            model: "m".into(),
            strategy,
            active_provider: active.map(String::from),
        }
    }

    fn tags(cs: Vec<Candidate>, p: Option<&ModelPolicyRecord>) -> Vec<String> {
        order(p, cs, 0.0)
    }

    #[test]
    fn priority_strategy_prefers_higher_priority() {
        let p = policy(ModelStrategy::Priority, None);
        assert_eq!(
            tags(
                vec![c("lo", 1, 1, None), c("hi", 10, 1, None), c("mid", 5, 1, None)],
                Some(&p)
            ),
            vec!["hi", "mid", "lo"]
        );
    }

    #[test]
    fn weight_breaks_priority_ties() {
        let p = policy(ModelStrategy::Priority, None);
        assert_eq!(
            tags(
                vec![c("light", 5, 1, None), c("heavy", 5, 9, None)],
                Some(&p)
            ),
            vec!["heavy", "light"]
        );
    }

    #[test]
    fn latency_strategy_puts_unmeasured_last() {
        // 还没测过的渠道不能因为"没有延迟数据"被当成最快的。
        let p = policy(ModelStrategy::Latency, None);
        assert_eq!(
            tags(
                vec![
                    c("unmeasured", 100, 1, None),
                    c("slow", 1, 1, Some(900)),
                    c("fast", 1, 1, Some(120)),
                ],
                Some(&p)
            ),
            vec!["fast", "slow", "unmeasured"],
            "优先级高但没测过，也要排在测过的后面"
        );
    }

    #[test]
    fn latency_strategy_ignores_priority() {
        let p = policy(ModelStrategy::Latency, None);
        assert_eq!(
            tags(
                vec![c("hi-prio", 99, 1, Some(800)), c("lo-prio", 0, 1, Some(100))],
                Some(&p)
            ),
            vec!["lo-prio", "hi-prio"]
        );
    }

    #[test]
    fn weight_strategy_picks_by_roll_and_keeps_the_rest_ordered() {
        let p = policy(ModelStrategy::Weight, None);
        let cs = vec![
            c("a", 10, 1, None),
            c("b", 5, 3, None),
            c("cc", 1, 6, None),
        ];

        // 权重合计 10：roll 落在 [0,0.1) → a，(0.1,0.4] → b，(0.4,1) → cc。
        assert_eq!(order(Some(&p), cs.clone(), 0.05)[0], "a");
        assert_eq!(order(Some(&p), cs.clone(), 0.30)[0], "b");
        assert_eq!(order(Some(&p), cs.clone(), 0.90)[0], "cc");

        // 被选中的排第一，其余保持基准序（不是再随机一次）。
        let with_c = order(Some(&p), cs, 0.90);
        assert_eq!(with_c, vec!["cc", "a", "b"]);
    }

    #[test]
    fn zero_weights_do_not_panic() {
        // 权重全为 0 时回落第一个，而不是除零或返回空。
        let p = policy(ModelStrategy::Weight, None);
        let cs = vec![c("a", 5, 0, None), c("b", 1, 0, None)];
        assert_eq!(order(Some(&p), cs, 0.7)[0], "a");
    }

    #[test]
    fn roll_at_the_top_of_the_range_still_lands_inside() {
        // roll 理论上 <1，但真接到 1.0 也不该越界或漏选。
        let p = policy(ModelStrategy::Weight, None);
        let cs = vec![c("a", 1, 1, None), c("b", 1, 1, None)];
        assert_eq!(order(Some(&p), cs.clone(), 1.0).len(), 2);
        assert_eq!(order(Some(&p), cs, -0.5).len(), 2);
    }

    #[test]
    fn manual_switch_beats_every_strategy() {
        let p = policy(ModelStrategy::Priority, Some("lo"));
        assert_eq!(
            tags(vec![c("hi", 10, 1, None), c("lo", 1, 1, None)], Some(&p)),
            vec!["lo", "hi"],
            "手动切换是用户的显式选择，优先级再低也该排第一"
        );
    }

    #[test]
    fn manual_switch_to_a_vanished_provider_falls_back_to_the_strategy() {
        // 渠道被停用或删掉之后，策略行里可能还留着它的 tag。
        // 这时按策略走，而不是让请求失败。
        let p = policy(ModelStrategy::Priority, Some("gone"));
        assert_eq!(
            tags(vec![c("hi", 10, 1, None), c("lo", 1, 1, None)], Some(&p)),
            vec!["hi", "lo"]
        );
    }

    #[test]
    fn no_policy_row_behaves_like_priority() {
        // 「没配过」等价于按优先级 —— 与改动前 channels_for_model 的排序一致。
        assert_eq!(
            tags(
                vec![c("a", 1, 1, None), c("b", 9, 1, None)],
                None
            ),
            vec!["b", "a"]
        );
    }

    #[test]
    fn stable_for_equal_candidates() {
        // 完全同分时保持调用方给的次序（那个次序来自 SQL，已经排过一轮）。
        let cs = vec![c("first", 1, 1, None), c("second", 1, 1, None)];
        assert_eq!(tags(cs, None), vec!["first", "second"]);
    }

    #[test]
    fn empty_candidates_produce_empty_order() {
        assert!(order(None, Vec::new(), 0.5).is_empty());
    }
}
