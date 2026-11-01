//! 一次请求的记账收口。
//!
//! 成功、失败、流式中断、客户端断连都从这里写日志与聚合 —— 分散在各分支里
//! 迟早会漏掉某一条，而漏掉的那条恰恰最需要排查。

use std::sync::Arc;

use http::HeaderMap;

use crate::billing::session::BillingSession;
use crate::protocol::dto::{UnifiedUsage, UsageSource};
use crate::shell::AppShell;
use crate::storage::logs::{CaptureRecord, RequestLogRecord};
use crate::storage::sessions::TitleSource;

pub(super) struct Recorder {
    pub(super) shell: Arc<AppShell>,
    pub(super) record: RequestLogRecord,
    pub(super) session: BillingSession,
    pub(super) capture: Option<CaptureRecord>,
    /// 这个对话的标题（用户第一条消息的第一句话），随日志一起落进 `session_titles`。
    ///
    /// 不放进 `record`：它是**会话级**的一行，不是这条请求的字段。
    pub(super) session_title: Option<String>,
    /// 这次请求是不是客户端的起标题任务（`traffic::session::is_title_request`）。
    /// 是的话，响应回来后值得再从里面取一次标题 —— 那比"第一句话切一刀"准。
    pub(super) title_request: bool,
    /// 起标题任务从回复里取回的标题。落库时覆盖 `session_title`，但不会被它覆盖。
    pub(super) generated_title: Option<String>,
    pub(super) finished: bool,
    /// 收尾已经交给了流式那条路（`finalize_stream` + `StreamEmitter`）。
    ///
    /// 必须与 `finished` 分开：`handle` 把响应体交出去之后自己就返回了，
    /// `Recorder` 随即被丢弃 —— 可那时**流才刚开始跑**。少了这个标记，
    /// 下面 `Drop` 里的兜底清理会把一条还活着的请求当成夭折处理。
    pub(super) handed_off: bool,
}

impl Recorder {
    /// 记下这次请求属于哪个对话。三条入口（对话 / 原样转发 / 读体失败）都要调，
    /// 漏了哪条，那类请求就会从对话链路里消失 —— 而失败的那几条恰恰最需要追溯。
    pub(super) fn tag_session(&mut self, headers: &HeaderMap, body: &[u8]) {
        let t = crate::traffic::session::tag(headers, &self.record.path, body);
        self.record.session_id = t.session_id;
        self.record.thread_key = t.thread_key;
        self.record.message_count = t.message_count;
        self.record.has_tools = t.has_tools;
        self.session_title = t.title;
        self.title_request = t.title_request;
        // 落库留痕：会话视图靠它把这条线认成「标题」（见 `classify_threads`）。流式那条
        // 收尾不经过这里的 `record`，`finalize_stream` 照同样的口径再写一次。
        if t.title_request {
            self.record.other["title_request"] = serde_json::json!(true);
        }
    }

    /// 这个对话被固定了模型就改用它；返回改写后的模型名。
    ///
    /// 必须在 `tag_session` 之后（要会话 ID 与带没带工具）、在模型策略之后（固定是
    /// 更具体的意图，不该被全局策略盖掉）。改了就在 `other.session_pin` 里留痕：
    /// 监控里一条请求的模型与客户端发的不一样，用户得看得出是谁改的。
    pub(super) fn apply_session_pin(&mut self, model: String, is_conversation: bool) -> String {
        let pinned = self.shell.session_pins.model_for(
            self.record.session_id.as_deref(),
            self.record.has_tools,
            is_conversation,
        );
        match pinned {
            Some(m) if m != model => {
                self.record.other["session_pin"] = serde_json::json!({ "from": model, "to": m });
                m
            }
            _ => model,
        }
    }

    pub(super) fn finish(&mut self, status: i32, usage: UnifiedUsage, ttfb_ms: Option<i64>) {
        if self.finished {
            return;
        }
        self.finished = true;

        let settings = self.shell.settings();

        self.record.status_code = status;
        self.record.ttfb_ms = ttfb_ms;
        if let Some(ms) = ttfb_ms {
            // 只有成功的请求才计入 TTFB 分位：失败的往往是立刻返回的错误，
            // 混进去会把中位数拉低到没有参考价值。
            if status < 400 {
                self.shell.traffic.record_ttfb(ms);
            }
        }
        self.record.usage_source = match usage.source {
            UsageSource::Upstream => "upstream".into(),
            UsageSource::LocalEstimate => "local".into(),
        };
        self.record.input_tokens = usage.input_tokens;
        self.record.output_tokens = usage.output_tokens;
        self.record.cache_read_tokens = usage.cache_read_tokens;
        self.record.cache_creation_tokens = usage.cache_creation_tokens;
        self.record.reasoning_tokens = usage.reasoning_tokens;

        // 冗余存一份美元金额：查询与图表都直接用它，不必每条都在前端换算。
        // 必须在写库前算好 —— 只给事件副本算的话，落库的那份永远是 0。
        self.record.cost_usd = crate::billing::quota::quota_to_usd(self.record.quota);

        let _ = self.session.settle(self.record.quota);
        // 估算偏差：预扣用的本地估算 vs 上游真实 usage。正值表示低估。
        // 让用户在监控页能看到"估算靠不靠谱"，而不是只能看到最终账单。
        if let Some(drift) = self.session.estimate_drift() {
            if drift != 0 {
                self.record.other["estimate_drift"] =
                    serde_json::json!(drift);
            }
        }

        let shell = self.shell.clone();
        let rec = self.record.clone();
        let capture = self.capture.take();
        let session_title = self.session_title.take();
        let generated_title = self.generated_title.take();

        // 「结束」信号同步发，不走下面那个后台任务：前端要立刻把这条从
        // 「进行中」移走，晚一个调度周期就看着像卡住了。负载极小。
        //
        // 流式请求不走这里 —— 它们的结束由 `StreamEmitter` 负责（流真正跑完
        // 才算结束，而 `Recorder` 早就返回了）；这里是失败路径与缓存的兜底。
        shell.inflight.remove(&rec.request_id);
        shell
            .events
            .request_finished(&crate::traffic::stream_events::RequestFinished {
                request_id: rec.request_id.clone(),
                status_code: rec.status_code,
                error: rec.error_message.clone(),
            });

        // 记账不该阻塞响应返回，放到后台任务里做。
        tauri::async_runtime::spawn(async move {
            shell.aggregates.record(&rec);
            if let Err(e) = crate::storage::logs::insert(&shell.db, &rec).await {
                tracing::warn!("写请求日志失败: {e}");
            }
            save_session_title(&shell, &rec, session_title.as_deref(), TitleSource::FirstLine).await;
            save_session_title(&shell, &rec, generated_title.as_deref(), TitleSource::Generated).await;

            if settings.capture_enabled {
                if let Some(mut c) = capture {
                    c.request_id = rec.request_id.clone();
                    c.ts = rec.ts;
                    if let Err(e) = crate::storage::logs::save_capture(&shell.db, &c).await {
                        tracing::warn!("写捕获失败: {e}");
                    }
                    let _ = crate::storage::logs::prune_captures(
                        &shell.db,
                        settings.capture_max_entries as i64,
                    )
                    .await;
                }
            }

            shell.events.request(&rec);
        });
    }

    pub(super) fn fail(&mut self, status: i32, message: impl Into<String>) {
        // 失败请求退回预扣：不能因为上游抖动就把预估的额度记成实际花费。
        let _ = self.session.refund();
        self.record.error_message = Some(message.into());
        self.finish(status, UnifiedUsage::default(), None);
    }
}

/// 把这个对话的标题落库（一个会话一行）。
///
/// 两个来源各调一次，规则在 `TitleSource` 里：客户端起标题任务的产出覆盖
/// 「用户第一句话」，同来源先到者说话。
///
/// **两条收尾路都要调**：非流式在 `Recorder::finish` 里，流式在 `finalize_stream` 里 ——
/// 后者自己造 record 落库、不走 `finish`（见那边的注释）。漏掉哪条，那类请求的对话
/// 就永远没有标题：Claude Code 全是流式，漏的多半是它。
///
/// 取不出标题的请求（开场只有一张图、起标题任务失败）不写：宁可让列表回落成
/// `客户端 #会话ID`，也不要凭空编一个。
pub(super) async fn save_session_title(
    shell: &Arc<AppShell>,
    rec: &RequestLogRecord,
    title: Option<&str>,
    source: TitleSource,
) {
    let (Some(session_id), Some(title)) = (rec.session_id.as_deref(), title) else {
        return;
    };
    if let Err(e) =
        crate::storage::sessions::save_title(&shell.db, session_id, title, rec.ts, source).await
    {
        tracing::warn!("写对话标题失败: {e}");
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.finished || self.handed_off {
            return;
        }

        // 走到这里说明请求**夭折**了：客户端在网关拿到上游响应之前就断开，
        // axum 于是把整个 handler future 丢掉 —— `finish` / `fail` 一个都轮不上。
        //
        // 不在这里兜一把，后果不是"少写一条日志"，而是监控页的「进行中」里从此
        // 多一个幽灵：前端重新挂载时会用 `get_inflight_requests` 把这条拉回来，
        // 而此后再也没有事件能把它移走（请求早就不存在了）。
        let request_id = self.record.request_id.clone();
        self.shell.inflight.remove(&request_id);
        self.shell
            .events
            .request_finished(&crate::traffic::stream_events::RequestFinished {
                request_id,
                // 499 是 nginx 给"客户端关闭请求"定的码，语义与这里一致。
                status_code: 499,
                error: Some("请求未完成：客户端提前断开".into()),
            });
    }
}
