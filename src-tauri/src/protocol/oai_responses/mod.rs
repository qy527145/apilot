//! OpenAI Responses API 编解码器（`/v1/responses`，Codex 使用）。

mod stream;

pub use stream::{ResponsesStreamDecoder, ResponsesStreamEncoder};

use serde_json::{json, Map, Value};

use super::codec::{Codec, ConvertError, StreamDecoder, StreamEncoder};
use super::dto::{
    ContentBlock, FinishReason, Protocol, ReasoningConfig, Role, ToolChoice, ToolDef,
    UnifiedMessage, UnifiedRequest, UnifiedResponse, UnifiedUsage, UsageSource,
};
use super::shared::tools::parse_tool_input;

const P: Protocol = Protocol::OpenAiResponses;

/// Responses 协议。与 Chat 的最大差别：
/// - system 叫 `instructions`
/// - 输入是扁平的 item 数组，工具调用与结果都是顶层 item
/// - `max_tokens` 叫 `max_output_tokens`
pub struct OpenAiResponsesCodec;

impl Codec for OpenAiResponsesCodec {
    fn protocol(&self) -> Protocol {
        Protocol::OpenAiResponses
    }

    fn decode_request(&self, raw: &[u8]) -> Result<UnifiedRequest, ConvertError> {
        decode_request(raw)
    }

    fn encode_request(&self, req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError> {
        encode_request(req)
    }

    fn decode_response(&self, raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError> {
        decode_response(raw)
    }

    fn encode_response(
        &self,
        resp: &UnifiedResponse,
        usage: &UnifiedUsage,
    ) -> Result<Vec<u8>, ConvertError> {
        encode_response(resp, usage)
    }

    fn new_stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesStreamDecoder::new())
    }

    fn new_stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ResponsesStreamEncoder::new())
    }
}

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// 已被 IR 显式建模、编码时会重新写出的字段。
///
/// **不在这个列表里的字段一律进 `extra` 原样透传。**
/// 把字段列进来等于承诺「编码时会写回去」；漏写就是静默丢数据，
/// 所以有状态语义的 `store` / `previous_response_id` 刻意留在这里之外，
/// 让它们无损穿过本层。
const KNOWN_TOP_LEVEL: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "max_output_tokens",
    "temperature",
    "top_p",
    "stream",
    "reasoning",
];

fn decode_request(raw: &[u8]) -> Result<UnifiedRequest, ConvertError> {
    let v: Value = serde_json::from_slice(raw)?;
    let obj = v
        .as_object()
        .ok_or_else(|| ConvertError::decode_request(P, "请求体不是 JSON 对象"))?;

    let model = obj
        .get("model")
        .and_then(|m| m.as_str())
        .ok_or_else(|| ConvertError::decode_request(P, "缺少 model 字段"))?
        .to_string();

    let mut req = UnifiedRequest::new(model);
    req.stream = obj.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    req.max_tokens = obj
        .get("max_output_tokens")
        .and_then(|m| m.as_u64())
        .map(|m| m as u32);
    req.temperature = obj.get("temperature").and_then(|t| t.as_f64());
    req.top_p = obj.get("top_p").and_then(|t| t.as_f64());

    if let Some(instr) = obj.get("instructions").and_then(|i| i.as_str()) {
        if !instr.is_empty() {
            req.system = vec![ContentBlock::text(instr)];
        }
    }

    if let Some(r) = obj.get("reasoning") {
        let effort = r.get("effort").and_then(|e| e.as_str());
        if let Some(e) = effort {
            req.reasoning = Some(ReasoningConfig {
                enabled: true,
                budget_tokens: None,
                effort: Some(e.to_string()),
            });
        }
    }

    // 顶层 tools 必须先于 input 解析：Codex 的 Responses Lite 格式会把工具塞进
    // input 里的 `additional_tools` 条目，那些条目要追加到这里已经建好的列表上。
    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        req.tools = tools.iter().filter_map(decode_tool).collect();
    }
    if let Some(tc) = obj.get("tool_choice") {
        req.tool_choice = decode_tool_choice(tc);
    }

    // input 可以是字符串（单轮），也可以是 item 数组。
    match obj.get("input") {
        Some(Value::String(s)) => {
            req.messages = vec![UnifiedMessage::user_text(s.clone())];
        }
        Some(Value::Array(items)) => {
            for item in items {
                decode_item(item, &mut req)?;
            }
        }
        _ => {}
    }

    for (k, val) in obj {
        if !KNOWN_TOP_LEVEL.contains(&k.as_str()) {
            req.extra.insert(k.clone(), val.clone());
        }
    }

    Ok(req)
}

fn decode_item(item: &Value, req: &mut UnifiedRequest) -> Result<(), ConvertError> {
    match item.get("type").and_then(|t| t.as_str()) {
        // 工具调用与结果是顶层 item，要折回 IR 的消息块。
        Some("function_call") => {
            req.messages.push(UnifiedMessage::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse {
                    // Responses 用 call_id 关联调用与结果，id 只是 item 自身的标识。
                    id: item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    input: parse_tool_input(
                        item.get("arguments")
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}"),
                    ),
                }],
            ));
            Ok(())
        }
        Some("function_call_output") => {
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let text = match output {
                Value::String(s) => s,
                other => other.to_string(),
            };
            req.messages.push(UnifiedMessage::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id: item
                        .get("call_id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    content: vec![ContentBlock::text(text)],
                    is_error: false,
                }],
            ));
            Ok(())
        }
        Some("message") | None => {
            let role = match item.get("role").and_then(|r| r.as_str()) {
                Some("assistant") => Role::Assistant,
                Some("system") | Some("developer") => {
                    req.system.extend(decode_message_content(
                        item.get("content").unwrap_or(&Value::Null),
                    )?);
                    return Ok(());
                }
                _ => Role::User,
            };
            let content =
                decode_message_content(item.get("content").unwrap_or(&Value::Null))?;
            req.messages.push(UnifiedMessage::new(role, content));
            Ok(())
        }
        Some("reasoning") => Ok(()),
        // Codex 的 Responses Lite 格式（GPT-5.6 一类的模型走这个）把工具 schema 放在
        // input 里当成一条会话条目发，同时把顶层 tools 置空。丢掉它 = 模型手里一个
        // 工具都没有，Codex 直接瘫掉；所以必须把里面的工具捞出来并进 req.tools。
        Some("additional_tools") => {
            if let Some(tools) = item.get("tools").and_then(|t| t.as_array()) {
                for t in tools {
                    if let Some(def) = decode_tool(t) {
                        // 与顶层 tools 重名时保留先到的，避免同名工具被两个 schema 打架。
                        if !req.tools.iter().any(|existing| existing.name == def.name) {
                            req.tools.push(def);
                        }
                    }
                }
            }
            Ok(())
        }
        // 没建模的条目类型只跳过、不报错。
        //
        // 解码是路由前的必经步骤（见 gateway/pipeline.rs 第 1 步），同协议直通时
        // 转发的是原始字节、IR 只用于统计，所以这里丢掉的内容并不影响那种场景；
        // 而一旦报错，整条请求 400 —— 每来一种新条目类型（web_search_call、
        // local_shell_call、item_reference……）都会让网关在客户端升级后全线崩掉，
        // 代价远大于"转码时少了一段我们本来也表达不了的内容"。
        Some(other) => {
            tracing::warn!(item_type = %other, "跳过未建模的 Responses input 条目");
            Ok(())
        }
    }
}

fn decode_message_content(v: &Value) -> Result<Vec<ContentBlock>, ConvertError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![ContentBlock::text(s.clone())]),
        Value::Array(parts) => parts
            .iter()
            .map(|p| match p.get("type").and_then(|t| t.as_str()).unwrap_or("input_text") {
                "input_text" | "output_text" | "text" => Ok(ContentBlock::text(
                    p.get("text").and_then(|t| t.as_str()).unwrap_or_default(),
                )),
                "input_image" => {
                    let url = p
                        .get("image_url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default();
                    let (media_type, data) = match url.split_once(',') {
                        Some((head, payload)) if head.starts_with("data:") => (
                            head.trim_start_matches("data:")
                                .split(';')
                                .next()
                                .unwrap_or("image/png")
                                .to_string(),
                            payload.to_string(),
                        ),
                        _ => ("image/png".to_string(), format!("url:{url}")),
                    };
                    Ok(ContentBlock::Image { media_type, data })
                }
                other => Err(ConvertError::decode_request(
                    P,
                    format!("未知的 content 类型: {other}"),
                )),
            })
            .collect(),
        other => Err(ConvertError::decode_request(
            P,
            format!("content 既不是字符串也不是数组: {other}"),
        )),
    }
}

fn encode_request(req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError> {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(req.model));

    if !req.system.is_empty() {
        let text: String = req
            .system
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        obj.insert("instructions".into(), json!(text));
    }

    // IR 的顺序消息 → Responses 的扁平 item 列表。
    let mut items: Vec<Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::Assistant => {
                let mut texts: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => {
                            texts.push(json!({ "type": "output_text", "text": text }))
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".into()),
                            }));
                        }
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": texts,
                    }));
                }
            }
            role => {
                let role_str = if role == Role::System { "system" } else { "user" };
                let mut texts: Vec<Value> = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => {
                            texts.push(json!({ "type": "input_text", "text": text }))
                        }
                        ContentBlock::Image { media_type, data } => {
                            let url = if let Some(u) = data.strip_prefix("url:") {
                                u.to_string()
                            } else {
                                format!("data:{media_type};base64,{data}")
                            };
                            texts.push(json!({ "type": "input_image", "image_url": url }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            items.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": super::shared::tools::flatten_tool_result_text(content),
                            }));
                        }
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": role_str,
                        "content": texts,
                    }));
                }
            }
        }
    }
    obj.insert("input".into(), Value::Array(items));

    if !req.tools.is_empty() {
        // 注意 Responses 把 name/parameters 放在 tool 对象顶层，不套 "function"。
        obj.insert(
            "tools".into(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        let mut v = json!({
                            "type": "function",
                            "name": t.name,
                            "parameters": t.input_schema,
                        });
                        if let Some(d) = &t.description {
                            v["description"] = json!(d);
                        }
                        v
                    })
                    .collect(),
            ),
        );
    }
    if let Some(tc) = &req.tool_choice {
        obj.insert(
            "tool_choice".into(),
            match tc {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
                ToolChoice::Tool { name } => {
                    json!({ "type": "function", "name": name })
                }
            },
        );
    }
    if let Some(m) = req.max_tokens {
        obj.insert("max_output_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        obj.insert("temperature".into(), json!(t));
    }
    if let Some(t) = req.top_p {
        obj.insert("top_p".into(), json!(t));
    }
    if req.stream {
        obj.insert("stream".into(), json!(true));
    }
    if let Some(r) = &req.reasoning {
        if r.enabled {
            if let Some(e) = &r.effort {
                obj.insert("reasoning".into(), json!({ "effort": e }));
            }
        }
    }

    for (k, v) in &req.extra {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }

    serde_json::to_vec(&Value::Object(obj)).map_err(|e| ConvertError::encode(P, e))
}

fn decode_tool(v: &Value) -> Option<ToolDef> {
    // Responses 的 function 工具字段在顶层；但部分中转会套一层 "function"，两者都认。
    let src = v.get("function").unwrap_or(v);
    Some(ToolDef {
        name: src.get("name")?.as_str()?.to_string(),
        description: src
            .get("description")
            .and_then(|d| d.as_str())
            .map(String::from),
        input_schema: src
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
    })
}

fn decode_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(o) => o
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Tool { name: n.to_string() }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 响应
// ---------------------------------------------------------------------------

fn decode_response(raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError> {
    let v: Value = serde_json::from_slice(raw)?;

    let mut content: Vec<ContentBlock> = Vec::new();
    let mut saw_tool_call = false;

    if let Some(output) = v.get("output").and_then(|o| o.as_array()) {
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                        for part in parts {
                            // API 用 output_text，SDK 聚合后叫 text。
                            if let Some(t) = part
                                .get("text")
                                .or_else(|| part.get("output_text"))
                                .and_then(|t| t.as_str())
                            {
                                content.push(ContentBlock::text(t));
                            }
                        }
                    }
                }
                Some("function_call") => {
                    saw_tool_call = true;
                    content.push(ContentBlock::ToolUse {
                        id: item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        input: parse_tool_input(
                            item.get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("{}"),
                        ),
                    });
                }
                // reasoning item 的摘要可作思考内容保留。
                Some("reasoning") => {
                    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                        let text: String = summary
                            .iter()
                            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            content.insert(
                                0,
                                ContentBlock::Thinking {
                                    text,
                                    signature: None,
                                },
                            );
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("completed");
    let finish_reason = if saw_tool_call {
        FinishReason::ToolUse
    } else {
        match status {
            "incomplete" => FinishReason::Length,
            _ => FinishReason::Stop,
        }
    };

    Ok((
        UnifiedResponse {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("resp_unknown")
                .to_string(),
            model: v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            content,
            finish_reason,
        },
        decode_usage(v.get("usage")),
    ))
}

/// Responses 的 usage → IR 口径。
///
/// 与 Chat 一样，`input_tokens` **包含** `input_tokens_details.cached_tokens`，
/// 必须做减法才是 IR 约定的 fresh 输入。
pub fn decode_usage(v: Option<&Value>) -> UnifiedUsage {
    let Some(u) = v else {
        return UnifiedUsage::default();
    };

    let input_tokens = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let output_tokens = u
        .get("output_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let reasoning = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    UnifiedUsage {
        input_tokens: input_tokens.saturating_sub(cached),
        output_tokens,
        cache_read_tokens: cached,
        cache_creation_tokens: 0,
        reasoning_tokens: reasoning,
        total_tokens: u
            .get("total_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or_else(|| input_tokens.saturating_add(output_tokens)),
        source: UsageSource::Upstream,
        raw: Some(u.clone()),
    }
}

fn encode_response(
    resp: &UnifiedResponse,
    usage: &UnifiedUsage,
) -> Result<Vec<u8>, ConvertError> {
    let mut output: Vec<Value> = Vec::new();
    let mut texts: Vec<Value> = Vec::new();

    for b in &resp.content {
        match b {
            ContentBlock::Text { text } => {
                texts.push(json!({ "type": "output_text", "text": text, "annotations": [] }))
            }
            ContentBlock::ToolUse { id, name, input } => {
                output.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                }));
            }
            ContentBlock::Thinking { text, .. } => {
                output.push(json!({
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": text }],
                }));
            }
            _ => {}
        }
    }

    if !texts.is_empty() {
        output.insert(
            0,
            json!({
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": texts,
            }),
        );
    }

    let has_tool = resp
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

    let prompt_total = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_creation_tokens);

    let body = json!({
        "id": resp.id,
        "object": "response",
        "created_at": crate::util::now_secs(),
        "model": resp.model,
        "status": if has_tool { "completed" } else { "completed" },
        "output": output,
        "parallel_tool_calls": true,
        "usage": {
            "input_tokens": prompt_total,
            "output_tokens": usage.output_tokens,
            "total_tokens": prompt_total.saturating_add(usage.output_tokens),
            "input_tokens_details": { "cached_tokens": usage.cache_read_tokens },
            "output_tokens_details": { "reasoning_tokens": usage.reasoning_tokens },
        },
    });

    serde_json::to_vec(&body).map_err(|e| ConvertError::encode(P, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(raw: &str) -> Value {
        let req = decode_request(raw.as_bytes()).unwrap();
        serde_json::from_slice(&encode_request(&req).unwrap()).unwrap()
    }

    #[test]
    fn decodes_string_input() {
        let req = decode_request(br#"{"model":"gpt-5","input":"hi"}"#).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].concat_text(), "hi");
    }

    #[test]
    fn lifts_additional_tools_items_into_request_tools() {
        // Codex Responses Lite：顶层 tools 为 null，工具 schema 塞在 input 里。
        // 不捞出来的话转成 Chat 时模型手上一个工具都没有。
        let req = decode_request(
            br#"{"model":"gpt-5.6-sol","tools":null,"input":[
                {"type":"additional_tools","role":"developer","tools":[
                    {"type":"function","name":"shell","description":"run","parameters":{"type":"object"}},
                    {"type":"function","name":"update_plan","parameters":{"type":"object"}}
                ]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}
            ]}"#,
        )
        .unwrap();

        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["shell", "update_plan"]);
        assert_eq!(req.tools[0].description.as_deref(), Some("run"));
        // 载体条目本身不该变成一条消息。
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].concat_text(), "go");
    }

    #[test]
    fn additional_tools_are_appended_after_top_level_tools() {
        let req = decode_request(
            br#"{"model":"m",
                "tools":[{"type":"function","name":"top","parameters":{"type":"object"}}],
                "input":[{"type":"additional_tools","role":"developer","tools":[
                    {"type":"function","name":"extra","parameters":{"type":"object"}}]}]}"#,
        )
        .unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["top", "extra"]);
    }

    #[test]
    fn duplicate_tool_names_keep_the_first_definition() {
        let req = decode_request(
            r#"{"model":"m",
                "tools":[{"type":"function","name":"dup","description":"first","parameters":{"type":"object"}}],
                "input":[{"type":"additional_tools","role":"developer","tools":[
                    {"type":"function","name":"dup","description":"later","parameters":{"type":"object"}}]}]}"#
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].description.as_deref(), Some("first"));
    }

    #[test]
    fn unmodeled_input_items_are_skipped_not_fatal() {
        // 新条目类型不该让整条请求 400 —— 同协议直通时转发的是原始字节，
        // 这里丢掉的内容并不影响那类请求。
        let req = decode_request(
            br#"{"model":"m","input":[
                {"type":"web_search_call","id":"ws_1","status":"completed"},
                {"type":"local_shell_call","call_id":"c1"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].concat_text(), "hi");
    }

    #[test]
    fn additional_tools_without_tools_array_is_harmless() {
        // 坏数据（缺 tools / tools 不是数组）只当空处理，不能 panic 也不能报错。
        for body in [
            &br#"{"model":"m","input":[{"type":"additional_tools","role":"developer"}]}"#[..],
            &br#"{"model":"m","input":[{"type":"additional_tools","tools":"nope"}]}"#[..],
            &br#"{"model":"m","input":[{"type":"additional_tools","tools":[{"no_name":1},null]}]}"#[..],
        ] {
            let req = decode_request(body).unwrap();
            assert!(req.tools.is_empty(), "坏数据不该产出工具: {req:?}");
        }
    }

    #[test]
    fn instructions_become_system() {
        let req = decode_request(
            br#"{"model":"m","instructions":"be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
        )
        .unwrap();
        assert_eq!(req.system[0].as_text(), Some("be brief"));
    }

    #[test]
    fn decodes_function_call_and_output_items() {
        let req = decode_request(
            br#"{"model":"m","input":[
                {"type":"function_call","call_id":"call_1","name":"search","arguments":"{\"q\":\"x\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"result text"}
            ]}"#,
        )
        .unwrap();

        match &req.messages[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search");
                assert_eq!(input["q"], "x");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
        match &req.messages[1].content[0] {
            ContentBlock::ToolResult { tool_use_id, .. } => assert_eq!(tool_use_id, "call_1"),
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[test]
    fn max_output_tokens_maps_to_ir_max_tokens() {
        let req = decode_request(br#"{"model":"m","input":"x","max_output_tokens":256}"#).unwrap();
        assert_eq!(req.max_tokens, Some(256));
        let out = rt(r#"{"model":"m","input":"x","max_output_tokens":256}"#);
        assert_eq!(out["max_output_tokens"], 256);
    }

    #[test]
    fn tools_use_flat_schema_not_nested_function() {
        let out = rt(
            r#"{"model":"m","input":"x","tools":[{"type":"function","name":"f","description":"d","parameters":{"type":"object"}}]}"#,
        );
        assert_eq!(out["tools"][0]["name"], "f");
        assert!(out["tools"][0].get("function").is_none(), "Responses 不套 function");
    }

    #[test]
    fn decodes_response_with_output_text() {
        let (resp, _) = decode_response(
            br#"{"id":"resp_1","model":"gpt-5","status":"completed","output":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi there"}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(resp.concat_text(), "hi there");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn decodes_response_with_function_call() {
        let (resp, _) = decode_response(
            br#"{"id":"r","model":"gpt-5","status":"completed","output":[
                {"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        match &resp.content[0] {
            ContentBlock::ToolUse { id, input, .. } => {
                assert_eq!(id, "c1");
                assert_eq!(input["a"], 1);
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn incomplete_status_maps_to_length() {
        let (resp, _) = decode_response(
            br#"{"id":"r","model":"m","status":"incomplete","output":[]}"#,
        )
        .unwrap();
        assert_eq!(resp.finish_reason, FinishReason::Length);
    }

    /// 与 Chat 相同的陷阱：input_tokens 含缓存，必须折算。
    #[test]
    fn cached_tokens_are_subtracted_from_input_tokens() {
        let (_, usage) = decode_response(
            br#"{"id":"r","model":"m","status":"completed","output":[],
                 "usage":{"input_tokens":1000,"output_tokens":50,"total_tokens":1050,
                          "input_tokens_details":{"cached_tokens":750},
                          "output_tokens_details":{"reasoning_tokens":20}}}"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 250);
        assert_eq!(usage.cache_read_tokens, 750);
        assert_eq!(usage.reasoning_tokens, 20);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn encode_response_reports_inclusive_input_tokens() {
        let resp = UnifiedResponse {
            id: "r".into(),
            model: "m".into(),
            content: vec![ContentBlock::text("x")],
            finish_reason: FinishReason::Stop,
        };
        let usage = UnifiedUsage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 900,
            ..Default::default()
        };
        let out: Value =
            serde_json::from_slice(&encode_response(&resp, &usage).unwrap()).unwrap();
        assert_eq!(out["usage"]["input_tokens"], 1000, "必须含缓存");
        assert_eq!(out["usage"]["input_tokens_details"]["cached_tokens"], 900);
    }

    #[test]
    fn reasoning_item_becomes_thinking() {
        let (resp, _) = decode_response(
            br#"{"id":"r","model":"m","status":"completed","output":[
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thought"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ans"}]}
            ]}"#,
        )
        .unwrap();
        assert!(matches!(resp.content[0], ContentBlock::Thinking { .. }));
        assert_eq!(resp.concat_text(), "ans");
    }

    #[test]
    fn unknown_fields_survive_roundtrip() {
        let out = rt(r#"{"model":"m","input":"x","store":false,"previous_response_id":"r1"}"#);
        assert_eq!(out["store"], false);
        assert_eq!(out["previous_response_id"], "r1");
    }
}
