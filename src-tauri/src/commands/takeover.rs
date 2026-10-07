//! 客户端接管与还原。

use std::sync::Arc;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::takeover::clients::{self, ClientId, ClientInfo};
use crate::takeover::codex_daemon;
use crate::takeover::diff;
use crate::takeover::engine::{TakeoverEngine, TakeoverPlan, TakeoverResult};
use crate::takeover::patch;

/// 改完 Codex 的配置后，顺手重启它的常驻 app-server。
///
/// **为什么必须做**：Codex 的 TUI / 桌面版连的是一个常驻 app-server 进程，provider、
/// `base_url`、模型元数据都是**那个进程启动时**读的 —— 光重启客户端没用，用户看到的是
/// 「明明接管了却不生效」。详见 `takeover::codex_daemon`。
///
/// 只有 Codex 需要：Claude Code / Gemini CLI 都是自己读配置发请求的。
///
/// 放 `spawn_blocking`：这是个真起进程的同步调用，不该占着异步运行时。返回的是给用户
/// 看的那句话（没什么可说时是 `None`）。
async fn restart_codex_daemon(id: ClientId) -> Option<String> {
    if id != ClientId::Codex {
        return None;
    }
    match tokio::task::spawn_blocking(codex_daemon::restart_if_running).await {
        Ok(outcome) => {
            tracing::info!(?outcome, "已尝试重启 Codex 后台进程");
            outcome.note()
        }
        Err(e) => Some(format!("重启 Codex 后台进程的任务失败（{e}），请手动重启 Codex。")),
    }
}

#[tauri::command]
pub fn detect_clients() -> Vec<ClientInfo> {
    clients::describe_all(&TakeoverEngine::new())
}

#[tauri::command]
pub fn takeover_status() -> Vec<ClientInfo> {
    clients::describe_all(&TakeoverEngine::new())
}

/// 预览接管会做什么，但不写盘。
///
/// 返回各文件的 unified diff 文本；前端直接按行着色即可 ——
/// 让用户在点「接管」之前先看清要改什么，是这类工具该有的基本尊重。
#[tauri::command]
pub fn preview_takeover(shell: State<'_, Arc<AppShell>>, client: String) -> AppResult<String> {
    let id = ClientId::parse(&client).ok_or(AppError::UnknownClient(client))?;
    let base_url = gateway_base_url(&shell)?;

    let patches = id.plan_apply(&base_url, shell.settings().client_model(id.as_str()))?;

    let mut out = String::new();
    for p in patches {
        let old = patch::read_optional(&p.path)?
            .map(|b| String::from_utf8_lossy(&b).to_string());
        let new = p
            .content
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string());

        let section = diff::unified_diff(&p.path.display().to_string(), old.as_deref(), new.as_deref());
        if section.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&section);
    }

    Ok(out)
}

#[tauri::command]
pub async fn apply_takeover(
    shell: State<'_, Arc<AppShell>>,
    client: String,
) -> AppResult<TakeoverResult> {
    let id = ClientId::parse(&client).ok_or(AppError::UnknownClient(client.clone()))?;

    // 先确认"接管之后请求真的能通"，再动用户的配置文件。
    // 顺序反过来就会出现：配置文件被改了、客户端里每条请求都报错，
    // 而真正的原因（还没配渠道和模型）在客户端里完全看不到。
    let readiness = readiness_of(shell.inner()).await?;
    if !readiness.ready {
        return Err(AppError::NoUsableModel);
    }

    let base_url = gateway_base_url(&shell)?;

    let engine = TakeoverEngine::new();
    let patches = id.plan_apply(&base_url, shell.settings().client_model(id.as_str()))?;

    let plan = TakeoverPlan {
        client: id.display_name().to_string(),
        files: Vec::new(),
    };

    let mut result = engine.commit(&plan, &patches)?;
    result.client = id.as_str().to_string();
    let mut message = format!(
        "已接管 {}，base_url 指向 {}。重启该客户端后生效。",
        id.display_name(),
        base_url
    );
    if let Some(note) = restart_codex_daemon(id).await {
        message.push(' ');
        message.push_str(&note);
    }
    result.message = message;

    tracing::info!(client = id.as_str(), %base_url, "已接管客户端");
    Ok(result)
}

/// 用系统默认程序打开客户端的配置文件。
///
/// 路径**按 client 在后端解析**，不接受前端传路径 —— 收下一个任意路径就等于
/// 给渲染进程开一个「打开本机任意文件」的口子，而这里要的只是那三个固定文件。
#[tauri::command]
pub fn open_client_config(client: String) -> AppResult<()> {
    let id = ClientId::parse(&client).ok_or(AppError::UnknownClient(client))?;
    let path = id
        .primary_config_path()
        .ok_or_else(|| AppError::msg("该客户端没有可打开的配置文件"))?;

    // 文件不存在时先给一句人话：`open_path` 抛的是裸 IO 错误
    // （Windows 上就一句"系统找不到指定的文件"），用户会当成 Apilot 的 bug。
    if !path.exists() {
        return Err(AppError::msg(format!(
            "{} 还没有配置文件（{}）。先点「接管」，或手动创建后再打开。",
            id.display_name(),
            path.display()
        )));
    }

    tauri_plugin_opener::open_path(&path, None::<&str>)
        .map_err(|e| AppError::msg(format!("打开配置文件失败：{e}")))
}

#[tauri::command]
pub async fn restore_client(
    _shell: State<'_, Arc<AppShell>>,
    client: String,
) -> AppResult<TakeoverResult> {
    let id = ClientId::parse(&client).ok_or(AppError::UnknownClient(client.clone()))?;
    let engine = TakeoverEngine::new();

    let mut result = engine.restore(id.as_str(), &id.config_paths())?;
    if result.applied {
        let mut message = format!("已还原 {}", id.display_name());
        // 还原同样要重启：不重启的话，Codex 的常驻进程还攥着指向网关的那份配置，
        // 用户会以为"还原没生效"。
        if let Some(note) = restart_codex_daemon(id).await {
            message.push(' ');
            message.push_str(&note);
        }
        result.message = message;
    }

    tracing::info!(client = id.as_str(), "已还原客户端");
    Ok(result)
}

/// 取网关当前的可接管地址。
///
/// 网关没跑时**拒绝接管** —— 把客户端指向一个没在监听的端口，
/// 会让用户以为"接管成功但模型坏了"，不如直接报错。
fn gateway_base_url(shell: &Arc<AppShell>) -> AppResult<String> {
    shell.gateway.base_url().ok_or(AppError::GatewayNotRunning)
}

/// 接管前的就绪状态。
///
/// 前端拿它来决定「接管」按钮是否可点、以及提示用户缺哪一步 ——
/// 判定逻辑留在后端，避免前后端各写一份规则后悄悄漂移。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TakeoverReadiness {
    pub gateway_running: bool,
    pub has_models: bool,
    pub ready: bool,
    /// 未就绪的原因，直接展示给用户；就绪时为 `None`。
    pub reason: Option<String>,
}

async fn readiness_of(shell: &Arc<AppShell>) -> AppResult<TakeoverReadiness> {
    let gateway_running = shell.gateway.base_url().is_some();
    let has_models = crate::storage::providers::has_declared_models(&shell.db).await?;

    let reason = if !gateway_running {
        Some("网关未运行，请先在「设置」或概览页启动网关".to_string())
    } else if !has_models {
        Some("还没有可用的模型：请先在「渠道管理」添加上游渠道并指定要使用的模型".to_string())
    } else {
        None
    };

    Ok(TakeoverReadiness {
        gateway_running,
        has_models,
        ready: reason.is_none(),
        reason,
    })
}

#[tauri::command]
pub async fn takeover_readiness(shell: State<'_, Arc<AppShell>>) -> AppResult<TakeoverReadiness> {
    readiness_of(shell.inner()).await
}
