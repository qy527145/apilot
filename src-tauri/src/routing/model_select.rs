//! 按模型的渠道优先级给候选渠道排序。
//!
//! 只剩一条规则：**优先级高的在前，点名的渠道提到最前**。排序是纯函数，
//! 没有随机、没有测速 —— 「这个模型现在走哪个渠道」必须在界面上说得清，
//! 而每次请求重抽、或跟着测速结果漂移的选法，用户既复现不了也解释不清。

use crate::storage::models::ModelPolicyRecord;

/// 一条待排序的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub tag: String,
    /// 这个模型在该渠道上的优先级，高者先。
    pub priority: i64,
}

/// 给候选排序，返回渠道 tag 列表 —— **第一个是主渠道**，其余按顺序故障转移。
pub fn order(policy: Option<&ModelPolicyRecord>, mut candidates: Vec<Candidate>) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // 稳定排序：同优先级时保持调用方给进来的次序（来自 SQL，已按渠道优先级排过），
    // 那是合理的兜底，也让结果的确定性由数据本身决定。
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority));

    // 点名的渠道盖过优先级 —— 那是用户的显式选择。
    if let Some(active) = policy.and_then(|p| p.active_provider.as_deref()) {
        if let Some(idx) = candidates.iter().position(|c| c.tag == active) {
            let chosen = candidates.remove(idx);
            candidates.insert(0, chosen);
        }
        // 点名的渠道不在候选里（被停用或删了）就按优先级走，而不是失败。
    }

    candidates.into_iter().map(|c| c.tag).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(tag: &str, priority: i64) -> Candidate {
        Candidate {
            tag: tag.into(),
            priority,
        }
    }

    fn policy(active: Option<&str>) -> ModelPolicyRecord {
        ModelPolicyRecord {
            model: "m".into(),
            active_provider: active.map(String::from),
        }
    }

    #[test]
    fn higher_priority_comes_first() {
        assert_eq!(
            order(
                Some(&policy(None)),
                vec![c("lo", 1), c("hi", 10), c("mid", 5)]
            ),
            vec!["hi", "mid", "lo"]
        );
    }

    #[test]
    fn no_policy_row_still_orders_by_priority() {
        assert_eq!(
            order(None, vec![c("a", 1), c("b", 9)]),
            vec!["b", "a"],
            "没配过策略的模型按优先级排，与改动前一致"
        );
    }

    #[test]
    fn the_pinned_channel_always_comes_first() {
        assert_eq!(
            order(Some(&policy(Some("lo"))), vec![c("hi", 10), c("lo", 1)]),
            vec!["lo", "hi"],
            "点名的渠道是显式选择，优先级再低也该排第一"
        );
    }

    #[test]
    fn the_rest_keep_priority_order_after_the_pin() {
        assert_eq!(
            order(
                Some(&policy(Some("mid"))),
                vec![c("hi", 10), c("mid", 5), c("lo", 1)]
            ),
            vec!["mid", "hi", "lo"],
            "点名只影响第一名，其余仍按优先级 —— 故障转移顺序不该被打乱"
        );
    }

    #[test]
    fn a_pin_to_a_vanished_channel_falls_back_to_priority() {
        // 渠道被停用或删掉之后，策略行里可能还留着它的 tag。
        assert_eq!(
            order(Some(&policy(Some("gone"))), vec![c("hi", 10), c("lo", 1)]),
            vec!["hi", "lo"]
        );
    }

    #[test]
    fn stable_for_equal_priorities() {
        assert_eq!(
            order(None, vec![c("first", 1), c("second", 1)]),
            vec!["first", "second"],
            "同分时保持调用方给的次序"
        );
    }

    #[test]
    fn empty_candidates_produce_empty_order() {
        assert!(order(None, Vec::new()).is_empty());
    }
}
