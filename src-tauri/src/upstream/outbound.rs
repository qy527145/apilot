//! 出站（上游渠道）抽象。

use axum::body::Bytes;
use futures::stream::BoxStream;

use crate::protocol::dto::Protocol;
use crate::storage::models::Provider;

/// 一个已组装完毕、可直接发出的上游请求。
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub method: http::Method,
    pub url: String,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub stream: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("连接上游失败: {0}")]
    Connect(String),

    #[error("上游超时")]
    Timeout,

    #[error("上游返回 {status}: {body}")]
    Status { status: u16, body: String },

    #[error("构造请求失败: {0}")]
    Build(String),

    #[error("读取上游响应失败: {0}")]
    Io(String),
}

impl UpstreamError {
    /// 该错误是否值得换一个渠道重试。
    ///
    /// 4xx（除 429）是请求本身的问题，换渠道也是同样结果，重试只会浪费时间和配额。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Connect(_) | Self::Timeout | Self::Io(_) => true,
            Self::Status { status, .. } => *status >= 500 || *status == 429,
            Self::Build(_) => false,
        }
    }
}

pub enum UpstreamBody {
    Buffered(Bytes),
    Stream(BoxStream<'static, reqwest::Result<Bytes>>),
}

pub struct UpstreamResponse {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: UpstreamBody,
}

impl UpstreamResponse {
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }
}

/// 上游渠道的统一接口。
///
/// 目前只有 HTTP 渠道一种实现；抽成 trait 是为了后续能加入本地 mock、
/// 缓存渠道或其它传输方式而不改动路由层。
#[async_trait::async_trait]
pub trait Outbound: Send + Sync {
    /// 路由与 selector 引用的标识。
    fn tag(&self) -> &str;

    /// 该渠道的**首选**线协议。探测、拉模型列表，以及入站协议不被支持时的
    /// 转换目标，都用它。真正发请求时用 [`Outbound::wire_for`]。
    fn wire(&self) -> Protocol;

    /// 针对入站协议，实际该用哪种线协议。
    ///
    /// 能同协议就同协议 —— 此时管线走直通（原始字节转发），零转换；
    /// 对不上才回落到首选协议，由管线做跨协议转换。
    fn wire_for(&self, incoming: Protocol) -> Protocol;

    /// 该渠道是否原生支持某种协议。
    fn supports(&self, protocol: Protocol) -> bool;

    /// 渠道配置。
    fn provider(&self) -> &Provider;

    /// 组装出站请求：按 `wire` 拼 URL、注入鉴权头、应用渠道级 header 覆盖。
    ///
    /// 出站路径由 `wire` 决定，与入站路径无关 —— 入站是 Anthropic 协议时，
    /// 发往 OpenAI 渠道就应该走 `/v1/chat/completions`。
    fn prepare(
        &self,
        wire: Protocol,
        incoming: &http::HeaderMap,
        body: Bytes,
        stream: bool,
    ) -> Result<PreparedRequest, UpstreamError> {
        self.prepare_at(wire, incoming, body, stream, None)
    }

    /// 同上，但可以指定出站路径。
    ///
    /// 非对话请求（`/v1/messages/count_tokens` 这类）没有 IR 可言，只能原样转发，
    /// 而它们的路径不是任何协议的默认路径 —— 那时由调用方把客户端那条路径传进来。
    /// `None` 表示"用协议默认路径"，也就是 [`Outbound::prepare`] 一直以来的行为。
    fn prepare_at(
        &self,
        wire: Protocol,
        incoming: &http::HeaderMap,
        body: Bytes,
        stream: bool,
        path: Option<&str>,
    ) -> Result<PreparedRequest, UpstreamError>;

    /// 发出请求。
    async fn dial(&self, req: PreparedRequest) -> Result<UpstreamResponse, UpstreamError>;
}
