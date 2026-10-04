//! Anthropic `/v1/messages` 响应的解析与生成。

use serde_json::{json, Map, Value};

use super::request::{encode_block, decode_content};
use crate::protocol::codec::ConvertError;
use crate::protocol::dto::{
    FinishReason, Protocol, UnifiedResponse, UnifiedUsage, UsageSource,
};

const P: Protocol = Protocol::AnthropicMessages;

pub fn decode_response(raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError> {
    let v: Value = serde_json::from_slice(raw)?;

    let id = v
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("msg_unknown")
        .to_string();
    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();

    let content = decode_content(v.get("content").unwrap_or(&Value::Null))?;

    let finish_reason = v
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(FinishReason::parse)
        .unwrap_or(FinishReason::Stop);

    let usage = decode_usage(v.get("usage"));

    Ok((
        UnifiedResponse {
            id,
            model,
            content,
            finish_reason,
        },
        usage,
    ))
}

/// Anthropic 的 usage 字段。
///
/// 与 OpenAI 不同，Anthropic 的 `input_tokens` **不含**缓存部分 —— 这正是
/// 本项目 IR 采用的口径，因此这里是一一对应，无需折算。
pub fn decode_usage(v: Option<&Value>) -> UnifiedUsage {
    let Some(u) = v else {
        return UnifiedUsage::default();
    };

    let input_tokens = u
        .get("input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let output_tokens = u
        .get("output_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = u
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_creation_tokens = u
        .get("cache_creation_input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);

    // 若上游没给 total，按 Anthropic 语义把缓存算进去。
    let total_tokens = u
        .get("total_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or_else(|| {
            input_tokens
                .saturating_add(cache_read_tokens)
                .saturating_add(cache_creation_tokens)
                .saturating_add(output_tokens)
        });

    UnifiedUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        reasoning_tokens: 0,
        total_tokens,
        source: UsageSource::Upstream,
        raw: Some(u.clone()),
    }
}

pub fn encode_response(
    resp: &UnifiedResponse,
    usage: &UnifiedUsage,
) -> Result<Vec<u8>, ConvertError> {
    let mut obj = Map::new();
    obj.insert("id".into(), json!(resp.id));
    obj.insert("type".into(), json!("message"));
    obj.insert("role".into(), json!("assistant"));
    obj.insert("model".into(), json!(resp.model));

    obj.insert(
        "content".into(),
        Value::Array(resp.content.iter().map(encode_block).collect()),
    );

    obj.insert(
        "stop_reason".into(),
        json!(resp.finish_reason.to_wire(P)),
    );
    obj.insert("stop_sequence".into(), Value::Null);
    obj.insert("usage".into(), encode_usage(usage));

    serde_json::to_vec(&Value::Object(obj)).map_err(|e| ConvertError::encode(P, e))
}

pub fn encode_usage(u: &UnifiedUsage) -> Value {
    let mut obj = Map::new();
    obj.insert("input_tokens".into(), json!(u.input_tokens));
    obj.insert("output_tokens".into(), json!(u.output_tokens));
    // 仅在非零时输出，避免给不认识这些字段的客户端造成困惑。
    if u.cache_read_tokens > 0 {
        obj.insert(
            "cache_read_input_tokens".into(),
            json!(u.cache_read_tokens),
        );
    }
    if u.cache_creation_tokens > 0 {
        obj.insert(
            "cache_creation_input_tokens".into(),
            json!(u.cache_creation_tokens),
        );
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::ContentBlock;

    #[test]
    fn decodes_text_response() {
        let (resp, usage) = decode_response(
            br#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-sonnet-5",
                 "content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn",
                 "usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();

        assert_eq!(resp.id, "msg_1");
        assert_eq!(resp.model, "claude-sonnet-5");
        assert_eq!(resp.concat_text(), "hello");
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn anthropic_input_tokens_excludes_cache_by_definition() {
        // 这是 IR 的口径基准：input_tokens 保持原样，缓存单独记账。
        let (_, usage) = decode_response(
            br#"{"id":"m","model":"x","content":[],"stop_reason":"end_turn",
                 "usage":{"input_tokens":100,"output_tokens":20,
                          "cache_read_input_tokens":900,"cache_creation_input_tokens":50}}"#,
        )
        .unwrap();

        assert_eq!(usage.input_tokens, 100, "不得把缓存并进 input_tokens");
        assert_eq!(usage.cache_read_tokens, 900);
        assert_eq!(usage.cache_creation_tokens, 50);
        assert_eq!(usage.total(), 1070);
    }

    #[test]
    fn decodes_tool_use_response() {
        let (resp, _) = decode_response(
            br#"{"id":"m","model":"x","content":[
                 {"type":"tool_use","id":"tu_1","name":"search","input":{"q":"rust"}}],
                 "stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#,
        )
        .unwrap();

        assert_eq!(resp.finish_reason, FinishReason::ToolUse);
        match &resp.content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "search");
                assert_eq!(input["q"], "rust");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn missing_usage_yields_zeros() {
        let (_, usage) = decode_response(
            br#"{"id":"m","model":"x","content":[],"stop_reason":"end_turn"}"#,
        )
        .unwrap();
        assert_eq!(usage.total(), 0);
        assert!(usage.is_empty());
    }

    #[test]
    fn encode_response_roundtrips() {
        let (resp, usage) = decode_response(
            br#"{"id":"msg_9","model":"claude-opus-5","content":[{"type":"text","text":"ok"}],
                 "stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":3}}"#,
        )
        .unwrap();

        let encoded = encode_response(&resp, &usage).unwrap();
        let v: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(v["id"], "msg_9");
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["text"], "ok");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 7);
        // 零值缓存字段不应出现
        assert!(v["usage"].get("cache_read_input_tokens").is_none());
    }

    #[test]
    fn encode_usage_emits_cache_fields_only_when_nonzero() {
        let u = UnifiedUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 30,
            cache_creation_tokens: 0,
            ..Default::default()
        };
        let v = encode_usage(&u);
        assert_eq!(v["cache_read_input_tokens"], 30);
        assert!(v.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn stop_reason_maps_to_anthropic_wire_values() {
        assert_eq!(FinishReason::Stop.to_wire(P), "end_turn");
        assert_eq!(FinishReason::ToolUse.to_wire(P), "tool_use");
        assert_eq!(FinishReason::Length.to_wire(P), "max_tokens");
    }
}
