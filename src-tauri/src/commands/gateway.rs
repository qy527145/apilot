//! 网关启停与状态。

use std::sync::Arc;

use tauri::State;

use crate::error::AppResult;
use crate::gateway::server::GatewayStatus;
use crate::shell::AppShell;

#[tauri::command]
pub async fn gateway_start(shell: State<'_, Arc<AppShell>>) -> AppResult<GatewayStatus> {
    shell.gateway.start(shell.inner().clone()).await
}

#[tauri::command]
pub async fn gateway_stop(shell: State<'_, Arc<AppShell>>) -> AppResult<GatewayStatus> {
    shell
        .gateway
        .stop(Some(shell.inner()))
        .await
}

#[tauri::command]
pub fn gateway_status(shell: State<'_, Arc<AppShell>>) -> GatewayStatus {
    shell.gateway.status()
}
