//! 流式转发管线。
//!
//! 上游 SSE → 解码器（转 IR）→ 编码器（转下游协议）→ 下游字节流。
//! 同一条循环里顺带完成 TTFB 计时、usage 提取、文本拼接与异常归因。
//!
//! **同协议时走直通**：原样转发上游字节，只是旁路喂给解码器统计用量。
//! 这样既避免了"解码再编码"可能引入的偏差，又保留了完整的监控能力。
//! 只有跨协议时才真正重编码。

use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;

use crate::protocol::codec::{StreamDecoder, StreamEncoder};
use crate::protocol::dto::{FinishReason, UnifiedDelta, UnifiedResponse, UnifiedUsage};

use super::sse::{append_utf8_safe, encode_event, parse_event, take_sse_block};

/// 原始帧缓冲的上限（字节）。超出后停止累积并置 `raw_truncated`。
///
/// 一个长回答的 SSE 帧轻松上 MB，无上限累积等于把响应大小变成内存占用。
/// 截断而不是丢弃，是因为"原始帧"本来就只有排查时才看，半份也远比没有有用 ——
/// 前提是界面明确标出它被截断了。
const MAX_RAW_STREAM_BYTES: usize = 2 * 1024 * 1024;

/// 时间轴里最多记多少个事件。超出后不再记，只把 `truncated` 立起来。
///
/// 两千帧足够看出"哪一段在等"，而这份记录是**每个请求都存一份**的：
/// 不封顶的话，一个长回答就能让一条日志重上几百 KB。
const MAX_TIMINGS: usize = 2000;

/// 流式响应里每个事件的时间点，供「时间轴」看每个事件花了多久。
///
/// 存成**平行数组 + 名字表**而不是 `[{at_ms, name}]`：事件名高度重复
/// （一个长回答里 `content_block_delta` 能占九成），逐个存一遍字符串会让
/// 这条记录膨胀好几倍。名字表加上去之后，两千帧是几 KB —— 跟同一行里
/// 那个原始帧 blob 比可以忽略。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct StreamTimings {
    /// 每个事件相对流开始的毫秒数，按发生顺序。
    pub at_ms: Vec<i64>,
    /// 去重后的事件名。
    pub names: Vec<String>,
    /// 与 `at_ms` 等长；每项是 `names` 的下标。
    pub name_idx: Vec<u16>,
    /// 事件数超过上限，后面的没记。
    pub truncated: bool,
}

impl StreamTimings {
    pub fn push(&mut self, at_ms: i64, name: &str) {
        if self.at_ms.len() >= MAX_TIMINGS {
            self.truncated = true;
            return;
        }

        // 线性找即可：不同的事件名现实中不超过几十个。
        let idx = match self.names.iter().position(|n| n == name) {
            Some(i) => i,
            None => {
                self.names.push(name.to_string());
                self.names.len() - 1
            }
        };

        self.name_idx.push(idx as u16);
        self.at_ms.push(at_ms);
    }

    pub fn is_empty(&self) -> bool {
        self.at_ms.is_empty()
    }
}

/// 一个事件在时间轴里的名字：优先 SSE 的 `event:` 行，没有就回落到增量类型。
///
/// 与前端实时视图的取法保持一致（那边也是先找 `event:` 再回落到
/// `deltas[0].kind`）—— 两边不一致的话，同一个事件在实时视图和明细里
/// 会显示成两个名字。
fn timeline_name(raw: &str, deltas: &[UnifiedDelta]) -> String {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            return rest.trim().to_string();
        }
    }
    match deltas.first() {
        Some(d) => delta_display_name(d).to_string(),
        None => "—".to_string(),
    }
}

/// `UnifiedDelta` 在界面上的名字。
///
/// **必须与 `StreamDelta` 的 serde 标签逐字相同** —— 前端在实时视图里就是按
/// 那个标签取名字的。有测试钉着这一点（`delta_names_match_the_wire_tags`）。
fn delta_display_name(d: &UnifiedDelta) -> &'static str {
    match d {
        UnifiedDelta::MessageStart { .. } => "message_start",
        UnifiedDelta::BlockStart { .. } => "block_start",
        UnifiedDelta::TextDelta { .. } => "text",
        UnifiedDelta::ThinkingDelta { .. } => "thinking",
        UnifiedDelta::ToolInputDelta { .. } => "tool_input",
        UnifiedDelta::BlockStop { .. } => "block_stop",
        UnifiedDelta::Usage(_) => "usage",
        UnifiedDelta::Finish(_) => "finish",
        UnifiedDelta::Error { .. } => "error",
    }
}

/// 流结束后的统计结果。
#[derive(Debug, Clone, Default)]
pub struct StreamOutcome {
    pub usage: UnifiedUsage,
    /// 拼接后的最终文本，供监控展示与缓存写入。
    pub text: String,
    /// 由流式增量重建出的结构化内容，用于把流式结果写进缓存。
    pub content: Vec<crate::protocol::dto::ContentBlock>,
    /// 上游发来的原始 SSE 字节。
    pub raw_upstream: Vec<u8>,
    /// 重编码后发给客户端的原始 SSE 字节。
    ///
    /// 直通时为 `None`：那种情况下客户端收到的就是 `raw_upstream`，再存一份是纯浪费。
    pub raw_client: Option<Vec<u8>>,
    /// 原始帧是否因为超过上限被截断。
    pub raw_truncated: bool,
    /// 首字节耗时。
    pub ttfb: Option<Duration>,
    pub total: Option<Duration>,
    /// 处理过的 SSE 事件数。
    pub events: u64,
    /// 每个事件的时间点，供时间轴用。
    pub timings: StreamTimings,
    pub finish_reason: Option<FinishReason>,
    pub error: Option<String>,
}

/// 有上限地累积原始帧。到顶之后不再增长，只把 `truncated` 立起来。
#[derive(Debug, Default)]
struct RawBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl RawBuffer {
    fn push(&mut self, chunk: &[u8]) {
        if self.truncated {
            return;
        }
        if self.bytes.len() + chunk.len() > MAX_RAW_STREAM_BYTES {
            self.truncated = true;
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }
}

/// 把流式增量重建成完整的内容块列表。
///
/// 流式协议只给增量，想把它缓存下来重放就必须先还原成完整响应。
/// 工具调用的入参是分片 JSON，**收齐前不可解析**，因此先攒字符串、
/// 到 `BlockStop` 或流结束时才解析。
#[derive(Debug, Default)]
pub struct ContentAccumulator {
    /// block index → 已累积的内容块。
    blocks: std::collections::BTreeMap<u32, crate::protocol::dto::ContentBlock>,
    /// block index → 工具入参的分片 JSON。
    tool_json: std::collections::BTreeMap<u32, String>,
}

impl ContentAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, d: &UnifiedDelta) {
        use crate::protocol::dto::ContentBlock;
        match d {
            UnifiedDelta::BlockStart { index, block } => {
                let entry = self.blocks.entry(*index).or_insert_with(|| block.clone());
                // 已经因为收到 delta 而占位过的块，补上 id/name 这类首帧才有的信息。
                if let (
                    ContentBlock::ToolUse { id, name, .. },
                    ContentBlock::ToolUse {
                        id: cur_id,
                        name: cur_name,
                        ..
                    },
                ) = (block, entry)
                {
                    if !id.is_empty() {
                        *cur_id = id.clone();
                    }
                    if !name.is_empty() {
                        *cur_name = name.clone();
                    }
                }
            }
            UnifiedDelta::TextDelta { index, text } => {
                let entry = self
                    .blocks
                    .entry(*index)
                    .or_insert_with(|| ContentBlock::text(""));
                if let ContentBlock::Text { text: cur } = entry {
                    cur.push_str(text);
                }
            }
            UnifiedDelta::ThinkingDelta { index, text } => {
                let entry = self.blocks.entry(*index).or_insert_with(|| {
                    ContentBlock::Thinking {
                        text: String::new(),
                        signature: None,
                    }
                });
                if let ContentBlock::Thinking { text: cur, .. } = entry {
                    cur.push_str(text);
                }
            }
            UnifiedDelta::ToolInputDelta {
                index,
                partial_json,
            } => {
                self.tool_json
                    .entry(*index)
                    .or_default()
                    .push_str(partial_json);
                self.blocks.entry(*index).or_insert_with(|| {
                    ContentBlock::ToolUse {
                        id: String::new(),
                        name: String::new(),
                        input: serde_json::json!({}),
                    }
                });
            }
            // 其余增量不影响内容本身。
            _ => {}
        }
    }

    /// 产出最终内容块，按 index 顺序排列。
    pub fn finish(mut self) -> Vec<crate::protocol::dto::ContentBlock> {
        use crate::protocol::dto::ContentBlock;

        // 入参收齐后才能解析。
        for (index, raw) in std::mem::take(&mut self.tool_json) {
            if let Some(ContentBlock::ToolUse { input, .. }) = self.blocks.get_mut(&index) {
                *input = crate::protocol::shared::tools::parse_tool_input(&raw);
            }
        }

        self.blocks
            .into_values()
            // 空文本块通常是协议噪声，不必保留。
            .filter(|b| !matches!(b, ContentBlock::Text { text } if text.is_empty()))
            .collect()
    }
}

/// 流的超时配置。
#[derive(Debug, Clone, Copy)]
pub struct StreamTimeouts {
    /// 首字节超时：建立连接后多久没收到第一个字节就判失败。
    pub first_byte: Duration,
    /// 空闲超时：两个 chunk 之间的最大间隔。
    pub idle: Duration,
}

impl Default for StreamTimeouts {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(60),
            idle: Duration::from_secs(300),
        }
    }
}

/// 流式过程中的实时观察者。
///
/// 与 `on_finish` 分开：finish 只在收尾跑一次，可以做重活（计费、写库、落捕获）；
/// 观察者在每个 SSE 事件上被调用，必须廉价、同步、绝不阻塞下游 —— 它跑在
/// 转发循环里，慢一点就是客户端多等一点。
///
/// 存在的理由：流式请求在整条流结束前不落库，不在这里插一脚，前端就只能
/// 对着空列表猜请求是不是还活着。
pub trait StreamObserver: Send + 'static {
    /// 一个上游 SSE 块被解码后。`raw` 是该块的原文（直通时就是客户端收到的那份）。
    ///
    /// 解析失败时 `deltas` 为空，但**照样要回调** —— 坏帧正是排查时最想看的东西。
    fn on_event(&mut self, raw: &str, deltas: &[UnifiedDelta]);

    /// 流结束（正常、出错或客户端断连）时调用一次。
    fn on_finish(&mut self, error: Option<&str>);
}

/// 把一个上游字节流变成客户端可读的响应体。
///
/// `encoder` 为 `None` 时走直通：原样转发上游字节。
/// `observer` 为 `None` 时不推实时事件流（测试与不需要该能力的调用方走这条）。
/// `on_finish` 在流结束时调用一次，用于记账、写缓存与发事件。
pub fn translate_stream_observed<F>(
    upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    mut decoder: Box<dyn StreamDecoder>,
    mut encoder: Option<Box<dyn StreamEncoder>>,
    timeouts: StreamTimeouts,
    mut observer: Option<Box<dyn StreamObserver>>,
    on_finish: F,
) -> Body
where
    F: FnOnce(StreamOutcome) + Send + 'static,
{
    let passthrough = encoder.is_none();
    let mut on_finish = Some(on_finish);

    let stream = async_stream::stream! {
        let start = Instant::now();
        let mut outcome = StreamOutcome::default();
        let mut acc = ContentAccumulator::new();
        let mut buf = String::new();
        let mut rem: Vec<u8> = Vec::new();
        let mut pinned = upstream;
        let mut raw_upstream = RawBuffer::default();
        let mut raw_client = RawBuffer::default();

        loop {
            // 首个字节用更短的超时，之后用空闲超时。
            let timeout = if outcome.ttfb.is_none() {
                timeouts.first_byte
            } else {
                timeouts.idle
            };

            let chunk = match tokio::time::timeout(timeout, pinned.next()).await {
                Err(_) => {
                    outcome.error = Some(if outcome.ttfb.is_none() {
                        "等待上游首字节超时".to_string()
                    } else {
                        "上游流空闲超时".to_string()
                    });
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    outcome.error = Some(format!("读取上游流失败: {e}"));
                    break;
                }
                Ok(Some(Ok(chunk))) => chunk,
            };

            if outcome.ttfb.is_none() {
                outcome.ttfb = Some(start.elapsed());
            }

            // 原始帧：上游侧无条件留一份。
            raw_upstream.push(&chunk);

            // 直通模式：先把原始字节原样发给客户端。
            if passthrough {
                yield Ok::<Bytes, std::io::Error>(chunk.clone());
            }

            // 无论哪种模式都要喂给解码器，用于统计用量与拼接文本。
            append_utf8_safe(&mut buf, &mut rem, &chunk);

            while let Some(block) = take_sse_block(&mut buf) {
                let Some(ev) = parse_event(&block) else { continue };
                outcome.events += 1;

                let deltas = match decoder.on_event(&ev) {
                    Ok(d) => d,
                    Err(e) => {
                        // 单个事件解析失败不该中断整条流 —— 记下来继续，
                        // 否则客户端的回答会被从中间截断。观察者照样收到这个块，
                        // 只是增量是空的：坏帧恰恰是排查时最该看见的。
                        tracing::warn!("流事件解析失败，已跳过: {e}");
                        Vec::new()
                    }
                };

                if let Some(o) = observer.as_mut() {
                    o.on_event(&block, &deltas);
                }

                // 时间轴用的时间点。记在这里而不是记在观察者里：`StreamOutcome`
                // 是要落库的，观察者只管实时推送，两边各记各的会跑出两份不一致的
                // 时间。放在观察者之后是为了让"推给前端"这件事先发生 ——
                // 名字推导要遍历原文，不能让排查用的东西拖慢实时视图。
                outcome.timings.push(
                    start.elapsed().as_millis() as i64,
                    &timeline_name(&block, &deltas),
                );

                for d in &deltas {
                    if let UnifiedDelta::Finish(r) = d {
                        outcome.finish_reason = Some(r.clone());
                    }
                    acc.apply(d);
                }

                if !passthrough {
                    for d in &deltas {
                        if let Some(enc) = encoder.as_mut() {
                            for out in enc.on_delta(d) {
                                let bytes = encode_event(&out);
                                raw_client.push(&bytes);
                                yield Ok::<Bytes, std::io::Error>(bytes);
                            }
                        }
                    }
                }
            }
        }

        // 收尾：补齐解码器的兜底事件，再由编码器输出终止帧。
        for d in decoder.finish() {
            if let UnifiedDelta::Finish(r) = &d {
                outcome.finish_reason = Some(r.clone());
            }
            if !passthrough {
                if let Some(enc) = encoder.as_mut() {
                    for out in enc.on_delta(&d) {
                        let bytes = encode_event(&out);
                        raw_client.push(&bytes);
                        yield Ok::<Bytes, std::io::Error>(bytes);
                    }
                }
            }
        }
        if !passthrough {
            if let Some(enc) = encoder.as_mut() {
                for out in enc.finish() {
                    let bytes = encode_event(&out);
                    raw_client.push(&bytes);
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
            }
        }

        outcome.usage = decoder.usage();
        outcome.text = decoder.text();
        outcome.content = acc.finish();
        // 直通时客户端收到的就是上游字节，客户端侧那份不必重复存。
        outcome.raw_upstream = raw_upstream.bytes;
        outcome.raw_client = (!passthrough).then_some(raw_client.bytes);
        outcome.raw_truncated = raw_upstream.truncated || raw_client.truncated;
        outcome.total = Some(start.elapsed());

        if let Some(o) = observer.as_mut() {
            o.on_finish(outcome.error.as_deref());
        }
        if let Some(f) = on_finish.take() {
            f(outcome);
        }
    };

    Body::from_stream(stream)
}

/// 从一份已完成的响应合成 SSE 流。
///
/// 用于缓存命中：客户端要流式，但结果是现成的，于是用编码器把它"假装"成流。
/// 这样客户端拿到的响应形态与真实调用一致，不需要它区分是否命中缓存。
pub fn stream_from_response<F>(
    response: UnifiedResponse,
    usage: UnifiedUsage,
    mut encoder: Box<dyn StreamEncoder>,
    on_finish: F,
) -> Body
where
    F: FnOnce(StreamOutcome) + Send + 'static,
{
    let mut on_finish = Some(on_finish);

    let stream = async_stream::stream! {
        let start = Instant::now();
        let mut outcome = StreamOutcome::default();
        outcome.finish_reason = Some(response.finish_reason.clone());

        let mut deltas = vec![UnifiedDelta::MessageStart {
            id: response.id.clone(),
            model: response.model.clone(),
        }];

        // 按内容块顺序产出，尽量贴近真实流式的形态。
        for (i, block) in response.content.iter().enumerate() {
            let index = i as u32;
            match block {
                crate::protocol::dto::ContentBlock::Text { text } => {
                    deltas.push(UnifiedDelta::BlockStart {
                        index,
                        block: crate::protocol::dto::ContentBlock::text(""),
                    });
                    deltas.push(UnifiedDelta::TextDelta { index, text: text.clone() });
                }
                crate::protocol::dto::ContentBlock::ToolUse { id, name, input } => {
                    deltas.push(UnifiedDelta::BlockStart {
                        index,
                        block: crate::protocol::dto::ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: serde_json::json!({}),
                        },
                    });
                    deltas.push(UnifiedDelta::ToolInputDelta {
                        index,
                        partial_json: serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    });
                }
                crate::protocol::dto::ContentBlock::Thinking { text, .. } => {
                    deltas.push(UnifiedDelta::ThinkingDelta { index, text: text.clone() });
                }
                _ => {}
            }
            deltas.push(UnifiedDelta::BlockStop { index });
        }

        deltas.push(UnifiedDelta::Usage(crate::protocol::dto::UsageDelta {
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            cache_read_tokens: Some(usage.cache_read_tokens),
            cache_creation_tokens: Some(usage.cache_creation_tokens),
            reasoning_tokens: Some(usage.reasoning_tokens),
            total_tokens: Some(usage.total()),
        }));
        deltas.push(UnifiedDelta::Finish(response.finish_reason.clone()));

        for d in &deltas {
            for out in encoder.on_delta(d) {
                yield Ok::<Bytes, std::io::Error>(encode_event(&out));
            }
        }
        for out in encoder.finish() {
            yield Ok::<Bytes, std::io::Error>(encode_event(&out));
        }

        outcome.ttfb = Some(start.elapsed());
        outcome.total = Some(start.elapsed());
        outcome.events = deltas.len() as u64;
        outcome.text = response.concat_text();
        outcome.content = response.content.clone();
        outcome.usage = usage;

        if let Some(f) = on_finish.take() {
            f(outcome);
        }
    };

    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::sse::done_event;
    use crate::protocol::anthropic::AnthropicStreamDecoder;
    use crate::protocol::oai_chat::{ChatStreamDecoder, ChatStreamEncoder};

    /// 把若干字节块做成上游流。
    fn upstream(chunks: Vec<&'static str>) -> BoxStream<'static, reqwest::Result<Bytes>> {
        Box::pin(futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c.as_bytes()))),
        ))
    }

    /// 收集一个响应体的全部字节。
    async fn run(body: Body) -> Vec<u8> {
        body.into_data_stream()
            .filter_map(|r| async move { r.ok() })
            .fold(Vec::new(), |mut acc, b| async move {
                acc.extend_from_slice(&b);
                acc
            })
            .await
    }

    #[tokio::test]
    async fn passthrough_forwards_original_bytes_verbatim() {
        let chunks = vec![
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: [DONE]\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks.clone()),
            Box::new(AnthropicStreamDecoder::new()),
            None, // 直通
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });

        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\"text\":\"hi\""));
        assert!(text.contains("[DONE]"));

        let outcome = rx.await.unwrap();
        assert_eq!(outcome.text, "hi");
        assert!(outcome.ttfb.is_some());
        assert!(outcome.events >= 2);
    }

    /// 直通时客户端收到的就是上游字节，不必再存一份。
    #[tokio::test]
    async fn passthrough_records_upstream_raw_but_not_a_duplicate_client_copy() {
        let chunks = vec![
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "data: [DONE]\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None, // 直通
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;

        let outcome = rx.await.unwrap();
        assert_eq!(
            outcome.raw_upstream, bytes,
            "直通时上游帧应逐字节等于客户端收到的"
        );
        assert!(outcome.raw_client.is_none(), "直通不该重复存客户端那份");
        assert!(!outcome.raw_truncated);
    }

    /// 跨协议时两侧原始帧都要留，且必须是不同的两份 —— 这正是排查
    /// "转换把什么改坏了" 时唯一能对照的东西。
    #[tokio::test]
    async fn transcoding_keeps_both_raw_sides_and_they_differ() {
        let chunks = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"m\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(ChatStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;

        let outcome = rx.await.unwrap();
        let client_raw = outcome.raw_client.expect("重编码时必须留下游原始帧");

        assert!(String::from_utf8_lossy(&outcome.raw_upstream).contains("message_start"));
        assert!(String::from_utf8_lossy(&client_raw).contains("chat.completion.chunk"));
        assert_ne!(outcome.raw_upstream, client_raw);
        assert_eq!(client_raw, bytes, "下游原始帧应等于客户端实际收到的字节");
    }

    /// 原始帧缓冲必须有上限：一个长回答的 SSE 轻松上 MB，无限累积等于把
    /// 响应体大小变成常驻内存。
    #[test]
    fn raw_buffer_stops_growing_at_the_cap_and_reports_truncation() {
        let mut b = RawBuffer::default();

        b.push(&vec![b'x'; MAX_RAW_STREAM_BYTES - 1]);
        assert!(!b.truncated);
        assert_eq!(b.bytes.len(), MAX_RAW_STREAM_BYTES - 1);

        // 这一下会越界：整个 chunk 丢弃并置位，而不是只存一半 ——
        // 存一半会切在 SSE 帧中间，看起来像报文本身坏了。
        b.push(b"yy");
        assert!(b.truncated);
        assert_eq!(b.bytes.len(), MAX_RAW_STREAM_BYTES - 1);

        // 之后不再增长。
        b.push(b"zzz");
        assert_eq!(b.bytes.len(), MAX_RAW_STREAM_BYTES - 1);
    }

    #[tokio::test]
    async fn cross_protocol_stream_is_transcoded() {
        // 上游是 Anthropic 流，下游要 OpenAI Chat 形态
        let chunks = vec![
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(ChatStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });

        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("\"chat.completion.chunk\""), "应转成 Chat 形态: {text}");
        assert!(text.contains("你好"));
        assert!(text.contains("[DONE]"), "OpenAI 流必须以 [DONE] 收尾");

        let outcome = rx.await.unwrap();
        assert_eq!(outcome.text, "你好");
    }

    #[tokio::test]
    async fn multibyte_split_across_chunks_is_not_mangled_in_passthrough() {
        // 直通模式下字节完全不动，不会产生乱码；同时解码器统计的文本应正确。
        // 注意切点落在 JSON 中间，正是多字节字符会被 TCP 分片切开的真实场景。
        let first = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"中";
        let second = "文\"}}\n\n";

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(vec![first, second]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });

        let bytes = run(body).await;
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            format!("{first}{second}"),
            "直通模式下字节必须原样"
        );

        let outcome = rx.await.unwrap();
        assert_eq!(outcome.text, "中文");
    }

    #[tokio::test]
    async fn usage_is_extracted_from_stream() {
        let chunks = vec![
            "event: message_start\ndata: {\"message\":{\"id\":\"m\",\"model\":\"x\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":900}}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        run(body).await;

        let o = rx.await.unwrap();
        assert_eq!(o.usage.input_tokens, 100);
        assert_eq!(o.usage.cache_read_tokens, 900);
        assert_eq!(o.usage.output_tokens, 7);
    }

    #[tokio::test]
    async fn malformed_event_is_skipped_without_breaking_the_stream() {
        let chunks = vec![
            "data: this is not json\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: [DONE]\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;

        assert!(
            String::from_utf8_lossy(&bytes).contains("\"text\":\"ok\""),
            "坏事件之后的内容仍须转发"
        );
        let o = rx.await.unwrap();
        assert_eq!(o.text, "ok");
    }

    #[tokio::test]
    async fn idle_timeout_is_attributed() {
        // 永不产出的上游
        let stalled: BoxStream<'static, reqwest::Result<Bytes>> =
            Box::pin(futures::stream::pending());

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            stalled,
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts {
                first_byte: Duration::from_millis(30),
                idle: Duration::from_millis(30),
            },
            None,
            move |o| {
                let _ = tx.send(o);
            });
        run(body).await;

        let o = rx.await.unwrap();
        assert!(o.error.is_some(), "超时必须被归因");
        assert!(o.error.unwrap().contains("首字节"), "首次超时应报首字节超时");
    }

    #[tokio::test]
    async fn on_finish_runs_exactly_once() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = counter.clone();

        let body = translate_stream_observed(
            upstream(vec!["data: [DONE]\n\n"]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |_| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            });
        run(body).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // --- 缓存命中重放 ---

    #[tokio::test]
    async fn stream_from_response_produces_valid_sse() {
        use crate::protocol::dto::{ContentBlock, FinishReason, UnifiedResponse};

        let resp = UnifiedResponse {
            id: "resp_1".into(),
            model: "claude-sonnet-5".into(),
            content: vec![ContentBlock::text("缓存的答案")],
            finish_reason: FinishReason::Stop,
        };
        let usage = UnifiedUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = stream_from_response(
            resp,
            usage,
            Box::new(crate::protocol::anthropic::AnthropicStreamEncoder::new()),
            move |o| {
                let _ = tx.send(o);
            },
        );

        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("message_start"));
        assert!(text.contains("缓存的答案"));
        assert!(text.contains("message_stop"), "必须以 message_stop 收尾");

        let o = rx.await.unwrap();
        assert_eq!(o.text, "缓存的答案");
        assert_eq!(o.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn stream_from_response_can_encode_tool_calls() {
        use crate::protocol::dto::{ContentBlock, FinishReason, UnifiedResponse};

        let resp = UnifiedResponse {
            id: "r".into(),
            model: "m".into(),
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "search".into(),
                input: serde_json::json!({"q": "rust"}),
            }],
            finish_reason: FinishReason::ToolUse,
        };

        let body = stream_from_response(
            resp,
            UnifiedUsage::default(),
            Box::new(ChatStreamEncoder::new()),
            |_| {},
        );

        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("call_1"), "工具调用必须能重放: {text}");
        assert!(text.contains("tool_calls"));
    }

    #[tokio::test]
    async fn empty_upstream_produces_no_output_but_still_finishes() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(vec![]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;
        assert!(bytes.is_empty());
        let o = rx.await.unwrap();
        assert!(o.ttfb.is_none(), "没有任何字节就没有 TTFB");
    }

    #[tokio::test]
    async fn chat_decoder_usage_flows_through_passthrough() {
        let chunks = vec![
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":400}}}\n\n",
            "data: [DONE]\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(ChatStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        run(body).await;

        let o = rx.await.unwrap();
        assert_eq!(o.usage.input_tokens, 100, "缓存必须被扣除");
        assert_eq!(o.usage.cache_read_tokens, 400);
    }

    /// 用户的真实配置：上游是 Chat 协议，客户端是 Codex（Responses）。
    /// Chat 解码器不产 BlockStop，所以这条路最容易丢掉 `output_item.done`。
    #[tokio::test]
    async fn chat_upstream_tool_call_reaches_codex_as_a_complete_item() {
        use crate::protocol::oai_responses::ResponsesStreamEncoder;

        let chunks = vec![
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"exec_command\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\n",
            "data: {\"id\":\"c\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(ChatStreamDecoder::new()),
            Some(Box::new(ResponsesStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );
        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);

        // done 必须出现，且带着拼全的 arguments —— 否则 Codex 不会执行工具。
        let done_line = text
            .lines()
            .find(|l| l.starts_with("data:") && l.contains("response.output_item.done"))
            .expect("必须发出 output_item.done");
        let v: serde_json::Value =
            serde_json::from_str(done_line.trim_start_matches("data:").trim()).unwrap();
        assert_eq!(v["item"]["type"], "function_call");
        assert_eq!(v["item"]["call_id"], "call_1");
        assert_eq!(v["item"]["name"], "exec_command");
        assert_eq!(v["item"]["arguments"], r#"{"cmd":"ls"}"#);

        // 顺序：done 在 completed 之前。
        let di = text.find("response.output_item.done").unwrap();
        let ci = text.find("response.completed").unwrap();
        assert!(di < ci, "done 必须早于 completed");

        let _ = rx.await;
    }

    #[tokio::test]
    async fn sse_done_marker_alone_is_handled() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(vec!["data: [DONE]\n\n"]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;
        assert!(String::from_utf8_lossy(&bytes).contains("[DONE]"));
        let _ = rx.await;
    }

    #[test]
    fn done_event_helper_is_reachable() {
        assert!(done_event().is_done());
    }

    // --- 内容重建 ---

    #[test]
    fn accumulator_rebuilds_text() {
        use crate::protocol::dto::ContentBlock;
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::BlockStart {
            index: 0,
            block: ContentBlock::text(""),
        });
        acc.apply(&UnifiedDelta::text(0, "你"));
        acc.apply(&UnifiedDelta::text(0, "好"));

        let content = acc.finish();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), Some("你好"));
    }

    #[test]
    fn accumulator_rebuilds_tool_call_from_fragments() {
        use crate::protocol::dto::ContentBlock;
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::BlockStart {
            index: 0,
            block: ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "search".into(),
                input: serde_json::json!({}),
            },
        });
        acc.apply(&UnifiedDelta::ToolInputDelta {
            index: 0,
            partial_json: r#"{"q":"#.into(),
        });
        acc.apply(&UnifiedDelta::ToolInputDelta {
            index: 0,
            partial_json: r#""rust"}"#.into(),
        });

        let content = acc.finish();
        match &content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "search");
                // 分片收齐后才解析
                assert_eq!(input["q"], "rust");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn accumulator_keeps_blocks_in_index_order() {
        let mut acc = ContentAccumulator::new();
        // 乱序到达
        acc.apply(&UnifiedDelta::text(2, "c"));
        acc.apply(&UnifiedDelta::text(0, "a"));
        acc.apply(&UnifiedDelta::text(1, "b"));

        let content = acc.finish();
        let joined: String = content.iter().filter_map(|b| b.as_text()).collect();
        assert_eq!(joined, "abc");
    }

    #[test]
    fn accumulator_drops_empty_text_blocks() {
        use crate::protocol::dto::ContentBlock;
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::BlockStart {
            index: 0,
            block: ContentBlock::text(""),
        });
        assert!(acc.finish().is_empty(), "空文本块是协议噪声，应被丢掉");
    }

    #[test]
    fn accumulator_merges_block_start_into_existing_entry() {
        // 有些上游先发 delta 再发 BlockStart，id/name 不能丢
        use crate::protocol::dto::ContentBlock;
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::ToolInputDelta {
            index: 0,
            partial_json: "{}".into(),
        });
        acc.apply(&UnifiedDelta::BlockStart {
            index: 0,
            block: ContentBlock::ToolUse {
                id: "call_9".into(),
                name: "f".into(),
                input: serde_json::json!({}),
            },
        });

        let content = acc.finish();
        match &content[0] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "call_9");
                assert_eq!(name, "f");
            }
            other => panic!("期望 ToolUse，得到 {other:?}"),
        }
    }

    #[test]
    fn accumulator_rebuilds_thinking_blocks() {
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::ThinkingDelta {
            index: 0,
            text: "想".into(),
        });
        acc.apply(&UnifiedDelta::ThinkingDelta {
            index: 0,
            text: "一下".into(),
        });
        acc.apply(&UnifiedDelta::text(1, "答案"));

        let content = acc.finish();
        assert!(matches!(
            content[0],
            crate::protocol::dto::ContentBlock::Thinking { .. }
        ));
        assert_eq!(content[1].as_text(), Some("答案"));
    }

    #[tokio::test]
    async fn stream_outcome_carries_reconstructed_content() {
        let chunks = vec![
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"最终\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        run(body).await;

        let o = rx.await.unwrap();
        assert_eq!(o.content.len(), 1);
        assert_eq!(o.content[0].as_text(), Some("最终"));
    }

    #[tokio::test]
    async fn a_streamed_response_records_a_timing_per_event() {
        // 时间轴完全靠这份记录画出来，少一个事件就少一根条。
        let chunks = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"b\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        run(body).await;

        let o = rx.await.unwrap();
        let t = &o.timings;
        assert_eq!(t.at_ms.len() as u64, o.events, "时间点数必须与事件数一致");
        assert_eq!(t.at_ms.len(), 4);
        assert!(!t.truncated);

        let names: Vec<&str> = t
            .name_idx
            .iter()
            .map(|i| t.names[*i as usize].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_delta",
                "content_block_delta",
                "message_stop"
            ]
        );
        assert_eq!(
            t.names.len(),
            3,
            "名字表要去重：重复的事件名只该占一个条目"
        );

        // 单调不减，否则"两个事件之间花了多久"会算出负数。
        assert!(t.at_ms.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn timings_dedupe_names_and_stop_at_the_cap() {
        let mut t = StreamTimings::default();
        for i in 0..10i64 {
            t.push(i, if i % 3 == 0 { "a" } else { "b" });
        }
        assert_eq!(t.at_ms.len(), 10);
        assert_eq!(t.names, ["a", "b"]);
        assert!(!t.truncated);

        let mut t = StreamTimings::default();
        for i in 0..(MAX_TIMINGS as i64 + 5) {
            t.push(i, "x");
        }
        assert_eq!(t.at_ms.len(), MAX_TIMINGS, "封顶之后不再增长");
        assert!(t.truncated, "封顶了必须让界面知道后面还有");
        assert_eq!(t.names.len(), 1, "名字表不该跟着帧数一起涨");
    }

    #[test]
    fn delta_names_match_the_wire_tags() {
        // 时间轴里的名字有两种来源：SSE 的 `event:` 行（各协议自己的），
        // 以及没有那一行时回落到增量类型。回落的那套**必须**与前端按 `kind`
        // 分支用的标签逐字相同，否则同一个事件在实时视图和明细里会显示成
        // 两个名字 —— 那种不一致没人会在测试里发现，只会在界面上困惑。
        use crate::protocol::dto::{ContentBlock, UsageDelta};
        use crate::traffic::stream_events::StreamDelta;

        let samples = vec![
            UnifiedDelta::MessageStart {
                id: "m".into(),
                model: "x".into(),
            },
            UnifiedDelta::BlockStart {
                index: 0,
                block: ContentBlock::text(""),
            },
            UnifiedDelta::text(0, "hi"),
            UnifiedDelta::ThinkingDelta {
                index: 0,
                text: "t".into(),
            },
            UnifiedDelta::ToolInputDelta {
                index: 0,
                partial_json: "{}".into(),
            },
            UnifiedDelta::BlockStop { index: 0 },
            UnifiedDelta::Usage(UsageDelta {
                output_tokens: Some(1),
                ..Default::default()
            }),
            UnifiedDelta::Finish(FinishReason::ToolUse),
            UnifiedDelta::Error {
                code: "c".into(),
                message: "m".into(),
            },
        ];

        for d in &samples {
            let wire = serde_json::to_value(StreamDelta::from(d)).unwrap();
            assert_eq!(
                wire["kind"].as_str().unwrap(),
                delta_display_name(d),
                "时间轴的名字与前端的 kind 标签对不上：{d:?}"
            );
        }
    }
}
