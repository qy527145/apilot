//! 直通路上对原始 JSON 的最小改写。
//!
//! 同协议直通转发的是**原始字节**，不能走 IR 再序列化 —— 那会丢掉未建模的字段，
//! 还会规范化空白把上游的 prompt cache 打掉。所以这里只做「哪几个键要变」的
//! 精准编辑，没变就原样返回。

use axum::body::Bytes;
use serde_json::json;

use crate::protocol::dto::{ContentBlock, Protocol, ReasoningConfig, UnifiedRequest};

/// 把原始 JSON 里的 `model` 字段换成映射后的名字，其余字节尽量保留。
///
/// 非对话路径（`handle_raw`）用它 —— 那边连 IR 都不解，自然也没有思考接管。
pub(super) fn patch_model_field(original: &[u8], mapped: &str) -> Bytes {
    patch_json_body(original, |obj| set_top(obj, "model", json!(mapped)))
}

/// 一次 JSON 往返，把出站报文里的 `model` 与**接管过的思考配置**一起换掉。
///
/// `thinking` 必须是 `handle` 里改写 IR 的那一份（`req.reasoning`），不能在这里从设置
/// 重算：缓存键是按那份配置算的，两条路一旦在当前档位表达不出来这类分支上分歧，
/// 就会变成「键按 A 算、字节按 B 发」—— 一种不会报错的错误缓存。
///
/// 什么都没变时原样返回原始字节。这不是省事：重序列化会规范化空白与数字写法，
/// 而上游的 prompt cache 是**前缀字节**敏感的，无谓的重写会把缓存打掉。
pub(super) fn patch_outbound_body(
    original: &[u8],
    wire: Protocol,
    mapped_model: &str,
    thinking: Option<&ReasoningConfig>,
) -> Bytes {
    patch_json_body(original, |obj| {
        let changed = set_top(obj, "model", json!(mapped_model));
        // 两处都要试着改，所以先把思考那次调用做完再合并结果（`||` 会短路）。
        match thinking {
            Some(cfg) => patch_thinking_key(obj, wire, cfg) || changed,
            None => changed,
        }
    })
}

/// 按上游的**线协议**写（或删）思考参数；返回是否真的改了。
///
/// 三个协议键名不同，而且 Anthropic / Responses 的那两个是**子对象**：只改自己那一个
/// 键，同级的 `display`（Anthropic）、`summary`（Responses）是客户端的，一律保留。
fn patch_thinking_key(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    wire: Protocol,
    cfg: &ReasoningConfig,
) -> bool {
    if !cfg.enabled {
        return match wire {
            Protocol::AnthropicMessages => obj.remove("thinking").is_some(),
            Protocol::OpenAiChat => obj.remove("reasoning_effort").is_some(),
            // Responses 只去掉自己那个键；删空了才把对象也去掉（留个空对象是噪音，
            // 上游也未必认），`summary` 一类同级键不碰。
            Protocol::OpenAiResponses => {
                let Some(r) = obj.get_mut("reasoning").and_then(|v| v.as_object_mut()) else {
                    return false;
                };
                if r.remove("effort").is_none() {
                    return false;
                }
                if r.is_empty() {
                    obj.remove("reasoning");
                }
                true
            }
        };
    }

    match wire {
        Protocol::AnthropicMessages => {
            let Some(budget) = cfg.budget_tokens else {
                return false;
            };
            // `type` 也要一起写：客户端可能送来 `{type:"disabled", ..}`（新版 Claude 那边的
            // 形状），只补一个预算会留下自相矛盾的 thinking。
            let wrote_type = set_nested(obj, "thinking", "type", json!("enabled"));
            let wrote_budget = set_nested(obj, "thinking", "budget_tokens", json!(budget));
            wrote_type || wrote_budget
        }
        Protocol::OpenAiChat => match cfg.effort.as_deref() {
            Some(e) => set_top(obj, "reasoning_effort", json!(e)),
            None => false,
        },
        Protocol::OpenAiResponses => match cfg.effort.as_deref() {
            Some(e) => set_nested(obj, "reasoning", "effort", json!(e)),
            None => false,
        },
    }
}

/// 设一个顶层键；值本来就一样时返回 false（见 `patch_outbound_body` 的"没变就别动"）。
fn set_top(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: serde_json::Value,
) -> bool {
    if obj.get(key) == Some(&value) {
        return false;
    }
    obj.insert(key.to_string(), value);
    true
}

/// 设 `obj[outer]` 这个子对象里的一个键；子对象不存在就建一个。语义同 [`set_top`]。
fn set_nested(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    outer: &str,
    key: &str,
    value: serde_json::Value,
) -> bool {
    let sub = obj
        .entry(outer.to_string())
        .or_insert_with(|| serde_json::json!({}));
    let Some(sub) = sub.as_object_mut() else {
        return false;
    };
    if sub.get(key) == Some(&value) {
        return false;
    }
    sub.insert(key.to_string(), value);
    true
}

/// 在原始 JSON 对象上编辑一次；`edit` 说没改就原样返回原始字节。
fn patch_json_body(
    original: &[u8],
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> bool,
) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(original) else {
        return Bytes::from(original.to_vec());
    };
    let Some(obj) = v.as_object_mut() else {
        return Bytes::from(original.to_vec());
    };

    if !edit(obj) {
        return Bytes::from(original.to_vec());
    }

    serde_json::to_vec(&v)
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::from(original.to_vec()))
}

/// 剥离无法跨协议保真的思考块。
///
/// Anthropic 的 `signature` 由上游签名，转到别的协议再转回来必然失效，
/// 回传会让上游 400。与其带着坏签名过去，不如明确丢掉。
///
/// `budget_tokens` 只在**目标不是 Anthropic** 时才清：它本身没有被签名这回事，清掉
/// 只是因为别的协议用不上。反过来（Chat → Anthropic）时它恰恰是唯一能用的思考参数 ——
/// 思考接管注入的就是它，清掉会编出一个没有预算的 `thinking`，上游直接 400。
pub(super) fn strip_unportable_thinking(req: &mut UnifiedRequest, wire: Protocol) {
    for msg in &mut req.messages {
        msg.content.retain(|b| {
            !matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
    if wire != Protocol::AnthropicMessages {
        if let Some(r) = &mut req.reasoning {
            r.budget_tokens = None;
        }
    }
}

pub(super) fn count_tool_uses(messages: &[crate::protocol::dto::UnifiedMessage]) -> u32 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn model_field_is_patched_preserving_other_fields() {
        let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"custom_field":42}"#;
        let out = patch_model_field(raw, "deepseek-chat");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["model"], "deepseek-chat");
        assert_eq!(v["custom_field"], 42, "其余字段必须原样保留");
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn patch_model_on_invalid_json_returns_original() {
        let raw = b"not json";
        assert_eq!(&patch_model_field(raw, "x")[..], raw);
    }

    #[test]
    fn an_unchanged_model_name_leaves_the_body_byte_for_byte() {
        // 名字没变就一个字节都别动：重序列化会规范化空白，"原样转发"这句承诺
        // 会因此不再成立。
        let raw = b"{ \"model\" : \"deepseek-chat\" ,\n  \"messages\" : [] }";
        assert_eq!(&patch_model_field(raw, "deepseek-chat")[..], raw);

        // 真的变了才重写。
        let changed = patch_model_field(raw, "other");
        assert_ne!(&changed[..], raw);
        let v: serde_json::Value = serde_json::from_slice(&changed).unwrap();
        assert_eq!(v["model"], "other");
        assert!(v.get("messages").is_some(), "改 model 不该丢掉别的字段");
    }

    #[test]
    fn a_body_without_a_model_gets_one_inserted() {
        // 非对话请求的 body 未必带 model；带上渠道映射后的名字总比不带强。
        let out = patch_model_field(br#"{"messages":[]}"#, "deepseek-chat");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "deepseek-chat");
    }

    // --- 思考接管（同协议直通那条路） ---

    /// 一份像样的 Anthropic 请求体：模型名**已经是映射后的**，这样只有思考字段会变，
    /// 断言才盯得住"只改思考"这件事。
    const ANTHROPIC_BODY: &[u8] = br#"{"model":"deepseek","max_tokens":32000,"messages":[]}"#;

    fn out_body(raw: &[u8], wire: Protocol, thinking: Option<&ReasoningConfig>) -> Bytes {
        patch_outbound_body(raw, wire, "deepseek", thinking)
    }

    fn tier(effort: &str, budget: u32) -> ReasoningConfig {
        ReasoningConfig {
            enabled: true,
            budget_tokens: Some(budget),
            effort: Some(effort.into()),
        }
    }

    #[test]
    fn off_never_touches_the_same_protocol_body() {
        // 不接管 == 一个字都不改（包括客户端自己带的思考字段）。
        let raw = br#"{"model":"deepseek","thinking":{"type":"enabled","budget_tokens":2048}}"#;
        assert_eq!(&out_body(raw, Protocol::AnthropicMessages, None)[..], raw);
    }

    #[test]
    fn an_unchanged_thinking_field_leaves_the_body_byte_for_byte() {
        // 已经是这个档位了就没什么可改的 —— 重写一遍只会规范化空白，
        // 而上游的 prompt cache 是前缀字节敏感的。
        let raw = br#"{ "model" : "deepseek" , "thinking" : { "type" : "enabled" , "budget_tokens" : 16384 } }"#;
        assert_eq!(
            &out_body(raw, Protocol::AnthropicMessages, Some(&tier("high", 16_384)))[..],
            raw
        );
    }

    #[test]
    fn a_tier_writes_thinking_with_a_budget_for_an_anthropic_body() {
        let out = out_body(ANTHROPIC_BODY, Protocol::AnthropicMessages, Some(&tier("high", 16_384)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["thinking"]["type"], "enabled");
        assert_eq!(v["thinking"]["budget_tokens"], 16_384);
        assert_eq!(v["max_tokens"], 32_000, "别的字段不动");
    }

    #[test]
    fn a_tier_keeps_the_anthropic_thinking_siblings() {
        // `display` 之类是同级的客户端设置，只动我们那一个键。
        let raw = br#"{"model":"deepseek","thinking":{"type":"enabled","budget_tokens":2048,"display":"summarized"}}"#;
        let out = out_body(raw, Protocol::AnthropicMessages, Some(&tier("high", 16_384)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["thinking"]["budget_tokens"], 16_384);
        assert_eq!(v["thinking"]["display"], "summarized");
    }

    #[test]
    fn a_tier_writes_reasoning_effort_for_a_chat_body() {
        let raw = br#"{"model":"deepseek","messages":[]}"#;
        let out = out_body(raw, Protocol::OpenAiChat, Some(&tier("xhigh", 32_768)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["reasoning_effort"], "xhigh");
        assert!(v.get("thinking").is_none(), "Chat 侧没有 thinking 这个键");
    }

    #[test]
    fn a_tier_keeps_the_responses_reasoning_summary() {
        // Codex 发的是 `reasoning: {effort, summary}`；只改 effort，summary 留着。
        let raw = br#"{"model":"deepseek","reasoning":{"effort":"low","summary":"auto"},"input":[]}"#;
        let out = out_body(raw, Protocol::OpenAiResponses, Some(&tier("high", 16_384)));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["reasoning"]["effort"], "high");
        assert_eq!(v["reasoning"]["summary"], "auto");
    }

    #[test]
    fn disabling_removes_only_our_key() {
        let anthropic = out_body(
            br#"{"model":"deepseek","thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}"#,
            Protocol::AnthropicMessages,
            Some(&ReasoningConfig::default()),
        );
        let v: serde_json::Value = serde_json::from_slice(&anthropic).unwrap();
        assert!(
            v.get("thinking").is_none(),
            "关闭就把整个 thinking 去掉 —— 连 display 一起，因为思考都关了"
        );

        let chat = out_body(
            br#"{"model":"deepseek","reasoning_effort":"high","messages":[]}"#,
            Protocol::OpenAiChat,
            Some(&ReasoningConfig::default()),
        );
        let v: serde_json::Value = serde_json::from_slice(&chat).unwrap();
        assert!(v.get("reasoning_effort").is_none());
        assert!(v.get("messages").is_some());

        // Responses 只去掉自己那个键；删空了才把对象也去掉。
        let responses = out_body(
            br#"{"model":"deepseek","reasoning":{"effort":"high","summary":"auto"},"input":[]}"#,
            Protocol::OpenAiResponses,
            Some(&ReasoningConfig::default()),
        );
        let v: serde_json::Value = serde_json::from_slice(&responses).unwrap();
        assert!(v["reasoning"].get("effort").is_none());
        assert_eq!(v["reasoning"]["summary"], "auto");

        let emptied = out_body(
            br#"{"model":"deepseek","reasoning":{"effort":"high"},"input":[]}"#,
            Protocol::OpenAiResponses,
            Some(&ReasoningConfig::default()),
        );
        let v: serde_json::Value = serde_json::from_slice(&emptied).unwrap();
        assert!(v.get("reasoning").is_none(), "删空了就别留个空对象");
    }

    #[test]
    fn disconnecting_nothing_is_not_a_change() {
        // 原始报文里本来就没有思考字段时，关闭它不该触发一次重序列化。
        let raw = br#"{ "model" : "deepseek" , "messages" : [] }"#;
        assert_eq!(
            &out_body(raw, Protocol::OpenAiChat, Some(&ReasoningConfig::default()))[..],
            raw
        );
    }


    #[test]
    fn thinking_is_stripped_for_cross_protocol() {
        let mut req = UnifiedRequest::new("m");
        req.messages = vec![crate::protocol::dto::UnifiedMessage::new(
            crate::protocol::dto::Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    text: "想了很久".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::text("答案"),
            ],
        )];
        req.reasoning = Some(crate::protocol::dto::ReasoningConfig {
            enabled: true,
            budget_tokens: Some(8000),
            effort: None,
        });

        strip_unportable_thinking(&mut req, Protocol::OpenAiChat);

        assert_eq!(req.messages[0].content.len(), 1, "思考块应被剥离");
        assert_eq!(req.messages[0].content[0].as_text(), Some("答案"));
        assert!(req.reasoning.as_ref().unwrap().budget_tokens.is_none());
    }

    #[test]
    fn converting_to_anthropic_keeps_the_injected_budget() {
        // Chat → Anthropic 这条路上，`budget_tokens` 是 Anthropic 唯一能用的思考参数
        // （思考接管注入的正是它）。清掉就会编出 `{"type":"enabled"}` 这种没有预算的
        // thinking，上游直接 400。剥思考块的行为不受影响 —— 那才是这个函数的本职。
        let mut req = UnifiedRequest::new("m");
        req.messages = vec![crate::protocol::dto::UnifiedMessage::new(
            crate::protocol::dto::Role::Assistant,
            vec![ContentBlock::Thinking {
                text: "从 chat 那边带过来的".into(),
                signature: None,
            }],
        )];
        req.reasoning = Some(ReasoningConfig {
            enabled: true,
            budget_tokens: Some(8000),
            effort: Some("high".into()),
        });

        strip_unportable_thinking(&mut req, Protocol::AnthropicMessages);

        assert!(req.messages[0].content.is_empty(), "没有签名的思考块仍要剥掉");
        let r = req.reasoning.as_ref().unwrap();
        assert_eq!(r.budget_tokens, Some(8000), "预算必须留着");
        assert_eq!(r.effort.as_deref(), Some("high"));
    }

    #[test]
    fn tool_use_counting() {
        use crate::protocol::dto::{Role, UnifiedMessage};
        let msgs = vec![UnifiedMessage::new(
            Role::Assistant,
            vec![
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "f".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "g".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::text("x"),
            ],
        )];
        assert_eq!(count_tool_uses(&msgs), 2);
    }
}
