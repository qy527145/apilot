//! OpenAI Chat Completions 请求的解析与生成。

use serde_json::{json, Map, Value};

use crate::protocol::codec::ConvertError;
use crate::protocol::dto::{
    ContentBlock, Protocol, ReasoningConfig, Role, ToolChoice, ToolDef, UnifiedMessage,
    UnifiedRequest,
};
use crate::protocol::shared::tools::parse_tool_input;

const P: Protocol = Protocol::OpenAiChat;

/// 已被 IR 显式建模、编码时会重新写出的字段。
///
/// **不在这个列表里的字段一律进 `extra` 原样透传。**
/// `stream_options` 刻意不列在这里：编码时要保留用户设置的其他键
/// （例如 `include_usage` 之外的扩展），只在 extra 的基础上强制打开 usage。
const KNOWN_TOP_LEVEL: &[&str] = &[
    "model",
    "messages",
    "tools",
    "tool_choice",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "stop",
    "stream",
    "reasoning_effort",
];

pub fn decode_request(raw: &[u8]) -> Result<UnifiedRequest, ConvertError> {
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

    // max_tokens 是旧字段，新模型用 max_completion_tokens；两者都认。
    req.max_tokens = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(|m| m.as_u64())
        .map(|m| m as u32);

    req.temperature = obj.get("temperature").and_then(|t| t.as_f64());
    req.top_p = obj.get("top_p").and_then(|t| t.as_f64());

    req.stop = match obj.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };

    // OpenAI 的 reasoning_effort 映射到 IR 的 ReasoningConfig。
    if let Some(effort) = obj.get("reasoning_effort").and_then(|e| e.as_str()) {
        req.reasoning = Some(ReasoningConfig {
            enabled: true,
            budget_tokens: None,
            effort: Some(effort.to_string()),
        });
    }

    let messages = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| ConvertError::decode_request(P, "缺少 messages 字段"))?;

    for m in messages {
        decode_message(m, &mut req)?;
    }

    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        req.tools = tools.iter().filter_map(decode_tool).collect();
    }

    if let Some(tc) = obj.get("tool_choice") {
        req.tool_choice = decode_tool_choice(tc);
    }

    for (k, val) in obj {
        if !KNOWN_TOP_LEVEL.contains(&k.as_str()) {
            req.extra.insert(k.clone(), val.clone());
        }
    }

    Ok(req)
}

/// 把一条 OpenAI 消息折进 IR。
///
/// system 消息会被提升到 `req.system`（IR 与 Anthropic 一致，system 在顶层）；
/// `role: "tool"` 消息折成 user 消息里的 `tool_result` 块。
fn decode_message(v: &Value, req: &mut UnifiedRequest) -> Result<(), ConvertError> {
    let role_str = v.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = v.get("content").unwrap_or(&Value::Null);

    match role_str {
        "system" | "developer" => {
            // developer 是 OpenAI 给 o 系列准备的 system 替身，语义等价。
            req.system.extend(decode_content(content)?);
            Ok(())
        }
        "tool" => {
            // 独立的 tool 消息 → user 消息里的 tool_result 块。
            let tool_use_id = v
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string();
            let result_content = decode_content(content)?;
            req.messages.push(UnifiedMessage::new(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_use_id,
                    content: result_content,
                    is_error: false,
                }],
            ));
            Ok(())
        }
        "assistant" => {
            let mut blocks = decode_content(content)?;

            // 先放推理内容，再放正文，保持与实际生成顺序一致。
            if let Some(rc) = v.get("reasoning_content").and_then(|r| r.as_str()) {
                if !rc.is_empty() {
                    blocks.insert(
                        0,
                        ContentBlock::Thinking {
                            text: rc.to_string(),
                            signature: None,
                        },
                    );
                }
            }

            if let Some(calls) = v.get("tool_calls").and_then(|t| t.as_array()) {
                for call in calls {
                    let func = call.get("function").unwrap_or(&Value::Null);
                    let args = func
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");
                    blocks.push(ContentBlock::ToolUse {
                        id: call
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        name: func
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        input: parse_tool_input(args),
                    });
                }
            }

            req.messages.push(UnifiedMessage::new(Role::Assistant, blocks));
            Ok(())
        }
        _ => {
            let blocks = decode_content(content)?;
            req.messages.push(UnifiedMessage::new(Role::User, blocks));
            Ok(())
        }
    }
}

fn decode_content(v: &Value) -> Result<Vec<ContentBlock>, ConvertError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![ContentBlock::text(s.clone())]),
        Value::Array(items) => items.iter().map(decode_part).collect(),
        other => Err(ConvertError::decode_request(
            P,
            format!("content 既不是字符串也不是数组: {other}"),
        )),
    }
}

fn decode_part(v: &Value) -> Result<ContentBlock, ConvertError> {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("text") {
        "text" | "input_text" => Ok(ContentBlock::text(
            v.get("text").and_then(|t| t.as_str()).unwrap_or_default(),
        )),
        "image_url" => {
            let url = v
                .get("image_url")
                .and_then(|i| i.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or_default();
            // data URL 形如 `data:image/png;base64,AAAA`，拆出 media_type。
            let (media_type, data) = match url.split_once(',') {
                Some((head, payload)) if head.starts_with("data:") => {
                    let mt = head
                        .trim_start_matches("data:")
                        .split(';')
                        .next()
                        .unwrap_or("image/png")
                        .to_string();
                    (mt, payload.to_string())
                }
                _ => ("image/png".to_string(), format!("url:{url}")),
            };
            Ok(ContentBlock::Image { media_type, data })
        }
        "refusal" => Ok(ContentBlock::text(
            v.get("refusal").and_then(|r| r.as_str()).unwrap_or_default(),
        )),
        // 同 anthropic 那侧：不认识的 part 整块留着，不拒收整个请求。
        other => {
            tracing::debug!(part_type = other, "未建模的 content part 类型，整块原样保留");
            Ok(ContentBlock::Unmodeled { raw: v.clone() })
        }
    }
}

pub fn encode_request(req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError> {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(req.model));

    let mut messages: Vec<Value> = Vec::new();

    // IR 的 system 在顶层，OpenAI 要放在 messages 首位。
    if !req.system.is_empty() {
        messages.push(json!({
            "role": "system",
            "content": encode_content(&req.system),
        }));
    }

    for m in &req.messages {
        encode_message(m, &mut messages);
    }

    obj.insert("messages".into(), Value::Array(messages));

    if !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(encode_tool).collect()),
        );
    }
    if let Some(tc) = &req.tool_choice {
        obj.insert("tool_choice".into(), encode_tool_choice(tc));
    }
    if let Some(m) = req.max_tokens {
        obj.insert("max_tokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        obj.insert("temperature".into(), json!(t));
    }
    if let Some(t) = req.top_p {
        obj.insert("top_p".into(), json!(t));
    }
    if !req.stop.is_empty() {
        obj.insert("stop".into(), json!(req.stop));
    }
    if req.stream {
        obj.insert("stream".into(), json!(true));
        // 保留用户原有的 stream_options，只强制打开 usage —— 不显式要求 usage，
        // 流式响应就不会带 usage 块，计费只能退化成估算。
        let mut so = req
            .extra
            .get("stream_options")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| json!({}));
        so["include_usage"] = json!(true);
        obj.insert("stream_options".into(), so);
    }
    if let Some(r) = &req.reasoning {
        if r.enabled {
            if let Some(e) = &r.effort {
                obj.insert("reasoning_effort".into(), json!(e));
            }
        }
    }

    for (k, v) in &req.extra {
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }

    serde_json::to_vec(&Value::Object(obj)).map_err(|e| ConvertError::encode(P, e))
}

/// 一条 IR 消息可能拆成多条 OpenAI 消息：助手消息带工具调用时，
/// tool_calls 留在 assistant 消息上；而 IR 里独立的 tool_result 块要拆成 `role: "tool"`。
fn encode_message(m: &UnifiedMessage, out: &mut Vec<Value>) {
    match m.role {
        Role::System => {
            out.push(json!({
                "role": "system",
                "content": encode_content(&m.content),
            }));
        }
        Role::Tool => {
            // 少见路径：IR 里被显式标成 Tool 的消息，按 tool_result 处理。
            for b in &m.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = b
                {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": crate::protocol::shared::tools::flatten_tool_result_text(content),
                    }));
                }
            }
        }
        Role::User => {
            // user 消息里可能混着 tool_result（来自 Anthropic 风格的历史）。
            // OpenAI 不接受这种混合，必须拆开：tool_result 单独成 tool 消息。
            let mut plain: Vec<&ContentBlock> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": crate::protocol::shared::tools::flatten_tool_result_text(content),
                        }));
                    }
                    other => plain.push(other),
                }
            }
            if !plain.is_empty() {
                let blocks: Vec<Value> = plain.iter().map(|b| encode_block(b)).collect();
                out.push(json!({ "role": "user", "content": Value::Array(blocks) }));
            }
        }
        Role::Assistant => {
            let mut blocks: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut reasoning: Option<String> = None;

            for b in &m.content {
                match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                // OpenAI 的 arguments 是 JSON **字符串**，不是对象。
                                "arguments": serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".into()),
                            },
                        }));
                    }
                    ContentBlock::Thinking { text, .. } => {
                        reasoning = Some(text.clone());
                    }
                    // 加密思考无法表达，丢弃（调用方已在 IR 层记录）。
                    ContentBlock::RedactedThinking { .. } => {}
                    other => blocks.push(encode_block(other)),
                }
            }

            let mut msg = Map::new();
            msg.insert("role".into(), json!("assistant"));
            // 只发工具调用不发文本时，content 必须是 null 而不是空数组。
            if blocks.is_empty() {
                msg.insert("content".into(), Value::Null);
            } else {
                msg.insert("content".into(), Value::Array(blocks));
            }
            if let Some(r) = reasoning {
                msg.insert("reasoning_content".into(), json!(r));
            }
            if !tool_calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(tool_calls));
            }

            out.push(Value::Object(msg));
        }
    }
}

fn encode_content(blocks: &[ContentBlock]) -> Value {
    Value::Array(blocks.iter().map(encode_block).collect())
}

fn encode_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Image { media_type, data } => {
            let url = if let Some(u) = data.strip_prefix("url:") {
                u.to_string()
            } else {
                format!("data:{media_type};base64,{data}")
            };
            json!({ "type": "image_url", "image_url": { "url": url } })
        }
        // 工具相关块由 encode_message 单独处理，这里给一个安全的兜底表示，
        // 避免出现在不该出现的位置时产生非法 JSON。
        ContentBlock::ToolUse { name, input, .. } => json!({
            "type": "text",
            "text": format!("[tool_use {name}: {input}]"),
        }),
        ContentBlock::ToolResult { content, .. } => json!({
            "type": "text",
            "text": crate::protocol::shared::tools::flatten_tool_result_text(content),
        }),
        ContentBlock::Thinking { text, .. } => {
            json!({ "type": "text", "text": text })
        }
        ContentBlock::RedactedThinking { .. } => json!({ "type": "text", "text": "" }),
        // Chat 没有能装下它的位置，只能丢 —— 与加密思考同一个处置。
        // 直通时走不到这里，所以"没建模"不等于"内容一定会丢"。
        ContentBlock::Unmodeled { .. } => json!({ "type": "text", "text": "" }),
    }
}

fn decode_tool(v: &Value) -> Option<ToolDef> {
    let func = v.get("function")?;
    Some(ToolDef {
        name: func.get("name")?.as_str()?.to_string(),
        // Chat Completions 没有 namespace 的概念，工具本来就是平的。
        namespace: None,
        description: func
            .get("description")
            .and_then(|d| d.as_str())
            .map(String::from),
        input_schema: func
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
    })
}

fn encode_tool(t: &ToolDef) -> Value {
    let mut func = json!({
        "name": t.name,
        "parameters": t.input_schema,
    });
    if let Some(d) = &t.description {
        func["description"] = json!(d);
    }
    json!({ "type": "function", "function": func })
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
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Tool { name: n.to_string() }),
        _ => None,
    }
}

fn encode_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({
            "type": "function",
            "function": { "name": name },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(raw: &str) -> Value {
        let req = decode_request(raw.as_bytes()).unwrap();
        serde_json::from_slice(&encode_request(&req).unwrap()).unwrap()
    }

    #[test]
    fn decodes_minimal_request() {
        let req = decode_request(
            br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].concat_text(), "hi");
    }

    #[test]
    fn system_message_is_lifted_to_top_level() {
        let req = decode_request(
            br#"{"model":"m","messages":[
                {"role":"system","content":"be brief"},
                {"role":"user","content":"hi"}]}"#,
        )
        .unwrap();

        assert_eq!(req.system.len(), 1, "system 应提升到顶层");
        assert_eq!(req.system[0].as_text(), Some("be brief"));
        assert_eq!(req.messages.len(), 1, "system 不应留在 messages 里");
    }

    #[test]
    fn developer_role_is_treated_as_system() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"developer","content":"rules"}]}"#,
        )
        .unwrap();
        assert_eq!(req.system[0].as_text(), Some("rules"));
    }

    #[test]
    fn decodes_tool_calls_and_arguments_string() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[
                {"id":"call_1","type":"function","function":{"name":"search","arguments":"{\"q\":\"rust\"}"}}
            ]}]}"#,
        )
        .unwrap();

        match &req.messages[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search");
                // arguments 是字符串，必须解析成对象
                assert_eq!(input["q"], "rust");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn tool_role_message_becomes_tool_result_block() {
        let req = decode_request(
            br#"{"model":"m","messages":[
                {"role":"tool","tool_call_id":"call_1","content":"sunny"}]}"#,
        )
        .unwrap();

        assert_eq!(req.messages[0].role, Role::User);
        match &req.messages[0].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content[0].as_text(), Some("sunny"));
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[test]
    fn data_url_image_is_split_into_media_type_and_payload() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,AAAA"}}]}]}"#,
        )
        .unwrap();

        match &req.messages[0].content[0] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(data, "AAAA");
            }
            other => panic!("期望 Image，得到 {other:?}"),
        }
    }

    #[test]
    fn http_image_url_is_preserved() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"user","content":[
                {"type":"image_url","image_url":{"url":"https://x/y.png"}}]}]}"#,
        )
        .unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Image { data, .. } => assert_eq!(data, "url:https://x/y.png"),
            other => panic!("期望 Image，得到 {other:?}"),
        }
    }

    #[test]
    fn tool_choice_variants_roundtrip() {
        assert_eq!(
            rt(r#"{"model":"m","messages":[],"tool_choice":"auto"}"#)["tool_choice"],
            json!("auto")
        );
        assert_eq!(
            rt(r#"{"model":"m","messages":[],"tool_choice":"required"}"#)["tool_choice"],
            json!("required")
        );
        let named = rt(
            r#"{"model":"m","messages":[],"tool_choice":{"type":"function","function":{"name":"f"}}}"#,
        );
        assert_eq!(named["tool_choice"]["function"]["name"], "f");
    }

    #[test]
    fn streaming_adds_include_usage() {
        // 不要求 usage，流式就没有计费依据
        let out = rt(r#"{"model":"m","messages":[],"stream":true}"#);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn streaming_preserves_user_stream_options() {
        let out = rt(
            r#"{"model":"m","messages":[],"stream":true,"stream_options":{"custom_flag":true}}"#,
        );
        assert_eq!(out["stream_options"]["custom_flag"], true, "用户设置不能丢");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn assistant_with_only_tool_calls_gets_null_content() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"assistant","tool_calls":[
                {"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}]}"#,
        )
        .unwrap();
        let out: Value = serde_json::from_slice(&encode_request(&req).unwrap()).unwrap();
        assert_eq!(out["messages"][0]["content"], Value::Null);
        assert_eq!(out["messages"][0]["tool_calls"][0]["function"]["name"], "f");
    }

    #[test]
    fn arguments_are_encoded_as_a_json_string_not_object() {
        let req = decode_request(
            br#"{"model":"m","messages":[{"role":"assistant","tool_calls":[
                {"id":"c","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]}]}"#,
        )
        .unwrap();
        let out: Value = serde_json::from_slice(&encode_request(&req).unwrap()).unwrap();
        let args = &out["messages"][0]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string(), "arguments 必须是字符串，实际是 {args}");
        assert_eq!(args.as_str().unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn reasoning_effort_roundtrips() {
        let out = rt(r#"{"model":"m","messages":[],"reasoning_effort":"high"}"#);
        assert_eq!(out["reasoning_effort"], "high");
    }

    #[test]
    fn max_completion_tokens_is_accepted_and_emitted_as_max_tokens() {
        let out = rt(r#"{"model":"m","messages":[],"max_completion_tokens":512}"#);
        assert_eq!(out["max_tokens"], 512);
    }

    #[test]
    fn unknown_fields_survive_roundtrip() {
        let out = rt(r#"{"model":"m","messages":[],"parallel_tool_calls":false}"#);
        assert_eq!(out["parallel_tool_calls"], false);
    }

    #[test]
    fn rejects_request_without_model() {
        assert!(decode_request(br#"{"messages":[]}"#).is_err());
    }
}
