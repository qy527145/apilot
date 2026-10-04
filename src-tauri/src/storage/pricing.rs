//! 模型单价系数的读写。

use std::collections::HashMap;

use sqlx::{Row, SqlitePool};

use crate::billing::pricing::{ModelPricing, PricingTable};
use crate::error::{AppError, AppResult};
use crate::util::now_ms;

const FALLBACK_KEY: &str = "pricing_fallback";

const COLUMNS: &str = "model, model_ratio, completion_ratio, cache_ratio, cache_create_ratio, \
     group_ratio, image_ratio, audio_ratio, tool_call_surcharge, other_ratios, currency, updated_at";

fn row_to_pricing(r: &sqlx::sqlite::SqliteRow) -> ModelPricing {
    let other_json: String = r.get("other_ratios");
    ModelPricing {
        model: r.get("model"),
        model_ratio: r.get("model_ratio"),
        completion_ratio: r.get("completion_ratio"),
        cache_ratio: r.get("cache_ratio"),
        cache_create_ratio: r.get("cache_create_ratio"),
        group_ratio: r.get("group_ratio"),
        image_ratio: r.get("image_ratio"),
        audio_ratio: r.get("audio_ratio"),
        tool_call_surcharge: r.get("tool_call_surcharge"),
        other_ratios: serde_json::from_str::<HashMap<String, f64>>(&other_json).unwrap_or_default(),
        currency: r.get("currency"),
        updated_at: r.get("updated_at"),
    }
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<ModelPricing>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM model_pricing ORDER BY model ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_pricing).collect())
}

pub async fn upsert(pool: &SqlitePool, input: &ModelPricing) -> AppResult<ModelPricing> {
    if input.model.trim().is_empty() {
        return Err(AppError::msg("模型名不能为空"));
    }

    // 归一化会挡掉负倍率与非有限值 —— 否则计费会算出负数（等于倒贴）。
    let p = input.clone().normalized();
    let other = serde_json::to_string(&p.other_ratios)?;
    let currency = if p.currency.trim().is_empty() {
        "USD".to_string()
    } else {
        p.currency.clone()
    };

    sqlx::query(
        "INSERT INTO model_pricing (model, model_ratio, completion_ratio, cache_ratio,
             cache_create_ratio, group_ratio, image_ratio, audio_ratio, tool_call_surcharge,
             other_ratios, currency, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
         ON CONFLICT(model) DO UPDATE SET
             model_ratio = excluded.model_ratio,
             completion_ratio = excluded.completion_ratio,
             cache_ratio = excluded.cache_ratio,
             cache_create_ratio = excluded.cache_create_ratio,
             group_ratio = excluded.group_ratio,
             image_ratio = excluded.image_ratio,
             audio_ratio = excluded.audio_ratio,
             tool_call_surcharge = excluded.tool_call_surcharge,
             other_ratios = excluded.other_ratios,
             currency = excluded.currency,
             updated_at = excluded.updated_at",
    )
    .bind(&p.model)
    .bind(p.model_ratio)
    .bind(p.completion_ratio)
    .bind(p.cache_ratio)
    .bind(p.cache_create_ratio)
    .bind(p.group_ratio)
    .bind(p.image_ratio)
    .bind(p.audio_ratio)
    .bind(p.tool_call_surcharge)
    .bind(&other)
    .bind(&currency)
    .bind(now_ms())
    .execute(pool)
    .await?;

    get(pool, &p.model)
        .await?
        .ok_or_else(|| AppError::msg("单价写入后读取失败"))
}

pub async fn get(pool: &SqlitePool, model: &str) -> AppResult<Option<ModelPricing>> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM model_pricing WHERE model = ?1"
    ))
    .bind(model)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_pricing))
}

pub async fn delete(pool: &SqlitePool, model: &str) -> AppResult<()> {
    let r = sqlx::query("DELETE FROM model_pricing WHERE model = ?1")
        .bind(model)
        .execute(pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::msg(format!("找不到模型「{model}」的单价配置")));
    }
    Ok(())
}

/// 未配置模型的兜底倍率。
pub async fn fallback(pool: &SqlitePool) -> AppResult<ModelPricing> {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings_kv WHERE key = ?1")
        .bind(FALLBACK_KEY)
        .fetch_optional(pool)
        .await?;

    Ok(raw
        .and_then(|s| serde_json::from_str::<ModelPricing>(&s).ok())
        .map(|p| p.normalized())
        .unwrap_or_default())
}

/// 从数据库装配一份可直接投入使用的单价表。
pub async fn load_table(pool: &SqlitePool) -> AppResult<PricingTable> {
    let entries = list(pool).await?;
    let fb = fallback(pool).await?;
    Ok(PricingTable::new(entries, fb))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn pricing(model: &str) -> ModelPricing {
        ModelPricing {
            model: model.into(),
            model_ratio: 3.0,
            completion_ratio: 5.0,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn insert_and_read_back() {
        let p = pool().await;
        let created = upsert(&p, &pricing("claude-sonnet-5")).await.unwrap();
        assert_eq!(created.model_ratio, 3.0);
        assert_eq!(created.completion_ratio, 5.0);
        assert_eq!(created.currency, "USD");

        let got = get(&p, "claude-sonnet-5").await.unwrap().unwrap();
        assert_eq!(got.model_ratio, 3.0);
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let p = pool().await;
        upsert(&p, &pricing("m")).await.unwrap();

        let mut upd = pricing("m");
        upd.model_ratio = 9.0;
        upsert(&p, &upd).await.unwrap();

        assert_eq!(list(&p).await.unwrap().len(), 1);
        assert_eq!(get(&p, "m").await.unwrap().unwrap().model_ratio, 9.0);
    }

    #[tokio::test]
    async fn negative_ratio_is_normalized_on_write() {
        let p = pool().await;
        let mut bad = pricing("m");
        bad.model_ratio = -1.0;
        bad.tool_call_surcharge = -50;

        let saved = upsert(&p, &bad).await.unwrap();
        assert_eq!(saved.model_ratio, 0.0);
        assert_eq!(saved.tool_call_surcharge, 0);
    }

    #[tokio::test]
    async fn blank_model_is_rejected() {
        let p = pool().await;
        let mut bad = pricing("m");
        bad.model = "  ".into();
        assert!(upsert(&p, &bad).await.is_err());
    }

    #[tokio::test]
    async fn delete_works_and_missing_is_an_error() {
        let p = pool().await;
        upsert(&p, &pricing("m")).await.unwrap();
        delete(&p, "m").await.unwrap();
        assert!(list(&p).await.unwrap().is_empty());
        assert!(delete(&p, "m").await.is_err());
    }

    #[tokio::test]
    async fn other_ratios_roundtrip() {
        let p = pool().await;
        let mut m = pricing("m");
        m.other_ratios.insert("peak".into(), 1.8);
        upsert(&p, &m).await.unwrap();

        let got = get(&p, "m").await.unwrap().unwrap();
        assert_eq!(got.other_ratios.get("peak"), Some(&1.8));
    }


    #[tokio::test]
    async fn corrupt_fallback_falls_back_to_default() {
        let p = pool().await;
        sqlx::query("INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, 'not json', 0)")
            .bind(FALLBACK_KEY)
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(fallback(&p).await.unwrap().model_ratio, 1.0);
    }

}
