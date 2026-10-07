//! 请求全链路编排。
//!
//! ```text
//! 解码 → 路由决策 → 选渠道 → 查缓存 → 协议转换 → 出站 → 响应 → 记账 → 落库
//! ```
//!
//! 记账统一在 [`Recorder`] 里收口：无论成功、失败、流式中断还是客户端断连，
//! 都从同一条路径写日志与聚合，避免某个分支漏账。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::billing::engine::BillingEngine;
use crate::billing::session::BillingSession;
use crate::cache::{cache_key, policy::is_cacheable, store::CacheEntry};
use crate::protocol::codec::ConvertError;
use crate::protocol::dto::{
    ContentBlock, FinishReason, Protocol, UnifiedRequest, UnifiedResponse, UnifiedUsage,
    UsageSource,
};
use crate::protocol::shared::tokens::estimate_request_tokens;
use crate::routing::{model_policy, RouteMetadata, RouteOutcome};
use crate::shell::AppShell;
use crate::storage::logs::{CaptureRecord, RequestLogRecord};
use crate::upstream::outbound::{Outbound, UpstreamBody, UpstreamError};

use super::stream::{translate_stream_observed, StreamOutcome, StreamTimeouts};

/// 从请求头 / 路径识别调用方。
///
/// 接管时我们会往客户端配置里写入标识头，因此它是最可靠的来源；
/// 其次看 User-Agent，最后落到 `unknown`。
pub fn detect_client(headers: &HeaderMap, path: &str) -> String {
    if let Some(v) = headers.get("x-apilot-client").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return v.to_string();
        }
    }

    let ua = headers
        .get(http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ua.contains("claude-cli") || ua.contains("claude-code") || ua.contains("anthropic") {
        return "claude-code".into();
    }
    if ua.contains("codex") {
        return "codex".into();
    }
    if ua.contains("gemini") {
        return "gemini-cli".into();
    }
    if ua.contains("cursor") {
        return "cursor".into();
    }

    // 兜底：按协议路径推断，至少让统计能区分流量来源。
    match Protocol::from_path(path) {
        Some(Protocol::AnthropicMessages) => "claude-code".into(),
        Some(Protocol::OpenAiResponses) => "codex".into(),
        _ => "unknown".into(),
    }
}

/// 一次请求的记账收口。
struct Recorder {
    shell: Arc<AppShell>,
    record: RequestLogRecord,
    session: BillingSession,
    capture: Option<CaptureRecord>,
    finished: bool,
}

impl Recorder {
    fn finish(&mut self, status: i32, usage: UnifiedUsage, ttfb_ms: Option<i64>) {
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

        // 「结束」信号同步发，不走下面那个后台任务：前端要立刻把这条从
        // 「进行中」移走，晚一个调度周期就看着像卡住了。负载极小。
        //
        // 流式请求不走这里 —— 它们的结束由 `StreamEmitter` 负责（流真正跑完
        // 才算结束，而 `Recorder` 早就返回了）；这里是失败路径与缓存的兜底。
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

    fn fail(&mut self, status: i32, message: impl Into<String>) {
        // 失败请求退回预扣：不能因为上游抖动就把预估的额度记成实际花费。
        let _ = self.session.refund();
        self.record.error_message = Some(message.into());
        self.finish(status, UnifiedUsage::default(), None);
    }
}

/// 处理一次网关请求。
pub async fn handle(
    shell: Arc<AppShell>,
    protocol: Protocol,
    path: String,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let _active = shell.traffic.begin_request();

    let request_id = uuid::Uuid::new_v4().to_string();
    let client = detect_client(&headers, &path);
    let request_body_for_capture = body.clone();

    // ---- 1. 解码入站请求 ----
    let codec_in = shell.codecs.codec(protocol);
    // `mut` 是因为路由之后会把 model 换成生效模型（见下面「2. 路由决策」）。
    let mut req = match codec_in.decode_request(&body) {
        Ok(r) => r,
        Err(e) => {
            shell.traffic.record_failure();
            tracing::warn!(%client, "请求解码失败: {e}");
            return error_response(protocol, 400, "请求解析失败", &e.to_string());
        }
    };

    let mut recorder = Recorder {
        shell: shell.clone(),
        record: RequestLogRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            client: client.clone(),
            protocol_in: protocol.as_str().to_string(),
            protocol_out: protocol.as_str().to_string(),
            // 此刻 req.model 还是客户端请求的名字，两者先填一样的。
            // 路由之后 `model` 会被改写成生效模型，`request_model` 保持原样不动 ——
            // 计费与缓存键取 `model`，所以两者绝不能混为一谈。
            model: req.model.clone(),
            request_model: req.model.clone(),
            path: path.clone(),
            is_stream: req.stream,
            latency_ms: 0,
            ..Default::default()
        },
        session: BillingSession::deferred(),
        capture: Some(CaptureRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            method: "POST".into(),
            path: path.clone(),
            request_headers: headers_to_json(&headers),
            request_body: Some(request_body_for_capture.to_vec()),
            ..Default::default()
        }),
        finished: false,
    };

    // ---- 1.5 全局模型替换 ----
    //
    // 放在路由规则之前：这样规则链看到的就是生效模型。规则里的 ModelOverride
    // 仍可在它之上再改（那是"例外规则"的用法）。
    //
    // `request_model` 在 Recorder 初始化时已经填成了原始值，这里只动 `req.model`，
    // 让它带着生效模型往下走。
    // 这里**不再 clone 整份 policy**：`settings()` 给的是 Arc，`ArcSwap` 没有守卫，
    // 跨下面那次 await 持有它是安全的；而 policy 现在装着映射表与脚本文本，
    // 每请求深拷贝一次纯属浪费。
    let settings = shell.settings();
    let policy = &settings.model_policy;

    let has_channels = if model_policy::needs_channel_check(policy, &client) {
        // 只有兜底模式需要这个判据，其余模式不白查一次库。
        crate::storage::providers::channels_for_model(&shell.db, &req.model)
            .await
            .map(|v| !v.is_empty())
            .unwrap_or(true) // 查库失败时按"有渠道"处理：宁可不动，也不要把请求改道
    } else {
        true
    };

    // 先算到临时变量再赋值：`RequestCtx` 借着 `req.model`，让它在该借用的
    // 生命周期里干净结束，别把"借用中又改写"这件事交给 NLL 去赌。
    let effective = {
        let ctx = model_policy::RequestCtx {
            client: &client,
            model: &req.model,
            protocol: protocol.as_str(),
        };
        model_policy::effective_model(policy, &ctx, &ShellModelEnv { has_channels })
    };
    req.model = effective;

    // ---- 2. 路由决策 ----
    let est_tokens = estimate_request_tokens(&req);
    let mut meta = RouteMetadata::new(req.model.clone(), protocol, path.clone(), headers.clone());
    meta.client = client.clone();
    meta.stream = req.stream;
    meta.est_input_tokens = est_tokens;

    let outcome = shell.router.route(&mut meta);
    let selector_tag = match outcome {
        RouteOutcome::Reject { reason, .. } => {
            shell.traffic.record_failure();
            recorder.fail(403, &reason);
            return error_response(protocol, 403, "请求被路由规则拒绝", &reason);
        }
        RouteOutcome::Final { selector, .. } => selector,
    };

    // 规则可能改写过模型名（ModelOverride 是非终结动作）。**从这一行之后，「模型」
    // 一律指最终生效的那个** —— 计费、聚合、缓存键全部跟着它，不再各算各的。
    //
    // 三个模型名各司其职，别再混用：
    //   request_model  客户端请求的名字，只作展示
    //   model          生效模型，计费 / 聚合 / 缓存键都用它
    //   upstream_model 渠道映射后真正发出去的名字（在 try_outbound 里填）
    req.model = meta.model.clone();
    recorder.record.model = meta.model.clone();

    // 预扣估算：拿本地 token 估算 + max_tokens 当输出上界。它**不参与真实计费**，
    // 只用来记下"估算与实际的偏差"，让用户在监控页看到估算靠不靠谱。
    // 口径要跟真实计费一致，所以在生效模型确定之后才算。
    let price_for_estimate = shell.pricing.load().get(&req.model);
    let est_usage = UnifiedUsage {
        input_tokens: est_tokens,
        output_tokens: req.max_tokens.unwrap_or(0) as u64,
        source: UsageSource::LocalEstimate,
        ..Default::default()
    };
    recorder.session = BillingSession::new(
        BillingEngine::settle(&est_usage, &price_for_estimate, 0).total_quota,
    );

    let upstream_model = meta.model.clone();

    // ---- 3. 选定渠道 ----
    let primary = match shell.selectors.resolve(&selector_tag) {
        Some(o) => o,
        None => {
            shell.traffic.record_failure();
            let msg = format!("selector「{selector_tag}」没有可用的渠道，请先在「路由」页配置");
            recorder.fail(503, &msg);
            return error_response(protocol, 503, "没有可用渠道", &msg);
        }
    };

    // ---- 4. 缓存查找 ----
    let policy = shell.cache.policy();
    let cacheable = is_cacheable(&req, &policy);
    let key = cacheable.then(|| cache_key(protocol, &req));

    if let Some(key) = &key {
        match shell.cache.get(&shell.db, key).await {
            Ok(Some(entry)) => {
                return serve_from_cache(
                    shell,
                    protocol,
                    &req,
                    entry,
                    recorder,
                    started,
                )
                .await;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("读缓存失败: {e}"),
        }
    }

    // ---- 5. 出站 ----
    let candidates = build_candidates(&shell, &primary, &upstream_model, protocol).await;

    // 监控页的「进行中」列表靠这一条进入。放在这里而不是请求最开始：
    // 此刻才既有生效模型、又有选定的渠道，而用户在意的正是"这一条在跑哪条路"。
    // 缓存命中在上面就返回了，所以它不会出现在「进行中」—— 那是对的，
    // 缓存命中没有过程可看。
    shell
        .events
        .request_started(&crate::traffic::stream_events::RequestStarted {
            request_id: recorder.record.request_id.clone(),
            ts: recorder.record.ts,
            client: recorder.record.client.clone(),
            model: recorder.record.model.clone(),
            path: recorder.record.path.clone(),
            protocol_in: protocol.as_str().to_string(),
            provider_tag: primary.tag().to_string(),
            is_stream: req.stream,
        });

    let mut last_error: Option<UpstreamError> = None;

    for (idx, outbound) in candidates.iter().enumerate() {
        let is_last = idx + 1 == candidates.len();

        match try_outbound(
            &shell,
            outbound,
            &req,
            &body,
            &upstream_model,
            &headers,
            &mut recorder,
            protocol,
            cacheable,
            key.clone(),
            started,
        )
        .await
        {
            Ok(resp) => return resp,
            Err(e) => {
                let retryable = e.is_retryable();
                tracing::warn!(
                    provider = outbound.tag(),
                    error = %e,
                    retryable,
                    "上游请求失败"
                );
                if !retryable || is_last {
                    last_error = Some(e);
                    break;
                }
                last_error = Some(e);
                // 换下一个渠道重试。
                continue;
            }
        }
    }

    // ---- 全部失败 ----
    shell.traffic.record_failure();
    let err = last_error
        .map(|e| e.to_string())
        .unwrap_or_else(|| "没有可用的上游渠道".to_string());
    recorder.fail(502, &err);
    error_response(protocol, 502, "上游请求失败", &err)
}

/// 组装候选渠道：主渠道 + 其余（用于故障转移）。
///
/// 主渠道由谁决定，取决于这个模型有没有配过渠道策略
/// （`model_policies` 表，界面上在**路由页**的「模型的渠道选择」里改）：
///
/// - **配过** → 按策略在声明了这个模型的所有渠道里排序，第一个当主渠道。
///   这就是"像切换代理一样切换渠道"的落点。
/// - **没配过** → 沿用 `selector` 选出来的那个（现有行为，一键不动）。
///
/// 这条界线是整个方案不破坏既有配置的基础，改动时别把它弄丢了。
///
/// 注意它是**模型策略 > selector**：模型配了策略，规则链选出来的 selector
/// 就只剩"兜底"的意义了。这是刻意的 —— 别顺手把优先级反过来，
/// 那会改变线上实际发请求的行为。
async fn build_candidates(
    shell: &Arc<AppShell>,
    primary: &Arc<dyn Outbound>,
    model: &str,
    protocol_in: Protocol,
) -> Vec<Arc<dyn Outbound>> {
    let channels = crate::storage::providers::candidate_channels(&shell.db, model)
        .await
        .unwrap_or_default();

    let policy = crate::storage::model_policies::get(&shell.db, model)
        .await
        .ok()
        .flatten();

    let mut out = match policy {
        Some(ref p) => {
            // 延迟策略要有测速结果；没测过的候选在 order() 里会被排到最后。
            let ranked: Vec<crate::routing::model_select::Candidate> = channels
                .iter()
                .map(|c| crate::routing::model_select::Candidate {
                    tag: c.provider.tag.clone(),
                    priority: c.priority,
                    weight: c.weight,
                    latency_ms: shell.probe_latency.get(&c.provider.tag).map(|v| *v),
                })
                .collect();

            let ordered =
                crate::routing::model_select::order(Some(p), ranked, crate::util::rand_unit());

            let mut cs: Vec<Arc<dyn Outbound>> = Vec::new();
            for tag in ordered {
                if let Some(o) = shell.registry.get(&tag) {
                    cs.push(o);
                }
            }

            // 模型页配的渠道一个都用不了（全被停用/删了）时不要就此失败，
            // 落回 selector 选出的那个，让请求还有救。
            if cs.is_empty() {
                vec![primary.clone()]
            } else {
                cs
            }
        }
        None => {
            let mut cs = vec![primary.clone()];
            for c in &channels {
                if c.provider.tag != primary.tag() {
                    if let Some(o) = shell.registry.get(&c.provider.tag) {
                        cs.push(o);
                    }
                }
            }
            cs
        }
    };

    prefer_native_protocol(&mut out, primary, protocol_in);
    out
}

/// 管线侧的 [`model_policy::ModelEnv`]。
///
/// 渠道检查用预先查好的结果（要不要查由 `needs_channel_check` 决定），
/// 脚本则交给 `model_script` 的全局引擎 —— memo 缓存也在那边，这里看不见。
/// 这是**唯一**把那个 trait 接到真实引擎上的地方，测试里换掉它就完全不碰 JS。
struct ShellModelEnv {
    has_channels: bool,
}

impl model_policy::ModelEnv for ShellModelEnv {
    fn has_channels(&self, _: &str) -> bool {
        self.has_channels
    }

    fn run_script(&self, source: &str, input: &model_policy::ScriptInput<'_>) -> Option<String> {
        crate::routing::model_script::run(source, input)
    }
}

/// 把候选渠道按「能原生说客户端协议」重排：能直通的不转换。
///
/// `sort_by_key` 是稳定排序，同一组内部保持调用方排好的 priority / weight 顺序
/// —— 只在"协议匹配与否"这一维上重新分组，不推翻用户的优先级配置。
///
/// 首选渠道不参与排序：它是 selector 选出来的，位置由用户的路由规则决定，
/// 不能因为我们更想用别的协议就把它顶掉。
fn prefer_native_protocol(
    candidates: &mut [Arc<dyn Outbound>],
    primary: &Arc<dyn Outbound>,
    protocol_in: Protocol,
) {
    let primary_tag = primary.tag();
    candidates.sort_by_key(|o| o.tag() != primary_tag && !o.supports(protocol_in));
}

/// 针对单个渠道执行一次完整请求。
#[allow(clippy::too_many_arguments)]
async fn try_outbound(
    shell: &Arc<AppShell>,
    outbound: &Arc<dyn Outbound>,
    req: &UnifiedRequest,
    raw_body: &Bytes,
    upstream_model: &str,
    headers: &HeaderMap,
    recorder: &mut Recorder,
    protocol_in: Protocol,
    cacheable: bool,
    cache_key: Option<String>,
    started: Instant,
) -> Result<Response, UpstreamError> {
    // 渠道声明支持入站协议就直接说那种协议（直通），不支持才回落到首选协议做转换。
    let wire = outbound.wire_for(protocol_in);
    recorder.record.protocol_out = wire.as_str().to_string();
    recorder.record.provider_tag = Some(outbound.tag().to_string());
    recorder.record.channel_kind = Some(outbound.provider().kind.as_str().to_string());

    // 渠道级模型映射：入站模型名 → 上游真实模型名。
    let mapped_model = outbound.provider().upstream_model(upstream_model).to_string();
    recorder.record.upstream_model = Some(mapped_model.clone());

    let needs_conversion = protocol_in != wire;

    // ---- 组装出站请求体 ----
    let out_body: Bytes = if needs_conversion {
        // 跨协议：用 IR 重新编码成渠道的线协议。
        let mut to_send = req.clone();
        to_send.model = mapped_model.clone();
        to_send.stream = req.stream;

        // 思考签名无法跨协议保真，剥离以免上游报 Invalid signature。
        strip_unportable_thinking(&mut to_send);

        let codec = shell.codecs.codec(wire);
        match codec.encode_request(&to_send) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                return Err(UpstreamError::Build(format!("请求编码为 {wire} 失败: {e}")))
            }
        }
    } else {
        // 同协议：尽量保留原始字节，只把 model 换成映射后的名字。
        // 走原始 body 而不是重新序列化 IR —— 后者会丢掉未建模的字段。
        patch_model_field(raw_body, &mapped_model)
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

    let resp = outbound.dial(prepared).await?;
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
        shell.traffic.record_failure();
        recorder.fail(status.as_u16() as i32, &msg);
        return Err(UpstreamError::Status {
            status: status.as_u16(),
            body: msg,
        });
    }

    // ---- 流式 ----
    if req.stream {
        let decoder = shell.codecs.codec(wire).new_stream_decoder();
        // 同协议走直通（不重编码），异协议才用编码器。
        let encoder = needs_conversion.then(|| shell.codecs.codec(protocol_in).new_stream_encoder());

        let upstream_stream = match resp.body {
            UpstreamBody::Stream(s) => s,
            // 客户端要流式但上游给了完整响应：按一次性响应处理。
            UpstreamBody::Buffered(b) => {
                return finish_buffered(
                    shell, protocol_in, wire, &b, resp.headers, recorder, status, needs_conversion,
                    cacheable, cache_key, started, &mapped_model,
                )
                .await;
            }
        };

        let settings = shell.settings();
        let timeouts = StreamTimeouts {
            first_byte: std::time::Duration::from_millis(settings.first_byte_timeout_ms),
            idle: std::time::Duration::from_millis(settings.idle_timeout_ms),
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
            provider_tag: outbound.tag().to_string(),
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
        ));

        let body = translate_stream_observed(
            upstream_stream,
            decoder,
            encoder,
            timeouts,
            Some(observer),
            move |outcome| finalize_stream(ctx, outcome),
        );

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
                        return Err(UpstreamError::Io(format!("读取上游响应失败: {e}")))
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
}

/// 处理非流式（或上游一次性返回的）响应。
#[allow(clippy::too_many_arguments)]
async fn finish_buffered(
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
    let needs_conversion = needs_conversion || decoded_from_sse;

    let Some((decoded, usage)) = decoded else {
        // 完全解不出来：把原文交给客户端，至少让功能可用，同时把用量记成未知。
        recorder.record.quota = 0;
        recorder.finish(status.as_u16() as i32, UnifiedUsage::default(), Some(0));
        return Ok(build_response(status, headers, body.clone()));
    };

    // 计费
    let price = shell.pricing.load().get(&recorder.record.model);
    let tool_calls = decoded
        .content
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count() as u32;
    let breakdown = BillingEngine::settle(&usage, &price, tool_calls);

    recorder.record.quota = breakdown.total_quota;
    recorder.record.saved_quota = breakdown.cache_saved_quota;

    let latency = started.elapsed().as_millis() as i64;
    recorder.record.latency_ms = latency;

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

    // 非流式没有独立的 TTFB，用整体耗时近似（对客户端而言等价）。
    recorder.finish(status.as_u16() as i32, usage.clone(), Some(latency));

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
        return Ok((
            status,
            sse_headers,
            super::stream::stream_from_response(decoded, usage, encoder, |_| {}),
        )
            .into_response());
    }

    if let Some(c) = recorder.capture.as_mut() {
        c.response_headers = headers_to_json(&headers);
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

    if let Some(c) = recorder.capture.as_mut() {
        c.response_body = Some(out_bytes.to_vec());
    }

    Ok(build_response(status, headers, out_bytes))
}

/// 流式收尾所需的全部上下文。
///
/// 打包成一个结构体而不是继续加参数：`finalize_stream` 原本就有 13 个入参，
/// 本次还要再带捕获与出站追踪，散着传下去没人看得懂哪个对应哪个。
/// `capture` 是从 `Recorder` 里 take 出来的 —— 流式路径没有 `Recorder::finish`，
/// 不在这里传下去，入站那份（路径、请求头、请求体）就永远丢了。
struct StreamContext {
    shell: Arc<AppShell>,
    request_id: String,
    client: String,
    path: String,
    model: String,
    provider_tag: String,
    protocol_in: Protocol,
    protocol_out: Protocol,
    channel_kind: Option<String>,
    price: crate::billing::pricing::ModelPricing,
    tool_calls: u32,
    started: Instant,
    cache_key: Option<String>,
    cacheable: bool,
    capture: Option<CaptureRecord>,
    upstream_url: Option<String>,
    upstream_model: Option<String>,
    upstream_status: Option<i32>,
}

/// 流结束后的收尾：计费、写缓存、补捕获。
fn finalize_stream(
    ctx: StreamContext,
    outcome: StreamOutcome,
) {
    let StreamContext {
        shell,
        request_id,
        client,
        path,
        model,
        provider_tag,
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
    } = ctx;

    let latency = started.elapsed().as_millis() as i64;
    let ttfb = outcome.ttfb.map(|d| d.as_millis() as i64);
    let status_code = if outcome.error.is_some() { 502 } else { 200 };

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
            channel_kind,
            model: model.clone(),
            request_model: model.clone(),
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
        };

        shell.aggregates.record(&rec);
        if let Err(e) = crate::storage::logs::insert(&shell.db, &rec).await {
            tracing::warn!("写流式请求日志失败: {e}");
        }
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
        }
    });
}

/// 把一段 SSE 正文解析成完整响应。
///
/// 用于兜住一类很常见的中转上游：它们**无视 `stream: false`**，一律回 SSE。
/// 不处理的话，非流式客户端会收到一堆 `event: ...` 文本，直接解析失败。
pub fn decode_sse_as_response(
    shell: &Arc<AppShell>,
    protocol: Protocol,
    raw: &[u8],
) -> Option<(UnifiedResponse, UnifiedUsage)> {
    let mut decoder = shell.codecs.codec(protocol).new_stream_decoder();
    let mut acc = super::stream::ContentAccumulator::new();

    let mut buf = String::new();
    let mut remainder = Vec::new();
    super::sse::append_utf8_safe(&mut buf, &mut remainder, raw);

    let mut id = String::new();
    let mut model = String::new();

    while let Some(block) = super::sse::take_sse_block(&mut buf) {
        let Some(ev) = super::sse::parse_event(&block) else {
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

/// 命中缓存时直接构造响应。
async fn serve_from_cache(
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
            recorder.fail(500, "缓存条目损坏");
            return error_response(protocol, 500, "缓存条目损坏", &e.to_string());
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

        let body = super::stream::stream_from_response(response, usage, encoder, move |outcome| {
            let mut r = rec.clone();
            r.output_tokens = outcome.usage.output_tokens;
            r.ts = crate::util::now_ms();
            // 缓存重放不走 `Recorder::finish`（它早就被丢下了），所以这里的
            // 结束事件得自己补 —— 否则前端那条「进行中」会一直挂着。
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

        return (http::StatusCode::OK, headers, body).into_response();
    }

    let body = match shell.codecs.codec(protocol).encode_response(&response, &usage) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return error_response(protocol, 500, "缓存响应编码失败", &e.to_string())
        }
    };

    recorder.finish(200, usage, Some(0));

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert("x-apilot-cache", http::HeaderValue::from_static("hit"));

    build_response(http::StatusCode::OK, headers, body)
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 把原始 JSON 里的 `model` 字段换成映射后的名字，其余字节尽量保留。
fn patch_model_field(original: &[u8], mapped: &str) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(original) else {
        return Bytes::from(original.to_vec());
    };
    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".into(), serde_json::json!(mapped));
    }
    serde_json::to_vec(&v)
        .map(Bytes::from)
        .unwrap_or_else(|_| Bytes::from(original.to_vec()))
}

/// 剥离无法跨协议保真的思考块。
///
/// Anthropic 的 `signature` 由上游签名，转到别的协议再转回来必然失效，
/// 回传会让上游 400。与其带着坏签名过去，不如明确丢掉。
fn strip_unportable_thinking(req: &mut UnifiedRequest) {
    for msg in &mut req.messages {
        msg.content.retain(|b| {
            !matches!(
                b,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
    if let Some(r) = &mut req.reasoning {
        r.budget_tokens = None;
    }
}

fn count_tool_uses(messages: &[crate::protocol::dto::UnifiedMessage]) -> u32 {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
        .count() as u32
}

fn build_response(status: http::StatusCode, headers: HeaderMap, body: Bytes) -> Response {
    let mut builder = Response::builder().status(status);
    if let Some(h) = builder.headers_mut() {
        for (k, v) in headers.iter() {
            // 长度与编码由我们自己重算，不沿用上游的。
            if k == http::header::CONTENT_LENGTH || k == http::header::CONTENT_ENCODING {
                continue;
            }
            h.insert(k, v.clone());
        }
    }
    builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| Response::new(axum::body::Body::empty()))
}

fn error_response(
    protocol: Protocol,
    status: u16,
    title: &str,
    detail: &str,
) -> Response {
    let code = http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);

    // 各协议的错误体结构不同；按客户端预期返回，它才能正确展示。
    let payload = match protocol {
        Protocol::AnthropicMessages => serde_json::json!({
            "type": "error",
            "error": { "type": "api_error", "message": format!("{title}: {detail}") },
        }),
        _ => serde_json::json!({
            "error": { "message": format!("{title}: {detail}"), "type": "api_error" },
        }),
    };

    (code, axum::Json(payload)).into_response()
}

/// 落库前必须隐去的请求头。
///
/// 入站这份带的是客户端的 key，出站那份带的是**渠道真实密钥**（`channel.rs`
/// 会把上游凭据注入进来）—— 名单漏一个就是明文落库。
const REDACTED_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "proxy-authorization",
];

fn headers_to_json(headers: &HeaderMap) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in headers.iter() {
        if REDACTED_HEADERS
            .iter()
            .any(|h| k.as_str().eq_ignore_ascii_case(h))
        {
            map.insert(k.to_string(), serde_json::json!("<已隐去>"));
            continue;
        }
        map.insert(
            k.to_string(),
            serde_json::json!(v.to_str().unwrap_or("<binary>")),
        );
    }
    serde_json::Value::Object(map)
}

#[allow(dead_code)]
fn convert_err(e: ConvertError) -> UpstreamError {
    UpstreamError::Build(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn explicit_client_header_wins() {
        let h = headers_with(&[("x-apilot-client", "codex"), ("user-agent", "claude-cli/1.0")]);
        assert_eq!(detect_client(&h, "/v1/messages"), "codex");
    }

    #[test]
    fn user_agent_is_used_when_header_absent() {
        let h = headers_with(&[("user-agent", "claude-cli/2.1.0 (external)")]);
        assert_eq!(detect_client(&h, "/v1/messages"), "claude-code");

        let h = headers_with(&[("user-agent", "codex_cli_rs/0.9")]);
        assert_eq!(detect_client(&h, "/v1/responses"), "codex");
    }

    #[test]
    fn path_is_the_last_resort() {
        let h = HeaderMap::new();
        assert_eq!(detect_client(&h, "/v1/messages"), "claude-code");
        assert_eq!(detect_client(&h, "/v1/responses"), "codex");
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "unknown");
    }

    #[test]
    fn empty_client_header_falls_through() {
        let h = headers_with(&[("x-apilot-client", "")]);
        assert_eq!(detect_client(&h, "/v1/messages"), "claude-code");
    }

    #[test]
    fn model_field_is_patched_preserving_other_fields() {
        let raw = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"custom_field":42}"#;
        let out = patch_model_field(raw, "deepseek-chat");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["model"], "deepseek-chat");
        assert_eq!(v["custom_field"], 42, "其余字段必须原样保留");
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn patch_model_on_invalid_json_returns_original() {
        let raw = b"not json";
        assert_eq!(&patch_model_field(raw, "x")[..], raw);
    }

    #[test]
    fn thinking_is_stripped_for_cross_protocol() {
        let mut req = UnifiedRequest::new("m");
        req.messages = vec![crate::protocol::dto::UnifiedMessage::new(
            crate::protocol::dto::Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    text: "想了很久".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::text("答案"),
            ],
        )];
        req.reasoning = Some(crate::protocol::dto::ReasoningConfig {
            enabled: true,
            budget_tokens: Some(8000),
            effort: None,
        });

        strip_unportable_thinking(&mut req);

        assert_eq!(req.messages[0].content.len(), 1, "思考块应被剥离");
        assert_eq!(req.messages[0].content[0].as_text(), Some("答案"));
        assert!(req.reasoning.as_ref().unwrap().budget_tokens.is_none());
    }

    #[test]
    fn auth_headers_are_redacted_in_capture() {
        let h = headers_with(&[
            ("authorization", "Bearer sk-secret"),
            ("x-api-key", "sk-also-secret"),
            ("content-type", "application/json"),
        ]);
        let v = headers_to_json(&h);

        assert_eq!(v["authorization"], "<已隐去>");
        assert_eq!(v["x-api-key"], "<已隐去>");
        assert_eq!(v["content-type"], "application/json");
        assert!(
            !v.to_string().contains("sk-secret"),
            "密钥绝不能出现在捕获里"
        );
    }

    #[test]
    fn every_upstream_auth_header_style_is_redacted() {
        // 捕获出站方向的 headers 时，里面装的是**渠道真实密钥**：
        // channel.rs 按 auth_style 注入什么名字，这里就得隐去什么名字。
        // 名单漏一个 = 密钥明文落库。
        for name in ["api-key", "x-goog-api-key", "proxy-authorization"] {
            let h = headers_with(&[(name, "sk-channel-secret")]);
            let v = headers_to_json(&h);
            assert_eq!(v[name], "<已隐去>", "{name} 必须被隐去");
            assert!(!v.to_string().contains("sk-channel-secret"));
        }
    }

    /// 只关心"支不支持某种协议"的假渠道，用于验证候选排序。
    struct FakeOutbound {
        tag: String,
        supports: Vec<Protocol>,
    }

    impl FakeOutbound {
        fn new(tag: &str, supports: Vec<Protocol>) -> Arc<dyn Outbound> {
            Arc::new(Self {
                tag: tag.into(),
                supports,
            })
        }
    }

    #[async_trait::async_trait]
    impl Outbound for FakeOutbound {
        fn tag(&self) -> &str {
            &self.tag
        }
        fn wire(&self) -> Protocol {
            Protocol::OpenAiChat
        }
        fn wire_for(&self, incoming: Protocol) -> Protocol {
            if self.supports(incoming) {
                incoming
            } else {
                Protocol::OpenAiChat
            }
        }
        fn supports(&self, protocol: Protocol) -> bool {
            self.supports.contains(&protocol)
        }
        fn provider(&self) -> &crate::storage::models::Provider {
            unimplemented!("排序测试不读渠道配置")
        }
        fn prepare(
            &self,
            _wire: Protocol,
            _incoming: &HeaderMap,
            _body: Bytes,
            _stream: bool,
        ) -> Result<crate::upstream::outbound::PreparedRequest, UpstreamError> {
            unimplemented!("排序测试不发请求")
        }
        async fn dial(
            &self,
            _req: crate::upstream::outbound::PreparedRequest,
        ) -> Result<crate::upstream::outbound::UpstreamResponse, UpstreamError> {
            unimplemented!("排序测试不发请求")
        }
    }

    fn tags(cs: &[Arc<dyn Outbound>]) -> Vec<String> {
        cs.iter().map(|c| c.tag().to_string()).collect()
    }

    #[test]
    fn candidates_speaking_the_client_protocol_come_first() {
        // 首选(selector 选的)不支持 Anthropic，后面两个支持 —— 支持的提到前面，
        // 但首选仍留在第一位，它的位置是路由规则定的。
        let primary = FakeOutbound::new("primary", vec![Protocol::OpenAiChat]);
        let mut cs = vec![
            primary.clone(),
            FakeOutbound::new("conv", vec![Protocol::OpenAiChat]),
            FakeOutbound::new("native", vec![Protocol::AnthropicMessages]),
        ];

        prefer_native_protocol(&mut cs, &primary, Protocol::AnthropicMessages);

        assert_eq!(tags(&cs), vec!["primary", "native", "conv"]);
    }

    #[test]
    fn candidate_order_within_a_group_is_preserved() {
        // 稳定排序：同组内保持调用方排好的优先级，不能被打乱。
        let primary = FakeOutbound::new("a-native", vec![Protocol::AnthropicMessages]);
        let mut cs = vec![
            primary.clone(),
            FakeOutbound::new("b-conv", vec![Protocol::OpenAiChat]),
            FakeOutbound::new("c-native", vec![Protocol::AnthropicMessages]),
            FakeOutbound::new("d-conv", vec![Protocol::OpenAiChat]),
        ];

        prefer_native_protocol(&mut cs, &primary, Protocol::AnthropicMessages);

        assert_eq!(tags(&cs), vec!["a-native", "c-native", "b-conv", "d-conv"]);
    }

    #[test]
    fn tool_use_counting() {
        use crate::protocol::dto::{Role, UnifiedMessage};
        let msgs = vec![UnifiedMessage::new(
            Role::Assistant,
            vec![
                ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "f".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::ToolUse {
                    id: "b".into(),
                    name: "g".into(),
                    input: serde_json::json!({}),
                },
                ContentBlock::text("x"),
            ],
        )];
        assert_eq!(count_tool_uses(&msgs), 2);
    }

    #[test]
    fn error_response_shape_matches_protocol() {
        let r = error_response(Protocol::AnthropicMessages, 400, "T", "D");
        assert_eq!(r.status(), http::StatusCode::BAD_REQUEST);

        let r = error_response(Protocol::OpenAiChat, 503, "T", "D");
        assert_eq!(r.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }
}
