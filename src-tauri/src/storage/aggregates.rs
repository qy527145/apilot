//! 按小时聚合的用量统计。
//!
//! 明细写 `request_logs`，同时累加进内存缓冲；定时把缓冲 upsert 进 `usage_hourly`。
//! 这样既保留逐条可查，又让「按客户端/模型/渠道汇总」不必每次扫全表。

use std::collections::HashMap;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlx::{QueryBuilder, Row, SqlitePool};

use super::logs::RequestLogRecord;
use crate::billing::quota::quota_to_usd;
use crate::error::AppResult;
use crate::util::hour_bucket;

/// 聚合主键：小时 × 客户端 × 渠道 × 模型。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AggKey {
    bucket_ts: i64,
    client: String,
    provider_tag: String,
    model: String,
}

/// 一行聚合累加值。
#[derive(Debug, Clone, Default)]
pub struct AggRow {
    pub bucket_ts: i64,
    pub client: String,
    pub provider_tag: String,
    pub model: String,
    pub requests: i64,
    pub failed_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub quota: i64,
    pub cache_hits: i64,
    pub saved_quota: i64,
    pub latency_sum_ms: i64,
}

/// 内存聚合缓冲。
///
/// 用 `parking_lot::Mutex` 而非 tokio 锁：临界区是纯内存加法、不含 `await`，
/// 用同步锁更快且不会阻塞运行时。
#[derive(Default)]
pub struct AggregateBuffer {
    inner: Mutex<HashMap<AggKey, AggRow>>,
}

impl AggregateBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 把一条请求日志累加进缓冲。
    pub fn record(&self, rec: &RequestLogRecord) {
        let key = AggKey {
            bucket_ts: hour_bucket(rec.ts / 1000),
            client: rec.client.clone(),
            // provider_tag 可空，归一成占位符，避免 NULL 在 GROUP BY 里被漏掉。
            provider_tag: rec
                .provider_tag
                .clone()
                .unwrap_or_else(|| "(未指定)".to_string()),
            model: rec.model.clone(),
        };

        let mut guard = self.inner.lock();
        let row = guard.entry(key).or_insert_with(|| AggRow {
            bucket_ts: hour_bucket(rec.ts / 1000),
            client: rec.client.clone(),
            provider_tag: rec
                .provider_tag
                .clone()
                .unwrap_or_else(|| "(未指定)".to_string()),
            model: rec.model.clone(),
            ..Default::default()
        });

        row.requests += 1;
        if rec.status_code >= 400 {
            row.failed_requests += 1;
        }
        row.input_tokens += rec.input_tokens as i64;
        row.output_tokens += rec.output_tokens as i64;
        row.cache_read_tokens += rec.cache_read_tokens as i64;
        row.cache_creation_tokens += rec.cache_creation_tokens as i64;
        row.quota += rec.quota;
        if rec.cache_hit {
            row.cache_hits += 1;
        }
        row.saved_quota += rec.saved_quota;
        row.latency_sum_ms += rec.latency_ms;
    }

    /// 当前缓冲里有几组聚合。
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// 取出并清空缓冲。
    pub fn drain(&self) -> Vec<AggRow> {
        let mut guard = self.inner.lock();
        std::mem::take(&mut *guard).into_values().collect()
    }

    /// 落库。取出的行 upsert 进 `usage_hourly`。
    ///
    /// 用 `ON CONFLICT ... DO UPDATE SET x = x + excluded.x` 做增量累加，
    /// 这样即使同一小时被多次 flush 也不会互相覆盖。
    pub async fn flush(&self, pool: &SqlitePool) -> AppResult<usize> {
        let rows = self.drain();
        if rows.is_empty() {
            return Ok(0);
        }

        let mut tx = pool.begin().await?;
        for r in &rows {
            sqlx::query(
                "INSERT INTO usage_hourly (
                     bucket_ts, client, provider_tag, model, requests, failed_requests,
                     input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                     quota, cache_hits, saved_quota, latency_sum_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(bucket_ts, client, provider_tag, model) DO UPDATE SET
                     requests              = requests + excluded.requests,
                     failed_requests       = failed_requests + excluded.failed_requests,
                     input_tokens          = input_tokens + excluded.input_tokens,
                     output_tokens         = output_tokens + excluded.output_tokens,
                     cache_read_tokens     = cache_read_tokens + excluded.cache_read_tokens,
                     cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                     quota                 = quota + excluded.quota,
                     cache_hits            = cache_hits + excluded.cache_hits,
                     saved_quota           = saved_quota + excluded.saved_quota,
                     latency_sum_ms        = latency_sum_ms + excluded.latency_sum_ms",
            )
            .bind(r.bucket_ts)
            .bind(&r.client)
            .bind(&r.provider_tag)
            .bind(&r.model)
            .bind(r.requests)
            .bind(r.failed_requests)
            .bind(r.input_tokens)
            .bind(r.output_tokens)
            .bind(r.cache_read_tokens)
            .bind(r.cache_creation_tokens)
            .bind(r.quota)
            .bind(r.cache_hits)
            .bind(r.saved_quota)
            .bind(r.latency_sum_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        Ok(rows.len())
    }
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

/// 聚合维度。只允许这三个，避免把列名拼进 SQL 时出现注入面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    Client,
    Model,
    Provider,
}

impl GroupBy {
    fn column(&self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Model => "model",
            Self::Provider => "provider_tag",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "client" => Some(Self::Client),
            "model" => Some(Self::Model),
            "provider" | "provider_tag" => Some(Self::Provider),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BillingBucket {
    pub key: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub quota: i64,
    pub cache_hits: i64,
    pub saved_quota: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BillingSummary {
    pub requests: i64,
    pub quota: i64,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_saved_quota: i64,
    /// 0.0 ~ 1.0。
    pub cache_hit_rate: f64,
    /// TTFB 中位数（毫秒）。样本为空时为 `None`。
    pub p50_ttfb_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HourlyPoint {
    pub bucket_ts: i64,
    pub requests: i64,
    pub quota: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// 把毫秒时间戳转成聚合表的秒级小时桶。
fn range_to_bucket(from_ms: i64, to_ms: i64) -> (i64, i64) {
    (hour_bucket(from_ms / 1000), hour_bucket(to_ms / 1000))
}

/// 按维度聚合。
pub async fn summary_by(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
    group_by: GroupBy,
) -> AppResult<Vec<BillingBucket>> {
    let (from, to) = range_to_bucket(from_ms, to_ms);
    let col = group_by.column();

    // col 来自封闭枚举，不是用户输入，拼接安全。
    let sql = format!(
        "SELECT {col} AS k, SUM(requests) AS requests, SUM(input_tokens) AS input_tokens,
                SUM(output_tokens) AS output_tokens, SUM(cache_read_tokens) AS cache_read_tokens,
                SUM(quota) AS quota, SUM(cache_hits) AS cache_hits, SUM(saved_quota) AS saved_quota
         FROM usage_hourly
         WHERE bucket_ts >= ?1 AND bucket_ts <= ?2
         GROUP BY {col}
         ORDER BY SUM(quota) DESC"
    );

    let rows = sqlx::query(&sql)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| BillingBucket {
            key: r.get::<String, _>("k"),
            requests: r.get("requests"),
            input_tokens: r.get("input_tokens"),
            output_tokens: r.get("output_tokens"),
            cache_read_tokens: r.get("cache_read_tokens"),
            quota: r.get("quota"),
            cache_hits: r.get("cache_hits"),
            saved_quota: r.get("saved_quota"),
        })
        .collect())
}

/// 总体汇总。
pub async fn summary(pool: &SqlitePool, from_ms: i64, to_ms: i64) -> AppResult<BillingSummary> {
    let (from, to) = range_to_bucket(from_ms, to_ms);

    let row = sqlx::query(
        "SELECT COALESCE(SUM(requests),0) AS requests, COALESCE(SUM(quota),0) AS quota,
                COALESCE(SUM(input_tokens),0) AS input_tokens,
                COALESCE(SUM(output_tokens),0) AS output_tokens,
                COALESCE(SUM(cache_read_tokens),0) AS cache_read_tokens,
                COALESCE(SUM(saved_quota),0) AS saved_quota,
                COALESCE(SUM(cache_hits),0) AS cache_hits
         FROM usage_hourly WHERE bucket_ts >= ?1 AND bucket_ts <= ?2",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;

    let requests: i64 = row.get("requests");
    let cache_hits: i64 = row.get("cache_hits");
    let quota: i64 = row.get("quota");

    Ok(BillingSummary {
        requests,
        quota,
        cost_usd: quota_to_usd(quota),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        cache_read_tokens: row.get("cache_read_tokens"),
        cache_saved_quota: row.get("saved_quota"),
        cache_hit_rate: if requests > 0 {
            cache_hits as f64 / requests as f64
        } else {
            0.0
        },
        p50_ttfb_ms: p50_ttfb(pool, from_ms, to_ms).await?,
    })
}

/// 时间序列（按小时）。
pub async fn timeseries(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
) -> AppResult<Vec<HourlyPoint>> {
    let (from, to) = range_to_bucket(from_ms, to_ms);

    let rows = sqlx::query(
        "SELECT bucket_ts, SUM(requests) AS requests, SUM(quota) AS quota,
                SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens
         FROM usage_hourly WHERE bucket_ts >= ?1 AND bucket_ts <= ?2
         GROUP BY bucket_ts ORDER BY bucket_ts ASC",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| HourlyPoint {
            bucket_ts: r.get("bucket_ts"),
            requests: r.get("requests"),
            quota: r.get("quota"),
            input_tokens: r.get("input_tokens"),
            output_tokens: r.get("output_tokens"),
        })
        .collect())
}

/// TTFB 中位数。
///
/// 走 `request_logs` 而不是聚合表：中位数不可由 SUM 推出，而聚合表只存了
/// 延迟总和。范围通常只有几小时，且有 `idx_logs_ts` 支撑，代价可接受。
async fn p50_ttfb(pool: &SqlitePool, from_ms: i64, to_ms: i64) -> AppResult<Option<i64>> {
    let mut qb: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
        "SELECT ttfb_ms FROM request_logs WHERE ttfb_ms IS NOT NULL AND ts >= ",
    );
    qb.push_bind(from_ms)
        .push(" AND ts <= ")
        .push_bind(to_ms)
        .push(" ORDER BY ttfb_ms ASC");

    let values: Vec<i64> = qb.build_query_scalar().fetch_all(pool).await?;
    if values.is_empty() {
        return Ok(None);
    }
    Ok(Some(values[values.len() / 2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::logs;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn rec(client: &str, provider: &str, model: &str, quota: i64, ts_ms: i64) -> RequestLogRecord {
        RequestLogRecord {
            request_id: format!("{client}-{model}-{ts_ms}"),
            ts: ts_ms,
            client: client.into(),
            protocol_in: "anthropic".into(),
            protocol_out: "anthropic".into(),
            provider_tag: Some(provider.into()),
            model: model.into(),
            request_model: model.into(),
            status_code: 200,
            input_tokens: 100,
            output_tokens: 20,
            quota,
            latency_ms: 500,
            ttfb_ms: Some(200),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn buffer_accumulates_by_key() {
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();
        b.record(&rec("c1", "p1", "m1", 100, now));
        b.record(&rec("c1", "p1", "m1", 150, now));
        b.record(&rec("c2", "p1", "m1", 100, now));

        assert_eq!(b.len(), 2, "同 key 应合并，不同 client 分开");

        let rows = b.drain();
        let first = rows.iter().find(|r| r.client == "c1").unwrap();
        assert_eq!(first.requests, 2);
        assert_eq!(first.quota, 250);
        assert_eq!(first.input_tokens, 200);
        assert!(b.is_empty(), "drain 后应清空");
    }

    #[tokio::test]
    async fn buffer_buckets_by_hour() {
        let b = AggregateBuffer::new();
        let base = 1_700_000_000_000i64; // 固定时刻，避免测试跨小时抖动
        b.record(&rec("c", "p", "m", 10, base));
        b.record(&rec("c", "p", "m", 10, base + 3_600_000));

        assert_eq!(b.len(), 2, "跨小时应分成两个桶");
    }

    #[tokio::test]
    async fn buffer_tracks_failures_and_cache_hits() {
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();

        let mut failed = rec("c", "p", "m", 0, now);
        failed.status_code = 500;
        b.record(&failed);

        let mut cached = rec("c", "p", "m", 0, now);
        cached.cache_hit = true;
        cached.saved_quota = 500;
        b.record(&cached);

        let row = b.drain().pop().unwrap();
        assert_eq!(row.requests, 2);
        assert_eq!(row.failed_requests, 1);
        assert_eq!(row.cache_hits, 1);
        assert_eq!(row.saved_quota, 500);
    }

    #[tokio::test]
    async fn flush_writes_and_accumulates_across_calls() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();

        b.record(&rec("c", "p", "m", 100, now));
        assert_eq!(b.flush(&p).await.unwrap(), 1);

        // 第二次 flush 同一小时应累加而不是覆盖
        b.record(&rec("c", "p", "m", 200, now));
        assert_eq!(b.flush(&p).await.unwrap(), 1);

        let total: i64 = sqlx::query_scalar("SELECT SUM(quota) FROM usage_hourly")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(total, 300, "同一小时多次 flush 必须累加");
    }

    #[tokio::test]
    async fn flush_with_empty_buffer_is_a_noop() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        assert_eq!(b.flush(&p).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn summary_by_client() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();

        b.record(&rec("claude-code", "p1", "m1", 100, now));
        b.record(&rec("claude-code", "p1", "m1", 100, now));
        b.record(&rec("codex", "p1", "m2", 50, now));
        b.flush(&p).await.unwrap();

        let buckets = summary_by(&p, now - 3_600_000, now + 3_600_000, GroupBy::Client)
            .await
            .unwrap();
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].key, "claude-code", "按额度降序");
        assert_eq!(buckets[0].requests, 2);
        assert_eq!(buckets[0].quota, 200);
    }

    #[tokio::test]
    async fn summary_by_model_and_provider() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();
        b.record(&rec("c", "prov-a", "model-x", 10, now));
        b.record(&rec("c", "prov-b", "model-y", 20, now));
        b.flush(&p).await.unwrap();

        let by_model = summary_by(&p, now - 3_600_000, now + 3_600_000, GroupBy::Model)
            .await
            .unwrap();
        assert_eq!(by_model.len(), 2);

        let by_prov = summary_by(&p, now - 3_600_000, now + 3_600_000, GroupBy::Provider)
            .await
            .unwrap();
        assert_eq!(by_prov.len(), 2);
        assert_eq!(by_prov[0].key, "prov-b");
    }

    #[tokio::test]
    async fn group_by_parse_rejects_unknown_column() {
        assert_eq!(GroupBy::parse("client"), Some(GroupBy::Client));
        assert_eq!(GroupBy::parse("provider_tag"), Some(GroupBy::Provider));
        assert_eq!(GroupBy::parse("password"), None, "未知列名必须被挡住");
    }

    #[tokio::test]
    async fn summary_computes_totals_and_hit_rate() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        let now = crate::util::now_ms();

        b.record(&rec("c", "p", "m", 100, now));

        let mut hit = rec("c", "p", "m", 0, now);
        hit.cache_hit = true;
        hit.saved_quota = 400;
        b.record(&hit);
        b.flush(&p).await.unwrap();

        let s = summary(&p, now - 3_600_000, now + 3_600_000).await.unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.quota, 100);
        assert_eq!(s.cache_saved_quota, 400);
        assert!((s.cache_hit_rate - 0.5).abs() < 1e-9);
        assert!((s.cost_usd - quota_to_usd(100)).abs() < 1e-12);
    }

    #[tokio::test]
    async fn summary_is_all_zero_when_empty() {
        let p = pool().await;
        let now = crate::util::now_ms();
        let s = summary(&p, now - 3_600_000, now + 3_600_000).await.unwrap();
        assert_eq!(s.requests, 0);
        assert_eq!(s.quota, 0);
        assert_eq!(s.cache_hit_rate, 0.0, "无数据时不应除零");
        assert_eq!(s.p50_ttfb_ms, None);
    }

    #[tokio::test]
    async fn timeseries_returns_sorted_buckets() {
        let p = pool().await;
        let b = AggregateBuffer::new();
        let base = 1_700_000_000_000i64;
        b.record(&rec("c", "p", "m", 10, base));
        b.record(&rec("c", "p", "m", 20, base + 3_600_000));
        b.flush(&p).await.unwrap();

        let pts = timeseries(&p, base - 3_600_000, base + 7_200_000).await.unwrap();
        assert_eq!(pts.len(), 2);
        assert!(pts[0].bucket_ts < pts[1].bucket_ts);
        assert_eq!(pts[0].requests, 1);
    }

    #[tokio::test]
    async fn p50_ttfb_reads_detail_table() {
        let p = pool().await;
        let now = crate::util::now_ms();

        for (i, ttfb) in [100i64, 200, 300, 400, 500].iter().enumerate() {
            let mut r = rec("c", "p", "m", 1, now);
            r.request_id = format!("r{i}");
            r.ttfb_ms = Some(*ttfb);
            logs::insert(&p, &r).await.unwrap();
        }

        let p50 = p50_ttfb(&p, now - 1000, now + 1000).await.unwrap();
        assert_eq!(p50, Some(300), "5 个样本的中位数是第 3 个");
    }

    #[tokio::test]
    async fn p50_ttfb_ignores_null_and_empty() {
        let p = pool().await;
        let now = crate::util::now_ms();
        assert_eq!(p50_ttfb(&p, now - 1000, now + 1000).await.unwrap(), None);

        let mut r = rec("c", "p", "m", 1, now);
        r.ttfb_ms = None;
        logs::insert(&p, &r).await.unwrap();
        assert_eq!(p50_ttfb(&p, now - 1000, now + 1000).await.unwrap(), None);
    }

}
