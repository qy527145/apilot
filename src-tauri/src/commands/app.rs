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
    shell.update_settings(next.clone()).await?;
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
