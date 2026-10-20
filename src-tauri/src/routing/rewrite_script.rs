//! 通用请求 / 响应改写脚本。
//!
//! 复用 `model_script` 的 QuickJS 引擎与安全边界（线程本地、25ms 超时、8MB 堆上限、
//! 任何异常都按不改写处理），不另起一个 runtime。与模型脚本的区别只在入参：
//! 那边只需要「谁在问哪个模型」，这边拿到的是整个 IR 的 JSON。
//!
//! ## 脚本契约（前端说明文案按这里写）
//!
//! 脚本形如 `function resolve(ctx) { ... }` 或 `const resolve = ctx => ...`，
//! 必须返回**一个字符串或 null/undefined**：
//!
//! - `ctx.client` / `ctx.protocol` / `ctx.model` / `ctx.kind` 是字符串，
//!   其中 `ctx.kind` 是 `"request"` 或 `"response"`；
//! - `ctx.json` 是 IR 的 **JSON 字符串**。脚本自己 `JSON.parse(ctx.json)`，
//!   改完 `return JSON.stringify(obj)`；
//! - 返回 null / undefined / 空串 = 不改写；返回非字符串或不合法 JSON = 不改写；
//! - 语法错、抛异常、超时 = 不改写，**绝不会让请求失败**。
//!
//! 例（请求）：把温度钉成 0
//!
//! ```js
//! function resolve(ctx) {
//!   const obj = JSON.parse(ctx.json);
//!   obj.temperature = 0;
//!   return JSON.stringify(obj);
//! }
//! ```
//!
//! 为什么传入的是字符串而不是现成的对象：脚本的返回值也要经过同一层类型判定，
//! 「字符串进、字符串出」让 `Option<String>` 的 FromJs 一次把 null / 非字符串都判掉，
//! 宿主侧不需要猜脚本返回了什么。

use serde::{Deserialize, Serialize};

use super::model_script::{self, JsonScriptInput};
use crate::protocol::dto::{
    ContentBlock, FinishReason, UnifiedDelta, UsageDelta,
};

/// 流式增量在脚本里的 JSON 形态。
///
/// 为什么不直接序列化 `UnifiedDelta`：它用内部标签 `type`，而 `Finish` 里包的
/// `FinishReason` **也**用 `type` 当标签，两者会拼出重复键（实时推流那边踩过
/// 同一个坑，见 `traffic::stream_events::StreamDelta` 的注释）。这里用 `kind`
/// 当标签、把嵌套值放进字段，脚本看到的形状稳定且无歧义。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DeltaEdit {
    MessageStart { id: String, model: String },
    BlockStart { index: u32, block: ContentBlock },
    Text { index: u32, text: String },
    Thinking { index: u32, text: String },
    ThinkingSignature { index: u32, signature: String },
    ToolInput { index: u32, partial_json: String },
    BlockStop { index: u32 },
    Usage(UsageDelta),
    Finish { reason: FinishReason },
    Error { code: String, message: String },
}

impl From<&UnifiedDelta> for DeltaEdit {
    fn from(d: &UnifiedDelta) -> Self {
        match d {
            UnifiedDelta::MessageStart { id, model } => Self::MessageStart {
                id: id.clone(),
                model: model.clone(),
            },
            UnifiedDelta::BlockStart { index, block } => Self::BlockStart {
                index: *index,
                block: block.clone(),
            },
            UnifiedDelta::TextDelta { index, text } => Self::Text {
                index: *index,
                text: text.clone(),
            },
            UnifiedDelta::ThinkingDelta { index, text } => Self::Thinking {
                index: *index,
                text: text.clone(),
            },
            UnifiedDelta::ThinkingSignature { index, signature } => Self::ThinkingSignature {
                index: *index,
                signature: signature.clone(),
            },
            UnifiedDelta::ToolInputDelta {
                index,
                partial_json,
            } => Self::ToolInput {
                index: *index,
                partial_json: partial_json.clone(),
            },
            UnifiedDelta::BlockStop { index } => Self::BlockStop { index: *index },
            UnifiedDelta::Usage(u) => Self::Usage(u.clone()),
            UnifiedDelta::Finish(r) => Self::Finish { reason: r.clone() },
            UnifiedDelta::Error { code, message } => Self::Error {
                code: code.clone(),
                message: message.clone(),
            },
        }
    }
}

impl From<DeltaEdit> for UnifiedDelta {
    fn from(e: DeltaEdit) -> Self {
        match e {
            DeltaEdit::MessageStart { id, model } => Self::MessageStart { id, model },
            DeltaEdit::BlockStart { index, block } => Self::BlockStart { index, block },
            DeltaEdit::Text { index, text } => Self::TextDelta { index, text },
            DeltaEdit::Thinking { index, text } => Self::ThinkingDelta { index, text },
            DeltaEdit::ThinkingSignature { index, signature } => {
                Self::ThinkingSignature { index, signature }
            }
            DeltaEdit::ToolInput {
                index,
                partial_json,
            } => Self::ToolInputDelta {
                index,
                partial_json,
            },
            DeltaEdit::BlockStop { index } => Self::BlockStop { index },
            DeltaEdit::Usage(u) => Self::Usage(u),
            DeltaEdit::Finish { reason } => Self::Finish(reason),
            DeltaEdit::Error { code, message } => Self::Error { code, message },
        }
    }
}

/// 跑一段改写脚本。`Some` 表示**确实改动了**（结果与入参语义不同）。
///
/// 返回 `None` 的场合：脚本为空、脚本没返回、返回值不是合法 JSON、或者返回的 JSON
/// 与入参完全一样。最后一种不是失败，而是让调用方能少做一次无谓的重新编码 / 落库。
pub fn apply(
    source: &str,
    client: &str,
    protocol: &str,
    model: &str,
    kind: &str,
    value: &serde_json::Value,
) -> Option<serde_json::Value> {
    if source.trim().is_empty() {
        return None;
    }

    // 先判空再序列化：脚本没配时（最常见的情况）连一次 json 拷贝都不该发生。
    let json = serde_json::to_string(value).ok()?;
    let input = JsonScriptInput {
        client,
        protocol,
        model,
        kind,
        json: &json,
    };

    let out = model_script::run_json(source, &input)?;

    // 脚本返回的也可能是 `"null"`、`"42"` 这类合法但非对象的 JSON —— 契约只要求
    // 能解析回 `Value`，形状对不对由脚本作者自己负责（比起拦下来，放过去更像
    // 「用户知道自己在干什么」，而且 IR 侧本来就有容错）。
    let rewritten: serde_json::Value = serde_json::from_str(&out).ok()?;

    if rewritten == *value {
        return None;
    }
    Some(rewritten)
}

/// 跑流式增量改写脚本。
///
/// `Some` 表示**确实改动了**：返回的是替换掉这条增量的 0..n 条新增量
/// （空数组 = 丢弃这条）。`None` = 原样放行（脚本没配、没返回、结果不合法，
/// 或者返回的内容与原增量完全相同）。
///
/// 每条增量单独调用，**绝不缓冲整条流** —— 首字延迟与没开脚本时一致。
/// 代价是脚本看不到完整回答，文本增量可能把一个词切成两半；跨 chunk 的
/// 模式匹配需要脚本自己处理。
pub fn apply_delta(
    source: &str,
    client: &str,
    protocol: &str,
    model: &str,
    delta: &UnifiedDelta,
) -> Option<Vec<UnifiedDelta>> {
    if source.trim().is_empty() {
        return None;
    }

    let json = serde_json::to_string(&DeltaEdit::from(delta)).ok()?;
    let input = JsonScriptInput {
        client,
        protocol,
        model,
        kind: "delta",
        json: &json,
    };
    let out = model_script::run_json(source, &input)?;

    // 允许返回单个对象或数组：数组可以丢弃增量，也可以把一条拆成多条。
    let value: serde_json::Value = serde_json::from_str(&out).ok()?;
    let edits: Vec<DeltaEdit> = match value {
        serde_json::Value::Array(items) => {
            let mut v = Vec::with_capacity(items.len());
            for item in items {
                v.push(serde_json::from_value(item).ok()?);
            }
            v
        }
        other => vec![serde_json::from_value(other).ok()?],
    };

    let rewritten: Vec<UnifiedDelta> = edits.into_iter().map(Into::into).collect();

    // 语义相同就不算改动，调用方可以少走一次分支。
    if rewritten.len() == 1 && rewritten[0] == *delta {
        return None;
    }
    Some(rewritten)
}

/// 校验脚本能否使用，`Err` 是给用户看的错因。与模型脚本共用同一套校验。
pub fn validate(source: &str) -> Result<(), String> {
    model_script::validate(source)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn apply_request(source: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
        apply(source, "claude-code", "anthropic", "claude-sonnet-4", "request", value)
    }

    fn apply_response(source: &str, value: &serde_json::Value) -> Option<serde_json::Value> {
        apply(source, "claude-code", "anthropic", "claude-sonnet-4", "response", value)
    }

    #[test]
    fn a_request_script_can_change_temperature() {
        let src = "function resolve(ctx) { const obj = JSON.parse(ctx.json); obj.temperature = 0; return JSON.stringify(obj); }";
        let input = json!({ "model": "m", "temperature": 1, "messages": [] });

        assert_eq!(
            apply_request(src, &input),
            Some(json!({ "model": "m", "temperature": 0, "messages": [] }))
        );
    }

    #[test]
    fn a_response_script_can_rewrite_the_text() {
        let src = "const resolve = ctx => { const obj = JSON.parse(ctx.json); obj.content[0].text = obj.content[0].text.toUpperCase(); return JSON.stringify(obj); };";
        let input = json!({ "content": [{ "type": "text", "text": "hi" }] });

        assert_eq!(
            apply_response(src, &input),
            Some(json!({ "content": [{ "type": "text", "text": "HI" }] }))
        );
    }

    #[test]
    fn ctx_exposes_client_protocol_model_kind_and_json() {
        let src = "function resolve(ctx) { return JSON.stringify({ c: ctx.client, p: ctx.protocol, m: ctx.model, k: ctx.kind, raw: ctx.json }); }";
        let input = json!({ "a": 1 });

        let out = apply_request(src, &input).expect("该改写");
        assert_eq!(out["c"], json!("claude-code"));
        assert_eq!(out["p"], json!("anthropic"));
        assert_eq!(out["m"], json!("claude-sonnet-4"));
        assert_eq!(out["k"], json!("request"));
        // ctx.json 是字符串，不是对象。
        assert_eq!(out["raw"], json!(input.to_string()));
    }

    #[test]
    fn returning_the_same_json_is_reported_as_no_change() {
        // 脚本跑了但什么也没改：调用方拿到 None 才能跳过重新编码。
        let src = "function resolve(ctx) { return JSON.stringify(JSON.parse(ctx.json), null, 2); }";
        let input = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });

        assert_eq!(apply_request(src, &input), None);
    }

    #[test]
    fn null_undefined_and_empty_returns_do_not_rewrite() {
        let null = "function resolve(ctx) { return null; }";
        let undef = "function resolve(ctx) { }";
        let blank = "const resolve = ctx => '   ';";
        let input = json!({ "model": "m" });

        assert_eq!(apply_request(null, &input), None);
        assert_eq!(apply_request(undef, &input), None);
        assert_eq!(apply_request(blank, &input), None);
    }

    #[test]
    fn a_non_string_return_does_not_rewrite() {
        let src = "function resolve(ctx) { return 42; }";
        assert_eq!(apply_request(src, &json!({ "model": "m" })), None);
    }

    #[test]
    fn invalid_json_from_the_script_does_not_rewrite() {
        let src = "function resolve(ctx) { return '{\"broken\": '; }";
        assert_eq!(apply_request(src, &json!({ "model": "m" })), None);
    }

    #[test]
    fn a_throwing_script_does_not_rewrite() {
        let src = "function resolve(ctx) { const obj = JSON.parse(ctx.json); throw new Error('boom'); }";
        assert_eq!(apply_request(src, &json!({ "model": "m" })), None);
    }

    #[test]
    fn a_syntax_error_does_not_rewrite() {
        assert_eq!(apply_request("function resolve( {", &json!({ "model": "m" })), None);
    }

    #[test]
    fn a_script_without_a_resolve_function_does_not_rewrite() {
        let src = "const other = ctx => ctx.json;";
        assert_eq!(apply_request(src, &json!({ "model": "m" })), None);
    }

    #[test]
    fn a_broken_script_does_not_poison_the_next_one() {
        // 挂起的异常必须在同一个 ctx 上清掉，否则后面所有请求的改写都会失效。
        let bad = "function resolve(ctx) { return nope.undefined.thing; }";
        let good = "function resolve(ctx) { const obj = JSON.parse(ctx.json); obj.ok = true; return JSON.stringify(obj); }";
        let input = json!({ "model": "m" });

        assert_eq!(apply_request(bad, &input), None);
        assert_eq!(
            apply_request(good, &input),
            Some(json!({ "model": "m", "ok": true }))
        );
    }

    #[test]
    fn an_infinite_loop_is_interrupted_within_the_timeout() {
        let src = "function resolve(ctx) { while (true) {} }";
        let started = std::time::Instant::now();

        assert_eq!(apply_request(src, &json!({ "model": "m" })), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "死循环该被 interrupt handler 打断，实际花了 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn an_empty_script_is_a_noop() {
        let input = json!({ "model": "m" });
        assert_eq!(apply_request("", &input), None);
        assert_eq!(apply_request("   \n  ", &input), None);
    }

    #[test]
    fn validate_is_shared_with_the_model_script() {
        assert!(validate("function resolve(ctx) { return ctx.json; }").is_ok());
        assert!(validate("const resolve = ctx => ctx.json;").is_ok());
        assert!(validate("").is_ok());
        assert!(validate("function resolve( {").is_err());
        assert!(validate("const other = 1;").is_err());
    }

    // -----------------------------------------------------------------------
    // 流式增量改写
    // -----------------------------------------------------------------------

    fn text_delta(text: &str) -> UnifiedDelta {
        UnifiedDelta::TextDelta {
            index: 0,
            text: text.into(),
        }
    }

    #[test]
    fn a_delta_script_can_rewrite_text_in_place() {
        let src = "function resolve(ctx) { const d = JSON.parse(ctx.json); if (ctx.kind === 'delta' && d.kind === 'text') d.text = d.text.toUpperCase(); return JSON.stringify(d); }";
        let out = apply_delta(src, "claude-code", "anthropic", "m", &text_delta("hi")).unwrap();
        assert_eq!(out, vec![text_delta("HI")]);
    }

    #[test]
    fn a_delta_script_can_drop_a_delta() {
        // 空数组是明确指令"这条增量不要发"，与"没改动"（None）不是一回事。
        let src = "function resolve(ctx) { return '[]'; }";
        assert_eq!(
            apply_delta(src, "c", "anthropic", "m", &text_delta("x")),
            Some(Vec::new())
        );
    }

    #[test]
    fn a_delta_script_can_split_one_delta_into_several() {
        let src = "function resolve(ctx) { const d = JSON.parse(ctx.json); if (d.kind === 'text') { return JSON.stringify([{...d, text: 'a'}, {...d, text: 'b'}]); } return JSON.stringify(d); }";
        let out = apply_delta(src, "c", "anthropic", "m", &text_delta("x")).unwrap();
        assert_eq!(out, vec![text_delta("a"), text_delta("b")]);
    }

    #[test]
    fn an_unchanged_delta_is_reported_as_no_change() {
        let src = "function resolve(ctx) { return JSON.stringify(JSON.parse(ctx.json), null, 2); }";
        assert_eq!(
            apply_delta(src, "c", "anthropic", "m", &text_delta("x")),
            None
        );
    }

    #[test]
    fn delta_script_failures_leave_the_delta_untouched() {
        for src in [
            "function resolve(ctx) { throw new Error('boom'); }",
            "function resolve( {",
            "function resolve(ctx) { return 42; }",
            "function resolve(ctx) { return '{\"kind\":'; }",
        ] {
            assert_eq!(
                apply_delta(src, "c", "anthropic", "m", &text_delta("x")),
                None,
                "脚本出错时必须按不改写处理: {src}"
            );
        }
    }

    #[test]
    fn delta_edit_roundtrips_every_variant() {
        // 脚本边界上丢字段是静默的：签名 / usage / 工具块任何一个字段丢了，
        // 下游都只会看到"少了一点东西"，不会报错。逐个变体钉住。
        let samples = vec![
            UnifiedDelta::MessageStart {
                id: "m".into(),
                model: "x".into(),
            },
            UnifiedDelta::BlockStart {
                index: 1,
                block: ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "n".into(),
                    input: json!({ "a": 1 }),
                },
            },
            text_delta("hi"),
            UnifiedDelta::ThinkingDelta {
                index: 0,
                text: "想".into(),
            },
            UnifiedDelta::ThinkingSignature {
                index: 0,
                signature: "sig-1".into(),
            },
            UnifiedDelta::ToolInputDelta {
                index: 1,
                partial_json: r#"{"a":1}"#.into(),
            },
            UnifiedDelta::BlockStop { index: 1 },
            UnifiedDelta::Usage(UsageDelta {
                output_tokens: Some(7),
                ..Default::default()
            }),
            UnifiedDelta::Finish(FinishReason::ToolUse),
            UnifiedDelta::Error {
                code: "c".into(),
                message: "m".into(),
            },
        ];

        for d in samples {
            let json = serde_json::to_string(&DeltaEdit::from(&d)).unwrap();
            let edit: DeltaEdit = serde_json::from_str(&json).unwrap();
            assert_eq!(UnifiedDelta::from(edit), d, "序列化边界丢了字段: {json}");
        }
    }
}
