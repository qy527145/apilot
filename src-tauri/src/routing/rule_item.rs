//! 规则条件。
//!
//! 这里用**封闭枚举**而不是 `trait RuleItem` 的对象列表：条件类型是固定的一套，
//! 枚举既能直接 serde 成前端契约里的 JSON 树，又免去装箱与 trait object 的间接层。
//! 真要扩展时加一个变体即可，编译器会强制所有 `matches` 分支覆盖到它。

use serde::{Deserialize, Serialize};

use super::metadata::RouteMetadata;
use crate::protocol::dto::Protocol;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogicMode {
    And,
    Or,
}

/// 一个匹配条件（叶子或逻辑组合）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleItem {
    /// 客户端标识在给定集合内。
    Client { any: Vec<String> },

    /// 模型名匹配任一 glob 模式。
    Model { patterns: Vec<String> },

    /// 入站协议在给定集合内。
    Protocol { any: Vec<Protocol> },

    /// 请求路径以任一前缀开头。
    Path { prefixes: Vec<String> },

    /// 存在某个请求头；给了 `equals` 时还要求值相等。
    Header {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        equals: Option<String>,
    },

    /// 估算输入 token 落在区间内（边界可选，闭区间）。
    TokenEstimate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u64>,
    },

    /// 逻辑组合，可嵌套。
    Logical {
        mode: LogicMode,
        #[serde(default)]
        invert: bool,
        rules: Vec<RuleItem>,
    },
}

impl RuleItem {
    /// 求值。
    pub fn matches(&self, meta: &RouteMetadata) -> bool {
        match self {
            Self::Client { any } => any.iter().any(|c| c.eq_ignore_ascii_case(&meta.client)),

            Self::Model { patterns } => patterns
                .iter()
                .any(|p| glob_match(p, &meta.model)),

            Self::Protocol { any } => any.contains(&meta.protocol),

            Self::Path { prefixes } => prefixes.iter().any(|p| meta.path.starts_with(p)),

            Self::Header { name, equals } => match meta.header(name) {
                None => false,
                Some(v) => match equals {
                    None => true,
                    Some(expected) => v == expected,
                },
            },

            Self::TokenEstimate { min, max } => {
                let t = meta.est_input_tokens;
                min.map(|m| t >= m).unwrap_or(true) && max.map(|m| t <= m).unwrap_or(true)
            }

            Self::Logical {
                mode,
                invert,
                rules,
            } => {
                // 空规则集的 and 为真、or 为假，与数学约定一致；
                // 但这几乎总是配置失误，所以额外记一条日志便于排查。
                if rules.is_empty() {
                    tracing::debug!("路由规则里的逻辑组合为空，按中性值处理");
                }
                let raw = match mode {
                    LogicMode::And => rules.iter().all(|r| r.matches(meta)),
                    LogicMode::Or => rules.iter().any(|r| r.matches(meta)),
                };
                raw ^ invert
            }
        }
    }

    /// 生成给 UI 看的一句话摘要。
    pub fn describe(&self) -> String {
        match self {
            Self::Client { any } => format!("客户端 ∈ [{}]", any.join(", ")),
            Self::Model { patterns } => format!("模型匹配 [{}]", patterns.join(", ")),
            Self::Protocol { any } => format!(
                "协议 ∈ [{}]",
                any.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
            ),
            Self::Path { prefixes } => format!("路径以 [{}] 开头", prefixes.join(", ")),
            Self::Header { name, equals } => match equals {
                Some(v) => format!("请求头 {name} = {v}"),
                None => format!("存在请求头 {name}"),
            },
            Self::TokenEstimate { min, max } => match (min, max) {
                (Some(a), Some(b)) => format!("输入 token 在 {a}~{b}"),
                (Some(a), None) => format!("输入 token ≥ {a}"),
                (None, Some(b)) => format!("输入 token ≤ {b}"),
                (None, None) => "任意输入 token".to_string(),
            },
            Self::Logical {
                mode,
                invert,
                rules,
            } => {
                let op = match mode {
                    LogicMode::And => " 且 ",
                    LogicMode::Or => " 或 ",
                };
                let inner = rules
                    .iter()
                    .map(|r| r.describe())
                    .collect::<Vec<_>>()
                    .join(op);
                let body = if rules.len() > 1 {
                    format!("({inner})")
                } else {
                    inner
                };
                if *invert {
                    format!("非 {body}")
                } else {
                    body
                }
            }
        }
    }
}

/// 简单 glob 匹配，只支持 `*`（任意长度）与 `?`（单字符）。
///
/// 不用 `glob` crate：模型名不需要 `[...]`、`{a,b}` 这类特性，
/// 而每请求为每条规则编译一次 Pattern 是纯浪费。这里是 O(n·m) 的经典回溯，
/// 输入规模（模型名字符数）极小，完全够用。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    // 双指针 + 星号回溯点：pi/ti 是当前位置，star 记录最近一个 `*` 的位置。
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            // 回退：让上一个 `*` 多吞一个字符再试。
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::Protocol;

    fn meta(model: &str, client: &str) -> RouteMetadata {
        let mut m = RouteMetadata::new(
            model,
            Protocol::AnthropicMessages,
            "/v1/messages",
            http::HeaderMap::new(),
        );
        m.client = client.to_string();
        m
    }

    // --- glob ---

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("claude-sonnet-5", "claude-sonnet-5"));
        assert!(!glob_match("claude-sonnet-5", "claude-opus-5"));
    }

    #[test]
    fn glob_star_matches_prefix_and_suffix() {
        assert!(glob_match("claude-*", "claude-sonnet-5"));
        assert!(glob_match("*-5", "claude-sonnet-5"));
        assert!(!glob_match("claude-*", "gpt-5"));
    }

    #[test]
    fn glob_star_matches_empty() {
        assert!(glob_match("claude*", "claude"));
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn glob_question_mark_matches_exactly_one() {
        assert!(glob_match("gpt-?", "gpt-5"));
        assert!(!glob_match("gpt-?", "gpt-55"));
        assert!(!glob_match("gpt-?", "gpt-"));
    }

    #[test]
    fn glob_multiple_stars() {
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("*-*-*", "a-b-c"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn glob_backtracking_case() {
        // 经典回溯陷阱：`*` 需要回退重试
        assert!(glob_match("a*ab", "aaab"));
        assert!(glob_match("*a*b*", "xaybz"));
    }

    #[test]
    fn glob_handles_multibyte() {
        assert!(glob_match("模型-*", "模型-一"));
        assert!(glob_match("中?", "中文"));
    }

    // --- 规则求值 ---

    #[test]
    fn client_rule_is_case_insensitive() {
        let r = RuleItem::Client {
            any: vec!["codex".into()],
        };
        assert!(r.matches(&meta("m", "Codex")));
        assert!(!r.matches(&meta("m", "claude-code")));
    }

    #[test]
    fn model_rule_uses_glob() {
        let r = RuleItem::Model {
            patterns: vec!["claude-*".into()],
        };
        assert!(r.matches(&meta("claude-sonnet-5", "x")));
        assert!(!r.matches(&meta("gpt-5", "x")));
    }

    #[test]
    fn protocol_rule_matches_inbound_protocol() {
        let r = RuleItem::Protocol {
            any: vec![Protocol::AnthropicMessages],
        };
        assert!(r.matches(&meta("m", "x")));
        assert!(!r.matches(&RouteMetadata::new(
            "m",
            Protocol::OpenAiChat,
            "/v1/chat/completions",
            http::HeaderMap::new()
        )));
    }

    #[test]
    fn path_rule_matches_prefix() {
        let r = RuleItem::Path {
            prefixes: vec!["/v1/messages".into()],
        };
        assert!(r.matches(&meta("m", "x")));
    }

    #[test]
    fn header_rule_with_and_without_equals() {
        let mut h = http::HeaderMap::new();
        h.insert("x-tenant", "acme".parse().unwrap());
        let m = RouteMetadata::new("m", Protocol::OpenAiChat, "/p", h);

        assert!(RuleItem::Header {
            name: "x-tenant".into(),
            equals: None
        }
        .matches(&m));
        assert!(RuleItem::Header {
            name: "x-tenant".into(),
            equals: Some("acme".into())
        }
        .matches(&m));
        assert!(!RuleItem::Header {
            name: "x-tenant".into(),
            equals: Some("other".into())
        }
        .matches(&m));
        assert!(!RuleItem::Header {
            name: "absent".into(),
            equals: None
        }
        .matches(&m));
    }

    #[test]
    fn token_estimate_bounds_are_inclusive() {
        let mut m = meta("m", "x");
        m.est_input_tokens = 100;

        assert!(RuleItem::TokenEstimate {
            min: Some(100),
            max: Some(100)
        }
        .matches(&m));
        assert!(RuleItem::TokenEstimate {
            min: Some(50),
            max: None
        }
        .matches(&m));
        assert!(RuleItem::TokenEstimate {
            min: None,
            max: Some(200)
        }
        .matches(&m));
        assert!(!RuleItem::TokenEstimate {
            min: Some(101),
            max: None
        }
        .matches(&m));
        assert!(!RuleItem::TokenEstimate {
            min: None,
            max: Some(99)
        }
        .matches(&m));
        // 无边界 = 恒真
        assert!(RuleItem::TokenEstimate {
            min: None,
            max: None
        }
        .matches(&m));
    }

    #[test]
    fn logical_and_or_invert() {
        let m = meta("claude-sonnet-5", "codex");
        let is_claude = RuleItem::Model {
            patterns: vec!["claude-*".into()],
        };
        let is_gpt = RuleItem::Model {
            patterns: vec!["gpt-*".into()],
        };

        assert!(RuleItem::Logical {
            mode: LogicMode::And,
            invert: false,
            rules: vec![is_claude.clone(), RuleItem::Client { any: vec!["codex".into()] }],
        }
        .matches(&m));

        assert!(!RuleItem::Logical {
            mode: LogicMode::And,
            invert: false,
            rules: vec![is_claude.clone(), is_gpt.clone()],
        }
        .matches(&m));

        assert!(RuleItem::Logical {
            mode: LogicMode::Or,
            invert: false,
            rules: vec![is_claude.clone(), is_gpt.clone()],
        }
        .matches(&m));

        assert!(RuleItem::Logical {
            mode: LogicMode::Or,
            invert: true,
            rules: vec![is_gpt.clone()],
        }
        .matches(&m));
    }

    #[test]
    fn nested_logical_rules() {
        let m = meta("claude-sonnet-5", "codex");
        let inner = RuleItem::Logical {
            mode: LogicMode::Or,
            invert: false,
            rules: vec![
                RuleItem::Model {
                    patterns: vec!["gpt-*".into()],
                },
                RuleItem::Model {
                    patterns: vec!["gemini-*".into()],
                },
            ],
        };
        let outer = RuleItem::Logical {
            mode: LogicMode::And,
            invert: false,
            rules: vec![
                RuleItem::Client {
                    any: vec!["codex".into()],
                },
                RuleItem::Logical {
                    mode: LogicMode::And,
                    invert: true,
                    rules: vec![inner],
                },
            ],
        };
        // codex 且「不是 gpt/gemini」→ 真
        assert!(outer.matches(&m));
    }

    #[test]
    fn empty_logical_group_uses_neutral_value() {
        assert!(RuleItem::Logical {
            mode: LogicMode::And,
            invert: false,
            rules: vec![]
        }
        .matches(&meta("m", "x")));
        assert!(!RuleItem::Logical {
            mode: LogicMode::Or,
            invert: false,
            rules: vec![]
        }
        .matches(&meta("m", "x")));
    }

    #[test]
    fn json_shape_matches_frontend_contract() {
        let r = RuleItem::Model {
            patterns: vec!["claude-*".into()],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["type"], "model");
        assert_eq!(v["patterns"][0], "claude-*");

        let logical = RuleItem::Logical {
            mode: LogicMode::Or,
            invert: true,
            rules: vec![r],
        };
        let v = serde_json::to_value(&logical).unwrap();
        assert_eq!(v["type"], "logical");
        assert_eq!(v["mode"], "or");
        assert_eq!(v["invert"], true);
        assert_eq!(v["rules"][0]["type"], "model");
    }

    #[test]
    fn json_roundtrips() {
        let r = RuleItem::Logical {
            mode: LogicMode::And,
            invert: false,
            rules: vec![
                RuleItem::Client {
                    any: vec!["codex".into()],
                },
                RuleItem::TokenEstimate {
                    min: Some(1000),
                    max: None,
                },
            ],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: RuleItem = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn describe_produces_readable_summary() {
        assert_eq!(
            RuleItem::Model {
                patterns: vec!["claude-*".into()]
            }
            .describe(),
            "模型匹配 [claude-*]"
        );
        let logical = RuleItem::Logical {
            mode: LogicMode::Or,
            invert: true,
            rules: vec![
                RuleItem::Client {
                    any: vec!["a".into()],
                },
                RuleItem::Client {
                    any: vec!["b".into()],
                },
            ],
        };
        assert_eq!(logical.describe(), "非 (客户端 ∈ [a] 或 客户端 ∈ [b])");
    }
}
