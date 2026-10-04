//! 共享的 HTTP 客户端。
//!
//! 所有渠道复用同一个 `reqwest::Client`，从而共享连接池；超时按渠道在
//! 每次请求上单独设置（`RequestBuilder::timeout`），所以这里不设全局超时。

use std::time::Duration;

/// 明确不走代理的地址。
///
/// 回环与私有网段必须直连。用户机器上常配着 `HTTP_PROXY`（公司网络、抓包工具、
/// 本地加速器），若不加这个清单，指向本机模型服务（ollama / LM Studio /
/// llama.cpp）的请求会被发到代理上 —— 代理不认识 `127.0.0.1` 就返回 502，
/// 表现为"本地模型连不上"，排查起来很费劲。
const NO_PROXY_LIST: &str = "localhost,127.0.0.1,::1,0.0.0.0,\
     10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,.local,.internal";

/// 构造网关使用的 HTTP 客户端。
///
/// 刻意不设全局 timeout：流式响应可能合法地持续很久，全局超时会把长回答掐断。
/// 真正的超时控制分三层，都在请求级：
/// - 首字节超时（网关侧 `first_byte_timeout_ms`）
/// - 空闲超时（网关侧 `idle_timeout_ms`）
/// - 渠道整体超时（渠道的 `timeout_ms`）
pub fn build() -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(8)
        // TCP 层保活，避免长连接被中间设备静默断开。
        .tcp_keepalive(Duration::from_secs(60))
        // 允许压缩协商：reqwest 按启用的解压特性自动处理并透明解压，
        // 因此我们转发给下游时会剔除 content-encoding（见 channel.rs）。
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true);

    // 自己读环境代理并按需配置，而不是让 reqwest 隐式接管 ——
    // 这样才能同时挂上 no_proxy 清单。
    let builder = match env_proxy_url() {
        Some(url) => match reqwest::Proxy::all(&url) {
            Ok(proxy) => builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(NO_PROXY_LIST))),
            Err(e) => {
                tracing::warn!("环境代理 {url} 无法解析，改为直连: {e}");
                builder.no_proxy()
            }
        },
        // 没有环境代理就彻底关掉代理探测，避免意外继承其它来源的设置。
        None => builder.no_proxy(),
    };

    builder.build().expect("构造 reqwest 客户端失败")
}

/// 从环境变量里找代理地址。
///
/// 大写小写都看：不同工具写入的变量名不一致，只认一种会漏。
fn env_proxy_url() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_without_panicking() {
        let _ = build();
    }

    #[test]
    fn no_proxy_list_covers_loopback_and_private_ranges() {
        // 这几项是"本地模型连不上"的根因，缺一不可
        for must_have in ["localhost", "127.0.0.1", "::1", "192.168.0.0/16", "10.0.0.0/8"] {
            assert!(
                NO_PROXY_LIST.contains(must_have),
                "no_proxy 清单缺少 {must_have}"
            );
        }
    }

    #[test]
    fn no_proxy_list_parses_into_noproxy() {
        assert!(
            reqwest::NoProxy::from_string(NO_PROXY_LIST).is_some(),
            "清单必须能被 reqwest 解析"
        );
    }
}

