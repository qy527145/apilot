//! 网关服务器的生命周期管理。

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};

use crate::config::settings::AppSettings;
use crate::error::{AppError, AppResult};
use crate::shell::AppShell;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub host: String,
    /// 实际监听的端口。设为 0 时由系统分配，这里回填真实端口。
    pub port: u16,
    pub error: Option<String>,
}

/// 网关服务器的启停控制。
pub struct GatewayServer {
    shutdown_tx: RwLock<Option<oneshot::Sender<()>>>,
    handle: RwLock<Option<tauri::async_runtime::JoinHandle<()>>>,
    status: ArcSwap<GatewayStatus>,
    actual_port: AtomicU16,
}

impl GatewayServer {
    pub fn new() -> Self {
        Self {
            shutdown_tx: RwLock::new(None),
            handle: RwLock::new(None),
            status: ArcSwap::from_pointee(GatewayStatus::default()),
            actual_port: AtomicU16::new(0),
        }
    }

    pub fn status(&self) -> GatewayStatus {
        (**self.status.load()).clone()
    }

    pub fn port(&self) -> u16 {
        self.actual_port.load(Ordering::Relaxed)
    }

    /// 客户端应当指向的 base_url。
    pub fn base_url(&self) -> Option<String> {
        let s = self.status();
        if s.running {
            Some(format!("http://{}:{}", reachable_host(&s.host), s.port))
        } else {
            None
        }
    }

    async fn set_status(&self, status: GatewayStatus) {
        self.actual_port.store(status.port, Ordering::Relaxed);
        self.status.store(Arc::new(status));
    }

    /// 启动网关。重复调用是幂等的 —— 已在运行就原样返回当前状态。
    pub async fn start(&self, shell: Arc<AppShell>) -> AppResult<GatewayStatus> {
        let settings = shell.settings();
        let (host, port) = (settings.listen_host.clone(), settings.listen_port);
        self.start_at(shell, &host, port).await
    }

    /// 在指定地址上启动。
    ///
    /// 地址与 `shell.settings()` 分开传，是为了**回退**：新地址起不来时得按旧地址
    /// 把网关拉回来，而那一刻设置里已经是新值了。
    pub async fn start_at(
        &self,
        shell: Arc<AppShell>,
        host: &str,
        port: u16,
    ) -> AppResult<GatewayStatus> {
        {
            let guard = self.shutdown_tx.read().await;
            if guard.is_some() {
                return Ok(self.status());
            }
        }

        let addr = format!("{host}:{port}");
        // 先绑定再返回状态：端口被占用时立刻拿到错误，而不是让用户以为启动成功了。
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| bind_error(&addr, port, e))?;

        self.serve_on(listener, host, port, shell).await
    }

    /// 把已经绑好的 listener 交给 axum 跑起来并记下状态。
    async fn serve_on(
        &self,
        listener: tokio::net::TcpListener,
        host: &str,
        port: u16,
        shell: Arc<AppShell>,
    ) -> AppResult<GatewayStatus> {
        let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);

        let app = crate::gateway::router::build(shell.clone());
        let (tx, rx) = oneshot::channel::<()>();

        *self.shutdown_tx.write().await = Some(tx);

        let handle = tauri::async_runtime::spawn(async move {
            let result = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;

            if let Err(e) = result {
                tracing::error!("网关服务器异常退出: {e}");
            }
        });
        *self.handle.write().await = Some(handle);

        self.set_status(GatewayStatus {
            running: true,
            host: host.to_string(),
            port: actual,
            error: None,
        })
        .await;

        tracing::info!(%host, port, actual, "网关已启动");

        // 网关换了地址，已接管的客户端还指着老地址 —— 不改它们，用户看到的就是
        // "改了端口，客户端全连不上"。
        //
        // 放在这里而不是设置命令里，是因为**只有这里知道网关真正跑在哪**：设置里改完
        // 未必立刻生效（端口被占会退回老地址），端口填 0 时真实端口也是这一刻才分配出来。
        // 启动、换地址、退回老地址三条路都汇到 serve_on，一处就全覆盖了。
        //
        // 没有变化时 `repoint_taken_over` 一个字节都不写，所以每次启动都来问一遍是安全的。
        // 失败只记日志：客户端配置没跟上不该让网关起不来。
        if let Some(base_url) = self.base_url() {
            match crate::takeover::clients::repoint_taken_over(&base_url, &shell.settings()) {
                Ok(changed) if !changed.is_empty() => {
                    tracing::info!(%base_url, clients = ?changed, "已把接管的客户端指向新地址");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("同步客户端 base_url 失败（客户端可能仍指向旧地址）: {e}"),
            }
        }

        shell.events.gateway(&self.status());

        Ok(self.status())
    }

    /// 换到设置里新的监听地址上。
    ///
    /// 两处顺序不能想当然：
    ///
    /// - **端口变了就先绑一次探路**。绑不上时原样退回，绝不"先停再试" ——
    ///   用户填了个被占用的端口，结果连原来在服务的那份也停了，那是比
    ///   "端口没生效"严重得多的事故。端口没变（只换了网卡）时没法先绑，
    ///   老 listener 还占着同一个端口号，只能先停后起。
    /// - 新地址起不来时**退回老地址**，而不是把网关留在半死状态。
    pub async fn rebind(
        &self,
        shell: Arc<AppShell>,
        old_host: &str,
        old_port: u16,
    ) -> AppResult<GatewayStatus> {
        if !self.status().running {
            return Ok(self.status()); // 本来就没跑，下次 start 自然读新值
        }

        let settings = shell.settings();
        let (new_host, new_port) = (settings.listen_host.clone(), settings.listen_port);
        let (new_addr, old_addr) = (
            format!("{new_host}:{new_port}"),
            format!("{old_host}:{old_port}"),
        );
        if new_addr == old_addr {
            return Ok(self.status());
        }

        let prebound = if new_port != old_port {
            match tokio::net::TcpListener::bind(&new_addr).await {
                Ok(l) => Some(l),
                Err(e) => {
                    let err = bind_error(&new_addr, new_port, e);
                    self.report_error(&shell, format!("监听地址未生效：{err}")).await;
                    return Err(err);
                }
            }
        } else {
            None
        };

        self.stop(Some(&shell)).await?;

        let started = match prebound {
            Some(listener) => self.serve_on(listener, &new_host, new_port, shell.clone()).await,
            None => self.start(shell.clone()).await,
        };

        match started {
            Ok(s) => Ok(s),
            Err(e) => {
                // 别让一次设置改动把网关弄没了：把老地址还回去。
                tracing::warn!("换到 {new_addr} 失败，退回 {old_addr}: {e}");
                let back = self.start_at(shell.clone(), old_host, old_port).await;
                let detail = match back {
                    Ok(_) => format!("已退回 {old_addr}"),
                    Err(e2) => format!("退回 {old_addr} 也失败了：{e2}"),
                };
                self.report_error(&shell, format!("换到 {new_addr} 失败（{detail}）：{e}"))
                    .await;
                Err(e)
            }
        }
    }

    /// 把错误挂到状态上并广播。
    ///
    /// 后台换地址失败时没有调用方可以收错误，只能靠状态面板说话 ——
    /// 否则"改了端口没反应"会第二次发生，而且这次连原因都看不到。
    async fn report_error(&self, shell: &Arc<AppShell>, message: String) {
        let mut status = self.status();
        status.error = Some(message);
        self.set_status(status).await;
        shell.events.gateway(&self.status());
    }

    /// 停止网关。未运行时是空操作。
    pub async fn stop(&self, shell: Option<&Arc<AppShell>>) -> AppResult<GatewayStatus> {
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.write().await.take() {
            // 等待服务循环真正退出，确保端口被释放后再返回。
            let _ = handle.await;
        }

        let current = self.status();
        let stopped = GatewayStatus {
            running: false,
            host: current.host,
            port: 0,
            error: None,
        };
        self.set_status(stopped.clone()).await;

        tracing::info!("网关已停止");
        if let Some(shell) = shell {
            shell.events.gateway(&stopped);
        }

        Ok(stopped)
    }
}

impl Default for GatewayServer {
    fn default() -> Self {
        Self::new()
    }
}

/// 写进客户端配置的地址得真的能连上。
///
/// 监听地址可以是通配的 `0.0.0.0`（愿意被局域网访问时就这么填），但**客户端去连它
/// 是不确定的** —— macOS 上干脆连不上。客户端永远在本机，换成回环地址既准确，
/// 又不改变它实际的可达性。
fn reachable_host(host: &str) -> &str {
    match host.trim() {
        "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1",
        h => h,
    }
}

/// 绑定失败归一成 `AppError`。端口被占用单独成一类 —— 那是用户最容易撞上、
/// 也最需要一句人话的错。
fn bind_error(addr: &str, port: u16, e: std::io::Error) -> AppError {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        AppError::PortInUse(port)
    } else {
        AppError::msg(format!("监听 {addr} 失败: {e}"))
    }
}

/// 这次设置变更要不要把网关换到新地址上。
///
/// 两个条件缺一不可：网关**正在跑**（没跑的话下次 `start` 自然读新值，
/// 没必要先起一个再换），以及**监听地址真的变了** —— 改缓存、超时这些
/// 不该顺手把网关重启一遍，那会把在途的流式回答全掐了。
pub fn needs_rebind(old: &AppSettings, new: &AppSettings, running: bool) -> bool {
    running && old.listen_addr() != new.listen_addr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_server_is_not_running() {
        let s = GatewayServer::new();
        let st = s.status();
        assert!(!st.running);
        assert_eq!(st.port, 0);
        assert!(s.base_url().is_none());
    }

    #[test]
    fn base_url_reflects_status() {
        let s = GatewayServer::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            s.set_status(GatewayStatus {
                running: true,
                host: "127.0.0.1".into(),
                port: 8787,
                error: None,
            })
            .await;
        });
        assert_eq!(s.base_url().as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(s.port(), 8787);
    }

    #[test]
    fn wildcard_listen_hosts_are_written_as_loopback() {
        // 监听 0.0.0.0 是"愿意被局域网访问"，但把这个地址写进客户端配置就不对了：
        // 客户端只会从本机连，而某些平台上连 0.0.0.0 根本连不上。
        assert_eq!(reachable_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(reachable_host("::"), "127.0.0.1");
        assert_eq!(reachable_host("[::]"), "127.0.0.1");
        assert_eq!(reachable_host(" 0.0.0.0 "), "127.0.0.1");
    }

    #[test]
    fn specific_hosts_are_kept_verbatim() {
        // 用户特意填了某个网卡地址（比如想让 WSL 里的客户端连进来），不能自作主张改掉。
        for h in ["127.0.0.1", "localhost", "192.168.1.5", "10.0.0.2"] {
            assert_eq!(reachable_host(h), h);
        }
    }

    #[test]
    fn status_default_has_no_error() {
        assert!(GatewayStatus::default().error.is_none());
    }

    fn with_addr(host: &str, port: u16) -> AppSettings {
        AppSettings {
            listen_host: host.to_string(),
            listen_port: port,
            ..Default::default()
        }
    }

    #[test]
    fn rebind_only_when_the_listen_address_really_changed() {
        let old = with_addr("127.0.0.1", 8787);

        assert!(
            !needs_rebind(&old, &with_addr("127.0.0.1", 8787), true),
            "地址没变就不该动网关 —— 那会把在途的流式回答全掐了"
        );
        assert!(needs_rebind(&old, &with_addr("127.0.0.1", 8788), true));
        assert!(needs_rebind(&old, &with_addr("0.0.0.0", 8787), true));
    }

    #[test]
    fn a_stopped_gateway_does_not_need_a_rebind() {
        // 没跑的时候下次 start 自然读新值，先起一个再换纯属多余。
        let old = with_addr("127.0.0.1", 8787);
        assert!(!needs_rebind(&old, &with_addr("127.0.0.1", 9999), false));
    }

    #[test]
    fn address_in_use_is_reported_as_such() {
        // 端口被占用是用户最容易撞上的那类错，要能单独识别出来给一句人话。
        let e = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert!(matches!(bind_error("127.0.0.1:8787", 8787, e), AppError::PortInUse(8787)));

        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let msg = bind_error("127.0.0.1:80", 80, e).to_string();
        assert!(msg.contains("127.0.0.1:80"), "别的错要带上地址：{msg}");
    }
}
