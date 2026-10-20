//! 跨源放行（CORS）。
//!
//! 受设置里的 [`crate::config::settings::AppSettings::allow_browser_origins`] 控制，
//! 默认开：公网上的网页客户端指向本机网关是最常见的用法。关掉后这一层完全透明，
//! 由 [`super::guard`] 把跨源网页请求挡回去 —— 放行跨源等于让用户访问的每一个
//! 页面都能花掉他的渠道额度。
//!
//! 打开之后两种请求分开处理：
//!
//! - **预检**（`OPTIONS` + `Access-Control-Request-Method`）在这里直接回 204，
//!   不进管线 —— 预检不带正文，没必要让路由和 fallback 看到它。
//! - **实际请求**交给下游，回程时补上 `Access-Control-Allow-Origin`。补在回程
//!   而不是入口，是因为错误响应（403 / 502 / 上游 4xx）也得能让浏览器读到；
//!   只在成功路径上加，用户看到的永远是一句没头没尾的 "CORS error"。
//!
//! 用 `*` 而不是回显 Origin：客户端的鉴权走请求头（`x-api-key` /
//! `authorization`），不用 Cookie，所以不需要 `Allow-Credentials`；`*` 也省掉了
//! 一张源站清单要跟着用户改的问题。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};

use crate::shell::AppShell;

/// 预检结果的缓存时长。取常见默认值：太短会让浏览器每次都先发一轮 OPTIONS，
/// 太长会让用户改完设置后仍被旧的预检结果挡住。
const MAX_AGE_SECS: &str = "600";

pub async fn cors(
    State(shell): State<Arc<AppShell>>,
    req: Request,
    next: Next,
) -> Response {
    // 每请求读一次：这个开关改完立刻生效，不用重启网关（路由表是启动时建的）。
    if !shell.settings().allow_browser_origins {
        return next.run(req).await;
    }

    if req.method() == Method::OPTIONS {
        if let Some(res) = preflight(req.headers()) {
            return res;
        }
    }

    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("*"),
    );
    res
}

/// 这是一条预检请求吗？是就给出应答，不是返回 `None` 交给下游。
///
/// 判据是 `Access-Control-Request-Method`：只有浏览器发的预检才有它，普通
/// 跨源 OPTIONS（少见）不该被我们吞掉。
fn preflight(req_headers: &HeaderMap) -> Option<Response> {
    req_headers.get(header::ACCESS_CONTROL_REQUEST_METHOD)?;

    let mut res = StatusCode::NO_CONTENT.into_response();
    let headers = res.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(MAX_AGE_SECS),
    );
    // 回显浏览器要用的头（`x-api-key`、`anthropic-version`、`anthropic-beta`……）。
    // 写一张固定清单必然漏掉某个客户端的新头，而漏掉时浏览器只报一句笼统的
    // "doesn't pass access control check"。
    //
    // 非法值退回 `*`：`*` 对非凭据请求同样合法，且比直接报错实用。
    let allow_headers = req_headers
        .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok())
        .unwrap_or_else(|| HeaderValue::from_static("*"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allow_headers);

    // Chrome 的 Private Network Access：公网页面访问 127.0.0.1 时会在预检里
    // 带上 `Access-Control-Request-Private-Network`，不明确同意就会被拦下 ——
    // 那是另一句同样笼统的 CORS 报错。这里一律同意（开关本身就代表用户同意）。
    headers.insert(
        HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );

    Some(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn allow(h: &HeaderMap, name: &str) -> Option<String> {
        h.get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[test]
    fn a_plain_options_is_not_a_preflight() {
        // 没有 Access-Control-Request-Method 的 OPTIONS 不是浏览器预检，
        // 吞掉它会改变普通请求的语义。
        assert!(preflight(&headers(&[])).is_none());
        assert!(preflight(&headers(&[("origin", "https://lm.example.com")])).is_none());
    }

    #[test]
    fn preflight_allows_any_origin_and_echoes_requested_headers() {
        let res = preflight(&headers(&[
            ("origin", "https://lm.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type, x-api-key, anthropic-version"),
        ]))
        .expect("应当识别为预检");

        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let h = res.headers();
        assert_eq!(allow(h, "access-control-allow-origin").as_deref(), Some("*"));
        assert_eq!(
            allow(h, "access-control-allow-headers").as_deref(),
            Some("content-type, x-api-key, anthropic-version")
        );
        assert_eq!(
            allow(h, "access-control-allow-private-network").as_deref(),
            Some("true")
        );
        assert!(allow(h, "access-control-allow-methods")
            .unwrap()
            .contains("POST"));
    }

    #[test]
    fn preflight_without_requested_headers_falls_back_to_wildcard() {
        let res = preflight(&headers(&[("access-control-request-method", "POST")])).unwrap();
        assert_eq!(
            allow(res.headers(), "access-control-allow-headers").as_deref(),
            Some("*")
        );
    }
}
