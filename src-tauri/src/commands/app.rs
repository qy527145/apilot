//! 应用级命令：版本信息与设置读写。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::commands::takeover::restart_codex_daemon;
use crate::config::settings::ModelPolicy;
use crate::config::AppSettings;
use crate::error::AppResult;
use crate::shell::AppShell;
use crate::takeover::clients::{self, ClientId};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    pub apilot_home: String,
    pub db_path: String,
    pub started_at: i64,
}

#[tauri::command]
pub fn app_info(shell: State<'_, Arc<AppShell>>) -> AppInfo {
    AppInfo {
        name: "Apilot".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        apilot_home: crate::config::apilot_home().display().to_string(),
        db_path: crate::config::db_path().display().to_string(),
        started_at: shell.started_at(),
    }
}

#[tauri::command]
pub fn get_settings(shell: State<'_, Arc<AppShell>>) -> AppSettings {
    shell.settings().as_ref().clone()
}

#[tauri::command]
pub async fn update_settings(
    shell: State<'_, Arc<AppShell>>,
    settings: AppSettings,
) -> AppResult<AppSettings> {
    let next = settings.normalized();
    let before = shell.settings();

    let rebind = crate::gateway::server::needs_rebind(
        &before,
        &next,
        shell.gateway.status().running,
    );

    shell.update_settings(next.clone()).await?;

    // 接管策略 / 模型策略变了，已接管的客户端得按新策略重写一遍 —— 界面上没有第二个
    // 「接管」按钮（已接管的行显示的是「还原」），不在这里落地就等于没有补救手段。
    // 换地址那条走下面的 rebind → `serve_on`，不归这里。
    reapply_after_policy_change(shell.inner(), &before, &next).await;

    // 监听地址变了要真的换过去 —— 以前只是存下来，网关照旧跑在老地址上，
    // 表现为"改了端口没反应"。
    //
    // **放后台**：`stop()` 是优雅停机，会等在途请求跑完，一条长回答能挂几分钟，
    // 设置页不该为此卡住。失败会挂到网关状态上（`report_error`），界面上看得到。
    if rebind {
        let shell = shell.inner().clone();
        let gateway = shell.gateway.clone();
        let (host, port) = (before.listen_host.clone(), before.listen_port);
        tauri::async_runtime::spawn(async move {
            if let Err(e) = gateway.rebind(shell, &host, port).await {
                tracing::warn!("换监听地址未生效: {e}");
            }
        });
    }

    Ok(next)
}

/// 只改全局模型策略。
///
/// 比让前端回传整个 `AppSettings` 稳妥：模型页不该、也不必覆盖监听端口、超时、
/// 缓存策略这些它不关心的字段 —— 整份回传会把别处刚改的设置一起冲掉。
#[tauri::command]
pub async fn set_model_policy(
    shell: State<'_, Arc<AppShell>>,
    policy: ModelPolicy,
) -> AppResult<ModelPolicy> {
    let before = shell.settings();
    let mut settings = before.as_ref().clone();
    settings.model_policy = policy.normalized();
    let saved = settings.model_policy.clone();
    shell.update_settings(settings.clone()).await?;
    // 模型名是要写进客户端配置的（`AppSettings::client_model`），改完同样要重写。
    reapply_after_policy_change(shell.inner(), &before, &settings).await;
    Ok(saved)
}

/// 策略改动后立刻把已接管的客户端配置重写一遍。
///
/// 抽成一处而不是散在两个命令里：`update_settings` 与 `set_model_policy` 都会改到
/// 决定"往客户端写什么"的字段，两处各写一遍必然漏一处 —— 而漏掉的表现是"改了策略
/// 客户端没反应"，用户完全无从判断是哪条路没接上。
///
/// 失败只记日志：客户端配置没跟上，不该让"保存设置"这个动作整个失败。
async fn reapply_after_policy_change(
    shell: &Arc<AppShell>,
    before: &AppSettings,
    after: &AppSettings,
) {
    if !clients::plan_inputs_differ(before, after) {
        return;
    }

    // 网关没在跑时传 `None`，由 `reapply_taken_over` 退回配置里存着的那份地址。
    let base_url = shell.gateway.base_url();
    match clients::reapply_taken_over(base_url.as_deref(), after) {
        Ok(changed) if !changed.is_empty() => {
            let names: Vec<&str> = changed.iter().map(|c| c.as_str()).collect();
            tracing::info!(clients = ?names, "策略已变更，重写了客户端配置");

            // Codex 的常驻 app-server 只在启动时读配置，不重启就还是老策略 ——
            // 与接管、换地址那两条路同样的理由（见 `takeover::codex_daemon`）。
            if changed.contains(&ClientId::Codex) {
                if let Some(note) = restart_codex_daemon(ClientId::Codex).await {
                    tracing::info!(%note, "已顺带重启 Codex 后台进程");
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("策略变更后重写客户端配置失败: {e}"),
    }
}

/// 校验一段模型脚本，`Some(错因)` 表示用不了。
///
/// 界面上是**保存前**的即时校验：规则里的脚本写错了只会静默不生效，
/// 那种错用户很难自己发现。
#[tauri::command]
pub fn validate_model_script(source: String) -> Option<String> {
    crate::routing::model_script::validate(&source).err()
}
