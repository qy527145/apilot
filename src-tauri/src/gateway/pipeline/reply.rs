//! 回给客户端的响应与错误报文。
//!
//! 失败路径有个硬要求：**先**把要发出去的那几行字节落进捕获，再返回。
//! 少了它，监控页里「上游 → Apilot」有原文、「Apilot → 客户端」却空白，
//! 用户分不清那句报错是谁说的。

use axum::body::Bytes;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;

use crate::protocol::dto::Protocol;
use crate::storage::logs::CaptureRecord;
use crate::upstream::outbound::UpstreamError;

use super::recorder::Recorder;

pub(super) fn build_response(status: http::StatusCode, headers: HeaderMap, body: Bytes) -> Response {
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

fn error_payload(protocol: Protocol, title: &str, detail: &str) -> serde_json::Value {
    // 各协议的错误体结构不同；按客户端预期返回，它才能正确展示。
    match protocol {
        Protocol::AnthropicMessages => serde_json::json!({
            "type": "error",
            "error": { "type": "api_error", "message": format!("{title}: {detail}") },
        }),
        _ => serde_json::json!({
            "error": { "message": format!("{title}: {detail}"), "type": "api_error" },
        }),
    }
}

/// 数字状态码 → `http::StatusCode`。
///
/// 认不出的取值一律当 500：回给客户端一个"状态码是 0"的响应比回 500 更难排查。
/// 抽出来是因为失败路径的每一处都要走这一遭，各处 `unwrap_or` 一次迟早会分叉。
fn http_code(status: u16) -> http::StatusCode {
    http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR)
}

/// 失败收口：**先**把要回给客户端的那份报文落进捕获，再返回响应。
///
/// 失败路径上这一步不能省。成功时客户端那一栏本来就是上游那一份（或它的重编码），
/// 而失败时它写的是我们包装过的错误 —— 少了它，监控页里「上游 → Apilot」有原文、
/// 「Apilot → 客户端」却空空如也，用户只能猜那句报错是谁说的。这正是"看不出问题
/// 出在 Apilot 还是上游"的来源。
pub(super) fn fail_response(
    recorder: &mut Recorder,
    protocol: Protocol,
    status: u16,
    title: &str,
    detail: &str,
) -> Response {
    fail_with_payload(
        recorder,
        status,
        error_payload(protocol, title, detail),
        &format!("{title}: {detail}"),
    )
}

/// 同上，给原样转发那条路用。
///
/// 不用 [`error_payload`]：那套按协议族拼 JSON（Anthropic 是 `{"type":"error",…}`，
/// OpenAI 是 `{"error":{…}}`），而这条路连客户端说哪种协议都不知道 ——
/// 用一个三方都认得出的形状，别猜。
pub(super) fn fail_raw_response(recorder: &mut Recorder, status: u16, title: &str, detail: &str) -> Response {
    let message = format!("{title}：{detail}");
    fail_with_payload(
        recorder,
        status,
        serde_json::json!({ "error": { "type": "apilot_error", "message": message } }),
        &message,
    )
}

/// 共同的那一半：记下回给客户端的字节 → 结账 → 拼响应。
///
/// 正文用 `to_vec` 固化下来而不是交给 `axum::Json` 现序列化：捕获要的那份
/// 必须与真正发出去的那份逐字节相同，两处各序列化一次就有分叉的余地。
fn fail_with_payload(
    recorder: &mut Recorder,
    status: u16,
    payload: serde_json::Value,
    message: &str,
) -> Response {
    let body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());

    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    record_client_response(recorder.capture.as_mut(), &headers, &body);

    recorder.fail(status as i32, message.to_string());
    (http_code(status), headers, body).into_response()
}

/// 记下**最终回给客户端的那份报文**。
///
/// 成功时它与上游那一份（或它的重编码）基本一致，看不出来也不打紧；失败时它是
/// 唯一能说明"客户端到底收到了什么"的地方 —— 所以每条收口路径都得走一遍这里。
/// 抽成一处而不是散在几处手写：这是一组必须同时写对的字段，漏一个就是监控页
/// 那一栏半空着，而用户只会以为"网关什么都没回"。
pub(super) fn record_client_response(capture: Option<&mut CaptureRecord>, headers: &HeaderMap, body: &[u8]) {
    if let Some(c) = capture {
        c.response_headers = headers_to_json(headers);
        c.response_body = Some(body.to_vec());
    }
}

/// 最终回给客户端的状态码。
///
/// 上游**真的回了 HTTP 响应**时原样透传它的码。折成 502 会让客户端把"这次请求
/// 的格式不对"（400）当成网关抽风而反复重试，也让用户分不清那句报错是谁说的；
/// 连不上、超时这类**根本没有响应**的失败才是 502。
pub(super) fn client_status_for(last_error: Option<&UpstreamError>) -> u16 {
    match last_error {
        // 只认 4xx/5xx：3xx 之类出现在这条路上说明上游行为异常，
        // 把 302 透给客户端只会更莫名其妙。
        Some(UpstreamError::Status { status, .. }) if (400..600).contains(status) => *status as u16,
        _ => 502,
    }
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

pub(super) fn headers_to_json(headers: &HeaderMap) -> serde_json::Value {
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

    #[test]
    fn known_status_codes_map_to_themselves() {
        assert_eq!(http_code(400), http::StatusCode::BAD_REQUEST);
        assert_eq!(http_code(503), http::StatusCode::SERVICE_UNAVAILABLE);
        // 认不出的取值不能变成"状态码 0"这种客户端看不懂的东西。
        assert_eq!(http_code(0), http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(http_code(99), http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    // --- 失败路径的可见性 ---

    /// 上游**真的回了响应**时，状态码原样交给客户端。
    ///
    /// 以前一律折成 502：客户端于是把"你的请求格式不对"当成网关抽风反复重试，
    /// 用户也分不清那句报错出自 Apilot 还是上游。
    #[test]
    fn an_upstream_status_is_passed_through_verbatim() {
        for status in [400, 401, 403, 404, 413, 429, 500, 502, 529] {
            let e = UpstreamError::Status {
                status,
                body: "x".into(),
            };
            assert_eq!(
                client_status_for(Some(&e)),
                status,
                "{status} 该原样透传"
            );
        }
    }

    #[test]
    fn only_failures_without_a_response_fall_back_to_502() {
        // 连不上 / 超时 / 组建失败：上游什么都没说，502 才是实话。
        for e in [
            UpstreamError::Timeout,
            UpstreamError::Deadline("等待上游响应头超时（120 秒）".into()),
            UpstreamError::Connect("refused".into()),
            UpstreamError::Build("bad".into()),
            UpstreamError::Io("broken".into()),
        ] {
            assert_eq!(client_status_for(Some(&e)), 502, "{e} 该回落成 502");
        }
        // 一个渠道都没试过（理论上不会走到，但别让它变成 200）。
        assert_eq!(client_status_for(None), 502);
    }

    /// 上游回了个非 4xx/5xx 的"错误状态"（比如 302）时不能透传：
    /// 把重定向原样甩给客户端只会更莫名其妙。
    #[test]
    fn a_non_error_status_never_leaks_through_as_is() {
        let e = UpstreamError::Status {
            status: 302,
            body: "redirect".into(),
        };
        assert_eq!(client_status_for(Some(&e)), 502);
    }

    /// 回给客户端的那份报文必须落进捕获。
    ///
    /// 少了它，监控页里「上游 → Apilot」有原文、「Apilot → 客户端」却是空的 ——
    /// 用户只能猜那句报错是谁说的，这正是"看不出 apilot 层还是上游"的来源。
    #[test]
    fn the_client_facing_response_is_recorded_verbatim() {
        let mut cap = CaptureRecord::default();
        let headers = headers_with(&[("content-type", "application/json")]);
        let body = br#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#;

        record_client_response(Some(&mut cap), &headers, body);

        assert_eq!(
            cap.response_body.as_deref(),
            Some(&body[..]),
            "正文要一个字节不差"
        );
        assert_eq!(cap.response_headers["content-type"], "application/json");
    }

    /// 没开捕获时也不该 panic —— 那条路上 `capture` 就是 `None`。
    #[test]
    fn recording_the_client_response_without_a_capture_is_a_no_op() {
        let headers = headers_with(&[("x", "y")]);
        record_client_response(None, &headers, b"{}");
    }

    /// 错误正文里要带上说明，否则 raw 视图看到的只是一句没头没尾的错。
    #[test]
    fn the_error_body_carries_the_detail() {
        let v = error_payload(Protocol::AnthropicMessages, "上游请求失败", "上游返回 400: 不支持");
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "api_error");
        assert_eq!(v["error"]["message"], "上游请求失败: 上游返回 400: 不支持");

        // OpenAI 系要的是另一个形状，客户端才认。
        let v = error_payload(Protocol::OpenAiChat, "上游请求失败", "boom");
        assert_eq!(v["error"]["message"], "上游请求失败: boom");
        assert!(v.get("type").is_none());
    }
}
