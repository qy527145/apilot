//! 每个模型的渠道选择策略。
//!
//! 表里**没有行**就等于"这个模型交给 selector 与路由规则管" —— 这条界线是
//! 旧行为得以保持的关键：不在模型页碰过的模型，路由结果与改动前逐条一致。

use sqlx::{Row, SqlitePool};

use super::models::{ModelPolicyRecord, ModelStrategy};
use crate::error::AppResult;
use crate::util::now_ms;

pub async fn get(pool: &SqlitePool, model: &str) -> AppResult<Option<ModelPolicyRecord>> {
    let row = sqlx::query("SELECT model, strategy, active_provider FROM model_policies WHERE model = ?1")
        .bind(model)
        .fetch_optional(pool)
        .await?;

    Ok(row.as_ref().map(|r| ModelPolicyRecord {
        model: r.get("model"),
        strategy: ModelStrategy::parse(r.get::<String, _>("strategy").as_str()),
        active_provider: r.get("active_provider"),
    }))
}

/// 一次读回全部策略，避免模型目录逐条查库。
pub async fn all(pool: &SqlitePool) -> AppResult<Vec<ModelPolicyRecord>> {
    let rows = sqlx::query("SELECT model, strategy, active_provider FROM model_policies")
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|r| ModelPolicyRecord {
            model: r.get("model"),
            strategy: ModelStrategy::parse(r.get::<String, _>("strategy").as_str()),
            active_provider: r.get("active_provider"),
        })
        .collect())
}

/// 新建或更新。
///
/// 校验 `active_provider` 必须是个真实存在的渠道 tag —— 写进去一个拼错的名字，
/// 表现是"切换了但没生效"，而界面上看不出任何异常，很难排查。
pub async fn upsert(pool: &SqlitePool, rec: &ModelPolicyRecord) -> AppResult<()> {
    if let Some(tag) = rec.active_provider.as_deref().filter(|s| !s.is_empty()) {
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM providers WHERE tag = ?1")
            .bind(tag)
            .fetch_optional(pool)
            .await?;
        if exists.is_none() {
            return Err(crate::error::AppError::ProviderNotFound(tag.to_string()));
        }
    }

    sqlx::query(
        "INSERT INTO model_policies (model, strategy, active_provider, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(model) DO UPDATE SET
             strategy = excluded.strategy,
             active_provider = excluded.active_provider,
             updated_at = excluded.updated_at",
    )
    .bind(&rec.model)
    .bind(rec.strategy.as_str())
    .bind(rec.active_provider.as_deref().filter(|s| !s.is_empty()))
    .bind(now_ms())
    .execute(pool)
    .await?;

    Ok(())
}

/// 删掉该模型的策略行 → 交回 selector 与路由规则。
pub async fn delete(pool: &SqlitePool, model: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM model_policies WHERE model = ?1")
        .bind(model)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::providers::ProviderInput;
    use crate::storage::models::{AuthStyle, ProviderKind};

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    async fn add_provider(p: &SqlitePool, tag: &str) {
        crate::storage::providers::upsert(
            p,
            &ProviderInput {
                id: None,
                name: tag.into(),
                kind: ProviderKind::OpenAiChat,
                base_url: "https://x".into(),
                api_key: None,
                auth_style: AuthStyle::Bearer,
                protocols: Vec::new(),
                extra_headers: Default::default(),
                param_override: None,
                weight: 1,
                priority: 0,
                enabled: true,
                timeout_ms: 60_000,
                proxy: Default::default(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn absent_row_means_no_policy() {
        let p = pool().await;
        assert!(get(&p, "m").await.unwrap().is_none());
        assert!(all(&p).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_then_get_roundtrips_and_updates_in_place() {
        let p = pool().await;
        add_provider(&p, "a").await;

        upsert(
            &p,
            &ModelPolicyRecord {
                model: "m".into(),
                strategy: ModelStrategy::Latency,
                active_provider: None,
            },
        )
        .await
        .unwrap();

        let got = get(&p, "m").await.unwrap().unwrap();
        assert_eq!(got.strategy, ModelStrategy::Latency);

        // 再写一次是原地更新，不是插一行新的。
        upsert(
            &p,
            &ModelPolicyRecord {
                model: "m".into(),
                strategy: ModelStrategy::Weight,
                active_provider: Some("a".into()),
            },
        )
        .await
        .unwrap();

        assert_eq!(all(&p).await.unwrap().len(), 1);
        let got = get(&p, "m").await.unwrap().unwrap();
        assert_eq!(got.strategy, ModelStrategy::Weight);
        assert_eq!(got.active_provider.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn unknown_active_provider_is_rejected() {
        // 拼错的 tag 会让"切换了但不生效"变成一条查不出来的现象，所以在入口挡掉。
        let p = pool().await;
        let err = upsert(
            &p,
            &ModelPolicyRecord {
                model: "m".into(),
                strategy: ModelStrategy::Priority,
                active_provider: Some("nope".into()),
            },
        )
        .await;
        assert!(err.is_err());
        assert!(get(&p, "m").await.unwrap().is_none(), "失败不该留下半条记录");
    }

    #[tokio::test]
    async fn empty_active_provider_is_stored_as_null() {
        // 前端"按策略自动"会传空串，那等同于没选，不能存成一个空 tag。
        let p = pool().await;
        upsert(
            &p,
            &ModelPolicyRecord {
                model: "m".into(),
                strategy: ModelStrategy::Priority,
                active_provider: Some(String::new()),
            },
        )
        .await
        .unwrap();
        assert_eq!(get(&p, "m").await.unwrap().unwrap().active_provider, None);
    }

    #[tokio::test]
    async fn delete_hands_the_model_back_to_the_selector() {
        let p = pool().await;
        upsert(
            &p,
            &ModelPolicyRecord {
                model: "m".into(),
                strategy: ModelStrategy::Priority,
                active_provider: None,
            },
        )
        .await
        .unwrap();

        delete(&p, "m").await.unwrap();
        assert!(get(&p, "m").await.unwrap().is_none());
        // 删不存在的也不报错。
        delete(&p, "m").await.unwrap();
    }

    #[test]
    fn strategy_parse_falls_back_to_priority() {
        // 脏数据不该让整个模型页打不开。
        assert_eq!(ModelStrategy::parse("nonsense"), ModelStrategy::Priority);
        assert_eq!(ModelStrategy::parse("latency"), ModelStrategy::Latency);
        assert_eq!(ModelStrategy::parse("weight"), ModelStrategy::Weight);
    }
}
