//! 单渠道出站。
//!
//! 两条路：对话请求的 [`try_outbound`]（解码 / 编码 / 流式转发 / 重试）
//! 与非对话请求的 [`forward_raw`]（原样收发，连 IR 都不解）。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::gateway::stream::{
    translate_stream_with_rewrite, StreamDeltaRewrite, StreamRestart, StreamTimeouts,
};
use crate::protocol::dto::{Protocol, ReasoningConfig, UnifiedDelta, UnifiedRequest};
use crate::shell::AppShell;
use crate::traffic::control::{RequestControl, RequestSignal};
use crate::upstream::outbound::{Outbound, UpstreamBody, UpstreamError};

use super::body_patch::{count_tool_uses, patch_model_field, patch_outbound_body,
    strip_unportable_thinking};
use super::buffered::finish_buffered;
use super::finalize::{finalize_stream, StreamContext};
use super::recorder::Recorder;
use super::reply::{build_response, headers_to_json, record_client_response};
use super::{handle_with, RetryHint};

/// 单次出站的结果。
///
/// 比 `Result<Response, UpstreamError>` 多出来的一支是给界面上那个「中断 /
/// 中断并重试」按钮的：那**不是**错误，而是一次有意为之的放弃 —— 调用方据此
/// 换渠道或整条重来，而不是把它记成上游抽风。
pub(super) enum OutboundOutcome {
    Failed(UpstreamError),
    /// 用户按了按钮，`RequestSignal` 说按的是哪一个。
    Interrupted(RequestSignal),
}

impl From<UpstreamError> for OutboundOutcome {
    fn from(e: UpstreamError) -> Self {
        Self::Failed(e)
    }
}

/// 给一次出站套上时限，超时归成 [`UpstreamError::Deadline`]（**可重试**，
/// 于是候选循环会自动换下一条渠道）。
///
/// 为什么需要它：`dial` 只在 HTTP 客户端自己的 `timeout` 到点时才返回，
/// 而那个值来自渠道的 `timeout_ms`（现场配的是 100 分钟）。中转站排队时
/// 连响应头都不给，用户就看到实时视图里一个转圈转到天荒地老。
async fn with_deadline<T>(
    limit: std::time::Duration,
    describe: &str,
    fut: impl std::future::Future<Output = Result<T, UpstreamError>>,
) -> Result<T, UpstreamError> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => {
            let secs = limit.as_secs();
            Err(UpstreamError::Deadline(format!("{describe}超时（{secs} 秒）")))
        }
    }
}

/// 发一次原样转发，返回**未经改写的**上游响应。
///
/// 与 [`try_outbound`] 的区别全在"不做"上：不解码、不重编码、不按流处理、
/// 不把非 2xx 当成错误。只有连不上或超时才返回 `Err`，让调用方换渠道。
#[allow(clippy::too_many_arguments)]
pub(super) async fn forward_raw(
    outbound: &Arc<dyn Outbound>,
    method: &http::Method,
    headers: &HeaderMap,
    body: &Bytes,
    client_path: &str,
    upstream_model: &str,
    recorder: &mut Recorder,
    // 这类请求不分流式/非流式，一律按"非流式整体超时"管。
    timeout: std::time::Duration,
) -> Result<Response, UpstreamError> {
    let wire = outbound.wire_for(Protocol::AnthropicMessages);
    recorder.record.protocol_out = wire.as_str().to_string();
    recorder.record.provider_tag = Some(outbound.tag().to_string());
    recorder.record.provider_id = Some(outbound.provider().id);
    recorder.record.channel_kind = Some(outbound.provider().kind.as_str().to_string());

    // 渠道级模型映射照样生效：客户端问的是 A，上游只认 B，那也得按 B 去问。
    let mapped_model = outbound.provider().upstream_model(upstream_model).to_string();
    recorder.record.upstream_model = Some(mapped_model.clone());

    // body 原样，只换 model 那个字段。`patch_model_field` 对非 JSON 体是原样返回的。
    let out_body = patch_model_field(body, &mapped_model);

    let mut prepared = outbound.prepare_at(wire, headers, out_body, false, Some(client_path))?;
    // prepare 一律按 POST 组装，而原样转发要连方法一起还原 —— 不然一个 GET 会被
    // 悄悄变成 POST，那种谎没人会发现。
    prepared.method = method.clone();

    recorder.record.upstream_url = Some(prepared.url.clone());
    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.url = prepared.url.clone();
        // 这份 headers 含渠道真实密钥，headers_to_json 会把鉴权头隐去。
        c.upstream.headers = headers_to_json(&prepared.headers);
        c.upstream.body = Some(prepared.body.to_vec());
    }

    let resp = with_deadline(timeout, "非对话请求", outbound.dial(prepared)).await?;

    recorder.record.upstream_status = Some(resp.status.as_u16() as i32);
    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.status = recorder.record.upstream_status;
        c.upstream.response_headers = headers_to_json(&resp.headers);
    }

    let bytes = match resp.body {
        UpstreamBody::Buffered(b) => b,
        // `prepare` 里 stream 给了 false，dial 因此不会按流处理。真出现流，
        // 只能读完再回 —— 这类接口返回的本来就是一个小 JSON。
        UpstreamBody::Stream(_) => Bytes::new(),
    };

    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.response_body = Some(bytes.to_vec());
    }
    // 原样转发时客户端收到的就是上游那几行字节 —— 头也一样，所以两边都记全。
    record_client_response(recorder.capture.as_mut(), &resp.headers, &bytes);

    tracing::debug!(provider = outbound.tag(), status = %resp.status, "非对话请求已原样转发");
    Ok(build_response(resp.status, resp.headers, bytes))
}

/// 针对单个渠道执行一次完整请求。
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_outbound(
    shell: &Arc<AppShell>,
    outbound: &Arc<dyn Outbound>,
    req: &UnifiedRequest,
    raw_body: &Bytes,
    upstream_model: &str,
    headers: &HeaderMap,
    recorder: &mut Recorder,
    protocol_in: Protocol,
    // 思考接管写进 IR 的那份配置（`None` = 不接管）。同协议直通要拿它改原始 body。
    thinking: Option<&ReasoningConfig>,
    cacheable: bool,
    cache_key: Option<String>,
    started: Instant,
    // 请求改写脚本确实改动了 IR。为真时即使同协议也必须重编码。
    force_encode: bool,
    // 界面上的「中断 / 中断并重试」从这个句柄进来。
    control: &Arc<RequestControl>,
) -> Result<Response, OutboundOutcome> {
    // 渠道声明支持入站协议就直接说那种协议（直通），不支持才回落到首选协议做转换。
    let wire = outbound.wire_for(protocol_in);
    // 「试过哪些渠道」交给控制句柄攒着：重试时要把它们排到最后。
    control.note_provider(outbound.tag());
    recorder.record.protocol_out = wire.as_str().to_string();
    recorder.record.provider_tag = Some(outbound.tag().to_string());
    recorder.record.provider_id = Some(outbound.provider().id);
    recorder.record.channel_kind = Some(outbound.provider().kind.as_str().to_string());

    // 渠道级模型映射：入站模型名 → 上游真实模型名。
    let mapped_model = outbound.provider().upstream_model(upstream_model).to_string();
    recorder.record.upstream_model = Some(mapped_model.clone());

    let cross_protocol = protocol_in != wire;
    let needs_conversion = cross_protocol || force_encode;

    // ---- 组装出站请求体 ----
    let out_body: Bytes = if needs_conversion {
        // 跨协议（或脚本改动了 IR）：用 IR 重新编码成渠道的线协议。
        let mut to_send = req.clone();
        to_send.model = mapped_model.clone();
        to_send.stream = req.stream;

        // 思考签名无法跨协议保真，剥离以免上游报 Invalid signature。
        // **只在真的跨协议时剥**：脚本导致的同协议重编码仍处在同一种协议里，
        // 签名完全有效，剥掉反而把客户端自己的思考上下文弄丢了。
        if cross_protocol {
            strip_unportable_thinking(&mut to_send, wire);
        }

        let codec = shell.codecs.codec(wire);
        match codec.encode_request(&to_send) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                return Err(UpstreamError::Build(format!("请求编码为 {wire} 失败: {e}")).into())
            }
        }
    } else {
        // 同协议：尽量保留原始字节，只把 model 换成映射后的名字（以及接管过的思考配置）。
        // 走原始 body 而不是重新序列化 IR —— 后者会丢掉未建模的字段。
        //
        // 思考那份从 `thinking` 来，**不是**在这里从设置重算：缓存键是按 `req.reasoning`
        // 算的，两条路一旦分歧就是「键按 A 算、字节按 B 发」。
        patch_outbound_body(raw_body, wire, &mapped_model, thinking)
    };

    // 修工具配对。放在**两条路之后**而不是 IR 上：同协议直通的请求根本不经过
    // IR（上面那一支原样转发原始字节），而中断恰恰最常发生在直通路上 —— 用户用
    // Claude Code 接 Anthropic 官方渠道，按 Esc 就是直通。放在这里两条路都覆盖。
    //
    // 没改动的请求会拿到原字节（`mend_request_body` 自己比较过），所以上游的
    // prompt cache 不会被无谓的重写打掉。
    let out_body: Bytes = Bytes::from(crate::protocol::shared::tool_pair::mend_request_body(
        &out_body, wire,
    ));

    // ---- Anthropic prompt cache 断点 ----
    //
    // 只补**跨协议转 Anthropic** 这一条路：直通时客户端自己的断点原样保留，
    // 我们不去加戏。放在所有改写之后（工具配对修补会重排 JSON），保证加的是
    // 最终形态；注入函数在客户端已经带断点时会一个字节都不动。
    let out_body: Bytes = if cross_protocol
        && wire == Protocol::AnthropicMessages
        && shell.settings().prompt_cache_injection
    {
        match crate::protocol::shared::prompt_cache::inject_anthropic_breakpoints(&out_body) {
            Some(b) => Bytes::from(b),
            None => out_body,
        }
    } else {
        out_body
    };

    let prepared = outbound.prepare(wire, headers, out_body.clone(), req.stream)?;

    // 记下出站方向。上游报错时（尤其 404）「到底打到了哪个 URL」是唯一能快速
    // 分辨"base_url 拼错 / 模型名映射错 / 协议选错"的证据，必须在发出去之前就留下。
    recorder.record.upstream_url = Some(prepared.url.clone());
    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.url = prepared.url.clone();
        // 这里的 headers 含**渠道真实密钥**，headers_to_json 会把鉴权头隐去。
        c.upstream.headers = headers_to_json(&prepared.headers);
        c.upstream.body = Some(prepared.body.to_vec());
    }

    // **只有这一段可以被界面打断**。上游一开始回字节（`dial` 返回）之后，
    // 「重试」就换成流里面的那一套（那边才能把新请求接到同一条下游流上），
    // 「中断」也由流的转发循环负责收尾 —— 在这里凭空丢掉一个已经拿到响应头的
    // 请求，只会在日志里留下一条"成功"、客户端却什么都收不到。
    //
    // 放在 `dial` 这一处而不是包住整个 `try_outbound`：后面那半段里
    // `finish_buffered` 已经会把 `Recorder` 结账、把「进行中」摘掉，那时再
    // 撤销就成了幽灵——账记了、响应却被丢进垃圾桶。
    // 等多久算"上游没反应"。
    //
    // 流式用 `headers_timeout_ms`：`dial` 在流式下拿到响应头就返回，量到的正是
    // 上游排队那一段。非流式用 `request_timeout_ms`：那条路上 `dial` 会把正文
    // 一起读完，这个值就是它的整体超时。两者的默认值相差五倍，正因为它们
    // 计的不是同一件事。
    let (limit_ms, describe) = {
        let s = shell.settings();
        if req.stream {
            (s.headers_timeout_ms, "等待上游响应头")
        } else {
            (s.request_timeout_ms, "非流式请求整体")
        }
    };
    let limit = std::time::Duration::from_millis(limit_ms.max(1_000));

    let resp = tokio::select! {
        biased;
        // 用户点了按钮就优先于"恰好同一刻拿到了响应头"：他要的是换条路，
        // 这条路上的运气如何已经不重要了。
        _ = control.wait() => {
            // 取走信号。不取的话，换到下一个候选的第一轮 `select!` 会立刻
            // 又命中同一次点击，一路把候选表刷穿。
            let signal = control.take().unwrap_or(RequestSignal::Cancel);
            return Err(OutboundOutcome::Interrupted(signal));
        }
        r = with_deadline(limit, describe, outbound.dial(prepared)) => r?,
    };
    let status = resp.status;

    // 上游返回的原始状态码单独记：网关可能把它包装成别的码再返回给客户端，
    // 只留一个数字会让"日志说 404、客户端说 502"这种对不上的情况无从解释。
    recorder.record.upstream_status = Some(status.as_u16() as i32);
    if let Some(c) = recorder.capture.as_mut() {
        c.upstream.status = recorder.record.upstream_status;
        c.upstream.response_headers = headers_to_json(&resp.headers);
    }

    if !status.is_success() {
        // 非成功状态：把响应体读完，提取错误信息。
        let body = match resp.body {
            UpstreamBody::Buffered(b) => b,
            UpstreamBody::Stream(_) => Bytes::new(),
        };
        let msg = shell.codecs.codec(wire).extract_error_message(&body);
        // 上游的错误原文整段留着 —— 服务商通常会在里面写明原因
        //（"model not found" / "invalid api key"），比我们转述的 msg 有用得多。
        if let Some(c) = recorder.capture.as_mut() {
            c.upstream.response_body = Some(body.to_vec());
        }
        // **不在这里结账**：这一支只说明"这个渠道给了个错"，换下一个渠道可能就成了。
        // 收尾（日志状态码、错误响应）统一由调用方做，那里才知道最终回给客户端的是什么。
        // 曾经在这里先记一次 400，于是换渠道后成功也白记，而失败时日志里的状态码
        // 是上游的 400、客户端收到的却是 502 —— 两个数字对不上，正是最难排查的那种。
        return Err(UpstreamError::Status {
            status: status.as_u16(),
            body: msg,
        }
        .into());
    }

    // ---- 2xx 却不是 API 的回复 ----
    //
    // 中转站挂了、登录过期跳了登录页、Cloudflare 弹了验证页 —— 这些都以 200 返回，
    // 内容却是一页网页或空。不认的话它会被当成**正常答案**转给客户端（Codex 收到
    // 一页 HTML 只会报 "stream closed before response.completed"，完全指不到原因），
    // 而且**下一个渠道永远不会被试**：既然"成功了"，就没有换渠道的理由。
    //
    // 判在这里、而不是收尾处，正是为了那句"换下一个渠道" —— 当成失败返回，
    // 上面的重试循环才会去问下一个候选。
    let content_type = resp
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 流式只凭响应头判（正文还没读，也不该为了判断把整个流缓冲下来）；
    // 非流式把正文一并带上，能看到"空 body"和"声明是 JSON 却不是 JSON"。
    let suspect = if let UpstreamBody::Buffered(b) = &resp.body {
        crate::gateway::errors::not_an_api_reply(status.as_u16(), &content_type, Some(b))
    } else {
        crate::gateway::errors::not_an_api_reply(status.as_u16(), &content_type, None)
    };

    if let Some(why) = suspect {
        if let Some(c) = recorder.capture.as_mut() {
            // 把那页网页存下来：用户看到的是"上游给了个 502"，而真正的原因
            // （验证页？登录页？）只有这份原文里有。
            if let UpstreamBody::Buffered(b) = &resp.body {
                c.upstream.response_body = Some(b.to_vec());
            }
        }
        return Err(UpstreamError::Status { status: 502, body: why }.into());
    }

    // ---- 流式 ----
    if req.stream {
        let decoder = shell.codecs.codec(wire).new_stream_decoder();
        let settings = shell.settings();
        let delta_script = settings.response_delta_script.clone();

        // 流式改写是**逐增量**的：每解出一条就改、立刻下发，绝不缓冲整条流。
        // 因此只要有脚本就必须有编码器（不能走直通），仅此而已。
        let rewriting_stream = !delta_script.trim().is_empty();

        let encoder = if needs_conversion || rewriting_stream {
            Some(shell.codecs.codec(protocol_in).new_stream_encoder())
        } else {
            None
        };

        let delta_rewrite = rewriting_stream.then(|| StreamDeltaRewrite {
            apply: Box::new({
                let script = delta_script.clone();
                let client = recorder.record.client.clone();
                let model = req.model.clone();
                move |delta: &UnifiedDelta| {
                    crate::routing::rewrite_script::apply_delta(
                        &script,
                        &client,
                        protocol_in.as_str(),
                        &model,
                        delta,
                    )
                }
            }),
        });

        let upstream_stream = match resp.body {
            UpstreamBody::Stream(s) => s,
            // 客户端要流式但上游给了完整响应：按一次性响应处理。
            UpstreamBody::Buffered(b) => {
                return finish_buffered(
                    shell, protocol_in, wire, &b, resp.headers, recorder, status, needs_conversion,
                    cacheable, cache_key, started, &mapped_model,
                )
                .await
                .map_err(Into::into);
            }
        };

        let timeouts = StreamTimeouts {
            first_byte: std::time::Duration::from_millis(settings.first_byte_timeout_ms),
            idle: std::time::Duration::from_millis(settings.idle_timeout_ms),
        };

        // ---- 流里面的「中断并重试」 ----
        //
        // 响应头已经发给客户端了，所以不能靠"再回一个 Response"来重试：只能把
        // 新一次请求的字节接到**同一条下游流**上。前提是这条流还没往下游送过
        // 任何东西 —— 那条判据在 `stream.rs` 里，这里只负责把"怎么再跑一遍"交给它。
        let restart = StreamRestart {
            control: control.clone(),
            make: Box::new({
                let shell = shell.clone();
                let path = recorder.record.path.clone();
                let headers = headers.clone();
                let body = raw_body.clone();
                let retry_of = recorder.record.request_id.clone();
                let control = control.clone();
                move || {
                    Box::pin(async move {
                        let hint = RetryHint {
                            skip_provider_tags: control.attempted(),
                            retry_of: Some(retry_of),
                        };
                        let resp = handle_with(shell, protocol_in, path, headers, body, hint).await;
                        // 只接 2xx 的 SSE。重试自己就失败时（路由拒绝、渠道全挂）
                        // `handle` 回的是 JSON —— 灌进一条已经在送 SSE 的响应里，
                        // 客户端解析器看到的只是垃圾。宁可让这条流就此结束。
                        let is_sse = resp
                            .headers()
                            .get(http::header::CONTENT_TYPE)
                            .and_then(|v| v.to_str().ok())
                            .is_some_and(|v| v.contains("text/event-stream"));
                        if !resp.status().is_success() || !is_sse {
                            return None;
                        }
                        Some(resp.into_body())
                    })
                }
            }),
        };

        let mut response_headers = resp.headers.clone();
        // 我们可能改写了内容，长度与编码都不再由上游保证。
        response_headers.remove(http::header::CONTENT_LENGTH);
        response_headers.remove(http::header::CONTENT_ENCODING);
        response_headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/event-stream"),
        );
        response_headers.insert(
            http::header::CACHE_CONTROL,
            http::HeaderValue::from_static("no-cache"),
        );

        // 把真正发给客户端的响应头记下来。必须在这里做：下面 `ctx` 会把 capture
        // take 走，之后再想改就没地方改了。缺了它，监控页的「Apilot → 客户端」
        // 一栏永远是空的 —— 直通时尤其误导，用户会以为网关什么都没回。
        if let Some(c) = recorder.capture.as_mut() {
            c.response_headers = headers_to_json(&response_headers);
        }

        // 流式没有 `Recorder::finish` 这一步，收尾全在 finalize_stream 里做，
        // 所以把入站捕获和出站追踪都交给它 —— 不 take 走的话，入站那份
        //（路径、请求头、请求体）会随 recorder 一起被丢掉。
        let ctx = StreamContext {
            shell: shell.clone(),
            request_id: recorder.record.request_id.clone(),
            client: recorder.record.client.clone(),
            path: recorder.record.path.clone(),
            model: req.model.clone(),
            // `Recorder` 在流式这条路上会被丢掉（收尾全在 `finalize_stream` 里），
            // 所以它记着的那个「客户端请求的名字」必须在这里带过去 —— 到了
            // `finalize_stream` 再想拿，原始值已经不存在了。
            request_model: recorder.record.request_model.clone(),
            session: crate::traffic::session::SessionTag {
                session_id: recorder.record.session_id.clone(),
                thread_key: recorder.record.thread_key.clone(),
                message_count: recorder.record.message_count,
                has_tools: recorder.record.has_tools,
                title: recorder.session_title.clone(),
                title_request: recorder.title_request,
            },
            provider_tag: outbound.tag().to_string(),
            provider_id: Some(outbound.provider().id),
            protocol_in,
            protocol_out: wire,
            channel_kind: Some(outbound.provider().kind.as_str().to_string()),
            price: shell.pricing.load().get(&req.model),
            tool_calls: count_tool_uses(&req.messages),
            started,
            cache_key: cache_key.clone(),
            cacheable,
            capture: recorder.capture.take(),
            upstream_url: recorder.record.upstream_url.clone(),
            upstream_model: recorder.record.upstream_model.clone(),
            upstream_status: recorder.record.upstream_status,
        };

        // 实时事件流的出口。它的 Drop 会在客户端断连时补发一次 done ——
        // 那种情况下 finalize_stream 根本不会执行，没有兜底前端就永远清不掉
        // 这条「进行中」。
        let observer = Box::new(crate::traffic::stream_events::StreamEmitter::new(
            shell.events.clone(),
            recorder.record.request_id.clone(),
            shell.inflight.clone(),
        ));

        let body = translate_stream_with_rewrite(
            started,
            upstream_stream,
            decoder,
            encoder,
            timeouts,
            Some(observer),
            delta_rewrite,
            Some(restart),
            move |outcome| finalize_stream(ctx, outcome),
        );

        // 交棒给流：这行之后 `Recorder` 会被丢掉，但请求还活着。
        recorder.handed_off = true;

        return Ok((status, response_headers, body).into_response());
    }

    // ---- 非流式 ----
    let buffered = match resp.body {
        UpstreamBody::Buffered(b) => b,
        UpstreamBody::Stream(mut s) => {
            use futures::StreamExt;
            let mut acc = Vec::new();
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(b) => acc.extend_from_slice(&b),
                    Err(e) => {
                        return Err(UpstreamError::Io(format!("读取上游响应失败: {e}")).into())
                    }
                }
            }
            Bytes::from(acc)
        }
    };

    finish_buffered(
        shell,
        protocol_in,
        wire,
        &buffered,
        resp.headers,
        recorder,
        status,
        needs_conversion,
        cacheable,
        cache_key,
        started,
        &mapped_model,
    )
    .await
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_hung_dial_becomes_a_retryable_deadline_error() {
        // 上游收下了请求却连响应头都不给 —— 中转站排队时就是这个形状。
        // 以前它只能等渠道自己的 `timeout_ms`（现场配的是 100 分钟）。
        let hung = futures::future::pending::<Result<u8, UpstreamError>>();
        let err = with_deadline(Duration::from_millis(20), "等待上游响应头", hung)
            .await
            .expect_err("挂住的那次必须被掐掉");
        assert!(err.is_retryable(), "超时要能触发换渠道: {err}");
        let msg = err.to_string();
        assert!(msg.contains("等待上游响应头") && msg.contains("秒"), "原因要写人话: {msg}");
    }

    #[tokio::test]
    async fn a_fast_dial_passes_through_untouched() {
        let limit = Duration::from_secs(5);
        assert_eq!(
            with_deadline(limit, "x", async { Ok::<_, UpstreamError>(7) }).await.unwrap(),
            7
        );
        // 上游自己报的错原样交给调用方判定，不能被时限吞掉换成 Timeout。
        let err = with_deadline(limit, "x", async {
            Err::<(), _>(UpstreamError::Status { status: 429, body: "slow down".into() })
        })
        .await
        .expect_err("上游的错要透出去");
        assert!(matches!(err, UpstreamError::Status { status: 429, .. }), "被改写了: {err}");
    }
}
