//! 规则链求值引擎。
//!
//! 顺序遍历规则，**首个终结动作胜出**；非终结动作就地改写 metadata 后继续。
//! 全部不命中则回落到 `final_selector`。这个模型直接对应 sing-box 的
//! `route/route.go::matchRule`。

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::metadata::RouteMetadata;
use super::rule::RouteRule;

/// 路由结果。
#[derive(Debug, Clone, PartialEq)]
pub enum RouteOutcome {
    /// 选定某个 selector。`matched_rule` 为命中的规则 id，回落时为 `None`。
    Final {
        selector: String,
        matched_rule: Option<i64>,
    },
    /// 请求被规则显式拒绝。
    Reject { reason: String, matched_rule: Option<i64> },
}

impl RouteOutcome {
    pub fn selector(&self) -> Option<&str> {
        match self {
            Self::Final { selector, .. } => Some(selector),
            Self::Reject { .. } => None,
        }
    }

    pub fn matched_rule(&self) -> Option<i64> {
        match self {
            Self::Final { matched_rule, .. } | Self::Reject { matched_rule, .. } => *matched_rule,
        }
    }
}

/// 规则链引擎。规则集热可换。
pub struct Router {
    rules: ArcSwap<Vec<Arc<RouteRule>>>,
    final_selector: ArcSwap<String>,
}

impl Router {
    pub fn new(rules: Vec<Arc<RouteRule>>, final_selector: impl Into<String>) -> Self {
        Self {
            rules: ArcSwap::from_pointee(rules),
            final_selector: ArcSwap::from_pointee(final_selector.into()),
        }
    }

    /// 全量替换规则集与兜底 selector。在途请求持有的旧快照继续有效。
    pub fn reload(&self, rules: Vec<Arc<RouteRule>>, final_selector: impl Into<String>) {
        self.rules.store(Arc::new(rules));
        self.final_selector.store(Arc::new(final_selector.into()));
    }

    pub fn final_selector(&self) -> String {
        self.final_selector.load().as_ref().clone()
    }

    pub fn rule_count(&self) -> usize {
        self.rules.load().len()
    }

    /// 对请求求值。
    ///
    /// `meta` 是 `&mut`，因为非终结动作会改写它（模型名、路由选项），
    /// 调用方在拿到结果后应当继续使用被改写过的 `meta`。
    pub fn route(&self, meta: &mut RouteMetadata) -> RouteOutcome {
        let rules = self.rules.load();

        for rule in rules.iter() {
            if !rule.matches(meta) {
                continue;
            }

            if rule.action.is_final() {
                return match &rule.action {
                    super::rule::RouteAction::Final { selector } => RouteOutcome::Final {
                        selector: selector.clone(),
                        matched_rule: Some(rule.id),
                    },
                    super::rule::RouteAction::Reject { reason } => RouteOutcome::Reject {
                        reason: reason.clone(),
                        matched_rule: Some(rule.id),
                    },
                    // is_final 已经排除了其余分支
                    _ => unreachable!("is_final 与实际动作不一致"),
                };
            }

            // 非终结：改写 metadata 后继续往下匹配。
            rule.action.apply_non_final(meta);
        }

        RouteOutcome::Final {
            selector: self.final_selector(),
            matched_rule: None,
        }
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new(Vec::new(), "default")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::Protocol;
    use crate::routing::rule::{RouteAction, RouteRule};
    use crate::routing::rule_item::RuleItem;

    fn meta(model: &str) -> RouteMetadata {
        RouteMetadata::new(
            model,
            Protocol::AnthropicMessages,
            "/v1/messages",
            Default::default(),
        )
    }

    fn rule(id: i64, items: Vec<RuleItem>, action: RouteAction) -> Arc<RouteRule> {
        Arc::new(RouteRule {
            id,
            sort_index: id,
            name: format!("rule-{id}"),
            enabled: true,
            items,
            action,
            updated_at: 0,
        })
    }

    fn model_matches(pat: &str) -> RuleItem {
        RuleItem::Model {
            patterns: vec![pat.into()],
        }
    }

    #[test]
    fn falls_back_to_final_selector_when_nothing_matches() {
        let r = Router::new(vec![], "default");
        let mut m = meta("anything");
        assert_eq!(
            r.route(&mut m),
            RouteOutcome::Final {
                selector: "default".into(),
                matched_rule: None
            }
        );
    }

    #[test]
    fn first_matching_final_rule_wins() {
        let r = Router::new(
            vec![
                rule(1, vec![model_matches("claude-*")], RouteAction::Final { selector: "first".into() }),
                rule(2, vec![model_matches("claude-*")], RouteAction::Final { selector: "second".into() }),
            ],
            "default",
        );

        let mut m = meta("claude-sonnet-5");
        let out = r.route(&mut m);
        assert_eq!(out.selector(), Some("first"), "顺序靠前的规则优先");
        assert_eq!(out.matched_rule(), Some(1));
    }

    #[test]
    fn non_matching_rule_is_skipped() {
        let r = Router::new(
            vec![
                rule(1, vec![model_matches("gpt-*")], RouteAction::Final { selector: "a".into() }),
                rule(2, vec![model_matches("claude-*")], RouteAction::Final { selector: "b".into() }),
            ],
            "default",
        );
        let mut m = meta("claude-sonnet-5");
        assert_eq!(r.route(&mut m).selector(), Some("b"));
    }

    #[test]
    fn disabled_rule_is_skipped() {
        let mut disabled = RouteRule {
            id: 1,
            sort_index: 0,
            name: "off".into(),
            enabled: false,
            items: vec![],
            action: RouteAction::Final {
                selector: "never".into(),
            },
            updated_at: 0,
        };
        disabled.enabled = false;

        let r = Router::new(vec![Arc::new(disabled)], "default");
        let mut m = meta("m");
        assert_eq!(r.route(&mut m).selector(), Some("default"));
    }

    #[test]
    fn model_override_affects_later_rules() {
        // 这是非终结动作存在的意义：先改写，再按改写后的值分流。
        let r = Router::new(
            vec![
                rule(
                    1,
                    vec![model_matches("gpt-4o")],
                    RouteAction::ModelOverride {
                        model: "deepseek-chat".into(),
                    },
                ),
                rule(
                    2,
                    vec![model_matches("deepseek-*")],
                    RouteAction::Final {
                        selector: "deepseek-pool".into(),
                    },
                ),
            ],
            "default",
        );

        let mut m = meta("gpt-4o");
        let out = r.route(&mut m);
        assert_eq!(out.selector(), Some("deepseek-pool"));
        assert_eq!(m.model, "deepseek-chat", "metadata 应被改写供转发层使用");
    }

    #[test]
    fn route_options_accumulate_across_rules() {
        let r = Router::new(
            vec![
                rule(
                    1,
                    vec![model_matches("claude-*")],
                    RouteAction::RouteOptions {
                        target_selector: Some("claude-pool".into()),
                        cache: None,
                    },
                ),
                rule(
                    2,
                    vec![model_matches("claude-*")],
                    RouteAction::RouteOptions {
                        target_selector: None,
                        cache: Some(true),
                    },
                ),
                rule(3, vec![model_matches("claude-*")], RouteAction::Final { selector: "ignored".into() }),
            ],
            "default",
        );

        let mut m = meta("claude-sonnet-5");
        let out = r.route(&mut m);
        assert_eq!(out.selector(), Some("ignored"));
        assert_eq!(
            m.options.target_selector.as_deref(),
            Some("claude-pool"),
            "前一条规则设置的 selector 不应被后一条清空"
        );
        assert_eq!(m.options.cache, Some(true));
    }

    #[test]
    fn reject_stops_evaluation() {
        let r = Router::new(
            vec![
                rule(
                    1,
                    vec![model_matches("blocked-*")],
                    RouteAction::Reject {
                        reason: "该模型已禁用".into(),
                    },
                ),
                rule(2, vec![], RouteAction::Final { selector: "fallback".into() }),
            ],
            "default",
        );

        let mut m = meta("blocked-model");
        match r.route(&mut m) {
            RouteOutcome::Reject { reason, matched_rule } => {
                assert_eq!(reason, "该模型已禁用");
                assert_eq!(matched_rule, Some(1));
            }
            other => panic!("期望 Reject，得到 {other:?}"),
        }
    }

    #[test]
    fn reject_takes_priority_over_later_final() {
        let r = Router::new(
            vec![
                rule(1, vec![], RouteAction::Reject { reason: "no".into() }),
                rule(2, vec![], RouteAction::Final { selector: "yes".into() }),
            ],
            "default",
        );
        let mut m = meta("m");
        assert!(matches!(r.route(&mut m), RouteOutcome::Reject { .. }));
    }

    #[test]
    fn reload_swaps_rules_atomically() {
        let r = Router::new(
            vec![rule(1, vec![], RouteAction::Final { selector: "old".into() })],
            "default",
        );
        let mut m = meta("m");
        assert_eq!(r.route(&mut m).selector(), Some("old"));

        r.reload(
            vec![rule(2, vec![], RouteAction::Final { selector: "new".into() })],
            "new-default",
        );
        assert_eq!(r.route(&mut m).selector(), Some("new"));
        assert_eq!(r.final_selector(), "new-default");
        assert_eq!(r.rule_count(), 1);
    }

    #[test]
    fn reload_can_change_final_selector() {
        let r = Router::new(vec![], "a");
        let mut m = meta("m");
        assert_eq!(r.route(&mut m).selector(), Some("a"));
        r.reload(vec![], "b");
        assert_eq!(r.route(&mut m).selector(), Some("b"));
    }

    #[test]
    fn sniff_rule_does_not_stop_evaluation() {
        let r = Router::new(
            vec![
                rule(1, vec![model_matches("claude-*")], RouteAction::Sniff),
                rule(2, vec![], RouteAction::Final { selector: "after".into() }),
            ],
            "default",
        );
        let mut m = meta("claude-sonnet-5");
        assert_eq!(r.route(&mut m).selector(), Some("after"));
    }
}
