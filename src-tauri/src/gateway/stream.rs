//! 流式转发管线。
//!
//! 上游 SSE → 解码器（转 IR）→ 编码器（转下游协议）→ 下游字节流。
//! 同一条循环里顺带完成 TTFB 计时、usage 提取、文本拼接与异常归因。
//!
//! **同协议时走直通**：原样转发上游字节，只是旁路喂给解码器统计用量。
//! 这样既避免了"解码再编码"可能引入的偏差，又保留了完整的监控能力。
//! 只有跨协议时才真正重编码。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use futures::future::BoxFuture;
use futures::stream::{BoxStream, StreamExt};
use serde::Serialize;

use crate::traffic::control::{RequestControl, RequestSignal};

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
    /// 每个事件相对**请求到达网关**的毫秒数，按发生顺序（与 TTFB 同一个起点）。
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
        UnifiedDelta::ThinkingSignature { .. } => "thinking_signature",
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
    /// 首个可解析事件（带 `data:` 的 SSE 块）耗时，从请求到达网关算起。
    ///
    /// 不能量到"首个字节"：上游常在真正出内容前先发纯注释帧（`: ping`）保活，
    /// 源码注释与心跳帧在客户端那边是被忽略的，拿它们当首字会让"干等几十秒"
    /// 显示成"几毫秒"。
    pub ttft: Option<Duration>,
    /// 首个原始字节耗时。只用于把「首字节超时」切成「空闲超时」，
    /// 不作为对外指标 —— 对外看的是 [`Self::ttft`]。
    pub first_byte: Option<Duration>,
    pub total: Option<Duration>,
    /// 处理过的 SSE 事件数。
    pub events: u64,
    /// 每个事件的时间点，供时间轴用。
    pub timings: StreamTimings,
    pub finish_reason: Option<FinishReason>,
    pub error: Option<String>,
    /// 流是**被丢弃**的（客户端断开 / 请求被取消），不是跑完的。
    ///
    /// 与 `error` 分开：客户端断连时上游并没有报错，把它归成 502 会让客户端
    /// 以为是网关或上游抽风。收尾按 499（nginx 的「客户端关闭请求」）记。
    pub aborted: bool,
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
            UnifiedDelta::ThinkingSignature { index, signature } => {
                let entry = self.blocks.entry(*index).or_insert_with(|| {
                    ContentBlock::Thinking {
                        text: String::new(),
                        signature: None,
                    }
                });
                if let ContentBlock::Thinking {
                    signature: sig, ..
                } = entry
                {
                    *sig = Some(signature.clone());
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
    /// 首字节超时：拿到响应头后多久没收到第一个字节就判失败。
    ///
    /// 计时从拿到响应头算（`tokio::time::timeout` 每轮重新起表），与 TTFB 的起点
    /// （请求到达）不同 —— 等响应头那段归 HTTP 客户端的超时管。
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

/// 断流时给客户端看的那句话。
///
/// `Codex`、`Pi` 这类客户端是按**报文里的字**判断"这个错误能不能重试"的，
/// 所以要把原始原因带上，让用户和我们自己都看得出断在哪一步。
fn stream_break_message(error: &str) -> String {
    format!("上游流中断：{error}（回复可能不完整，可以重试本次请求）")
}

/// 把一个上游字节流变成客户端可读的响应体。
///
/// `encoder` 为 `None` 时走直通：原样转发上游字节。
/// `observer` 为 `None` 时不推实时事件流（测试与不需要该能力的调用方走这条）。
/// `on_finish` 在流结束时调用一次，用于记账、写缓存与发事件。
/// 流式增量的逐条改写钩子。
///
/// 每解出一条增量调用一次；返回 `Some` 表示替换成 0..n 条新增量
/// （空数组 = 丢弃），`None` = 原样放行。**不缓冲整条流**：改完立刻编码下发，
/// 首字延迟与没开脚本时一致。
///
/// 用 `FnMut` 而不是 `Fn`：将来若要支持需要跨增量状态的改写（例如把最后
/// 一个字符先扣住、等下一块再匹配），实现方可以在闭包里维护状态。
pub struct StreamDeltaRewrite {
    pub apply: Box<dyn FnMut(&UnifiedDelta) -> Option<Vec<UnifiedDelta>> + Send + 'static>,
}

/// 「中断并重试」在流中途用的那一半。
///
/// 响应头早就发给客户端了，没法再回一个 `Response` —— 只能把**新一次请求**的
/// 字节接到同一条下游流上。`make` 由 `pipeline` 提供（它才知道怎么把整条请求
/// 重跑一遍），这里只负责判断"现在能不能接"以及旧那条怎么收尾。
///
/// `make` 返回 `None` 表示重试本身也没成功：此时这条下游流就此结束，客户端看到
/// 的是"流没起来"，会自己重试 —— 总好过把一段 JSON 错误灌进 SSE 里。
pub struct StreamRestart {
    pub control: Arc<RequestControl>,
    pub make: Box<dyn FnOnce() -> BoxFuture<'static, Option<Body>> + Send>,
}

/// 对一批增量逐条跑改写钩子。钩子返回空数组时该条被丢弃。
fn apply_delta_rewrite(
    deltas: Vec<UnifiedDelta>,
    rewrite: &mut Option<StreamDeltaRewrite>,
) -> Vec<UnifiedDelta> {
    let Some(rw) = rewrite.as_mut() else {
        return deltas;
    };
    let mut out = Vec::with_capacity(deltas.len());
    for d in deltas {
        match (rw.apply)(&d) {
            Some(replaced) => out.extend(replaced),
            None => out.push(d),
        }
    }
    out
}

/// 一条流的收尾状态，**由 `Drop` 兜底**。
///
/// 只在生成器末尾调 `on_finish` 是不够的：`Body::from_stream` 的消费者随时可能
/// 在某个 `yield` 之后直接丢掉这条流 —— 客户端收完 `[DONE]` 就关连接是最常见的
/// 一种，hyper 于是把 Body 丢给析构，生成器后面那些记账代码一行都跑不到。
/// 那条请求既不进日志、也不进捕获，监控页上表现就是「并发发了三个，结束后只剩两个」。
///
/// 所以状态放在这个结构里、收尾放在 `Drop` 里：正常跑完显式调一次 `finalize`，
/// 被丢弃时由析构补上，二者靠 `finished` 保证只发生一次。
struct StreamTail<F: FnOnce(StreamOutcome)> {
    start: Instant,
    outcome: StreamOutcome,
    acc: ContentAccumulator,
    decoder: Box<dyn StreamDecoder>,
    observer: Option<Box<dyn StreamObserver>>,
    passthrough: bool,
    raw_upstream: RawBuffer,
    raw_client: RawBuffer,
    on_finish: Option<F>,
    finished: bool,
    /// 上游这一段是否已经走完（正常结束或失败都已定性）。
    ///
    /// 之后即使 Body 被丢弃，也只是尾帧没发完，内容与用量都齐了 —— 按正常收尾记账，
    /// 不能冤成「客户端提前断开」。只有循环中途被丢弃才是真的断连。
    upstream_finished: bool,
}

impl<F: FnOnce(StreamOutcome)> StreamTail<F> {
    fn finalize(&mut self, aborted: bool) {
        if self.finished {
            return;
        }
        self.finished = true;

        if aborted {
            self.outcome.aborted = true;
            if self.outcome.error.is_none() {
                self.outcome.error = Some("客户端提前断开".into());
            }
        }

        // 断连时前面收到的内容与 usage 照样要算进去：这条请求真的花了上游的
        // token，不记账等于让账单悄悄少一笔。没收全就按已有的记。
        self.outcome.usage = self.decoder.usage();
        let accumulated = std::mem::take(&mut self.acc).finish();
        // 文本从**累加器**取而不是 `decoder.text()`：后者是上游原文，逐增量
        // 改写之后两者会不一致。捕获里的 `stream_text` 必须与 `response_content`
        // 一样，反映客户端实际看到（或本应看到）的内容。
        self.outcome.text = accumulated
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("");
        self.outcome.content = accumulated;
        self.outcome.raw_upstream = std::mem::take(&mut self.raw_upstream.bytes);
        self.outcome.raw_client = (!self.passthrough)
            .then(|| std::mem::take(&mut self.raw_client.bytes));
        self.outcome.raw_truncated = self.raw_upstream.truncated || self.raw_client.truncated;
        self.outcome.total = Some(self.start.elapsed());

        if let Some(o) = self.observer.as_mut() {
            o.on_finish(self.outcome.error.as_deref());
        }
        if let Some(f) = self.on_finish.take() {
            f(std::mem::take(&mut self.outcome));
        }
    }
}

impl<F: FnOnce(StreamOutcome)> Drop for StreamTail<F> {
    fn drop(&mut self) {
        // 正常路径已经显式收过尾，这里什么都不会发生。
        let aborted = !self.upstream_finished;
        self.finalize(aborted);
    }
}

/// 逐增量改写的便捷入口，只给测试用：生产路径统一走
/// [`translate_stream_with_rewrite`]（它多一个 `delta_rewrite` 参数）。
/// 二十多个流式用例都从这条路进来，给每个都补一个 `None` 只是噪音。
#[cfg(test)]
pub fn translate_stream_observed<F>(
    upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: Box<dyn StreamDecoder>,
    encoder: Option<Box<dyn StreamEncoder>>,
    timeouts: StreamTimeouts,
    observer: Option<Box<dyn StreamObserver>>,
    on_finish: F,
) -> Body
where
    F: FnOnce(StreamOutcome) + Send + 'static,
{
    translate_stream_with_rewrite(
        Instant::now(), upstream, decoder, encoder, timeouts, observer, None, None, on_finish,
    )
}

/// 与 [`translate_stream_observed`] 相同，额外支持逐增量改写。
///
/// `delta_rewrite` 为 `Some` 时调用方必须传 `encoder`（改写后的增量要重新编码，
/// 不能走直通）；否则钩子会被忽略，按普通流式处理。
///
/// `started` 是**请求到达网关**的时刻，TTFB 与时间轴都从它算起。不能在这里
/// 取 `Instant::now()`：走到这一步时上游的响应头已经回来了，而不少上游（含
/// Anthropic 官方）要等首个 token 生成出来才发响应头 —— 从这里起算，用户真正
/// 干等的那几秒到几分钟全落在计时之外，TTFT 只剩个位数毫秒。
#[allow(clippy::too_many_arguments)]
pub fn translate_stream_with_rewrite<F>(
    started: Instant,
    upstream: BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: Box<dyn StreamDecoder>,
    encoder: Option<Box<dyn StreamEncoder>>,
    timeouts: StreamTimeouts,
    observer: Option<Box<dyn StreamObserver>>,
    delta_rewrite: Option<StreamDeltaRewrite>,
    restart: Option<StreamRestart>,
    on_finish: F,
) -> Body
where
    F: FnOnce(StreamOutcome) + Send + 'static,
{
    let passthrough = encoder.is_none();
    // 没有编码器就无法把改写后的增量发出去。宁可按普通流式走，也不要把请求弄坏。
    let mut delta_rewrite = delta_rewrite.filter(|_| !passthrough);
    let mut encoder = encoder;
    let mut restart = restart;

    // **在生成器外构造**：Body 可能一次都没被 poll 就被丢掉（客户端收到响应头
    // 就断开）。放到生成器里面的话，那种情况下状态根本没建起来，既不会记账、
    // 也不会把「进行中」摘掉 —— 前端从此挂着一个永远不消失的幽灵。
    let mut tail = StreamTail {
        start: started,
        outcome: StreamOutcome::default(),
        acc: ContentAccumulator::new(),
        decoder,
        observer,
        passthrough,
        raw_upstream: RawBuffer::default(),
        raw_client: RawBuffer::default(),
        on_finish: Some(on_finish),
        finished: false,
        upstream_finished: false,
    };

    let stream = async_stream::stream! {
        let mut buf = String::new();
        let mut rem: Vec<u8> = Vec::new();
        let mut pinned = upstream;

        // 用户按了按钮时从这里出去。`None` = 循环是正常/异常结束的。
        let mut interrupted: Option<RequestSignal> = None;
        // 已经往下游送过语义没有。`message_start` 也算 —— 重来一遍会在客户端
        // 那边出现第二条 `message_start`，那是它接不住的。
        let mut saw_delta = false;

        loop {
            // 首个字节用更短的超时，之后用空闲超时。
            let timeout = if tail.outcome.first_byte.is_none() {
                timeouts.first_byte
            } else {
                timeouts.idle
            };

            let wait = tokio::time::timeout(timeout, pinned.next());
            // 先把控制句柄单独拎出来：`StreamRestart` 里那个 `FnOnce` 只能保证
            // `Send`、不保证 `Sync`，而 `&StreamRestart` 一旦跨过下面这个 await，
            // 整个生成器就不再是 `Send` 了，`Body::from_stream` 直接拒收。
            let ctl = restart.as_ref().map(|r| r.control.clone());
            let chunk = match match ctl {
                // 有控制句柄才多等一个信号。没有的话保持原来的单路等待，
                // 别为一个用不上的分支多一次 poll。
                Some(ctl) => tokio::select! {
                    biased;
                    _ = ctl.wait() => {
                        // 信号必须取走：留着的话下一轮立刻又命中同一次点击。
                        interrupted = Some(ctl.take().unwrap_or(RequestSignal::Cancel));
                        break;
                    }
                    r = wait => r,
                },
                None => wait.await,
            } {
                Err(_) => {
                    tail.outcome.error = Some(if tail.outcome.first_byte.is_none() {
                        "等待上游首字节超时".to_string()
                    } else {
                        "上游流空闲超时".to_string()
                    });
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(e))) => {
                    tail.outcome.error = Some(format!("读取上游流失败: {e}"));
                    break;
                }
                Ok(Some(Ok(chunk))) => chunk,
            };

            if tail.outcome.first_byte.is_none() {
                tail.outcome.first_byte = Some(tail.start.elapsed());
            }

            // 原始帧：上游侧无条件留一份。
            tail.raw_upstream.push(&chunk);

            // 直通模式：先把原始字节原样发给客户端。
            if tail.passthrough {
                yield Ok::<Bytes, std::io::Error>(chunk.clone());
            }

            // 无论哪种模式都要喂给解码器，用于统计用量与拼接文本。
            append_utf8_safe(&mut buf, &mut rem, &chunk);

            while let Some(block) = take_sse_block(&mut buf) {
                let Some(ev) = parse_event(&block) else { continue };
                // 注释帧、`retry:` 之类没有 `data:` 的块到不了这里 —— 它们不算
                // 首字。见 `StreamOutcome::ttft` 的注释。
                if tail.outcome.ttft.is_none() {
                    tail.outcome.ttft = Some(tail.start.elapsed());
                }
                tail.outcome.events += 1;

                let deltas = match tail.decoder.on_event(&ev) {
                    Ok(d) => d,
                    Err(e) => {
                        // 单个事件解析失败不该中断整条流 —— 记下来继续，
                        // 否则客户端的回答会被从中间截断。观察者照样收到这个块，
                        // 只是增量是空的：坏帧恰恰是排查时最该看见的。
                        tracing::warn!("流事件解析失败，原样转发给客户端: {e}");

                        // 解不出就**原样发给客户端**，而不是替它丢掉。
                        //
                        // 非标准事件解不出是常态（上游自定义的帧、不是 JSON 的
                        // `data`），而"解不出"只说明 Apilot 的语义化视图缺一块，
                        // 不说明这个事件没有内容 —— 丢了才是真把内容吞了。
                        // 客户端的 SSE 解析器本来就按事件名挑、认不出的跳过
                        // （Claude Code 与 Codex 都如此），多一帧不会把它弄坏。
                        //
                        // 只在**转码**这条路上补发：直通时这一个块的原始字节上面
                        // 已经发过了，再发一次就是重复。补上 `\n\n` 是因为
                        // `take_sse_block` 只回块内容，不回上游用的是哪种换行。
                        if !tail.passthrough {
                            let bytes = Bytes::from(format!("{block}\n\n"));
                            tail.raw_client.push(&bytes);
                            yield Ok::<Bytes, std::io::Error>(bytes);
                        }
                        Vec::new()
                    }
                };

                // 逐增量改写放在观察者 / 累加器 / 编码器**之前**：三者必须看到
                // 同一份增量，否则实时视图、捕获内容与客户端收到的会互相矛盾。
                let deltas = apply_delta_rewrite(deltas, &mut delta_rewrite);
                // 只要解出过一条增量，下游就已经拿到了有语义的东西。
                if !deltas.is_empty() {
                    saw_delta = true;
                }

                if let Some(o) = tail.observer.as_mut() {
                    o.on_event(&block, &deltas);
                }

                // 时间轴用的时间点。记在这里而不是记在观察者里：`StreamOutcome`
                // 是要落库的，观察者只管实时推送，两边各记各的会跑出两份不一致的
                // 时间。放在观察者之后是为了让"推给前端"这件事先发生 ——
                // 名字推导要遍历原文，不能让排查用的东西拖慢实时视图。
                tail.outcome.timings.push(
                    tail.start.elapsed().as_millis() as i64,
                    &timeline_name(&block, &deltas),
                );

                for d in &deltas {
                    if let UnifiedDelta::Finish(r) = d {
                        tail.outcome.finish_reason = Some(r.clone());
                    }
                    tail.acc.apply(d);
                }

                if !tail.passthrough {
                    for d in &deltas {
                        if let Some(enc) = encoder.as_mut() {
                            for out in enc.on_delta(d) {
                                let bytes = encode_event(&out);
                                tail.raw_client.push(&bytes);
                                yield Ok::<Bytes, std::io::Error>(bytes);
                            }
                        }
                    }
                }
            }
        }

        // ---- 用户按了「中断」/「中断并重试」 ----
        if let Some(signal) = interrupted {
            // 能不能把新一次请求接到这条流上？前提是下游还没拿到任何有语义的
            // 东西，**而且**已经发出去的字节正好停在 SSE 块的边界上。
            //
            // 后半句不能省：直通模式是"先把原始字节转给客户端、再回头解析"，
            // 一块被 TCP 拆成两半的 `message_start` 完全可能已经发出去了 ——
            // 此时再接一条新流，客户端的解析器会把"半截旧帧 + 新流的开头"
            // 拼成一条垃圾事件。`buf` / `rem` 正好就是那两个"半截"。
            let can_restart = matches!(signal, RequestSignal::Retry)
                && !saw_delta
                && buf.is_empty()
                && rem.is_empty()
                && restart.is_some();

            tail.outcome.error = Some(if can_restart {
                "已手动中断并重试".to_string()
            } else {
                "已手动中断".to_string()
            });

            // 不重试时补一帧 `error`：客户端才知道这条响应是**被结束**的，
            // 而不是上游吐到一半断了。直通那条路不能补（客户端收到的本来就是
            // 上游原始字节，硬塞一帧别的协议的错误反而可能把它弄坏）——
            // 与下面 `broken` 那一段是同一个取舍。
            if !can_restart && !tail.passthrough {
                if let Some(enc) = encoder.as_mut() {
                    let events = enc.on_delta(&UnifiedDelta::Error {
                        code: "api_error".into(),
                        message: "已手动中断：这条响应被用户在监控页结束".into(),
                    });
                    for out in events {
                        let bytes = encode_event(&out);
                        tail.raw_client.push(&bytes);
                        yield Ok::<Bytes, std::io::Error>(bytes);
                    }
                }
            }

            // 用户按的按钮既不是上游抽风也不是客户端断连：按 499 记，原因写成人话。
            // `finalize(true)` 之后 `Drop` 不会再收一次尾（`finished` 已置位）。
            tail.finalize(true);

            if can_restart {
                if let Some(r) = restart.take() {
                    if let Some(body) = (r.make)().await {
                        let mut data = body.into_data_stream();
                        while let Some(chunk) = data.next().await {
                            match chunk {
                                Ok(b) => yield Ok::<Bytes, std::io::Error>(b),
                                // 新流自己坏了就到此为止：旧那条已经结完账，
                                // 这条下游流也没有第二套账可记。
                                Err(_) => break,
                            }
                        }
                    }
                }
            }
            return;
        }

        // 上游这一段已经走完：内容与用量都齐了。标记放在这里（而不是收尾之后），
        // 是为了让"尾帧还没发完客户端就断了"仍然按正常请求记账。
        tail.upstream_finished = true;

        // 收尾。**断流与正常结束必须分开走**：解码器的 `finish()` 是给"上游干脆
        // 没发终止帧"用的兜底（补一个 `end_turn`），而超时/读取失败是**失败** ——
        // 照旧调它会发出一个语法完整、内容却半截的"成功"回复，客户端于是把截断
        // 当成回答完了（不重试、不报错、什么都不说）。用户看到的是一条莫名断掉
        // 的回复，而监控里这一栏还写着成功。
        let broken = tail.outcome.error.is_some();

        if !broken {
            // 收尾补出来的增量也要过脚本：否则客户端收到的流里，最后那帧
            // （终止原因等）会绕过改写，出现"前面的都改了、最后一帧没改"。
            let finished = apply_delta_rewrite(tail.decoder.finish(), &mut delta_rewrite);
            for d in finished {
                if let UnifiedDelta::Finish(r) = &d {
                    tail.outcome.finish_reason = Some(r.clone());
                }
                tail.acc.apply(&d);
                if !tail.passthrough {
                    if let Some(enc) = encoder.as_mut() {
                        for out in enc.on_delta(&d) {
                            let bytes = encode_event(&out);
                            tail.raw_client.push(&bytes);
                            yield Ok::<Bytes, std::io::Error>(bytes);
                        }
                    }
                }
            }
        }

        // 直通那条路**不能补发 error 帧**：客户端收到的本来就是上游的原始字节，
        // 在末尾硬塞一帧别的协议的 error，反而可能把它弄坏。代价是客户端拿不到
        // 我们的说明 —— 用带状态码的那句话补上一部分：上游拒绝时的码与报文都
        // 原样在它手里，能自己判断要不要重试。监控里这一栏照旧标着失败原因。
        if !tail.passthrough {
            if let Some(enc) = encoder.as_mut() {
                // 失败的流**不发**终止帧：`error` 之后再来一个 `message_stop`
                // 就等于又说了一遍"正常结束"，正好抵消掉上面那句话。
                let events = if broken {
                    let message = stream_break_message(tail.outcome.error.as_deref().unwrap_or(""));
                    enc.on_delta(&UnifiedDelta::Error {
                        code: "api_error".into(),
                        message,
                    })
                } else {
                    enc.finish()
                };
                for out in events {
                    let bytes = encode_event(&out);
                    tail.raw_client.push(&bytes);
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
            }
        }

        tail.finalize(false);
    };

    Body::from_stream(stream)
}

/// 把一份完整响应拆成 IR 增量序列，供编码器合成 SSE。
///
/// 缓存命中与「非流式响应假装成流」两条路共用同一份拆法 —— 两处一旦分叉，
/// 客户端看到的流形态就会因为"命中缓存 / 上游没用流"而不一样。
fn response_to_deltas(response: &UnifiedResponse, usage: &UnifiedUsage) -> Vec<UnifiedDelta> {
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
            crate::protocol::dto::ContentBlock::Thinking { text, signature } => {
                // 思考块必须先 BlockStart，否则编码器的 `ensure_block_open`
                // 会按默认的文本块开——客户端看到的思考块就变成了正文。
                deltas.push(UnifiedDelta::BlockStart {
                    index,
                    block: crate::protocol::dto::ContentBlock::Thinking {
                        text: String::new(),
                        signature: None,
                    },
                });
                deltas.push(UnifiedDelta::ThinkingDelta { index, text: text.clone() });
                // 同协议重编码必须把签名原样带回去，否则客户端下一轮会被上游拒收。
                if let Some(sig) = signature {
                    deltas.push(UnifiedDelta::ThinkingSignature {
                        index,
                        signature: sig.clone(),
                    });
                }
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
    deltas
}

/// 从一份已完成的响应合成 SSE 流。
///
/// 用于缓存命中：客户端要流式，但结果是现成的，于是用编码器把它"假装"成流。
/// 这样客户端拿到的响应形态与真实调用一致，不需要它区分是否命中缓存。
/// 重放缓存的收尾守卫。与 [`StreamTail`] 同一个理由：消费者可能不把这条流
/// 读完就丢掉（缓存重放几乎是瞬间结束，客户端常常在最后一帧之后立刻关连接），
/// 收尾写在生成器末尾就会漏掉这一次请求的记账。
struct ReplayTail<F: FnOnce(StreamOutcome)> {
    start: Instant,
    outcome: StreamOutcome,
    on_finish: Option<F>,
    finished: bool,
}

impl<F: FnOnce(StreamOutcome)> ReplayTail<F> {
    fn finalize(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let elapsed = self.start.elapsed();
        // 缓存重放时"首个事件"是瞬间合成的，与整体耗时同义。
        self.outcome.ttft = Some(elapsed);
        self.outcome.first_byte = Some(elapsed);
        self.outcome.total = Some(elapsed);
        if let Some(f) = self.on_finish.take() {
            f(std::mem::take(&mut self.outcome));
        }
    }
}

impl<F: FnOnce(StreamOutcome)> Drop for ReplayTail<F> {
    fn drop(&mut self) {
        self.finalize();
    }
}

pub fn stream_from_response<F>(
    response: UnifiedResponse,
    usage: UnifiedUsage,
    mut encoder: Box<dyn StreamEncoder>,
    on_finish: F,
) -> Body
where
    F: FnOnce(StreamOutcome) + Send + 'static,
{
    let deltas = response_to_deltas(&response, &usage);

    // 与 `translate_stream_with_rewrite` 同理：在生成器外构造，Body 一次都
    // 没被 poll 就被丢掉时也能收尾。
    let mut tail = ReplayTail {
        start: Instant::now(),
        outcome: StreamOutcome {
            finish_reason: Some(response.finish_reason.clone()),
            text: response.concat_text(),
            content: response.content.clone(),
            usage,
            events: deltas.len() as u64,
            ..Default::default()
        },
        on_finish: Some(on_finish),
        finished: false,
    };

    let stream = async_stream::stream! {
        for d in &deltas {
            for out in encoder.on_delta(d) {
                yield Ok::<Bytes, std::io::Error>(encode_event(&out));
            }
        }
        for out in encoder.finish() {
            yield Ok::<Bytes, std::io::Error>(encode_event(&out));
        }

        tail.finalize();
    };

    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::sse::done_event;
    use crate::protocol::anthropic::{AnthropicStreamDecoder, AnthropicStreamEncoder};
    use crate::protocol::oai_chat::{ChatStreamDecoder, ChatStreamEncoder};

    /// 把若干字节块做成上游流。
    fn upstream(chunks: Vec<&'static str>) -> BoxStream<'static, reqwest::Result<Bytes>> {
        Box::pin(futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c.as_bytes()))),
        ))
    }

    /// 每帧之间隔一段时间的流，用来验证"等待落在计时里"。
    fn upstream_with_delay(
        chunks: Vec<&'static str>,
        gap: Duration,
    ) -> BoxStream<'static, reqwest::Result<Bytes>> {
        Box::pin(async_stream::stream! {
            for c in chunks {
                tokio::time::sleep(gap).await;
                yield Ok(Bytes::from_static(c.as_bytes()));
            }
        })
    }

    /// 发完这几帧就再也不产出的流。配合极短的 `idle` 超时，它就是"上游断了"。
    ///
    /// 不用 `Err(reqwest::Error)` 来造：那个类型没有公开的构造入口（字段全私有，
    /// `From` 实现也在 crate 内部）。走超时这条路既容易构造，又正是用户真正会
    /// 遇到的形状 —— 上游既不回数据、也不关连接。
    fn hanging_upstream(chunks: Vec<&'static str>) -> BoxStream<'static, reqwest::Result<Bytes>> {
        let head = chunks
            .into_iter()
            .map(|c| Ok(Bytes::from_static(c.as_bytes())));
        Box::pin(futures::stream::iter(head).chain(futures::stream::pending()))
    }

    /// 很快判超时的配置。
    fn impatient() -> StreamTimeouts {
        StreamTimeouts {
            first_byte: Duration::from_millis(200),
            idle: Duration::from_millis(50),
        }
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
    async fn a_broken_stream_ends_in_an_error_not_a_normal_finish() {
        // 回归：上游断流（超时/读取失败）以前照样走 `finish()`，客户端于是收到
        // 一条语法完整、内容却是半截的"成功"回复 —— 它不重试也不报错，用户只看到
        // 回答莫名断掉。转码这条路必须改成发 error 帧，且**不能再发终止帧**：
        // `error` 后面跟一个 `message_stop` 等于又说了一遍"正常结束"。
        let chunks = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"半截\"}}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            hanging_upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(AnthropicStreamEncoder::new())),
            impatient(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let bytes = run(body).await;
        let text = String::from_utf8_lossy(&bytes);

        assert!(text.contains("event: error"), "断流必须以 error 帧收尾: {text}");
        assert!(
            !text.contains("message_stop"),
            "失败的流不该再发终止帧，否则等于又说了一遍正常结束: {text}"
        );
        assert!(text.contains("上游流中断"), "要说清是我们这边看到的失败: {text}");

        let o = rx.await.unwrap();
        assert!(o.error.is_some());
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_stop_event_still_finishes_normally() {
        // 与上一条对照：上游**没报错**、只是没发终止帧时，兜底的 `finish()` 照旧
        // 有效。把这两种情况混起来会让"上游干脆省略了 stop_reason"的正常流
        // 被当失败，客户端于是无端重试。
        let chunks = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"完整\"}}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(AnthropicStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            });
        let text = String::from_utf8_lossy(&run(body).await).to_string();

        assert!(!text.contains("event: error"));
        assert!(text.contains("message_stop"), "正常结束仍要有终止帧: {text}");
        let o = rx.await.unwrap();
        assert!(o.error.is_none());
    }

    #[test]
    fn the_accumulator_keeps_thinking_signatures() {
        // 缓冲改写与缓存重放都靠累加器重建响应；签名丢了，客户端下一轮
        // 把思考块回传给 Anthropic 时会被拒。
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::ThinkingDelta {
            index: 0,
            text: "想".into(),
        });
        acc.apply(&UnifiedDelta::ThinkingSignature {
            index: 0,
            signature: "sig-1".into(),
        });

        let blocks = acc.finish();
        assert!(matches!(
            &blocks[0],
            crate::protocol::dto::ContentBlock::Thinking {
                signature: Some(s),
                ..
            } if s == "sig-1"
        ));
    }

    #[tokio::test]
    async fn stream_from_response_keeps_thinking_signatures() {
        use crate::protocol::dto::ContentBlock;

        let resp = UnifiedResponse {
            id: "r".into(),
            model: "m".into(),
            content: vec![ContentBlock::Thinking {
                text: "想".into(),
                signature: Some("sig-1".into()),
            }],
            finish_reason: FinishReason::Stop,
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = stream_from_response(
            resp,
            UnifiedUsage::default(),
            Box::new(AnthropicStreamEncoder::new()),
            move |o| {
                let _ = tx.send(o);
            },
        );
        let text = String::from_utf8_lossy(&run(body).await).to_string();

        assert!(text.contains("thinking_delta"), "思考正文要发出去: {text}");
        assert!(
            text.contains("signature_delta") && text.contains("sig-1"),
            "同协议重放必须带回签名: {text}"
        );

        let o = rx.await.unwrap();
        assert!(matches!(
            o.content.first(),
            Some(ContentBlock::Thinking {
                signature: Some(s),
                ..
            }) if s == "sig-1"
        ));
    }

    #[tokio::test]
    async fn a_delta_rewrite_is_applied_before_the_client_sees_it() {
        // 逐增量改写必须"边收边改边发"：这里不测时序，只钉住客户端拿到的
        // 和捕获记录里存的都是改写后的内容，原始文本一个字都不漏。
        let chunks = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\",\"usage\":{\"input_tokens\":3}}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"secret\"}}\n\n",
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" tail\"}}\n\n",
            "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];

        let rewrite = StreamDeltaRewrite {
            apply: Box::new(|d| match d {
                UnifiedDelta::TextDelta { index, text } => {
                    Some(vec![UnifiedDelta::TextDelta {
                        index: *index,
                        text: text.replace("secret", "[redacted]"),
                    }])
                }
                _ => None,
            }),
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            Instant::now(),
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(AnthropicStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            Some(rewrite),
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );
        let text = String::from_utf8_lossy(&run(body).await).to_string();

        assert!(
            text.contains("[redacted]") && text.contains(" tail"),
            "客户端应当收到改写后的两个增量: {text}"
        );
        assert!(
            !text.contains("secret"),
            "原始文本不能在改写前漏给客户端: {text}"
        );

        let o = rx.await.unwrap();
        assert_eq!(o.text, "[redacted] tail");
        assert!(
            o.raw_client
                .as_deref()
                .is_some_and(|b| String::from_utf8_lossy(b).contains("[redacted]")),
            "客户端侧捕获必须是实际发出去的流"
        );
        assert!(o.error.is_none());
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
        assert!(outcome.ttft.is_some());
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

    /// 解不出的事件照样要送到客户端 —— **只在转码**这条路上补发。
    ///
    /// 上游发了个 `data` 不是 JSON 的事件（自定义帧之类），解码器在最上面那层
    /// 就失败了。以前它被无声跳过：客户端凭空空掉一段，而"空掉的那段"恰恰是
    /// 排查时最该看见的东西。
    #[tokio::test]
    async fn an_undecodable_event_is_forwarded_verbatim_when_transcoding() {
        let chunks = vec![
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
            "event: content_block_delta\ndata: <not json at all>\n\n",
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
            },
        );
        let bytes = run(body).await;
        let client = String::from_utf8_lossy(&bytes);

        assert_eq!(
            client.matches("<not json at all>").count(),
            1,
            "解不出的事件要原样发一次给客户端，不能替它丢掉"
        );
        assert_eq!(
            rx.await.unwrap().raw_client.as_deref(),
            Some(bytes.as_slice()),
            "转发出去的字节同样算客户端侧原始帧"
        );
    }

    /// 直通时这一帧的原始字节上面已经发过，不能补发第二次。
    #[tokio::test]
    async fn an_undecodable_event_is_not_duplicated_when_passing_through() {
        let chunks = vec!["event: content_block_delta\ndata: <not json at all>\n\n"];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None, // 直通
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );
        let bytes = run(body).await;

        assert_eq!(
            String::from_utf8_lossy(&bytes)
                .matches("<not json at all>")
                .count(),
            1,
            "直通时原始字节只该出现一次"
        );
        assert!(rx.await.unwrap().raw_client.is_none());
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
            move |o| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(!o.aborted, "正常跑完不能标成断连");
            });
        run(body).await;
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// 客户端收完 `[DONE]` 就关连接时，hyper 会在最后一个 `yield` 之后直接丢掉
    /// Body —— 生成器末尾的收尾代码一行都轮不上。那正是「并发发三个、结束后
    /// 只剩两个」的现场：这一条请求既不在日志里，也不在捕获里。
    #[tokio::test]
    async fn dropping_the_body_mid_stream_still_finalizes_the_request() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            // 发完这一帧就不再产出：收尾一定不会自己发生。
            hanging_upstream(vec![
                "data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"半\"}}]}\n\n",
            ]),
            Box::new(ChatStreamDecoder::new()),
            // 走转码：直通会在处理这一帧之前就把它 yield 出去，测试拿到的
            // 时刻解码器还没见到内容；转码则保证"客户端看到的"已经处理过。
            Some(Box::new(ChatStreamEncoder::new())),
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );

        let mut data = body.into_data_stream();
        assert!(data.next().await.is_some(), "先拿到那一帧");
        drop(data);

        let o = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("丢 Body 之后必须补上收尾，否则这条请求就丢了")
            .unwrap();
        assert!(o.aborted);
        assert_eq!(o.text, "半", "已经收到的内容照样要记进捕获");
        assert!(
            o.error.as_deref().is_some_and(|e| e.contains("客户端")),
            "断连的归因要说清是谁断的"
        );
    }

    /// 连一次都没被 poll 的 Body 也要收尾：客户端收到响应头就断开时就是这种形状。
    /// 少了它，「进行中」里会留下一条永远不会被摘掉的幽灵。
    #[tokio::test]
    async fn dropping_the_body_before_it_is_polled_still_finalizes_the_request() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_observed(
            upstream(vec!["data: [DONE]\n\n"]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );

        drop(body);

        let o = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("未 poll 就被丢掉的 Body 也必须收尾")
            .unwrap();
        assert!(o.aborted);
    }

    /// 缓存重放同理：客户端在最后一帧之后立刻关连接，也要把这笔账记上。
    #[tokio::test]
    async fn dropping_a_cache_replay_early_still_finalizes() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let resp = UnifiedResponse {
            id: "r".into(),
            model: "m".into(),
            content: vec![crate::protocol::dto::ContentBlock::text("缓存的答案")],
            finish_reason: FinishReason::Stop,
        };
        let body = stream_from_response(
            resp,
            UnifiedUsage::default(),
            Box::new(AnthropicStreamEncoder::new()),
            move |o| {
                let _ = tx.send(o);
            },
        );

        // 读一帧就丢，模拟客户端收完就关连接。
        let mut data = body.into_data_stream();
        assert!(data.next().await.is_some());
        drop(data);

        let o = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("重放被丢掉也要收尾")
            .unwrap();
        assert_eq!(o.text, "缓存的答案");
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
    async fn ttft_counts_from_request_arrival_not_from_response_headers() {
        // 回归：计时曾在拿到上游响应头之后才开始，而 Anthropic 等上游要等首个
        // token 出来才回响应头 —— 用户干等几十秒，监控里的 TTFT 却只有几毫秒。
        // 这里用「请求 300ms 前就到了」模拟那段等在响应头之前的时间。
        let started = Instant::now() - Duration::from_millis(300);
        let chunks = vec![
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            started,
            upstream(chunks),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            None,
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );
        run(body).await;

        let o = rx.await.unwrap();
        let ttft = o.ttft.expect("收到了事件就该有 TTFT");
        assert!(ttft >= Duration::from_millis(300), "TTFT 必须包含响应头之前的等待: {ttft:?}");
        assert!(o.total.unwrap() >= ttft);
        assert!(
            o.timings.at_ms.first().is_some_and(|&ms| ms >= 300),
            "时间轴与 TTFT 同一个起点，否则首帧的等待在时间轴上也看不见"
        );
    }

    /// 上游先发纯注释帧保活、很久之后才吐第一个真事件。
    ///
    /// 回归：TTFT 曾量到「首个字节」，于是一串 `: ping` 就能让"干等 30 秒"
    /// 显示成"3 毫秒" —— 与用户在实时视图里看到的等待完全对不上。
    /// 注释帧在 SSE 里是给客户端忽略的，不是用户能看见的任何东西。
    #[tokio::test]
    async fn ttft_anchors_on_the_first_parseable_event_not_on_keepalive_bytes() {
        let started = Instant::now();
        let chunks = vec![
            ": keepalive\n\n",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
        ];

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            started,
            upstream_with_delay(chunks, Duration::from_millis(150)),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts::default(),
            None,
            None,
            None,
            move |o| {
                let _ = tx.send(o);
            },
        );
        run(body).await;

        let o = rx.await.unwrap();
        let first_byte = o.first_byte.expect("注释帧也是字节");
        let ttft = o.ttft.expect("真事件到达后就有 TTFT");
        assert!(
            first_byte < Duration::from_millis(200),
            "首字节是那条注释帧: {first_byte:?}"
        );
        assert!(
            ttft >= first_byte + Duration::from_millis(100),
            "TTFT 必须跳过注释帧，量到第一个可解析事件: first_byte={first_byte:?} ttft={ttft:?}"
        );
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
        assert!(o.ttft.is_none(), "没有任何事件就没有 TTFT");
        assert!(o.first_byte.is_none(), "没有任何字节就没有首字节时刻");
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
            UnifiedDelta::ThinkingSignature {
                index: 0,
                signature: "sig".into(),
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
    /// 「中断并重试」：下游还没拿到任何有语义的东西时，把新一次请求的字节
    /// 接到**同一条**下游流上。
    ///
    /// 上游"只发心跳然后干等"正是用户在实时视图里干看 250 秒的那种形状。
    #[tokio::test]
    async fn a_retry_before_any_delta_resumes_on_the_new_stream() {
        let control = Arc::new(RequestControl::new());
        {
            let control = control.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                control.request(RequestSignal::Retry);
            });
        }

        let made = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            Instant::now(),
            hanging_upstream(vec![": keepalive\n\n"]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts {
                first_byte: Duration::from_millis(500),
                idle: Duration::from_millis(500),
            },
            None,
            None,
            Some(StreamRestart {
                control: control.clone(),
                make: Box::new({
                    let made = made.clone();
                    move || {
                        Box::pin(async move {
                            made.store(true, std::sync::atomic::Ordering::SeqCst);
                            Some(Body::from(
                                "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"新的内容\"}}\n\n",
                            ))
                        })
                    }
                }),
            }),
            move |o| {
                let _ = tx.send(o);
            },
        );

        let out = String::from_utf8_lossy(&run(body).await).to_string();
        assert!(made.load(std::sync::atomic::Ordering::SeqCst), "重试没有被执行");
        assert!(out.contains("keepalive"), "已经发出去的心跳不能被吞掉: {out}");
        assert!(out.contains("新的内容"), "新一次请求的内容必须接到同一条流上: {out}");

        let o = rx.await.unwrap();
        assert!(o.aborted, "人为中止不是上游失败");
        assert_eq!(o.error.as_deref(), Some("已手动中断并重试"));
        // 旧那条的账要干净：一个事件都没有。
        assert_eq!(o.events, 0);
    }

    /// 只按「中断」：非直通那条路要补一帧 `error` 再收尾 —— 客户端得知道这是
    /// 被结束的，而不是上游吐到一半断了。
    #[tokio::test]
    async fn a_plain_cancel_ends_with_an_error_frame_and_no_terminal_frame() {
        let control = Arc::new(RequestControl::new());
        {
            let control = control.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                control.request(RequestSignal::Cancel);
            });
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            Instant::now(),
            hanging_upstream(vec![
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            ]),
            Box::new(AnthropicStreamDecoder::new()),
            Some(Box::new(AnthropicStreamEncoder::new())),
            StreamTimeouts {
                first_byte: Duration::from_millis(500),
                idle: Duration::from_millis(500),
            },
            None,
            None,
            Some(StreamRestart {
                control: control.clone(),
                // 中断不该重试，这个闭包一次都不该被调用。
                make: Box::new(|| Box::pin(async { None })),
            }),
            move |o| {
                let _ = tx.send(o);
            },
        );

        let out = String::from_utf8_lossy(&run(body).await).to_string();
        assert!(out.contains("event: error"), "客户端要收到明确的错误帧: {out}");
        assert!(out.contains("已手动中断"), "错误帧里要说清是被手动结束的: {out}");
        assert!(!out.contains("message_stop"), "error 之后不能再补终止帧: {out}");

        let o = rx.await.unwrap();
        assert!(o.aborted);
        assert_eq!(o.error.as_deref(), Some("已手动中断"));
    }

    /// 已经往下游送过语义之后就不能再"重试"了 —— 再来一遍会在客户端那边
    /// 出现第二条 `message_start`。这时按「中断」处理：直接结束这条流。
    #[tokio::test]
    async fn a_retry_after_content_started_ends_the_stream_instead() {
        let control = Arc::new(RequestControl::new());
        {
            let control = control.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                control.request(RequestSignal::Retry);
            });
        }

        let made = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = translate_stream_with_rewrite(
            Instant::now(),
            hanging_upstream(vec![
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n",
            ]),
            Box::new(AnthropicStreamDecoder::new()),
            None,
            StreamTimeouts {
                first_byte: Duration::from_millis(500),
                idle: Duration::from_millis(500),
            },
            None,
            None,
            Some(StreamRestart {
                control: control.clone(),
                make: Box::new({
                    let made = made.clone();
                    move || {
                        Box::pin(async move {
                            made.store(true, std::sync::atomic::Ordering::SeqCst);
                            None
                        })
                    }
                }),
            }),
            move |o| {
                let _ = tx.send(o);
            },
        );

        let out = String::from_utf8_lossy(&run(body).await).to_string();
        assert!(!made.load(std::sync::atomic::Ordering::SeqCst), "出过内容就不能再重试");
        assert!(out.contains("message_start"), "已经发出去的内容不能少: {out}");

        let o = rx.await.unwrap();
        assert!(o.aborted);
        assert_eq!(o.error.as_deref(), Some("已手动中断"));
    }
}
