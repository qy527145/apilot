//! 工具调用配对的修复。
//!
//! 三种协议都规定：调用与结果必须成对出现 —— Anthropic 要求一个 `tool_use`
//! 后面跟着 `tool_result`，Chat 要求 assistant 的 `tool_calls` 后面跟着
//! `role:"tool"` 消息，Responses 要求 `function_call` 配 `function_call_output`。
//! 而**客户端中断一轮**（用户按 Esc）留下的恰好是不成对的形状：模型已经把调用
//! 发出去了，结果永远不会来。客户端下一次请求会把这个残缺的历史原样带上，
//! 严格的上游（Anthropic、Kimi、OpenAI 系）于是回 400 —— 而且**之后每一轮都回**，
//! 因为历史不会自己好起来。用户看到的是"这个对话再也用不了了"。
//!
//! 压缩上下文也会造出同一类残缺：被压掉的那一段里可能正好有调用或结果。
//!
//! 修法一律是"补齐"而不是"删除"：给没结果的调用补一个**明确的错误结果**
//!（`is_error` / "结果不可用"），把找不到调用的孤立结果降级成一条普通消息。
//! 两者都保留信息 —— 把用户的操作记录悄悄删掉，等于篡改他的对话历史。
//!
//! 结果一律**按调用的顺序**排（客户端回传并行调用的结果时可能乱序，而三种协议
//! 都要求这个顺序），缺的那个就补在它本该在的位置上。

use serde_json::{json, Map, Value};

use crate::protocol::dto::Protocol;

/// 补出来的结果里写的那句话。
///
/// 明说"这一轮被打断了"而不是编一个空结果：模型读到它就知道当时发生了中断，
/// 不会把空输出当成工具真的返回了空。措辞与 magpie 保持一致，便于对照排查。
const INTERRUPTED: &str = "[The result of this tool call is unavailable: the turn was interrupted.]";

/// 修一个出站请求体里的工具配对；没改动时原样返回原始字节。
///
/// 作用在**出站字节**上而不是 IR 上，因为同协议直通的请求根本不经过 IR
///（`needs_conversion == false` 时转发原始字节）—— 而中断恰恰最常发生在直通路上：
/// 用户用 Claude Code 接 Anthropic 官方渠道，按 Esc 就是直通。
pub fn mend_request_body(body: &[u8], wire: Protocol) -> Vec<u8> {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.to_vec();
    };

    let changed = match wire {
        Protocol::AnthropicMessages => mend_anthropic(obj),
        Protocol::OpenAiChat => mend_chat(obj),
        Protocol::OpenAiResponses => mend_responses(obj),
    };
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
}

/// 一次"交换"的账：还开着哪些调用、哪些已经答过。
///
/// 三种协议的差别只在"调用与结果长什么样"，配对规则本身完全一样，抽出来共用 ——
/// 三份各自实现的规则迟早会分叉，而分叉的表现是某一路上游开始莫名其妙地 400。
#[derive(Default)]
struct Exchange {
    /// 还开着的调用 id，按出现顺序。
    pending: Vec<String>,
    /// 已经收到结果的调用。
    answered: std::collections::HashSet<String>,
    /// 结果块暂存在这里，等这一轮收尾时按调用顺序输出。
    held: Vec<(String, Value)>,
}

impl Exchange {
    /// 记下一批调用。
    fn open(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            self.pending.push(id);
        }
    }

    /// 收下一个结果；返回 `Kept`（留着并按顺序排）、`Doubled`（重复，丢掉）
    /// 或 `Orphan`（答的不是这一轮的调用）。
    fn take_result(&mut self, id: &str, value: Value) -> Taken {
        if !self.pending.iter().any(|p| p == id) {
            return Taken::Orphan;
        }
        if self.answered.contains(id) {
            return Taken::Doubled;
        }
        self.answered.insert(id.to_string());
        self.held.push((id.to_string(), value));
        Taken::Kept
    }

    /// 收尾：按调用顺序吐出结果，缺的用 `synthetic(id)` 补上。
    ///
    /// 返回 `(排好序的结果, 是否补过东西)`。
    fn close(&mut self, synthetic: impl Fn(&str) -> Value) -> (Vec<Value>, bool) {
        // 结果按**调用顺序**排：`take_result` 只收 pending 里的 id，
        // 所以 held 里的每一项都能在 pending 里找到位置。
        let mut by_id: std::collections::HashMap<&str, &Value> =
            self.held.iter().map(|(id, v)| (id.as_str(), v)).collect();

        let mut out = Vec::with_capacity(self.pending.len());
        let mut filled = false;
        for id in &self.pending {
            match by_id.remove(id.as_str()) {
                Some(v) => out.push(v.clone()),
                None => {
                    out.push(synthetic(id));
                    filled = true;
                }
            }
        }
        self.pending.clear();
        self.answered.clear();
        self.held.clear();
        (out, filled)
    }

    fn is_open(&self) -> bool {
        !self.pending.is_empty()
    }
}

enum Taken {
    Kept,
    Doubled,
    Orphan,
}

// ---------------------------------------------------------------------------
// Anthropic Messages
// ---------------------------------------------------------------------------

/// 修 `messages[]` 里 `tool_use` ↔ `tool_result` 的配对。
///
/// 一次交换从带着 `tool_use` 的 assistant 消息开始，到下一个 assistant 消息
///（或消息结束）为止。
fn mend_anthropic(obj: &mut Map<String, Value>) -> bool {
    let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };

    let original = msgs.clone();
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut ex = Exchange::default();

    /// 把这一轮的结果连同这条消息的其余内容一起输出。
    ///
    /// **必须就地输出**：Anthropic 要求 `tool_result` 出现在紧跟调用的那条消息里，
    /// 挪到后面的新消息上，上游照样 400 —— 只是换了一句话说这件事。
    macro_rules! flush_into {
        ($msg:expr, $rest:expr) => {{
            let (mut results, _filled) = ex.close(|id| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": INTERRUPTED,
                    "is_error": true,
                })
            });
            results.extend($rest);
            out.push(json!({
                "role": $msg.get("role").cloned().unwrap_or(json!("user")),
                "content": results,
            }));
        }};
    }

    for msg in msgs.iter() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            // 内容不是数组（理论上不该有）：关掉手上的交换后原样留着。
            if ex.is_open() {
                flush_into!(msg, Vec::<Value>::new());
            } else {
                out.push(msg.clone());
            }
            continue;
        };

        if role == "assistant" {
            if ex.is_open() {
                // 上一轮没答完就到了下一个 assistant：给它补在**它自己那条消息**里
                // 已经来不及了，只能补一条独立的消息（上游对此是宽容的）。
                let (results, _filled) = ex.close(|id| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": INTERRUPTED,
                        "is_error": true,
                    })
                });
                if !results.is_empty() {
                    out.push(json!({ "role": "user", "content": results }));
                }
            }
            let ids: Vec<String> = content
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .filter_map(|b| b.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect();
            ex.open(ids);
            out.push(msg.clone());
            continue;
        }

        // 其余角色：挑出结果块，别的原样留着。
        let mut rest: Vec<Value> = Vec::new();
        for b in content {
            if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                rest.push(b.clone());
                continue;
            }
            let Some(id) = b.get("tool_use_id").and_then(|i| i.as_str()) else {
                rest.push(b.clone());
                continue;
            };
            match ex.take_result(id, b.clone()) {
                Taken::Kept => {}
                Taken::Doubled => {}
                Taken::Orphan => {
                    // 答的不是手上这一轮的调用：它的调用可能已经被答过、
                    // 或者在压缩时被删掉了。降级成文本，信息留着。
                    rest.extend(result_as_text(b));
                }
            }
        }

        let has_work = ex.is_open() || !rest.is_empty();
        if has_work {
            flush_into!(msg, rest);
        } else {
            // 一个跟配对无关的消息（普通对话）：原样留着。
            out.push(msg.clone());
        }
    }

    // 收尾：最后一个 assistant 消息带着没答完的调用时，补一条结果消息。
    if ex.is_open() {
        let (results, _filled) = ex.close(|id| {
            json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": INTERRUPTED,
                "is_error": true,
            })
        });
        if !results.is_empty() {
            out.push(json!({ "role": "user", "content": results }));
        }
    }

    // 判据就是"输出与输入不同"，不另立一个标记。
    //
    // 标记式的判据漏掉整整一类改动：**只是重排**（并行调用的结果乱序回传，
    // 我们把它排回调用顺序）—— 补和降级都置了位，重排没有，于是 `*msgs` 不写回，
    // 排好的顺序连同这次修复一起被丢掉，而上游照样 400。
    if out != original {
        *msgs = out;
        return true;
    }
    false
}

/// 一个答不上任何调用的结果降级成的文本。
///
/// 只把文本提出来，图片一类的块原样保留 —— 截图工具的结果同样有信息量。
fn result_as_text(b: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut said = String::new();
    match b.get("content") {
        Some(Value::String(s)) => said.push_str(s),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            said.push_str(t);
                        }
                    }
                    Some("image") => out.push(block.clone()),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if said.trim().is_empty() {
        said = INTERRUPTED.to_string();
    }
    let mut blocks = vec![json!({
        "type": "text",
        "text": format!("（一个找不到对应调用的工具结果）\n{said}"),
    })];
    blocks.extend(out);
    blocks
}

// ---------------------------------------------------------------------------
// OpenAI Chat Completions
// ---------------------------------------------------------------------------

/// 修 `messages[]` 里 assistant `tool_calls` ↔ `role:"tool"` 的配对。
fn mend_chat(obj: &mut Map<String, Value>) -> bool {
    let Some(msgs) = obj.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };

    let original = msgs.clone();
    let mut out: Vec<Value> = Vec::with_capacity(msgs.len());
    let mut ex = Exchange::default();

    /// 补出来的结果消息。
    ///
    /// 用内容表明失败而不是加个字段：Chat 的 tool 消息没有 `is_error`，
    /// 硬加一个上游未必认。
    macro_rules! synthetic_tool {
        ($id:expr) => {
            json!({ "role": "tool", "tool_call_id": $id, "content": INTERRUPTED })
        };
    }

    macro_rules! flush_tools {
        () => {{
            let (results, _filled) = ex.close(|id| synthetic_tool!(id));
            for m in results {
                out.push(m);
            }
        }};
    }

    /// 一个答不上任何调用的 tool 消息降级成 user 消息，内容留着。
    ///
    /// 连着两条 user 消息有些模型的模板不接受，所以并入前一条。
    macro_rules! tool_as_user {
        ($m:expr) => {{
            let mut text = match $m.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            if text.trim().is_empty() {
                text = INTERRUPTED.to_string();
            }
            let last_is_user = out
                .last()
                .map(|m: &Value| m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .unwrap_or(false);
            if last_is_user {
                if let Some(prev) = out.last_mut() {
                    let merged = match prev.get("content") {
                        Some(Value::String(s)) => format!("{s}\n\n{text}"),
                        _ => text.clone(),
                    };
                    prev["content"] = json!(merged);
                }
            } else {
                out.push(json!({ "role": "user", "content": text }));
            }
        }};
    }

    for msg in msgs.iter() {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        if role == "assistant" {
            let calls = msg
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                // 一次只有推理、没有内容的空 assistant 消息夹在调用与结果之间，
                // 会让上游认为这条交换断了。交换未完就丢掉它。
                if ex.is_open() && is_empty_assistant(msg) {
                    continue;
                }
                flush_tools!();
                out.push(msg.clone());
                continue;
            }
            flush_tools!();
            out.push(msg.clone());
            ex.open(calls.iter().filter_map(|c| {
                c.get("id").and_then(|i| i.as_str()).map(String::from)
            }));
            continue;
        }

        if role == "tool" {
            let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) else {
                flush_tools!();
                out.push(msg.clone());
                continue;
            };
            match ex.take_result(id, msg.clone()) {
                Taken::Kept => {}
                Taken::Doubled => {}
                Taken::Orphan => {
                    // 先把这一轮收掉，那条降级出来的 user 消息才不会插进
                    // 调用与它结果之间。
                    flush_tools!();
                    tool_as_user!(msg);
                }
            }
            continue;
        }

        // 别的角色：关掉手上的交换再原样输出。
        flush_tools!();
        out.push(msg.clone());
    }

    flush_tools!();

    // 判据同 Anthropic：输出与输入不同才算改了（见那边关于"只是重排"的说明）。
    if out != original {
        *msgs = out;
        return true;
    }
    false
}

/// 一条既没有文本、也没有调用、也没有推理的 assistant 消息。
fn is_empty_assistant(msg: &Value) -> bool {
    let has_text = match msg.get("content") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        _ => false,
    };
    !has_text
        && msg.get("reasoning_content").is_none()
        && msg.get("reasoning").is_none()
        && msg.get("function_call").is_none()
}

// ---------------------------------------------------------------------------
// OpenAI Responses
// ---------------------------------------------------------------------------

/// 修 `input[]` 里 `function_call` ↔ `function_call_output` 的配对。
///
/// Responses 的 `input` 是**扁平**的 item 列表，没有消息边界，所以"交换"就是
/// 一段连续的调用/结果 item。规则与另外两个一致：结果按调用顺序输出、重复的丢掉、
/// 孤立的降级成 message、缺的补上。
fn mend_responses(obj: &mut Map<String, Value>) -> bool {
    let Some(input) = obj.get_mut("input").and_then(|i| i.as_array_mut()) else {
        return false;
    };

    // 第一遍：哪些调用已经有结果了。Responses 的调用与结果可以隔得很远
    //（Codex 会把别的 item 夹在中间），所以不能只看前一条。
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in input.iter() {
        if let Some(id) = item.get("call_id").and_then(|c| c.as_str()) {
            if is_tool_result_item(item) {
                answered.insert(id.to_string());
            } else if is_tool_call_item(item) {
                calls.insert(id.to_string());
            }
        }
    }

    let original = input.clone();
    let mut out: Vec<Value> = Vec::with_capacity(input.len() + 1);
    // 这一轮欠下的结果，攒到这一串结束时补。
    let mut owed: Vec<Value> = Vec::new();
    let mut paid: std::collections::HashSet<String> = std::collections::HashSet::new();

    for item in input.iter() {
        let is_call = is_tool_call_item(item);
        let is_result = is_tool_result_item(item);

        // 一串结束（碰到不属于这一串的 item）：先把欠的补上。
        if !is_call && !is_result && !owed.is_empty() {
            out.append(&mut owed);
        }

        if is_call {
            if let Some(id) = item.get("call_id").and_then(|c| c.as_str()) {
                if !answered.contains(id) && !paid.contains(id) {
                    paid.insert(id.to_string());
                    owed.push(synthetic_output(item, id));
                }
            }
            out.push(item.clone());
            continue;
        }

        if is_result {
            let id = item.get("call_id").and_then(|c| c.as_str()).unwrap_or("");
            if paid.contains(id) {
                // 一个结果只作数一次。
                continue;
            }
            if answered.contains(id) && !calls.contains(id) {
                // 答的是一个历史里根本没有的调用：降级成 message，内容留着。
                out.push(as_message_item(item));
                continue;
            }
            paid.insert(id.to_string());
            out.push(item.clone());
            continue;
        }

        out.push(item.clone());
    }
    if !owed.is_empty() {
        out.append(&mut owed);
    }

    if out != original {
        *input = out;
        return true;
    }
    false
}

/// 补出来的结果 item。`custom_tool_call` 要配 `custom_tool_call_output`。
fn synthetic_output(call: &Value, id: &str) -> Value {
    let kind = if call.get("type").and_then(|t| t.as_str()) == Some("custom_tool_call") {
        "custom_tool_call_output"
    } else {
        "function_call_output"
    };
    json!({ "type": kind, "call_id": id, "output": INTERRUPTED })
}

/// 是不是"发起的调用"item。
fn is_tool_call_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(|t| t.as_str()),
        Some("function_call") | Some("custom_tool_call") | Some("local_shell_call")
    )
}

/// 是不是"工具结果"item。
fn is_tool_result_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(|t| t.as_str()),
        Some("function_call_output") | Some("custom_tool_call_output")
    )
}

/// 孤立的结果降级成 `message`（Responses 的 `input` 里没有 tool 角色）。
fn as_message_item(item: &Value) -> Value {
    let text = match item.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    };
    let text = if text.trim().is_empty() {
        INTERRUPTED.to_string()
    } else {
        text
    };
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mend(v: Value, wire: Protocol) -> Value {
        let raw = serde_json::to_vec(&v).unwrap();
        serde_json::from_slice(&mend_request_body(&raw, wire)).unwrap()
    }

    fn result_ids(msgs: &[Value], key: &str) -> Vec<String> {
        msgs.iter()
            .filter_map(|m| m.get(key).and_then(|v| v.as_str()).map(String::from))
            .collect()
    }

    #[test]
    fn a_paired_anthropic_history_is_untouched_bytes() {
        // 正常的请求必须一个字节都不动：上游的 prompt cache 是**前缀字节**敏感的，
        // 无谓的重写会把缓存打掉，用户的成本立刻上升。
        let raw = br#"{"model":"m","messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}]}"#;
        assert_eq!(mend_request_body(raw, Protocol::AnthropicMessages), raw);
    }

    #[test]
    fn an_interrupted_anthropic_turn_gets_an_error_result() {
        // 用户按 Esc：调用发出去了，结果永远不会来。客户端下一轮把它原样带上，
        // 严格的上游回 400，而且之后每一轮都回。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":[
                    {"type":"text","text":"我来读一下"},
                    {"type":"tool_use","id":"t1","name":"read","input":{"p":"a"}}]},
                {"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}]}),
            Protocol::AnthropicMessages,
        );

        let msgs = v["messages"].as_array().unwrap();
        let blocks = msgs[1]["content"].as_array().unwrap();
        // 补出来的结果必须**就在紧跟调用的那条消息里** —— 另起一条消息上游照样 400。
        let synthetic = blocks
            .iter()
            .find(|b| b["type"] == "tool_result")
            .expect("必须补上一个 tool_result");
        assert_eq!(synthetic["tool_use_id"], "t1");
        assert_eq!(synthetic["is_error"], true);
        // 用户那句 "[Request interrupted]" 不能被吞掉 —— 它是上下文的一部分。
        assert!(blocks.iter().any(|b| b["type"] == "text"));
    }

    #[test]
    fn an_orphan_anthropic_result_becomes_text() {
        // 压缩把调用删了，结果留下来。报 400 是错的，直接删也是错的（篡改历史）。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"ghost","content":"输出"}]}]}),
            Protocol::AnthropicMessages,
        );
        let blocks = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert!(blocks[0]["text"].as_str().unwrap().contains("找不到对应调用"));
        assert!(blocks[0]["text"].as_str().unwrap().contains("输出"));
    }

    #[test]
    fn a_repeated_anthropic_result_keeps_its_content_but_stops_being_a_result() {
        // 一次交换在第一个结果消息处就收尾了，所以第二个同名结果已经不是"重复",
        // 而是"答不出任何调用的结果" —— 降级成文本。**内容必须留着**：直接丢掉
        // 等于悄悄篡改用户的对话历史，而降级成文本既避开了上游的 400，又保住了信息。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"第一"}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"第二"}]}]}),
            Protocol::AnthropicMessages,
        );
        let text = serde_json::to_string(&v).unwrap();
        assert!(text.contains("第二"), "内容不能丢: {text}");
        // 第一条消息里那个才是作数的结果。
        assert_eq!(v["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(v["messages"][1]["content"][0]["content"], "第一");
        // 答过的调用不该再补：`is_error` 是补出来的结果才有的标记。
        assert!(!text.contains("is_error"), "已经答过的调用不该再补: {text}");
    }

    #[test]
    fn a_doubled_result_in_one_message_is_dropped() {
        // 同一消息里出现两次才是真正的"重复"：第一份作数，后来那个丢掉。
        // 这不是篡改历史 —— 那个 id 的答案已经在这条消息里了。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read","input":{}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"第一"},
                    {"type":"tool_result","tool_use_id":"t1","content":"第二"}]}]}),
            Protocol::AnthropicMessages,
        );
        let text = serde_json::to_string(&v).unwrap();
        assert!(text.contains("第一"));
        assert!(!text.contains("第二"), "同一消息里的重复该丢掉: {text}");
    }

    #[test]
    fn out_of_order_anthropic_results_are_reordered() {
        // 并行调用时客户端可能乱序回传，而 Anthropic 要求按调用顺序。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t1","name":"a","input":{}},
                    {"type":"tool_use","id":"t2","name":"b","input":{}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t2","content":"第二个"},
                    {"type":"tool_result","tool_use_id":"t1","content":"第一个"}]}]}),
            Protocol::AnthropicMessages,
        );
        let ids = result_ids(v["messages"][1]["content"].as_array().unwrap(), "tool_use_id");
        assert_eq!(ids, vec!["t1", "t2"]);
    }

    #[test]
    fn a_missing_middle_anthropic_result_is_filled_in_place() {
        // t1 从来没回、t2 回了：补出来的 t1 必须排在它**本该在的位置**（第一个），
        // 而不是追加到最后 —— 顺序错了严格的上游同样拒。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":[
                    {"type":"tool_use","id":"t1","name":"a","input":{}},
                    {"type":"tool_use","id":"t2","name":"b","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"第二个"}]}]}),
            Protocol::AnthropicMessages,
        );
        let blocks = v["messages"][1]["content"].as_array().unwrap();
        let ids = result_ids(blocks, "tool_use_id");
        assert_eq!(ids, vec!["t1", "t2"]);
        assert_eq!(blocks[0]["is_error"], true);
    }

    #[test]
    fn an_interrupted_chat_turn_gets_a_tool_message_in_call_order() {
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":null,"tool_calls":[
                    {"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}},
                    {"id":"c2","type":"function","function":{"name":"list","arguments":"{}"}}]},
                {"role":"tool","tool_call_id":"c2","content":"c2 的结果"}]}),
            Protocol::OpenAiChat,
        );

        let msgs = v["messages"].as_array().unwrap();
        let tools: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
        let ids = result_ids(msgs, "tool_call_id");
        assert_eq!(ids, vec!["c1", "c2"], "缺的那个要补在它该在的位置: {v}");
        assert!(tools[0]["content"].as_str().unwrap().contains("interrupted"));
        assert_eq!(tools[1]["content"], "c2 的结果");
    }

    #[test]
    fn an_orphan_chat_tool_message_becomes_a_user_message() {
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"user","content":"开始"},
                {"role":"tool","tool_call_id":"ghost","content":"孤儿结果"}]}),
            Protocol::OpenAiChat,
        );
        let msgs = v["messages"].as_array().unwrap();
        // 并进前一条 user，而不是连着两条 user —— 有些模型的模板不接受。
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert!(msgs[0]["content"].as_str().unwrap().contains("孤儿结果"));
    }

    #[test]
    fn a_thinking_only_assistant_between_calls_and_results_is_dropped() {
        // 有些上游会把"只有推理、没有内容"的一条 assistant 夹在中间，
        // 严格的 Chat 上游会认为这条交换断了。
        let v = mend(
            json!({"model":"m","messages":[
                {"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"f","arguments":"{}"}}]},
                {"role":"assistant","content":""},
                {"role":"tool","tool_call_id":"c1","content":"结果"}]}),
            Protocol::OpenAiChat,
        );
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "空的 assistant 消息该被丢掉: {v}");
        assert_eq!(msgs[1]["role"], "tool");
    }

    #[test]
    fn an_interrupted_responses_turn_gets_an_output_item() {
        let v = mend(
            json!({"model":"m","input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"function_call","call_id":"c1","name":"read","arguments":"{}"}]}),
            Protocol::OpenAiResponses,
        );
        let input = v["input"].as_array().unwrap();
        let out = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .expect("必须补上 function_call_output");
        assert_eq!(out["call_id"], "c1");
        assert!(out["output"].as_str().unwrap().contains("interrupted"));
    }

    #[test]
    fn an_orphan_responses_output_becomes_a_message() {
        let v = mend(
            json!({"model":"m","input":[
                {"type":"function_call_output","call_id":"ghost","output":"孤儿"}]}),
            Protocol::OpenAiResponses,
        );
        let input = v["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "message");
        assert!(input[0]["content"][0]["text"].as_str().unwrap().contains("孤儿"));
    }

    #[test]
    fn an_answered_responses_call_is_not_answered_twice() {
        let raw = br#"{"model":"m","input":[
            {"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":"ok"}]}"#;
        assert_eq!(mend_request_body(raw, Protocol::OpenAiResponses), raw);
    }

    #[test]
    fn a_call_whose_output_came_much_later_needs_nothing() {
        // Codex 会把别的 item 夹在调用与结果之间（`<image_resize_notice>` 之类），
        // 只看前一条会误判成"没答"，凭空补一个结果反而把它弄坏。
        let raw = br#"{"model":"m","input":[
            {"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"notice"}]},
            {"type":"function_call_output","call_id":"c1","output":"ok"}]}"#;
        assert_eq!(mend_request_body(raw, Protocol::OpenAiResponses), raw);
    }

    #[test]
    fn a_body_that_is_not_json_passes_through() {
        // 解不出不等于要失败：这条路只做修复，不该成为新的失败点。
        let raw = b"not json at all";
        assert_eq!(mend_request_body(raw, Protocol::AnthropicMessages), raw);
    }

    #[test]
    fn a_plain_conversation_is_left_alone() {
        let raw = r#"{"model":"m","messages":[
            {"role":"user","content":"你好"},
            {"role":"assistant","content":"你好，有什么可以帮你"},
            {"role":"user","content":"读一下 README"}]}"#;
        let bytes = raw.as_bytes();
        assert_eq!(mend_request_body(bytes, Protocol::AnthropicMessages), bytes);
        assert_eq!(mend_request_body(bytes, Protocol::OpenAiChat), bytes);
    }
}
