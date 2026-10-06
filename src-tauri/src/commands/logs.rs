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

/// 详情命令的返回：日志 + 捕获原文 + 语义化视图。
///
/// `flatten` 让前端看到的就是一个扁平的 `RequestDetail` 再加一个 `views` 字段
/// —— 既有的类型定义不用改结构。
#[derive(Debug, Clone, Serialize)]
pub struct DetailResponse {
    #[serde(flatten)]
    pub detail: RequestDetail,
    /// 把捕获的报文解成 IR 的结果。解不出来的方向会带上原因。
    pub views: crate::protocol::inspect::DetailViews,
}

#[tauri::command]
pub async fn get_request_detail(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
) -> AppResult<Option<DetailResponse>> {
    let Some(detail) = crate::storage::logs::get_detail(&shell.db, &request_id).await? else {
        return Ok(None);
    };

    // 解码放在这里而不是落库时：它只在用户点开某一条详情时才跑，
    // 而且报文可能很重（请求体动辄上百 KB），不该占用请求热路径。
    let views = crate::protocol::inspect::inspect(&shell.codecs, &detail);

    Ok(Some(DetailResponse { detail, views }))
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
