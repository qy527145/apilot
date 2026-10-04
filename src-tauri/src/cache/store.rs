//! 响应缓存的存储与统计。
//!
//! 缓存条目落在 `response_cache` 表（`body` 存归一后的 `UnifiedResponse` JSON），
//! 命中计数用进程内原子量、并定期写回 `settings_kv`，避免每次命中都写库。

use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::policy::{CachePolicy, CacheScope};
use crate::error::AppResult;
use crate::protocol::dto::{Protocol, UnifiedResponse, UnifiedUsage};
use crate::util::now_ms;

const STATS_KEY: &str = "cache_counters";

/// 一条缓存条目。
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub key: String,
    pub protocol: Protocol,
    pub model: String,
    pub provider_tag: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
    pub status_code: i32,
    /// 归一后的 `UnifiedResponse` JSON。
    pub body: Vec<u8>,
    pub usage: UnifiedUsage,
    /// 首次生成时花的额度 —— 命中一次就省这么多。
    pub quota: i64,
    pub hits: i64,
    pub last_hit_at: Option<i64>,
    pub size_bytes: i64,
}

impl CacheEntry {
    /// 从响应构造条目，自动算过期时间与体积。
    pub fn from_response(
        key: String,
        protocol: Protocol,
        model: String,
        provider_tag: Option<String>,
        resp: &UnifiedResponse,
        usage: &UnifiedUsage,
        quota: i64,
        ttl_secs: u64,
    ) -> AppResult<Self> {
        let body = serde_json::to_vec(resp)?;
        let now = now_ms();
        Ok(Self {
            key,
            protocol,
            model,
            provider_tag,
            created_at: now,
            expires_at: now + (ttl_secs as i64) * 1000,
            status_code: 200,
            size_bytes: body.len() as i64,
            body,
            usage: usage.clone(),
            quota,
            hits: 0,
            last_hit_at: None,
        })
    }

    /// 还原出缓存的响应对象。
    pub fn response(&self) -> AppResult<UnifiedResponse> {
        Ok(serde_json::from_slice(&self.body)?)
    }

    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    pub entries: i64,
    pub hits: i64,
    pub misses: i64,
    /// 0.0 ~ 1.0。
    pub hit_rate: f64,
    /// 命中累计省下的额度。
    pub saved_quota: i64,
    pub total_bytes: i64,
}

/// 累积计数器，随缓存一起持久化。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Counters {
    hits: u64,
    misses: u64,
    saved_quota: i64,
}

pub struct ResponseCache {
    policy: ArcSwap<CachePolicy>,
    hits: AtomicU64,
    misses: AtomicU64,
    saved_quota: AtomicU64,
}

impl ResponseCache {
    pub fn new(policy: CachePolicy) -> Self {
        Self {
            policy: ArcSwap::from_pointee(policy.normalized()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            saved_quota: AtomicU64::new(0),
        }
    }

    pub fn policy(&self) -> CachePolicy {
        (**self.policy.load()).clone()
    }

    pub fn set_policy(&self, policy: CachePolicy) {
        self.policy.store(std::sync::Arc::new(policy.normalized()));
    }

    /// 从 `settings_kv` 恢复计数器，让命中率跨重启连续。
    pub async fn restore_counters(&self, pool: &SqlitePool) -> AppResult<()> {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings_kv WHERE key = ?1")
                .bind(STATS_KEY)
                .fetch_optional(pool)
                .await?;

        if let Some(c) = raw.and_then(|s| serde_json::from_str::<Counters>(&s).ok()) {
            self.hits.store(c.hits, Ordering::Relaxed);
            self.misses.store(c.misses, Ordering::Relaxed);
            self.saved_quota
                .store(c.saved_quota.max(0) as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    /// 把计数器写回 `settings_kv`。
    pub async fn persist_counters(&self, pool: &SqlitePool) -> AppResult<()> {
        let c = Counters {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            saved_quota: self.saved_quota.load(Ordering::Relaxed) as i64,
        };
        sqlx::query(
            "INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(STATS_KEY)
        .bind(serde_json::to_string(&c)?)
        .bind(now_ms())
        .execute(pool)
        .await?;
        Ok(())
    }

    /// 查询缓存。命中时累加计数并刷新 LRU 时间戳。
    pub async fn get(&self, pool: &SqlitePool, key: &str) -> AppResult<Option<CacheEntry>> {
        let now = now_ms();
        let row = sqlx::query(
            "SELECT key, protocol, model, provider_tag, created_at, expires_at, status_code,
                    body, usage, quota, hits, last_hit_at, size_bytes
             FROM response_cache WHERE key = ?1",
        )
        .bind(key)
        .fetch_optional(pool)
        .await?;

        let Some(r) = row else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };

        let entry = row_to_entry(&r);
        if entry.is_expired(now) {
            // 过期条目当作未命中，并顺手删掉，避免它继续占用 LRU 名额。
            let _ = sqlx::query("DELETE FROM response_cache WHERE key = ?1")
                .bind(key)
                .execute(pool)
                .await;
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }

        self.hits.fetch_add(1, Ordering::Relaxed);
        self.saved_quota
            .fetch_add(entry.quota.max(0) as u64, Ordering::Relaxed);

        // 记录命中时间：LRU 淘汰依赖它。
        let _ = sqlx::query("UPDATE response_cache SET hits = hits + 1, last_hit_at = ?1 WHERE key = ?2")
            .bind(now)
            .bind(key)
            .execute(pool)
            .await;

        Ok(Some(entry))
    }

    /// 写入缓存条目。超容量时按 LRU 淘汰。
    pub async fn put(&self, pool: &SqlitePool, entry: &CacheEntry) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO response_cache (key, protocol, model, provider_tag, created_at, expires_at,
                 status_code, body, usage, quota, hits, last_hit_at, size_bytes)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(key) DO UPDATE SET
                 expires_at = excluded.expires_at,
                 body = excluded.body,
                 usage = excluded.usage,
                 quota = excluded.quota,
                 size_bytes = excluded.size_bytes",
        )
        .bind(&entry.key)
        .bind(entry.protocol.as_str())
        .bind(&entry.model)
        .bind(&entry.provider_tag)
        .bind(entry.created_at)
        .bind(entry.expires_at)
        .bind(entry.status_code)
        .bind(&entry.body)
        .bind(serde_json::to_string(&entry.usage)?)
        .bind(entry.quota)
        .bind(entry.hits)
        .bind(entry.last_hit_at)
        .bind(entry.size_bytes)
        .execute(pool)
        .await?;

        let max = self.policy().max_entries as i64;
        evict_lru(pool, max).await?;
        Ok(())
    }

    pub async fn stats(&self, pool: &SqlitePool) -> AppResult<CacheStats> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS entries, COALESCE(SUM(size_bytes),0) AS total_bytes
             FROM response_cache",
        )
        .fetch_one(pool)
        .await?;

        let hits = self.hits.load(Ordering::Relaxed) as i64;
        let misses = self.misses.load(Ordering::Relaxed) as i64;
        let lookups = hits + misses;

        Ok(CacheStats {
            entries: row.get("entries"),
            hits,
            misses,
            hit_rate: if lookups > 0 {
                hits as f64 / lookups as f64
            } else {
                0.0
            },
            saved_quota: self.saved_quota.load(Ordering::Relaxed) as i64,
            total_bytes: row.get("total_bytes"),
        })
    }

    /// 按范围清空，返回删除条数。
    pub async fn clear(&self, pool: &SqlitePool, scope: &CacheScope) -> AppResult<u64> {
        let now = now_ms();
        let r = match scope {
            CacheScope::All => sqlx::query("DELETE FROM response_cache").execute(pool).await?,
            CacheScope::Expired => sqlx::query("DELETE FROM response_cache WHERE expires_at <= ?1")
                .bind(now)
                .execute(pool)
                .await?,
            CacheScope::Model { model } => {
                sqlx::query("DELETE FROM response_cache WHERE model = ?1")
                    .bind(model)
                    .execute(pool)
                    .await?
            }
        };
        Ok(r.rows_affected())
    }

    /// 清掉所有已过期条目。
    pub async fn purge_expired(&self, pool: &SqlitePool) -> AppResult<u64> {
        let r = sqlx::query("DELETE FROM response_cache WHERE expires_at <= ?1")
            .bind(now_ms())
            .execute(pool)
            .await?;
        Ok(r.rows_affected())
    }
}

fn row_to_entry(r: &sqlx::sqlite::SqliteRow) -> CacheEntry {
    let protocol = match r.get::<String, _>("protocol").as_str() {
        "anthropic" => Protocol::AnthropicMessages,
        "openai_responses" => Protocol::OpenAiResponses,
        _ => Protocol::OpenAiChat,
    };
    let usage_json: String = r.get("usage");

    CacheEntry {
        key: r.get("key"),
        protocol,
        model: r.get("model"),
        provider_tag: r.get("provider_tag"),
        created_at: r.get("created_at"),
        expires_at: r.get("expires_at"),
        status_code: r.get("status_code"),
        body: r.get("body"),
        usage: serde_json::from_str(&usage_json).unwrap_or_default(),
        quota: r.get("quota"),
        hits: r.get("hits"),
        last_hit_at: r.get("last_hit_at"),
        size_bytes: r.get("size_bytes"),
    }
}

/// 按 LRU（最久未命中）淘汰到 `max_entries` 以内。
///
/// 两级排序：
/// 1. **被命中过的条目优先保留** —— 有实际价值的证据就是"它被用过"；
/// 2. 同组内按最后使用时间倒序，越近的越留。
///
/// 因此从未命中的条目会先被淘汰，哪怕它比某些命中过的条目新。
async fn evict_lru(pool: &SqlitePool, max_entries: i64) -> AppResult<u64> {
    if max_entries <= 0 {
        return Ok(0);
    }
    let r = sqlx::query(
        "DELETE FROM response_cache WHERE key NOT IN (
             SELECT key FROM response_cache
             ORDER BY (last_hit_at IS NULL) ASC,
                      COALESCE(last_hit_at, created_at) DESC
             LIMIT ?1
         )",
    )
    .bind(max_entries)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{ContentBlock, FinishReason, UnifiedUsage};

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn response(text: &str) -> UnifiedResponse {
        UnifiedResponse {
            id: "resp_1".into(),
            model: "claude-sonnet-5".into(),
            content: vec![ContentBlock::text(text)],
            finish_reason: FinishReason::Stop,
        }
    }

    fn entry(key: &str, quota: i64, ttl_secs: u64) -> CacheEntry {
        CacheEntry::from_response(
            key.into(),
            Protocol::AnthropicMessages,
            "claude-sonnet-5".into(),
            Some("p1".into()),
            &response("hello"),
            &UnifiedUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            quota,
            ttl_secs,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn put_then_get_roundtrips_the_response() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 100, 3600)).await.unwrap();

        let got = c.get(&p, "k1").await.unwrap().unwrap();
        assert_eq!(got.model, "claude-sonnet-5");
        assert_eq!(got.quota, 100);
        let resp = got.response().unwrap();
        assert_eq!(resp.concat_text(), "hello");
        assert_eq!(got.usage.input_tokens, 10);
    }

    #[tokio::test]
    async fn miss_counts_and_returns_none() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        assert!(c.get(&p, "absent").await.unwrap().is_none());

        let s = c.stats(&p).await.unwrap();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 0);
        assert_eq!(s.hit_rate, 0.0);
    }

    #[tokio::test]
    async fn expired_entry_is_a_miss_and_is_removed() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());

        // TTL 1 秒，然后手动把过期时间改到过去
        c.put(&p, &entry("k1", 100, 1)).await.unwrap();
        sqlx::query("UPDATE response_cache SET expires_at = 1 WHERE key = 'k1'")
            .execute(&p)
            .await
            .unwrap();

        assert!(c.get(&p, "k1").await.unwrap().is_none());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM response_cache")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 0, "过期条目应被顺手删掉");
    }

    #[tokio::test]
    async fn hit_rate_tracks_hits_over_total_lookups() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 100, 3600)).await.unwrap();

        c.get(&p, "k1").await.unwrap(); // hit
        c.get(&p, "k1").await.unwrap(); // hit
        c.get(&p, "nope").await.unwrap(); // miss

        let s = c.stats(&p).await.unwrap();
        assert_eq!(s.hits, 2);
        assert_eq!(s.misses, 1);
        assert!((s.hit_rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn saved_quota_accumulates_per_hit() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 250, 3600)).await.unwrap();

        c.get(&p, "k1").await.unwrap();
        c.get(&p, "k1").await.unwrap();

        assert_eq!(c.stats(&p).await.unwrap().saved_quota, 500);
    }

    #[tokio::test]
    async fn hit_updates_lru_timestamp_and_counter() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 1, 3600)).await.unwrap();
        c.get(&p, "k1").await.unwrap();

        let hits: i64 = sqlx::query_scalar("SELECT hits FROM response_cache WHERE key='k1'")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(hits, 1);

        let last: Option<i64> =
            sqlx::query_scalar("SELECT last_hit_at FROM response_cache WHERE key='k1'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(last.is_some(), "命中必须刷新 LRU 时间戳");
    }

    #[tokio::test]
    async fn clear_all_removes_everything() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 1, 3600)).await.unwrap();
        c.put(&p, &entry("k2", 1, 3600)).await.unwrap();

        assert_eq!(c.clear(&p, &CacheScope::All).await.unwrap(), 2);
        assert_eq!(c.stats(&p).await.unwrap().entries, 0);
    }

    #[tokio::test]
    async fn clear_by_model_is_selective() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());

        let mut other = entry("k2", 1, 3600);
        other.model = "gpt-5".into();
        c.put(&p, &entry("k1", 1, 3600)).await.unwrap();
        c.put(&p, &other).await.unwrap();

        let removed = c
            .clear(
                &p,
                &CacheScope::Model {
                    model: "gpt-5".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(c.stats(&p).await.unwrap().entries == 1);
    }

    #[tokio::test]
    async fn evicts_least_recently_used_when_over_capacity() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy {
            enabled: true,
            ttl_secs: 3600,
            max_entries: 2,
        });

        for k in ["a", "b", "c"] {
            c.put(&p, &entry(k, 1, 3600)).await.unwrap();
        }

        let entries: Vec<String> =
            sqlx::query_scalar("SELECT key FROM response_cache ORDER BY COALESCE(last_hit_at, created_at) DESC")
                .fetch_all(&p)
                .await
                .unwrap();
        assert_eq!(entries.len(), 2, "应淘汰到容量上限");
        assert!(!entries.contains(&"a".to_string()), "最旧的应被淘汰");
    }

    #[tokio::test]
    async fn never_hit_entries_are_evicted_first() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy {
            enabled: true,
            ttl_secs: 3600,
            max_entries: 2,
        });

        // a 被命中过，b/c 从未命中
        c.put(&p, &entry("a", 1, 3600)).await.unwrap();
        c.get(&p, "a").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        c.put(&p, &entry("b", 1, 3600)).await.unwrap();
        c.put(&p, &entry("c", 1, 3600)).await.unwrap();

        let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM response_cache")
            .fetch_all(&p)
            .await
            .unwrap();
        assert!(keys.contains(&"a".to_string()), "命中过的应被保留");
    }

    #[tokio::test]
    async fn purge_expired_only_removes_expired() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("live", 1, 3600)).await.unwrap();
        c.put(&p, &entry("dead", 1, 3600)).await.unwrap();
        sqlx::query("UPDATE response_cache SET expires_at = 1 WHERE key = 'dead'")
            .execute(&p)
            .await
            .unwrap();

        assert_eq!(c.purge_expired(&p).await.unwrap(), 1);
        assert_eq!(c.stats(&p).await.unwrap().entries, 1);
    }

    #[tokio::test]
    async fn counters_survive_persist_and_restore() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 300, 3600)).await.unwrap();
        c.get(&p, "k1").await.unwrap();
        c.get(&p, "miss").await.unwrap();
        c.persist_counters(&p).await.unwrap();

        let restored = ResponseCache::new(CachePolicy::default());
        restored.restore_counters(&p).await.unwrap();

        let s = restored.stats(&p).await.unwrap();
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 1);
        assert_eq!(s.saved_quota, 300);
    }

    #[tokio::test]
    async fn restore_without_saved_counters_is_safe() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.restore_counters(&p).await.unwrap();
        assert_eq!(c.stats(&p).await.unwrap().hits, 0);
    }

    #[tokio::test]
    async fn policy_update_takes_effect() {
        let c = ResponseCache::new(CachePolicy::default());
        assert!(!c.policy().enabled);

        c.set_policy(CachePolicy {
            enabled: true,
            ttl_secs: 7200,
            max_entries: 5,
        });
        assert!(c.policy().enabled);
        assert_eq!(c.policy().ttl_secs, 7200);
    }

    #[tokio::test]
    async fn put_is_idempotent_on_same_key() {
        let p = pool().await;
        let c = ResponseCache::new(CachePolicy::default());
        c.put(&p, &entry("k1", 100, 3600)).await.unwrap();

        let mut updated = entry("k1", 999, 3600);
        updated.body = serde_json::to_vec(&response("updated")).unwrap();
        updated.size_bytes = updated.body.len() as i64;
        c.put(&p, &updated).await.unwrap();

        assert_eq!(c.stats(&p).await.unwrap().entries, 1);
        let got = c.get(&p, "k1").await.unwrap().unwrap();
        assert_eq!(got.quota, 999);
        assert_eq!(got.response().unwrap().concat_text(), "updated");
    }

    #[test]
    fn stats_default_is_all_zero() {
        let s = CacheStats::default();
        assert_eq!(s.entries, 0);
        assert_eq!(s.hit_rate, 0.0);
    }
}
