//! 缓存命中时的重放。
//!
//! 命中没有过程可看，但它照样要记账、发「结束」事件 —— 否则前端那条
//! 「进行中」会一直挂着。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::cache::store::CacheEntry;
use crate::protocol::dto::{Protocol, UnifiedRequest};
use crate::shell::AppShell;

use super::recorder::Recorder;
use super::reply::{build_response, fail_response, headers_to_json, record_client_response};

/// 命中缓存时直接构造响应。
pub(super) async fn serve_from_cache(
    shell: Arc<AppShell>,
    protocol: Protocol,
    req: &UnifiedRequest,
    entry: CacheEntry,
    mut recorder: Recorder,
    started: Instant,
) -> Response {
    let response = match entry.response() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("缓存条目损坏: {e}");
            return fail_response(&mut recorder, protocol, 500, "缓存条目损坏", &e.to_string());
        }
    };

    let quota_saved = entry.quota;
    recorder.record.cache_hit = true;
    recorder.record.saved_quota = quota_saved;
    recorder.record.quota = 0;
    recorder.record.provider_tag = None;
    recorder.record.latency_ms = started.elapsed().as_millis() as i64;

    let usage = entry.usage.clone();

    if req.stream {
        let encoder = shell.codecs.codec(protocol).new_stream_encoder();
        let shell_cb = shell.clone();
        let mut rec = recorder.record.clone();
        rec.client = recorder.record.client.clone();

        let body = crate::gateway::stream::stream_from_response(response, usage, encoder, move |outcome| {
            let mut r = rec.clone();
            r.output_tokens = outcome.usage.output_tokens;
            r.ts = crate::util::now_ms();
            // 缓存重放不走 `Recorder::finish`（它早就被丢下了），所以这里的
            // 结束事件得自己补 —— 否则前端那条「进行中」会一直挂着。
            shell_cb.inflight.remove(&r.request_id);
            shell_cb
                .events
                .request_finished(&crate::traffic::stream_events::RequestFinished {
                    request_id: r.request_id.clone(),
                    status_code: 200,
                    error: None,
                });
            let shell2 = shell_cb.clone();
            tauri::async_runtime::spawn(async move {
                shell2.aggregates.record(&r);
                let _ = crate::storage::logs::insert(&shell2.db, &r).await;
                shell2.events.request(&r);
            });
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        );
        headers.insert("x-apilot-cache", http::HeaderValue::from_static("hit"));

        // 与真实流式那条路一致：把发给客户端的响应头也记进捕获，
        // 否则监控页的「Apilot → 客户端」在缓存命中时是空的。
        if let Some(c) = recorder.capture.as_mut() {
            c.response_headers = headers_to_json(&headers);
        }

        // 与真实流式一样交棒：这条响应体的收尾由上面的回调负责。
        recorder.handed_off = true;

        return (http::StatusCode::OK, headers, body).into_response();
    }

    let body = match shell.codecs.codec(protocol).encode_response(&response, &usage) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            // 这条以前直接返回、不记任何账，于是它既不在日志里，又只能靠
            // `Recorder::drop` 兜底 —— 而兜出来的理由是"客户端断开"，与事实不符。
            // 按其余错误路径的规矩记一笔，理由才对得上。
            return fail_response(&mut recorder, protocol, 500, "缓存响应编码失败", &e.to_string());
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert("x-apilot-cache", http::HeaderValue::from_static("hit"));

    // 先记客户端报文再结账：`finish` 会把 capture take 走落库，顺序反了这份就丢了。
    record_client_response(recorder.capture.as_mut(), &headers, &body);
    recorder.finish(200, usage, Some(0));

    build_response(http::StatusCode::OK, headers, body)
}
