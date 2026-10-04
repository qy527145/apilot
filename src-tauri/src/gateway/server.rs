//! 网关服务器的生命周期管理。

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};

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
            Some(format!("http://{}:{}", s.host, s.port))
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
        {
            let guard = self.shutdown_tx.read().await;
            if guard.is_some() {
                return Ok(self.status());
            }
        }

        let settings = shell.settings();
        let addr = settings.listen_addr();

        // 先绑定再返回状态：端口被占用时立刻拿到错误，而不是让用户以为启动成功了。
        let listener = tokio::net::TcpListener::bind(&addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                AppError::PortInUse(settings.listen_port)
            } else {
                AppError::msg(format!("监听 {addr} 失败: {e}"))
            }
        })?;

        let actual = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(settings.listen_port);

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
            host: settings.listen_host.clone(),
            port: actual,
            error: None,
        })
        .await;

        tracing::info!(%addr, actual, "网关已启动");
        shell.events.gateway(&self.status());

        Ok(self.status())
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
    fn status_default_has_no_error() {
        assert!(GatewayStatus::default().error.is_none());
    }
}
