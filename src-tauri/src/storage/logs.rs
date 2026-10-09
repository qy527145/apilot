//! 请求明细日志与流量捕获的读写。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, SqlitePool};

use crate::error::AppResult;
// `Row` 已经被 sqlx 占了，这里给表达式的行另起一个名字，免得读的人以为
// 「行」是数据库行 —— 它其实是喂给 JS 求值器的一行。
use crate::traffic::log_filter::{lazy_slot, Payload, Row as ExprRow};

/// 写入一条请求日志所需的全部字段。
///
/// 实现 `Serialize` 是因为它也会作为实时事件推送给前端。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestLogRecord {
    pub request_id: String,
    pub ts: i64,
    pub client: String,
    pub protocol_in: String,
    pub protocol_out: String,
    pub provider_tag: Option<String>,
    /// 渠道的整数 id，落库后可 JOIN providers.name 获取最新名称。
    pub provider_id: Option<i64>,
    pub channel_kind: Option<String>,
    pub model: String,
    pub request_model: String,
    /// 入站请求路径（客户端打给 Apilot 的，如 `/v1/responses`）。
    pub path: String,
    /// Apilot 实际请求的上游 URL。缓存命中或路由失败时为 `None`。
    pub upstream_url: Option<String>,
    /// 映射后实际发给上游的模型名。与 `model`（客户端要的）不同时，
    /// 排查"上游说模型不存在"能一眼看出是映射把它改错了。
    pub upstream_model: Option<String>,
    /// 上游返回的原始状态码。与 `status_code` 分开记：网关可能把上游的
    /// 404 包装成 502 再返回给客户端，只留一个数字就对不上了。
    pub upstream_status: Option<i32>,
    pub is_stream: bool,
    pub status_code: i32,
    pub error_message: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub usage_source: String,
    pub quota: i64,
    pub cost_usd: f64,
    pub latency_ms: i64,
    pub ttfb_ms: Option<i64>,
    pub cache_hit: bool,
    pub saved_quota: i64,
    /// 附加信息（工具调用数、命中的规则等），存成 JSON。
    pub other: serde_json::Value,
}

/// 查询结果里返回给前端的日志行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub request_id: String,
    pub ts: i64,
    pub client: String,
    pub protocol_in: String,
    pub protocol_out: String,
    /// 渠道的文本 slug（历史兼容，写入时同步写 provider_id）。
    pub provider_tag: Option<String>,
    /// 渠道整数 id；查询时 LEFT JOIN providers 得到最新名称。
    pub provider_id: Option<i64>,
    /// 渠道当前名称（JOIN 自 providers，改名后自动更新）。
    pub provider_name: Option<String>,
    pub model: String,
    pub request_model: String,
    pub path: String,
    pub upstream_url: Option<String>,
    pub upstream_model: Option<String>,
    pub upstream_status: Option<i32>,
    pub is_stream: bool,
    pub status_code: i32,
    pub error_message: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub usage_source: String,
    pub quota: i64,
    pub cost_usd: f64,
    pub latency_ms: i64,
    pub ttfb_ms: Option<i64>,
    pub cache_hit: bool,
    pub saved_quota: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogFilter {
    pub client: Option<String>,
    pub model: Option<String>,
    pub provider_tag: Option<String>,
    /// unix 毫秒，闭区间。
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub only_cache_hit: Option<bool>,
    /// 入站协议（`protocol_in`）。认不出的值被忽略，等同于不筛。
    pub protocol: Option<String>,
    /// `"ok"`（< 400）或 `"error"`（≥ 400）。认不出的值被忽略。
    pub status: Option<String>,
    /// 只看流式 / 只看非流式。
    pub is_stream: Option<bool>,
    /// 客户端请求的模型名精确匹配（`request_model` 列）。
    pub request_model: Option<String>,
    /// 模型名模糊匹配，同时命中 `model`（实际路由模型）和 `request_model`（客户端请求模型）。
    pub model_like: Option<String>,
    /// 自定义 JS 表达式筛选。对「日志字段 + 捕获报文」求值，命中的才返回。
    /// 求值在后端做，见 `traffic::log_filter` 里关于「为什么不能放前端」的说明。
    pub expr: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

/// 手写而不是 derive：`#[serde(default = "default_limit")]` 只管反序列化，
/// derive 出来的 `Default` 会给 `limit: 0`，再被 `normalized()` 夹成 1 ——
/// 于是"不传筛选条件"悄悄变成"只返回一条"。
impl Default for LogFilter {
    fn default() -> Self {
        Self {
            client: None,
            model: None,
            provider_tag: None,
            from: None,
            to: None,
            only_cache_hit: None,
            protocol: None,
            status: None,
            is_stream: None,
            request_model: None,
            model_like: None,
            expr: None,
            limit: default_limit(),
            offset: 0,
        }
    }
}

pub const STATUS_OK: &str = "ok";
pub const STATUS_ERROR: &str = "error";

impl LogFilter {
    /// 夹住分页参数，避免一条查询把整个表拉出来。
    ///
    /// 同时把认不出的枚举值清掉：筛选条件来自 URL / 前端状态，一个过期或拼错的
    /// 字符串应该等于"不筛"，而不是让整个查询报错、页面变成一片错误提示。
    pub fn normalized(mut self) -> Self {
        self.limit = self.limit.clamp(1, 1000);
        self.offset = self.offset.max(0);

        self.protocol = self
            .protocol
            .map(|p| p.trim().to_string())
            .filter(|p| crate::protocol::dto::Protocol::parse(p).is_some());

        self.status = self
            .status
            .map(|s| s.trim().to_string())
            .filter(|s| s == STATUS_OK || s == STATUS_ERROR);

        self.model_like = self
            .model_like
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        self.expr = self
            .expr
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        self
    }
}

const LOG_COLUMNS: &str = "l.request_id, l.ts, l.client, l.protocol_in, l.protocol_out, l.provider_tag, \
     l.provider_id, p.name AS provider_name, \
     l.model, l.request_model, l.path, l.upstream_url, l.upstream_model, l.upstream_status, l.is_stream, \
     l.status_code, l.error_message, l.input_tokens, l.output_tokens, \
     l.cache_read_tokens, l.cache_creation_tokens, l.usage_source, l.quota, l.cost_usd, l.latency_ms, \
     l.ttfb_ms, l.cache_hit, l.saved_quota";

const LOG_FROM: &str = "request_logs l LEFT JOIN providers p ON l.provider_id = p.id";

fn row_to_log(r: &sqlx::sqlite::SqliteRow) -> RequestLog {
    RequestLog {
        request_id: r.get("request_id"),
        ts: r.get("ts"),
        client: r.get("client"),
        protocol_in: r.get("protocol_in"),
        protocol_out: r.get("protocol_out"),
        provider_tag: r.get("provider_tag"),
        provider_id: r.get("provider_id"),
        provider_name: r.get("provider_name"),
        model: r.get("model"),
        request_model: r.get("request_model"),
        path: r.get("path"),
        upstream_url: r.get("upstream_url"),
        upstream_model: r.get("upstream_model"),
        upstream_status: r.get("upstream_status"),
        is_stream: r.get::<i64, _>("is_stream") != 0,
        status_code: r.get("status_code"),
        error_message: r.get("error_message"),
        input_tokens: r.get::<i64, _>("input_tokens").max(0) as u64,
        output_tokens: r.get::<i64, _>("output_tokens").max(0) as u64,
        cache_read_tokens: r.get::<i64, _>("cache_read_tokens").max(0) as u64,
        cache_creation_tokens: r.get::<i64, _>("cache_creation_tokens").max(0) as u64,
        usage_source: r.get("usage_source"),
        quota: r.get("quota"),
        cost_usd: r.get("cost_usd"),
        latency_ms: r.get("latency_ms"),
        ttfb_ms: r.get("ttfb_ms"),
        cache_hit: r.get::<i64, _>("cache_hit") != 0,
        saved_quota: r.get("saved_quota"),
    }
}

/// 写入一条请求日志。
pub async fn insert(pool: &SqlitePool, rec: &RequestLogRecord) -> AppResult<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO request_logs (
             request_id, ts, client, protocol_in, protocol_out, provider_tag, provider_id,
             channel_kind, model, request_model, path, upstream_url, upstream_model,
             upstream_status, is_stream, status_code, error_message,
             input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
             reasoning_tokens, usage_source, quota, cost_usd, latency_ms, ttfb_ms,
             cache_hit, saved_quota, other)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                 ?22,?23,?24,?25,?26,?27,?28,?29,?30)",
    )
    .bind(&rec.request_id)
    .bind(rec.ts)
    .bind(&rec.client)
    .bind(&rec.protocol_in)
    .bind(&rec.protocol_out)
    .bind(&rec.provider_tag)
    .bind(rec.provider_id)
    .bind(&rec.channel_kind)
    .bind(&rec.model)
    .bind(&rec.request_model)
    .bind(&rec.path)
    .bind(&rec.upstream_url)
    .bind(&rec.upstream_model)
    .bind(rec.upstream_status)
    .bind(rec.is_stream as i64)
    .bind(rec.status_code)
    .bind(&rec.error_message)
    .bind(rec.input_tokens as i64)
    .bind(rec.output_tokens as i64)
    .bind(rec.cache_read_tokens as i64)
    .bind(rec.cache_creation_tokens as i64)
    .bind(rec.reasoning_tokens as i64)
    .bind(&rec.usage_source)
    .bind(rec.quota)
    .bind(rec.cost_usd)
    .bind(rec.latency_ms)
    .bind(rec.ttfb_ms)
    .bind(rec.cache_hit as i64)
    .bind(rec.saved_quota)
    .bind(serde_json::to_string(&rec.other)?)
    .execute(pool)
    .await?;
    Ok(())
}

/// 一页日志。
///
/// `truncated` 只可能在表达式筛选下为真 —— 见 `query_with_expr`：那时 `total`
/// 是**扫过的那部分**里的匹配数，不是全库的口径。
#[derive(Debug, Clone, Serialize)]
pub struct LogPage {
    pub items: Vec<RequestLog>,
    pub total: i64,
    pub truncated: bool,
}

/// 按条件分页查询。
pub async fn query(pool: &SqlitePool, filter: &LogFilter) -> AppResult<LogPage> {
    let filter = filter.clone().normalized();

    // 表达式筛选走另一条路：它要读捕获报文，没法下推给 SQL 分页。
    if let Some(expr) = filter.expr.clone() {
        return query_with_expr(pool, &filter, &expr).await;
    }

    // 计数与取数用同一套 WHERE，避免两者口径漂移。
    let mut count_qb: QueryBuilder<sqlx::Sqlite> =
        QueryBuilder::new(format!("SELECT COUNT(*) FROM {LOG_FROM} WHERE 1=1"));
    push_where(&mut count_qb, &filter);
    let total: i64 = count_qb.build_query_scalar().fetch_one(pool).await?;

    let mut qb: QueryBuilder<sqlx::Sqlite> =
        QueryBuilder::new(format!("SELECT {LOG_COLUMNS} FROM {LOG_FROM} WHERE 1=1"));
    push_where(&mut qb, &filter);
    qb.push(" ORDER BY l.ts DESC, l.id DESC LIMIT ")
        .push_bind(filter.limit)
        .push(" OFFSET ")
        .push_bind(filter.offset);

    let rows = qb.build().fetch_all(pool).await?;
    Ok(LogPage { items: rows.iter().map(row_to_log).collect(), total, truncated: false })
}

/// 表达式筛选时最多扫多少行。
///
/// 表达式没法下推给 SQL —— 它要读捕获报文，那是另一张表的内容。所以只能先把
/// 候选行捞上来、在内存里逐行求值、再在内存里分页。限制扫描量是为了让最坏情况
/// 有界：本地库几十万行时全表求值会把界面卡死好几秒。
///
/// 代价是**超过这个数的匹配项不会出现在结果里**，因此界面上的「共 N 条」在
/// 表达式生效时实际是「最近 N 行里的匹配数」。这是刻意的取舍。
const EXPR_SCAN_CAP: i64 = 5000;

/// 第二趟取的捕获列 —— **一行捕获里除 request_id 之外的全部**。
///
/// 第一趟一列都不取，两个理由都是实测出来的：
///
/// - body 列是几 MB 的 BLOB，读进来再解析就是 out of memory 那个 bug 的一半；
/// - 连 `method`、`headers` 这种小字段也不能在第一趟取。它们住在 `captures` 里，
///   而那张表的行被几 MB 的报文撑得极长 —— 只为读一个小字段 JOIN 上去，
///   「只看状态码」的表达式就从 15ms 变成 800ms（本机 425 行实测）。
///
/// 别名 `c_` 前缀用来和日志列区分 —— 两张表都有 `upstream_url`、`path` 这类
/// 同名列，不前缀会在 `row.get` 撞上。
const PAYLOAD_COLUMNS: &str = "method AS c_method, \
     request_headers AS c_request_headers, response_headers AS c_response_headers, \
     upstream_headers AS c_upstream_headers, \
     upstream_response_headers AS c_upstream_response_headers, \
     request_body AS c_request_body, response_body AS c_response_body, \
     stream_text AS c_stream_text, upstream_body AS c_upstream_body, \
     upstream_response_body AS c_upstream_response_body";

/// 第二趟最多把多少字节的捕获读进内存。
///
/// 按**字节**而不是行数：行数拦不住「400 行 × 1.7MB」，而按字节算，只读 headers
/// 的表达式（每行几 KB）能覆盖几千行，读 body 的只剩几十行 —— 两种都恰好落在
/// 「一次查询几百 MB」这条安全线内。上限本身也只是兜底，真正先撞上的通常是
/// 求值器那边的时间预算（每行解析上百毫秒）。
const PAYLOAD_FETCH_BUDGET: usize = 64 * 1024 * 1024;

/// 带表达式的查询：两趟求值 → 在内存里过滤和分页。
///
/// **第一趟**只查 `request_logs`，连 `captures` 都不 JOIN：凡是来自捕获的字段
/// （method、headers、四个方向的 body）在元数据里都是哨兵，表达式读到的是
/// `null`。于是「只看状态码」这类表达式**一次都不碰**那张几 MB 一行的表，
/// 5000 行的窗口也就跑得完。
///
/// 读过报文的行会被求值器标成 `dirty` —— 包括读到 `null`、以及没写可选链时
/// 直接抛错（`ctx.request.body.tools`）这两种。它们的结果**不可信**，必须由
/// **第二趟**带上真报文重算。没被标记的行结果已经定音，不必再碰。
///
/// 第二趟按 `PAYLOAD_FETCH_BUDGET` 截住，超出就如实标 `truncated`。
async fn query_with_expr(pool: &SqlitePool, filter: &LogFilter, expr: &str) -> AppResult<LogPage> {
    query_with_expr_budgeted(pool, filter, expr, PAYLOAD_FETCH_BUDGET).await
}

/// 预算可调的版本。测试传小预算，才能不真的造 64MB 报文。
async fn query_with_expr_budgeted(
    pool: &SqlitePool,
    filter: &LogFilter,
    expr: &str,
    payload_budget: usize,
) -> AppResult<LogPage> {
    let mut qb: QueryBuilder<sqlx::Sqlite> =
        QueryBuilder::new(format!("SELECT {LOG_COLUMNS} FROM {LOG_FROM} WHERE 1=1"));
    // 其余筛选条件（客户端、协议、时间…）照常下推给 SQL —— 少求值一行是一行。
    push_where(&mut qb, filter);
    qb.push(" ORDER BY l.ts DESC, l.id DESC LIMIT ")
        .push_bind(EXPR_SCAN_CAP);

    let rows = qb.build().fetch_all(pool).await?;
    let logs: Vec<RequestLog> = rows.iter().map(row_to_log).collect();

    let first = crate::traffic::log_filter::probe(expr, logs.len(), |i| ExprRow {
        meta: expr_meta(&logs[i]),
        payload: Payload::default(),
    })
    .map_err(expr_err)?;

    let mut mask = first.mask;
    let mut truncated = first.truncated;
    let mut dirty = first.dirty;

    if !dirty.is_empty() {
        let ids: Vec<&str> = dirty.iter().map(|&i| logs[i].request_id.as_str()).collect();

        // 先问一句每行的捕获有多大，再按扫描顺序累加到预算为止：只取前面这些行，
        // 内存才有上限。`length()` 走的是记录头，不会把 BLOB 内容拖出来。
        let sizes = fetch_payload_sizes(pool, &ids).await?;
        let mut left = payload_budget;
        let mut take = 0usize;
        for id in &ids {
            let size = sizes.get(*id).copied().unwrap_or(0);
            if size > left {
                break;
            }
            left -= size;
            take += 1;
        }
        // 第一行就超预算时也硬着头皮算它一行：一行大报文的成败该由求值器的内存
        // 上限去判（它会给出「报文过大」的错因），而不是在这里被静默跳过 ——
        // 那样用户看到的会是「一条都没匹配」，方向完全错。
        if take == 0 {
            take = 1;
        }
        if take < dirty.len() {
            dirty.truncate(take);
            truncated = true;
        }

        let ids: Vec<&str> = dirty.iter().map(|&i| logs[i].request_id.as_str()).collect();
        let mut payloads = fetch_payloads(pool, &ids).await?;

        let second = crate::traffic::log_filter::filter(expr, dirty.len(), |j| ExprRow {
            meta: expr_meta(&logs[dirty[j]]),
            payload: payloads.remove(&logs[dirty[j]].request_id).unwrap_or_default(),
        })
        .map_err(expr_err)?;

        // 第二趟自己也会撞预算。没轮到的行**不能**留着第一趟的结果 ——
        // 那是拿空报文算出来的，留着就是撒谎。按不命中处理并把 truncated 立起来。
        let recomputed = second.mask.len();
        for (j, hit) in second.mask.into_iter().enumerate() {
            mask[dirty[j]] = hit;
        }
        for &i in dirty.iter().skip(recomputed) {
            mask[i] = false;
        }
        if second.truncated || recomputed < dirty.len() {
            truncated = true;
        }
    }

    // 截断时 `mask` 比 `logs` 短：没被求值的行在这里被 zip 丢掉（按不命中处理），
    // 与「扫描窗口」是同一类取舍 —— 由 `truncated` 如实告诉用户。
    let matched: Vec<RequestLog> = logs
        .into_iter()
        .zip(mask)
        .filter_map(|(l, hit)| hit.then_some(l))
        .collect();

    let total = matched.len() as i64;
    let offset = filter.offset.max(0) as usize;
    let limit = filter.limit.max(1) as usize;
    let items = matched.into_iter().skip(offset).take(limit).collect();
    Ok(LogPage { items, total, truncated })
}

/// 表达式出错时的统一错因。
///
/// 表达式出错（语法错、某一行**自己**跑飞）时**如实报错**，不能静默返回空 ——
/// 空结果看起来就是「没有匹配的请求」，会把用户引到完全错误的方向。
/// 「扫到一半停下」不在此列：那不是错误，走 `Outcome::truncated`。
fn expr_err(e: String) -> crate::error::AppError {
    crate::error::AppError::msg(format!("筛选表达式出错：{e}"))
}

/// 只把这几个 request_id 的报文选出来。
///
/// **只搬字节、不解析** —— 解析推迟到表达式真的读到它的时候（见 `traffic::log_filter`）。
async fn fetch_payloads(pool: &SqlitePool, ids: &[&str]) -> AppResult<HashMap<String, Payload>> {
    let mut qb: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(format!(
        "SELECT c.request_id AS c_request_id, {PAYLOAD_COLUMNS} FROM captures c \
         WHERE c.request_id IN ("
    ));
    push_in_list(&mut qb, ids);

    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<String, _>("c_request_id"), expr_payload(r)))
        .collect())
}

/// 这几行的捕获各有多大（字节）。第二趟据此决定能带上多少行。
///
/// 只取长度不取内容：`length()` 读的是记录头里的字段宽度，不会把几 MB 的 BLOB
/// 拖出来 —— 否则这个「先看看有多大」的查询自己就先把内存吃掉了。
async fn fetch_payload_sizes(pool: &SqlitePool, ids: &[&str]) -> AppResult<HashMap<String, usize>> {
    let mut qb: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
        "SELECT request_id, coalesce(length(request_body), 0) \
             + coalesce(length(response_body), 0) + coalesce(length(stream_text), 0) \
             + coalesce(length(upstream_body), 0) \
             + coalesce(length(upstream_response_body), 0) \
             + coalesce(length(request_headers), 0) \
             + coalesce(length(response_headers), 0) \
             + coalesce(length(upstream_headers), 0) \
             + coalesce(length(upstream_response_headers), 0) AS bytes \
         FROM captures WHERE request_id IN (",
    );
    push_in_list(&mut qb, ids);

    let rows = qb.build().fetch_all(pool).await?;
    Ok(rows.iter().map(|r| (r.get::<String, _>("request_id"), r.get::<i64, _>("bytes") as usize)).collect())
}

/// 给 `IN (...)` 填参数。两个查询共用，免得各写一遍还写歪。
fn push_in_list(qb: &mut QueryBuilder<'_, sqlx::Sqlite>, ids: &[&str]) {
    let mut sep = qb.separated(", ");
    for id in ids {
        sep.push_bind((*id).to_string());
    }
    sep.push_unseparated(")");
}

/// 组装表达式能看到的**元数据** —— 就是界面上「内置对象」那份说明的实现。
///
/// 字段名用 camelCase：写表达式的用户面对的是 JS，`latency_ms` 那种下划线
/// 命名在 JS 里格格不入。**改这里就必须同步改前端的说明面板与
/// `src/lib/logExpr.ts` 的进行中求值**，否则文档与行为会对不上。
///
/// 凡是从捕获来的字段（method、headers、四个方向的 body）只放哨兵，真值由
/// `expr_payload` 按需给 —— 所以这里**不需要数据库行**，也就不会 JOIN captures。
fn expr_meta(log: &RequestLog) -> String {
    use serde_json::json;

    json!({
        // --- 日志本身的字段 ---
        "id": log.request_id,
        "ts": log.ts,
        "client": log.client,
        "model": log.model,
        "requestModel": log.request_model,
        "upstreamModel": log.upstream_model,
        "protocolIn": log.protocol_in,
        "protocolOut": log.protocol_out,
        "provider": log.provider_name.clone().or_else(|| log.provider_tag.clone()),
        "path": log.path,
        "status": log.status_code,
        "upstreamStatus": log.upstream_status,
        "isStream": log.is_stream,
        "error": log.error_message,
        "latencyMs": log.latency_ms,
        "ttfbMs": log.ttfb_ms,
        "inputTokens": log.input_tokens,
        "outputTokens": log.output_tokens,
        "cacheReadTokens": log.cache_read_tokens,
        "cacheCreationTokens": log.cache_creation_tokens,
        "quota": log.quota,
        "costUsd": log.cost_usd,
        "cacheHit": log.cache_hit,

        // --- 客户端 → Apilot ---
        "request": {
            "method": lazy_slot("request.method"),
            "path": log.path,
            "headers": lazy_slot("request.headers"),
            "body": lazy_slot("request.body"),
        },
        // --- Apilot → 客户端 ---
        "response": {
            "headers": lazy_slot("response.headers"),
            "body": lazy_slot("response.body"),
        },
        // --- Apilot → 上游 ---
        "upstreamRequest": {
            "url": log.upstream_url,
            "headers": lazy_slot("upstreamRequest.headers"),
            "body": lazy_slot("upstreamRequest.body"),
        },
        // --- 上游 → Apilot ---
        "upstreamResponse": {
            "status": log.upstream_status,
            "headers": lazy_slot("upstreamResponse.headers"),
            "body": lazy_slot("upstreamResponse.body"),
        },
    })
    .to_string()
}

/// 一行里按需取用的捕获内容。构造它只搬字节，**不解析** —— 那正是它能按需的原因。
fn expr_payload(r: &sqlx::sqlite::SqliteRow) -> Payload {
    Payload {
        request_method: r.get::<Option<String>, _>("c_method"),
        request_headers: r.get::<Option<String>, _>("c_request_headers"),
        response_headers: r.get::<Option<String>, _>("c_response_headers"),
        upstream_request_headers: r.get::<Option<String>, _>("c_upstream_headers"),
        upstream_response_headers: r.get::<Option<String>, _>("c_upstream_response_headers"),
        request_body: r.get::<Option<Vec<u8>>, _>("c_request_body"),
        response_body: r.get::<Option<Vec<u8>>, _>("c_response_body"),
        response_stream_text: r.get::<Option<String>, _>("c_stream_text"),
        upstream_request_body: r.get::<Option<Vec<u8>>, _>("c_upstream_body"),
        upstream_response_body: r.get::<Option<Vec<u8>>, _>("c_upstream_response_body"),
    }
}

/// 筛选下拉的数据源：最近这些请求里实际出现过的客户端 / 模型 / 协议。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LogFacets {
    pub clients: Vec<String>,
    /// 客户端请求的模型名（`request_model` 列）。
    pub models: Vec<String>,
    /// 实际路由到的模型名（`model` 列）。与 `models` 分开，
    /// 是因为模型策略会把客户端要的名字映射成另一个 —— 两者都要能单选。
    pub routed_models: Vec<String>,
    pub protocols: Vec<String>,
}

/// 只扫最近 `scan` 条，不对全表做 DISTINCT。
///
/// 本地库跑上几个月，`request_logs` 是几十万行级别，而筛选下拉要回答的
/// 只是"最近都有什么"——全表去重既慢又没意义（三个月前用过一次的模型
/// 出现在下拉里只会干扰选择）。
pub async fn facets(pool: &SqlitePool, scan: i64) -> AppResult<LogFacets> {
    let scan = scan.clamp(1, 10_000);
    let rows = sqlx::query(
        "SELECT client, request_model, model, protocol_in FROM request_logs ORDER BY ts DESC LIMIT ?1",
    )
    .bind(scan)
    .fetch_all(pool)
    .await?;

    let mut clients = std::collections::BTreeSet::new();
    let mut models = std::collections::BTreeSet::new();
    let mut routed_models = std::collections::BTreeSet::new();
    let mut protocols = std::collections::BTreeSet::new();

    for r in &rows {
        let c: String = r.get("client");
        let m: String = r.get("request_model");
        let rm: String = r.get("model");
        let p: String = r.get("protocol_in");
        if !c.is_empty() {
            clients.insert(c);
        }
        if !m.is_empty() {
            models.insert(m);
        }
        if !rm.is_empty() {
            routed_models.insert(rm);
        }
        if !p.is_empty() {
            protocols.insert(p);
        }
    }

    Ok(LogFacets {
        clients: clients.into_iter().collect(),
        models: models.into_iter().collect(),
        routed_models: routed_models.into_iter().collect(),
        protocols: protocols.into_iter().collect(),
    })
}

fn push_where(qb: &mut QueryBuilder<'_, sqlx::Sqlite>, f: &LogFilter) {
    if let Some(c) = &f.client {
        qb.push(" AND client = ").push_bind(c.clone());
    }
    if let Some(m) = &f.model {
        qb.push(" AND model = ").push_bind(m.clone());
    }
    if let Some(p) = &f.provider_tag {
        qb.push(" AND provider_tag = ").push_bind(p.clone());
    }
    if let Some(from) = f.from {
        qb.push(" AND ts >= ").push_bind(from);
    }
    if let Some(to) = f.to {
        qb.push(" AND ts <= ").push_bind(to);
    }
    if f.only_cache_hit == Some(true) {
        qb.push(" AND cache_hit = 1");
    }
    if let Some(p) = &f.protocol {
        qb.push(" AND protocol_in = ").push_bind(p.clone());
    }
    match f.status.as_deref() {
        Some(STATUS_OK) => {
            qb.push(" AND status_code < 400");
        }
        Some(STATUS_ERROR) => {
            qb.push(" AND status_code >= 400");
        }
        _ => {}
    }
    if let Some(s) = f.is_stream {
        qb.push(" AND is_stream = ").push_bind(s as i64);
    }
    if let Some(m) = &f.request_model {
        qb.push(" AND request_model = ").push_bind(m.clone());
    }
    if let Some(m) = &f.model_like {
        // 转义 LIKE 的通配符：模型名里出现 % 或 _ 时不该被当成通配符。
        let escaped = m.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        // 同时命中客户端请求的模型名（request_model）和实际路由模型名（model）。
        qb.push(" AND (model LIKE ")
            .push_bind(pattern.clone())
            .push(" ESCAPE '\\' OR request_model LIKE ")
            .push_bind(pattern)
            .push(" ESCAPE '\\')");
    }
}

pub async fn get(pool: &SqlitePool, request_id: &str) -> AppResult<Option<RequestLog>> {
    let row = sqlx::query(&format!(
        "SELECT {LOG_COLUMNS} FROM {LOG_FROM} WHERE l.request_id = ?1"
    ))
    .bind(request_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_log))
}

// ---------------------------------------------------------------------------
// 流量捕获（请求 / 响应原文）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetail {
    pub request_id: String,
    pub ts: i64,
    pub client: String,
    pub protocol_in: String,
    pub protocol_out: String,
    pub provider_tag: Option<String>,
    /// 渠道整数 id，用于关联渠道名称。
    pub provider_id: Option<i64>,
    /// 渠道当前名称（从 providers 表 JOIN 而来，改名后自动更新）。
    pub provider_name: Option<String>,
    pub model: String,
    pub request_model: String,
    pub is_stream: bool,
    pub status_code: i32,
    pub error_message: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub usage_source: String,
    pub quota: i64,
    pub cost_usd: f64,
    pub latency_ms: i64,
    pub ttfb_ms: Option<i64>,
    /// 平均 token 间隔（毫秒）= 解码窗口 / (输出 token - 1)。
    ///
    /// 只对能取出 `speed_sample` 的请求有值 —— 流式、非缓存命中、成功，且首字节
    /// 早于流结束。非流式的首字节就是全文，没有可分的窗口。测不出给 `None`，
    /// 界面显示「—」。
    pub itl_ms: Option<f64>,
    /// 输出速度（token / 秒）= 输出 token / 解码窗口。
    ///
    /// 与 `itl_ms` 同一个样本，见 `speed_sample`。
    pub tps: Option<f64>,
    pub cache_hit: bool,
    pub saved_quota: i64,

    // --- 入站方向：客户端 → Apilot ---
    pub method: String,
    pub path: String,
    pub request_headers: serde_json::Value,
    pub request_body: Option<String>,

    // --- 出站方向：Apilot → 上游 ---
    /// Apilot 实际请求的 URL。缓存命中或路由失败时为 `None`。
    pub upstream_url: Option<String>,
    /// 映射后实际发给上游的模型名。
    pub upstream_model: Option<String>,
    /// 上游返回的原始状态码。
    pub upstream_status: Option<i32>,
    pub upstream_headers: serde_json::Value,
    pub upstream_body: Option<String>,
    pub upstream_response_headers: serde_json::Value,
    pub upstream_response_body: Option<String>,

    // --- 出站方向：Apilot → 客户端 ---
    pub response_headers: serde_json::Value,
    pub response_body: Option<String>,
    pub stream_text: Option<String>,
    pub stream_events: i64,
    /// 流式响应的结构化内容（`UnifiedResponse` 的 JSON）。
    ///
    /// 流式没有完整的响应体可存，但由增量重建出的内容块里有思考与工具调用 ——
    /// 只留 `stream_text`（纯文本）的话这两样就没了。
    pub response_content: Option<String>,
    /// 上游发来的原始 SSE 帧；`raw_truncated` 为真时是被截断过的。
    pub upstream_stream_raw: Option<String>,
    /// 重编码后发给客户端的原始 SSE 帧。直通时为 `None`（与上游那份相同）。
    pub client_stream_raw: Option<String>,
    /// 原始 SSE 帧是否因为超过上限被截断。
    pub stream_raw_truncated: bool,
    /// 每个事件的时间点（`StreamTimings` 的 JSON 文本），供时间轴用。
    ///
    /// 老日志没有这一项（改动前没记），界面上据此不显示时间轴，而不是画一根
    /// 全零的假轴。
    pub stream_timings: Option<String>,
}

/// 出站方向的追踪：Apilot 实际发给上游的请求，与上游返回的原始响应。
///
/// 与 `CaptureRecord` 顶层的入站字段成对存在。两者必须都留：跨协议转换时
/// 上游收到的请求体和我们返回给客户端的响应体内容并不相同，只留一份就看不出
/// 「转换到底做了什么」，而排查上游报错时最需要的恰恰是上游那一份原文。
#[derive(Debug, Clone, Default)]
pub struct UpstreamTrace {
    pub url: String,
    pub headers: serde_json::Value,
    pub body: Option<Vec<u8>>,
    pub status: Option<i32>,
    pub response_headers: serde_json::Value,
    pub response_body: Option<Vec<u8>>,
}

/// 保存一次请求 / 响应的原文。
#[derive(Debug, Clone, Default)]
pub struct CaptureRecord {
    pub request_id: String,
    pub ts: i64,
    // 入站
    pub method: String,
    pub path: String,
    pub request_headers: serde_json::Value,
    pub request_body: Option<Vec<u8>>,
    // 出站
    pub upstream: UpstreamTrace,
    // 返回给客户端
    pub response_headers: serde_json::Value,
    pub response_body: Option<Vec<u8>>,
    pub stream_text: Option<String>,
    pub stream_events: i64,
    /// 流式响应的结构化内容（`UnifiedResponse` 的 JSON 文本）。
    pub response_content: Option<String>,
    pub upstream_stream_raw: Option<Vec<u8>>,
    pub client_stream_raw: Option<Vec<u8>>,
    pub stream_raw_truncated: bool,
    /// 每个事件的时间点（`StreamTimings` 的 JSON 文本），供时间轴用。
    pub stream_timings: Option<String>,
}

pub async fn save_capture(pool: &SqlitePool, c: &CaptureRecord) -> AppResult<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO captures (
             request_id, ts, method, path, request_headers, request_body,
             upstream_url, upstream_headers, upstream_body, upstream_status,
             upstream_response_headers, upstream_response_body,
             response_headers, response_body, stream_text, stream_events,
             response_content, upstream_stream_raw, client_stream_raw, stream_raw_truncated,
             stream_timings)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
    )
    .bind(&c.request_id)
    .bind(c.ts)
    .bind(&c.method)
    .bind(&c.path)
    .bind(serde_json::to_string(&c.request_headers)?)
    .bind(&c.request_body)
    .bind(&c.upstream.url)
    .bind(serde_json::to_string(&c.upstream.headers)?)
    .bind(&c.upstream.body)
    .bind(c.upstream.status)
    .bind(serde_json::to_string(&c.upstream.response_headers)?)
    .bind(&c.upstream.response_body)
    .bind(serde_json::to_string(&c.response_headers)?)
    .bind(&c.response_body)
    .bind(&c.stream_text)
    .bind(c.stream_events)
    .bind(&c.response_content)
    .bind(&c.upstream_stream_raw)
    .bind(&c.client_stream_raw)
    .bind(c.stream_raw_truncated as i64)
    .bind(&c.stream_timings)
    .execute(pool)
    .await?;
    Ok(())
}

/// 读一条完整详情：日志行 + 捕获原文。
pub async fn get_detail(pool: &SqlitePool, request_id: &str) -> AppResult<Option<RequestDetail>> {
    let Some(log) = get(pool, request_id).await? else {
        return Ok(None);
    };

    let cap = sqlx::query(
        "SELECT method, path, request_headers, request_body,
                upstream_url, upstream_headers, upstream_body, upstream_status,
                upstream_response_headers, upstream_response_body,
                response_headers, response_body, stream_text, stream_events,
                response_content, upstream_stream_raw, client_stream_raw, stream_raw_truncated,
                stream_timings
         FROM captures WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;

    // 没有捕获行时全部留空，而不是报错 —— 日志本身（用量、状态码）始终可读。
    let (
        method,
        path,
        request_headers,
        request_body,
        upstream_url,
        upstream_headers,
        upstream_body,
        upstream_status,
        upstream_response_headers,
        upstream_response_body,
        response_headers,
        response_body,
        stream_text,
        stream_events,
        response_content,
        upstream_stream_raw,
        client_stream_raw,
        stream_raw_truncated,
        stream_timings,
    ) = match &cap {
        Some(r) => (
            r.get::<Option<String>, _>("method").unwrap_or_default(),
            r.get::<Option<String>, _>("path").unwrap_or_default(),
            parse_json(r.get::<Option<String>, _>("request_headers")),
            r.get::<Option<Vec<u8>>, _>("request_body"),
            r.get::<Option<String>, _>("upstream_url"),
            parse_json(r.get::<Option<String>, _>("upstream_headers")),
            r.get::<Option<Vec<u8>>, _>("upstream_body"),
            r.get::<Option<i64>, _>("upstream_status").map(|v| v as i32),
            parse_json(r.get::<Option<String>, _>("upstream_response_headers")),
            r.get::<Option<Vec<u8>>, _>("upstream_response_body"),
            parse_json(r.get::<Option<String>, _>("response_headers")),
            r.get::<Option<Vec<u8>>, _>("response_body"),
            r.get::<Option<String>, _>("stream_text"),
            r.get::<Option<i64>, _>("stream_events").unwrap_or(0),
            r.get::<Option<String>, _>("response_content"),
            r.get::<Option<Vec<u8>>, _>("upstream_stream_raw"),
            r.get::<Option<Vec<u8>>, _>("client_stream_raw"),
            r.get::<Option<i64>, _>("stream_raw_truncated").unwrap_or(0) != 0,
            r.get::<Option<String>, _>("stream_timings"),
        ),
        None => (
            String::new(),
            String::new(),
            serde_json::json!({}),
            None,
            None,
            serde_json::json!({}),
            None,
            None,
            serde_json::json!({}),
            None,
            serde_json::json!({}),
            None,
            None,
            0,
            None,
            None,
            None,
            false,
            None,
        ),
    };

    // 上游状态码优先取捕获里的；没捕获时退回日志列的（两者本就同源）。
    let upstream_status = upstream_status.or(log.upstream_status);
    let upstream_url = upstream_url.or(log.upstream_url);

    // 观感指标由已落库的标量推出来，不在落库时算：口径改了不必迁移数据，老日志
    // 打开详情也能看到。统计页那边的累加量是另一回事（v10 迁移）。
    let sample = speed_sample(
        log.is_stream,
        log.cache_hit,
        log.status_code,
        log.latency_ms,
        log.ttfb_ms,
        log.output_tokens,
    );
    let itl_ms = sample.and_then(|s| s.itl_ms());
    let tps = sample.and_then(|s| s.tps());

    Ok(Some(RequestDetail {
        request_id: log.request_id,
        ts: log.ts,
        client: log.client,
        protocol_in: log.protocol_in,
        protocol_out: log.protocol_out,
        provider_tag: log.provider_tag,
        provider_id: log.provider_id,
        provider_name: log.provider_name,
        model: log.model,
        request_model: log.request_model,
        is_stream: log.is_stream,
        status_code: log.status_code,
        error_message: log.error_message,
        input_tokens: log.input_tokens,
        output_tokens: log.output_tokens,
        cache_read_tokens: log.cache_read_tokens,
        cache_creation_tokens: log.cache_creation_tokens,
        usage_source: log.usage_source,
        quota: log.quota,
        cost_usd: log.cost_usd,
        latency_ms: log.latency_ms,
        ttfb_ms: log.ttfb_ms,
        itl_ms,
        tps,
        cache_hit: log.cache_hit,
        saved_quota: log.saved_quota,
        method,
        path: if path.is_empty() { log.path } else { path },
        request_headers,
        // 捕获体按 UTF-8 展示；非法字节用替换字符，不因一个坏字节丢掉整条详情。
        request_body: request_body.map(|b| String::from_utf8_lossy(&b).to_string()),
        upstream_url,
        upstream_model: log.upstream_model,
        upstream_status,
        upstream_headers,
        upstream_body: upstream_body.map(|b| String::from_utf8_lossy(&b).to_string()),
        upstream_response_headers,
        upstream_response_body: upstream_response_body
            .map(|b| String::from_utf8_lossy(&b).to_string()),
        response_headers,
        response_body: response_body.map(|b| String::from_utf8_lossy(&b).to_string()),
        stream_text,
        stream_events,
        response_content,
        upstream_stream_raw: upstream_stream_raw
            .map(|b| String::from_utf8_lossy(&b).to_string()),
        client_stream_raw: client_stream_raw
            .map(|b| String::from_utf8_lossy(&b).to_string()),
        stream_raw_truncated,
        stream_timings,
    }))
}

/// 一条请求提供的观感样本。
///
/// 三个指标里的 TTFT 就是 `ttfb_ms`；ITL 与 TPS 由这里的两个量推出 —— 单条详情
/// 用它算，聚合表也用它累加，**口径只此一处**：同一个模型在详情弹窗与统计页
/// 不会给出两个数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedSample {
    /// 首字节耗时（毫秒）。
    pub ttft_ms: i64,
    /// 解码窗口（毫秒）= 总耗时 - 首字节，恒为正。
    pub decode_ms: i64,
    /// 这段窗口里吐出的 token 数。
    pub output_tokens: u64,
}

impl SpeedSample {
    /// 平均 token 间隔（毫秒）。输出不足两个 token 时没有“间隔”可言。
    pub fn itl_ms(&self) -> Option<f64> {
        (self.output_tokens >= 2)
            .then(|| self.decode_ms as f64 / (self.output_tokens - 1) as f64)
    }

    /// 输出速度（token / 秒）。
    pub fn tps(&self) -> Option<f64> {
        (self.output_tokens > 0)
            .then(|| self.output_tokens as f64 * 1000.0 / self.decode_ms as f64)
    }
}

/// 取一条请求的观感样本；取不出来就说明它没有可与别人比较的解码过程。
///
/// 条件是**流式 + 非缓存命中 + 成功 + 量得出解码窗口**：
///
/// - 非流式那条路把 ttfb 记成整体耗时（上游一次给全文，见 `gateway/pipeline.rs`），
///   差值恒为 0。拿它算“首 token 耗时”是拿整段耗时冒充，算“吐字速度”则要除零。
/// - 缓存命中的耗时是重放时间，token 也不是刚生成的 —— 与生成速度无关。
/// - 失败（≥400）的请求没把 token 吐完，耗时不代表这个模型的能力。
pub fn speed_sample(
    is_stream: bool,
    cache_hit: bool,
    status_code: i32,
    latency_ms: i64,
    ttfb_ms: Option<i64>,
    output_tokens: u64,
) -> Option<SpeedSample> {
    if !is_stream || cache_hit || status_code >= 400 {
        return None;
    }
    // 没等到首字节就没有窗口可言。
    let ttft_ms = ttfb_ms?.max(0);
    // 时钟抖动可能让首字节落在总耗时之后，负数窗口不能拿去算速度。
    let decode_ms = latency_ms.max(0) - ttft_ms;
    if decode_ms <= 0 {
        return None;
    }
    Some(SpeedSample {
        ttft_ms,
        decode_ms,
        output_tokens,
    })
}

fn parse_json(s: Option<String>) -> serde_json::Value {
    s.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// 把捕获表裁剪到 `max_entries` 条，保留最新的。
///
/// 捕获含完整请求/响应体，是数据库增长的主要来源，必须定期裁。
pub async fn prune_captures(pool: &SqlitePool, max_entries: i64) -> AppResult<u64> {
    if max_entries <= 0 {
        return Ok(0);
    }
    let r = sqlx::query(
        "DELETE FROM captures WHERE request_id NOT IN (
             SELECT request_id FROM captures ORDER BY ts DESC LIMIT ?1
         )",
    )
    .bind(max_entries)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// 清空超过保留期的日志明细。
///
/// 捕获表有独立的条数上限，但明细行本身也会无限增长 —— 一个活跃的 Agent
/// 每天能产生上万条。由后台任务定期调用。
pub async fn prune_logs(pool: &SqlitePool, before_ts: i64) -> AppResult<u64> {
    let r = sqlx::query("DELETE FROM request_logs WHERE ts < ?1")
        .bind(before_ts)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// 清空全部请求明细与捕获原文。返回 `(删除的日志条数, 删除的捕获条数)`。
///
/// 刻意**不动** `usage_hourly` 与内存里的累计计数器：那些是计费口径的历史账目，
/// 用户在监控页点"清空日志"是不想让列表继续堆着，不是想把自己的账单抹掉。
pub async fn clear_all(pool: &SqlitePool) -> AppResult<(u64, u64)> {
    // 放一个事务里：两张表要么都空，要么都留，不会出现"列表空了但详情还在"。
    let mut tx = pool.begin().await?;
    let logs = sqlx::query("DELETE FROM request_logs")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let captures = sqlx::query("DELETE FROM captures")
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok((logs, captures))
}

/// 删除单条请求明细与它的捕获原文。返回 `(删除的日志条数, 删除的捕获条数)`。
///
/// 范围与 `clear_all` 一致，只是多了 `request_id` 这一个条件 —— 同样**不动**
/// `usage_hourly` 与内存计数器，删掉一条监控记录不该让账单跟着变。
///
/// 记录已经不存在时返回 `(0, 0)` 而不是报错：后台的定期裁剪随时可能先把它删掉，
/// 用户点的那一行本来就是一份可能过期的快照。
pub async fn delete_one(pool: &SqlitePool, request_id: &str) -> AppResult<(u64, u64)> {
    // 与 clear_all 同理放一个事务：两张表要么都删，要么都留，
    // 不会出现"列表里没有这行了、点进去详情却还在"。
    let mut tx = pool.begin().await?;
    let logs = sqlx::query("DELETE FROM request_logs WHERE request_id = ?1")
        .bind(request_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let captures = sqlx::query("DELETE FROM captures WHERE request_id = ?1")
        .bind(request_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    tx.commit().await?;
    Ok((logs, captures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::now_ms;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn rec(id: &str, client: &str, model: &str) -> RequestLogRecord {
        RequestLogRecord {
            request_id: id.into(),
            ts: now_ms(),
            client: client.into(),
            protocol_in: "anthropic".into(),
            protocol_out: "openai_chat".into(),
            provider_tag: Some("p1".into()),
            provider_id: None,
            channel_kind: Some("openai_chat".into()),
            model: model.into(),
            request_model: model.into(),
            path: "/v1/messages".into(),
            upstream_url: Some("https://api.example.com/v1/chat/completions".into()),
            upstream_model: Some(model.into()),
            upstream_status: Some(200),
            is_stream: true,
            status_code: 200,
            input_tokens: 100,
            output_tokens: 50,
            quota: 300,
            cost_usd: 0.0006,
            latency_ms: 1200,
            ttfb_ms: Some(300),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn insert_and_get_roundtrip() {
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "claude-sonnet-5")).await.unwrap();

        let got = get(&p, "r1").await.unwrap().unwrap();
        assert_eq!(got.client, "claude-code");
        assert_eq!(got.model, "claude-sonnet-5");
        assert!(got.is_stream);
        assert_eq!(got.ttfb_ms, Some(300));
        assert_eq!(got.input_tokens, 100);
    }

    #[tokio::test]
    async fn direction_fields_roundtrip() {
        // 入站路径与出站 URL / 模型 / 状态码都要能存能取 —— 排查上游 404 全靠它们。
        let p = pool().await;
        insert(&p, &rec("r1", "codex", "gpt-5")).await.unwrap();

        let got = get(&p, "r1").await.unwrap().unwrap();
        assert_eq!(got.path, "/v1/messages");
        assert_eq!(
            got.upstream_url.as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
        assert_eq!(got.upstream_model.as_deref(), Some("gpt-5"));
        assert_eq!(got.upstream_status, Some(200));
    }

    #[tokio::test]
    async fn upstream_fields_default_to_none() {
        // 缓存命中 / 路由失败没有上游，不能凭空造一个空字符串出来。
        let p = pool().await;
        let mut r = rec("r1", "a", "m");
        r.upstream_url = None;
        r.upstream_model = None;
        r.upstream_status = None;
        insert(&p, &r).await.unwrap();

        let got = get(&p, "r1").await.unwrap().unwrap();
        assert_eq!(got.upstream_url, None);
        assert_eq!(got.upstream_model, None);
        assert_eq!(got.upstream_status, None);
    }

    #[tokio::test]
    async fn reinserting_same_request_id_replaces() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();

        let mut updated = rec("r1", "a", "m");
        updated.status_code = 500;
        insert(&p, &updated).await.unwrap();

        let LogPage { items, total, .. } = query(&p, &LogFilter::default()).await.unwrap();
        assert_eq!(total, 1, "同一 request_id 不应产生两行");
        assert_eq!(items[0].status_code, 500);
    }

    #[tokio::test]
    async fn query_filters_by_client_and_model() {
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "claude-sonnet-5")).await.unwrap();
        insert(&p, &rec("r2", "codex", "gpt-5")).await.unwrap();

        let LogPage { items, total, .. } = query(
            &p,
            &LogFilter {
                client: Some("codex".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].request_id, "r2");

        let LogPage { items, .. } = query(
            &p,
            &LogFilter {
                model: Some("claude-sonnet-5".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn query_filters_by_time_range() {
        let p = pool().await;
        let mut old = rec("old", "a", "m");
        old.ts = 1_000;
        insert(&p, &old).await.unwrap();
        insert(&p, &rec("new", "a", "m")).await.unwrap();

        let LogPage { total, .. } = query(
            &p,
            &LogFilter {
                from: Some(now_ms() - 1000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn query_filters_by_an_explicit_end_time() {
        // 监控页「自定义时间范围」的下界就是它。两端都是**闭区间**：只给下界的话，
        // 用户选「10:00 到 11:00」会把 11:00 之后的一并带出来。
        let p = pool().await;
        let at = |id: &str, ts: i64| {
            let mut r = rec(id, "a", "m");
            r.ts = ts;
            r
        };
        insert(&p, &at("old", 1_000)).await.unwrap();
        insert(&p, &at("mid", 2_000)).await.unwrap();
        insert(&p, &at("new", 3_000)).await.unwrap();

        let ids = query_ids(
            &p,
            LogFilter {
                from: Some(1_500),
                to: Some(2_500),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(ids, vec!["mid"]);
    }

    #[tokio::test]
    async fn query_filters_cache_hits() {
        let p = pool().await;
        insert(&p, &rec("miss", "a", "m")).await.unwrap();

        let mut hit = rec("hit", "a", "m");
        hit.cache_hit = true;
        hit.saved_quota = 300;
        insert(&p, &hit).await.unwrap();

        let LogPage { items, total, .. } = query(
            &p,
            &LogFilter {
                only_cache_hit: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(total, 1);
        assert_eq!(items[0].request_id, "hit");
    }

    #[tokio::test]
    async fn query_paginates_and_counts_total_independently() {
        let p = pool().await;
        for i in 0..10 {
            insert(&p, &rec(&format!("r{i}"), "a", "m")).await.unwrap();
        }

        let LogPage { items: page, total, .. } = query(
            &p,
            &LogFilter {
                limit: 3,
                offset: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(total, 10, "总数应是满足条件的全部，而不是本页大小");
    }

    #[tokio::test]
    async fn query_returns_newest_first() {
        let p = pool().await;
        for (i, ts) in [(0, 1_000i64), (1, 2_000), (2, 3_000)] {
            let mut r = rec(&format!("r{i}"), "a", "m");
            r.ts = ts;
            insert(&p, &r).await.unwrap();
        }
        let LogPage { items, .. } = query(&p, &LogFilter::default()).await.unwrap();
        assert_eq!(items[0].ts, 3_000);
    }

    #[tokio::test]
    async fn limit_is_clamped_to_protect_the_database() {
        let f = LogFilter {
            limit: 999_999,
            ..Default::default()
        }
        .normalized();
        assert_eq!(f.limit, 1000);

        let f = LogFilter {
            limit: -5,
            ..Default::default()
        }
        .normalized();
        assert_eq!(f.limit, 1);
    }

    #[test]
    fn speed_sample_splits_ttft_from_the_decode_window() {
        // 首字节 1s、总共 3s、吐了 21 个 token → 窗口 2s。
        let s = speed_sample(true, false, 200, 3000, Some(1000), 21).unwrap();
        assert_eq!((s.ttft_ms, s.decode_ms, s.output_tokens), (1000, 2000, 21));
        assert_eq!(s.itl_ms(), Some(100.0)); // 2000 / (21-1)
        assert_eq!(s.tps(), Some(10.5)); // 21 个 token / 2s
    }

    #[test]
    fn non_stream_and_single_chunk_streams_have_no_sample() {
        // 非流式：ttfb 被记成整体耗时，窗口为 0。
        assert_eq!(speed_sample(false, false, 200, 2000, Some(2000), 20), None);
        // 流式但整段正文随首字节一起到（上游无视 stream，由我们重编码）：同样没有窗口。
        assert_eq!(speed_sample(true, false, 200, 2000, Some(2000), 20), None);
        assert_eq!(
            speed_sample(true, false, 200, 2000, None, 20),
            None,
            "没等到首字节"
        );
    }

    #[test]
    fn cache_hits_and_failures_have_no_sample() {
        // 缓存命中：耗时是重放时间，token 也不是刚生成的。
        assert_eq!(speed_sample(true, true, 200, 5, Some(0), 100), None);
        // 失败的流没吐完，耗时不代表这个模型的能力。
        assert_eq!(speed_sample(true, false, 502, 3000, Some(1000), 10), None);
        assert_eq!(speed_sample(true, false, 200, 0, Some(0), 10), None, "零耗时");
    }

    #[test]
    fn ttfb_later_than_total_yields_no_window() {
        // 时钟抖动可能让首字节晚于总耗时；负数窗口不能拿去算速度。
        assert_eq!(speed_sample(true, false, 200, 1000, Some(1200), 10), None);
    }

    #[test]
    fn one_output_token_has_speed_but_no_interval() {
        let s = speed_sample(true, false, 200, 1500, Some(500), 1).unwrap();
        assert_eq!(s.itl_ms(), None);
        assert_eq!(s.tps(), Some(1.0));
    }

    #[test]
    fn zero_output_tokens_still_yield_a_ttft() {
        // 一个 token 都没吐（只回了个终止帧之类）：速度无意义，但首字节是量到了的 ——
        // 统计页的平均 TTFT 得把它算进去，所以样本不按输出 token 数筛。
        let s = speed_sample(true, false, 200, 1500, Some(500), 0).unwrap();
        assert_eq!(s.ttft_ms, 500);
        assert_eq!(s.tps(), None);
        assert_eq!(s.itl_ms(), None);
    }

    #[tokio::test]
    async fn detail_reports_speeds_from_stored_scalars() {
        // 详情里的 ITL / TPS 是读的时候推出来的，不在落库时算 —— 加这两个指标
        // 不需要迁移，老日志一样能显示。
        let p = pool().await;
        let mut r = rec("r1", "claude-code", "m");
        r.latency_ms = 3000;
        r.ttfb_ms = Some(1000);
        r.output_tokens = 21;
        insert(&p, &r).await.unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(d.itl_ms, Some(100.0));
        assert_eq!(d.tps, Some(10.5));
    }

    #[tokio::test]
    async fn detail_merges_log_and_capture() {
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "m")).await.unwrap();

        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                method: "POST".into(),
                path: "/v1/messages".into(),
                request_headers: serde_json::json!({"content-type": "application/json"}),
                request_body: Some(br#"{"model":"m"}"#.to_vec()),
                upstream: UpstreamTrace {
                    url: "https://api.example.com/v1/chat/completions".into(),
                    headers: serde_json::json!({"authorization": "<已隐去>"}),
                    body: Some(br#"{"model":"deepseek-chat"}"#.to_vec()),
                    status: Some(404),
                    response_headers: serde_json::json!({"content-type": "application/json"}),
                    response_body: Some(br#"{"error":"model not found"}"#.to_vec()),
                },
                response_headers: serde_json::json!({"x-req-id": "abc"}),
                response_body: Some(br#"{"ok":true}"#.to_vec()),
                stream_text: Some("最终答案".into()),
                stream_events: 42,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(d.model, "m");
        assert_eq!(d.path, "/v1/messages");
        assert_eq!(d.request_body.as_deref(), Some(r#"{"model":"m"}"#));
        assert_eq!(d.stream_text.as_deref(), Some("最终答案"));
        assert_eq!(d.stream_events, 42);
    }

    #[tokio::test]
    async fn stream_specific_capture_fields_roundtrip() {
        // 流式响应没有完整响应体，思考与工具调用只存在于 response_content 里，
        // 原始帧则只在两个 raw 列里。少存任何一个，界面都做不出对应的视图。
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "m")).await.unwrap();

        let content = serde_json::json!({
            "id": "msg_1",
            "model": "m",
            "content": [
                { "type": "thinking", "text": "先看看目录" },
                { "type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"} },
            ],
            "finish_reason": { "type": "tool_use" },
        });

        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                stream_text: Some("".into()),
                stream_events: 17,
                response_content: Some(content.to_string()),
                upstream_stream_raw: Some(b"event: x\ndata: {}\n\n".to_vec()),
                client_stream_raw: Some(b"event: y\ndata: {}\n\n".to_vec()),
                stream_raw_truncated: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(d.response_content.as_deref().unwrap()).unwrap();
        assert_eq!(parsed["content"][0]["type"], "thinking");
        assert_eq!(parsed["content"][1]["input"]["cmd"], "ls");
        assert_eq!(parsed["finish_reason"]["type"], "tool_use");
        assert!(d.upstream_stream_raw.as_deref().unwrap().contains("event: x"));
        assert!(d.client_stream_raw.as_deref().unwrap().contains("event: y"));
        assert!(d.stream_raw_truncated);
    }

    #[tokio::test]
    async fn stream_timings_roundtrip() {
        // 时间轴完全靠这一列画出来。存成 JSON 文本（与 response_content 同一套做法），
        // 前端自己去解。
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "m")).await.unwrap();

        let timings = serde_json::json!({
            "at_ms": [0, 12, 1240],
            "names": ["message_start", "content_block_delta"],
            "name_idx": [0, 1, 1],
            "truncated": false,
        });

        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                stream_timings: Some(timings.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(d.stream_timings.as_deref().unwrap()).unwrap();
        assert_eq!(parsed["at_ms"][2], 1240);
        assert_eq!(parsed["names"][1], "content_block_delta");
        assert_eq!(parsed["name_idx"][2], 1);
        assert_eq!(parsed["truncated"], false);
    }

    #[tokio::test]
    async fn stream_raw_fields_default_to_none_and_false() {
        // 非流式捕获不该被这些列影响；截断标志默认必须是 false，
        // 否则界面会对所有请求都显示"已截断"。
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();
        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(d.response_content, None);
        assert_eq!(d.upstream_stream_raw, None);
        assert_eq!(d.client_stream_raw, None);
        assert!(!d.stream_raw_truncated);
        assert_eq!(d.stream_timings, None, "没有就是不显示时间轴，不是画一根全零的假轴");
    }

    #[tokio::test]
    async fn detail_keeps_both_directions_separate() {
        // 两个方向必须各留一份：跨协议转换时上游收到的体与返回给客户端的体不同，
        // 合成一份就看不出转换做了什么。
        let p = pool().await;
        insert(&p, &rec("r1", "codex", "m")).await.unwrap();

        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                method: "POST".into(),
                path: "/v1/responses".into(),
                request_headers: serde_json::json!({"user-agent": "codex"}),
                request_body: Some(br#"{"model":"m","input":"hi"}"#.to_vec()),
                upstream: UpstreamTrace {
                    url: "https://api.deepseek.com/v1/chat/completions".into(),
                    headers: serde_json::json!({}),
                    body: Some(br#"{"model":"deepseek-chat","messages":[]}"#.to_vec()),
                    status: Some(200),
                    response_headers: serde_json::json!({}),
                    response_body: Some(br#"{"choices":[]}"#.to_vec()),
                },
                response_body: Some(br#"{"output":"hi"}"#.to_vec()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(
            d.upstream_url.as_deref(),
            Some("https://api.deepseek.com/v1/chat/completions")
        );
        assert_eq!(d.upstream_status, Some(200));
        // 入站体是 Responses 形状，出站体是 Chat 形状。
        assert!(d.request_body.as_deref().unwrap().contains("input"));
        assert!(d.upstream_body.as_deref().unwrap().contains("messages"));
        // 上游原文与返回给客户端的那份也不一样。
        assert!(d.upstream_response_body.as_deref().unwrap().contains("choices"));
        assert!(d.response_body.as_deref().unwrap().contains("output"));
    }

    #[tokio::test]
    async fn detail_falls_back_to_log_columns_without_capture() {
        // 没有捕获行时，upstream_url / upstream_status 仍要从日志列里取到，
        // 否则"日志有、捕获被裁掉"的请求会显示成"没请求过上游"。
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(
            d.upstream_url.as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
        assert_eq!(d.upstream_status, Some(200));
        assert_eq!(d.path, "/v1/messages", "路径也要从日志列回退");
        assert_eq!(d.method, "", "方法只存在于捕获里，没有就是空");
    }

    #[tokio::test]
    async fn detail_without_capture_still_returns_log_fields() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(d.model, "m");
        assert!(d.request_body.is_none(), "捕获被裁掉时不该伪造请求体");
        assert_eq!(
            d.request_headers,
            serde_json::json!({}),
            "没有捕获时 headers 应是空对象，而不是 null"
        );
    }

    #[tokio::test]
    async fn detail_for_unknown_request_is_none() {
        let p = pool().await;
        assert!(get_detail(&p, "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prune_captures_keeps_newest_n() {
        let p = pool().await;
        for i in 0..5 {
            save_capture(
                &p,
                &CaptureRecord {
                    request_id: format!("r{i}"),
                    ts: i as i64,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let removed = prune_captures(&p, 2).await.unwrap();
        assert_eq!(removed, 3);

        let left: Vec<String> =
            sqlx::query_scalar("SELECT request_id FROM captures ORDER BY ts DESC")
                .fetch_all(&p)
                .await
                .unwrap();
        assert_eq!(left, vec!["r4", "r3"]);
    }

    #[tokio::test]
    async fn prune_captures_with_zero_keeps_everything() {
        let p = pool().await;
        save_capture(&p, &CaptureRecord { request_id: "r".into(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(prune_captures(&p, 0).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn prune_logs_removes_old_rows() {
        let p = pool().await;
        let mut old = rec("old", "a", "m");
        old.ts = 1_000;
        insert(&p, &old).await.unwrap();
        insert(&p, &rec("new", "a", "m")).await.unwrap();

        let removed = prune_logs(&p, 5_000).await.unwrap();
        assert_eq!(removed, 1);
        assert!(get(&p, "new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn clear_all_empties_logs_and_captures() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();
        insert(&p, &rec("r2", "a", "m")).await.unwrap();
        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (logs, captures) = clear_all(&p).await.unwrap();
        assert_eq!(logs, 2);
        assert_eq!(captures, 1);

        let LogPage { items, total, .. } = query(&p, &LogFilter::default()).await.unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
        assert!(get_detail(&p, "r1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_all_on_empty_database_is_a_noop() {
        let p = pool().await;
        assert_eq!(clear_all(&p).await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn clear_all_leaves_hourly_aggregates_alone() {
        // 计费口径的历史账目不在"清空日志"的范围内 —— 用户点这个按钮是不想让
        // 列表堆着，不是想把自己已花的钱从账单上抹掉。
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();
        sqlx::query(
            "INSERT INTO usage_hourly
                 (bucket_ts, client, provider_tag, model, request_model, requests, quota)
             VALUES (0, 'a', 'p1', 'm', 'm', 1, 300)",
        )
        .execute(&p)
        .await
        .unwrap();

        clear_all(&p).await.unwrap();

        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_hourly")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(left, 1, "清空日志不应影响计费聚合");
    }

    #[tokio::test]
    async fn delete_one_removes_only_that_request() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();
        insert(&p, &rec("r2", "a", "m")).await.unwrap();
        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                ts: now_ms(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r2".into(),
                ts: now_ms(),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let (logs, captures) = delete_one(&p, "r1").await.unwrap();
        assert_eq!(logs, 1);
        assert_eq!(captures, 1);

        // 邻居必须原样留着 —— 这是"单独删除"与"清空"的全部区别。
        assert!(get(&p, "r1").await.unwrap().is_none());
        assert!(get(&p, "r2").await.unwrap().is_some());
        assert!(get_detail(&p, "r1").await.unwrap().is_none());
        assert!(get_detail(&p, "r2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_one_on_missing_request_is_a_noop() {
        // 后台定期裁剪可能已经把它删掉了，这时再点一次不该报错。
        let p = pool().await;
        assert_eq!(delete_one(&p, "nope").await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn delete_one_leaves_hourly_aggregates_alone() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();
        sqlx::query(
            "INSERT INTO usage_hourly
                 (bucket_ts, client, provider_tag, model, request_model, requests, quota)
             VALUES (0, 'a', 'p1', 'm', 'm', 1, 300)",
        )
        .execute(&p)
        .await
        .unwrap();

        delete_one(&p, "r1").await.unwrap();

        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_hourly")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(left, 1, "删一条日志不应影响计费聚合");
    }

    // -----------------------------------------------------------------------
    // 筛选
    // -----------------------------------------------------------------------

    /// 造一条可控的日志：状态码、协议、是否流式都从外面给。
    fn shaped(id: &str, model: &str, status: i32, protocol: &str, stream: bool) -> RequestLogRecord {
        RequestLogRecord {
            status_code: status,
            protocol_in: protocol.into(),
            is_stream: stream,
            ..rec(id, "claude-code", model)
        }
    }

    async fn query_ids(p: &SqlitePool, f: LogFilter) -> Vec<String> {
        let LogPage { items, .. } = query(p, &f).await.unwrap();
        items.into_iter().map(|r| r.request_id).collect()
    }

    #[tokio::test]
    async fn protocol_filter_selects_only_that_protocol() {
        let p = pool().await;
        insert(&p, &shaped("a", "m", 200, "anthropic", false)).await.unwrap();
        insert(&p, &shaped("b", "m", 200, "openai_chat", false)).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            protocol: Some("anthropic".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["a"]);
    }

    #[tokio::test]
    async fn an_unknown_protocol_name_filters_nothing_instead_of_erroring() {
        // 筛选条件来自前端状态，可能是过期的。它该被忽略，而不是让页面报错。
        let p = pool().await;
        insert(&p, &shaped("a", "m", 200, "anthropic", false)).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            protocol: Some("gemini".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["a"], "认不出的协议名等同于不筛");
    }

    #[tokio::test]
    async fn status_filter_splits_success_from_failure() {
        let p = pool().await;
        insert(&p, &shaped("ok", "m", 200, "anthropic", false)).await.unwrap();
        insert(&p, &shaped("bad", "m", 502, "anthropic", false)).await.unwrap();

        let ok = query_ids(&p, LogFilter {
            status: Some(STATUS_OK.into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ok, vec!["ok"]);

        let bad = query_ids(&p, LogFilter {
            status: Some(STATUS_ERROR.into()),
            ..Default::default()
        })
        .await;
        assert_eq!(bad, vec!["bad"]);
    }

    #[tokio::test]
    async fn an_unknown_status_value_filters_nothing() {
        let p = pool().await;
        insert(&p, &shaped("a", "m", 500, "anthropic", false)).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            status: Some("失败的".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["a"]);
    }

    #[tokio::test]
    async fn is_stream_filter_separates_the_two_kinds() {
        let p = pool().await;
        insert(&p, &shaped("s", "m", 200, "anthropic", true)).await.unwrap();
        insert(&p, &shaped("n", "m", 200, "anthropic", false)).await.unwrap();

        let streaming = query_ids(&p, LogFilter {
            is_stream: Some(true),
            ..Default::default()
        })
        .await;
        assert_eq!(streaming, vec!["s"], "只看流式时非流式必须被排除");

        let plain = query_ids(&p, LogFilter {
            is_stream: Some(false),
            ..Default::default()
        })
        .await;
        assert_eq!(plain, vec!["n"]);
    }

    #[tokio::test]
    async fn model_like_matches_a_fragment() {
        let p = pool().await;
        insert(&p, &rec("a", "c", "claude-sonnet-5")).await.unwrap();
        insert(&p, &rec("b", "c", "gpt-5")).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            model_like: Some("sonnet".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["a"]);
    }

    #[tokio::test]
    async fn model_like_does_not_treat_user_input_as_a_wildcard() {
        // 用户输入的 % 应当当作普通字符：把 "gpt-5" 输成 "gpt%" 不该匹配到所有 gpt 模型。
        let p = pool().await;
        insert(&p, &rec("a", "c", "gpt-5")).await.unwrap();
        insert(&p, &rec("b", "c", "gpt%5")).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            model_like: Some("gpt%5".into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["b"], "% 必须被转义成字面量");
    }

    #[tokio::test]
    async fn filters_combine_with_and() {
        let p = pool().await;
        insert(&p, &shaped("hit", "m", 200, "anthropic", true)).await.unwrap();
        insert(&p, &shaped("no-stream", "m", 200, "anthropic", false)).await.unwrap();
        insert(&p, &shaped("other-proto", "m", 200, "openai_chat", true)).await.unwrap();

        let ids = query_ids(&p, LogFilter {
            protocol: Some("anthropic".into()),
            is_stream: Some(true),
            status: Some(STATUS_OK.into()),
            ..Default::default()
        })
        .await;
        assert_eq!(ids, vec!["hit"], "多个条件必须同时生效");
    }

    #[tokio::test]
    async fn an_empty_filter_returns_everything() {
        let p = pool().await;
        insert(&p, &rec("a", "c", "m1")).await.unwrap();
        insert(&p, &rec("b", "c", "m2")).await.unwrap();

        let LogPage { items, total, .. } = query(&p, &LogFilter::default()).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn expr_filter_reads_the_body_via_the_second_pass() {
        // 第一趟不带报文（读到的永远是 null），所以「按 body 筛」必须靠第二趟补上 ——
        // 少了第二趟，这条会一条都不命中。
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "m")).await.unwrap();
        save_capture(
            &p,
            &CaptureRecord {
                request_id: "r1".into(),
                request_body: Some(br#"{"tools":[{"name":"bash"}]}"#.to_vec()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let page = query(
            &p,
            &LogFilter {
                expr: Some(r#"ctx.request.body.tools.length === 1"#.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(page.total, 1);
        assert!(!page.truncated);
    }

    #[tokio::test]
    async fn expr_filter_keeps_metadata_only_results_intact() {
        // 不读报文的表达式在第一趟就定音了：结果该是对的，也不该被标成截断。
        let p = pool().await;
        insert(&p, &rec("ok", "a", "m")).await.unwrap();
        let mut bad = rec("bad", "a", "m");
        bad.status_code = 500;
        insert(&p, &bad).await.unwrap();

        let page = query(
            &p,
            &LogFilter {
                expr: Some("ctx.status >= 400".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].request_id, "bad");
        assert!(!page.truncated);
    }

    /// 造一行：日志 + 捕获，捕获带指定大小的 body。
    async fn row_with_body(p: &SqlitePool, id: &str, body: Vec<u8>) {
        insert(p, &rec(id, "a", "m")).await.unwrap();
        save_capture(
            p,
            &CaptureRecord { request_id: id.into(), request_body: Some(body), ..Default::default() },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn expr_filter_reports_a_truncated_scan() {
        // 第二趟的字节预算用尽时必须如实标 truncated：没重算的行不能拿「读到空
        // 报文」的第一趟结果冒充结论 —— 那样用户会看到一个少了的匹配数，
        // 却以为已经扫完了。
        let p = pool().await;
        for i in 0..6 {
            let id = format!("r{i}");
            let mut body = br#"{"hit":true,"pad":""#.to_vec();
            body.extend(vec![b'x'; 1024]);
            body.extend(br#""}"#);
            row_with_body(&p, &id, body).await;
        }

        let page = query_with_expr_budgeted(
            &p,
            &LogFilter { expr: Some("ctx.request.body.hit === true".into()), limit: 1000, ..Default::default() },
            "ctx.request.body.hit === true",
            3 * 1024,
        )
        .await
        .unwrap();

        assert!(page.truncated, "预算用尽必须报出来");
        assert_eq!(page.total, 2, "只有重算过的两行才算数");
    }

    #[tokio::test]
    async fn expr_filter_widens_its_window_when_payloads_are_small() {
        // 按字节算预算的好处：只读 headers 的表达式每行才几 KB，于是 60 行
        // 全都算得上 —— 换成一个固定行数上限就会白白丢掉一半。
        let p = pool().await;
        for i in 0..60 {
            let id = format!("r{i}");
            row_with_body(&p, &id, br#"{"hit":true}"#.to_vec()).await;
            save_capture(
                &p,
                &CaptureRecord {
                    request_id: id,
                    request_headers: serde_json::json!({ "x-hit": "1" }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }

        let page = query(
            &p,
            &LogFilter {
                expr: Some(r#"ctx.request.headers["x-hit"] === "1""#.into()),
                limit: 1000,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(!page.truncated, "几十行小 headers 远在预算之内");
        assert_eq!(page.total, 60, "每一行都该被算过");
    }

    // -----------------------------------------------------------------------
    // 筛选下拉的数据源
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn facets_lists_distinct_sorted_values() {
        let p = pool().await;
        insert(&p, &shaped("a", "m1", 200, "anthropic", false)).await.unwrap();
        insert(&p, &shaped("b", "m2", 200, "anthropic", false)).await.unwrap();
        insert(&p, &shaped("c", "m1", 200, "openai_chat", false)).await.unwrap();

        let f = facets(&p, 100).await.unwrap();
        assert_eq!(f.models, vec!["m1", "m2"], "必须去重且有序");
        assert_eq!(f.protocols, vec!["anthropic", "openai_chat"]);
        assert_eq!(f.clients, vec!["claude-code"]);
    }

    #[tokio::test]
    async fn facets_on_an_empty_table_is_empty_not_an_error() {
        let p = pool().await;
        let f = facets(&p, 100).await.unwrap();
        assert!(f.models.is_empty());
        assert!(f.clients.is_empty());
    }
}
