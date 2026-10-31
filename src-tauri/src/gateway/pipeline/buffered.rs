//! 非流式响应的收尾：解码 → 计费 → 改写 → 缓存 → 编码回客户端。
//!
//! 「客户端要流式、上游却回了完整响应」也走这里，最后一步再包装成 SSE。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::billing::engine::BillingEngine;
use crate::cache::store::CacheEntry;
use crate::protocol::dto::{ContentBlock, Protocol, UnifiedResponse, UnifiedUsage};
use crate::shell::AppShell;
use crate::upstream::outbound::UpstreamError;

use super::finalize::decode_sse_as_response;
use super::recorder::Recorder;
use super::reply::{build_response, headers_to_json, record_client_response};

/// 处理非流式（或上游一次性返回的）响应。
#[allow(clippy::too_many_arguments)]
/// 跑响应改写脚本。`Some` = 确实改写了。
///
/// 非流式与「缓冲整条流再改写」共用这一份：`id` / `model` 是客户端认的契约字段，
/// 两条路都必须把它们还原成上游给的原值 —— 一个脱敏或改文案的脚本没有理由
/// 顺手换掉模型名，而两条路一旦处理不一致，客户端看到的模型名就会因为
/// "是不是流式"而不同。
fn rewrite_response_with(
    script: &str,
    protocol: Protocol,
    client: &str,
    model: &str,
    resp: UnifiedResponse,
) -> Option<UnifiedResponse> {
    let script = script.trim();
    if script.is_empty() {
        return None;
    }

    let value = serde_json::to_value(&resp).ok()?;
    let next = crate::routing::rewrite_script::apply(
        script,
        client,
        protocol.as_str(),
        model,
        "response",
        &value,
    )?;

    match serde_json::from_value::<UnifiedResponse>(next) {
        Ok(mut r) => {
            r.id = resp.id.clone();
            r.model = resp.model.clone();
            Some(r)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "响应改写脚本返回的结构不合法，按不改写处理"
            );
            None
        }
    }
}

pub(super) async fn finish_buffered(
    shell: &Arc<AppShell>,
    protocol_in: Protocol,
    wire: Protocol,
    body: &Bytes,
    headers: http::HeaderMap,
    recorder: &mut Recorder,
    status: http::StatusCode,
    needs_conversion: bool,
    cacheable: bool,
    cache_key: Option<String>,
    started: Instant,
    _mapped_model: &str,
) -> Result<Response, UpstreamError> {
    // 上游响应头已经由 try_outbound 记进捕获了，这里补上**上游原文**。
    // 与稍后写进 `response_body`（返回给客户端、可能已重编码）的那份分开存：
    // 只有两份都在，才看得出协议转换到底改了什么。
    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.response_body = Some(body.to_vec());
        c.upstream.status = Some(status.as_u16() as i32);
    }

    // 上游响应先解码，用于取 usage 与（必要时）重编码。
    // `decoded_from_sse` 记录"这份响应是从 SSE 还原出来的" —— 那种情况下
    // 即使入站与出站协议相同，也必须重新编码：原始字节是 SSE 正文，
    // 直接透传会把 event: 行喂给一个要 JSON 的客户端。
    let mut decoded_from_sse = false;
    let decoded = match shell.codecs.codec(wire).decode_response(body) {
        Ok(v) => Some(v),
        Err(e) => {
            // 解不出 JSON 有两种可能：上游真的坏了（透传原文保可用），
            // 或者它无视 stream=false 一律回了 SSE（很常见，需要解成完整响应）。
            let is_sse = headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.to_ascii_lowercase().contains("text/event-stream"))
                .unwrap_or(false);

            if is_sse {
                match decode_sse_as_response(shell, wire, body) {
                    Some(v) => {
                        tracing::info!("上游无视 stream=false 返回了 SSE，已解析为完整响应");
                        decoded_from_sse = true;
                        Some(v)
                    }
                    None => {
                        tracing::warn!("上游响应解码失败，改为透传原文: {e}");
                        None
                    }
                }
            } else {
                tracing::warn!("上游响应解码失败，改为透传原文: {e}");
                None
            }
        }
    };

    // 从 SSE 还原出来的响应必须走编码路径。
    let mut needs_conversion = needs_conversion || decoded_from_sse;

    let Some((mut decoded, usage)) = decoded else {
        // 完全解不出来：把原文交给客户端，至少让功能可用，同时把用量记成未知。
        recorder.record.quota = 0;
        // 转发的是原文，客户端那一栏就得有原文 —— 这条路上"解不出来"往往正是
        // 上游回了个不合规范的东西，看原文是唯一的排查手段。
        record_client_response(recorder.capture.as_mut(), &headers, body);
        recorder.finish(status.as_u16() as i32, UnifiedUsage::default(), Some(0));
        return Ok(build_response(status, headers, body.clone()));
    };

    // 起标题任务的回复就是标题本身（`{"title": "…"}`）。取不到就保持「用户第一句话」，
    // 所以这一步失败不需要任何兜底。
    if recorder.title_request {
        recorder.generated_title =
            crate::traffic::session::parse_generated_title(&decoded.concat_text());
    }

    // 计费
    let price = shell.pricing.load().get(&recorder.record.model);
    let tool_calls = decoded
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count() as u32;
    let breakdown = BillingEngine::settle(&usage, &price, tool_calls);

    // 工具频率统计：与非流式同一口径 —— 数**响应**里的调用，不数请求历史。
    // 放在改写脚本之前：脚本改的是「给客户端看什么」，不是"模型做了什么"，
    // 而这次响应确实调过这些工具是客观事实。
    shell.tool_usage.record(
        crate::util::now_ms(),
        &recorder.record.client,
        &recorder.record.model,
        &crate::protocol::shared::tools::tool_call_names(&decoded.content),
    );

    recorder.record.quota = breakdown.total_quota;
    recorder.record.saved_quota = breakdown.cache_saved_quota;

    let latency = started.elapsed().as_millis() as i64;
    recorder.record.latency_ms = latency;

    // ---- 响应改写脚本 ----
    //
    // 这条分支覆盖非流式（含"客户端要流式、上游却回了完整响应"这种伪流式）。
    // 真正的流式走 `translate_stream_with_rewrite` 的**逐增量**脚本
    // （`AppSettings::response_delta_script`），不经过这里。
    //
    // 位置在计费之后：这次请求花了多少 token 是上游的客观事实，脚本不该能通过
    // 增删内容把它改小。位置在写缓存之前：缓存里存的必须是客户端真正看到的那份。
    let script = shell.settings().response_script.clone();
    if let Some(r) = rewrite_response_with(
        &script,
        protocol_in,
        &recorder.record.client,
        &recorder.record.model,
        decoded.clone(),
    ) {
        decoded = r;
        // 同协议本来会透传原始字节，脚本改动只有重编码才落地。
        needs_conversion = true;
    }

    // 写缓存：把这次的结果存下来供后续相同请求复用。
    if cacheable {
        if let Some(key) = cache_key {
            if let Ok(entry) = CacheEntry::from_response(
                key,
                protocol_in,
                recorder.record.model.clone(),
                recorder.record.provider_tag.clone(),
                &decoded,
                &usage,
                breakdown.total_quota,
                shell.cache.policy().ttl_secs,
            ) {
                if let Err(e) = shell.cache.put(&shell.db, &entry).await {
                    tracing::warn!("写缓存失败: {e}");
                }
            }
        }
    }

    // 客户端要流式，但上游给的是完整响应（有些中转会无视 stream=true）：
    // 用编码器把结果"假装"成流，客户端的解码路径就不必区分这两种情况。
    if recorder.record.is_stream {
        let encoder = shell.codecs.codec(protocol_in).new_stream_encoder();
        let mut sse_headers = HeaderMap::new();
        sse_headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        sse_headers.insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        );
        if let Some(c) = recorder.capture.as_mut() {
            c.response_headers = headers_to_json(&sse_headers);
        }
        // 非流式没有独立的 TTFB，用整体耗时近似（对客户端而言等价）。
        //
        // `finish` 会把 capture take 走交给后台落库，**必须放在捕获填完之后**。
        // 顺序反了就永远只落得下上游那一份，「Apilot → 客户端」在监控里全空。
        recorder.finish(status.as_u16() as i32, usage.clone(), Some(latency));
        return Ok((
            status,
            sse_headers,
            crate::gateway::stream::stream_from_response(decoded, usage, encoder, |_| {}),
        )
            .into_response());
    }

    let out_bytes = if needs_conversion {
        match shell.codecs.codec(protocol_in).encode_response(&decoded, &usage) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                tracing::warn!("响应编码为 {protocol_in} 失败: {e}");
                body.clone()
            }
        }
    } else {
        body.clone()
    };

    // 回给客户端的那份报文要在结账之前记完 —— 理由同上：`finish` 里 capture 会被 take。
    record_client_response(recorder.capture.as_mut(), &headers, &out_bytes);
    // 非流式没有独立的 TTFB，用整体耗时近似（对客户端而言等价）。
    recorder.finish(status.as_u16() as i32, usage, Some(latency));

    Ok(build_response(status, headers, out_bytes))
}
