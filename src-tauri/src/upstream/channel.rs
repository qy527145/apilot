//! HTTP 渠道实现。

use std::time::Duration;

use axum::body::Bytes;
use http::header::{HeaderName, HeaderValue};

use super::outbound::{
    Outbound, PreparedRequest, UpstreamBody, UpstreamError, UpstreamResponse,
};
use crate::protocol::dto::Protocol;
use crate::storage::models::Provider;

/// 不向上游转发的请求头。
///
/// - 逐跳头（connection / te / transfer-encoding ...）不该跨代理传播
/// - content-length 由 reqwest 按实际 body 重算
/// - accept-encoding 交给 reqwest，它会按启用的解压特性自行协商
/// - 鉴权头必须剔除，换成渠道自己的凭据
const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "accept-encoding",
    "authorization",
    "x-api-key",
    "api-key",
];

/// 不向下游转发的响应头。
///
/// 我们可能改写了响应体（协议转换），此时 content-length 与 content-encoding
/// 都会失效；保留它们会让客户端解压失败或截断读取。
const STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
];

/// 一个可用的 HTTP 上游渠道。
pub struct Channel {
    provider: Provider,
    client: reqwest::Client,
}

impl Channel {
    pub fn new(provider: Provider, client: reqwest::Client) -> Self {
        Self { provider, client }
    }

    /// 该渠道的首选线协议。
    pub fn wire_protocol(&self) -> Protocol {
        self.provider.kind.wire_protocol()
    }
}

/// 剔除逐跳与鉴权头，其余原样转发。
fn forwardable_headers(incoming: &http::HeaderMap) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();
    for (name, value) in incoming.iter() {
        if STRIPPED_REQUEST_HEADERS
            .iter()
            .any(|h| name.as_str().eq_ignore_ascii_case(h))
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// 过滤上游响应头。
pub fn filter_response_headers(headers: &reqwest::header::HeaderMap) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();
    for (name, value) in headers.iter() {
        if STRIPPED_RESPONSE_HEADERS
            .iter()
            .any(|h| name.as_str().eq_ignore_ascii_case(h))
        {
            continue;
        }
        // reqwest 与 http crate 的 HeaderValue 都是 bytes 包装，可直接转换。
        if let Ok(v) = HeaderValue::from_bytes(value.as_bytes()) {
            out.append(HeaderName::from_bytes(name.as_str().as_bytes()).unwrap_or(
                HeaderName::from_static("x-unknown"),
            ), v);
        }
    }
    out
}

#[async_trait::async_trait]
impl Outbound for Channel {
    fn tag(&self) -> &str {
        &self.provider.tag
    }

    fn wire(&self) -> Protocol {
        self.wire_protocol()
    }

    fn wire_for(&self, incoming: Protocol) -> Protocol {
        self.provider.wire_for(incoming)
    }

    fn supports(&self, protocol: Protocol) -> bool {
        self.provider.supports(protocol)
    }

    fn provider(&self) -> &Provider {
        &self.provider
    }

    fn prepare_at(
        &self,
        wire: Protocol,
        incoming: &http::HeaderMap,
        body: Bytes,
        stream: bool,
        path: Option<&str>,
    ) -> Result<PreparedRequest, UpstreamError> {
        // 调用方理应先用 `wire_for` 挑好协议，但 prepare_at 是 trait 上的公开方法，
        // 不能假设。这里再归一一次：否则传一个渠道不支持的协议进来，会拼出一个
        // 渠道根本没提供的路径（比如给纯 Chat 渠道拼出 /v1/messages）。
        let wire = self.provider.wire_for(wire);
        // 非对话请求带着自己的路径来，那时它说了算 —— 渠道里配的协议覆盖路径
        // 是给对话协议用的，套上去只会把它带到另一个接口。
        let url = match path {
            Some(p) => self.provider.endpoint(p),
            None => self.provider.endpoint_for(wire),
        };
        let mut headers = forwardable_headers(incoming);

        // 注入该渠道的鉴权头。
        if let Some((name, value)) = self.provider.auth_header() {
            let hv = HeaderValue::from_str(&value)
                .map_err(|e| UpstreamError::Build(format!("鉴权头含非法字符: {e}")))?;
            headers.insert(HeaderName::from_static(name), hv);
        }

        // 渠道级自定义头，优先级最高，可覆盖上面的一切。
        for (k, v) in &self.provider.extra_headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| UpstreamError::Build(format!("非法请求头名 {k}: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| UpstreamError::Build(format!("非法请求头值 {k}: {e}")))?;
            headers.insert(name, value);
        }

        Ok(PreparedRequest {
            method: http::Method::POST,
            url,
            headers,
            body,
            stream,
        })
    }

    async fn dial(&self, req: PreparedRequest) -> Result<UpstreamResponse, UpstreamError> {
        let timeout = Duration::from_millis(self.provider.timeout_ms.max(1000) as u64);

        let mut builder = self
            .client
            .request(req.method.clone(), &req.url)
            .headers(req.headers)
            .body(req.body.to_vec());

        if self.provider.timeout_ms > 0 {
            builder = builder.timeout(timeout);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| classify_reqwest_error(&e))?;

        let status = http::StatusCode::from_u16(resp.status().as_u16())
            .unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR);
        let headers = filter_response_headers(resp.headers());

        // 只有"客户端要流式 + 上游确实是流"才当流处理。
        //
        // 判据是 content-type **不是 JSON**，而不是"必须是 text/event-stream"：
        // 有些上游用 text/plain 甚至 octet-stream 发 SSE，严格要求 event-stream
        // 会把它们整段缓冲下来，长回答就没了 TTFB。反过来，上游忽略 stream=true
        // 直接返回一个 JSON 对象的情况必须挡住 —— 把一个 JSON 体当成 SSE 流喂给
        // 解析器，客户端会一个字都收不到。
        if req.stream && status.is_success() && !looks_like_json(&headers) {
            let stream = resp.bytes_stream();
            return Ok(UpstreamResponse {
                status,
                headers,
                body: UpstreamBody::Stream(Box::pin(stream)),
            });
        }

        // 非流式，或上游已经出错：把 body 读完便于记录错误信息。
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| UpstreamError::Io(e.to_string()))?;

        Ok(UpstreamResponse {
            status,
            headers,
            body: UpstreamBody::Buffered(bytes),
        })
    }
}

/// 响应头是否表明这是一个完整的 JSON 响应。
fn looks_like_json(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("json"))
        .unwrap_or(false)
}

/// 把 reqwest 的错误归类，便于决定是否换渠道重试。
fn classify_reqwest_error(e: &reqwest::Error) -> UpstreamError {
    if e.is_timeout() {
        UpstreamError::Timeout
    } else if e.is_connect() {
        UpstreamError::Connect(describe_error_chain(e))
    } else if e.is_request() || e.is_builder() {
        UpstreamError::Build(describe_error_chain(e))
    } else {
        UpstreamError::Io(describe_error_chain(e))
    }
}

/// 把一条错误链摊平成一行。
///
/// 不能只用 `to_string()`：reqwest 的 `Display` 只写到「error sending request for
/// url (...)」就停了，真正的原因（`invalid peer certificate: UnknownIssuer`、
/// `tcp connect error: Connection refused`）全在 `source()` 里。只打 Display 的话，
/// 代理没生效、代理生效但 TLS 握手被拒、上游不可达这三种完全不同的故障长得一模一样，
/// 排查得靠猜。
fn describe_error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cursor = e.source();
    while let Some(cause) = cursor {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        cursor = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::{AuthStyle, ProtocolEndpoint, ProviderKind};
    use indexmap::IndexMap;

    fn provider(kind: ProviderKind, auth: AuthStyle) -> Provider {
        Provider {
            id: 1,
            tag: "test".into(),
            name: "测试渠道".into(),
            kind,
            base_url: "https://api.example.com".into(),
            api_key: Some("secret-key".into()),
            auth_style: auth,
            protocols: Vec::new(),
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            proxy: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn channel(kind: ProviderKind, auth: AuthStyle) -> Channel {
        Channel::new(provider(kind, auth), reqwest::Client::new())
    }

    #[test]
    fn bearer_auth_sets_authorization_header() {
        let c = channel(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(
            req.headers.get("authorization").unwrap(),
            "Bearer secret-key"
        );
        assert!(req.headers.get("x-api-key").is_none());
    }

    #[test]
    fn x_api_key_auth_sets_x_api_key_header() {
        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.headers.get("x-api-key").unwrap(), "secret-key");
        assert!(req.headers.get("authorization").is_none());
    }

    #[test]
    fn incoming_authorization_is_never_forwarded() {
        // 客户端的 key 只用于本地鉴权，绝不能泄漏给上游。
        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let mut incoming = http::HeaderMap::new();
        incoming.insert(
            "authorization",
            HeaderValue::from_static("Bearer client-side-token"),
        );
        incoming.insert("x-api-key", HeaderValue::from_static("client-key"));

        let req = c.prepare(c.wire(), &incoming, Bytes::new(), false).unwrap();
        assert_eq!(req.headers.get("x-api-key").unwrap(), "secret-key");
        assert!(req.headers.get("authorization").is_none());
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let c = channel(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        let mut incoming = http::HeaderMap::new();
        incoming.insert("connection", HeaderValue::from_static("keep-alive"));
        incoming.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        incoming.insert("accept-encoding", HeaderValue::from_static("gzip"));
        incoming.insert("host", HeaderValue::from_static("localhost:8787"));

        let req = c.prepare(c.wire(), &incoming, Bytes::new(), false).unwrap();
        assert!(req.headers.get("connection").is_none());
        assert!(req.headers.get("transfer-encoding").is_none());
        assert!(req.headers.get("accept-encoding").is_none());
        assert!(req.headers.get("host").is_none());
    }

    #[test]
    fn content_type_and_anthropic_version_are_forwarded() {
        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let mut incoming = http::HeaderMap::new();
        incoming.insert("content-type", HeaderValue::from_static("application/json"));
        incoming.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        incoming.insert("anthropic-beta", HeaderValue::from_static("prompt-caching"));

        let req = c.prepare(c.wire(), &incoming, Bytes::new(), false).unwrap();
        assert_eq!(req.headers.get("content-type").unwrap(), "application/json");
        // Anthropic 要求带版本头，丢了会 400
        assert_eq!(req.headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(req.headers.get("anthropic-beta").unwrap(), "prompt-caching");
    }

    #[test]
    fn extra_headers_override_everything() {
        let mut p = provider(ProviderKind::Anthropic, AuthStyle::XApiKey);
        p.extra_headers
            .insert("x-api-key".into(), "override-key".into());
        let c = Channel::new(p, reqwest::Client::new());

        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.headers.get("x-api-key").unwrap(), "override-key");
    }

    #[test]
    fn url_follows_own_wire_protocol_not_incoming_path() {
        // 入站是 Anthropic 的 /v1/messages，但渠道是 OpenAI，就该发到 chat/completions
        let c = channel(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/chat/completions");

        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/messages");

        let c = channel(ProviderKind::OpenAiResponses, AuthStyle::Bearer);
        let req = c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/responses");
    }

    /// 造一个"同时支持 Anthropic 与 Chat 两种协议"的渠道。
    fn dual_protocol_channel() -> Channel {
        let mut p = provider(ProviderKind::Anthropic, AuthStyle::XApiKey);
        p.protocols = vec![
            ProtocolEndpoint::plain(Protocol::AnthropicMessages),
            ProtocolEndpoint::plain(Protocol::OpenAiChat),
        ];
        Channel::new(p, reqwest::Client::new())
    }

    #[test]
    fn channel_speaks_the_incoming_protocol_when_it_supports_it() {
        // 客户端说 Anthropic，渠道也支持 Anthropic —— 就该发 /v1/messages，
        // 让管线走直通。这是"优先同协议"的核心：省一次编解码，也不丢字段。
        let c = dual_protocol_channel();
        assert_eq!(c.wire_for(Protocol::AnthropicMessages), Protocol::AnthropicMessages);
        assert!(c.supports(Protocol::AnthropicMessages));
        assert!(c.supports(Protocol::OpenAiChat));

        let req = c
            .prepare(Protocol::AnthropicMessages, &http::HeaderMap::new(), Bytes::new(), false)
            .unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/messages");
    }

    #[test]
    fn channel_falls_back_to_preferred_protocol_when_unsupported() {
        // 声明里没有 Responses，才轮到转换 —— 目标是首选协议（Anthropic）。
        let c = dual_protocol_channel();
        assert!(!c.supports(Protocol::OpenAiResponses));
        assert_eq!(c.wire_for(Protocol::OpenAiResponses), Protocol::AnthropicMessages);

        // 转换时发往首选协议的路径。`prepare` 会自己把协议归一，
        // 所以即使这里传的是不被支持的 Responses，也不会拼出一个假的 /v1/responses。
        let req = c
            .prepare(Protocol::OpenAiResponses, &http::HeaderMap::new(), Bytes::new(), false)
            .unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/messages");
    }

    #[test]
    fn single_protocol_channel_behaves_exactly_as_before() {
        // 没声明过协议的渠道（老数据、预设）永远只有 kind 那一种，
        // 任何入站协议都走它 —— 行为与引入 protocols 之前完全一致。
        let c = channel(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        assert_eq!(c.wire_for(Protocol::AnthropicMessages), Protocol::OpenAiChat);
        assert_eq!(c.wire_for(Protocol::OpenAiChat), Protocol::OpenAiChat);
        assert!(!c.supports(Protocol::AnthropicMessages));
    }

    #[test]
    fn per_protocol_path_override_is_used() {
        // 服务商把接口挂在子路径下时，靠这个覆盖避免拼错 → 404。
        let mut p = provider(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        p.protocols = vec![ProtocolEndpoint {
            protocol: Protocol::OpenAiChat,
            path: Some("/api/v2/chat".into()),
        }];
        let c = Channel::new(p, reqwest::Client::new());

        let req = c
            .prepare(Protocol::OpenAiChat, &http::HeaderMap::new(), Bytes::new(), false)
            .unwrap();
        assert_eq!(req.url, "https://api.example.com/api/v2/chat");
    }

    #[test]
    fn invalid_extra_header_name_is_rejected_not_silently_dropped() {
        let mut p = provider(ProviderKind::Anthropic, AuthStyle::XApiKey);
        p.extra_headers
            .insert("bad header name".into(), "v".into());
        let c = Channel::new(p, reqwest::Client::new());
        assert!(c.prepare(c.wire(), &http::HeaderMap::new(), Bytes::new(), false).is_err());
    }

    #[test]
    fn retryable_classification() {
        assert!(UpstreamError::Timeout.is_retryable());
        assert!(UpstreamError::Connect("x".into()).is_retryable());
        assert!(UpstreamError::Status {
            status: 500,
            body: String::new()
        }
        .is_retryable());
        assert!(UpstreamError::Status {
            status: 429,
            body: String::new()
        }
        .is_retryable());
        // 400 是请求本身的问题，换渠道也一样失败
        assert!(!UpstreamError::Status {
            status: 400,
            body: String::new()
        }
        .is_retryable());
        assert!(!UpstreamError::Build("x".into()).is_retryable());
    }

    /// 手工造一条三层错误链，验证摊平逻辑本身。
    ///
    /// 刻意不真的发一个请求去拿 reqwest 的错误：那要依赖某个端口一定是关着的，
    /// 而这里要守的只是"链有没有走到底"，用假错误就能覆盖。
    #[derive(Debug, thiserror::Error)]
    #[error("error sending request for url (https://api.deepseek.com/v1/responses)")]
    struct Outer(#[source] Middle);

    #[derive(Debug, thiserror::Error)]
    #[error("client error (Connect)")]
    struct Middle(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("invalid peer certificate: UnknownIssuer")]
    struct Inner;

    #[test]
    fn error_chain_keeps_the_root_cause_that_display_drops() {
        let e = Outer(Middle(Inner));
        let described = describe_error_chain(&e);
        // 根因必须在 —— 少了它，"代理没生效"和"代理生效但证书被拒"没法区分。
        assert!(described.contains("UnknownIssuer"), "丢掉了根因: {described}");
        assert!(described.contains("client error (Connect)"), "丢掉了中间层: {described}");
        // 顺序是从外到内，读起来才是"因为 A 因为 B"。
        assert!(
            described.find("error sending request").unwrap()
                < described.find("UnknownIssuer").unwrap()
        );
    }

    #[test]
    fn a_single_layer_error_needs_no_separator() {
        // 没有 source 时不能多出一个尾巴冒号。
        assert_eq!(describe_error_chain(&Inner), "invalid peer certificate: UnknownIssuer");
    }
}
