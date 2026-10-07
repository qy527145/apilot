//! 端到端测试：起一个真实的上游 HTTP 服务，跑通「出站 → 流式转换 → 下游」全链路。
//!
//! 单元测试覆盖了各层的逻辑，但它们连起来是否还对，只有真发一次 HTTP 才知道。
//! 这里刻意走真实 socket，覆盖 reqwest 流式、SSE 分片、协议转码这几处最容易
//! 在集成点出问题的地方。

use std::sync::{Arc, Mutex};

use axum::body::Body as AxumBody;
use axum::extract::State;
use axum::response::Response as AxumResponse;
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use futures::StreamExt;
use http::HeaderMap;

use crate::gateway::stream::{translate_stream_observed, StreamOutcome, StreamTimeouts};
use crate::protocol::anthropic::{AnthropicStreamDecoder, AnthropicStreamEncoder};
use crate::protocol::oai_chat::{ChatStreamDecoder, ChatStreamEncoder};
use crate::protocol::dto::Protocol;
use crate::storage::models::{AuthStyle, Provider, ProviderKind};
use crate::upstream::channel::Channel;
use crate::upstream::client::{build_with, ProxySpec};
use crate::upstream::outbound::{Outbound, UpstreamBody};

/// e2e 都打本地假上游，直连即可 —— 也用不上宿主机的环境代理。
fn direct_client() -> reqwest::Client {
    build_with(&ProxySpec::Direct)
}

/// 上游收到的请求快照，用于断言出站请求确实带上了该带的东西。
#[derive(Debug, Default, Clone)]
struct ReceivedRequest {
    method: String,
    path: String,
    headers: HeaderMap,
    body: String,
}

/// 起一个"回声"上游：不认路径，收到什么记什么，并按给定状态与原样回一段正文。
///
/// 原样转发的测试要的正是"路径 / 方法 / 正文都没被改写"，所以上游必须能收到**任意**
/// 路径 —— [`spawn_upstream`] 只注册了两条固定路径，那正是这类测试要排除的前提。
async fn spawn_echo_upstream(
    status: u16,
    response_body: &'static str,
) -> (String, Arc<Mutex<Vec<ReceivedRequest>>>) {
    let captured: Arc<Mutex<Vec<ReceivedRequest>>> = Arc::new(Mutex::new(Vec::new()));

    // 闭包是 `move`，会把这个句柄整个拿走 —— 而外面还要用它当 state 和返回值，
    // 所以各留一份克隆。
    let for_handler = captured.clone();
    let for_state = captured.clone();
    let app = Router::new()
        .fallback(
            move |method: http::Method,
                  uri: http::Uri,
                  headers: HeaderMap,
                  body: Bytes| {
                let cap = for_handler.clone();
                async move {
                    cap.lock().unwrap().push(ReceivedRequest {
                        method: method.to_string(),
                        path: uri.path().to_string(),
                        headers,
                        body: String::from_utf8_lossy(&body).to_string(),
                    });
                    AxumResponse::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(AxumBody::from(response_body))
                        .unwrap()
                }
            },
        )
        .with_state(for_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), captured)
}

/// 起一个假上游，按 `sse_body` 原样返回 SSE。
async fn spawn_upstream(sse_body: &'static str) -> (String, Arc<Mutex<Vec<ReceivedRequest>>>) {
    let captured: Arc<Mutex<Vec<ReceivedRequest>>> = Arc::new(Mutex::new(Vec::new()));

    // 两条路径共用一个处理体。闭包写成 `move`，才能把 `&'static str` 复制进去
    // 而不是借用外层函数的局部变量。
    let handler = move |path: &'static str,
                        cap: Arc<Mutex<Vec<ReceivedRequest>>>,
                        headers: HeaderMap,
                        body: Bytes| async move {
        cap.lock().unwrap().push(ReceivedRequest {
            method: "POST".into(),
            path: path.to_string(),
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        });
        AxumResponse::builder()
            .header("content-type", "text/event-stream")
            .body(AxumBody::from(sse_body))
            .unwrap()
    };

    let messages_cap = captured.clone();
    let chat_cap = captured.clone();
    let messages_handler = handler.clone();
    let chat_handler = handler;

    let app = Router::new()
        .route(
            "/v1/messages",
            post(move |State(cap): State<Arc<Mutex<Vec<ReceivedRequest>>>>,
                  headers: HeaderMap,
                  body: Bytes| {
                messages_handler("/v1/messages", cap, headers, body)
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move |State(cap): State<Arc<Mutex<Vec<ReceivedRequest>>>>,
                  headers: HeaderMap,
                  body: Bytes| {
                chat_handler("/v1/chat/completions", cap, headers, body)
            }),
        )
        .with_state(captured.clone());

    let _ = (messages_cap, chat_cap);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), captured)
}

fn provider(base_url: &str, kind: ProviderKind) -> Provider {
    // Anthropic 官方用 x-api-key，OpenAI 系用 Bearer —— 按渠道类型给默认值。
    let auth_style = match kind {
        ProviderKind::Anthropic => AuthStyle::XApiKey,
        _ => AuthStyle::Bearer,
    };
    Provider {
        id: 1,
        tag: "mock".into(),
        name: "mock".into(),
        kind,
        base_url: base_url.to_string(),
        api_key: Some("sk-test-key".into()),
        auth_style,
        protocols: Vec::new(),
        extra_headers: Default::default(),
        param_override: None,
        model_mapping: Default::default(),
        weight: 1,
        priority: 0,
        enabled: true,
        timeout_ms: 30_000,
        proxy: Default::default(),
        created_at: 0,
        updated_at: 0,
    }
}

/// 收集响应体的全部字节。
async fn drain(body: axum::body::Body) -> Vec<u8> {
    body.into_data_stream()
        .filter_map(|r| async move { r.ok() })
        .fold(Vec::new(), |mut acc, b| async move {
            acc.extend_from_slice(&b);
            acc
        })
        .await
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-5\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":900}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"，世界\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const CHAT_SSE: &str = concat!(
    "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"id\":\"c1\",\"model\":\"gpt-4o\",\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":800}}}\n\n",
    "data: [DONE]\n\n",
);

/// 把一次出站请求的响应流跑过上转管线，返回下游字节与统计。
async fn run_pipeline(
    channel: &Channel,
    want_chat: bool,
) -> (Vec<u8>, StreamOutcome, Option<String>) {
    let body = Bytes::from_static(
        br#"{"model":"claude-sonnet-5","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let prepared = channel.prepare(Protocol::AnthropicMessages, &HeaderMap::new(), body, true).unwrap();
    let resp = channel.dial(prepared).await.expect("上游应当可达");
    assert!(resp.is_success(), "上游应当返回成功");

    let stream = match resp.body {
        UpstreamBody::Stream(s) => s,
        UpstreamBody::Buffered(b) => {
            panic!("期望流式响应，实际拿到 {} 字节的完整响应", b.len())
        }
    };

    let (tx, rx) = tokio::sync::oneshot::channel();
    let decoder = Box::new(AnthropicStreamDecoder::new());
    let encoder = if want_chat {
        Some(Box::new(ChatStreamEncoder::new()) as Box<dyn crate::protocol::codec::StreamEncoder>)
    } else {
        None
    };

    let body = translate_stream_observed(
        stream,
        decoder,
        encoder,
        StreamTimeouts::default(),
        None,
        move |o| {
            let _ = tx.send(o);
        });

    let bytes = drain(body).await;
    let outcome = rx.await.expect("on_finish 应当被调用");
    (bytes, outcome, None)
}

#[tokio::test]
async fn cross_protocol_stream_end_to_end() {
    let (base, captured) = spawn_upstream(ANTHROPIC_SSE).await;
    let channel = Channel::new(provider(&base, ProviderKind::Anthropic), direct_client());

    let (bytes, outcome, _) = run_pipeline(&channel, true).await;
    let downstream = String::from_utf8_lossy(&bytes).to_string();

    // --- 上游确实收到了我们的请求 ---
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 1, "上游应当收到恰好一次请求");
    let req = &reqs[0];
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(
        req.headers.get("x-api-key").map(|v| v.to_str().unwrap()),
        Some("sk-test-key"),
        "鉴权头必须按渠道配置注入"
    );
    assert!(req.body.contains("claude-sonnet-5"));

    // --- 下游拿到的是 OpenAI Chat 形态 ---
    assert!(
        downstream.contains("\"object\":\"chat.completion.chunk\""),
        "应转码为 Chat 形态，实际: {downstream}"
    );
    assert!(downstream.contains("你好"), "中文内容不得损坏");
    assert!(downstream.contains("，世界"));
    assert!(downstream.contains("[DONE]"), "Chat 流必须以 [DONE] 收尾");
    assert!(
        !downstream.contains("message_stop"),
        "不应把 Anthropic 的事件名漏给下游"
    );

    // --- 统计正确 ---
    assert_eq!(outcome.text, "你好，世界");
    assert_eq!(outcome.usage.input_tokens, 100, "input 应保持 fresh 口径");
    assert_eq!(outcome.usage.cache_read_tokens, 900);
    assert_eq!(outcome.usage.output_tokens, 7);
    assert!(outcome.ttfb.is_some(), "必须记录 TTFB");
    assert!(outcome.error.is_none(), "不应有错误: {:?}", outcome.error);
    assert!(!outcome.content.is_empty(), "应重建出内容块供缓存使用");
}

#[tokio::test]
async fn same_protocol_stream_passes_through_byte_for_byte() {
    let (base, _captured) = spawn_upstream(ANTHROPIC_SSE).await;
    let channel = Channel::new(provider(&base, ProviderKind::Anthropic), direct_client());

    let (bytes, outcome, _) = run_pipeline(&channel, false).await;
    let downstream = String::from_utf8_lossy(&bytes).to_string();

    // 直通模式下下游收到的必须与上游发出的完全一致 —— 一个字节都不能变。
    assert_eq!(
        downstream, ANTHROPIC_SSE,
        "同协议直通不应改动任何字节"
    );

    // 但统计照样要拿到
    assert_eq!(outcome.text, "你好，世界");
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.output_tokens, 7);
    assert!(outcome.ttfb.is_some());
}

#[tokio::test]
async fn outbound_url_follows_channel_protocol_not_client_protocol() {
    // 客户端发的是 Anthropic 路径，但渠道是 Chat 类型 —— 应当打到 chat/completions
    let (base, captured) = spawn_upstream(CHAT_SSE).await;
    let channel = Channel::new(
        provider(&base, ProviderKind::OpenAiChat),
        direct_client(),
    );

    let prepared = channel
        .prepare(Protocol::AnthropicMessages, &HeaderMap::new(), Bytes::from_static(b"{}"), true)
        .unwrap();
    assert!(
        prepared.url.ends_with("/v1/chat/completions"),
        "出站 URL 应由渠道线协议决定，实际: {}",
        prepared.url
    );

    // 真的打过去，确认上游收到
    let _ = channel.dial(prepared).await.unwrap();
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs[0].path, "/v1/chat/completions");
    assert_eq!(
        reqs[0].headers.get("authorization").map(|v| v.to_str().unwrap()),
        Some("Bearer sk-test-key"),
        "Bearer 风格渠道应注入 Authorization"
    );
}

#[tokio::test]
async fn chat_upstream_to_anthropic_client_end_to_end() {
    // 反向：上游 Chat，下游要 Anthropic（缓存命中重放走的就是这条路径）
    let (base, _captured) = spawn_upstream(CHAT_SSE).await;
    let channel = Channel::new(
        provider(&base, ProviderKind::OpenAiChat),
        direct_client(),
    );

    let prepared = channel
        .prepare(Protocol::AnthropicMessages, &HeaderMap::new(), Bytes::from_static(b"{}"), true)
        .unwrap();
    let resp = channel.dial(prepared).await.unwrap();
    let stream = match resp.body {
        UpstreamBody::Stream(s) => s,
        _ => panic!("期望流式"),
    };

    let (tx, rx) = tokio::sync::oneshot::channel();
    let body = translate_stream_observed(
        stream,
        Box::new(ChatStreamDecoder::new()),
        Some(Box::new(AnthropicStreamEncoder::new())),
        StreamTimeouts::default(),
        None,
        move |o| {
            let _ = tx.send(o);
        });

    let bytes = drain(body).await;
    let down = String::from_utf8_lossy(&bytes).to_string();

    assert!(down.contains("message_start"), "应转成 Anthropic 事件: {down}");
    assert!(down.contains("content_block_delta"));
    assert!(down.contains("message_stop"), "必须以 message_stop 收尾");
    assert!(!down.contains("[DONE]"), "Anthropic 流不应带 [DONE]");

    let outcome = rx.await.unwrap();
    // Chat 上游的 prompt_tokens 含缓存，转成 IR 时必须减掉
    assert_eq!(outcome.usage.input_tokens, 200, "1000 - 800");
    assert_eq!(outcome.usage.cache_read_tokens, 800);
    assert_eq!(outcome.text, "hi");
}

#[tokio::test]
async fn unreachable_upstream_reports_a_connect_error() {
    // 指向一个没人监听的端口
    let channel = Channel::new(
        provider("http://127.0.0.1:1", ProviderKind::Anthropic),
        direct_client(),
    );

    let prepared = channel
        .prepare(Protocol::AnthropicMessages, &HeaderMap::new(), Bytes::from_static(b"{}"), false)
        .unwrap();
    // `UpstreamResponse` 内含流对象，无法 derive Debug，因此不用 `unwrap_err`。
    let err = match channel.dial(prepared).await {
        Ok(_) => panic!("连一个没人监听的端口不该成功"),
        Err(e) => e,
    };

    assert!(err.is_retryable(), "连不上属于可重试错误，应触发换渠道");
    assert!(
        matches!(
            err,
            crate::upstream::outbound::UpstreamError::Connect(_)
                | crate::upstream::outbound::UpstreamError::Io(_)
        ),
        "应归类为连接类错误，实际: {err}"
    );
}

#[tokio::test]
async fn request_direction_conversion_reaches_upstream_correctly() {
    // 客户端说 Anthropic，渠道说 OpenAI Chat —— 上半程（请求方向）的转换
    // 之前只在单元测试里验证过，这里走完整 HTTP 确认它真的落地。
    let (base, captured) = spawn_upstream(CHAT_SSE).await;
    let channel = Channel::new(
        provider(&base, ProviderKind::OpenAiChat),
        direct_client(),
    );

    let codecs = crate::protocol::codec::CodecRegistry::new();
    let anthropic_body = Bytes::from(
        r#"{"model":"claude-sonnet-5","max_tokens":512,"temperature":0.0,
             "system":"你是助手",
             "messages":[{"role":"user","content":"北京天气如何"}],
             "tools":[{"name":"get_weather","description":"查天气","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
             "tool_choice":{"type":"auto"}}"#
            .as_bytes()
            .to_vec(),
    );

    // 转换到渠道的线协议
    let converted = codecs
        .convert_request(
            crate::protocol::dto::Protocol::AnthropicMessages,
            crate::protocol::dto::Protocol::OpenAiChat,
            &anthropic_body,
        )
        .expect("跨协议请求转换不应失败");

    let prepared = channel
        .prepare(Protocol::AnthropicMessages, &HeaderMap::new(), Bytes::from(converted), false)
        .unwrap();
    let resp = channel.dial(prepared).await.unwrap();
    assert!(resp.is_success());

    // 上游收到的应当是合法的 OpenAI Chat 请求
    let reqs = captured.lock().unwrap();
    let received: serde_json::Value = serde_json::from_str(&reqs[0].body).expect("必须是合法 JSON");

    assert_eq!(received["model"], "claude-sonnet-5");
    assert_eq!(received["max_tokens"], 512);
    // Anthropic 的顶层 system 要折进 messages 首位
    assert_eq!(received["messages"][0]["role"], "system");
    assert_eq!(received["messages"][0]["content"][0]["text"], "你是助手");
    assert_eq!(received["messages"][1]["role"], "user");
    assert_eq!(received["messages"][1]["content"][0]["text"], "北京天气如何");
    // 工具定义要换成 Chat 的嵌套结构
    assert_eq!(received["tools"][0]["type"], "function");
    assert_eq!(received["tools"][0]["function"]["name"], "get_weather");
    assert!(received["tools"][0]["function"]["parameters"].is_object());
    // Anthropic 的 {"type":"auto"} 要变成字符串 "auto"
    assert_eq!(received["tool_choice"], "auto");
}

#[tokio::test]
async fn sse_body_can_be_decoded_into_a_complete_response() {
    // 有些中转上游无视 stream=false 一律回 SSE。用 Anthropic 解析器验证
    // 这类响应能被还原成完整答案 + 用量，供非流式客户端使用与计费。
    //
    // 这里直接调解析函数（不需要 AppShell 之外的东西），因此用一个最小
    // 构造的 codec 走一遍等价逻辑：手工喂 SSE 给解码器与累加器。
    use crate::gateway::sse::{append_utf8_safe, parse_event, take_sse_block};
    use crate::gateway::stream::ContentAccumulator;
    use crate::protocol::codec::StreamDecoder;

    let mut decoder = AnthropicStreamDecoder::new();
    let mut acc = ContentAccumulator::new();

    let mut buf = String::new();
    let mut remainder = Vec::new();
    append_utf8_safe(&mut buf, &mut remainder, ANTHROPIC_SSE.as_bytes());

    while let Some(block) = take_sse_block(&mut buf) {
        let Some(ev) = parse_event(&block) else { continue };
        let deltas = decoder.on_event(&ev).unwrap();
        for d in &deltas {
            acc.apply(d);
        }
    }
    for d in decoder.finish() {
        acc.apply(&d);
    }

    let content = acc.finish();
    let usage = decoder.usage();

    assert_eq!(content.len(), 1);
    assert_eq!(content[0].as_text(), Some("你好，世界"));
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.cache_read_tokens, 900);
    assert_eq!(usage.output_tokens, 7);
}

#[tokio::test]
async fn bare_sse_body_is_reconstructible_from_single_chunk() {
    // 模拟"上游一次性返回整个 SSE 正文"的场景（非流式请求打到了只回 SSE 的上游）
    let (base, _cap) = spawn_upstream(ANTHROPIC_SSE).await;
    let channel = Channel::new(provider(&base, ProviderKind::Anthropic), direct_client());

    let prepared = channel
        .prepare(Protocol::AnthropicMessages, &HeaderMap::new(), Bytes::from_static(b"{}"), false) // 非流式
        .unwrap();
    let resp = channel.dial(prepared).await.unwrap();

    // 非流式请求：即使上游回的是 SSE，也应当被缓冲成一整块
    let body = match resp.body {
        UpstreamBody::Buffered(b) => b,
        UpstreamBody::Stream(_) => panic!("非流式请求不该拿到流式 body"),
    };

    assert!(String::from_utf8_lossy(&body).contains("message_stop"));
    assert!(
        resp.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream"),
        "content-type 应当保留，供上层判断这是 SSE"
    );
}

#[tokio::test]
async fn buffered_upstream_response_is_handled() {
    let app = Router::new().route(
        "/v1/messages",
        post(|_body: Bytes| async move {
            AxumResponse::builder()
                .header("content-type", "application/json")
                .body(AxumBody::from(
                    r#"{"id":"msg_1","model":"m","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}}"#,
                ))
                .unwrap()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let channel = Channel::new(
        provider(&format!("http://{addr}"), ProviderKind::Anthropic),
        direct_client(),
    );
    let prepared = channel
        .prepare(
            Protocol::AnthropicMessages,
            &HeaderMap::new(),
            Bytes::from_static(br#"{"stream":true}"#),
            true,
        )
        .unwrap();
    let resp = channel.dial(prepared).await.unwrap();

    // 请求的是流式，但上游返回了完整 JSON —— 应当落进 Buffered 分支
    match resp.body {
        UpstreamBody::Buffered(b) => {
            assert!(String::from_utf8_lossy(&b).contains("done"));
        }
        UpstreamBody::Stream(_) => panic!("上游返回完整 JSON，不该被当成流"),
    }
}

// ---------------------------------------------------------------------------
// 渠道声明支持多种协议时，按入站协议挑直通的那一种
// ---------------------------------------------------------------------------

/// 把 `provider` 改造成"同时声明 Anthropic 与 Chat"的渠道。
fn declare_both_protocols(p: &mut Provider) {
    p.protocols = vec![
        crate::storage::models::ProtocolEndpoint::plain(Protocol::AnthropicMessages),
        crate::storage::models::ProtocolEndpoint::plain(Protocol::OpenAiChat),
    ];
}

#[tokio::test]
async fn client_protocol_is_used_verbatim_when_the_channel_supports_it() {
    // 渠道两种协议都支持 —— 客户端说 Anthropic 就该原样发 /v1/messages，
    // 而不是"反正能转换"就一律转成某个首选协议。
    let (base, captured) = spawn_upstream(ANTHROPIC_SSE).await;
    let mut p = provider(&base, ProviderKind::Anthropic);
    declare_both_protocols(&mut p);
    let channel = Channel::new(p, direct_client());

    let raw = br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let wire = channel.wire_for(Protocol::AnthropicMessages);
    assert_eq!(wire, Protocol::AnthropicMessages, "支持就该直通");

    let prepared = channel
        .prepare(wire, &HeaderMap::new(), Bytes::from_static(raw), true)
        .unwrap();
    assert!(prepared.url.ends_with("/v1/messages"), "实际: {}", prepared.url);

    // 直通的含义就是"字节不动"：转换路径会重新序列化 IR，
    // 那样未建模的字段就丢了。这里逐字节比对，守住这一点。
    assert_eq!(prepared.body.as_ref(), raw, "同协议必须原样转发");

    let _ = channel.dial(prepared).await.unwrap();
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs[0].path, "/v1/messages");
}

#[tokio::test]
async fn channel_without_the_incoming_protocol_converts_to_its_preferred_one() {
    // 同一个上游地址，但这次渠道只声明 Chat —— 客户端仍说 Anthropic，
    // 就该转换并发到 chat/completions。
    let (base, captured) = spawn_upstream(CHAT_SSE).await;
    let channel = Channel::new(
        provider(&base, ProviderKind::OpenAiChat),
        direct_client(),
    );

    let wire = channel.wire_for(Protocol::AnthropicMessages);
    assert_eq!(wire, Protocol::OpenAiChat, "不支持才回落到首选协议");

    let prepared = channel
        .prepare(wire, &HeaderMap::new(), Bytes::from_static(br#"{"model":"x"}"#), true)
        .unwrap();
    assert!(prepared.url.ends_with("/v1/chat/completions"), "实际: {}", prepared.url);

    let _ = channel.dial(prepared).await.unwrap();
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs[0].path, "/v1/chat/completions");
}

/// 配了代理也不能把回环地址送出去。
///
/// 这是「回环与私有网段必须直连」那条铁律的端到端验证：本地模型服务
/// （ollama / LM Studio）常年跑在 127.0.0.1，一旦被送到代理上就是 502，
/// 表现为"本地模型连不上"，而日志里只看得见一次失败的连接。
///
/// 代理指向一个必然连不上的端口 —— 只要请求真的走了代理，这条就会失败。
#[tokio::test]
async fn a_configured_proxy_never_swallows_loopback_requests() {
    let (base, captured) = spawn_upstream(ANTHROPIC_SSE).await;

    for url in ["http://127.0.0.1:1", "socks5://127.0.0.1:1"] {
        let channel = Channel::new(
            provider(&base, ProviderKind::Anthropic),
            build_with(&ProxySpec::Proxied { url: url.into() }),
        );
        let (bytes, _, _) = run_pipeline(&channel, false).await;
        assert!(
            !bytes.is_empty(),
            "{url}: 目标是回环地址，必须直连而不是交给代理"
        );
    }

    assert_eq!(captured.lock().unwrap().len(), 2, "两次都该真的打到上游");
}

// ---------------------------------------------------------------------------
// 非对话请求的原样转发
//
// `/v1/messages/count_tokens` 这类请求没有 IR 可言，只能原样送出去。下面两条
// 盯的正是"原样"：路径、方法、正文都没被改写，上游的报错也没被包装。
// ---------------------------------------------------------------------------

/// 路径与方法都要原样到达上游。
#[tokio::test]
async fn a_non_chat_request_reaches_the_upstream_verbatim() {
    let (base, captured) = spawn_echo_upstream(200, r#"{"input_tokens":1234}"#).await;
    let channel = Channel::new(
        provider(&base, ProviderKind::Anthropic),
        build_with(&ProxySpec::Direct),
    );

    let body = Bytes::from_static(
        br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}]}"#,
    );

    // 客户端带了自己的 base path（`/anthropic`）。那段是它本地的事，转给上游要丢掉。
    let mut prepared = channel
        .prepare_at(
            Protocol::AnthropicMessages,
            &HeaderMap::new(),
            body.clone(),
            false,
            Some("v1/messages/count_tokens"),
        )
        .unwrap();
    // 用一个不寻常的方法，好让"写死成 POST"这件事暴露出来。
    prepared.method = http::Method::PUT;

    assert!(
        prepared.url.ends_with("/v1/messages/count_tokens"),
        "出站 URL 该落在客户端那条路径上：{}",
        prepared.url
    );
    assert!(
        !prepared.url.contains("/anthropic"),
        "客户端自己的 base path 不该带上游：{}",
        prepared.url
    );

    let resp = channel.dial(prepared).await.unwrap();
    assert!(resp.is_success());

    let got = captured.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].path, "/v1/messages/count_tokens");
    assert_eq!(got[0].method, "PUT", "方法要原样，不能退回成写死的 POST");
    assert_eq!(got[0].body, String::from_utf8_lossy(&body), "正文一个字节都不该动");
}

/// 上游的报错原样交给客户端，不变成我们自己的 502。
///
/// 这正是这类接口存在的意义之一：「这个服务商不支持它」该由客户端自己看见。
#[tokio::test]
async fn an_upstream_error_comes_back_verbatim() {
    let (base, _cap) = spawn_echo_upstream(
        404,
        r#"{"error":{"message":"count_tokens not supported"}}"#,
    )
    .await;
    let channel = Channel::new(
        provider(&base, ProviderKind::Anthropic),
        build_with(&ProxySpec::Direct),
    );

    let prepared = channel
        .prepare_at(
            Protocol::AnthropicMessages,
            &HeaderMap::new(),
            Bytes::from_static(b"{}"),
            false,
            Some("v1/messages/count_tokens"),
        )
        .unwrap();

    // `dial` 不把非 2xx 当错误 —— 原样转发要的正是这一点：由调用方原封不动回给客户端，
    // 而不是像对话路径那样把它折成 UpstreamError 去换渠道。
    let resp = channel.dial(prepared).await.unwrap();
    assert_eq!(resp.status.as_u16(), 404);
    match resp.body {
        UpstreamBody::Buffered(b) => {
            assert!(
                String::from_utf8_lossy(&b).contains("not supported"),
                "上游的错误正文该原样留在响应里"
            );
        }
        UpstreamBody::Stream(_) => panic!("非流式请求不该拿到流"),
    }
}
