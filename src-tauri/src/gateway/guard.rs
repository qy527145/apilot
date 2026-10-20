//! 入站防护：挡住浏览器里的网页与 DNS rebinding。
//!
//! 网关默认只监听回环，但「只有本机能连」并不等于「只有本机客户端能连」：
//! 用户访问的任意网页都能向 `127.0.0.1:8787` 发一个跨域 POST。跨域只挡读响应，
//! 挡不住请求本身 —— 那个 POST 会照样花掉他配好的渠道额度。把域名解析到
//! 127.0.0.1 的页面（DNS rebinding）在浏览器眼里还是同源的，连响应都读得到。
//!
//! 两条判据，都不需要用户配置：
//!
//! - **Origin**：Agent 客户端不发这个头，发的都是网页。回环之外的网页一律拒。
//! - **Host**：带浏览器标记的请求，Host 必须是本机的名字（IP 字面量 / localhost /
//!   容器给宿主的名字）。rebinding 靠的正是"名字"，而 IP 字面量解析不出别处去。
//!
//! 这里只管浏览器。局域网里另一台机器上的 Agent 客户端两点都不满足（它不发
//! Origin，Host 是网关的 IP），所以这条防线对它是透明的 —— 那台机器的准入是
//! 另一个问题，见后续的网关密钥。
//!
//! 设置里的 [`crate::config::settings::AppSettings::allow_browser_origins`] 是**明确的逃生门**：用户要把
//! 部署在公网上的网页客户端指向本机网关时（`https://lm.example.com` →
//! `http://127.0.0.1:8787`），Origin 判据必须让路。打开后任何网页都能花掉他的
//! 渠道额度，所以默认关闭。Host 那条判据**不让路** —— 合法跨域请求的 Host 就是
//! 网关自己的地址，而 rebinding 页面带的是它自己的域名，仍旧拦得住。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode};

use crate::shell::AppShell;

/// 本地容器运行时给宿主的名字。
///
/// 容器里的客户端用它连宿主上的网关，而我们自己的 base_url 写的是 127.0.0.1，
/// 所以这些名字**只能来自用户手填** —— 是明确指令，认。
const CONTAINER_HOSTS: &[&str] = &[
    "host.docker.internal",
    "gateway.docker.internal",
    "host.containers.internal",
    "host.orb.internal",
    "docker.for.mac.localhost",
];

/// 浏览器一定会带、而页面脚本去不掉的头。
///
/// `Origin` 之外还有 `Sec-Fetch-*`：Fetch 元数据由浏览器生成，脚本改不了。
/// 同一页面的同源 POST 也会带 `Origin`，所以这两组一起用，rebinding 那条路
/// 不会因为"它自认为是同源"而漏掉。
const BROWSER_HEADERS: &[&str] = &["origin", "sec-fetch-site", "sec-fetch-mode"];

/// 中间件入口。判为不放行时直接回 403，不进管线 —— 被拒的请求不该产生
/// 请求日志、捕获或任何计费痕迹，否则用户的历史里会混进别人的请求。
pub async fn guard(
    State(shell): State<Arc<AppShell>>,
    req: Request,
    next: Next,
) -> Response {
    // 每请求读一次：这个开关改完立刻生效，不用重启网关（路由表是启动时建的）。
    let allow_any_origin = shell.settings().allow_browser_origins;
    match refusal(req.headers(), allow_any_origin) {
        None => next.run(req).await,
        Some(reason) => {
            tracing::warn!(path = %req.uri().path(), "{reason}");
            refuse_response(&reason)
        }
    }
}

/// 403 的响应体。
///
/// 不用 `pipeline::error_payload`：那是按客户端协议拼的，而这条路在解析出协议
/// 之前就拒了，猜错形状会让客户端把 403 当成解析错误。用一个三方都读得出的形状。
fn refuse_response(reason: &str) -> Response {
    let body = serde_json::json!({
        "error": { "type": "apilot_error", "message": reason },
    });
    (
        StatusCode::FORBIDDEN,
        [(http::header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
    )
        .into_response()
}

/// 该拒绝吗？`None` = 放行；`Some(原因)` 会原样展示给用户。
///
/// `allow_any_origin` 只豁免 Origin 判据；Host 判据照旧（见模块注释）。
fn refusal(headers: &HeaderMap, allow_any_origin: bool) -> Option<String> {
    if !allow_any_origin {
        if let Some(origin) = header(headers, "origin") {
            if let Some(reason) = judge_origin(&origin, headers) {
                return Some(reason);
            }
        }
    }

    if browserish(headers) {
        let host = header(headers, "host").unwrap_or_default();
        if !own_host(&host) {
            return Some(format!(
                "拒绝了来自 {host} 的请求：浏览器没在用它以为的地址访问本机（DNS rebinding 的典型形状）。\
                 请用 localhost 或 127.0.0.1 访问网关。"
            ));
        }
    }

    None
}

/// 判断一个 Origin 该不该拒。
fn judge_origin(origin: &str, headers: &HeaderMap) -> Option<String> {
    // 脚本去不掉的那个字面量：沙箱 iframe 和本地文件都会发它，任何网页都发得出。
    if origin.eq_ignore_ascii_case("null") {
        return Some(
            "拒绝了 Origin: null 的请求：沙箱 iframe 或本地文件都能发出它，\
             而它们可能来自任何地方。Agent 客户端不发 Origin。"
                .to_string(),
        );
    }

    let lower = origin.to_ascii_lowercase();
    // 非 http(s) 的 scheme（tauri://、file://、chrome-extension://）只有桌面应用、
    // 编辑器 webview 和浏览器扩展发得出，普通网页放不上请求头，按原样放行。
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return None;
    }

    let host = origin_host(&lower)?;

    // localhost 的子域要先拦：Chrome 和 Firefox 按规则把 *.localhost 解析到回环，
    // 但 Safari 与其他 WebKit 视图会去问系统解析器 —— 网络里的解析器一答，那个
    // 页面就可能来自任何地方。它既不是 `localhost`，也不是 IP 字面量，
    // **不能**落到下面"看着像本机"的判据里。
    if host != "localhost" && host.ends_with(".localhost") {
        return Some(format!(
            "拒绝了网页 {origin} 发来的请求：*.localhost 这个名字未必解析在本机\
             （Safari 等 WebKit 浏览器会问系统解析器）。请用 localhost 或 127.0.0.1。"
        ));
    }

    // 页面就在网关自己的地址上：用户把网关绑到 0.0.0.0 后，在另一台机器上用
    // `http://192.168.1.5:8787` 打开自己写的调用页 —— 那是明确指令。**这一条必须
    // 在"回环才算本机"之前**，否则网关的局域网地址会被当成外来页面。
    if let Some(own) = header(headers, "host") {
        if same_address(&own, &host, origin_port(&lower)) {
            return None;
        }
    }

    if loopbackish(&host) {
        return None;
    }

    Some(format!(
        "拒绝了网页 {origin} 发来的请求：网关只服务本机客户端。\
         需要让网页调用网关时，请把页面放在本机，或用 localhost / 127.0.0.1 访问。"
    ))
}

/// 回环或 `localhost`。局域网 IP 字面量**不算** —— 另一台机器上的页面正是它。
fn loopbackish(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// 带浏览器标记？WebKit 之外还有些客户端会带 `Mozilla/` 的 UA。
fn browserish(headers: &HeaderMap) -> bool {
    if BROWSER_HEADERS.iter().any(|h| header(headers, h).is_some()) {
        return true;
    }
    header(headers, "user-agent")
        .map(|ua| ua.starts_with("Mozilla/"))
        .unwrap_or(false)
}

/// `host`（Host 头或 Origin 里的主机名）是不是本机的名字。
///
/// 判据是"它解析不出别的机器去"：IP 字面量天然满足；`localhost` 按名字由浏览器
/// 自己折成回环（这是 RFC 6761 的规定，不经过 DNS）；容器给宿主的名字是用户手填的
/// 明确指令。其余名字一律不认 —— 域名正是 rebinding 唯一的载体。
fn own_host(host: &str) -> bool {
    let h = normalize_host(host);
    if h.is_empty() {
        return false;
    }
    if h == "localhost" || CONTAINER_HOSTS.contains(&h.as_str()) {
        return true;
    }
    // 通配地址也认：能连上它说明连接已经落在本机了（0.0.0.0 在任何系统上
    // 都表示"本机的所有地址"，解析不出第二个去处）。
    h.parse::<std::net::IpAddr>().is_ok()
}

/// 主机名规范化：小写、去方括号、去端口、去末尾的点。
fn normalize_host(host: &str) -> String {
    let h = host.trim();
    // IPv6 字面量带方括号，也可能带端口（`[::1]:8787`）。
    let h = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => inner,
            None => rest,
        }
    } else {
        h
    };
    let h = h.to_ascii_lowercase();
    // 末尾的点是 FQDN 写法（`localhost.`），等价于不带。
    let h = h.trim_end_matches('.');
    // 裸 IPv6（`::1`）里每个 `:` 都是地址的一部分，不能当端口分隔符。
    if h.matches(':').count() == 1 {
        return h.split(':').next().unwrap_or("").to_string();
    }
    h.to_string()
}

/// 取 Origin 里的主机名。解析不出就返回 `None`（调用方原样放行 —— 认不出的形状
/// 不该被当成恶意，但也不是本机）。
fn origin_host(origin: &str) -> Option<String> {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))?;
    // 取到第一个 `/` 为止，再交给 `normalize_host` 处理端口。
    let authority = rest.split('/').next().unwrap_or("");
    let host = normalize_host(authority);
    (!host.is_empty()).then_some(host)
}

/// Origin 里的端口。没写就是 scheme 的默认端口。
fn origin_port(origin: &str) -> Option<u16> {
    let (scheme, rest) = if origin.starts_with("https://") {
        (443u16, &origin[8..])
    } else if origin.starts_with("http://") {
        (80u16, &origin[7..])
    } else {
        return None;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.starts_with('[') {
        // IPv6：端口在 `]` 之后。
        return match authority.split_once(']') {
            Some((_, after)) => after
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .or(Some(scheme)),
            None => Some(scheme),
        };
    }
    match authority.rsplit_once(':') {
        Some((_, port)) => port.parse().ok(),
        None => Some(scheme),
    }
}

/// Host 头与 Origin 是不是同一个地址（含端口）。
///
/// 端口也要比：本机 5173 上的页面与网关 8787 不同源，不算"页面就在网关地址上"。
/// 但那种情况会落到下面的回环判据里放行 —— 本机页面拦不住也没有意义。
fn same_address(host_header: &str, origin_hostname: &str, origin_port: Option<u16>) -> bool {
    let Some(port) = origin_port else {
        return false;
    };
    let own = normalize_host(host_header);
    if own != origin_hostname {
        return false;
    }
    // Host 头一般带端口；不带时按 scheme 默认端口比对会误判，宁可不认。
    port_from_authority(host_header).map(|p| p == port).unwrap_or(false)
}

/// 从 `host:port` / `[::1]:port` 里取端口。
fn port_from_authority(authority: &str) -> Option<u16> {
    if authority.starts_with('[') {
        return authority
            .split_once(']')
            .and_then(|(_, after)| after.strip_prefix(':'))
            .and_then(|p| p.parse().ok());
    }
    if authority.matches(':').count() == 1 {
        return authority
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok());
    }
    None
}

/// 取头（小写名），空值当没带。
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req_headers(host: &str, extra: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("host", host.parse().unwrap());
        for (k, v) in extra {
            h.insert(
                http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn denied(host: &str, extra: &[(&str, &str)]) -> bool {
        refusal(&req_headers(host, extra), false).is_some()
    }

    /// 打开「允许任意网页调用」后的判定。
    fn denied_when_open(host: &str, extra: &[(&str, &str)]) -> bool {
        refusal(&req_headers(host, extra), true).is_some()
    }

    #[test]
    fn an_agent_client_passes() {
        // 三个客户端都不发 Origin，Host 是回环地址。
        assert!(!denied("127.0.0.1:8787", &[]));
        assert!(!denied("127.0.0.1:8787", &[("user-agent", "claude-cli/2.0.1")]));
        assert!(!denied("localhost:8787", &[("user-agent", "codex_cli_rs/0.9")]));
    }

    #[test]
    fn a_foreign_web_page_is_refused() {
        // 用户随便访问的一个页面就能向回环发跨域 POST，花掉他的渠道额度。
        assert!(denied(
            "127.0.0.1:8787",
            &[("origin", "https://evil.example")]
        ));
        assert!(denied(
            "127.0.0.1:8787",
            &[("origin", "http://192.168.1.9:3000")]
        ));
    }

    #[test]
    fn allowing_any_origin_lets_a_public_web_client_in() {
        // 部署在公网上的网页客户端指向本机网关：Host 仍是网关自己的地址。
        assert!(!denied_when_open(
            "127.0.0.1:8787",
            &[("origin", "https://lm.example.com")]
        ));
        assert!(!denied_when_open(
            "127.0.0.1:8787",
            &[("origin", "null")]
        ));
    }

    #[test]
    fn allowing_any_origin_keeps_dns_rebinding_out() {
        // 放行的是"源站"，不是"任何 Host"。rebinding 页面带的是它自己的域名，
        // 仍旧拦得住 —— 这条是打开开关后唯一还站得住的判据。
        assert!(denied_when_open(
            "evil.example:8787",
            &[("origin", "https://evil.example")]
        ));
    }

    #[test]
    fn a_null_origin_is_refused() {
        // 沙箱 iframe 与本地文件都发得出 `null`，它们可能来自任何地方。
        assert!(denied("127.0.0.1:8787", &[("origin", "null")]));
    }

    #[test]
    fn desktop_webview_schemes_pass() {
        // tauri://、file:// 这类 scheme 只有应用自己的 webview 发得出来，
        // 普通网页放不上请求头。我们的界面走 Tauri invoke，但保留这条路
        // 是为了不误伤用户自己的 webview 客户端。
        assert!(!denied("127.0.0.1:8787", &[("origin", "tauri://localhost")]));
        assert!(!denied("127.0.0.1:8787", &[("origin", "chrome-extension://abc")]));
    }

    #[test]
    fn a_local_dev_server_page_passes() {
        // 本机 dev server 指着网关：Origin 是回环，Host 也是回环。
        assert!(!denied(
            "127.0.0.1:8787",
            &[("origin", "http://localhost:5173")]
        ));
        assert!(!denied(
            "localhost:8787",
            &[("origin", "http://127.0.0.1:3000")]
        ));
    }

    #[test]
    fn a_localhost_subdomain_is_refused() {
        // Safari 等 WebKit 会把 *.localhost 交给系统解析器，网络里的解析器一答，
        // 那个页面就可能来自任何地方。
        assert!(denied(
            "127.0.0.1:8787",
            &[("origin", "http://evil.localhost")]
        ));
    }

    #[test]
    fn dns_rebinding_is_refused() {
        // 域名解析到 127.0.0.1 的页面在浏览器眼里是同源的 —— 连 Origin 都可能是
        // 它自己的域名，所以还得看 Host 认不认得出本机。
        assert!(denied(
            "evil.example:8787",
            &[("origin", "http://evil.example:8787"), ("sec-fetch-site", "same-origin")]
        ));
        // 只带 UA 的老式页面同样拦得住。
        assert!(denied(
            "evil.example:8787",
            &[("user-agent", "Mozilla/5.0 (Macintosh)")]
        ));
    }

    #[test]
    fn one_own_host_is_enough() {
        // Host 是本机名字时，Origin 是哪个页面由上面那条判据负责。
        for host in [
            "127.0.0.1:8787",
            "localhost:8787",
            "[::1]:8787",
            "192.168.1.5:8787",
            "0.0.0.0:8787",
            "host.docker.internal:8787",
        ] {
            assert!(own_host(host), "{host} 应当算本机");
        }
        for host in ["evil.example", "evil.localhost", "localhost.evil.example", ""] {
            assert!(!own_host(host), "{host} 不该算本机");
        }
    }

    #[test]
    fn a_lan_client_hitting_the_gateway_by_ip_passes() {
        // 局域网里另一台机器上的 Agent：它不发 Origin，Host 是网关的 IP。
        assert!(!denied("192.168.1.5:8787", &[("user-agent", "claude-cli/2.0.1")]));
    }

    #[test]
    fn a_browser_on_another_port_of_the_same_host_is_not_the_same_origin() {
        // 本机 5173 上的页面与网关 8787 不同源，但确实是本机页面 —— 放行是
        // 有意的：它已经能在本机跑代码，拦它没有意义。这里只是固定住行为。
        assert!(!denied(
            "127.0.0.1:8787",
            &[("origin", "http://127.0.0.1:5173")]
        ));
    }

    #[test]
    fn ports_are_read_from_the_origin() {
        assert_eq!(origin_port("http://localhost:5173"), Some(5173));
        assert_eq!(origin_port("https://localhost"), Some(443));
        assert_eq!(origin_port("http://[::1]:8787"), Some(8787));
        assert_eq!(origin_host("http://evil.example:8787"), Some("evil.example".into()));
    }
}
