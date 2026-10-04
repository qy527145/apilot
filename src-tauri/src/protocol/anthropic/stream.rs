//! Anthropic 流式编解码。
//!
//! 解码：上游 SSE 事件 → IR delta（供计费、监控与跨协议转换消费）。
//! 编码：IR delta → 客户端可读的 Anthropic SSE 序列（用于缓存命中重放与跨协议转换）。

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::gateway::sse::SseEvent;
use crate::protocol::codec::{ConvertError, StreamDecoder, StreamEncoder};
use crate::protocol::dto::{
    ContentBlock, FinishReason, Protocol, UnifiedDelta, UnifiedUsage, UsageDelta, UsageSource,
};

use super::response::decode_usage;

const P: Protocol = Protocol::AnthropicMessages;

/// content block 的类型，决定后续 delta 事件如何解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    Other,
}

// ---------------------------------------------------------------------------
// 解码器：上游 SSE → IR
// ---------------------------------------------------------------------------

pub struct AnthropicStreamDecoder {
    blocks: HashMap<u32, BlockKind>,
    usage: UnifiedUsage,
    text: String,
    thinking: String,
    finish_reason: Option<FinishReason>,
    finished: bool,
    /// 工具入参的分片 JSON，按 block index 累积。
    tool_input: HashMap<u32, String>,
}

impl AnthropicStreamDecoder {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            usage: UnifiedUsage::default(),
            text: String::new(),
            thinking: String::new(),
            finish_reason: None,
            finished: false,
            tool_input: HashMap::new(),
        }
    }

    fn on_message_start(&mut self, v: &Value) -> Vec<UnifiedDelta> {
        let msg = v.get("message").unwrap_or(&Value::Null);
        let id = msg
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("msg_unknown")
            .to_string();
        let model = msg
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();

        // input_tokens 与缓存字段在 message_start 就给出了。
        let mut u = decode_usage(msg.get("usage"));
        u.source = UsageSource::Upstream;
        self.usage.merge_non_zero(&u);

        let mut out = vec![UnifiedDelta::MessageStart {
            id,
            model: model.clone(),
        }];

        // 有些上游会在 message_start 里就给出完整的 content（非标准但存在）。
        if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
            for (i, block) in content.iter().enumerate() {
                let idx = i as u32;
                if let Ok(b) = serde_json::from_value::<ContentBlock>(block.clone()) {
                    out.push(UnifiedDelta::BlockStart { index: idx, block: b });
                }
            }
        }

        out
    }

    fn on_block_start(&mut self, v: &Value) -> Vec<UnifiedDelta> {
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
        let cb = v.get("content_block").unwrap_or(&Value::Null);
        let ty = cb.get("type").and_then(|t| t.as_str()).unwrap_or("text");

        let kind = match ty {
            "text" => BlockKind::Text,
            "thinking" => BlockKind::Thinking,
            "tool_use" => BlockKind::ToolUse,
            other => {
                tracing::debug!(block_type = other, "未建模的 content block 类型");
                BlockKind::Other
            }
        };
        self.blocks.insert(index, kind);
        if kind == BlockKind::ToolUse {
            self.tool_input.entry(index).or_default();
        }

        let block = match kind {
            BlockKind::Text => ContentBlock::Text {
                text: cb
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
            BlockKind::Thinking => ContentBlock::Thinking {
                text: cb
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string(),
                signature: None,
            },
            BlockKind::ToolUse => ContentBlock::ToolUse {
                id: cb
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string(),
                name: cb
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string(),
                input: cb.get("input").cloned().unwrap_or_else(|| json!({})),
            },
            BlockKind::Other => ContentBlock::Text {
                text: String::new(),
            },
        };

        vec![UnifiedDelta::BlockStart { index, block }]
    }

    fn on_block_delta(&mut self, v: &Value) -> Vec<UnifiedDelta> {
        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
        let delta = v.get("delta").unwrap_or(&Value::Null);

        match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text_delta" => {
                let text = delta
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                self.text.push_str(&text);
                vec![UnifiedDelta::TextDelta { index, text }]
            }
            "thinking_delta" => {
                let text = delta
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                self.thinking.push_str(&text);
                vec![UnifiedDelta::ThinkingDelta { index, text }]
            }
            "signature_delta" => {
                // 签名本身对计费与文本无意义，但转换到同协议时必须原样回传。
                // 这里记录到 thinking 累积器的尾部由 encoder 处理；解码侧暂不产出 delta。
                Vec::new()
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                self.tool_input.entry(index).or_default().push_str(&partial);
                vec![UnifiedDelta::ToolInputDelta {
                    index,
                    partial_json: partial,
                }]
            }
            other => {
                tracing::debug!(delta_type = other, "未建模的 delta 类型");
                Vec::new()
            }
        }
    }

    fn on_message_delta(&mut self, v: &Value) -> Vec<UnifiedDelta> {
        let mut out = Vec::new();

        if let Some(u) = v.get("usage") {
            let parsed = decode_usage(Some(u));
            // message_delta 通常只带 output_tokens，input 侧的零值不能覆盖已累积的值。
            let delta = UsageDelta {
                input_tokens: (parsed.input_tokens > 0).then_some(parsed.input_tokens),
                output_tokens: (parsed.output_tokens > 0).then_some(parsed.output_tokens),
                cache_read_tokens: (parsed.cache_read_tokens > 0)
                    .then_some(parsed.cache_read_tokens),
                cache_creation_tokens: (parsed.cache_creation_tokens > 0)
                    .then_some(parsed.cache_creation_tokens),
                reasoning_tokens: None,
                total_tokens: None,
            };
            self.usage.merge_non_zero(&parsed);
            out.push(UnifiedDelta::Usage(delta));
        }

        if let Some(stop) = v
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(|s| s.as_str())
        {
            let reason = FinishReason::parse(stop);
            self.finish_reason = Some(reason.clone());
            out.push(UnifiedDelta::Finish(reason));
        }

        out
    }

    fn on_error(&mut self, v: &Value) -> Vec<UnifiedDelta> {
        self.finished = true;
        let err = v.get("error").unwrap_or(v);
        vec![UnifiedDelta::Error {
            code: err
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("error")
                .to_string(),
            message: err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("上游返回未知错误")
                .to_string(),
        }]
    }
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder for AnthropicStreamDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<UnifiedDelta>, ConvertError> {
        // 事件名优先取 event: 字段，缺失时看 data 里的 type。
        let name = ev
            .resolved_name()
            .unwrap_or_else(|| "message_delta".to_string());

        let out = match name.as_str() {
            "message_start" => self.on_message_start(&ev.json()?),
            "content_block_start" => self.on_block_start(&ev.json()?),
            "content_block_delta" => self.on_block_delta(&ev.json()?),
            "content_block_stop" => {
                let index = ev
                    .json()
                    .ok()
                    .and_then(|v| v.get("index").and_then(|i| i.as_u64()))
                    .unwrap_or(0) as u32;
                vec![UnifiedDelta::BlockStop { index }]
            }
            "message_delta" => self.on_message_delta(&ev.json()?),
            "message_stop" => {
                self.finished = true;
                let reason = self.finish_reason.clone().unwrap_or(FinishReason::Stop);
                vec![UnifiedDelta::Finish(reason)]
            }
            "ping" => Vec::new(),
            "error" => self.on_error(&ev.json()?),
            other => {
                tracing::debug!(event = other, "未识别的 Anthropic 流事件");
                Vec::new()
            }
        };

        Ok(out)
    }

    fn finish(&mut self) -> Vec<UnifiedDelta> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        // 上游断了但没发 message_stop：补一个终止，保证下游不会挂住。
        vec![UnifiedDelta::Finish(
            self.finish_reason.clone().unwrap_or(FinishReason::Stop),
        )]
    }

    fn usage(&self) -> UnifiedUsage {
        let mut u = self.usage.clone();
        // 上游若没报 output_tokens，用已累积的文本粗略兜底由上层决定；
        // 这里不擅自填数，保持 source=Upstream 的可信度。
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
// 编码器：IR → 客户端 SSE
// ---------------------------------------------------------------------------

/// 把 IR delta 编成客户端可读的 Anthropic SSE 序列。
///
/// 关键约束：Anthropic 要求 `message_start` 在最前、`content_block_start` 先于
/// 同 index 的任何 delta、`content_block_stop` 后不能再发该 index 的 delta。
/// 上游换协议时这些顺序未必成立，所以由本编码器统一补齐。
pub struct AnthropicStreamEncoder {
    started: bool,
    id: String,
    model: String,
    /// 当前处于开启状态的 block index。
    open_block: Option<u32>,
    /// 下一个新 block 要用的 index。
    next_index: u32,
    finished: bool,
    usage: UnifiedUsage,
    finish_reason: Option<FinishReason>,
    /// tool_use block 已发过 content_block_start，input 走 input_json_delta。
    tool_blocks: HashMap<u32, bool>,
}

impl AnthropicStreamEncoder {
    pub fn new() -> Self {
        Self {
            started: false,
            id: String::new(),
            model: String::new(),
            open_block: None,
            next_index: 0,
            finished: false,
            usage: UnifiedUsage::default(),
            finish_reason: None,
            tool_blocks: HashMap::new(),
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = format!("msg_{}", Uuid::new_v4().simple());
        }
        let payload = json!({
            "type": "message_start",
            "message": {
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "model": self.model,
                "content": [],
                "stop_reason": Value::Null,
                "stop_sequence": Value::Null,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            }
        });
        out.push(SseEvent::named("message_start", payload.to_string()));
    }

    fn close_open_block(&mut self, out: &mut Vec<SseEvent>) {
        if let Some(idx) = self.open_block.take() {
            let payload = json!({ "type": "content_block_stop", "index": idx });
            out.push(SseEvent::named("content_block_stop", payload.to_string()));
        }
    }

    /// 确保 index 对应的 block 已开启；未开启则补一个空的 start 事件。
    fn ensure_block_open(&mut self, index: u32, out: &mut Vec<SseEvent>) {
        if self.open_block == Some(index) {
            return;
        }
        self.close_open_block(out);

        self.tool_blocks.entry(index).or_insert(false);
        let payload = json!({
            "type": "content_block_start",
            "index": index,
            "content_block": { "type": "text", "text": "" }
        });
        out.push(SseEvent::named(
            "content_block_start",
            payload.to_string(),
        ));
        self.open_block = Some(index);
        if index >= self.next_index {
            self.next_index = index + 1;
        }
    }
}

impl Default for AnthropicStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder for AnthropicStreamEncoder {
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
                self.close_open_block(&mut out);

                let content_block = match block {
                    ContentBlock::Text { .. } => json!({ "type": "text", "text": "" }),
                    ContentBlock::Thinking { .. } => {
                        json!({ "type": "thinking", "thinking": "" })
                    }
                    ContentBlock::ToolUse { id, name, .. } => {
                        self.tool_blocks.insert(*index, true);
                        json!({ "type": "tool_use", "id": id, "name": name, "input": {} })
                    }
                    // Anthropic 没有对应的空块形态，退化成文本块。
                    _ => json!({ "type": "text", "text": "" }),
                };

                out.push(SseEvent::named(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": content_block,
                    })
                    .to_string(),
                ));
                self.open_block = Some(*index);
                if *index >= self.next_index {
                    self.next_index = index + 1;
                }
            }

            UnifiedDelta::TextDelta { index, text } => {
                self.ensure_started(&mut out);
                self.ensure_block_open(*index, &mut out);
                out.push(SseEvent::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": text },
                    })
                    .to_string(),
                ));
            }

            UnifiedDelta::ThinkingDelta { index, text } => {
                self.ensure_started(&mut out);
                self.ensure_block_open(*index, &mut out);
                out.push(SseEvent::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "thinking_delta", "thinking": text },
                    })
                    .to_string(),
                ));
            }

            UnifiedDelta::ToolInputDelta { index, partial_json } => {
                self.ensure_started(&mut out);
                self.ensure_block_open(*index, &mut out);
                out.push(SseEvent::named(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": partial_json },
                    })
                    .to_string(),
                ));
            }

            UnifiedDelta::BlockStop { index } => {
                self.ensure_started(&mut out);
                if self.open_block == Some(*index) {
                    self.close_open_block(&mut out);
                }
            }

            UnifiedDelta::Usage(u) => {
                u.apply(&mut self.usage);
            }

            UnifiedDelta::Finish(reason) => {
                self.finish_reason = Some(reason.clone());
            }

            UnifiedDelta::Error { code, message } => {
                self.ensure_started(&mut out);
                out.push(SseEvent::named(
                    "error",
                    json!({
                        "type": "error",
                        "error": { "type": code, "message": message },
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
        self.close_open_block(&mut out);

        let reason = self
            .finish_reason
            .clone()
            .unwrap_or(FinishReason::Stop)
            .to_wire(P)
            .to_string();

        let mut usage = json!({
            "output_tokens": self.usage.output_tokens,
        });
        // input 侧的字段在 message_start 已给过，这里只补 output 以贴合 Anthropic 语义。
        if self.usage.cache_read_tokens > 0 {
            usage["cache_read_input_tokens"] = json!(self.usage.cache_read_tokens);
        }

        out.push(SseEvent::named(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": reason, "stop_sequence": Value::Null },
                "usage": usage,
            })
            .to_string(),
        ));

        out.push(SseEvent::named(
            "message_stop",
            json!({ "type": "message_stop" }).to_string(),
        ));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(dec: &mut AnthropicStreamDecoder, events: &[SseEvent]) -> Vec<UnifiedDelta> {
        let mut out = Vec::new();
        for e in events {
            out.extend(dec.on_event(e).expect("解码不应失败"));
        }
        out.extend(dec.finish());
        out
    }

    fn text_delta<'a>(deltas: &'a [UnifiedDelta]) -> String {
        deltas
            .iter()
            .filter_map(|d| match d {
                UnifiedDelta::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn decodes_a_simple_text_stream() {
        let mut dec = AnthropicStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                SseEvent::named(
                    "message_start",
                    r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-5","usage":{"input_tokens":25,"output_tokens":0}}}"#,
                ),
                SseEvent::named(
                    "content_block_start",
                    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                ),
                SseEvent::named(
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                ),
                SseEvent::named(
                    "content_block_delta",
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                ),
                SseEvent::named("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
                SseEvent::named(
                    "message_delta",
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
                ),
                SseEvent::named("message_stop", r#"{"type":"message_stop"}"#),
            ],
        );

        assert_eq!(text_delta(&deltas), "Hello");
        assert_eq!(dec.text(), "Hello");

        let usage = dec.usage();
        assert_eq!(usage.input_tokens, 25, "input 来自 message_start");
        assert_eq!(usage.output_tokens, 7, "output 来自 message_delta");
        assert!(dec.is_finished());
    }

    #[test]
    fn message_delta_does_not_clobber_input_tokens() {
        // message_delta 的 usage 只有 output_tokens，零值不得覆盖已拿到的 input。
        let mut dec = AnthropicStreamDecoder::new();
        decode_all(
            &mut dec,
            &[
                SseEvent::named(
                    "message_start",
                    r#"{"message":{"id":"m","model":"x","usage":{"input_tokens":100,"cache_read_input_tokens":900}}}"#,
                ),
                SseEvent::named(
                    "message_delta",
                    r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
                ),
            ],
        );

        let u = dec.usage();
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.cache_read_tokens, 900);
        assert_eq!(u.output_tokens, 5);
    }

    #[test]
    fn ping_events_are_ignored() {
        let mut dec = AnthropicStreamDecoder::new();
        let out = dec
            .on_event(&SseEvent::named("ping", r#"{"type":"ping"}"#))
            .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn error_event_surfaces_as_error_delta() {
        let mut dec = AnthropicStreamDecoder::new();
        let out = dec
            .on_event(&SseEvent::named(
                "error",
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
            ))
            .unwrap();
        match &out[0] {
            UnifiedDelta::Error { code, message } => {
                assert_eq!(code, "overloaded_error");
                assert_eq!(message, "busy");
            }
            other => panic!("期望 Error，得到 {other:?}"),
        }
    }

    #[test]
    fn finish_is_idempotent_and_synthesizes_a_terminator() {
        let mut dec = AnthropicStreamDecoder::new();
        // 模拟上游中途断开：没有任何终止事件
        dec.on_event(&SseEvent::named(
            "message_start",
            r#"{"message":{"id":"m","model":"x","usage":{"input_tokens":1}}}"#,
        ))
        .unwrap();

        let first = dec.finish();
        assert_eq!(first.len(), 1, "应当补一个 Finish");
        assert!(matches!(first[0], UnifiedDelta::Finish(_)));

        assert!(dec.finish().is_empty(), "重复调用不应再产出事件");
    }

    // --- 编码器 ---

    fn encode_all(enc: &mut AnthropicStreamEncoder, deltas: &[UnifiedDelta]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for d in deltas {
            out.extend(enc.on_delta(d));
        }
        out.extend(enc.finish());
        out
    }

    fn names(events: &[SseEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| e.event.clone().unwrap_or_default())
            .collect()
    }

    #[test]
    fn encoder_emits_well_formed_sequence() {
        let mut enc = AnthropicStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::MessageStart {
                    id: "msg_x".into(),
                    model: "claude-sonnet-5".into(),
                },
                UnifiedDelta::BlockStart {
                    index: 0,
                    block: ContentBlock::text(""),
                },
                UnifiedDelta::text(0, "Hi"),
                UnifiedDelta::BlockStop { index: 0 },
                UnifiedDelta::Usage(UsageDelta {
                    output_tokens: Some(2),
                    ..Default::default()
                }),
                UnifiedDelta::Finish(FinishReason::Stop),
            ],
        );

        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        // message_start 必须最先
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        // 终止必须是 message_stop
        assert_eq!(
            events.last().unwrap().event.as_deref(),
            Some("message_stop")
        );
        // message_delta 带正确的 stop_reason
        assert!(events[4].data.contains("end_turn"));
        assert!(events[4].data.contains("\"output_tokens\":2"));
    }

    #[test]
    fn encoder_auto_opens_block_for_orphan_text_delta() {
        // 上游（例如 OpenAI）可能不显式发 BlockStart，编码器必须补齐。
        let mut enc = AnthropicStreamEncoder::new();
        let events = encode_all(&mut enc, &[UnifiedDelta::text(0, "orphan")]);

        assert_eq!(
            names(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn encoder_closes_previous_block_when_a_new_one_starts() {
        let mut enc = AnthropicStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(0, "a"),
                UnifiedDelta::text(1, "b"),
            ],
        );

        let n = names(&events);
        // start(0) delta stop(0) start(1) delta stop(1) message_delta message_stop
        assert_eq!(
            n,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
    }

    #[test]
    fn encoder_emits_tool_use_block_start_with_id_and_name() {
        let mut enc = AnthropicStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::BlockStart {
                    index: 0,
                    block: ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "search".into(),
                        input: json!({}),
                    },
                },
                UnifiedDelta::ToolInputDelta {
                    index: 0,
                    partial_json: r#"{"q":"x"}"#.into(),
                },
            ],
        );

        let start = &events[1];
        assert!(start.data.contains("\"type\":\"tool_use\""));
        assert!(start.data.contains("\"id\":\"tu_1\""));
        assert!(start.data.contains("\"name\":\"search\""));

        let delta = &events[2];
        assert!(delta.data.contains("input_json_delta"));
    }

    #[test]
    fn encoder_finish_is_idempotent() {
        let mut enc = AnthropicStreamEncoder::new();
        enc.on_delta(&UnifiedDelta::text(0, "x"));
        let first = enc.finish();
        assert!(!first.is_empty());
        assert!(enc.finish().is_empty());
    }

    #[test]
    fn encoder_never_emits_two_message_starts() {
        let mut enc = AnthropicStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(0, "a"),
                UnifiedDelta::MessageStart {
                    id: "late".into(),
                    model: "m".into(),
                },
                UnifiedDelta::text(0, "b"),
            ],
        );
        let starts = events
            .iter()
            .filter(|e| e.event.as_deref() == Some("message_start"))
            .count();
        assert_eq!(starts, 1);
    }

    /// 端到端：解码一段上游流，再用编码器重新编码，文本必须一致。
    #[test]
    fn decode_then_encode_roundtrip_preserves_text() {
        let mut dec = AnthropicStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                SseEvent::named(
                    "message_start",
                    r#"{"message":{"id":"msg_1","model":"claude-sonnet-5","usage":{"input_tokens":3}}}"#,
                ),
                SseEvent::named(
                    "content_block_delta",
                    r#"{"index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
                ),
                SseEvent::named(
                    "content_block_delta",
                    r#"{"index":0,"delta":{"type":"text_delta","text":"世界"}}"#,
                ),
                SseEvent::named(
                    "message_delta",
                    r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
                ),
                SseEvent::named("message_stop", r#"{"type":"message_stop"}"#),
            ],
        );

        let mut enc = AnthropicStreamEncoder::new();
        let reencoded = encode_all(&mut enc, &deltas);

        let text: String = reencoded
            .iter()
            .filter(|e| e.event.as_deref() == Some("content_block_delta"))
            .filter_map(|e| e.json().ok())
            .filter_map(|v| {
                v.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .map(String::from)
            })
            .collect();

        assert_eq!(text, "你好世界");
        assert_eq!(
            reencoded.last().unwrap().event.as_deref(),
            Some("message_stop")
        );
    }
}
