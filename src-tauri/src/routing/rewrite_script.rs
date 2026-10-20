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

use super::model_script::{self, JsonScriptInput};

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
}
