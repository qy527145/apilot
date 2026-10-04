//! OpenAI Chat Completions 流式编解码。

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::gateway::sse::SseEvent;
use crate::protocol::codec::{ConvertError, StreamDecoder, StreamEncoder};
use crate::protocol::dto::{
    ContentBlock, FinishReason, Protocol, UnifiedDelta, UnifiedUsage, UsageSource,
};
use super::response::decode_usage;

const P: Protocol = Protocol::OpenAiChat;

/// Chat 的流式协议没有"内容块"概念，这里合成一组稳定的 index，
/// 让上游 delta 在 IR 层依然有块结构可依。
const IDX_THINKING: u32 = 0;
const IDX_TEXT: u32 = 1;
/// 工具调用使用 `IDX_TOOL_BASE + tool_call_index`。
const IDX_TOOL_BASE: u32 = 2;

// ---------------------------------------------------------------------------
// 解码器
// ---------------------------------------------------------------------------

pub struct ChatStreamDecoder {
    usage: UnifiedUsage,
    text: String,
    thinking: String,
    /// 已开启过的工具调用 index，用于只在首次发 BlockStart。
    tool_started: HashMap<u64, bool>,
    /// 按工具调用 index 累积的 arguments 文本。
    tool_args: HashMap<u64, String>,
    /// 工具调用的 id/name，BlockStart 时需要。
    tool_meta: HashMap<u64, (String, String)>,
    finish_reason: Option<FinishReason>,
    finished: bool,
    started: bool,
    id: String,
    model: String,
}

impl ChatStreamDecoder {
    pub fn new() -> Self {
        Self {
            usage: UnifiedUsage::default(),
            text: String::new(),
            thinking: String::new(),
            tool_started: HashMap::new(),
            tool_args: HashMap::new(),
            tool_meta: HashMap::new(),
            finish_reason: None,
            finished: false,
            started: false,
            id: String::new(),
            model: String::new(),
        }
    }
}

impl Default for ChatStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder for ChatStreamDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<UnifiedDelta>, ConvertError> {
        // `[DONE]` 是 Chat 协议的终止标记，没有 JSON 体。
        if ev.is_done() {
            self.finished = true;
            let reason = self.finish_reason.clone().unwrap_or(FinishReason::Stop);
            return Ok(vec![UnifiedDelta::Finish(reason)]);
        }

        let v = ev.json()?;
        let mut out = Vec::new();

        if !self.started {
            self.started = true;
            self.id = v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or_default()
                .to_string();
            self.model = v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            out.push(UnifiedDelta::MessageStart {
                id: self.id.clone(),
                model: self.model.clone(),
            });
        }

        // usage 只出现在开了 include_usage 的最后一个 chunk 里，且该 chunk 的
        // choices 通常是空数组。
        if let Some(u) = v.get("usage") {
            if !u.is_null() {
                let parsed = decode_usage(Some(u));
                if !parsed.is_empty() {
                    let delta = crate::protocol::dto::UsageDelta {
                        input_tokens: Some(parsed.input_tokens),
                        output_tokens: Some(parsed.output_tokens),
                        cache_read_tokens: Some(parsed.cache_read_tokens),
                        cache_creation_tokens: None,
                        reasoning_tokens: Some(parsed.reasoning_tokens),
                        total_tokens: Some(parsed.total_tokens),
                    };
                    self.usage.merge_non_zero(&parsed);
                    out.push(UnifiedDelta::Usage(delta));
                }
            }
        }

        let Some(choice) = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
        else {
            return Ok(out);
        };

        if let Some(delta) = choice.get("delta") {
            // 推理内容先于正文出现。
            if let Some(rc) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
                if !rc.is_empty() {
                    self.thinking.push_str(rc);
                    out.push(UnifiedDelta::ThinkingDelta {
                        index: IDX_THINKING,
                        text: rc.to_string(),
                    });
                }
            }

            match delta.get("content") {
                Some(Value::String(s)) if !s.is_empty() => {
                    self.text.push_str(s);
                    out.push(UnifiedDelta::TextDelta {
                        index: IDX_TEXT,
                        text: s.clone(),
                    });
                }
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                self.text.push_str(t);
                                out.push(UnifiedDelta::TextDelta {
                                    index: IDX_TEXT,
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }

            if let Some(calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for call in calls {
                    let raw_idx = call.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                    let func = call.get("function").unwrap_or(&Value::Null);

                    let id = call
                        .get("id")
                        .and_then(|i| i.as_str())
                        .map(String::from);
                    let name = func
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(String::from);

                    // 首个分片带 id 与 name，后续分片只有 arguments。
                    if id.is_some() || name.is_some() {
                        let prev = self.tool_meta.get(&raw_idx).cloned().unwrap_or_default();
                        let merged = (
                            id.unwrap_or(prev.0),
                            name.unwrap_or(prev.1),
                        );
                        self.tool_meta.insert(raw_idx, merged.clone());

                        if !self.tool_started.contains_key(&raw_idx) {
                            self.tool_started.insert(raw_idx, true);
                            out.push(UnifiedDelta::BlockStart {
                                index: IDX_TOOL_BASE + raw_idx as u32,
                                block: ContentBlock::ToolUse {
                                    id: merged.0,
                                    name: merged.1,
                                    input: json!({}),
                                },
                            });
                        }
                    }

                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            self.tool_args
                                .entry(raw_idx)
                                .or_default()
                                .push_str(args);
                            out.push(UnifiedDelta::ToolInputDelta {
                                index: IDX_TOOL_BASE + raw_idx as u32,
                                partial_json: args.to_string(),
                            });
                        }
                    }
                }
            }
        }

        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            let reason = FinishReason::parse(fr);
            self.finish_reason = Some(reason.clone());
            out.push(UnifiedDelta::Finish(reason));
        }

        Ok(out)
    }

    fn finish(&mut self) -> Vec<UnifiedDelta> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![UnifiedDelta::Finish(
            self.finish_reason.clone().unwrap_or(FinishReason::Stop),
        )]
    }

    fn usage(&self) -> UnifiedUsage {
        let mut u = self.usage.clone();
        u.source = UsageSource::Upstream;
        u
    }

    fn text(&self) -> String {
        self.text.clone()
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

// ---------------------------------------------------------------------------
// 编码器
// ---------------------------------------------------------------------------

pub struct ChatStreamEncoder {
    id: String,
    model: String,
    started: bool,
    role_sent: bool,
    finished: bool,
    /// IR 的 block index → OpenAI 的 tool_call index。
    tool_index: HashMap<u32, u32>,
    next_tool_index: u32,
    /// 已开启过的非工具块（用于避免重复发内容）。
    open_block: Option<u32>,
    usage: UnifiedUsage,
    finish_reason: Option<FinishReason>,
}

impl ChatStreamEncoder {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            started: false,
            role_sent: false,
            finished: false,
            tool_index: HashMap::new(),
            next_tool_index: 0,
            open_block: None,
            usage: UnifiedUsage::default(),
            finish_reason: None,
        }
    }

    fn chunk(&self, delta: Value, finish_reason: Value) -> SseEvent {
        let payload = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": crate::util::now_secs(),
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        });
        SseEvent::data(payload.to_string())
    }

    fn ensure_started(&mut self, out: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = format!("chatcmpl-{}", Uuid::new_v4().simple());
        }
        // 首个 chunk 只带 role，符合 OpenAI 的习惯。
        out.push(self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null));
        self.role_sent = true;
    }

    fn tool_idx(&mut self, block_index: u32) -> u32 {
        if let Some(i) = self.tool_index.get(&block_index) {
            return *i;
        }
        let i = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_index.insert(block_index, i);
        i
    }
}

impl Default for ChatStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder for ChatStreamEncoder {
    fn on_delta(&mut self, d: &UnifiedDelta) -> Vec<SseEvent> {
        let mut out = Vec::new();

        match d {
            UnifiedDelta::MessageStart { id, model } => {
                self.id = id.clone();
                self.model = model.clone();
                self.ensure_started(&mut out);
            }

            UnifiedDelta::BlockStart { index, block } => {
                self.ensure_started(&mut out);
                self.open_block = Some(*index);
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    let ti = self.tool_idx(*index);
                    out.push(self.chunk(
                        json!({ "tool_calls": [{
                            "index": ti,
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": "" },
                        }]}),
                        Value::Null,
                    ));
                }
            }

            UnifiedDelta::TextDelta { text, .. } => {
                self.ensure_started(&mut out);
                if !text.is_empty() {
                    out.push(self.chunk(json!({ "content": text }), Value::Null));
                }
            }

            UnifiedDelta::ThinkingDelta { text, .. } => {
                self.ensure_started(&mut out);
                if !text.is_empty() {
                    out.push(self.chunk(json!({ "reasoning_content": text }), Value::Null));
                }
            }

            UnifiedDelta::ToolInputDelta {
                index,
                partial_json,
            } => {
                self.ensure_started(&mut out);
                // 上游可能没发过 BlockStart，这里补一个带占位的开头，
                // 否则下游拿不到 tool_call 的 id。
                let ti = match self.tool_index.get(index) {
                    Some(i) => *i,
                    None => {
                        let ti = self.tool_idx(*index);
                        out.push(self.chunk(
                            json!({ "tool_calls": [{
                                "index": ti,
                                "id": format!("call_{ti}"),
                                "type": "function",
                                "function": { "name": "", "arguments": "" },
                            }]}),
                            Value::Null,
                        ));
                        ti
                    }
                };
                if !partial_json.is_empty() {
                    out.push(self.chunk(
                        json!({ "tool_calls": [{
                            "index": ti,
                            "function": { "arguments": partial_json },
                        }]}),
                        Value::Null,
                    ));
                }
            }

            UnifiedDelta::BlockStop { .. } => {}

            UnifiedDelta::Usage(u) => u.apply(&mut self.usage),

            UnifiedDelta::Finish(reason) => {
                self.finish_reason = Some(reason.clone());
            }

            UnifiedDelta::Error { code, message } => {
                self.ensure_started(&mut out);
                out.push(SseEvent::data(
                    json!({
                        "error": { "type": code, "message": message }
                    })
                    .to_string(),
                ));
            }
        }

        out
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut out = Vec::new();
        self.ensure_started(&mut out);

        let reason = self
            .finish_reason
            .clone()
            .unwrap_or(FinishReason::Stop)
            .to_wire(P)
            .to_string();

        // 带 finish_reason 的空 delta chunk
        out.push(self.chunk(json!({}), json!(reason)));

        // 独立 usage chunk：choices 为空数组，这是 OpenAI 的约定。
        let prompt_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.cache_read_tokens)
            .saturating_add(self.usage.cache_creation_tokens);
        out.push(SseEvent::data(
            json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": crate::util::now_secs(),
                "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": self.usage.output_tokens,
                    "total_tokens": prompt_tokens.saturating_add(self.usage.output_tokens),
                },
            })
            .to_string(),
        ));

        out.push(crate::gateway::sse::done_event());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(dec: &mut ChatStreamDecoder, events: &[SseEvent]) -> Vec<UnifiedDelta> {
        let mut out = Vec::new();
        for e in events {
            out.extend(dec.on_event(e).expect("解码不应失败"));
        }
        out.extend(dec.finish());
        out
    }

    fn text_of(deltas: &[UnifiedDelta]) -> String {
        deltas
            .iter()
            .filter_map(|d| match d {
                UnifiedDelta::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn decodes_simple_stream() {
        let mut dec = ChatStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                SseEvent::data(
                    r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":null}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c1","model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                ),
                crate::gateway::sse::done_event(),
            ],
        );

        assert_eq!(text_of(&deltas), "Hello");
        assert_eq!(dec.text(), "Hello");
        assert!(dec.is_finished());
        assert!(matches!(deltas[0], UnifiedDelta::MessageStart { .. }));
    }

    #[test]
    fn usage_chunk_updates_totals_and_subtracts_cache() {
        let mut dec = ChatStreamDecoder::new();
        decode_all(
            &mut dec,
            &[
                SseEvent::data(
                    r#"{"id":"c","model":"m","choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c","model":"m","choices":[],"usage":{"prompt_tokens":500,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":400}}}"#,
                ),
                crate::gateway::sse::done_event(),
            ],
        );

        let u = dec.usage();
        assert_eq!(u.input_tokens, 100, "500-400");
        assert_eq!(u.cache_read_tokens, 400);
        assert_eq!(u.output_tokens, 10);
    }

    #[test]
    fn reasoning_content_streams_as_thinking() {
        let mut dec = ChatStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                SseEvent::data(
                    r#"{"id":"c","model":"r1","choices":[{"delta":{"reasoning_content":"hmm"},"finish_reason":null}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c","model":"r1","choices":[{"delta":{"content":"ans"},"finish_reason":null}]}"#,
                ),
            ],
        );

        assert!(deltas
            .iter()
            .any(|d| matches!(d, UnifiedDelta::ThinkingDelta { .. })));
        assert_eq!(text_of(&deltas), "ans");
    }

    #[test]
    fn done_marker_terminates() {
        let mut dec = ChatStreamDecoder::new();
        let out = dec
            .on_event(&crate::gateway::sse::done_event())
            .unwrap();
        assert!(matches!(out[0], UnifiedDelta::Finish(_)));
        assert!(dec.is_finished());
    }

    // --- 编码器 ---

    fn encode_all(enc: &mut ChatStreamEncoder, deltas: &[UnifiedDelta]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for d in deltas {
            out.extend(enc.on_delta(d));
        }
        out.extend(enc.finish());
        out
    }

    #[test]
    fn encoder_produces_role_then_content_then_done() {
        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(&mut enc, &[UnifiedDelta::text(IDX_TEXT, "hi")]);

        assert!(events.len() >= 3);
        // 第一个 chunk 带 role
        assert!(events[0].data.contains("\"role\":\"assistant\""));
        // 第二个带内容
        assert!(events[1].data.contains("\"content\":\"hi\""));
        // 最后是 [DONE]
        assert!(events.last().unwrap().is_done());
    }

    #[test]
    fn encoder_emits_finish_reason_chunk() {
        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(IDX_TEXT, "x"),
                UnifiedDelta::Finish(FinishReason::ToolUse),
            ],
        );

        let fr = events
            .iter()
            .filter(|e| !e.is_done())
            .filter_map(|e| e.json().ok())
            .find_map(|v| {
                v.get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("finish_reason"))
                    .filter(|f| !f.is_null())
                    .and_then(|f| f.as_str())
                    .map(String::from)
            });
        assert_eq!(fr.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn encoder_emits_usage_chunk_before_done() {
        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(IDX_TEXT, "x"),
                UnifiedDelta::Usage(crate::protocol::dto::UsageDelta {
                    input_tokens: Some(100),
                    output_tokens: Some(5),
                    cache_read_tokens: Some(900),
                    ..Default::default()
                }),
            ],
        );

        let usage_chunk = events
            .iter()
            .filter(|e| !e.is_done())
            .filter_map(|e| e.json().ok())
            .find(|v| v.get("usage").is_some())
            .expect("应当有一个 usage chunk");

        // prompt_tokens 必须含缓存（OpenAI 口径）
        assert_eq!(usage_chunk["usage"]["prompt_tokens"], 1000);
        assert_eq!(usage_chunk["usage"]["completion_tokens"], 5);
        assert_eq!(usage_chunk["choices"].as_array().unwrap().len(), 0);

        assert!(events.last().unwrap().is_done());
    }

    #[test]
    fn encoder_numbers_tool_calls_sequentially_from_zero() {
        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::BlockStart {
                    index: 5,
                    block: ContentBlock::ToolUse {
                        id: "call_a".into(),
                        name: "f1".into(),
                        input: json!({}),
                    },
                },
                UnifiedDelta::BlockStart {
                    index: 9,
                    block: ContentBlock::ToolUse {
                        id: "call_b".into(),
                        name: "f2".into(),
                        input: json!({}),
                    },
                },
            ],
        );

        let indices: Vec<u64> = events
            .iter()
            .filter(|e| !e.is_done())
            .filter_map(|e| e.json().ok())
            .filter_map(|v| {
                v.get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(|t| t.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("index"))
                    .and_then(|i| i.as_u64())
            })
            .collect();

        assert_eq!(indices, vec![0, 1], "工具调用序号必须从 0 连续递增");
    }

    #[test]
    fn encoder_synthesizes_tool_call_header_for_orphan_arguments() {
        // 上游只给了 arguments 分片，没有 BlockStart：必须补出 id，
        // 否则下游无法把结果关联回调用。
        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[UnifiedDelta::ToolInputDelta {
                index: 0,
                partial_json: "{}".into(),
            }],
        );

        let has_id = events
            .iter()
            .filter(|e| !e.is_done())
            .filter_map(|e| e.json().ok())
            .any(|v| v.to_string().contains("\"id\":\"call_0\""));
        assert!(has_id, "应当合成 tool_call id");
    }

    #[test]
    fn decoder_then_encoder_roundtrip() {
        let mut dec = ChatStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                SseEvent::data(
                    r#"{"id":"c","model":"gpt-4o","choices":[{"delta":{"content":"你好"},"finish_reason":null}]}"#,
                ),
                SseEvent::data(
                    r#"{"id":"c","model":"gpt-4o","choices":[{"delta":{"content":"世界"},"finish_reason":"stop"}]}"#,
                ),
                crate::gateway::sse::done_event(),
            ],
        );

        let mut enc = ChatStreamEncoder::new();
        let events = encode_all(&mut enc, &deltas);

        let text: String = events
            .iter()
            .filter(|e| !e.is_done())
            .filter_map(|e| e.json().ok())
            .filter_map(|v| {
                v.get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                    .map(String::from)
            })
            .collect();

        assert_eq!(text, "你好世界");
        assert!(events.last().unwrap().is_done());
    }
}
