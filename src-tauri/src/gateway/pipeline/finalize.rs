//! 流式收尾，以及把「无视 stream=false 的 SSE」还原成完整响应。
//!
//! 流式路径没有 `Recorder::finish`：`Recorder` 在响应体交出去时就返回了，
//! 而流才刚开始跑。所以收尾所需的一切都打包进 [`StreamContext`] 带到这里。

use std::sync::Arc;
use std::time::Instant;

use crate::billing::engine::BillingEngine;
use crate::cache::store::CacheEntry;
use crate::gateway::stream::StreamOutcome;
use crate::protocol::dto::{FinishReason, Protocol, UnifiedResponse, UnifiedUsage, UsageSource};
use crate::shell::AppShell;
use crate::storage::logs::{CaptureRecord, RequestLogRecord};
use crate::storage::sessions::TitleSource;

use super::recorder::save_session_title;

/// 流式收尾所需的全部上下文。
///
/// 打包成一个结构体而不是继续加参数：`finalize_stream` 原本就有 13 个入参，
/// 本次还要再带捕获与出站追踪，散着传下去没人看得懂哪个对应哪个。
/// `capture` 是从 `Recorder` 里 take 出来的 —— 流式路径没有 `Recorder::finish`，
/// 不在这里传下去，入站那份（路径、请求头、请求体）就永远丢了。
pub(super) struct StreamContext {
    pub(super) shell: Arc<AppShell>,
    pub(super) request_id: String,
    pub(super) client: String,
    pub(super) path: String,
    /// 生效模型（计费、缓存键按它算）。
    pub(super) model: String,
    /// 客户端请求的原始名字。只作展示，但**必须活着送到收尾** ——
    /// 少了它，监控页就只剩生效模型一个名字，"我发的 A 怎么按 B 计费"查不出来。
    pub(super) request_model: String,
    pub(super) provider_tag: String,
    /// 渠道整数 id，用于日志里关联渠道名称。
    pub(super) provider_id: Option<i64>,
    pub(super) protocol_in: Protocol,
    pub(super) protocol_out: Protocol,
    pub(super) channel_kind: Option<String>,
    pub(super) price: crate::billing::pricing::ModelPricing,
    pub(super) tool_calls: u32,
    pub(super) started: Instant,
    pub(super) cache_key: Option<String>,
    pub(super) cacheable: bool,
    pub(super) capture: Option<CaptureRecord>,
    pub(super) upstream_url: Option<String>,
    pub(super) upstream_model: Option<String>,
    pub(super) upstream_status: Option<i32>,
    /// 对话归属。与 `request_model` 同理：`Recorder` 在这条路上会被丢掉，得带过来。
    pub(super) session: crate::traffic::session::SessionTag,
}

/// 流结束后的收尾：计费、写缓存、补捕获。
pub(super) fn finalize_stream(
    ctx: StreamContext,
    outcome: StreamOutcome,
) {
    let StreamContext {
        shell,
        request_id,
        client,
        path,
        model,
        request_model,
        provider_tag,
        provider_id,
        protocol_in,
        protocol_out,
        channel_kind,
        price,
        tool_calls,
        started,
        cache_key,
        cacheable,
        capture,
        upstream_url,
        upstream_model,
        upstream_status,
        session,
    } = ctx;

    let latency = started.elapsed().as_millis() as i64;
    let ttfb = outcome.ttfb.map(|d| d.as_millis() as i64);
    // 499 = nginx 的「客户端关闭请求」。客户端收完内容就断、或者中途按 Esc，
    // 上游都没错；记成 502 会让人以为是上游抽风。见 `Recorder::Drop` 同样的取舍。
    let status_code = if outcome.aborted {
        499
    } else if outcome.error.is_some() {
        502
    } else {
        200
    };

    // 起标题任务的回复就是标题本身。流断了或解析不出就算了 —— 保持「用户第一句话」。
    let generated_title = (session.title_request && outcome.error.is_none())
        .then(|| crate::traffic::session::parse_generated_title(&outcome.text))
        .flatten();

    if let Some(ms) = ttfb {
        if outcome.error.is_none() {
            shell.traffic.record_ttfb(ms);
        }
    }

    let breakdown = BillingEngine::settle(&outcome.usage, &price, tool_calls);

    tauri::async_runtime::spawn(async move {
        // 流式结果只有拿到完整内容才值得缓存 —— 中断的流缓存下来会永远返回残缺答案。
        if cacheable && outcome.error.is_none() && !outcome.content.is_empty() {
            if let Some(key) = cache_key {
                let resp = UnifiedResponse {
                    id: request_id.clone(),
                    model: model.clone(),
                    content: outcome.content.clone(),
                    finish_reason: outcome
                        .finish_reason
                        .clone()
                        .unwrap_or(FinishReason::Stop),
                };
                if let Ok(entry) = CacheEntry::from_response(
                    key,
                    // 缓存键是按入站协议算的，条目也必须记入站协议，
                    // 否则命中后会用错编码器。
                    protocol_in,
                    model.clone(),
                    Some(provider_tag.clone()),
                    &resp,
                    &outcome.usage,
                    breakdown.total_quota,
                    shell.cache.policy().ttl_secs,
                ) {
                    let _ = shell.cache.put(&shell.db, &entry).await;
                }
            }
        }

        let rec = RequestLogRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            client,
            protocol_in: protocol_in.as_str().to_string(),
            protocol_out: protocol_out.as_str().to_string(),
            provider_tag: Some(provider_tag),
            provider_id,
            channel_kind,
            model: model.clone(),
            request_model,
            path,
            upstream_url,
            upstream_model,
            upstream_status,
            is_stream: true,
            status_code,
            error_message: outcome.error.clone(),
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
            cache_read_tokens: outcome.usage.cache_read_tokens,
            cache_creation_tokens: outcome.usage.cache_creation_tokens,
            reasoning_tokens: outcome.usage.reasoning_tokens,
            usage_source: match outcome.usage.source {
                UsageSource::Upstream => "upstream".into(),
                UsageSource::LocalEstimate => "local".into(),
            },
            quota: breakdown.total_quota,
            cost_usd: crate::billing::quota::quota_to_usd(breakdown.total_quota),
            latency_ms: latency,
            ttfb_ms: ttfb,
            cache_hit: false,
            saved_quota: breakdown.cache_saved_quota,
            other: serde_json::json!({
                "stream_events": outcome.events,
            }),
            session_id: session.session_id,
            thread_key: session.thread_key,
            message_count: session.message_count,
            has_tools: session.has_tools,
        };

        shell.aggregates.record(&rec);
        if let Err(e) = crate::storage::logs::insert(&shell.db, &rec).await {
            tracing::warn!("写流式请求日志失败: {e}");
        }
        // 非流式那条路在 `Recorder::finish` 里，两处都要 —— 见 `save_session_title`。
        save_session_title(&shell, &rec, session.title.as_deref(), TitleSource::FirstLine).await;
        save_session_title(&shell, &rec, generated_title.as_deref(), TitleSource::Generated).await;
        shell.events.request(&rec);

        // 补捕获：入站字段沿用请求进来时建好的那份，再补上流式文本与出站追踪。
        // 不能新建一条空的 —— `save_capture` 是 INSERT OR REPLACE，那样会把
        // 入站的路径 / 请求头 / 请求体整条覆盖成空值。
        if shell.settings().capture_enabled {
            let mut cap = capture.unwrap_or_default();
            cap.request_id = rec.request_id.clone();
            cap.ts = rec.ts;
            cap.stream_text = Some(outcome.text);
            cap.stream_events = outcome.events as i64;

            // 结构化内容：思考与工具调用只存在于这里，纯文本的 stream_text 里没有。
            // 内容为空（流一开始就断了）时不写，免得给一条失败的请求塞个空响应。
            if !outcome.content.is_empty() {
                let resp = UnifiedResponse {
                    id: rec.request_id.clone(),
                    model: rec.model.clone(),
                    content: outcome.content.clone(),
                    finish_reason: outcome
                        .finish_reason
                        .clone()
                        .unwrap_or(FinishReason::Stop),
                };
                cap.response_content = serde_json::to_string(&resp).ok();
            }

            // 原始 SSE 帧，供「原始」视图。客户端侧那份在直通时与上游相同，不重复存。
            cap.upstream_stream_raw = (!outcome.raw_upstream.is_empty())
                .then_some(outcome.raw_upstream.clone());
            cap.client_stream_raw = outcome.raw_client.clone();
            cap.stream_raw_truncated = outcome.raw_truncated;

            // 每个事件的时间点，供时间轴。空流不写 —— 界面上要区分
            // "没有时间轴数据"与"有一份全零的假轴"。
            if !outcome.timings.is_empty() {
                cap.stream_timings = serde_json::to_string(&outcome.timings).ok();
            }

            if let Err(e) = crate::storage::logs::save_capture(&shell.db, &cap).await {
                tracing::warn!("写流式捕获失败: {e}");
            }

            // 条数上限必须在这里也执行一次。以前只有非流式那条路调 prune，
            // 而真实流量 99% 是流式 —— 结果是 `capture_max_entries` 对绝大多数
            // 请求形同虚设（现场：上限 500，表里 717 行）。
            let max_entries = shell.settings().capture_max_entries as i64;
            if let Err(e) = crate::storage::logs::prune_captures(&shell.db, max_entries).await {
                tracing::warn!("清理超限捕获失败: {e}");
            }
        }
    });
}

/// 把一段 SSE 正文解析成完整响应。
///
/// 用于兜住一类很常见的中转上游：它们**无视 `stream: false`**，一律回 SSE。
/// 不处理的话，非流式客户端会收到一堆 `event: ...` 文本，直接解析失败。
pub(super) fn decode_sse_as_response(
    shell: &Arc<AppShell>,
    protocol: Protocol,
    raw: &[u8],
) -> Option<(UnifiedResponse, UnifiedUsage)> {
    let mut decoder = shell.codecs.codec(protocol).new_stream_decoder();
    let mut acc = crate::gateway::stream::ContentAccumulator::new();

    let mut buf = String::new();
    let mut remainder = Vec::new();
    crate::gateway::sse::append_utf8_safe(&mut buf, &mut remainder, raw);

    let mut id = String::new();
    let mut model = String::new();

    while let Some(block) = crate::gateway::sse::take_sse_block(&mut buf) {
        let Some(ev) = crate::gateway::sse::parse_event(&block) else {
            continue;
        };
        let Ok(deltas) = decoder.on_event(&ev) else {
            continue;
        };
        for d in &deltas {
            if let crate::protocol::dto::UnifiedDelta::MessageStart {
                id: mid,
                model: mmodel,
            } = d
            {
                if !mid.is_empty() {
                    id = mid.clone();
                }
                if !mmodel.is_empty() {
                    model = mmodel.clone();
                }
            }
            acc.apply(d);
        }
    }
    for d in decoder.finish() {
        acc.apply(&d);
    }

    let content = acc.finish();
    let usage = decoder.usage();

    // 什么都没解出来说明这压根不是 SSE，交给调用方走原来的透传路径。
    if content.is_empty() && usage.is_empty() {
        return None;
    }

    if id.is_empty() {
        id = format!("apilot_{}", uuid::Uuid::new_v4().simple());
    }

    Some((
        UnifiedResponse {
            id,
            model,
            content,
            // 流式增量里带 stop_reason，但 `StreamDecoder` 没把它暴露出来。
            // 这里保守地用 Stop —— 客户端拿到的正文是完整的，只是终止原因
            // 可能不如上游标注的精确；比起让整个非流式请求失败，这个取舍划算。
            finish_reason: FinishReason::Stop,
        },
        usage,
    ))
}
