//! 模型单价系数的读写。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::billing::pricing::{ModelPricing, PricingTable};
use crate::error::{AppError, AppResult};
use crate::util::now_ms;

const FALLBACK_KEY: &str = "pricing_fallback";

const COLUMNS: &str = "model, model_ratio, completion_ratio, cache_ratio, cache_create_ratio, \
     group_ratio, image_ratio, audio_ratio, tool_call_surcharge, other_ratios, currency, \
     source, updated_at";

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
        source: r.get("source"),
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
             other_ratios, currency, source, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,?12)
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
             source = NULL,
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

/* ================================================================== */
/* 目录批量导入                                                        */
/* ================================================================== */

/// 目录导入时对某一行的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceAction {
    /// 库里没有 → 新增。
    Insert,
    /// 库里有，且是之前从目录导进去的 → 覆盖。
    Update,
    /// 库里有，但是**用户手填的** → 一律不碰。
    KeepUserOwned,
    /// 库里有，倍率和目录给的一模一样 → 没什么可做。
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceDiffRow {
    pub model: String,
    pub action: PriceAction,
    /// 库里的现值。新增时为 `None`。
    pub current: Option<ModelPricing>,
    pub incoming: ModelPricing,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceImportStats {
    pub inserted: u64,
    pub updated: u64,
    pub kept_user_owned: u64,
    pub unchanged: u64,
}

/// 只有这几个倍率参与"有没有变化"的比较。
///
/// 不比较 `updated_at`（每次导入都不同，会把每行都判成"变了"）和 `source`
/// （本来就是导入时写的），也不比较 `group_ratio` / `other_ratios` ——
/// 目录不提供这两项，拿默认值去比会把用户调过的分组倍率判成"变了"从而覆盖掉。
fn ratios_differ(a: &ModelPricing, b: &ModelPricing) -> bool {
    a.model_ratio != b.model_ratio
        || a.completion_ratio != b.completion_ratio
        || a.cache_ratio != b.cache_ratio
        || a.cache_create_ratio != b.cache_create_ratio
}

/// 算出「目录里这批价格落到库上会怎样」，**不写库**。
///
/// `incoming` 每行的 `source` 由调用方填好（形如 `catalog:models.dev`）；
/// 库里 `source` 为 `None` 的行被认定是用户手填的，永远跳过。
pub async fn diff_catalog(
    pool: &SqlitePool,
    incoming: &[ModelPricing],
) -> AppResult<Vec<PriceDiffRow>> {
    let existing: HashMap<String, ModelPricing> =
        list(pool).await?.into_iter().map(|p| (p.model.clone(), p)).collect();

    let mut rows: Vec<PriceDiffRow> = incoming
        .iter()
        .map(|inc| {
            let current = existing.get(&inc.model).cloned();
            let action = match &current {
                None => PriceAction::Insert,
                Some(cur) if cur.source.is_none() => PriceAction::KeepUserOwned,
                Some(cur) if !ratios_differ(cur, inc) => PriceAction::Unchanged,
                Some(_) => PriceAction::Update,
            };
            PriceDiffRow {
                model: inc.model.clone(),
                action,
                current,
                incoming: inc.clone().normalized(),
            }
        })
        .collect();

    rows.sort_by(|a, b| a.model.cmp(&b.model));
    Ok(rows)
}

/// 按 [`diff_catalog`] 的结果落库。
///
/// 刻意接收 diff 结果而不是自己重算：预览和实际应用必须是同一套判定，各写一遍
/// 迟早会分叉 —— 那时用户看到"将更新 12 个"，点下去却动了别的行。
pub async fn apply_catalog(
    pool: &SqlitePool,
    rows: &[PriceDiffRow],
) -> AppResult<PriceImportStats> {
    let mut stats = PriceImportStats::default();
    let mut tx = pool.begin().await?;
    let now = now_ms();

    for row in rows {
        match row.action {
            PriceAction::KeepUserOwned => {
                stats.kept_user_owned += 1;
                continue;
            }
            PriceAction::Unchanged => {
                stats.unchanged += 1;
                continue;
            }
            PriceAction::Insert | PriceAction::Update => {}
        }

        let p = &row.incoming;
        let other = serde_json::to_string(&p.other_ratios)?;
        let currency = if p.currency.trim().is_empty() {
            "USD".to_string()
        } else {
            p.currency.clone()
        };

        // 这里的 WHERE 是第二道闸：即便 diff 判错了，用户手填的行（source IS NULL）
        // 在 SQL 层面也覆盖不掉。
        sqlx::query(
            "INSERT INTO model_pricing (model, model_ratio, completion_ratio, cache_ratio,
                 cache_create_ratio, group_ratio, image_ratio, audio_ratio, tool_call_surcharge,
                 other_ratios, currency, source, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(model) DO UPDATE SET
                 model_ratio = excluded.model_ratio,
                 completion_ratio = excluded.completion_ratio,
                 cache_ratio = excluded.cache_ratio,
                 cache_create_ratio = excluded.cache_create_ratio,
                 image_ratio = excluded.image_ratio,
                 audio_ratio = excluded.audio_ratio,
                 tool_call_surcharge = excluded.tool_call_surcharge,
                 source = excluded.source,
                 updated_at = excluded.updated_at
             WHERE model_pricing.source IS NOT NULL",
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
        .bind(&p.source)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        match row.action {
            PriceAction::Insert => stats.inserted += 1,
            _ => stats.updated += 1,
        }
    }

    tx.commit().await?;
    Ok(stats)
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

    /* ------------------------------ 目录导入 ------------------------------ */

    fn catalog_entry(model: &str, ratio: f64) -> ModelPricing {
        ModelPricing {
            model: model.into(),
            model_ratio: ratio,
            completion_ratio: 5.0,
            cache_ratio: 0.1,
            cache_create_ratio: 1.25,
            source: Some("catalog:models.dev".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_brand_new_model_is_an_insert() {
        let p = pool().await;
        let diff = diff_catalog(&p, &[catalog_entry("fresh", 1.5)]).await.unwrap();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].action, PriceAction::Insert);
        assert!(diff[0].current.is_none());

        let stats = apply_catalog(&p, &diff).await.unwrap();
        assert_eq!(stats.inserted, 1);
        let got = get(&p, "fresh").await.unwrap().unwrap();
        assert_eq!(got.model_ratio, 1.5);
        assert_eq!(got.source.as_deref(), Some("catalog:models.dev"));
    }

    #[tokio::test]
    async fn a_hand_edited_price_is_never_touched() {
        // 整个导入功能的底线：用户手填的行不能被目录刷新改掉。
        let p = pool().await;
        let mut mine = pricing("mine");
        mine.model_ratio = 42.0;
        upsert(&p, &mine).await.unwrap();
        assert!(get(&p, "mine").await.unwrap().unwrap().source.is_none());

        let diff = diff_catalog(&p, &[catalog_entry("mine", 1.5)]).await.unwrap();
        assert_eq!(diff[0].action, PriceAction::KeepUserOwned);

        let stats = apply_catalog(&p, &diff).await.unwrap();
        assert_eq!(stats.kept_user_owned, 1);
        assert_eq!(stats.inserted, 0);
        assert_eq!(
            get(&p, "mine").await.unwrap().unwrap().model_ratio,
            42.0,
            "手填的价必须原封不动"
        );
    }

    #[tokio::test]
    async fn editing_a_catalog_price_hands_ownership_back_to_the_user() {
        // 用户改了目录导进来的价，那行就该归他，下次导入不能再覆盖。
        let p = pool().await;
        let diff = diff_catalog(&p, &[catalog_entry("m", 1.5)]).await.unwrap();
        apply_catalog(&p, &diff).await.unwrap();

        let mut edited = get(&p, "m").await.unwrap().unwrap();
        edited.model_ratio = 9.0;
        upsert(&p, &edited).await.unwrap();

        assert!(get(&p, "m").await.unwrap().unwrap().source.is_none());
        let again = diff_catalog(&p, &[catalog_entry("m", 1.5)]).await.unwrap();
        assert_eq!(again[0].action, PriceAction::KeepUserOwned);
    }

    #[tokio::test]
    async fn a_catalog_row_can_be_refreshed_by_a_later_import() {
        let p = pool().await;
        let diff = diff_catalog(&p, &[catalog_entry("m", 1.5)]).await.unwrap();
        apply_catalog(&p, &diff).await.unwrap();

        let diff2 = diff_catalog(&p, &[catalog_entry("m", 2.5)]).await.unwrap();
        assert_eq!(diff2[0].action, PriceAction::Update);
        assert_eq!(diff2[0].current.as_ref().unwrap().model_ratio, 1.5);

        let stats = apply_catalog(&p, &diff2).await.unwrap();
        assert_eq!(stats.updated, 1);
        assert_eq!(get(&p, "m").await.unwrap().unwrap().model_ratio, 2.5);
    }

    #[tokio::test]
    async fn an_identical_row_is_reported_as_unchanged() {
        // 目录天天重拉，绝大多数行是没变的，不该在界面上报成"将更新 3000 行"。
        let p = pool().await;
        let diff = diff_catalog(&p, &[catalog_entry("m", 1.5)]).await.unwrap();
        apply_catalog(&p, &diff).await.unwrap();

        let again = diff_catalog(&p, &[catalog_entry("m", 1.5)]).await.unwrap();
        assert_eq!(again[0].action, PriceAction::Unchanged);
        let stats = apply_catalog(&p, &again).await.unwrap();
        assert_eq!(stats.unchanged, 1);
        assert_eq!(stats.updated, 0);
    }

    #[tokio::test]
    async fn refreshing_a_row_keeps_the_users_group_ratio() {
        // 目录不提供分组倍率。覆盖时不能把它带回默认值 —— 那是用户按客户端
        // 或渠道调过的折扣，一次价格更新抹掉就再也找不回来了。
        let p = pool().await;
        let mut row = catalog_entry("m", 1.5);
        row.group_ratio = 0.8;
        row.other_ratios.insert("peak".into(), 1.8);
        let diff = diff_catalog(&p, &[row]).await.unwrap();
        apply_catalog(&p, &diff).await.unwrap();

        let diff2 = diff_catalog(&p, &[catalog_entry("m", 2.5)]).await.unwrap();
        apply_catalog(&p, &diff2).await.unwrap();

        let got = get(&p, "m").await.unwrap().unwrap();
        assert_eq!(got.model_ratio, 2.5, "目录给的要更新");
        assert_eq!(got.group_ratio, 0.8, "用户调的分组倍率要留着");
        assert_eq!(got.other_ratios.get("peak"), Some(&1.8));
    }

    #[tokio::test]
    async fn a_hand_edited_row_survives_even_if_the_diff_says_otherwise() {
        // SQL 层面还有第二道闸：就算 diff 判成了 Update，手填的行也改不动。
        let p = pool().await;
        let mine = pricing("mine");
        upsert(&p, &mine).await.unwrap();

        let forced = PriceDiffRow {
            model: "mine".into(),
            action: PriceAction::Update,
            current: None,
            incoming: catalog_entry("mine", 99.0),
        };
        apply_catalog(&p, &[forced]).await.unwrap();

        assert_eq!(
            get(&p, "mine").await.unwrap().unwrap().model_ratio,
            3.0,
            "手填的行在 SQL 层也挡住了"
        );
    }

    #[tokio::test]
    async fn the_diff_is_sorted_for_a_stable_ui() {
        let p = pool().await;
        let diff = diff_catalog(
            &p,
            &[catalog_entry("zeta", 1.0), catalog_entry("alpha", 1.0)],
        )
        .await
        .unwrap();
        assert_eq!(diff[0].model, "alpha");
        assert_eq!(diff[1].model, "zeta");
    }
}
