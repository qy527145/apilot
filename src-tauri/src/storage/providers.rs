//! 渠道（provider）的读写。

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

use super::models::{AuthStyle, Provider, ProviderKind, ProviderModel};
use crate::error::{AppError, AppResult};
use crate::util::now_ms;

/// 新建 / 更新渠道的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInput {
    /// 更新时必填；新建时忽略。
    pub id: Option<i64>,
    pub tag: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "default_auth_style")]
    pub auth_style: AuthStyle,
    #[serde(default)]
    pub extra_headers: IndexMap<String, String>,
    #[serde(default)]
    pub param_override: Option<Value>,
    #[serde(default)]
    pub model_mapping: IndexMap<String, String>,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: i64,
}

fn default_auth_style() -> AuthStyle {
    AuthStyle::Bearer
}
fn default_weight() -> i64 {
    1
}
fn default_true() -> bool {
    true
}
fn default_timeout() -> i64 {
    600_000
}

impl ProviderInput {
    /// 基本校验。失败时返回可读原因，供前端直接展示。
    pub fn validate(&self) -> Result<(), String> {
        if self.tag.trim().is_empty() {
            return Err("tag 不能为空".into());
        }
        // tag 会被 selector / 路由规则引用，限制字符集避免歧义。
        if !self
            .tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err("tag 只能包含字母、数字、-、_、.".into());
        }
        if self.name.trim().is_empty() {
            return Err("名称不能为空".into());
        }
        if self.base_url.trim().is_empty() {
            return Err("base_url 不能为空".into());
        }
        if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
            return Err("base_url 必须以 http:// 或 https:// 开头".into());
        }
        if self.timeout_ms < 1000 {
            return Err("超时不能小于 1 秒".into());
        }
        Ok(())
    }
}

const SELECT_COLUMNS: &str = "id, tag, name, kind, base_url, api_key, auth_style, \
     extra_headers, param_override, model_mapping, weight, priority, enabled, \
     timeout_ms, created_at, updated_at";

fn row_to_provider(row: &sqlx::sqlite::SqliteRow) -> Provider {
    let json_map = |s: String| -> IndexMap<String, String> {
        serde_json::from_str(&s).unwrap_or_default()
    };

    Provider {
        id: row.get("id"),
        tag: row.get("tag"),
        name: row.get("name"),
        kind: ProviderKind::parse(row.get::<String, _>("kind").as_str())
            .unwrap_or(ProviderKind::OpenAiChat),
        base_url: row.get("base_url"),
        api_key: row.get("api_key"),
        auth_style: AuthStyle::parse(row.get::<String, _>("auth_style").as_str()),
        extra_headers: json_map(row.get("extra_headers")),
        param_override: row
            .get::<Option<String>, _>("param_override")
            .and_then(|s| serde_json::from_str(&s).ok()),
        model_mapping: json_map(row.get("model_mapping")),
        weight: row.get("weight"),
        priority: row.get("priority"),
        enabled: row.get::<i64, _>("enabled") != 0,
        timeout_ms: row.get("timeout_ms"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Provider>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM providers ORDER BY priority DESC, id ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_provider).collect())
}

pub async fn list_enabled(pool: &SqlitePool) -> AppResult<Vec<Provider>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM providers WHERE enabled = 1 ORDER BY priority DESC, id ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_provider).collect())
}

pub async fn get(pool: &SqlitePool, id: i64) -> AppResult<Option<Provider>> {
    let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM providers WHERE id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_provider))
}

pub async fn get_by_tag(pool: &SqlitePool, tag: &str) -> AppResult<Option<Provider>> {
    let row = sqlx::query(&format!("SELECT {SELECT_COLUMNS} FROM providers WHERE tag = ?1"))
        .bind(tag)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_provider))
}

/// 新建或更新。返回落库后的完整记录。
pub async fn upsert(pool: &SqlitePool, input: &ProviderInput) -> AppResult<Provider> {
    input
        .validate()
        .map_err(|e| AppError::msg(format!("渠道配置无效: {e}")))?;

    let now = now_ms();
    let extra = serde_json::to_string(&input.extra_headers)?;
    let mapping = serde_json::to_string(&input.model_mapping)?;
    let param_override = match &input.param_override {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };

    let id = match input.id {
        Some(id) => {
            // 留空 api_key 表示"不改动已存的密钥"，避免前端因为不回显密钥而把 key 抹掉。
            let result = sqlx::query(
                "UPDATE providers SET tag=?1, name=?2, kind=?3, base_url=?4,
                     api_key = COALESCE(?5, api_key), auth_style=?6, extra_headers=?7,
                     param_override=?8, model_mapping=?9, weight=?10, priority=?11,
                     enabled=?12, timeout_ms=?13, updated_at=?14
                 WHERE id=?15",
            )
            .bind(&input.tag)
            .bind(&input.name)
            .bind(input.kind.as_str())
            .bind(&input.base_url)
            .bind(&input.api_key)
            .bind(input.auth_style.as_str())
            .bind(&extra)
            .bind(&param_override)
            .bind(&mapping)
            .bind(input.weight)
            .bind(input.priority)
            .bind(input.enabled as i64)
            .bind(input.timeout_ms)
            .bind(now)
            .bind(id)
            .execute(pool)
            .await?;

            if result.rows_affected() == 0 {
                return Err(AppError::ProviderNotFound(id.to_string()));
            }
            id
        }
        None => {
            let result = sqlx::query(
                "INSERT INTO providers (tag, name, kind, base_url, api_key, auth_style,
                     extra_headers, param_override, model_mapping, weight, priority,
                     enabled, timeout_ms, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
            )
            .bind(&input.tag)
            .bind(&input.name)
            .bind(input.kind.as_str())
            .bind(&input.base_url)
            .bind(&input.api_key)
            .bind(input.auth_style.as_str())
            .bind(&extra)
            .bind(&param_override)
            .bind(&mapping)
            .bind(input.weight)
            .bind(input.priority)
            .bind(input.enabled as i64)
            .bind(input.timeout_ms)
            .bind(now)
            .execute(pool)
            .await
            .map_err(|e| {
                // tag 有唯一约束；给出比原始 SQL 错误更可读的提示。
                if e.to_string().contains("UNIQUE") {
                    AppError::msg(format!("tag「{}」已被占用", input.tag))
                } else {
                    AppError::Db(e)
                }
            })?;
            result.last_insert_rowid()
        }
    };

    get(pool, id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(id.to_string()))
}

pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<()> {
    let r = sqlx::query("DELETE FROM providers WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::ProviderNotFound(id.to_string()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// model ↔ 渠道映射
// ---------------------------------------------------------------------------

pub async fn list_models(pool: &SqlitePool, provider_id: i64) -> AppResult<Vec<ProviderModel>> {
    let rows = sqlx::query(
        "SELECT id, provider_id, model, upstream_model, client_group, priority, weight, enabled
         FROM provider_models WHERE provider_id = ?1 ORDER BY model ASC",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| ProviderModel {
            id: r.get("id"),
            provider_id: r.get("provider_id"),
            model: r.get("model"),
            upstream_model: r.get("upstream_model"),
            client_group: r.get("client_group"),
            priority: r.get("priority"),
            weight: r.get("weight"),
            enabled: r.get::<i64, _>("enabled") != 0,
        })
        .collect())
}

/// 全量替换某渠道的模型映射。
///
/// 同时把 `providers.model_mapping` 派生成「入站名 → 上游名」写回去，这是本次改动的关键：
/// 请求改写（`Provider::upstream_model`）和网关 `GET /v1/models` 读的都是那一列，
/// 而 `provider_models` 表只负责"该渠道声明支持哪些模型"。两边各写各的会让人以为
/// 映射生效了，实际请求仍按原名发出去。
///
/// 同名条目也写进 mapping（而不是只在有差异时写）——`/v1/models` 广告的就是
/// `model_mapping.keys()`，只写差异项会让纯 DeepSeek 这类"入站名 == 上游名"的
/// 配置探测不到任何模型。
pub async fn set_models(
    pool: &SqlitePool,
    provider_id: i64,
    models: &[(String, Option<String>)],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM provider_models WHERE provider_id = ?1")
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;

    let mut mapping: IndexMap<String, String> = IndexMap::new();
    for (model, upstream) in models {
        sqlx::query(
            "INSERT INTO provider_models (provider_id, model, upstream_model, client_group,
                 priority, weight, enabled)
             VALUES (?1, ?2, ?3, '*', 0, 1, 1)",
        )
        .bind(provider_id)
        .bind(model)
        .bind(upstream)
        .execute(&mut *tx)
        .await?;

        mapping.insert(model.clone(), upstream.clone().unwrap_or_else(|| model.clone()));
    }

    let json = serde_json::to_string(&mapping)?;
    let affected = sqlx::query("UPDATE providers SET model_mapping = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(&json)
        .bind(now_ms())
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;
    if affected.rows_affected() == 0 {
        return Err(AppError::ProviderNotFound(provider_id.to_string()));
    }

    tx.commit().await?;
    Ok(())
}

/// 是否有「启用且声明过模型」的渠道。
///
/// 接管客户端前必须至少有一个 —— 否则客户端会把请求打到网关上然后全线报错，
/// 用户却在客户端里看不到"其实还没配渠道"这个真实原因。
pub async fn has_declared_models(pool: &SqlitePool) -> AppResult<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM providers p
         WHERE p.enabled = 1
           AND EXISTS (SELECT 1 FROM provider_models m
                       WHERE m.provider_id = p.id AND m.enabled = 1)
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// 声明自己支持某模型的所有启用渠道，按 priority 降序。
///
/// 没在 `provider_models` 里声明任何模型的渠道视为"通吃"，也会被返回 ——
/// 否则用户新建渠道后必须手工录入模型才能用，体验很差。
pub async fn channels_for_model(pool: &SqlitePool, model: &str) -> AppResult<Vec<Provider>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS} FROM providers p
         WHERE p.enabled = 1
           AND (
             NOT EXISTS (SELECT 1 FROM provider_models m WHERE m.provider_id = p.id)
             OR EXISTS (
               SELECT 1 FROM provider_models m
               WHERE m.provider_id = p.id AND m.model = ?1 AND m.enabled = 1
             )
           )
         ORDER BY p.priority DESC, p.weight DESC, p.id ASC"
    ))
    .bind(model)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_provider).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tag: &str) -> ProviderInput {
        ProviderInput {
            id: None,
            tag: tag.into(),
            name: format!("渠道 {tag}"),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            api_key: Some("sk-test".into()),
            auth_style: AuthStyle::XApiKey,
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
        }
    }

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    #[tokio::test]
    async fn create_and_read_back() {
        let p = pool().await;
        let created = upsert(&p, &input("anthropic-official")).await.unwrap();
        assert!(created.id > 0);
        assert_eq!(created.tag, "anthropic-official");
        assert_eq!(created.kind, ProviderKind::Anthropic);
        assert_eq!(created.auth_style, AuthStyle::XApiKey);

        let fetched = get_by_tag(&p, "anthropic-official").await.unwrap().unwrap();
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn update_preserves_api_key_when_omitted() {
        let p = pool().await;
        let created = upsert(&p, &input("a")).await.unwrap();

        let mut upd = input("a");
        upd.id = Some(created.id);
        upd.name = "改名了".into();
        upd.api_key = None; // 前端不回显密钥
        upsert(&p, &upd).await.unwrap();

        let after = get(&p, created.id).await.unwrap().unwrap();
        assert_eq!(after.name, "改名了");
        assert_eq!(after.api_key.as_deref(), Some("sk-test"), "密钥不应被抹掉");
    }

    #[tokio::test]
    async fn update_can_replace_api_key() {
        let p = pool().await;
        let created = upsert(&p, &input("a")).await.unwrap();

        let mut upd = input("a");
        upd.id = Some(created.id);
        upd.api_key = Some("sk-new".into());
        upsert(&p, &upd).await.unwrap();

        let after = get(&p, created.id).await.unwrap().unwrap();
        assert_eq!(after.api_key.as_deref(), Some("sk-new"));
    }

    #[tokio::test]
    async fn duplicate_tag_is_rejected_with_readable_message() {
        let p = pool().await;
        upsert(&p, &input("dup")).await.unwrap();
        let err = upsert(&p, &input("dup")).await.unwrap_err();
        assert!(err.to_string().contains("已被占用"), "实际: {err}");
    }

    #[tokio::test]
    async fn validation_rejects_bad_input() {
        let p = pool().await;

        let mut bad = input("ok");
        bad.tag = "has space".into();
        assert!(upsert(&p, &bad).await.is_err());

        let mut bad = input("ok");
        bad.base_url = "ftp://x".into();
        assert!(upsert(&p, &bad).await.is_err());

        let mut bad = input("ok");
        bad.name = "  ".into();
        assert!(upsert(&p, &bad).await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_row_and_cascades_models() {
        let p = pool().await;
        let c = upsert(&p, &input("del")).await.unwrap();
        set_models(&p, c.id, &[("m1".into(), None)])
            .await
            .unwrap();

        delete(&p, c.id).await.unwrap();
        assert!(get(&p, c.id).await.unwrap().is_none());
        assert!(list_models(&p, c.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_missing_is_an_error() {
        let p = pool().await;
        assert!(delete(&p, 999).await.is_err());
    }

    #[tokio::test]
    async fn json_fields_roundtrip() {
        let p = pool().await;
        let mut inp = input("json");
        inp.extra_headers
            .insert("X-Custom".into(), "v".into());
        inp.model_mapping
            .insert("claude-sonnet-5".into(), "claude-3-5-sonnet".into());
        inp.param_override = Some(serde_json::json!({ "temperature": 0.2 }));

        let created = upsert(&p, &inp).await.unwrap();
        assert_eq!(created.extra_headers.get("X-Custom").unwrap(), "v");
        assert_eq!(
            created.model_mapping.get("claude-sonnet-5").unwrap(),
            "claude-3-5-sonnet"
        );
        assert_eq!(created.param_override.unwrap()["temperature"], 0.2);
    }

    #[tokio::test]
    async fn channels_for_model_prefers_declared_then_falls_back_to_wildcard() {
        let p = pool().await;

        // c1 只声明支持 model-a
        let c1 = upsert(&p, &input("c1")).await.unwrap();
        set_models(&p, c1.id, &[("model-a".into(), None)])
            .await
            .unwrap();

        // c2 没有声明任何模型 → 通吃
        let _c2 = upsert(&p, &input("c2")).await.unwrap();

        let for_a = channels_for_model(&p, "model-a").await.unwrap();
        assert_eq!(for_a.len(), 2, "声明的 + 通吃的都应命中");

        let for_b = channels_for_model(&p, "model-b").await.unwrap();
        assert_eq!(for_b.len(), 1, "只有通吃渠道命中");
        assert_eq!(for_b[0].tag, "c2");
    }

    #[tokio::test]
    async fn channels_for_model_excludes_disabled() {
        let p = pool().await;
        let c = upsert(&p, &input("off")).await.unwrap();
        let mut upd = input("off");
        upd.id = Some(c.id);
        upd.enabled = false;
        upsert(&p, &upd).await.unwrap();

        assert!(channels_for_model(&p, "any").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn channels_for_model_orders_by_priority() {
        let p = pool().await;
        let mut low = input("low");
        low.priority = 1;
        upsert(&p, &low).await.unwrap();

        let mut high = input("high");
        high.priority = 10;
        upsert(&p, &high).await.unwrap();

        let list = channels_for_model(&p, "m").await.unwrap();
        assert_eq!(list[0].tag, "high", "priority 高的排前面");
    }

    #[tokio::test]
    async fn set_models_syncs_into_provider_model_mapping() {
        let p = pool().await;
        let c = upsert(&p, &input("sync")).await.unwrap();

        set_models(
            &p,
            c.id,
            &[
                ("claude-sonnet-4-5".into(), Some("deepseek-chat".into())),
                // 同名条目也要进 mapping —— /v1/models 广告的就是它的 keys。
                ("deepseek-reasoner".into(), None),
            ],
        )
        .await
        .unwrap();

        let after = get(&p, c.id).await.unwrap().unwrap();
        assert_eq!(
            after.upstream_model("claude-sonnet-4-5"),
            "deepseek-chat",
            "面板里填的上游名必须真的参与请求改写"
        );
        assert_eq!(after.upstream_model("deepseek-reasoner"), "deepseek-reasoner");
        assert_eq!(after.model_mapping.len(), 2);
    }

    #[tokio::test]
    async fn set_models_with_empty_list_restores_wildcard() {
        let p = pool().await;
        let c = upsert(&p, &input("clear")).await.unwrap();
        set_models(&p, c.id, &[("m".into(), None)]).await.unwrap();

        // 清空声明 == 恢复"通吃"，这也是用户撤销操作的路径。
        set_models(&p, c.id, &[]).await.unwrap();

        let after = get(&p, c.id).await.unwrap().unwrap();
        assert!(after.model_mapping.is_empty());
        assert_eq!(channels_for_model(&p, "随便什么").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_models_on_missing_provider_is_an_error() {
        let p = pool().await;
        assert!(set_models(&p, 999, &[("m".into(), None)]).await.is_err());
    }

    #[tokio::test]
    async fn has_declared_models_tracks_enabled_declared_channels() {
        let p = pool().await;
        assert!(!has_declared_models(&p).await.unwrap(), "没有渠道时未就绪");

        let c = upsert(&p, &input("wild")).await.unwrap();
        assert!(
            !has_declared_models(&p).await.unwrap(),
            "只建渠道但没声明模型的渠道是通吃的，仍视为未就绪"
        );

        set_models(&p, c.id, &[("m".into(), None)]).await.unwrap();
        assert!(has_declared_models(&p).await.unwrap());

        let mut off = input("wild");
        off.id = Some(c.id);
        off.enabled = false;
        upsert(&p, &off).await.unwrap();
        assert!(
            !has_declared_models(&p).await.unwrap(),
            "停用的渠道不算数"
        );
    }
}
