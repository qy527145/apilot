//! OpenAI Chat Completions 响应的解析与生成。

use serde_json::{json, Map, Value};

use crate::protocol::codec::ConvertError;
use crate::protocol::dto::{
    ContentBlock, FinishReason, Protocol, UnifiedResponse, UnifiedUsage, UsageSource,
};
use crate::protocol::shared::tools::parse_tool_input;

const P: Protocol = Protocol::OpenAiChat;

pub fn decode_response(raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError> {
    let v: Value = serde_json::from_slice(raw)?;

    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ConvertError::decode_response(P, "响应里没有 choices"))?;

    let message = choice.get("message").unwrap_or(&Value::Null);
    let content = decode_message_content(message);

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(FinishReason::parse)
        .unwrap_or(FinishReason::Stop);

    let usage = decode_usage(v.get("usage"));

    Ok((
        UnifiedResponse {
            id: v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("chatcmpl_unknown")
                .to_string(),
            model: v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            content,
            finish_reason,
        },
        usage,
    ))
}

/// 从 `message` 对象解出内容块（含工具调用与推理内容）。
pub fn decode_message_content(message: &Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    if let Some(rc) = message.get("reasoning_content").and_then(|r| r.as_str()) {
        if !rc.is_empty() {
            blocks.push(ContentBlock::Thinking {
                text: rc.to_string(),
                signature: None,
            });
        }
    }

    match message.get("content") {
        Some(Value::String(s)) if !s.is_empty() => blocks.push(ContentBlock::text(s.clone())),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    blocks.push(ContentBlock::text(t));
                }
            }
        }
        _ => {}
    }

    if let Some(calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for call in calls {
            let func = call.get("function").unwrap_or(&Value::Null);
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
                input: parse_tool_input(
                    func.get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}"),
                ),
            });
        }
    }

    blocks
}

/// OpenAI 的 usage → IR 口径。
///
/// **关键折算**：OpenAI 的 `prompt_tokens` **包含**命中缓存的 token
/// （`prompt_tokens_details.cached_tokens` 是其中的一部分）。而 IR 约定
/// `input_tokens` 是**不含缓存**的 fresh 输入。因此必须做减法，
/// 否则缓存部分会被计费两次。
pub fn decode_usage(v: Option<&Value>) -> UnifiedUsage {
    let Some(u) = v else {
        return UnifiedUsage::default();
    };

    let prompt_tokens = u
        .get("prompt_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let completion_tokens = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let cached_tokens = u
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    let reasoning_tokens = u
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    // 减法前先夹住，防止个别上游把 cached_tokens 报得比 prompt_tokens 还大
    // 导致 u64 下溢 panic。
    let fresh_input = prompt_tokens.saturating_sub(cached_tokens);

    UnifiedUsage {
        input_tokens: fresh_input,
        output_tokens: completion_tokens,
        cache_read_tokens: cached_tokens,
        cache_creation_tokens: 0,
        reasoning_tokens,
        total_tokens: u
            .get("total_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens)),
        source: UsageSource::Upstream,
        raw: Some(u.clone()),
    }
}

pub fn encode_response(
    resp: &UnifiedResponse,
    usage: &UnifiedUsage,
) -> Result<Vec<u8>, ConvertError> {
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning: Option<String> = None;

    for b in &resp.content {
        match b {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::Thinking { text: t, .. } => {
                reasoning = Some(match reasoning.take() {
                    Some(prev) => prev + t,
                    None => t.clone(),
                })
            }
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                },
            })),
            // 加密思考在 Chat 协议里没有对应表示。
            ContentBlock::RedactedThinking { .. } => {}
            _ => {}
        }
    }

    message.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            json!(text)
        },
    );
    if let Some(r) = reasoning {
        message.insert("reasoning_content".into(), json!(r));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    let body = json!({
        "id": resp.id,
        "object": "chat.completion",
        "created": crate::util::now_secs(),
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": resp.finish_reason.to_wire(P),
        }],
        "usage": encode_usage(usage),
    });

    serde_json::to_vec(&body).map_err(|e| ConvertError::encode(P, e))
}

/// IR usage → OpenAI 口径。
///
/// 反向折算：`prompt_tokens` 必须**含**缓存，因为 OpenAI 客户端会拿它做分母
/// 计算缓存命中率。
pub fn encode_usage(u: &UnifiedUsage) -> Value {
    let prompt_tokens = u
        .input_tokens
        .saturating_add(u.cache_read_tokens)
        .saturating_add(u.cache_creation_tokens);

    let mut usage = json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": u.output_tokens,
        "total_tokens": prompt_tokens.saturating_add(u.output_tokens),
    });

    if u.cache_read_tokens > 0 {
        usage["prompt_tokens_details"] = json!({ "cached_tokens": u.cache_read_tokens });
    }
    if u.reasoning_tokens > 0 {
        usage["completion_tokens_details"] = json!({ "reasoning_tokens": u.reasoning_tokens });
    }

    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_simple_response() {
        let (resp, usage) = decode_response(
            br#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt-4o",
                 "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},
                             "finish_reason":"stop"}],
                 "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
        )
        .unwrap();

        assert_eq!(resp.id, "chatcmpl-1");
        assert_eq!(resp.concat_text(), "hello");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    /// 这是全项目最关键的算术：OpenAI 的 prompt_tokens 含缓存，IR 不含。
    #[test]
    fn cached_tokens_are_subtracted_from_prompt_tokens() {
        let (_, usage) = decode_response(
            br#"{"id":"c","model":"gpt-4o","choices":[{"message":{"content":"x"},"finish_reason":"stop"}],
                 "usage":{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,
                          "prompt_tokens_details":{"cached_tokens":800}}}"#,
        )
        .unwrap();

        assert_eq!(
            usage.input_tokens, 200,
            "fresh 输入应为 1000-800=200，而不是 1000"
        );
        assert_eq!(usage.cache_read_tokens, 800);

        // 换算回 OpenAI 口径必须回到原值，证明折算是可逆的
        let back = encode_usage(&usage);
        assert_eq!(back["prompt_tokens"], 1000);
        assert_eq!(back["prompt_tokens_details"]["cached_tokens"], 800);
    }

    #[test]
    fn cached_tokens_larger_than_prompt_does_not_underflow() {
        // 防御畸形上游数据：不能 panic，也不能回绕成天文数字
        let (_, usage) = decode_response(
            br#"{"id":"c","model":"m","choices":[{"message":{"content":"x"},"finish_reason":"stop"}],
                 "usage":{"prompt_tokens":10,"completion_tokens":1,
                          "prompt_tokens_details":{"cached_tokens":999}}}"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 999);
    }

    #[test]
    fn reasoning_tokens_are_extracted() {
        let (_, usage) = decode_response(
            br#"{"id":"c","model":"o3","choices":[{"message":{"content":"x"},"finish_reason":"stop"}],
                 "usage":{"prompt_tokens":5,"completion_tokens":100,
                          "completion_tokens_details":{"reasoning_tokens":80}}}"#,
        )
        .unwrap();
        assert_eq!(usage.reasoning_tokens, 80);
        assert_eq!(usage.output_tokens, 100, "reasoning 已包含在 completion 内");
    }

    #[test]
    fn deepseek_style_cache_hit_field_is_not_confused() {
        // DeepSeek 用顶层 prompt_cache_hit_tokens；本 codec 不解析它，
        // 但必须保证不因此把 prompt_tokens 算错。
        let (_, usage) = decode_response(
            br#"{"id":"c","model":"deepseek-chat","choices":[{"message":{"content":"x"},"finish_reason":"stop"}],
                 "usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":60}}"#,
        )
        .unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    #[test]
    fn decodes_tool_calls_in_response() {
        let (resp, _) = decode_response(
            br#"{"id":"c","model":"m","choices":[{"message":{"role":"assistant","content":null,
                 "tool_calls":[{"id":"call_1","type":"function",
                                "function":{"name":"search","arguments":"{\"q\":\"x\"}"}}]},
                 "finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();

        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        match &resp.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search");
                assert_eq!(input["q"], "x");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn reasoning_content_becomes_thinking_block_first() {
        let (resp, _) = decode_response(
            br#"{"id":"c","model":"deepseek-reasoner","choices":[{"message":{
                 "role":"assistant","reasoning_content":"let me think","content":"answer"},
                 "finish_reason":"stop"}]}"#,
        )
        .unwrap();

        assert!(matches!(resp.content[0], ContentBlock::Thinking { .. }));
        assert_eq!(resp.content[1].as_text(), Some("answer"));
    }

    #[test]
    fn encode_response_roundtrips_text() {
        let (resp, usage) = decode_response(
            br#"{"id":"chatcmpl-9","model":"gpt-4o","choices":[{"message":{"content":"ok"},
                 "finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        )
        .unwrap();

        let out: Value = serde_json::from_slice(&encode_response(&resp, &usage).unwrap()).unwrap();
        assert_eq!(out["id"], "chatcmpl-9");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "ok");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 3);
    }

    #[test]
    fn encode_response_with_only_tool_calls_has_null_content() {
        let resp = UnifiedResponse {
            id: "c".into(),
            model: "m".into(),
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "f".into(),
                input: json!({"a": 1}),
            }],
            finish_reason: FinishReason::ToolUse,
        };
        let out: Value =
            serde_json::from_slice(&encode_response(&resp, &UnifiedUsage::default()).unwrap())
                .unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            r#"{"a":1}"#
        );
    }

    #[test]
    fn finish_reason_wire_mapping() {
        assert_eq!(FinishReason::Stop.to_wire(P), "stop");
        assert_eq!(FinishReason::Length.to_wire(P), "length");
        assert_eq!(FinishReason::ToolUse.to_wire(P), "tool_calls");
    }

    #[test]
    fn empty_choices_is_an_error() {
        assert!(decode_response(br#"{"id":"c","choices":[]}"#).is_err());
    }
}
