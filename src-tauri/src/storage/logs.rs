//! 请求明细日志与流量捕获的读写。

use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, SqlitePool};

use crate::error::AppResult;

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
    pub provider_tag: Option<String>,
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
    /// 模型名模糊匹配。`model` 是精确匹配，这个是给"记不全名字"用的。
    pub model_like: Option<String>,
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
            model_like: None,
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

        self
    }
}

const LOG_COLUMNS: &str = "request_id, ts, client, protocol_in, protocol_out, provider_tag, \
     model, request_model, path, upstream_url, upstream_model, upstream_status, is_stream, \
     status_code, error_message, input_tokens, output_tokens, \
     cache_read_tokens, cache_creation_tokens, usage_source, quota, cost_usd, latency_ms, \
     ttfb_ms, cache_hit, saved_quota";

fn row_to_log(r: &sqlx::sqlite::SqliteRow) -> RequestLog {
    RequestLog {
        request_id: r.get("request_id"),
        ts: r.get("ts"),
        client: r.get("client"),
        protocol_in: r.get("protocol_in"),
        protocol_out: r.get("protocol_out"),
        provider_tag: r.get("provider_tag"),
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
             request_id, ts, client, protocol_in, protocol_out, provider_tag, channel_kind,
             model, request_model, path, upstream_url, upstream_model, upstream_status,
             is_stream, status_code, error_message,
             input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
             reasoning_tokens, usage_source, quota, cost_usd, latency_ms, ttfb_ms,
             cache_hit, saved_quota, other)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                 ?22,?23,?24,?25,?26,?27,?28,?29)",
    )
    .bind(&rec.request_id)
    .bind(rec.ts)
    .bind(&rec.client)
    .bind(&rec.protocol_in)
    .bind(&rec.protocol_out)
    .bind(&rec.provider_tag)
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

/// 按条件分页查询。返回 `(本页数据, 满足条件的总数)`。
pub async fn query(pool: &SqlitePool, filter: &LogFilter) -> AppResult<(Vec<RequestLog>, i64)> {
    let filter = filter.clone().normalized();

    // 计数与取数用同一套 WHERE，避免两者口径漂移。
    let mut count_qb: QueryBuilder<sqlx::Sqlite> =
        QueryBuilder::new("SELECT COUNT(*) FROM request_logs WHERE 1=1");
    push_where(&mut count_qb, &filter);
    let total: i64 = count_qb.build_query_scalar().fetch_one(pool).await?;

    let mut qb: QueryBuilder<sqlx::Sqlite> =
        QueryBuilder::new(format!("SELECT {LOG_COLUMNS} FROM request_logs WHERE 1=1"));
    push_where(&mut qb, &filter);
    qb.push(" ORDER BY ts DESC, id DESC LIMIT ")
        .push_bind(filter.limit)
        .push(" OFFSET ")
        .push_bind(filter.offset);

    let rows = qb.build().fetch_all(pool).await?;
    Ok((rows.iter().map(row_to_log).collect(), total))
}

/// 筛选下拉的数据源：最近这些请求里实际出现过的客户端 / 模型 / 协议。
#[derive(Debug, Clone, Default, Serialize)]
pub struct LogFacets {
    pub clients: Vec<String>,
    pub models: Vec<String>,
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
        "SELECT client, model, protocol_in FROM request_logs ORDER BY ts DESC LIMIT ?1",
    )
    .bind(scan)
    .fetch_all(pool)
    .await?;

    let mut clients = std::collections::BTreeSet::new();
    let mut models = std::collections::BTreeSet::new();
    let mut protocols = std::collections::BTreeSet::new();

    for r in &rows {
        let c: String = r.get("client");
        let m: String = r.get("model");
        let p: String = r.get("protocol_in");
        if !c.is_empty() {
            clients.insert(c);
        }
        if !m.is_empty() {
            models.insert(m);
        }
        if !p.is_empty() {
            protocols.insert(p);
        }
    }

    Ok(LogFacets {
        clients: clients.into_iter().collect(),
        models: models.into_iter().collect(),
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
    if let Some(m) = &f.model_like {
        // 转义 LIKE 的通配符：模型名里出现 % 或 _ 时不该被当成通配符。
        let escaped = m.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        qb.push(" AND model LIKE ")
            .push_bind(format!("%{escaped}%"))
            .push(" ESCAPE '\\'");
    }
}

pub async fn get(pool: &SqlitePool, request_id: &str) -> AppResult<Option<RequestLog>> {
    let row = sqlx::query(&format!(
        "SELECT {LOG_COLUMNS} FROM request_logs WHERE request_id = ?1"
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

    Ok(Some(RequestDetail {
        request_id: log.request_id,
        ts: log.ts,
        client: log.client,
        protocol_in: log.protocol_in,
        protocol_out: log.protocol_out,
        provider_tag: log.provider_tag,
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

        let (items, total) = query(&p, &LogFilter::default()).await.unwrap();
        assert_eq!(total, 1, "同一 request_id 不应产生两行");
        assert_eq!(items[0].status_code, 500);
    }

    #[tokio::test]
    async fn query_filters_by_client_and_model() {
        let p = pool().await;
        insert(&p, &rec("r1", "claude-code", "claude-sonnet-5")).await.unwrap();
        insert(&p, &rec("r2", "codex", "gpt-5")).await.unwrap();

        let (items, total) = query(
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

        let (items, _) = query(
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

        let (_, total) = query(
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
    async fn query_filters_cache_hits() {
        let p = pool().await;
        insert(&p, &rec("miss", "a", "m")).await.unwrap();

        let mut hit = rec("hit", "a", "m");
        hit.cache_hit = true;
        hit.saved_quota = 300;
        insert(&p, &hit).await.unwrap();

        let (items, total) = query(
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

        let (page, total) = query(
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
        let (items, _) = query(&p, &LogFilter::default()).await.unwrap();
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

        let (items, total) = query(&p, &LogFilter::default()).await.unwrap();
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
        let (items, _) = query(p, &f).await.unwrap();
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

        let (items, total) = query(&p, &LogFilter::default()).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(total, 2);
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
