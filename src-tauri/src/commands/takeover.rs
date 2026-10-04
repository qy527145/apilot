//! 客户端接管与还原。

use std::sync::Arc;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::takeover::clients::{self, ClientId, ClientInfo};
use crate::takeover::diff;
use crate::takeover::engine::{TakeoverEngine, TakeoverPlan, TakeoverResult};
use crate::takeover::patch;

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

    let patches = id.plan_apply(&base_url)?;

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
    let patches = id.plan_apply(&base_url)?;

    let plan = TakeoverPlan {
        client: id.display_name().to_string(),
        files: Vec::new(),
    };

    let mut result = engine.commit(&plan, &patches)?;
    result.client = id.as_str().to_string();
    result.message = format!(
        "已接管 {}，base_url 指向 {}。重启该客户端后生效。",
        id.display_name(),
        base_url
    );

    tracing::info!(client = id.as_str(), %base_url, "已接管客户端");
    Ok(result)
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
        result.message = format!("已还原 {}", id.display_name());
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
