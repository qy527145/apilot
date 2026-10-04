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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LogFilter {
    pub client: Option<String>,
    pub model: Option<String>,
    pub provider_tag: Option<String>,
    /// unix 毫秒，闭区间。
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub only_cache_hit: Option<bool>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    50
}

impl LogFilter {
    /// 夹住分页参数，避免一条查询把整个表拉出来。
    pub fn normalized(mut self) -> Self {
        self.limit = self.limit.clamp(1, 1000);
        self.offset = self.offset.max(0);
        self
    }
}

const LOG_COLUMNS: &str = "request_id, ts, client, protocol_in, protocol_out, provider_tag, \
     model, request_model, is_stream, status_code, error_message, input_tokens, output_tokens, \
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
             model, request_model, is_stream, status_code, error_message,
             input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
             reasoning_tokens, usage_source, quota, cost_usd, latency_ms, ttfb_ms,
             cache_hit, saved_quota, other)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
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

    pub method: String,
    pub path: String,
    pub request_headers: serde_json::Value,
    pub request_body: Option<String>,
    pub response_headers: serde_json::Value,
    pub response_body: Option<String>,
    pub stream_text: Option<String>,
    pub stream_events: i64,
}

/// 保存一次请求 / 响应的原文。
#[derive(Debug, Clone, Default)]
pub struct CaptureRecord {
    pub request_id: String,
    pub ts: i64,
    pub method: String,
    pub path: String,
    pub request_headers: serde_json::Value,
    pub request_body: Option<Vec<u8>>,
    pub response_headers: serde_json::Value,
    pub response_body: Option<Vec<u8>>,
    pub stream_text: Option<String>,
    pub stream_events: i64,
}

pub async fn save_capture(pool: &SqlitePool, c: &CaptureRecord) -> AppResult<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO captures (
             request_id, ts, method, path, request_headers, request_body,
             response_headers, response_body, stream_text, stream_events)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
    )
    .bind(&c.request_id)
    .bind(c.ts)
    .bind(&c.method)
    .bind(&c.path)
    .bind(serde_json::to_string(&c.request_headers)?)
    .bind(&c.request_body)
    .bind(serde_json::to_string(&c.response_headers)?)
    .bind(&c.response_body)
    .bind(&c.stream_text)
    .bind(c.stream_events)
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
        "SELECT method, path, request_headers, request_body, response_headers,
                response_body, stream_text, stream_events
         FROM captures WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await?;

    let (
        method,
        path,
        request_headers,
        request_body,
        response_headers,
        response_body,
        stream_text,
        stream_events,
    ) = match &cap {
        Some(r) => (
            r.get::<Option<String>, _>("method").unwrap_or_default(),
            r.get::<Option<String>, _>("path").unwrap_or_default(),
            parse_json(r.get::<Option<String>, _>("request_headers")),
            r.get::<Option<Vec<u8>>, _>("request_body"),
            parse_json(r.get::<Option<String>, _>("response_headers")),
            r.get::<Option<Vec<u8>>, _>("response_body"),
            r.get::<Option<String>, _>("stream_text"),
            r.get::<Option<i64>, _>("stream_events").unwrap_or(0),
        ),
        None => (
            String::new(),
            String::new(),
            serde_json::json!({}),
            None,
            serde_json::json!({}),
            None,
            None,
            0,
        ),
    };

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
        path,
        request_headers,
        // 捕获体按 UTF-8 展示；非法字节用替换字符，不因一个坏字节丢掉整条详情。
        request_body: request_body.map(|b| String::from_utf8_lossy(&b).to_string()),
        response_headers,
        response_body: response_body.map(|b| String::from_utf8_lossy(&b).to_string()),
        stream_text,
        stream_events,
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
                response_headers: serde_json::json!({"x-req-id": "abc"}),
                response_body: Some(br#"{"ok":true}"#.to_vec()),
                stream_text: Some("最终答案".into()),
                stream_events: 42,
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
    async fn detail_without_capture_still_returns_log_fields() {
        let p = pool().await;
        insert(&p, &rec("r1", "a", "m")).await.unwrap();

        let d = get_detail(&p, "r1").await.unwrap().unwrap();
        assert_eq!(d.model, "m");
        assert_eq!(d.path, "", "无捕获时字段为空而不是报错");
        assert!(d.request_body.is_none());
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
}
