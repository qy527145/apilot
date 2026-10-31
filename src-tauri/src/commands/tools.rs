//! 工具调用频率统计。

use std::sync::Arc;

use tauri::State;

use crate::error::AppResult;
use crate::shell::AppShell;
use crate::storage::tool_usage::{self, ToolUsageReport};

use super::billing::TimeRange;

/// 工具调用分布面板的数据。
///
/// 先把内存缓冲刷一次再查：面板是用户主动打开的，此刻最新的十来秒还在内存里，
/// 不刷的话他翻到"刚刚跑的那条请求为什么没算进去"，只能想到「功能坏了」。
#[tauri::command]
pub async fn tool_usage_report(
    shell: State<'_, Arc<AppShell>>,
    range: TimeRange,
    client: Option<String>,
) -> AppResult<ToolUsageReport> {
    let range = range.normalized();
    if let Err(e) = shell.tool_usage.flush(&shell.db).await {
        // 刷不进去不影响查询本身：已经落库的那部分照样能看。
        tracing::warn!("刷新工具统计失败: {e}");
    }
    tool_usage::report(
        &shell.db,
        range.from,
        range.to,
        client.as_deref().filter(|c| !c.is_empty()),
    )
    .await
}