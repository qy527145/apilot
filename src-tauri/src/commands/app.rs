//! 应用级命令：版本信息与设置读写。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::config::settings::ModelPolicy;
use crate::config::AppSettings;
use crate::error::AppResult;
use crate::shell::AppShell;

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
    let mut settings = shell.settings().as_ref().clone();
    settings.model_policy = policy.normalized();
    let saved = settings.model_policy.clone();
    shell.update_settings(settings).await?;
    Ok(saved)
}

/// 校验一段模型脚本，`Some(错因)` 表示用不了。
///
/// 界面上是**保存前**的即时校验：规则里的脚本写错了只会静默不生效，
/// 那种错用户很难自己发现。
#[tauri::command]
pub fn validate_model_script(source: String) -> Option<String> {
    crate::routing::model_script::validate(&source).err()
}
