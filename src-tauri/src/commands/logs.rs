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
    /// 表达式筛选没扫完（扫描预算用尽）时为真 —— 此时 `total` 只是**扫过的那部分**
    /// 里的匹配数。普通查询恒为 false。界面必须如实说出来，否则用户会把一个
    /// 少了的数字当成全部。
    #[serde(default)]
    pub truncated: bool,
}

#[tauri::command]
pub async fn query_logs(
    shell: State<'_, Arc<AppShell>>,
    filter: LogFilter,
) -> AppResult<Page<RequestLog>> {
    let page = crate::storage::logs::query(&shell.db, &filter).await?;
    Ok(Page { items: page.items, total: page.total, truncated: page.truncated })
}

/// 最近活跃的对话。
#[tauri::command]
pub async fn list_sessions(
    shell: State<'_, Arc<AppShell>>,
    filter: crate::storage::sessions::SessionFilter,
) -> AppResult<Vec<crate::storage::sessions::SessionSummary>> {
    let mut list = crate::storage::sessions::list(&shell.db, &filter).await?;
    // 固定信息从内存那份取：它就是管线此刻在用的那份，界面显示的与实际生效的不会分叉。
    for s in &mut list {
        s.pin = shell.session_pins.get(&s.session_id);
    }
    Ok(list)
}

/// 一个对话的完整链路：各条线 + 按时间排好的请求 + 压缩点。
#[tauri::command]
pub async fn get_session_timeline(
    shell: State<'_, Arc<AppShell>>,
    session_id: String,
) -> AppResult<crate::storage::sessions::SessionTimeline> {
    let mut t = crate::storage::sessions::timeline(&shell.db, &session_id).await?;
    t.pin = shell.session_pins.get(&session_id);
    Ok(t)
}

/// 把一个对话固定到某个模型（已固定过就改成新的）。
///
/// 写完必须 `reload_session_pins`：管线读的是内存里那份，不重载就要等到重启才生效。
#[tauri::command]
pub async fn pin_session_model(
    shell: State<'_, Arc<AppShell>>,
    session_id: String,
    model: String,
    scope: crate::routing::session_pin::PinScope,
) -> AppResult<()> {
    let model = model.trim();
    if session_id.trim().is_empty() || model.is_empty() {
        return Err(crate::error::AppError::msg("会话与模型都不能为空"));
    }
    crate::storage::session_pins::upsert(
        &shell.db,
        &crate::routing::session_pin::SessionPin {
            session_id,
            model: model.to_string(),
            scope,
            created_at: crate::util::now_ms(),
        },
    )
    .await?;
    shell.reload_session_pins().await
}

/// 撤销对话的模型固定。
#[tauri::command]
pub async fn unpin_session_model(
    shell: State<'_, Arc<AppShell>>,
    session_id: String,
) -> AppResult<()> {
    crate::storage::session_pins::delete(&shell.db, &session_id).await?;
    shell.reload_session_pins().await
}

/// 筛选下拉的候选值。前端进监控页时取一次。
#[tauri::command]
pub async fn list_log_facets(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<crate::storage::logs::LogFacets> {
    crate::storage::logs::facets(&shell.db, 2000).await
}

/// 校验筛选表达式能否编译。给编辑框做行内报错用。
///
/// 只编译、不求值 —— 求值需要一个真实的请求上下文，而用户打字时还没有。
/// 返回 `Err` 时前端拿到的就是给用户看的那句话。
#[tauri::command]
pub fn validate_log_expr(expr: String) -> Result<(), String> {
    crate::traffic::log_filter::validate(&expr)
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

/// 返回当前正在进行的请求列表。供监控页进入时补齐遗漏的进行中条目。
///
/// 前端靠事件驱动维护「进行中」列表，但如果用户在请求开始后才打开监控页，
/// 那些 `request-start` 事件已经发出去了、没有人收到。这个命令用于补查。
///
/// 连**已经推出去的帧**一起给（见 `InflightStore`）：只补元信息的话，
/// 用户看到的是一条空白的实时视图，而那条流其实早就吐了几百个事件。
#[tauri::command]
pub async fn get_inflight_requests(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<Vec<crate::traffic::InflightEntry>> {
    Ok(shell.inflight.snapshot())
}

/// 一条请求的入站报文，供实时视图的「请求」页签展示。
///
/// 名字里还带 `inflight` 是因为入口在实时视图，但它**不只在「进行中」时才有**：
/// 请求一结束，内存里那份随底账撤掉，这里接着读数据库里落库的那份。用户从实时
/// 视图点进「请求」页签，看到的应当是"这次请求发了什么"，而不是"这条已经结束，
/// 去明细分页里自己找" —— 何况流刚结束时那行日志可能还没写完。
#[derive(Debug, Clone, Serialize)]
pub struct InflightRequestDetail {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub protocol: String,
    pub headers: serde_json::Value,
    /// 请求体原文。非 UTF-8 或有二进制时按 lossy 处理 —— 界面上要的是能读的文本，
    /// 不是逐字节精确的回放（回放那边有自己的路）。
    pub body: Option<String>,
    /// 用入站协议解出来的 IR。解不出来时 `value` 为空、`error` 写原因，
    /// 界面据此退回「原始」。整个为 null 表示根本没有报文。
    pub view: Option<crate::protocol::inspect::DecodedRequest>,
    /// 这份报文从哪来：`live` = 还在跑，内存里的；`detail` = 已落库的捕获。
    ///
    /// 界面据此说明"这是落库那份"，也让"没抓到"与"请求还没写完"能分开讲。
    pub source: String,
}

/// 取一条请求的入站报文。先在「进行中」底账里找，找不到就落到数据库的捕获。
///
/// 两处都没有（捕获被关掉 / 已被裁剪 / 这条压根不存在）才返回 `None`。
#[tauri::command]
pub async fn get_inflight_request(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
) -> AppResult<Option<InflightRequestDetail>> {
    if let Some(snap) = shell.inflight.request_snapshot(&request_id) {
        let body = snap
            .body
            .as_deref()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let view = crate::protocol::inspect::decode_request_text(
            &shell.codecs,
            &snap.protocol,
            body.as_deref(),
        );
        return Ok(Some(InflightRequestDetail {
            request_id,
            method: snap.method,
            path: snap.path,
            protocol: snap.protocol,
            headers: snap.headers,
            body,
            view,
            source: "live".into(),
        }));
    }

    // 已经结束：读落库那份。这里只取入站一侧，不必解上游 / 响应那几份报文 ——
    // 实时视图的「响应」页签看的是流本身，历史报文属于明细弹窗。
    let Some(detail) = crate::storage::logs::get_detail(&shell.db, &request_id).await? else {
        return Ok(None);
    };
    let view = crate::protocol::inspect::decode_request_text(
        &shell.codecs,
        &detail.protocol_in,
        detail.request_body.as_deref(),
    );
    Ok(Some(InflightRequestDetail {
        request_id,
        method: detail.method,
        path: detail.path,
        protocol: detail.protocol_in,
        headers: detail.request_headers,
        body: detail.request_body,
        view,
        source: "detail".into(),
    }))
}

/// 中断一条进行中的请求；`retry` 为真时顺带让它重跑一遍。
///
/// 返回 `false` 表示这条请求已经不在「进行中」里了（刚跑完，或者界面上的那条
/// 早就结束了）—— 界面据此说明"没赶上"，而不是假装点了有用。
///
/// 重试**不是**在同一条请求里换个渠道继续：网关会把它当成一次全新的请求
/// （新 `request_id`、新日志行），旧的那条记 499「已手动中断并重试」。
/// 客户端的连接始终不动 —— 除了卡住的那段等待，它看不出中间换过一次。
#[tauri::command]
pub async fn interrupt_inflight_request(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
    retry: bool,
) -> AppResult<bool> {
    let signal = if retry {
        crate::traffic::control::RequestSignal::Retry
    } else {
        crate::traffic::control::RequestSignal::Cancel
    };
    Ok(shell.inflight.request_signal(&request_id, signal))
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

/// 删除单条请求日志及其捕获原文。
///
/// 范围与 `clear_logs` 相同，只是限定到一条 —— 计费聚合同样不受影响。
/// 记录已被后台裁剪掉时返回全零，不当成错误。
#[tauri::command]
pub async fn delete_log(
    shell: State<'_, Arc<AppShell>>,
    request_id: String,
) -> AppResult<ClearResult> {
    let (logs, captures) = crate::storage::logs::delete_one(&shell.db, &request_id).await?;
    Ok(ClearResult { logs, captures })
}
