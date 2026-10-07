//! 脱离网关管线的单次真实请求。
//!
//! 「模型测试」和「能力探测」都要往某个 (渠道, 模型) 真发一次请求，但**都不能
//! 走网关管线**：管线会写请求日志、做预扣结算、查响应缓存、失败时换渠道。对一次
//! 探测来说这些都是副作用 —— 监控页会混进用户没发过的"请求"，账单上凭空多出
//! 几笔，缓存还可能把探测结果留下来影响真实流量。
//!
//! 所以这里直接拼出站层的原语：协议编码 → [`Outbound::prepare`] → [`Outbound::dial`]，
//! 中间不经过 pipeline。代价是绕开了管线的便利（重试、故障转移），但探测要的
//! 恰恰是**这条渠道本身**的结果，换渠道反而是错的。

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use serde::Serialize;

use crate::protocol::dto::{Protocol, UnifiedRequest, UnifiedResponse, UnifiedUsage};
use crate::shell::AppShell;
use crate::storage::models::Provider;
use crate::upstream::outbound::UpstreamBody;

/// 原始回包预览的截断长度。
///
/// 够看清上游到底回了什么（错误信息、模型名、被剥掉的字段），又不至于把一个
/// 几万 token 的回答塞进界面。
const PREVIEW_CHARS: usize = 600;

/// 一次真实请求的结果。
///
/// 和 `commands::providers::ProbeResult` 的区别：那个测的是**渠道连通性**
/// （打 `GET /v1/models`，不指定模型、不消耗配额），这个测的是**某个模型能不能
/// 真的对话**（走协议编码、消耗配额）。两者互补，不要互相替代。
#[derive(Debug, Clone, Serialize)]
pub struct OneshotOutcome {
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: i64,
    /// 实际用的线协议。渠道声明的协议集合决定它，不是入站协议。
    pub wire: Protocol,
    /// 出站 URL。排查"怎么打到这个地址去了"时必须有。
    pub url: String,
    /// 上游原始回包的开头。
    pub preview: String,
    /// 解出来的回答文本，供界面直接显示。
    pub text: String,
    /// 本次消耗。拿不到就是 None —— 上游不回 usage 是常态。
    pub usage: Option<UnifiedUsage>,
    pub error: Option<String>,
    /// 解码后的完整响应。**不跨 IPC 传**，只在进程内给能力探测判断有没有
    /// 工具调用 / 思考块用。
    #[serde(skip)]
    pub decoded: Option<UnifiedResponse>,
}

impl OneshotOutcome {
    fn failed(latency_ms: i64, wire: Protocol, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: None,
            latency_ms,
            wire,
            url: String::new(),
            preview: String::new(),
            text: String::new(),
            usage: None,
            error: Some(error.into()),
            decoded: None,
        }
    }
}

/// 往某个渠道真发一次请求，并尽量把回答解出来。
///
/// `provider` 与 `req.model` 的关系：**`req.model` 必须是上游认得的名字**，
/// 也就是已经过 `Provider::upstream_model` 映射的那个。这里不做映射 ——
/// 调用方可能故意要测声明名或上游名，替它猜只会掩盖问题。
///
/// 用的是渠道自己的代理（`Channel` 建的时候就绑好了），所以探测结论与真实
/// 转发一致。这条很关键：否则会出现"测试通过但真实请求连不上"这种没法排查的矛盾。
pub async fn send(
    shell: &Arc<AppShell>,
    provider: &Provider,
    req: &UnifiedRequest,
) -> OneshotOutcome {
    let started = Instant::now();

    let Some(outbound) = shell.registry.get(&provider.tag) else {
        return OneshotOutcome::failed(
            elapsed(&started),
            Protocol::AnthropicMessages,
            format!("渠道 {} 未注册（可能已被删除或停用）", provider.tag),
        );
    };

    // 用渠道自己的首选协议，不用入站协议 —— 探测问的是"这条渠道能不能干活"，
    // 它最擅长的那个协议才代表上限。
    let wire = outbound.wire();

    let body = match shell.codecs.codec(wire).encode_request(req) {
        Ok(b) => Bytes::from(b),
        Err(e) => {
            return OneshotOutcome::failed(elapsed(&started), wire, format!("编码请求失败：{e}"))
        }
    };

    // 必须自己带上 content-type：`prepare_at` 只转发调用方给的头，不会替你补。
    // 少了它绝大多数上游直接 400，而且报的是"格式错误"，看不出真正原因。
    let mut incoming = http::HeaderMap::new();
    incoming.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );

    let prepared = match outbound.prepare(wire, &incoming, body, false) {
        Ok(p) => p,
        Err(e) => return OneshotOutcome::failed(elapsed(&started), wire, e.to_string()),
    };
    let url = prepared.url.clone();

    let resp = match outbound.dial(prepared).await {
        Ok(r) => r,
        Err(e) => {
            let mut out = OneshotOutcome::failed(elapsed(&started), wire, e.to_string());
            out.url = url;
            return out;
        }
    };

    let latency_ms = elapsed(&started);
    let status = resp.status.as_u16();

    // 探测一律非流式，所以这里只可能是 Buffered。真出现流就说明渠道无视了
    // stream=false —— 不读它，直接按"没拿到可用的回包"处理。
    let bytes = match resp.body {
        UpstreamBody::Buffered(b) => b,
        UpstreamBody::Stream(_) => Bytes::new(),
    };

    let preview: String = String::from_utf8_lossy(&bytes)
        .chars()
        .take(PREVIEW_CHARS)
        .collect();

    if !resp.status.is_success() {
        // 非 2xx 时优先用协议解码器把上游的错误说明提出来，比一坨原始 JSON 有用。
        let msg = shell
            .codecs
            .codec(wire)
            .extract_error_message(&bytes);
        return OneshotOutcome {
            ok: false,
            status: Some(status),
            latency_ms,
            wire,
            url,
            preview,
            text: String::new(),
            usage: None,
            error: Some(format!("上游返回 {status}：{msg}")),
            decoded: None,
        };
    }

    // 解不出来不算失败：HTTP 层面已经成功了，只是这个渠道的回包不合协议。
    // 那本身也是有用的信息（说明渠道有问题），但别把它报成"请求失败"。
    let (decoded, text, usage) = match shell.codecs.codec(wire).decode_response(&bytes) {
        Ok((resp, usage)) => {
            let text = resp.concat_text();
            (Some(resp), text, Some(usage))
        }
        Err(e) => {
            return OneshotOutcome {
                ok: true,
                status: Some(status),
                latency_ms,
                wire,
                url,
                preview,
                text: String::new(),
                usage: None,
                error: Some(format!("回包解不出来（{e}）—— 渠道返回的内容不合协议")),
                decoded: None,
            }
        }
    };

    OneshotOutcome {
        ok: true,
        status: Some(status),
        latency_ms,
        wire,
        url,
        preview,
        text,
        usage,
        error: None,
        decoded,
    }
}

fn elapsed(started: &Instant) -> i64 {
    started.elapsed().as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_is_truncated_rather_than_unbounded() {
        // 上游回来几万 token 的回答时不能整段塞进界面。
        let long = "x".repeat(PREVIEW_CHARS * 3);
        let cut: String = long.chars().take(PREVIEW_CHARS).collect();
        assert_eq!(cut.chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn failure_carries_the_reason_and_zeroed_fields() {
        let o = OneshotOutcome::failed(12, Protocol::OpenAiChat, "连接超时");
        assert!(!o.ok);
        assert_eq!(o.latency_ms, 12);
        assert_eq!(o.error.as_deref(), Some("连接超时"));
        assert!(o.text.is_empty() && o.preview.is_empty() && o.decoded.is_none());
        assert_eq!(o.status, None, "压根没连上，不该编一个状态码出来");
    }

    #[test]
    fn decoded_response_is_not_serialized() {
        // 完整响应体可能很大，只该在进程内用于能力判断，不该跨 IPC 传。
        let o = OneshotOutcome::failed(0, Protocol::AnthropicMessages, "x");
        let json = serde_json::to_value(&o).unwrap();
        assert!(json.get("decoded").is_none());
    }
}
