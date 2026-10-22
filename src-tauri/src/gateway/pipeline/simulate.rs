//! 规则测试：给定一组请求条件，空跑一遍选路，看会落到哪个渠道的哪个模型。
//!
//! 走的是**线上同一份**判定代码 —— 客户端识别（`detect_client_with_source`）、
//! 模型策略（`policy_model`）、会话固定、规则链（`Router::route_traced`）、
//! selector、候选排序（`build_candidates_explained`）、渠道模型映射。这里只负责
//! 把它们串起来并记下每一步的结果，不重写任何一条判据：测试和线上一旦各算各的，
//! 测出来的就是另一套路由，比没有测试更误导。
//!
//! 不发请求、不写日志、不计费、不碰缓存。

use std::sync::Arc;

use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use super::candidates::{build_candidates_explained, CandidateBasis};
use super::{detect_client_with_source, policy_model, ClientSource};
use crate::protocol::dto::Protocol;
use crate::routing::{RouteMetadata, RouteOutcome};
use crate::shell::AppShell;

/// 模拟请求的条件。除了协议与模型，其余都可不填。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SimulateInput {
    pub protocol: Option<Protocol>,
    /// 客户端请求的模型名。
    pub model: String,
    /// 客户端带的令牌（按 `Authorization: Bearer` 发）。接管写的是 `apilot-<客户端>`。
    pub token: Option<String>,
    pub user_agent: Option<String>,
    /// 其余请求头（`x-apilot-client`、规则里按请求头匹配的那些）。
    pub headers: Vec<(String, String)>,
    /// 空 = 该协议的标准路径。
    pub path: Option<String>,
    pub stream: bool,
    /// 估算输入 token，给 `token_estimate` 条件用。
    pub est_input_tokens: u64,
    /// 会话 ID：填了才会考虑对话级模型固定。
    pub session_id: Option<String>,
    /// 请求带不带工具 —— 「只固定带工具的请求」那档固定看它。
    pub has_tools: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SimulateResult {
    pub client: String,
    pub client_source: ClientSource,
    pub protocol: Protocol,
    pub path: String,
    /// 每一步之后的模型名，按生效顺序排列。
    pub model_steps: Vec<ModelStep>,
    /// 规则链里**被求值过**的规则（终结动作之后的不在里面）。
    pub rules: Vec<RuleTrace>,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelStep {
    /// `request` / `policy` / `session_pin` / `rule`
    pub stage: &'static str,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuleTrace {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub matched: bool,
    pub conditions: String,
    pub action: String,
    /// 命中且是终结动作：求值到此为止。
    pub decisive: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Rejected {
        reason: String,
        rule_id: Option<i64>,
    },
    /// selector 解不出渠道（没配 / 成员全停用且没有默认渠道）。线上这里回 503。
    NoChannel { selector: String },
    Routed {
        selector: String,
        /// 命中的终结规则；`None` = 回落到兜底 selector。
        rule_id: Option<i64>,
        /// 规则链之后的生效模型（计费、缓存键都按它）。
        model: String,
        basis: CandidateBasis,
        /// 第一个是主渠道，其余按顺序故障转移。
        candidates: Vec<CandidateView>,
        /// 结果里带随机性或这里没模拟的部分，给用户的提醒。
        notes: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateView {
    pub tag: String,
    pub name: String,
    /// 渠道映射后真正发给上游的模型名。
    pub upstream_model: String,
    pub wire: Protocol,
    /// 同协议直通（不转换）。
    pub passthrough: bool,
    pub url: String,
}

pub async fn simulate(shell: &Arc<AppShell>, input: SimulateInput) -> SimulateResult {
    let protocol = input.protocol.unwrap_or(Protocol::AnthropicMessages);
    let path = input
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(protocol.default_path())
        .to_string();
    let headers = build_headers(&input);
    let (client, client_source) = detect_client_with_source(&headers, &path);

    let mut model_steps = vec![ModelStep {
        stage: "request",
        model: input.model.clone(),
    }];
    let mut push_step = |stage, model: &str| {
        if model_steps.last().map(|s| s.model.as_str()) != Some(model) {
            model_steps.push(ModelStep {
                stage,
                model: model.to_string(),
            });
        }
    };

    let model = policy_model(shell, &client, &input.model, protocol.as_str()).await;
    push_step("policy", &model);

    // 与 `Recorder::apply_session_pin` 同一个判据；这里没有 Recorder，直接问固定表。
    let model = shell
        .session_pins
        .model_for(input.session_id.as_deref(), input.has_tools, true)
        .unwrap_or(model);
    push_step("session_pin", &model);

    let mut meta = RouteMetadata::new(model, protocol, path.clone(), headers);
    meta.client = client.clone();
    meta.stream = input.stream;
    meta.est_input_tokens = input.est_input_tokens;

    let mut rules = Vec::new();
    let outcome = shell.router.route_traced(&mut meta, |rule, matched| {
        rules.push(RuleTrace {
            id: rule.id,
            name: rule.name.clone(),
            enabled: rule.enabled,
            matched,
            conditions: rule.describe_conditions(),
            action: rule.action.describe(),
            decisive: matched && rule.action.is_final(),
        });
    });
    push_step("rule", &meta.model);

    let outcome = match outcome {
        RouteOutcome::Reject {
            reason,
            matched_rule,
        } => Outcome::Rejected {
            reason,
            rule_id: matched_rule,
        },
        RouteOutcome::Final {
            selector,
            matched_rule,
        } => route_to_channels(shell, selector, matched_rule, &meta.model, protocol).await,
    };

    SimulateResult {
        client,
        client_source,
        protocol,
        path,
        model_steps,
        rules,
        outcome,
    }
}

async fn route_to_channels(
    shell: &Arc<AppShell>,
    selector: String,
    rule_id: Option<i64>,
    model: &str,
    protocol: Protocol,
) -> Outcome {
    let Some(primary) = shell.selectors.resolve(&selector) else {
        return Outcome::NoChannel { selector };
    };

    let (candidates, basis) =
        build_candidates_explained(shell, &primary, model, protocol, crate::util::rand_unit())
            .await;

    let candidates = candidates
        .iter()
        .map(|o| {
            let p = o.provider();
            let wire = o.wire_for(protocol);
            CandidateView {
                tag: o.tag().to_string(),
                name: p.name.clone(),
                upstream_model: p.upstream_model(model).to_string(),
                wire,
                passthrough: wire == protocol,
                url: p.endpoint_for(wire),
            }
        })
        .collect();

    Outcome::Routed {
        selector,
        rule_id,
        model: model.to_string(),
        notes: notes_for(basis),
        basis,
        candidates,
    }
}

fn notes_for(basis: CandidateBasis) -> Vec<String> {
    let mut notes = Vec::new();
    if let CandidateBasis::ModelPolicy {
        strategy: crate::storage::models::ModelStrategy::Weight,
        pinned: false,
    } = basis
    {
        notes.push("按权重随机选主渠道：每次测试（和每个真实请求）的主渠道可能不同".into());
    }
    if let CandidateBasis::ModelPolicy {
        strategy: crate::storage::models::ModelStrategy::Latency,
        pinned: false,
    } = basis
    {
        notes.push("按最近一次测速排序：测速结果变了，顺序也会跟着变".into());
    }
    notes
}

/// 把条件拼成请求头 —— 交给线上同一个 `detect_client` 和规则链去读。
///
/// 非法的头名 / 头值直接跳过：这是给人试规则用的表单，一个手误不该让整次测试失败，
/// 结果里客户端识别成什么一目了然。
fn build_headers(input: &SimulateInput) -> HeaderMap {
    let mut h = HeaderMap::new();
    let mut put = |name: &str, value: &str| {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value.trim()),
        ) {
            h.insert(n, v);
        }
    };
    for (k, v) in &input.headers {
        put(k, v);
    }
    // 令牌与 UA 有专门的输入框，盖过「其余请求头」里的同名项。
    if let Some(t) = input.token.as_deref().filter(|t| !t.trim().is_empty()) {
        put("authorization", &format!("Bearer {}", t.trim()));
    }
    if let Some(ua) = input.user_agent.as_deref().filter(|u| !u.trim().is_empty()) {
        put("user-agent", ua);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::pipeline::detect_client;

    fn input() -> SimulateInput {
        SimulateInput {
            model: "claude-sonnet-5".into(),
            ..Default::default()
        }
    }

    #[test]
    fn token_field_is_sent_as_bearer_and_names_the_client() {
        let mut i = input();
        i.token = Some("apilot-codex".into());
        let h = build_headers(&i);
        assert_eq!(h.get("authorization").unwrap(), "Bearer apilot-codex");
        assert_eq!(detect_client(&h, "/v1/messages"), "codex");
    }

    #[test]
    fn dedicated_fields_override_same_named_free_headers() {
        let mut i = input();
        i.headers = vec![
            ("Authorization".into(), "Bearer from-free-form".into()),
            ("x-apilot-client".into(), "cursor".into()),
        ];
        i.token = Some("apilot-gemini-cli".into());
        let h = build_headers(&i);
        assert_eq!(h.get("authorization").unwrap(), "Bearer apilot-gemini-cli");
        assert_eq!(h.get("x-apilot-client").unwrap(), "cursor");
    }

    #[test]
    fn malformed_headers_are_skipped_not_fatal() {
        let mut i = input();
        i.headers = vec![
            ("bad header".into(), "x".into()),
            ("x-ok".into(), "1".into()),
        ];
        i.token = Some("   ".into());
        let h = build_headers(&i);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get("x-ok").unwrap(), "1");
    }

    #[test]
    fn only_unpinned_random_or_latency_orderings_carry_a_note() {
        use crate::storage::models::ModelStrategy;
        let weight = CandidateBasis::ModelPolicy {
            strategy: ModelStrategy::Weight,
            pinned: false,
        };
        assert_eq!(notes_for(weight).len(), 1);
        // 手动切换过的渠道盖过策略，结果是确定的，不该再吓唬用户。
        let pinned = CandidateBasis::ModelPolicy {
            strategy: ModelStrategy::Weight,
            pinned: true,
        };
        assert!(notes_for(pinned).is_empty());
        assert!(notes_for(CandidateBasis::Selector).is_empty());
    }
}
