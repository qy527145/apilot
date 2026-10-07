//! 共享的 HTTP 客户端与出站代理。
//!
//! 同一个代理下的渠道复用同一个 `reqwest::Client`，从而共享连接池；超时按渠道在
//! 每次请求上单独设置（`RequestBuilder::timeout`），所以这里不设全局超时。

use std::time::Duration;

use dashmap::DashMap;

use crate::config::settings::{ProxyMode, ProxySettings};
use crate::storage::models::{ChannelProxy, ChannelProxyMode};

/// 明确不走代理的地址。
///
/// 回环与私有网段必须直连。用户机器上常配着 `HTTP_PROXY`（公司网络、抓包工具、
/// 本地加速器），若不加这个清单，指向本机模型服务（ollama / LM Studio /
/// llama.cpp）的请求会被发到代理上 —— 代理不认识 `127.0.0.1` 就返回 502，
/// 表现为"本地模型连不上"，排查起来很费劲。
const NO_PROXY_LIST: &str = "localhost,127.0.0.1,::1,0.0.0.0,\
     10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,.local,.internal";

/// 解析后的出站路径。**也是客户端缓存的键** —— 所以它必须完整描述客户端的一切
/// 差异：两个渠道只有在解析结果相同时才能共用连接池。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProxySpec {
    /// 明确直连，**连环境变量都不看**。
    Direct,
    Proxied { url: String },
}

/// reqwest 能处理的代理协议前缀。
///
/// `socks5h` 与 `socks5` 的区别值得留意：前者让代理解析域名（能绕开 DNS 污染），
/// 后者在本地解析。两者都支持，具体用哪个由用户填的 URL 决定。
const SUPPORTED_SCHEMES: [&str; 6] = [
    "http://",
    "https://",
    "socks4://",
    "socks4a://",
    "socks5://",
    "socks5h://",
];

/// 保存前校验用：拦下 `ftp://` 这种 reqwest 处理不了的写法，
/// 而不是等构造客户端时才 warn。
pub fn is_supported_proxy_url(url: &str) -> bool {
    let u = url.trim();
    !u.is_empty() && SUPPORTED_SCHEMES.iter().any(|p| u.starts_with(p))
}

/// 修剪并校验一个代理地址；空白或协议不支持时返回 `None`。
pub fn sanitize_proxy_url(url: Option<&str>) -> Option<String> {
    let u = url.map(str::trim).filter(|s| !s.is_empty())?;
    is_supported_proxy_url(u).then(|| u.to_string())
}

/// 全局设置解析成实际出站路径。`System` 模式在这里读环境变量。
pub fn resolve_global(settings: &ProxySettings) -> ProxySpec {
    resolve_global_with(settings, &env_proxy_url)
}

/// 同上，环境变量来源可注入 —— 测试不能去改进程环境（并行跑会互相踩）。
fn resolve_global_with(
    settings: &ProxySettings,
    env: &dyn Fn() -> Option<String>,
) -> ProxySpec {
    match settings.mode {
        ProxyMode::Direct => ProxySpec::Direct,
        ProxyMode::Manual => {
            // 填了自定义模式却没填地址：直连。退化成"随手走环境代理"更意外 ——
            // 用户刚明确表达了"我要指定代理"，此时静默走另一个代理最难排查。
            match sanitize_proxy_url(settings.url.as_deref()) {
                Some(url) => ProxySpec::Proxied { url },
                None => ProxySpec::Direct,
            }
        }
        ProxyMode::System | ProxyMode::Unknown => match env() {
            Some(url) => ProxySpec::Proxied { url },
            None => ProxySpec::Direct,
        },
    }
}

/// 渠道级设置解析成实际出站路径。`Inherit` 才回头看全局。
pub fn resolve_channel(channel: &ChannelProxy, global: &ProxySettings) -> ProxySpec {
    match channel.mode {
        ChannelProxyMode::Direct => ProxySpec::Direct,
        ChannelProxyMode::Manual => match sanitize_proxy_url(channel.url.as_deref()) {
            Some(url) => ProxySpec::Proxied { url },
            None => resolve_global(global),
        },
        ChannelProxyMode::Inherit | ChannelProxyMode::Unknown => resolve_global(global),
    }
}

/// 按解析结果构造 HTTP 客户端。
///
/// 刻意不设全局 timeout：流式响应可能合法地持续很久，全局超时会把长回答掐断。
/// 真正的超时控制分三层，都在请求级：
/// - 首字节超时（网关侧 `first_byte_timeout_ms`）
/// - 空闲超时（网关侧 `idle_timeout_ms`）
/// - 渠道整体超时（渠道的 `timeout_ms`）
pub fn build_with(spec: &ProxySpec) -> reqwest::Client {
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

    // 代理全部自己显式设置，不让 reqwest 隐式接管 —— 它的默认行为是读环境变量，
    // 那样「强制直连」根本不起作用（`no_proxy()` 那一支就是在关掉这个探测）。
    let builder = match spec {
        ProxySpec::Direct => builder.no_proxy(),
        ProxySpec::Proxied { url } => match reqwest::Proxy::all(url) {
            Ok(proxy) => builder.proxy(proxy.no_proxy(reqwest::NoProxy::from_string(NO_PROXY_LIST))),
            Err(e) => {
                tracing::warn!("代理 {url} 无法解析，改为直连: {e}");
                builder.no_proxy()
            }
        },
    };

    builder.build().expect("构造 reqwest 客户端失败")
}

/// 按代理记忆化客户端的池子。
///
/// 为什么不能每个渠道一个客户端：`Client` 的开销不在 clone（那是 Arc），而在构造 ——
/// 每个实例自带连接池与 TLS 配置，而 `reload()` 在每次渠道增删改后都会跑一遍，
/// 每次都新建会把已建立的连接全丢掉。客户端数量因此等于**不同代理地址的个数**，
/// 通常 1~3 个。
///
/// 代价：原本所有渠道共用一份 `pool_max_idle_per_host`，现在按代理各有一份，
/// 同主机跨代理时空闲连接上限会相乘。本地应用量级，可以忽略。
pub struct ClientPool {
    cache: DashMap<ProxySpec, reqwest::Client>,
    /// 测试用：任何 spec 都返回同一个客户端，免得每个用例都真去建连接池。
    fixed: Option<reqwest::Client>,
}

impl Default for ClientPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientPool {
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
            fixed: None,
        }
    }

    pub fn fixed(client: reqwest::Client) -> Self {
        Self {
            cache: DashMap::new(),
            fixed: Some(client),
        }
    }

    /// 取一个客户端，取不到才构造。返回的是 owned `Client`（内部是 Arc），
    /// 守卫不跨 await 残留。
    pub fn get(&self, spec: ProxySpec) -> reqwest::Client {
        if let Some(c) = &self.fixed {
            return c.clone();
        }
        if let Some(c) = self.cache.get(&spec) {
            return c.value().clone();
        }

        // 并发首次命中同一 spec 时可能各建一个，后插入的胜出，多出来的那个
        // 直接析构 —— 只是白建一次连接池，不影响正确性。为此加锁不值得。
        let client = build_with(&spec);
        self.cache.insert(spec, client.clone());
        client
    }
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

    fn settings(mode: ProxyMode, url: Option<&str>) -> ProxySettings {
        ProxySettings {
            mode,
            url: url.map(str::to_string),
        }
    }

    /// 构造一个"环境里有 HTTPS_PROXY"的假环境。
    fn env_with(url: &'static str) -> impl Fn() -> Option<String> {
        move || Some(url.to_string())
    }

    fn env_empty() -> impl Fn() -> Option<String> {
        || None
    }

    #[test]
    fn client_builds_without_panicking() {
        for spec in [
            ProxySpec::Direct,
            ProxySpec::Proxied {
                url: "http://127.0.0.1:7890".into(),
            },
        ] {
            let _ = build_with(&spec);
        }
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

    #[test]
    fn direct_mode_ignores_the_environment_proxy() {
        // 「强制直连」的全部意义就在这一条：环境里有代理也不许用。
        // 少了它，本地模型服务会被发到公司代理上。
        let spec = resolve_global_with(&settings(ProxyMode::Direct, None), &env_with("http://p:1"));
        assert_eq!(spec, ProxySpec::Direct);
    }

    #[test]
    fn system_mode_without_a_proxy_in_the_environment_is_direct() {
        let spec = resolve_global_with(&settings(ProxyMode::System, None), &env_empty());
        assert_eq!(spec, ProxySpec::Direct);
    }

    #[test]
    fn system_mode_uses_the_environment_proxy() {
        let spec = resolve_global_with(&settings(ProxyMode::System, None), &env_with("socks5://127.0.0.1:1080"));
        assert_eq!(
            spec,
            ProxySpec::Proxied {
                url: "socks5://127.0.0.1:1080".into()
            }
        );
    }

    #[test]
    fn manual_mode_uses_the_configured_url_and_ignores_the_environment() {
        let spec = resolve_global_with(
            &settings(ProxyMode::Manual, Some("http://127.0.0.1:7890")),
            &env_with("http://env:1"),
        );
        assert_eq!(
            spec,
            ProxySpec::Proxied {
                url: "http://127.0.0.1:7890".into()
            }
        );
    }

    #[test]
    fn manual_mode_without_a_url_falls_back_to_direct_not_to_the_environment() {
        // 用户明确说了"我要指定代理"，此时静默走环境里那个最难排查。
        let spec = resolve_global_with(&settings(ProxyMode::Manual, None), &env_with("http://env:1"));
        assert_eq!(spec, ProxySpec::Direct);
    }

    #[test]
    fn unsupported_proxy_schemes_are_rejected() {
        for bad in ["ftp://x:1", "127.0.0.1:1080", "", "   "] {
            assert!(!is_supported_proxy_url(bad), "{bad} 不该被接受");
            assert_eq!(sanitize_proxy_url(Some(bad)), None);
        }
    }

    #[test]
    fn supported_proxy_schemes_are_accepted_including_socks() {
        for good in [
            "http://127.0.0.1:7890",
            "https://proxy.example.com:443",
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
        ] {
            assert!(is_supported_proxy_url(good), "{good} 必须被接受");
        }
    }

    #[test]
    fn a_channel_saying_direct_wins_over_a_manual_global_proxy() {
        // 全局配了公司代理，本地 ollama 那条渠道标「直连」时必须真的直连。
        let global = settings(ProxyMode::Manual, Some("http://corp:8080"));
        let channel = ChannelProxy {
            mode: ChannelProxyMode::Direct,
            url: None,
        };
        assert_eq!(resolve_channel(&channel, &global), ProxySpec::Direct);
    }

    #[test]
    fn an_inheriting_channel_follows_the_global_proxy() {
        let global = settings(ProxyMode::Manual, Some("http://corp:8080"));
        let channel = ChannelProxy::default();
        assert_eq!(
            resolve_channel(&channel, &global),
            ProxySpec::Proxied {
                url: "http://corp:8080".into()
            }
        );
    }

    #[test]
    fn a_channel_with_its_own_proxy_ignores_the_global_one() {
        let global = settings(ProxyMode::Manual, Some("http://corp:8080"));
        let channel = ChannelProxy {
            mode: ChannelProxyMode::Manual,
            url: Some("socks5://127.0.0.1:1080".into()),
        };
        assert_eq!(
            resolve_channel(&channel, &global),
            ProxySpec::Proxied {
                url: "socks5://127.0.0.1:1080".into()
            }
        );
    }

    #[test]
    fn a_socks_client_can_be_built() {
        // 只是别 panic：真正能不能连由运行时决定，但构造阶段就炸说明 feature 没开。
        let _ = build_with(&ProxySpec::Proxied {
            url: "socks5://127.0.0.1:1080".into(),
        });
    }

    #[test]
    fn a_malformed_proxy_url_falls_back_to_a_direct_client_instead_of_panicking() {
        // 坏配置不该让应用起不来。能走到这里说明 URL 通过了前缀校验但 reqwest 不认，
        // 记一条 warn 后直连 —— 请求会失败，但失败的是那一条请求。
        let _ = build_with(&ProxySpec::Proxied {
            url: "socks5://".into(),
        });
    }

    #[test]
    fn the_pool_reuses_one_client_per_proxy() {
        let pool = ClientPool::new();
        let spec = ProxySpec::Proxied {
            url: "http://127.0.0.1:7890".into(),
        };
        let a = pool.get(spec.clone());
        let b = pool.get(spec);
        // reqwest::Client 不比指针，只能靠"缓存里有且只有一个条目"来验证复用。
        assert_eq!(pool.cache.len(), 1);
        drop((a, b));
    }

    #[test]
    fn the_pool_keeps_distinct_clients_for_distinct_proxies() {
        let pool = ClientPool::new();
        let _ = pool.get(ProxySpec::Direct);
        let _ = pool.get(ProxySpec::Proxied {
            url: "http://127.0.0.1:7890".into(),
        });
        assert_eq!(pool.cache.len(), 2);
    }
}
