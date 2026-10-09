//! Anthropic `/v1/messages` 请求的解析与生成。

use serde_json::{json, Map, Value};

use crate::protocol::codec::ConvertError;
use crate::protocol::dto::{
    ContentBlock, Protocol, ReasoningConfig, Role, ToolChoice, ToolDef, UnifiedMessage,
    UnifiedRequest,
};

const P: Protocol = Protocol::AnthropicMessages;

/// 已建模的顶层字段；其余字段进 `extra` 原样透传。
const KNOWN_TOP_LEVEL: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "max_tokens",
    "temperature",
    "top_p",
    "stop_sequences",
    "stream",
    "thinking",
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
    req.max_tokens = obj
        .get("max_tokens")
        .and_then(|m| m.as_u64())
        .map(|m| m as u32);
    req.temperature = obj.get("temperature").and_then(|t| t.as_f64());
    req.top_p = obj.get("top_p").and_then(|t| t.as_f64());

    req.stop = obj
        .get("stop_sequences")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if let Some(sys) = obj.get("system") {
        req.system = decode_content(sys)?;
    }

    req.messages = obj
        .get("messages")
        .and_then(|m| m.as_array())
        .ok_or_else(|| ConvertError::decode_request(P, "缺少 messages 字段"))?
        .iter()
        .map(decode_message)
        .collect::<Result<Vec<_>, _>>()?;

    if let Some(tools) = obj.get("tools").and_then(|t| t.as_array()) {
        req.tools = tools.iter().filter_map(decode_tool).collect();
    }

    if let Some(tc) = obj.get("tool_choice") {
        req.tool_choice = decode_tool_choice(tc);
    }

    if let Some(th) = obj.get("thinking") {
        let enabled = th.get("type").and_then(|t| t.as_str()) == Some("enabled");
        if enabled {
            req.reasoning = Some(ReasoningConfig {
                enabled: true,
                budget_tokens: th
                    .get("budget_tokens")
                    .and_then(|b| b.as_u64())
                    .map(|b| b as u32),
                effort: None,
            });
        }
    }

    // 未建模字段原样保留，避免转换时丢掉新特性（如 metadata、service_tier）。
    for (k, val) in obj {
        if !KNOWN_TOP_LEVEL.contains(&k.as_str()) {
            req.extra.insert(k.clone(), val.clone());
        }
    }

    Ok(req)
}

pub fn encode_request(req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError> {
    let mut obj = Map::new();
    obj.insert("model".into(), json!(req.model));

    if let Some(m) = req.max_tokens {
        obj.insert("max_tokens".into(), json!(m));
    } else {
        // Anthropic 的 max_tokens 是必填项。IR 里缺失时给一个足够大的默认值，
        // 否则上游会直接 400。
        obj.insert("max_tokens".into(), json!(8192));
    }

    if !req.system.is_empty() {
        obj.insert("system".into(), encode_content(&req.system));
    }

    obj.insert(
        "messages".into(),
        Value::Array(req.messages.iter().map(encode_message).collect()),
    );

    if !req.tools.is_empty() {
        obj.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(encode_tool).collect()),
        );
    }

    if let Some(tc) = &req.tool_choice {
        obj.insert("tool_choice".into(), encode_tool_choice(tc));
    }

    if let Some(t) = req.temperature {
        obj.insert("temperature".into(), json!(t));
    }
    if let Some(t) = req.top_p {
        obj.insert("top_p".into(), json!(t));
    }
    if !req.stop.is_empty() {
        obj.insert("stop_sequences".into(), json!(req.stop));
    }
    if req.stream {
        obj.insert("stream".into(), json!(true));
    }

    if let Some(r) = &req.reasoning {
        if r.enabled {
            let mut th = json!({ "type": "enabled" });
            if let Some(b) = r.budget_tokens {
                th["budget_tokens"] = json!(b);
            }
            obj.insert("thinking".into(), th);
        }
    }

    for (k, v) in &req.extra {
        // 已显式写入的字段优先，不被 extra 覆盖。
        obj.entry(k.clone()).or_insert_with(|| v.clone());
    }

    serde_json::to_vec(&Value::Object(obj)).map_err(|e| ConvertError::encode(P, e))
}

// ---------------------------------------------------------------------------
// 内容块
// ---------------------------------------------------------------------------

/// Anthropic 的 content 既可以是字符串，也可以是块数组。
pub fn decode_content(v: &Value) -> Result<Vec<ContentBlock>, ConvertError> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![ContentBlock::text(s.clone())]),
        Value::Array(items) => items.iter().map(decode_block).collect(),
        other => Err(ConvertError::decode_request(
            P,
            format!("content 既不是字符串也不是数组: {other}"),
        )),
    }
}

fn decode_block(v: &Value) -> Result<ContentBlock, ConvertError> {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("text");

    Ok(match ty {
        "text" => ContentBlock::Text {
            text: v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
        },
        "image" => {
            let source = v.get("source").cloned().unwrap_or(Value::Null);
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("image/png")
                .to_string();
            // base64 与 url 两种 source 统一压进 data 字段，用前缀区分。
            let data = match source.get("type").and_then(|t| t.as_str()) {
                Some("url") => source
                    .get("url")
                    .and_then(|u| u.as_str())
                    .map(|u| format!("url:{u}"))
                    .unwrap_or_default(),
                _ => source
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or_default()
                    .to_string(),
            };
            ContentBlock::Image { media_type, data }
        }
        "tool_use" => ContentBlock::ToolUse {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string(),
            name: v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string(),
            input: v.get("input").cloned().unwrap_or_else(|| json!({})),
        },
        "tool_result" => ContentBlock::ToolResult {
            tool_use_id: v
                .get("tool_use_id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string(),
            // tool_result 的 content 可以是字符串或块数组
            content: decode_content(v.get("content").unwrap_or(&Value::Null))?,
            is_error: v
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false),
        },
        "thinking" => ContentBlock::Thinking {
            text: v
                .get("thinking")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            signature: v
                .get("signature")
                .and_then(|s| s.as_str())
                .map(String::from),
        },
        "redacted_thinking" => ContentBlock::RedactedThinking {
            data: v
                .get("data")
                .and_then(|d| d.as_str())
                .unwrap_or_default()
                .to_string(),
        },
        other => {
            // 不认识的类型整块留着，**不再拒收整个请求**。
            //
            // 这里踩过坑：Claude Code 带上附件时会发 `document` 块，解不出就回
            // 400，用户看到的是"发个文件都发不出去"。可这个块对 Apilot 本就不需要
            // 理解 —— 渠道支持 Anthropic 时请求逐字直通，它一个字节都不用改。
            // 硬失败挡掉的是那条本来完全没问题的路。
            tracing::debug!(block_type = other, "未建模的 content block 类型，整块原样保留");
            ContentBlock::Unmodeled { raw: v.clone() }
        }
    })
}

/// 内容块数组 → Anthropic 的 content 表示。
pub fn encode_content(blocks: &[ContentBlock]) -> Value {
    // 单个纯文本块可以压成字符串，但这里统一输出数组：行为更可预测，
    // 且避免某些网关对字符串形式的 content 处理不一致。
    Value::Array(blocks.iter().map(encode_block).collect())
}

pub fn encode_block(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
        ContentBlock::Image { media_type, data } => {
            let source = if let Some(url) = data.strip_prefix("url:") {
                json!({ "type": "url", "url": url })
            } else {
                json!({ "type": "base64", "media_type": media_type, "data": data })
            };
            json!({ "type": "image", "source": source })
        }
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut v = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": encode_content(content),
            });
            // is_error=false 时省略，保持与上游一致的精简输出。
            if *is_error {
                v["is_error"] = json!(true);
            }
            v
        }
        ContentBlock::Thinking { text, signature } => {
            let mut v = json!({ "type": "thinking", "thinking": text });
            if let Some(s) = signature {
                v["signature"] = json!(s);
            }
            v
        }
        ContentBlock::RedactedThinking { data } => {
            json!({ "type": "redacted_thinking", "data": data })
        }
        // 原样写回 —— 这就是「解不出也要保真」的兑现处。
        ContentBlock::Unmodeled { raw } => raw.clone(),
    }
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

fn decode_message(v: &Value) -> Result<UnifiedMessage, ConvertError> {
    let role = match v.get("role").and_then(|r| r.as_str()) {
        Some("assistant") => Role::Assistant,
        // Anthropic 只有 user/assistant；tool_result 也是放在 user 消息里的。
        _ => Role::User,
    };
    let content = decode_content(v.get("content").unwrap_or(&Value::Null))?;
    Ok(UnifiedMessage::new(role, content))
}

fn encode_message(m: &UnifiedMessage) -> Value {
    json!({
        "role": if m.role == Role::Assistant { "assistant" } else { "user" },
        "content": encode_content(&m.content),
    })
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn decode_tool(v: &Value) -> Option<ToolDef> {
    let name = v.get("name")?.as_str()?.to_string();
    Some(ToolDef {
        name,
        // Anthropic 没有 namespace 的概念，工具本来就是平的。
        namespace: None,
        description: v
            .get("description")
            .and_then(|d| d.as_str())
            .map(String::from),
        input_schema: v
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "object" })),
    })
}

fn encode_tool(t: &ToolDef) -> Value {
    let mut v = json!({
        "name": t.name,
        "input_schema": t.input_schema,
    });
    if let Some(d) = &t.description {
        v["description"] = json!(d);
    }
    v
}

fn decode_tool_choice(v: &Value) -> Option<ToolChoice> {
    match v.get("type").and_then(|t| t.as_str())? {
        "auto" => Some(ToolChoice::Auto),
        "any" => Some(ToolChoice::Required),
        "none" => Some(ToolChoice::None),
        "tool" => v
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| ToolChoice::Tool { name: n.to_string() }),
        _ => None,
    }
}

fn encode_tool_choice(tc: &ToolChoice) -> Value {
    match tc {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::None => json!({ "type": "none" }),
        // Anthropic 没有 "required"，用 "any" 表达"必须调用某个工具"。
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::Tool { name } => json!({ "type": "tool", "name": name }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(raw: &str) -> Value {
        let req = decode_request(raw.as_bytes()).expect("应当解析成功");
        let encoded = encode_request(&req).expect("应当编码成功");
        serde_json::from_slice(&encoded).expect("编码结果应当是合法 JSON")
    }

    #[test]
    fn decodes_minimal_request() {
        let req = decode_request(
            br#"{"model":"claude-sonnet-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .unwrap();
        assert_eq!(req.model, "claude-sonnet-5");
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].concat_text(), "hi");
        assert!(!req.stream);
    }

    #[test]
    fn decodes_string_and_array_system() {
        let a = decode_request(
            br#"{"model":"m","max_tokens":1,"system":"be brief","messages":[]}"#,
        )
        .unwrap();
        assert_eq!(a.system.len(), 1);
        assert_eq!(a.system[0].as_text(), Some("be brief"));

        let b = decode_request(
            br#"{"model":"m","max_tokens":1,"system":[{"type":"text","text":"x"}],"messages":[]}"#,
        )
        .unwrap();
        assert_eq!(b.system[0].as_text(), Some("x"));
    }

    #[test]
    fn decodes_tool_use_and_result() {
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"get_weather","input":{"city":"SF"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"sunny","is_error":false}]}
            ]}"#,
        )
        .unwrap();

        match &req.messages[0].content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "get_weather");
                assert_eq!(input["city"], "SF");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }

        match &req.messages[1].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu_1");
                assert_eq!(content[0].as_text(), Some("sunny"));
                assert!(!is_error);
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[test]
    fn tool_result_with_array_content() {
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}
            ]}]}"#,
        )
        .unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content.len(), 2);
                assert_eq!(content[1].as_text(), Some("b"));
            }
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    #[test]
    fn decodes_thinking_block_with_signature() {
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"assistant","content":[
                {"type":"thinking","thinking":"hmm","signature":"sig123"}
            ]}]}"#,
        )
        .unwrap();
        match &req.messages[0].content[0] {
            ContentBlock::Thinking { text, signature } => {
                assert_eq!(text, "hmm");
                assert_eq!(signature.as_deref(), Some("sig123"));
            }
            other => panic!("期望 Thinking，得到 {other:?}"),
        }
    }

    #[test]
    fn decodes_base64_and_url_images() {
        let b64 = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}}
            ]}]}"#,
        )
        .unwrap();
        match &b64.messages[0].content[0] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "AAA");
            }
            other => panic!("期望 Image，得到 {other:?}"),
        }

        let url = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[
                {"type":"image","source":{"type":"url","url":"https://x/y.png"}}
            ]}]}"#,
        )
        .unwrap();
        match &url.messages[0].content[0] {
            ContentBlock::Image { data, .. } => assert_eq!(data, "url:https://x/y.png"),
            other => panic!("期望 Image，得到 {other:?}"),
        }
    }

    #[test]
    fn tool_choice_maps_any_to_required_and_back() {
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[],"tool_choice":{"type":"any"}}"#,
        )
        .unwrap();
        assert_eq!(req.tool_choice, Some(ToolChoice::Required));
        assert_eq!(
            encode_tool_choice(&ToolChoice::Required),
            json!({ "type": "any" })
        );
    }

    #[test]
    fn thinking_config_roundtrips() {
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[],"thinking":{"type":"enabled","budget_tokens":2048}}"#,
        )
        .unwrap();
        let r = req.reasoning.clone().unwrap();
        assert!(r.enabled);
        assert_eq!(r.budget_tokens, Some(2048));

        let out = roundtrip(
            r#"{"model":"m","max_tokens":1,"messages":[],"thinking":{"type":"enabled","budget_tokens":2048}}"#,
        );
        assert_eq!(out["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn missing_max_tokens_gets_a_default_on_encode() {
        // Anthropic 要求 max_tokens 必填；缺失时补默认值，否则上游 400。
        let req = UnifiedRequest::new("m");
        let out: Value = serde_json::from_slice(&encode_request(&req).unwrap()).unwrap();
        assert_eq!(out["max_tokens"], 8192);
    }

    #[test]
    fn unknown_fields_are_preserved_through_roundtrip() {
        let out = roundtrip(
            r#"{"model":"m","max_tokens":1,"messages":[],"metadata":{"user_id":"u1"},"service_tier":"auto"}"#,
        );
        assert_eq!(out["metadata"]["user_id"], "u1");
        assert_eq!(out["service_tier"], "auto");
    }

    #[test]
    fn rejects_non_object_body() {
        assert!(decode_request(b"[]").is_err());
        assert!(decode_request(b"not json").is_err());
    }

    #[test]
    fn rejects_request_without_model() {
        let e = decode_request(br#"{"messages":[]}"#).unwrap_err();
        assert!(e.to_string().contains("model"));
    }

    #[test]
    fn unknown_content_block_type_is_kept_verbatim_instead_of_rejected() {
        // 以前这里显式报错，理由是"静默丢弃会让用户以为功能正常但内容莫名消失"。
        // 理由成立，结论反了：真正的丢弃发生在跨协议转换那一侧，而报错把
        // **同协议直通**也一起挡掉了 —— Claude Code 带附件时发的 `document`
        // 就是被这条打回去的（400 未知的 content block 类型）。
        //
        // 现在整块原样留着，两头都不占：直通时逐字写回，转码时目标协议装不下才丢。
        let req = decode_request(
            br#"{"model":"m","max_tokens":1,"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AAA"},"title":"a.pdf"},{"type":"text","text":"\u770b\u770b\u8fd9\u4e2a"}]}]}"#,
        )
        .expect("未建模的内容块不该让请求失败");

        assert!(
            matches!(req.messages[0].content[0], ContentBlock::Unmodeled { .. }),
            "未知块应整块留下"
        );
        assert_eq!(req.messages[0].content.len(), 2, "其余块照常解析");

        let out = encode_request(&req).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let block = &v["messages"][0]["content"][0];
        assert_eq!(block["type"], "document");
        assert_eq!(block["source"]["media_type"], "application/pdf");
        assert_eq!(block["title"], "a.pdf", "块里的字段一个都不能少");
    }
}
