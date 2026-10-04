//! 路由规则与动作。

use serde::{Deserialize, Serialize};

use super::metadata::RouteMetadata;
use super::rule_item::RuleItem;

/// 规则命中后执行的动作。
///
/// 分**终结**与**非终结**两类，这是规则链的核心机制：
/// - 非终结动作就地改写 `RouteMetadata`，然后**继续**匹配后续规则；
/// - 终结动作一旦命中就选定结果，**立即停止**求值。
///
/// 有了这个区分，就能表达「先按特征打标/改写，再决定去哪」这类多段式策略，
/// 而不用把所有条件塞进一条巨型规则里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RouteAction {
    // ---- 终结 ----

    /// 选定一个 selector 并结束求值。
    Final { selector: String },

    /// 直接拒绝该请求（不转发给任何上游）。
    Reject { reason: String },

    // ---- 非终结 ----

    /// 改写模型名后继续匹配。
    ModelOverride { model: String },

    /// 写入路由选项（强制 selector / 缓存开关）后继续匹配。
    RouteOptions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_selector: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<bool>,
    },

    /// 仅采集特征（当前无副作用），便于用户先用一条规则观察流量再决定怎么分流。
    Sniff,
}

impl RouteAction {
    /// 是否为终结动作。
    pub fn is_final(&self) -> bool {
        matches!(self, Self::Final { .. } | Self::Reject { .. })
    }

    /// 应用非终结动作。终结动作不该走到这里。
    pub fn apply_non_final(&self, meta: &mut RouteMetadata) {
        match self {
            Self::ModelOverride { model } => {
                meta.model = model.clone();
            }
            Self::RouteOptions {
                target_selector,
                cache,
            } => {
                if target_selector.is_some() {
                    meta.options.target_selector = target_selector.clone();
                }
                if cache.is_some() {
                    meta.options.cache = *cache;
                }
            }
            Self::Sniff => {}
            Self::Final { .. } | Self::Reject { .. } => {
                debug_assert!(false, "终结动作不应进入 apply_non_final");
            }
        }
    }

    /// 给 UI 看的一句话摘要。
    pub fn describe(&self) -> String {
        match self {
            Self::Final { selector } => format!("走 selector「{selector}」"),
            Self::Reject { reason } => format!("拒绝：{reason}"),
            Self::ModelOverride { model } => format!("改写模型为 {model}"),
            Self::RouteOptions {
                target_selector,
                cache,
            } => {
                let mut parts = Vec::new();
                if let Some(s) = target_selector {
                    parts.push(format!("指定 selector={s}"));
                }
                if let Some(c) = cache {
                    parts.push(format!("缓存={c}"));
                }
                if parts.is_empty() {
                    "无操作".to_string()
                } else {
                    parts.join("，")
                }
            }
            Self::Sniff => "仅采集特征".to_string(),
        }
    }
}

/// 一条路由规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRule {
    pub id: i64,
    pub sort_index: i64,
    pub name: String,
    pub enabled: bool,
    /// 各条件之间是 **AND** 关系；要表达 OR 请用 `RuleItem::Logical`。
    pub items: Vec<RuleItem>,
    pub action: RouteAction,
    pub updated_at: i64,
}

impl RouteRule {
    /// 该请求是否命中本规则。
    ///
    /// 注意：`enabled == false` 的规则永不命中。空条件列表视为「命中一切」，
    /// 便于用户写一条纯动作用的规则（例如末尾的兜底改写）。
    pub fn matches(&self, meta: &RouteMetadata) -> bool {
        self.enabled && self.items.iter().all(|i| i.matches(meta))
    }

    /// 给 UI 展示的条件摘要。
    pub fn describe_conditions(&self) -> String {
        if self.items.is_empty() {
            return "任意请求".to_string();
        }
        self.items
            .iter()
            .map(|i| i.describe())
            .collect::<Vec<_>>()
            .join(" 且 ")
    }
}

/// 新建 / 更新路由规则的入参。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRuleInput {
    pub id: Option<i64>,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub items: Vec<RuleItem>,
    pub action: RouteAction,
}

fn default_true() -> bool {
    true
}

impl RouteRuleInput {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("规则名称不能为空".into());
        }
        match &self.action {
            RouteAction::Final { selector } if selector.trim().is_empty() => {
                Err("终结动作必须指定 selector".into())
            }
            RouteAction::ModelOverride { model } if model.trim().is_empty() => {
                Err("模型改写不能为空".into())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::Protocol;

    fn meta(model: &str) -> RouteMetadata {
        RouteMetadata::new(model, Protocol::AnthropicMessages, "/v1/messages", Default::default())
    }

    fn rule(items: Vec<RuleItem>, action: RouteAction) -> RouteRule {
        RouteRule {
            id: 1,
            sort_index: 0,
            name: "r".into(),
            enabled: true,
            items,
            action,
            updated_at: 0,
        }
    }

    #[test]
    fn final_and_reject_are_final_others_are_not() {
        assert!(RouteAction::Final {
            selector: "s".into()
        }
        .is_final());
        assert!(RouteAction::Reject {
            reason: "r".into()
        }
        .is_final());
        assert!(!RouteAction::Sniff.is_final());
        assert!(!RouteAction::ModelOverride {
            model: "m".into()
        }
        .is_final());
        assert!(!RouteAction::RouteOptions {
            target_selector: None,
            cache: None
        }
        .is_final());
    }

    #[test]
    fn model_override_rewrites_metadata_model() {
        let mut m = meta("gpt-4o");
        RouteAction::ModelOverride {
            model: "deepseek-chat".into(),
        }
        .apply_non_final(&mut m);
        assert_eq!(m.model, "deepseek-chat");
    }

    #[test]
    fn route_options_only_overwrites_provided_fields() {
        let mut m = meta("m");
        // 先设置一个值
        RouteAction::RouteOptions {
            target_selector: Some("fast".into()),
            cache: None,
        }
        .apply_non_final(&mut m);
        assert_eq!(m.options.target_selector.as_deref(), Some("fast"));
        assert_eq!(m.options.cache, None);

        // 后续规则只改 cache，不应把 selector 抹掉
        RouteAction::RouteOptions {
            target_selector: None,
            cache: Some(true),
        }
        .apply_non_final(&mut m);
        assert_eq!(
            m.options.target_selector.as_deref(),
            Some("fast"),
            "未提供的字段不应被清空"
        );
        assert_eq!(m.options.cache, Some(true));
    }

    #[test]
    fn disabled_rule_never_matches() {
        let mut r = rule(vec![], RouteAction::Sniff);
        r.enabled = false;
        assert!(!r.matches(&meta("m")));
    }

    #[test]
    fn rule_with_no_conditions_matches_everything() {
        let r = rule(vec![], RouteAction::Sniff);
        assert!(r.matches(&meta("anything")));
    }

    #[test]
    fn conditions_are_anded() {
        let r = rule(
            vec![
                RuleItem::Model {
                    patterns: vec!["claude-*".into()],
                },
                RuleItem::Client {
                    any: vec!["codex".into()],
                },
            ],
            RouteAction::Sniff,
        );
        let mut m = meta("claude-sonnet-5");
        m.client = "codex".into();
        assert!(r.matches(&m));

        m.client = "other".into();
        assert!(!r.matches(&m), "任一条件不满足即不命中");
    }

    #[test]
    fn action_json_shape_matches_frontend_contract() {
        let v = serde_json::to_value(RouteAction::Final {
            selector: "default".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "final");
        assert_eq!(v["selector"], "default");

        let v = serde_json::to_value(RouteAction::RouteOptions {
            target_selector: Some("s".into()),
            cache: Some(true),
        })
        .unwrap();
        assert_eq!(v["type"], "route_options");
        assert_eq!(v["target_selector"], "s");
        assert_eq!(v["cache"], true);

        let v = serde_json::to_value(RouteAction::Sniff).unwrap();
        assert_eq!(v["type"], "sniff");
    }

    #[test]
    fn action_json_roundtrips() {
        for a in [
            RouteAction::Final {
                selector: "s".into(),
            },
            RouteAction::Reject {
                reason: "r".into(),
            },
            RouteAction::ModelOverride {
                model: "m".into(),
            },
            RouteAction::RouteOptions {
                target_selector: Some("x".into()),
                cache: Some(false),
            },
            RouteAction::Sniff,
        ] {
            let s = serde_json::to_string(&a).unwrap();
            assert_eq!(serde_json::from_str::<RouteAction>(&s).unwrap(), a);
        }
    }

    #[test]
    fn input_validation() {
        let mut inp = RouteRuleInput {
            id: None,
            name: "".into(),
            enabled: true,
            items: vec![],
            action: RouteAction::Sniff,
        };
        assert!(inp.validate().is_err());

        inp.name = "ok".into();
        assert!(inp.validate().is_ok());

        inp.action = RouteAction::Final {
            selector: "".into(),
        };
        assert!(inp.validate().is_err(), "终结动作必须指定 selector");

        inp.action = RouteAction::ModelOverride {
            model: "".into(),
        };
        assert!(inp.validate().is_err());
    }

    #[test]
    fn describe_helpers() {
        assert_eq!(
            RouteAction::Final {
                selector: "default".into()
            }
            .describe(),
            "走 selector「default」"
        );
        let r = rule(vec![], RouteAction::Sniff);
        assert_eq!(r.describe_conditions(), "任意请求");
    }
}
