//! 请求日志查询。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::error::AppResult;
use crate::shell::AppShell;
use crate::storage::logs::{LogFilter, RequestDetail, RequestLog};

#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: i64,
}

#[tauri::command]
pub async fn query_logs(
    shell: State<'_, Arc<AppShell>>,
    filter: LogFilter,
) -> AppResult<Page<RequestLog>> {
    let (items, total) = crate::storage::logs::query(&shell.db, &filter).await?;
    Ok(Page { items, total })
}

#[tauri::command]
pub async fn get_request_detail(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
) -> AppResult<Option<RequestDetail>> {
    crate::storage::logs::get_detail(&shell.db, &request_id).await
}

/// 清空请求明细的返回：各删了多少条，供前端提示。
#[derive(Debug, Clone, Serialize)]
pub struct ClearResult {
    pub logs: u64,
    pub captures: u64,
}

/// 清空请求日志与捕获原文。
///
/// 只动这两张表：`usage_hourly` 与内存里的累计计数器是计费口径的历史账目，
/// 不该被监控页上的一个「清空」按钮抹掉。
#[tauri::command]
pub async fn clear_logs(shell: State<'_, Arc<AppShell>>) -> AppResult<ClearResult> {
    let (logs, captures) = crate::storage::logs::clear_all(&shell.db).await?;
    Ok(ClearResult { logs, captures })
}
