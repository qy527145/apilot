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

    /// 该渠道的线协议。
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

    fn provider(&self) -> &Provider {
        &self.provider
    }

    fn prepare(
        &self,
        incoming: &http::HeaderMap,
        body: Bytes,
        stream: bool,
    ) -> Result<PreparedRequest, UpstreamError> {
        let url = self.provider.endpoint(self.wire().default_path());
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
            .post(&req.url)
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
        UpstreamError::Connect(e.to_string())
    } else if e.is_request() || e.is_builder() {
        UpstreamError::Build(e.to_string())
    } else {
        UpstreamError::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::{AuthStyle, ProviderKind};
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
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
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
        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(
            req.headers.get("authorization").unwrap(),
            "Bearer secret-key"
        );
        assert!(req.headers.get("x-api-key").is_none());
    }

    #[test]
    fn x_api_key_auth_sets_x_api_key_header() {
        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
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

        let req = c.prepare(&incoming, Bytes::new(), false).unwrap();
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

        let req = c.prepare(&incoming, Bytes::new(), false).unwrap();
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

        let req = c.prepare(&incoming, Bytes::new(), false).unwrap();
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

        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.headers.get("x-api-key").unwrap(), "override-key");
    }

    #[test]
    fn url_follows_own_wire_protocol_not_incoming_path() {
        // 入站是 Anthropic 的 /v1/messages，但渠道是 OpenAI，就该发到 chat/completions
        let c = channel(ProviderKind::OpenAiChat, AuthStyle::Bearer);
        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/chat/completions");

        let c = channel(ProviderKind::Anthropic, AuthStyle::XApiKey);
        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/messages");

        let c = channel(ProviderKind::OpenAiResponses, AuthStyle::Bearer);
        let req = c.prepare(&http::HeaderMap::new(), Bytes::new(), false).unwrap();
        assert_eq!(req.url, "https://api.example.com/v1/responses");
    }

    #[test]
    fn invalid_extra_header_name_is_rejected_not_silently_dropped() {
        let mut p = provider(ProviderKind::Anthropic, AuthStyle::XApiKey);
        p.extra_headers
            .insert("bad header name".into(), "v".into());
        let c = Channel::new(p, reqwest::Client::new());
        assert!(c.prepare(&http::HeaderMap::new(), Bytes::new(), false).is_err());
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
}
