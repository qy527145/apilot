//! 请求全链路编排。
//!
//! ```text
//! 解码 → 路由决策 → 选渠道 → 查缓存 → 协议转换 → 出站 → 响应 → 记账 → 落库
//! ```
//!
//! 记账统一在 [`Recorder`] 里收口：无论成功、失败、流式中断还是客户端断连，
//! 都从同一条路径写日志与聚合，避免某个分支漏账。
//!
//! 本文件只放三条入口（对话 / 原样转发 / 读体失败）与它们共用的编排。
//! 各段落在哪个子模块：
//!
//! - [`recorder`] —— 一次请求的记账收口
//! - [`candidates`] —— 候选渠道的组装与排序
//! - [`outbound`] —— 单渠道出站（对话与旁路两条）
//! - [`buffered`] —— 非流式响应收尾
//! - [`finalize`] —— 流式收尾与「SSE 伪装成完整响应」的还原
//! - [`cache`] —— 缓存命中时的重放
//! - [`body_patch`] —— 直通路上对原始 JSON 的最小改写
//! - [`reply`] —— 回给客户端的响应与错误报文

mod body_patch;
mod buffered;
mod cache;
mod candidates;
mod finalize;
mod outbound;
mod recorder;
mod reply;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::Response;
use http::HeaderMap;

use crate::billing::engine::BillingEngine;
use crate::billing::session::BillingSession;
use crate::cache::{cache_key, policy::is_cacheable};
use crate::protocol::dto::{Protocol, UnifiedRequest, UnifiedUsage, UsageSource};
use crate::protocol::shared::tokens::estimate_request_tokens;
use crate::routing::{model_policy, RouteMetadata, RouteOutcome};
use crate::shell::AppShell;
use crate::storage::logs::{CaptureRecord, RequestLogRecord};
use crate::upstream::outbound::UpstreamError;

use self::cache::serve_from_cache;
use self::candidates::{build_candidates, ShellModelEnv};
use self::outbound::{forward_raw, try_outbound};
use self::recorder::Recorder;
use self::reply::{client_status_for, fail_raw_response, fail_response, headers_to_json};
use super::router::{protocol_family, upstream_path};

/// 令牌回显成客户端名时的长度上限（按字符）。客户端名要进日志列表和路由条件，
/// 太长的串在界面上只会是一团噪音。
const MAX_TOKEN_LABEL_CHARS: usize = 32;

/// 判断请求来自哪个客户端。
///
/// 优先级：本地令牌 > `x-apilot-client` > UA > 协议路径。令牌排第一是因为它是客户端
/// **每个请求都必带**的东西（接管时按客户端写进它的配置，见 `takeover::floor::client_token`），
/// 不依赖 UA 的写法，也不要求客户端支持自定义请求头。它只是标签，不是鉴权。
pub fn detect_client(headers: &HeaderMap, path: &str) -> String {
    if let Some(label) = token_label(headers) {
        return label;
    }

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

/// 从入站鉴权头里取出令牌，换成客户端名。
///
/// - `apilot-<名字>` → `<名字>`（接管写进去的就是这个形状）
/// - 旧版统一占位 `apilot-local` → 不认，交给后面的 UA / 路径判断
/// - 其他字符串 → 原样回显（截断）：用户手动接入别的工具时填个 `cursor` 就能区分
/// - 像真密钥的（`sk-` / `AIza` 开头）→ 不认。客户端名会落库、上界面，回显真密钥
///   等于绕过了「鉴权头落库前必须隐去」那条规矩
fn token_label(headers: &HeaderMap) -> Option<String> {
    let token = inbound_token(headers)?;
    if token == crate::takeover::floor::LOCAL_PLACEHOLDER_KEY || looks_like_secret(token) {
        return None;
    }
    let label = token
        .strip_prefix(crate::takeover::floor::CLIENT_TOKEN_PREFIX)
        .filter(|rest| !rest.is_empty())
        .unwrap_or(token);
    Some(truncate_label(label))
}

/// 客户端可能把令牌放在的几个头。Anthropic SDK 的 api key 模式发 `x-api-key`，
/// 走 `ANTHROPIC_AUTH_TOKEN` / OpenAI 系发 `Authorization: Bearer`，Gemini 发 `x-goog-api-key`。
fn inbound_token(headers: &HeaderMap) -> Option<&str> {
    let bearer = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            let v = v.trim();
            // 大小写不敏感地剥掉方案名；没带方案名的就当整串是令牌。
            match v.split_once(' ') {
                Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
                _ => v,
            }
        });
    bearer
        .into_iter()
        .chain(
            ["x-api-key", "x-goog-api-key", "api-key"]
                .iter()
                .filter_map(|h| headers.get(*h)?.to_str().ok().map(str::trim)),
        )
        .find(|t| !t.is_empty())
}

fn looks_like_secret(token: &str) -> bool {
    token.starts_with("sk-") || token.starts_with("AIza")
}

fn truncate_label(s: &str) -> String {
    match s.char_indices().nth(MAX_TOKEN_LABEL_CHARS) {
        Some((cut, _)) => format!("{}…", &s[..cut]),
        None => s.to_string(),
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

    // 记账对象**先建**，再解码。顺序反过来的话，解不出来的请求连一条日志都没有 ——
    // 而那条路上回给客户端的恰恰是我们自己造的 400，用户在监控页里却完全看不到它，
    // 只能看到客户端报错。"Apilot 报的错" 尤其必须留痕。
    //
    // 这一刻还不知道模型名与流式与否（都在 body 里），先留空，解码成功后补上。
    let mut recorder = Recorder {
        shell: shell.clone(),
        record: RequestLogRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            client: client.clone(),
            protocol_in: protocol.as_str().to_string(),
            protocol_out: protocol.as_str().to_string(),
            path: path.clone(),
            latency_ms: 0,
            ..Default::default()
        },
        session: BillingSession::deferred(),
        session_title: None,
        title_request: false,
        generated_title: None,
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
        handed_off: false,
    };
    // 在解码之前：解不出的请求也得能归到对话里。
    recorder.tag_session(&headers, &body);

    // ---- 1. 解码入站请求 ----
    let codec_in = shell.codecs.codec(protocol);
    // `mut` 是因为路由之后会把 model 换成生效模型（见下面「2. 路由决策」）。
    let mut req = match codec_in.decode_request(&body) {
        Ok(r) => r,
        Err(e) => {
            shell.traffic.record_failure();
            tracing::warn!(%client, "请求解码失败: {e}");
            // 整份解不出来，但 body 里往往还留着一个模型名 —— 捞出来填上，
            // 否则这条日志的「模型」是空的，按模型筛也筛不到它。
            recorder.record.model = peek_model(&body);
            recorder.record.request_model = recorder.record.model.clone();
            return fail_response(&mut recorder, protocol, 400, "请求解析失败", &e.to_string());
        }
    };

    // 此刻 req.model 还是客户端请求的名字，两者先填一样的。
    // 路由之后 `model` 会被改写成生效模型，`request_model` 保持原样不动 ——
    // 计费与缓存键取 `model`，所以两者绝不能混为一谈。
    recorder.record.model = req.model.clone();
    recorder.record.request_model = req.model.clone();
    recorder.record.is_stream = req.stream;

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
    // 会话固定排在策略之后：用户点名"这个对话用 X"，比"所有请求都走 Y"更具体。
    // 也必须在思考接管与缓存键之前 —— 键里含模型名。
    req.model = recorder.apply_session_pin(effective, true);

    // ---- 1.6 思考接管 ----
    //
    // 位置只有这一处是对的：**在缓存键之前**（键里含思考配置），否则会出现
    // 「键按改写前算、字节按改写后发」—— 那是不会报错的错误缓存。在它之前也没有
    // 任何代码读 `req.reasoning`（token 估算 / 路由 / 可缓存判定都不读）。
    //
    // 返回值要一路带到出站组装处：同协议直通那条路转发的是**原始字节**，IR 改了它
    // 不知道，得拿着同一份配置去改 raw body。那边绝不能自己从设置重算一遍。
    let thinking = settings.apply_thinking(&mut req);

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
            return fail_response(&mut recorder, protocol, 403, "请求被路由规则拒绝", &reason);
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

    // ---- 2.5 请求改写脚本 ----
    //
    // 位置很讲究：在路由之后（脚本看到的是**生效模型**，不是客户端原话），在
    // 缓存键之前（键必须覆盖脚本的改动，否则会出现"键按 A 算、字节按 B 发"）。
    //
    // `model` 与 `stream` 由管线掌管，脚本改了也还原：前者是路由、计费、聚合
    // 三者共用的口径，后者决定下游怎么收字节。想按条件换模型请用「模型策略」
    // 里的自定义规则 —— 那里本来就有完整的判定上下文。
    let mut rewritten_by_script = false;
    let script = settings.request_script.trim();
    if !script.is_empty() {
        if let Ok(value) = serde_json::to_value(&req) {
            if let Some(next) = crate::routing::rewrite_script::apply(
                script,
                &client,
                protocol.as_str(),
                &req.model,
                "request",
                &value,
            ) {
                match serde_json::from_value::<UnifiedRequest>(next) {
                    Ok(mut r) => {
                        r.model = req.model.clone();
                        r.stream = req.stream;
                        req = r;
                        rewritten_by_script = true;
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "请求改写脚本返回的结构不合法，按不改写处理"
                    ),
                }
            }
        }
    }

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
            return fail_response(&mut recorder, protocol, 503, "没有可用渠道", &msg);
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
    // 进行中条目要描述"这条请求真正走的那条路"，得用第一个候选，而不是 selector
    // 选的 primary：模型策略 / 模型声明渠道会把别的渠道排到 primary 前面（primary
    // 那时只作兜底），照 primary 填会让渠道名和上游 URL 都指向一条根本不会走的路。
    let first = &candidates[0];
    let started_payload = crate::traffic::stream_events::RequestStarted {
        request_id: recorder.record.request_id.clone(),
        ts: recorder.record.ts,
        client: recorder.record.client.clone(),
        model: recorder.record.model.clone(),
        request_model: recorder.record.request_model.clone(),
        path: recorder.record.path.clone(),
        protocol_in: protocol.as_str().to_string(),
        provider_tag: first.tag().to_string(),
        provider_name: first.provider().name.clone(),
        upstream_url: first.provider().endpoint_for(first.wire_for(protocol)),
        is_stream: req.stream,
        session_id: recorder.record.session_id.clone(),
        thread_key: recorder.record.thread_key.clone(),
    };
    // 先写进行中快照，再广播事件：顺序保证前端任何时刻查询都不会漏掉刚进来的请求。
    shell.inflight.insert(started_payload.clone());
    shell.events.request_started(&started_payload);

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
            thinking.as_ref(),
            cacheable,
            key.clone(),
            started,
            // 脚本改过 IR 时必须重编码：同协议直通转发的是原始字节，IR 的改动
            // 到不了上游。代价是放弃直通（未建模字段会丢），这是脚本功能的
            // 明确取舍 —— 只在用户真的配了脚本且确实改动了请求时发生。
            rewritten_by_script,
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

    // 上下文超长要**换一种说法**再回给客户端。
    //
    // 这不是美化措辞：Claude Code 与 Codex 是按报文里的字判断"该压缩了"的，
    // 被我们包成 `上游请求失败: ...` 之后它们认不出，于是既不压缩也不停止 ——
    // 用户看到的是每轮都失败、重试还是失败。上游原文附在后面，不影响排查。
    if let Some(UpstreamError::Status { status, body }) = last_error.as_ref() {
        if crate::gateway::errors::is_context_overflow(*status, body) {
            let message = crate::gateway::errors::overflow_message(body);
            // 状态码仍按上游的原值透传（400 就回 400）：客户端对"超长"的判断
            // 是看报文，而用户与监控需要看到上游真正给的是哪个码。
            return fail_response(&mut recorder, protocol, *status, "上下文超长", &message);
        }
    }

    let err = last_error
        .as_ref()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "没有可用的上游渠道".to_string());
    let status = client_status_for(last_error.as_ref());
    fail_response(&mut recorder, protocol, status, "上游请求失败", &err)
}
/// 请求体没读完整（客户端中途断开）时的收口。
///
/// 照样建一个 `Recorder` 走失败收口，并把**已经收到的那部分**存进捕获 ——
/// 监控不只是"有过这么一条"，还得能看到实际到了什么，才能分析是谁的问题。
///
/// `protocol` 为 `None` 表示非对话路径，错误体用原样转发那条路的通用形状。
pub(crate) fn reject_body(
    shell: Arc<AppShell>,
    protocol: Option<Protocol>,
    method: http::Method,
    path: String,
    headers: HeaderMap,
    err: super::router::BodyError,
) -> Response {
    let _active = shell.traffic.begin_request();
    shell.traffic.record_failure();

    let request_id = uuid::Uuid::new_v4().to_string();
    let client = detect_client(&headers, &path);
    tracing::warn!(%client, %path, "{}", err.detail);

    let protocol_in = protocol
        .or_else(|| protocol_family(&path))
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "raw".into());
    let mut recorder = Recorder {
        shell: shell.clone(),
        record: RequestLogRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            client,
            protocol_out: protocol_in.clone(),
            protocol_in,
            path: path.clone(),
            ..Default::default()
        },
        session: BillingSession::deferred(),
        session_title: None,
        title_request: false,
        generated_title: None,
        capture: Some(CaptureRecord {
            request_id,
            ts: crate::util::now_ms(),
            method: method.as_str().to_string(),
            path,
            request_headers: headers_to_json(&headers),
            request_body: Some(err.partial.to_vec()),
            ..Default::default()
        }),
        finished: false,
        handed_off: false,
    };
    recorder.tag_session(&headers, &err.partial);
    recorder.record.model = peek_model(&err.partial);
    recorder.record.request_model = recorder.record.model.clone();

    // 400：请求本身不完整。客户端多半已经断了收不到，但这条记录是给监控看的。
    match protocol {
        Some(p) => fail_response(&mut recorder, p, 400, "请求体未读完整", &err.detail),
        None => fail_raw_response(&mut recorder, 400, "请求体未读完整", &err.detail),
    }
}
/// 非对话请求的原样转发。
///
/// `/v1/messages/count_tokens` 是典型：客户端在问"我这段上下文占了多少 token"，
/// body 里没有对话语义，响应也不是补全结果。硬解成 IR 再编码回去，轻则丢字段，
/// 重则把上游的解释直接改坏。
///
/// 所以这条路**只从 body 里取一个 `model` 字段**用来选渠道，其余一律原样收发。
/// 上游的报错也原样返回 —— "这个服务商不支持这个接口"正是客户端该自己看到的信息，
/// 由我们包装成 502 反而把它藏了。
///
/// 与「同协议直通」不是一回事，别混：那个仍在对话那条路上，只是不做跨协议转码；
/// 这个连 IR 都不解。
pub async fn handle_raw(
    shell: Arc<AppShell>,
    method: http::Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let _active = shell.traffic.begin_request();

    let request_id = uuid::Uuid::new_v4().to_string();
    let client = detect_client(&headers, &path);

    // 只取一个字段。解不出 JSON、或者没有 model，都不算错 —— 路由规则与 selector
    // 照样能给出一个渠道，只是按模型匹配的条件不会命中。
    let requested_model = peek_model(&body);

    let mut recorder = Recorder {
        shell: shell.clone(),
        record: RequestLogRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            client: client.clone(),
            // 这一栏是给监控页看的：路径能看出是哪种协议族的客户端就填它，
            // 看不出就明说 raw。它不参与任何转发决策。
            protocol_in: protocol_family(&path)
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "raw".into()),
            protocol_out: "raw".into(),
            model: requested_model.clone(),
            request_model: requested_model.clone(),
            path: path.clone(),
            ..Default::default()
        },
        // 不预扣：这类请求没有可计费的产出，`deferred()` 本来就是 0。
        session: BillingSession::deferred(),
        session_title: None,
        title_request: false,
        generated_title: None,
        capture: Some(CaptureRecord {
            request_id: request_id.clone(),
            ts: crate::util::now_ms(),
            method: method.as_str().to_string(),
            path: path.clone(),
            request_headers: headers_to_json(&headers),
            request_body: Some(body.to_vec()),
            ..Default::default()
        }),
        finished: false,
        handed_off: false,
    };
    // count_tokens 这类请求也带会话 ID，是对话链路的一部分。
    recorder.tag_session(&headers, &body);

    // ---- 模型替换 ----
    //
    // 照样过策略。不过的话会变成「按 A 计数、按 B 对话」—— 两个数字对不上，
    // 而且上游可能根本没有 A。
    let policy_settings = shell.settings();
    let policy = &policy_settings.model_policy;
    let has_channels = if model_policy::needs_channel_check(policy, &client) {
        crate::storage::providers::channels_for_model(&shell.db, &requested_model)
            .await
            .map(|v| !v.is_empty())
            .unwrap_or(true)
    } else {
        true
    };
    let effective = {
        // 非对话请求没有"入站协议"，给策略看的是空串 —— 自定义规则里按协议分支的
        // 写法在这里自然落到 else 上，而不是拿到一个假的协议名。
        let ctx = model_policy::RequestCtx {
            client: &client,
            model: &requested_model,
            protocol: "",
        };
        model_policy::effective_model(policy, &ctx, &ShellModelEnv { has_channels })
    };
    // count_tokens 也跟着会话固定走：按 A 计数、按 B 对话，客户端的上下文估算就错了。
    let effective = recorder.apply_session_pin(effective, false);
    recorder.record.model = effective.clone();

    // ---- 路由与选渠道（与对话请求同一套）----
    //
    // 给规则链看的协议取路径猜出来的那个族：按 `Protocol` 分支的规则不至于因为
    // "这是个非对话请求"就整条落空。猜不出来时才回落到 Anthropic —— 路由规则里
    // 按协议匹配的写法本来就少见，不值得为它再引入一个"未知协议"的概念。
    let family = protocol_family(&path).unwrap_or(Protocol::AnthropicMessages);
    let mut meta = RouteMetadata::new(effective.clone(), family, path.clone(), headers.clone());
    meta.client = client.clone();
    meta.stream = false;

    let selector_tag = match shell.router.route(&mut meta) {
        RouteOutcome::Reject { reason, .. } => {
            shell.traffic.record_failure();
            recorder.record.latency_ms = started.elapsed().as_millis() as i64;
            return fail_raw_response(&mut recorder, 403, "请求被路由规则拒绝", &reason);
        }
        RouteOutcome::Final { selector, .. } => selector,
    };
    recorder.record.model = meta.model.clone();

    let primary = match shell.selectors.resolve(&selector_tag) {
        Some(o) => o,
        None => {
            shell.traffic.record_failure();
            let msg = format!("selector「{selector_tag}」没有可用的渠道，请先在「路由」页配置");
            recorder.record.latency_ms = started.elapsed().as_millis() as i64;
            return fail_raw_response(&mut recorder, 503, "没有可用渠道", &msg);
        }
    };

    let client_path = upstream_path(&path);
    let candidates = build_candidates(&shell, &primary, &meta.model, family).await;

    let mut last_error: Option<UpstreamError> = None;

    for (idx, outbound) in candidates.iter().enumerate() {
        let is_last = idx + 1 == candidates.len();

        match forward_raw(
            outbound,
            &method,
            &headers,
            &body,
            &client_path,
            &meta.model,
            &mut recorder,
        )
        .await
        {
            Ok(resp) => {
                recorder.record.latency_ms = started.elapsed().as_millis() as i64;
                // `finish` 自己会广播「结束」事件，这里不要再补一次。
                recorder.finish(resp.status().as_u16() as i32, UnifiedUsage::default(), None);
                return resp;
            }
            Err(e) => {
                // 只有**连不上/超时**才换下一个渠道。上游返回的 4xx/5xx 不是错误，
                // 是要原样交给客户端的答案（见 `forward_raw`）。
                let retryable = e.is_retryable();
                tracing::warn!(
                    provider = outbound.tag(),
                    error = %e,
                    retryable,
                    "非对话请求转发失败"
                );
                last_error = Some(e);
                if !retryable || is_last {
                    break;
                }
            }
        }
    }

    shell.traffic.record_failure();
    let err = last_error
        .map(|e| e.to_string())
        .unwrap_or_else(|| "没有可用的上游渠道".to_string());
    recorder.record.latency_ms = started.elapsed().as_millis() as i64;
    fail_raw_response(&mut recorder, 502, "上游请求失败", &err)
}
/// 从 body 里只取 `model` 一个字段。
///
/// 非对话请求不解成 IR —— 那正是要避免的事。取不出来就返回空串，让路由按
/// "没有模型"处理。
fn peek_model(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model")?.as_str().map(str::to_string))
        .unwrap_or_default()
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
    fn local_token_names_the_client_and_beats_everything_else() {
        let h = headers_with(&[
            ("authorization", "Bearer apilot-codex"),
            ("x-apilot-client", "cursor"),
            ("user-agent", "claude-cli/2.1.0"),
        ]);
        assert_eq!(detect_client(&h, "/v1/messages"), "codex");

        // Anthropic SDK 的 api key 模式、Gemini 的头也认。
        let h = headers_with(&[("x-api-key", "apilot-claude-code")]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "claude-code");
        let h = headers_with(&[("x-goog-api-key", "apilot-gemini-cli")]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "gemini-cli");
    }

    #[test]
    fn arbitrary_token_is_echoed_instead_of_unknown() {
        let h = headers_with(&[("authorization", "Bearer my-script")]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "my-script");

        // 光一个前缀不算名字，整串回显。
        let h = headers_with(&[("authorization", "Bearer apilot-")]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "apilot-");
    }

    #[test]
    fn long_token_is_truncated_on_a_char_boundary() {
        // 头值只能是可见 ASCII，多字节字符的截断直接测截断函数。
        let label = truncate_label(&"客".repeat(40));
        assert_eq!(label.chars().count(), MAX_TOKEN_LABEL_CHARS + 1);
        assert!(label.ends_with('…'));

        let h = headers_with(&[("authorization", &format!("Bearer {}", "a".repeat(100)))]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), format!("{}…", "a".repeat(32)));
    }

    #[test]
    fn legacy_placeholder_and_real_keys_fall_through_to_ua() {
        // 旧版接管写的统一占位：认不出是谁，走 UA。
        let h = headers_with(&[
            ("authorization", "Bearer apilot-local"),
            ("user-agent", "codex_cli_rs/0.9"),
        ]);
        assert_eq!(detect_client(&h, "/v1/responses"), "codex");

        // 真密钥绝不能被回显成客户端名 —— 它会落库、上界面。
        let h = headers_with(&[("x-api-key", "sk-ant-api03-secret")]);
        assert_eq!(detect_client(&h, "/v1/messages"), "claude-code");
        let h = headers_with(&[("x-goog-api-key", "AIzaSySecret")]);
        assert_eq!(detect_client(&h, "/v1/chat/completions"), "unknown");
    }

    #[test]
    fn empty_client_header_falls_through() {
        let h = headers_with(&[("x-apilot-client", "")]);
        assert_eq!(detect_client(&h, "/v1/messages"), "claude-code");
    }

    #[test]
    fn peek_model_reads_only_the_model_field() {
        assert_eq!(
            peek_model(br#"{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}"#),
            "gpt-5"
        );
        // 取不出来就是空串：调用方按"没有模型"处理，而不是当成错误。
        assert_eq!(peek_model(br#"{"messages":[]}"#), "");
        assert_eq!(peek_model(b"not json"), "");
        assert_eq!(peek_model(br#"{"model":123}"#), "");
    }
}
