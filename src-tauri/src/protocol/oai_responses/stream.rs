//! OpenAI Responses 流式编解码。

use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::gateway::sse::SseEvent;
use crate::protocol::codec::{ConvertError, StreamDecoder, StreamEncoder};
use crate::protocol::dto::{
    ContentBlock, FinishReason, UnifiedDelta, UnifiedUsage, UsageDelta, UsageSource,
};

use super::decode_usage;

/// Responses 的流式事件全都带 `event:` 行，同时 data 里也有 `type`。

pub struct ResponsesStreamDecoder {
    usage: UnifiedUsage,
    text: String,
    thinking: String,
    /// output_index → 该 item 是否为 function_call。
    item_is_tool: HashMap<u64, bool>,
    /// item_id → output_index，arguments 事件用的是 item_id。
    id_to_index: HashMap<String, u64>,
    tool_meta: HashMap<u64, (String, String)>,
    tool_args: HashMap<u64, String>,
    finish_reason: Option<FinishReason>,
    finished: bool,
    started: bool,
    id: String,
    model: String,
}

impl ResponsesStreamDecoder {
    pub fn new() -> Self {
        Self {
            usage: UnifiedUsage::default(),
            text: String::new(),
            thinking: String::new(),
            item_is_tool: HashMap::new(),
            id_to_index: HashMap::new(),
            tool_meta: HashMap::new(),
            tool_args: HashMap::new(),
            finish_reason: None,
            finished: false,
            started: false,
            id: String::new(),
            model: String::new(),
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<UnifiedDelta>) {
        if self.started {
            return;
        }
        self.started = true;
        out.push(UnifiedDelta::MessageStart {
            id: self.id.clone(),
            model: self.model.clone(),
        });
    }
}

impl Default for ResponsesStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder for ResponsesStreamDecoder {
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<UnifiedDelta>, ConvertError> {
        // Responses 没有 [DONE]，终止靠 response.completed。
        if ev.is_done() {
            self.finished = true;
            let reason = self.finish_reason.clone().unwrap_or(FinishReason::Stop);
            return Ok(vec![UnifiedDelta::Finish(reason)]);
        }

        let v = ev.json()?;
        let name = v
            .get("type")
            .and_then(|t| t.as_str())
            .map(String::from)
            .or_else(|| ev.event.clone())
            .unwrap_or_default();

        let mut out = Vec::new();

        match name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(r) = v.get("response") {
                    self.id = r
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string();
                    self.model = r
                        .get("model")
                        .and_then(|m| m.as_str())
                        .unwrap_or_default()
                        .to_string();
                }
                self.ensure_started(&mut out);
            }

            "response.output_item.added" => {
                self.ensure_started(&mut out);
                let output_index = v.get("output_index").and_then(|i| i.as_u64()).unwrap_or(0);
                let item = v.get("item").unwrap_or(&Value::Null);
                let item_id = item
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !item_id.is_empty() {
                    self.id_to_index.insert(item_id, output_index);
                }

                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    self.item_is_tool.insert(output_index, true);
                    let meta = (
                        item.get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        item.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    self.tool_meta.insert(output_index, meta.clone());
                    self.tool_args.entry(output_index).or_default();
                    out.push(UnifiedDelta::BlockStart {
                        index: output_index as u32,
                        block: ContentBlock::ToolUse {
                            id: meta.0,
                            name: meta.1,
                            input: json!({}),
                        },
                    });
                }
            }

            "response.output_text.delta" => {
                self.ensure_started(&mut out);
                let output_index = v.get("output_index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = v.get("delta").and_then(|d| d.as_str()).unwrap_or_default();
                if !delta.is_empty() {
                    self.text.push_str(delta);
                    out.push(UnifiedDelta::TextDelta {
                        index: output_index as u32,
                        text: delta.to_string(),
                    });
                }
            }

            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_started(&mut out);
                let output_index = v.get("output_index").and_then(|i| i.as_u64()).unwrap_or(0);
                let delta = v.get("delta").and_then(|d| d.as_str()).unwrap_or_default();
                if !delta.is_empty() {
                    self.thinking.push_str(delta);
                    out.push(UnifiedDelta::ThinkingDelta {
                        index: output_index as u32,
                        text: delta.to_string(),
                    });
                }
            }

            "response.function_call_arguments.delta" => {
                self.ensure_started(&mut out);
                let output_index = v
                    .get("output_index")
                    .and_then(|i| i.as_u64())
                    .or_else(|| {
                        v.get("item_id")
                            .and_then(|i| i.as_str())
                            .and_then(|id| self.id_to_index.get(id).copied())
                    })
                    .unwrap_or(0);
                let delta = v.get("delta").and_then(|d| d.as_str()).unwrap_or_default();
                if !delta.is_empty() {
                    self.tool_args
                        .entry(output_index)
                        .or_default()
                        .push_str(delta);
                    out.push(UnifiedDelta::ToolInputDelta {
                        index: output_index as u32,
                        partial_json: delta.to_string(),
                    });
                }
            }

            "response.output_item.done" => {
                // 工具调用结束时 arguments 一定已收齐，可用完整 item 校准一次。
                let output_index = v.get("output_index").and_then(|i| i.as_u64()).unwrap_or(0);
                let item = v.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    if let Some(args) = item.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            self.tool_args.insert(output_index, args.to_string());
                        }
                    }
                    let meta = (
                        item.get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(|i| i.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        item.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    );
                    self.tool_meta.insert(output_index, meta);
                }
                out.push(UnifiedDelta::BlockStop {
                    index: output_index as u32,
                });
            }

            "response.completed" | "response.incomplete" => {
                self.ensure_started(&mut out);
                if let Some(r) = v.get("response") {
                    let parsed = decode_usage(r.get("usage"));
                    self.usage.merge_non_zero(&parsed);
                    out.push(UnifiedDelta::Usage(UsageDelta {
                        input_tokens: Some(parsed.input_tokens),
                        output_tokens: Some(parsed.output_tokens),
                        cache_read_tokens: Some(parsed.cache_read_tokens),
                        cache_creation_tokens: None,
                        reasoning_tokens: Some(parsed.reasoning_tokens),
                        total_tokens: Some(parsed.total_tokens),
                    }));

                    let has_tool = !self.item_is_tool.is_empty();
                    let reason = match r.get("status").and_then(|s| s.as_str()) {
                        Some("incomplete") => FinishReason::Length,
                        _ if has_tool => FinishReason::ToolUse,
                        _ => FinishReason::Stop,
                    };
                    self.finish_reason = Some(reason.clone());
                    out.push(UnifiedDelta::Finish(reason));
                }
                self.finished = true;
            }

            "response.failed" => {
                self.finished = true;
                let msg = v
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("响应失败");
                out.push(UnifiedDelta::Error {
                    code: "response_failed".into(),
                    message: msg.to_string(),
                });
            }

            "error" => {
                self.finished = true;
                out.push(UnifiedDelta::Error {
                    code: v
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or("error")
                        .to_string(),
                    message: v
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("上游返回未知错误")
                        .to_string(),
                });
            }

            _ => {}
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

pub struct ResponsesStreamEncoder {
    id: String,
    model: String,
    started: bool,
    finished: bool,
    /// IR block index → Responses output_index。
    output_index: HashMap<u32, u64>,
    next_output_index: u64,
    /// 每个 output_index 是否已发过 output_item.added。
    item_added: HashMap<u64, bool>,
    tool_meta: HashMap<u32, (String, String)>,
    usage: UnifiedUsage,
    finish_reason: Option<FinishReason>,
}

impl ResponsesStreamEncoder {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            started: false,
            finished: false,
            output_index: HashMap::new(),
            next_output_index: 0,
            item_added: HashMap::new(),
            tool_meta: HashMap::new(),
            usage: UnifiedUsage::default(),
            finish_reason: None,
        }
    }

    fn ev(&self, name: &str, mut payload: Value) -> SseEvent {
        // 每个事件体都带 type；序列号交给客户端自行推断。
        payload["type"] = json!(name);
        SseEvent::named(name, payload.to_string())
    }

    fn ensure_started(&mut self, out: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.id.is_empty() {
            self.id = format!("resp_{}", Uuid::new_v4().simple());
        }
        out.push(self.ev(
            "response.created",
            json!({
                "response": {
                    "id": self.id,
                    "object": "response",
                    "model": self.model,
                    "status": "in_progress",
                    "output": [],
                }
            }),
        ));
    }

    fn idx(&mut self, block_index: u32) -> u64 {
        if let Some(i) = self.output_index.get(&block_index) {
            return *i;
        }
        let i = self.next_output_index;
        self.next_output_index += 1;
        self.output_index.insert(block_index, i);
        i
    }
}

impl Default for ResponsesStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamEncoder for ResponsesStreamEncoder {
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
                let oi = self.idx(*index);
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    self.tool_meta.insert(*index, (id.clone(), name.clone()));
                    self.item_added.insert(oi, true);
                    out.push(self.ev(
                        "response.output_item.added",
                        json!({
                            "output_index": oi,
                            "item": {
                                "type": "function_call",
                                "id": format!("fc_{oi}"),
                                "call_id": id,
                                "name": name,
                                "arguments": "",
                            }
                        }),
                    ));
                }
            }

            UnifiedDelta::TextDelta { index, text } => {
                self.ensure_started(&mut out);
                let oi = self.idx(*index);
                if !self.item_added.contains_key(&oi) {
                    self.item_added.insert(oi, true);
                    out.push(self.ev(
                        "response.output_item.added",
                        json!({
                            "output_index": oi,
                            "item": {
                                "type": "message",
                                "id": format!("msg_{oi}"),
                                "role": "assistant",
                                "status": "in_progress",
                                "content": [],
                            }
                        }),
                    ));
                }
                if !text.is_empty() {
                    out.push(self.ev(
                        "response.output_text.delta",
                        json!({ "output_index": oi, "delta": text }),
                    ));
                }
            }

            UnifiedDelta::ThinkingDelta { index, text } => {
                self.ensure_started(&mut out);
                let oi = self.idx(*index);
                if !text.is_empty() {
                    out.push(self.ev(
                        "response.reasoning_summary_text.delta",
                        json!({ "output_index": oi, "delta": text }),
                    ));
                }
            }

            UnifiedDelta::ToolInputDelta {
                index,
                partial_json,
            } => {
                self.ensure_started(&mut out);
                let oi = self.idx(*index);
                // 没有 BlockStart 时补一个，否则下游拿不到 call_id。
                if !self.item_added.contains_key(&oi) {
                    self.item_added.insert(oi, true);
                    let (id, name) = self
                        .tool_meta
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| (format!("call_{oi}"), String::new()));
                    out.push(self.ev(
                        "response.output_item.added",
                        json!({
                            "output_index": oi,
                            "item": {
                                "type": "function_call",
                                "id": format!("fc_{oi}"),
                                "call_id": id,
                                "name": name,
                                "arguments": "",
                            }
                        }),
                    ));
                }
                if !partial_json.is_empty() {
                    out.push(self.ev(
                        "response.function_call_arguments.delta",
                        json!({ "output_index": oi, "delta": partial_json }),
                    ));
                }
            }

            UnifiedDelta::BlockStop { index } => {
                let oi = self.output_index.get(index).copied().unwrap_or(0);
                out.push(self.ev(
                    "response.output_item.done",
                    json!({ "output_index": oi }),
                ));
            }

            UnifiedDelta::Usage(u) => u.apply(&mut self.usage),

            UnifiedDelta::Finish(reason) => {
                self.finish_reason = Some(reason.clone());
            }

            UnifiedDelta::Error { code, message } => {
                self.ensure_started(&mut out);
                out.push(self.ev(
                    "error",
                    json!({ "code": code, "message": message }),
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

        let prompt_total = self
            .usage
            .input_tokens
            .saturating_add(self.usage.cache_read_tokens)
            .saturating_add(self.usage.cache_creation_tokens);

        let status = match self.finish_reason {
            Some(FinishReason::Length) => "incomplete",
            _ => "completed",
        };

        out.push(self.ev(
            "response.completed",
            json!({
                "response": {
                    "id": self.id,
                    "object": "response",
                    "model": self.model,
                    "status": status,
                    "usage": {
                        "input_tokens": prompt_total,
                        "output_tokens": self.usage.output_tokens,
                        "total_tokens": prompt_total.saturating_add(self.usage.output_tokens),
                        "input_tokens_details": { "cached_tokens": self.usage.cache_read_tokens },
                        "output_tokens_details": { "reasoning_tokens": self.usage.reasoning_tokens },
                    },
                }
            }),
        ));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(dec: &mut ResponsesStreamDecoder, events: &[SseEvent]) -> Vec<UnifiedDelta> {
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

    fn ev(name: &str, body: &str) -> SseEvent {
        SseEvent::named(name, body)
    }

    #[test]
    fn decodes_text_stream() {
        let mut dec = ResponsesStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                ev(
                    "response.created",
                    r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                ),
                ev(
                    "response.output_text.delta",
                    r#"{"type":"response.output_text.delta","output_index":0,"delta":"Hel"}"#,
                ),
                ev(
                    "response.output_text.delta",
                    r#"{"type":"response.output_text.delta","output_index":0,"delta":"lo"}"#,
                ),
                ev(
                    "response.completed",
                    r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":2}}}"#,
                ),
            ],
        );

        assert_eq!(text_of(&deltas), "Hello");
        assert_eq!(dec.text(), "Hello");
        assert_eq!(dec.usage().input_tokens, 10);
        assert!(dec.is_finished());
    }

    #[test]
    fn completed_with_tool_marks_finish_as_tool_use() {
        let mut dec = ResponsesStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                ev(
                    "response.output_item.added",
                    r#"{"output_index":0,"item":{"type":"function_call","call_id":"c","name":"f"}}"#,
                ),
                ev(
                    "response.completed",
                    r#"{"response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ),
            ],
        );
        assert!(deltas
            .iter()
            .any(|d| matches!(d, UnifiedDelta::Finish(FinishReason::ToolUse))));
    }

    #[test]
    fn usage_subtracts_cached_tokens() {
        let mut dec = ResponsesStreamDecoder::new();
        decode_all(
            &mut dec,
            &[ev(
                "response.completed",
                r#"{"response":{"status":"completed","usage":{"input_tokens":1000,"output_tokens":50,"input_tokens_details":{"cached_tokens":800}}}}"#,
            )],
        );
        let u = dec.usage();
        assert_eq!(u.input_tokens, 200);
        assert_eq!(u.cache_read_tokens, 800);
    }

    #[test]
    fn failed_event_surfaces_error() {
        let mut dec = ResponsesStreamDecoder::new();
        let out = dec
            .on_event(&ev(
                "response.failed",
                r#"{"response":{"error":{"message":"boom"}}}"#,
            ))
            .unwrap();
        match &out[0] {
            UnifiedDelta::Error { message, .. } => assert_eq!(message, "boom"),
            other => panic!("期望 Error，得到 {other:?}"),
        }
        assert!(dec.is_finished());
    }

    #[test]
    fn finish_synthesizes_terminator_when_upstream_truncates() {
        let mut dec = ResponsesStreamDecoder::new();
        dec.on_event(&ev("response.created", r#"{"response":{"id":"r","model":"m"}}"#))
            .unwrap();
        let out = dec.finish();
        assert!(matches!(out[0], UnifiedDelta::Finish(_)));
    }

    // --- 编码器 ---

    fn encode_all(enc: &mut ResponsesStreamEncoder, deltas: &[UnifiedDelta]) -> Vec<SseEvent> {
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
    fn encoder_emits_created_then_delta_then_completed() {
        let mut enc = ResponsesStreamEncoder::new();
        let events = encode_all(&mut enc, &[UnifiedDelta::text(0, "hi")]);

        let n = names(&events);
        assert!(n.contains(&"response.created".to_string()));
        assert!(n.contains(&"response.output_text.delta".to_string()));
        assert_eq!(n.last().unwrap(), "response.completed");
    }

    #[test]
    fn encoder_completed_carries_inclusive_token_counts() {
        let mut enc = ResponsesStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(0, "x"),
                UnifiedDelta::Usage(UsageDelta {
                    input_tokens: Some(100),
                    output_tokens: Some(5),
                    cache_read_tokens: Some(900),
                    ..Default::default()
                }),
            ],
        );

        let completed = events
            .iter()
            .find(|e| e.event.as_deref() == Some("response.completed"))
            .unwrap();
        let v: Value = completed.json().unwrap();
        assert_eq!(v["response"]["usage"]["input_tokens"], 1000, "必须含缓存");
        assert_eq!(
            v["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            900
        );
    }

    #[test]
    fn encoder_emits_function_call_item_for_tool_blocks() {
        let mut enc = ResponsesStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::BlockStart {
                    index: 0,
                    block: ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "search".into(),
                        input: json!({}),
                    },
                },
                UnifiedDelta::ToolInputDelta {
                    index: 0,
                    partial_json: "{}".into(),
                },
            ],
        );

        let added = events
            .iter()
            .find(|e| e.event.as_deref() == Some("response.output_item.added"))
            .unwrap();
        let v: Value = added.json().unwrap();
        assert_eq!(v["item"]["type"], "function_call");
        assert_eq!(v["item"]["call_id"], "call_1");
        assert_eq!(v["item"]["name"], "search");
    }

    #[test]
    fn encoder_incomplete_status_for_length_finish() {
        let mut enc = ResponsesStreamEncoder::new();
        let events = encode_all(
            &mut enc,
            &[
                UnifiedDelta::text(0, "x"),
                UnifiedDelta::Finish(FinishReason::Length),
            ],
        );
        let v: Value = events.last().unwrap().json().unwrap();
        assert_eq!(v["response"]["status"], "incomplete");
    }

    #[test]
    fn encoder_finish_is_idempotent() {
        let mut enc = ResponsesStreamEncoder::new();
        enc.on_delta(&UnifiedDelta::text(0, "x"));
        assert!(!enc.finish().is_empty());
        assert!(enc.finish().is_empty());
    }

    #[test]
    fn decoder_then_encoder_roundtrip_preserves_text() {
        let mut dec = ResponsesStreamDecoder::new();
        let deltas = decode_all(
            &mut dec,
            &[
                ev(
                    "response.created",
                    r#"{"response":{"id":"resp_1","model":"gpt-5"}}"#,
                ),
                ev(
                    "response.output_text.delta",
                    r#"{"output_index":0,"delta":"你好"}"#,
                ),
                ev(
                    "response.output_text.delta",
                    r#"{"output_index":0,"delta":"世界"}"#,
                ),
                ev(
                    "response.completed",
                    r#"{"response":{"status":"completed","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                ),
            ],
        );

        let mut enc = ResponsesStreamEncoder::new();
        let events = encode_all(&mut enc, &deltas);

        let text: String = events
            .iter()
            .filter(|e| e.event.as_deref() == Some("response.output_text.delta"))
            .filter_map(|e| e.json().ok())
            .filter_map(|v| v.get("delta").and_then(|d| d.as_str()).map(String::from))
            .collect();

        assert_eq!(text, "你好世界");
        assert_eq!(
            names(&events).last().unwrap(),
            "response.completed"
        );
    }
}
