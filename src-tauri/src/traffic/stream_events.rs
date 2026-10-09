//! 流式请求的实时事件推送。
//!
//! 为什么需要它：流式请求**在整条流结束前不落库**（`pipeline::finalize_stream`
//! 只在收尾时写日志），转发过程中数据库里没有行、前端也收不到任何东西 ——
//! 用户对着空列表无从判断请求是卡住了还是还在跑。
//!
//! 三条约束决定了这里的设计：
//! 1. **不能逐帧 emit**。一个长回答轻松上千个 SSE 事件，逐个推会把 IPC 通道
//!    打满，前端反而卡死（`traffic/events.rs` 顶部那段注释说的是同一件事）。
//! 2. **不能丢帧**。现有的 `Throttle` 是"窗口期内只发一次"，用来推流量快照没问题，
//!    用在事件流上就是随机吞掉内容。所以改成攒批：攒够一批或等够时间就走一次。
//! 3. **断连也必须收尾**。客户端中途断开时，转发生成器在 `yield` 点被丢弃，
//!    `on_finish` 根本不会执行 —— 只能靠 `Drop` 兜底，否则"进行中"永远清不掉。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::gateway::stream::StreamObserver;
use crate::protocol::dto::{
    ContentBlock, FinishReason, UnifiedDelta, UsageDelta,
};

/// 攒批与截断的上限。
#[derive(Debug, Clone)]
pub struct BatchCaps {
    /// 距上次发送超过它就发一批（即使没攒够条数）。
    pub flush_interval: Duration,
    /// 单个请求最多推多少帧。超了就置 `truncated` 并停止推送。
    pub max_frames_per_request: u64,
    /// 一批最多带多少帧。
    pub max_frames_per_batch: usize,
    /// 单帧原文的上限（字节）。
    pub max_raw_frame_bytes: usize,
}

impl Default for BatchCaps {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_millis(120),
            max_frames_per_request: 2000,
            max_frames_per_batch: 32,
            max_raw_frame_bytes: 8 * 1024,
        }
    }
}

/// 推给前端的一帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFrame {
    /// 从 1 开始的序号，同一请求内单调递增。前端用它去重与排序。
    pub seq: u64,
    /// 相对流开始的毫秒数。
    pub at_ms: i64,
    /// 上游原始 SSE 块的原文（可能已截断）。
    pub raw: String,
    pub raw_truncated: bool,
    /// 该帧解码出的 IR 增量。解析失败或非内容事件时为空。
    pub deltas: Vec<StreamDelta>,
}

/// 推给前端的增量形态。
///
/// **为什么不直接序列化 `UnifiedDelta`**：它用的是内部标签（`#[serde(tag = "type")]`），
/// 而 `Finish(FinishReason)` 这种 newtype 变体里包着的又是个同样用 `type` 作标签的
/// 枚举 —— 两者会拼出 `{"type":"finish","type":"tool_use"}` 这种**重复键**的 JSON。
/// `JSON.parse` 只留最后一个，前端于是彻底看不到"这是一帧 finish"。
///
/// 与其去改 IR 的序列化（它服务的是编码器，不该为展示让路），不如给展示单独一个
/// 类型：标签叫 `kind`，嵌套的 `FinishReason` 放进字段里，两边互不干扰。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamDelta {
    MessageStart { id: String, model: String },
    BlockStart { index: u32, block: ContentBlock },
    Text { index: u32, text: String },
    Thinking { index: u32, text: String },
    ToolInput { index: u32, partial_json: String },
    BlockStop { index: u32 },
    Usage(UsageDelta),
    Finish { reason: FinishReason },
    Error { code: String, message: String },
}

impl From<&UnifiedDelta> for StreamDelta {
    fn from(d: &UnifiedDelta) -> Self {
        match d {
            UnifiedDelta::MessageStart { id, model } => Self::MessageStart {
                id: id.clone(),
                model: model.clone(),
            },
            UnifiedDelta::BlockStart { index, block } => Self::BlockStart {
                index: *index,
                block: block.clone(),
            },
            UnifiedDelta::TextDelta { index, text } => Self::Text {
                index: *index,
                text: text.clone(),
            },
            UnifiedDelta::ThinkingDelta { index, text } => Self::Thinking {
                index: *index,
                text: text.clone(),
            },
            UnifiedDelta::ToolInputDelta {
                index,
                partial_json,
            } => Self::ToolInput {
                index: *index,
                partial_json: partial_json.clone(),
            },
            UnifiedDelta::BlockStop { index } => Self::BlockStop { index: *index },
            UnifiedDelta::Usage(u) => Self::Usage(u.clone()),
            UnifiedDelta::Finish(r) => Self::Finish { reason: r.clone() },
            UnifiedDelta::Error { code, message } => Self::Error {
                code: code.clone(),
                message: message.clone(),
            },
        }
    }
}

/// 一批流事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    pub request_id: String,
    pub frames: Vec<StreamFrame>,
    /// 这个请求的流已经结束了，前端据此把它从「进行中」移走。
    pub done: bool,
    /// 因为超过每请求帧上限，后面的帧不再推送。
    pub truncated: bool,
    /// 结束时的错误（含客户端断连）。
    pub error: Option<String>,
}

/// 事件出口。
///
/// 抽成 trait 是为了能测：单测里造不出 `AppHandle`，而攒批/截断/断连兜底
/// 这些逻辑恰恰是最需要测的部分。真实实现见 `EventBus`。
pub trait StreamEventSink: Send + Sync {
    fn emit(&self, event: &StreamEvent);
}

/// 请求开始。前端据此把一条记录放进「进行中」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestStarted {
    pub request_id: String,
    pub ts: i64,
    pub client: String,
    /// 生效模型（计费口径）。
    pub model: String,
    /// 客户端请求的原始名字。与 `model` 不同时，界面要把它一并露出来 ——
    /// 这条请求还在跑的时候，正是最需要看清"我发的 A 现在跑在 B 上"的时刻。
    pub request_model: String,
    pub path: String,
    pub protocol_in: String,
    /// 主渠道 tag。
    pub provider_tag: String,
    /// 渠道显示名称，供进行中列表直接渲染用，不依赖前端再查一次。
    pub provider_name: String,
    /// 请求将发往的上游 URL（endpoint_for 的结果），进行中列表用。
    pub upstream_url: String,
    pub is_stream: bool,
}

/// 请求结束。与 `apilot://request` 分开：那个走的 `EventBus::request`
/// 带节流且会合并负载，做不了「结束」这种必须逐条送达的信号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFinished {
    pub request_id: String,
    pub status_code: i32,
    pub error: Option<String>,
}

/// 攒帧、决定何时吐一批、封顶、逐帧截断。**纯逻辑**，不碰任何 I/O。
pub struct StreamBatcher {
    caps: BatchCaps,
    started: Instant,
    last_flush: Instant,
    /// 已收到的帧数（含被丢弃的）。
    seq: u64,
    pending: Vec<StreamFrame>,
    /// 是否已越过每请求帧上限。
    pub truncated: bool,
}

impl StreamBatcher {
    pub fn new(caps: BatchCaps) -> Self {
        let now = Instant::now();
        Self {
            caps,
            started: now,
            last_flush: now,
            seq: 0,
            pending: Vec::new(),
            truncated: false,
        }
    }

    /// 收一帧。攒够了就返回这一批，否则返回 `None`。
    pub fn push(&mut self, raw: &str, deltas: &[UnifiedDelta]) -> Option<Vec<StreamFrame>> {
        if self.seq >= self.caps.max_frames_per_request {
            // 到顶了不再攒：缓冲保持不变，调用方下次 flush 会带上 truncated 标记。
            self.truncated = true;
            return None;
        }
        self.seq += 1;

        let (raw, raw_truncated) = truncate_raw(raw, self.caps.max_raw_frame_bytes);
        self.pending.push(StreamFrame {
            seq: self.seq,
            at_ms: self.started.elapsed().as_millis() as i64,
            raw,
            raw_truncated,
            deltas: deltas.iter().map(StreamDelta::from).collect(),
        });

        if self.pending.len() >= self.caps.max_frames_per_batch
            || self.last_flush.elapsed() >= self.caps.flush_interval
        {
            return Some(self.flush());
        }
        None
    }

    /// 把攒着的帧吐出来并清空。收尾与断连时调用。
    pub fn flush(&mut self) -> Vec<StreamFrame> {
        self.last_flush = Instant::now();
        std::mem::take(&mut self.pending)
    }
}

/// 按字节上限截断单帧，且**不切断多字节字符**（否则前端收到的 JSON 就废了）。
fn truncate_raw(raw: &str, limit: usize) -> (String, bool) {
    if raw.len() <= limit {
        return (raw.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}…（本帧已被截断）", &raw[..end]), true)
}

/// 观察者实现：攒批后推 `apilot://stream`，并在收尾/断连时补一个 `done`。
///
/// 它同时负责把这条请求从后端的 [`InflightStore`] 里摘掉。两件事必须由同一个
/// 对象在同一时刻做：前端那份「进行中」只是副本，底账在 [`InflightStore`] ——
/// 只清副本、或者只发 `done` 不摘底账的话，监控页一旦重新挂载就会用
/// `get_inflight_requests` 把这条请求原样拉回来，而此后再也没有事件能移走它。
pub struct StreamEmitter {
    sink: Arc<dyn StreamEventSink>,
    request_id: String,
    batcher: StreamBatcher,
    finished: bool,
    /// 后端的「进行中」表。
    inflight: Arc<super::InflightStore>,
}

impl StreamEmitter {
    pub fn new(
        sink: Arc<dyn StreamEventSink>,
        request_id: String,
        inflight: Arc<super::InflightStore>,
    ) -> Self {
        Self::with_caps(sink, request_id, inflight, BatchCaps::default())
    }

    /// 自定义上限。生产用默认值，测试用它把批次缩到一两条。
    pub fn with_caps(
        sink: Arc<dyn StreamEventSink>,
        request_id: String,
        inflight: Arc<super::InflightStore>,
        caps: BatchCaps,
    ) -> Self {
        Self {
            sink,
            request_id,
            batcher: StreamBatcher::new(caps),
            finished: false,
            inflight,
        }
    }

    /// 从后端的「进行中」表里摘掉这条请求。
    ///
    /// `finished` 已经替我们保证了"只做一次"，所以正常收尾与断连丢弃两条路
    /// 都调它也不会重复 —— 幂等的 `remove` 本来就允许重复调用。
    fn retire(&self) {
        self.inflight.remove(&self.request_id);
    }

    fn send(&self, frames: Vec<StreamFrame>, done: bool, error: Option<String>) {
        // 既没有内容也不是收尾，就别发空包了。
        if frames.is_empty() && !done {
            return;
        }
        self.sink.emit(&StreamEvent {
            request_id: self.request_id.clone(),
            frames,
            done,
            truncated: self.batcher.truncated,
            error,
        });
    }
}

impl StreamObserver for StreamEmitter {
    fn on_event(&mut self, raw: &str, deltas: &[UnifiedDelta]) {
        if let Some(frames) = self.batcher.push(raw, deltas) {
            self.send(frames, false, None);
        }
    }

    fn on_finish(&mut self, error: Option<&str>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.retire();
        let frames = self.batcher.flush();
        self.send(frames, true, error.map(str::to_string));
    }
}

impl Drop for StreamEmitter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        // 走到这里说明流是被丢弃的：客户端断连、或者请求被取消。
        // 补一个 done 是让前端把这条从「进行中」移走的唯一可靠手段。
        self.retire();
        let frames = self.batcher.flush();
        self.send(frames, true, Some("客户端提前断开".into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// 记录收到了什么，代替真实的 Tauri 事件通道。
    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<StreamEvent>>,
    }

    /// 一次性的「进行中」表。多数用例不关心它，需要断言它被摘空的用例
    /// 会自己建一个并留住句柄。
    fn store() -> Arc<crate::traffic::InflightStore> {
        Arc::new(crate::traffic::InflightStore::new())
    }

    /// 往表里塞一条，模拟"请求刚开始、`RequestStarted` 已经发出去"。
    fn started(request_id: &str) -> RequestStarted {
        RequestStarted {
            request_id: request_id.into(),
            ts: 0,
            client: "claude-code".into(),
            model: "m".into(),
            request_model: "m".into(),
            path: "/v1/messages".into(),
            protocol_in: "anthropic".into(),
            provider_tag: "t".into(),
            provider_name: "test".into(),
            upstream_url: "https://api.example.com".into(),
            is_stream: true,
        }
    }

    #[test]
    fn a_finished_stream_retires_the_request_from_the_inflight_store() {
        // 正常跑完的流同样要摘。漏掉这一步的话，前端重新挂载时会从后端把这条
        // 拉回来当成「进行中」，而它其实早就结束了。
        let inflight = store();
        inflight.insert(started("r1"));
        let mut e = StreamEmitter::with_caps(
            Arc::new(RecordingSink::default()),
            "r1".into(),
            inflight.clone(),
            caps(),
        );
        e.on_finish(None);

        assert!(inflight.snapshot().is_empty(), "收尾后底账里不该还有这条");
    }

    #[test]
    fn a_dropped_stream_retires_the_request_from_the_inflight_store() {
        // 客户端断连时生成器被直接丢弃，`on_finish` 不会跑 —— 这条是唯一的兜底。
        let inflight = store();
        inflight.insert(started("r1"));
        {
            let mut e = StreamEmitter::with_caps(
                Arc::new(RecordingSink::default()),
                "r1".into(),
                inflight.clone(),
                caps(),
            );
            e.on_event("e", &[]);
        }

        assert!(
            inflight.snapshot().is_empty(),
            "断连也要摘，否则这条会永远挂在监控页的「进行中」里"
        );
    }

    #[test]
    fn retiring_only_happens_once() {
        // `on_finish` 之后 Drop 还会跑一次，两次 remove 不能把别的请求也带走。
        let inflight = store();
        inflight.insert(started("r1"));
        inflight.insert(started("other"));
        {
            let mut e = StreamEmitter::with_caps(
                Arc::new(RecordingSink::default()),
                "r1".into(),
                inflight.clone(),
                caps(),
            );
            e.on_finish(None);
        }

        let left = inflight.snapshot();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].request_id, "other", "只摘自己那条");
    }

    impl RecordingSink {
        fn snapshot(&self) -> Vec<StreamEvent> {
            self.events.lock().clone()
        }
    }

    impl StreamEventSink for RecordingSink {
        fn emit(&self, event: &StreamEvent) {
            self.events.lock().push(event.clone());
        }
    }

    fn caps() -> BatchCaps {
        BatchCaps {
            flush_interval: Duration::from_secs(3600), // 测试里不靠时间触发
            max_frames_per_request: 100,
            max_frames_per_batch: 3,
            max_raw_frame_bytes: 16,
        }
    }

    /// 帧数上限压到 5，专供"到顶就该停"那条用例。
    fn tiny_caps() -> BatchCaps {
        BatchCaps {
            max_frames_per_request: 5,
            ..caps()
        }
    }

    fn text(s: &str) -> Vec<UnifiedDelta> {
        vec![UnifiedDelta::text(0, s)]
    }

    #[test]
    fn the_batcher_flushes_once_the_batch_is_full() {
        let mut b = StreamBatcher::new(caps());
        assert!(b.push("a", &text("1")).is_none());
        assert!(b.push("b", &text("2")).is_none());
        let batch = b.push("c", &text("3")).expect("攒够 3 帧就该吐一批");
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].seq, 1, "序号从 1 开始且连续");
        assert_eq!(batch[2].seq, 3);
        assert!(b.flush().is_empty(), "吐完之后缓冲必须清空");
    }

    #[test]
    fn the_batcher_flushes_once_the_interval_elapses() {
        let mut b = StreamBatcher::new(BatchCaps {
            flush_interval: Duration::from_millis(0),
            ..caps()
        });
        let batch = b.push("a", &text("1")).expect("时间窗已过就该立刻吐");
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn frames_are_numbered_even_when_they_arrive_one_by_one() {
        let mut b = StreamBatcher::new(caps());
        let mut seqs = Vec::new();
        for i in 0..3 {
            let batch = b
                .push(&format!("e{i}"), &text("x"))
                .map(|f| f)
                .unwrap_or_else(|| b.flush());
            seqs.extend(batch.into_iter().map(|f| f.seq));
        }
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn the_per_request_cap_stops_the_stream_and_flags_truncation() {
        let mut b = StreamBatcher::new(tiny_caps());
        for _ in 0..3 {
            b.push("x", &[]);
        }
        let _ = b.flush();
        for _ in 0..3 {
            b.push("x", &[]);
        }
        let _ = b.flush();

        assert!(b.push("x", &[]).is_none(), "越过上限后不再收帧");
        assert!(b.truncated, "必须立起 truncated 标记，否则前端不知道后面没了");
    }

    #[test]
    fn an_oversized_frame_is_truncated_and_flagged() {
        let mut b = StreamBatcher::new(caps());
        let long = "字".repeat(64);
        let batch = b.push(&long, &[]).unwrap_or_else(|| b.flush());
        assert!(batch[0].raw_truncated);
        assert!(batch[0].raw.len() < long.len());
    }

    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        // 截断点落在汉字中间会产生非法 UTF-8，前端 JSON.parse 直接炸。
        let mut b = StreamBatcher::new(BatchCaps {
            max_raw_frame_bytes: 7,
            ..caps()
        });
        let batch = b.push("中中中中中", &[]).unwrap_or_else(|| b.flush());
        assert!(batch[0].raw.starts_with("中中"));
    }

    #[test]
    fn the_emitter_reports_every_frame_across_batches() {
        let sink = Arc::new(RecordingSink::default());
        let mut e = StreamEmitter::with_caps(sink.clone(), "r1".into(), store(), caps());
        for i in 0..7 {
            e.on_event(&format!("e{i}"), &text("x"));
        }
        e.on_finish(None);

        let events = sink.snapshot();
        let total: usize = events.iter().map(|e| e.frames.len()).sum();
        assert_eq!(total, 7, "一帧都不能丢");
        assert_eq!(events.last().unwrap().request_id, "r1");
        assert!(events.last().unwrap().done);
    }

    #[test]
    fn only_the_last_batch_is_marked_done() {
        let sink = Arc::new(RecordingSink::default());
        let mut e = StreamEmitter::with_caps(sink.clone(), "r1".into(), store(), caps());
        for i in 0..4 {
            e.on_event(&format!("e{i}"), &[]);
        }
        e.on_finish(None);

        let events = sink.snapshot();
        assert!(events.len() >= 2, "前 3 帧该攒成一批吐出去");
        assert!(events[..events.len() - 1].iter().all(|e| !e.done));
        assert!(events.last().unwrap().done);
    }

    #[test]
    fn dropping_without_finishing_still_reports_done() {
        // 客户端断连时生成器被直接丢弃，on_finish 不会跑 —— 这是唯一的兜底。
        let sink = Arc::new(RecordingSink::default());
        {
            let mut e = StreamEmitter::with_caps(sink.clone(), "r1".into(), store(), caps());
            e.on_event("e", &[]);
        }
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert!(events[0].done, "断连也必须发 done，否则「进行中」清不掉");
        assert!(events[0].error.is_some(), "要说明是断开而非正常结束");
    }

    #[test]
    fn finishing_then_dropping_does_not_report_done_twice() {
        let sink = Arc::new(RecordingSink::default());
        {
            let mut e = StreamEmitter::with_caps(sink.clone(), "r1".into(), store(), caps());
            e.on_event("e", &[]);
            e.on_finish(None);
        }
        let events = sink.snapshot();
        assert_eq!(events.iter().filter(|e| e.done).count(), 1, "done 只能发一次");
    }

    #[test]
    fn a_finished_stream_carries_the_error_message() {
        let sink = Arc::new(RecordingSink::default());
        let mut e = StreamEmitter::new(sink.clone(), "r1".into(), store());
        e.on_finish(Some("上游流空闲超时"));
        let events = sink.snapshot();
        assert_eq!(events[0].error.as_deref(), Some("上游流空闲超时"));
    }


    #[test]
    fn every_delta_variant_survives_serialization() {
        // 前端按 `kind` 分支渲染增量。任何一种变体序列化失败，整批帧都会
        // emit 失败并被静默丢掉（`emit` 的错误我们无从处理），
        // 表现是「流在跑，但界面上什么都不显示」。
        let deltas: Vec<StreamDelta> = [
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
                index: 1,
                partial_json: "{}".into(),
            },
            UnifiedDelta::BlockStop { index: 1 },
            UnifiedDelta::Usage(UsageDelta {
                output_tokens: Some(3),
                ..Default::default()
            }),
            UnifiedDelta::Finish(FinishReason::ToolUse),
            UnifiedDelta::Error {
                code: "x".into(),
                message: "y".into(),
            },
        ]
        .iter()
        .map(StreamDelta::from)
        .collect();

        let event = StreamEvent {
            request_id: "r1".into(),
            frames: vec![StreamFrame {
                seq: 1,
                at_ms: 0,
                raw: "data: {}".into(),
                raw_truncated: false,
                deltas,
            }],
            done: true,
            truncated: false,
            error: None,
        };

        let json = serde_json::to_string(&event).expect("整批帧必须能序列化");
        for kind in [
            "message_start",
            "block_start",
            "text",
            "thinking",
            "tool_input",
            "block_stop",
            "usage",
            "finish",
            "error",
        ] {
            assert!(
                json.contains(&format!("\"kind\":\"{kind}\"")),
                "前端要靠 kind={kind} 分支，序列化结果里没有它"
            );
        }
    }

    #[test]
    fn the_stream_delta_never_produces_duplicate_json_keys() {
        // 直接序列化 UnifiedDelta 会拼出 `{"type":"finish","type":"tool_use"}`：
        // 两个枚举都用 `type` 作内部标签。JSON.parse 只留最后一个，前端就再也
        // 不知道这是 finish。这条守着「别再退回去直接序列化 IR」。
        let json = serde_json::to_string(&StreamDelta::Finish {
            reason: FinishReason::ToolUse,
        })
        .unwrap();
        assert_eq!(json.matches("\"type\"").count(), 1, "type 只该出现在嵌套的 reason 里");
        assert!(json.contains("\"kind\":\"finish\""));
        assert!(json.contains("\"type\":\"tool_use\""));

        // 更直接的判据：解析回 Value 之后，两个键都必须还在。
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["kind"], "finish");
        assert_eq!(v["reason"]["type"], "tool_use");
    }

    #[test]
    fn an_idle_stream_emits_nothing_until_it_ends() {
        let sink = Arc::new(RecordingSink::default());
        let e = StreamEmitter::new(sink.clone(), "r1".into(), store());
        drop(e);
        // 没有收到任何帧时不必发空批次，但结束了就得说一声。
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert!(events[0].frames.is_empty());
        assert!(events[0].done);
    }

    #[test]
    fn the_buffer_stays_bounded_under_a_flood_of_frames() {        let sink = Arc::new(RecordingSink::default());
        let mut e = StreamEmitter::new(sink.clone(), "r1".into(), store());
        for _ in 0..100_000 {
            e.on_event("x", &[]);
        }
        e.on_finish(None);

        let events = sink.snapshot();
        let total: usize = events.iter().map(|e| e.frames.len()).sum();
        assert!(
            total <= BatchCaps::default().max_frames_per_request as usize,
            "每请求的帧数必须有上限，否则前端内存会被长回答吃光"
        );
        assert!(events.last().unwrap().truncated);
    }
}
