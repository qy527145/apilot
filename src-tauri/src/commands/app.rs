//! 应用级命令：版本信息与设置读写。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

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
