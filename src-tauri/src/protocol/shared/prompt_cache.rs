//! Anthropic prompt cache 断点注入。
//!
//! 跨协议转成 Anthropic 时，客户端原本带的 `cache_control` 在 IR 里没有建模，
//! 转换完必然丢失 —— 长上下文因此一个 token 都命中不了缓存。这里在转换路径上补
//! 几个断点兜底；直通路径不调用（客户端自己带的断点原样转发即可）。
//!
//! 只在整份请求「一个 `cache_control` 都没有」时才动手：客户端显式给过断点，说明
//! 缓存布局是它自己安排的，再叠加既会挤掉它的断点（Anthropic 单请求最多 4 个），
//! 也会打乱它的前缀规划。这条同时让函数天然幂等。

use serde_json::{Map, Value};

/// Anthropic 单请求最多 4 个断点，超了上游直接报错。
const MAX_BREAKPOINTS: usize = 4;

/// 给一份 Anthropic Messages 请求体注入缓存断点。`None` = 没有改动，调用方原样转发。
pub fn inject_anthropic_breakpoints(body: &[u8]) -> Option<Vec<u8>> {
    // 解析失败就放手：这份字节可能本来就是上游认得的某种方言，替它「修」坏才是真事故。
    let mut root: Value = serde_json::from_slice(body).ok()?;
    if contains_cache_control(&root) {
        return None;
    }
    let Some(root) = root.as_object_mut() else {
        return None; // 顶层不是对象，没有地方挂断点
    };

    // 顺序即优先级，也恰好等于渲染顺序（tools -> system -> messages）：
    // 断点越靠前，复用的前缀越长，同样花掉一个名额收益更大。
    let mut injected = 0;

    if injected < MAX_BREAKPOINTS {
        if let Some(tools) = root.get_mut("tools").and_then(Value::as_array_mut) {
            if mark_last_cacheable_block(tools) {
                injected += 1;
            }
        }
    }
    if injected < MAX_BREAKPOINTS {
        if let Some(system) = root.get_mut("system") {
            if inject_into_system(system) {
                injected += 1;
            }
        }
    }
    if injected < MAX_BREAKPOINTS {
        if let Some(messages) = root.get_mut("messages") {
            if inject_into_last_message(messages) {
                injected += 1;
            }
        }
    }

    if injected == 0 {
        return None;
    }
    serde_json::to_vec(&root).ok()
}

/// 只要任意层级出现过 `cache_control` 键就认账。
///
/// 递归查而不是只查三个已知位置：顶层的 `cache_control`（自动缓存简写）也算
/// 客户端自己管缓存，将来它把断点塞到别处同样不应该被我们覆盖。
fn contains_cache_control(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key("cache_control") || map.values().any(contains_cache_control)
        }
        Value::Array(items) => items.iter().any(contains_cache_control),
        _ => false,
    }
}

/// 给内容块数组里最后一个「能挂断点」的对象打上标记，返回是否真的改了。
///
/// 从后往前找而不是只看末尾：末尾可能是客户端塞的裸字符串，也可能是
/// thinking / redacted_thinking —— 这两种块不接受 `cache_control`，硬塞会让
/// 整个请求 400。退回前一个可挂的块只损失一截缓存范围，不会把请求打挂。
fn mark_last_cacheable_block(blocks: &mut [Value]) -> bool {
    for block in blocks.iter_mut().rev() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        if matches!(
            obj.get("type").and_then(Value::as_str),
            Some("thinking") | Some("redacted_thinking")
        ) {
            continue;
        }
        obj.insert("cache_control".to_string(), ephemeral());
        return true;
    }
    false
}

fn inject_into_system(system: &mut Value) -> bool {
    if let Value::Array(blocks) = system {
        return !blocks.is_empty() && mark_last_cacheable_block(blocks);
    }
    // 裸字符串上挂不了 cache_control，只能先升格成单元素 text 块数组。
    match text_block_for(system) {
        Some(block) => {
            *system = block;
            true
        }
        None => false,
    }
}

fn inject_into_last_message(messages: &mut Value) -> bool {
    let Some(last) = messages.as_array_mut().and_then(|list| list.last_mut()) else {
        return false;
    };
    let Some(content) = last.get_mut("content") else {
        return false;
    };
    if let Value::Array(blocks) = content {
        return !blocks.is_empty() && mark_last_cacheable_block(blocks);
    }
    match text_block_for(content) {
        Some(block) => {
            *content = block;
            true
        }
        None => false,
    }
}

/// 把非空的裸字符串内容换成挂了断点的单元素 text 块数组；其余情况返回 `None`。
fn text_block_for(value: &mut Value) -> Option<Value> {
    let Value::String(text) = value else {
        return None;
    };
    if text.is_empty() {
        return None;
    }
    let text = std::mem::take(text);
    Some(Value::Array(vec![text_block_with_breakpoint(text)]))
}

fn ephemeral() -> Value {
    let mut obj = Map::new();
    obj.insert("type".to_string(), Value::String("ephemeral".to_string()));
    Value::Object(obj)
}

fn text_block_with_breakpoint(text: String) -> Value {
    let mut obj = Map::new();
    obj.insert("type".to_string(), Value::String("text".to_string()));
    obj.insert("text".to_string(), Value::String(text));
    obj.insert("cache_control".to_string(), ephemeral());
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inject(request: Value) -> Option<Value> {
        let bytes = serde_json::to_vec(&request).unwrap();
        inject_anthropic_breakpoints(&bytes)
            .map(|out| serde_json::from_slice(&out).expect("注入结果必须仍是合法 JSON"))
    }

    fn marked(block: &Value) -> bool {
        block.get("cache_control") == Some(&json!({ "type": "ephemeral" }))
    }

    fn request_with_system(system: Value) -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 1024,
            "system": system,
            "messages": [{ "role": "user", "content": "hello" }],
        })
    }

    #[test]
    fn string_system_is_promoted_to_a_text_block_carrying_the_breakpoint() {
        let out = inject(request_with_system(json!("be brief"))).expect("system 应当被改写");
        assert_eq!(
            out["system"],
            json!([{
                "type": "text",
                "text": "be brief",
                "cache_control": { "type": "ephemeral" }
            }])
        );
    }

    #[test]
    fn array_system_marks_only_its_last_block() {
        let req = request_with_system(json!([
            { "type": "text", "text": "a" },
            { "type": "text", "text": "b" }
        ]));
        let out = inject(req).unwrap();
        let blocks = out["system"].as_array().unwrap();
        assert!(!marked(&blocks[0]), "前面的块不该被动");
        assert!(marked(&blocks[1]));
        assert_eq!(blocks[1]["text"], "b", "原文本必须原样保留");
    }

    #[test]
    fn tools_get_exactly_one_breakpoint_on_the_last_definition() {
        let mut req = request_with_system(json!("s"));
        req["tools"] = json!([
            { "name": "read_file", "input_schema": { "type": "object" } },
            { "name": "write_file", "input_schema": { "type": "object" } }
        ]);
        let out = inject(req).unwrap();
        let tools = out["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert!(marked(&tools[1]));
    }

    #[test]
    fn string_content_of_the_last_message_is_promoted_to_a_text_block() {
        let req = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [
                { "role": "user", "content": "first" },
                { "role": "user", "content": "last" }
            ]
        });
        let out = inject(req).unwrap();
        assert_eq!(out["messages"][0]["content"], "first", "只有最后一条该动");
        assert_eq!(
            out["messages"][1]["content"],
            json!([{
                "type": "text",
                "text": "last",
                "cache_control": { "type": "ephemeral" }
            }])
        );
    }

    #[test]
    fn array_content_marks_only_the_last_block_of_the_last_message() {
        let req = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "old" }] },
                { "role": "user", "content": [
                    { "type": "text", "text": "one" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AA==" } }
                ] }
            ]
        });
        let out = inject(req).unwrap();
        assert!(out["messages"][0]["content"][0].get("cache_control").is_none());
        let blocks = out["messages"][1]["content"].as_array().unwrap();
        assert!(!marked(&blocks[0]));
        assert!(marked(&blocks[1]));
    }

    #[test]
    fn non_object_blocks_are_skipped_when_picking_the_last_block() {
        let req = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "one" },
                "legacy-string-block"
            ] }]
        });
        let out = inject(req).unwrap();
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1], "legacy-string-block", "非对象元素原样保留");
        assert!(marked(&blocks[0]), "断点应落在前一个对象块上");
    }

    #[test]
    fn thinking_blocks_never_receive_a_breakpoint() {
        // thinking 块不接受 cache_control，塞上去上游会 400，宁可少缓存一截。
        let req = json!({
            "model": "m",
            "max_tokens": 1,
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "thinking", "thinking": "hm" }
                ] }
            ]
        });
        let out = inject(req).unwrap();
        let blocks = out["messages"][1]["content"].as_array().unwrap();
        assert!(blocks[1].get("cache_control").is_none());
        assert!(marked(&blocks[0]));
    }

    #[test]
    fn all_three_sites_can_be_filled_in_one_pass() {
        let mut req = request_with_system(json!("s"));
        req["tools"] = json!([{ "name": "t", "input_schema": { "type": "object" } }]);
        let out = inject(req).unwrap();
        assert!(marked(&out["tools"][0]));
        assert!(marked(&out["system"][0]));
        assert!(marked(&out["messages"][0]["content"][0]));
    }

    #[test]
    fn any_existing_breakpoint_means_hands_off() {
        let cases = [
            // system 里有
            json!({ "model": "m", "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }],
                    "system": [{ "type": "text", "text": "s", "cache_control": { "type": "ephemeral" } }] }),
            // tools 里有
            json!({ "model": "m", "max_tokens": 1, "messages": [{ "role": "user", "content": "hi" }],
                    "tools": [{ "name": "t", "cache_control": { "type": "ephemeral" } }] }),
            // messages 里有
            json!({ "model": "m", "max_tokens": 1,
                    "messages": [{ "role": "user", "content": [
                        { "type": "text", "text": "x", "cache_control": { "type": "ephemeral" } }] }] }),
            // 顶层的自动缓存简写
            json!({ "model": "m", "max_tokens": 1, "messages": [], "cache_control": { "type": "ephemeral" } }),
        ];
        for case in cases {
            assert!(inject(case).is_none(), "客户端已有断点时必须原样透传");
        }
    }

    #[test]
    fn existing_breakpoint_leaves_even_the_bytes_untouched() {
        let body = br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[{"type":"text","text":"x","cache_control":{"type":"ephemeral"}}]}]}"#;
        assert!(inject_anthropic_breakpoints(body).is_none());
    }

    #[test]
    fn malformed_json_is_never_rewritten() {
        assert!(inject_anthropic_breakpoints(b"").is_none());
        assert!(inject_anthropic_breakpoints(b"{ not json").is_none());
        assert!(inject_anthropic_breakpoints(b"[1, 2, 3]").is_none(), "顶层不是对象");
        assert!(inject_anthropic_breakpoints(b"null").is_none());
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let out = inject(request_with_system(json!("s"))).expect("首次应当注入");
        let bytes = serde_json::to_vec(&out).unwrap();
        assert!(inject_anthropic_breakpoints(&bytes).is_none());
    }

    #[test]
    fn no_breakpoint_is_added_twice_by_a_single_pass() {
        let mut req = request_with_system(json!("s"));
        req["tools"] = json!([{ "name": "t", "input_schema": { "type": "object" } }]);
        let out = inject(req).unwrap();
        assert_eq!(
            out.to_string().matches("\"cache_control\"").count(),
            3,
            "三个位置各一个断点"
        );
    }

    #[test]
    fn empty_targets_get_no_breakpoint() {
        let cases = [
            json!({ "model": "m", "max_tokens": 1, "messages": [] }),
            json!({ "model": "m", "max_tokens": 1, "messages": [], "system": "" }),
            json!({ "model": "m", "max_tokens": 1, "messages": [], "system": [], "tools": [] }),
            json!({ "model": "m", "max_tokens": 1, "system": "",
                    "messages": [{ "role": "user", "content": "" }] }),
            json!({ "model": "m", "max_tokens": 1,
                    "messages": [{ "role": "user", "content": [] }] }),
        ];
        for case in cases {
            assert!(inject(case).is_none(), "空的 system/tools/messages 不该凭空长出断点");
        }
    }

    #[test]
    fn untouched_request_fields_survive() {
        let mut req = request_with_system(json!("s"));
        req["temperature"] = json!(0);
        req["stream"] = json!(true);
        let out = inject(req).unwrap();
        assert_eq!(out["model"], "claude-opus-5-5");
        assert_eq!(out["max_tokens"], 1024);
        assert_eq!(out["temperature"], 0);
        assert_eq!(out["stream"], true);
        assert_eq!(out["messages"][0]["role"], "user");
    }
}
