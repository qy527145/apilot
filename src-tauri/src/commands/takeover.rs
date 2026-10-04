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
