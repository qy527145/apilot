//! 请求回放命令。

use std::sync::Arc;

use tauri::State;

use crate::error::AppResult;
use crate::shell::AppShell;

/// 把一条捕获过的请求重发到指定渠道 / 模型，供 A/B 对比。
///
/// **不写日志、不计费**：这是用户主动做的一次对比实验，不应污染真实用量。
#[tauri::command]
pub async fn replay_request(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
    provider_tag: Option<String>,
    model: Option<String>,
) -> AppResult<crate::replay::ReplayOutcome> {
    crate::replay::replay(&shell, &request_id, provider_tag, model).await
}
