//! 路由规则、selector 与兜底配置的读写。

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use super::models::Provider;
use crate::error::{AppError, AppResult};
use crate::routing::rule::{RouteAction, RouteRule, RouteRuleInput};
use crate::routing::rule_item::RuleItem;
use crate::routing::selector::SelectorMode;
use crate::util::now_ms;

// ---------------------------------------------------------------------------
// 路由规则
// ---------------------------------------------------------------------------

pub async fn list_rules(pool: &SqlitePool) -> AppResult<Vec<RouteRule>> {
    let rows = sqlx::query(
        "SELECT id, sort_index, name, enabled, items, action, updated_at
         FROM route_rules ORDER BY sort_index ASC, id ASC",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let items_json: String = r.get("items");
        let action_json: String = r.get("action");

        // 单条规则解析失败不应让整个列表不可用 —— 记录后跳过，
        // 用户可以在 UI 里看到少了一条并去修它，而不是整个路由页打不开。
        let items: Vec<RuleItem> = match serde_json::from_str(&items_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(rule_id = r.get::<i64, _>("id"), "规则条件解析失败，已跳过: {e}");
                continue;
            }
        };
        let action: RouteAction = match serde_json::from_str(&action_json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(rule_id = r.get::<i64, _>("id"), "规则动作解析失败，已跳过: {e}");
                continue;
            }
        };

        out.push(RouteRule {
            id: r.get("id"),
            sort_index: r.get("sort_index"),
            name: r.get("name"),
            enabled: r.get::<i64, _>("enabled") != 0,
            items,
            action,
            updated_at: r.get("updated_at"),
        });
    }
    Ok(out)
}

pub async fn upsert_rule(pool: &SqlitePool, input: &RouteRuleInput) -> AppResult<RouteRule> {
    input
        .validate()
        .map_err(|e| AppError::msg(format!("路由规则无效: {e}")))?;

    let items = serde_json::to_string(&input.items)?;
    let action = serde_json::to_string(&input.action)?;
    let now = now_ms();

    let id = match input.id {
        Some(id) => {
            let r = sqlx::query(
                "UPDATE route_rules SET name=?1, enabled=?2, items=?3, action=?4, updated_at=?5
                 WHERE id=?6",
            )
            .bind(&input.name)
            .bind(input.enabled as i64)
            .bind(&items)
            .bind(&action)
            .bind(now)
            .bind(id)
            .execute(pool)
            .await?;
            if r.rows_affected() == 0 {
                return Err(AppError::msg(format!("找不到路由规则 {id}")));
            }
            id
        }
        None => {
            // 新规则追加到末尾。
            let next: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(sort_index), -1) + 1 FROM route_rules",
            )
            .fetch_one(pool)
            .await?;

            let r = sqlx::query(
                "INSERT INTO route_rules (sort_index, name, enabled, items, action, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
            )
            .bind(next)
            .bind(&input.name)
            .bind(input.enabled as i64)
            .bind(&items)
            .bind(&action)
            .bind(now)
            .execute(pool)
            .await?;
            r.last_insert_rowid()
        }
    };

    list_rules(pool)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| AppError::msg(format!("规则 {id} 写入后读取失败")))
}

pub async fn delete_rule(pool: &SqlitePool, id: i64) -> AppResult<()> {
    let r = sqlx::query("DELETE FROM route_rules WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::msg(format!("找不到路由规则 {id}")));
    }
    Ok(())
}

/// 按给定的 id 顺序重排。未出现在 `ids` 里的规则排在其后，保持原有相对顺序。
pub async fn reorder_rules(pool: &SqlitePool, ids: &[i64]) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    for (idx, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE route_rules SET sort_index = ?1 WHERE id = ?2")
            .bind(idx as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }

    // 没被提到的规则挪到末尾，按原 sort_index 保序。
    let offset = ids.len() as i64;
    sqlx::query(
        "UPDATE route_rules SET sort_index = sort_index + ?1
         WHERE id NOT IN (SELECT value FROM json_each(?2))",
    )
    .bind(offset)
    .bind(serde_json::to_string(ids)?)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 兜底 selector
// ---------------------------------------------------------------------------

pub async fn final_selector(pool: &SqlitePool) -> AppResult<String> {
    let v: Option<String> =
        sqlx::query_scalar("SELECT final_selector FROM route_config WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(v.unwrap_or_else(|| "default".to_string()))
}

pub async fn set_final_selector(pool: &SqlitePool, tag: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO route_config (id, final_selector, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET final_selector = excluded.final_selector,
                                       updated_at = excluded.updated_at",
    )
    .bind(tag)
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Selector
// ---------------------------------------------------------------------------

/// 数据库里的 selector 记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectorRecord {
    pub tag: String,
    pub name: String,
    pub mode: SelectorMode,
    pub members: Vec<String>,
    /// 热切换后持久化的当前选中项。
    pub current_provider: Option<String>,
    pub tolerance_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectorInput {
    pub tag: String,
    pub name: String,
    #[serde(default = "default_mode")]
    pub mode: SelectorMode,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default = "default_tolerance")]
    pub tolerance_ms: i64,
}

fn default_mode() -> SelectorMode {
    SelectorMode::Selector
}
fn default_tolerance() -> i64 {
    50
}

impl SelectorInput {
    pub fn validate(&self) -> Result<(), String> {
        if self.tag.trim().is_empty() {
            return Err("selector tag 不能为空".into());
        }
        if self.name.trim().is_empty() {
            return Err("selector 名称不能为空".into());
        }
        if self.tolerance_ms < 0 {
            return Err("容差不能为负".into());
        }
        let mut seen = std::collections::HashSet::new();
        for m in &self.members {
            if !seen.insert(m) {
                return Err(format!("成员「{m}」重复"));
            }
        }
        Ok(())
    }
}

const SELECTOR_COLUMNS: &str = "tag, name, mode, members, current_provider, tolerance_ms";

fn row_to_selector(row: &sqlx::sqlite::SqliteRow) -> SelectorRecord {
    let members_json: String = row.get("members");
    SelectorRecord {
        tag: row.get("tag"),
        name: row.get("name"),
        mode: SelectorMode::parse(row.get::<String, _>("mode").as_str()),
        members: serde_json::from_str(&members_json).unwrap_or_default(),
        current_provider: row.get("current_provider"),
        tolerance_ms: row.get("tolerance_ms"),
    }
}

pub async fn list_selectors(pool: &SqlitePool) -> AppResult<Vec<SelectorRecord>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECTOR_COLUMNS} FROM selectors ORDER BY tag ASC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_selector).collect())
}

pub async fn get_selector(pool: &SqlitePool, tag: &str) -> AppResult<Option<SelectorRecord>> {
    let row = sqlx::query(&format!(
        "SELECT {SELECTOR_COLUMNS} FROM selectors WHERE tag = ?1"
    ))
    .bind(tag)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_selector))
}

pub async fn upsert_selector(
    pool: &SqlitePool,
    input: &SelectorInput,
) -> AppResult<SelectorRecord> {
    input
        .validate()
        .map_err(|e| AppError::msg(format!("selector 配置无效: {e}")))?;

    let members = serde_json::to_string(&input.members)?;

    // 保留原有的 current_provider：改成员列表不该把用户当前的选择抹掉，
    // 是否失效交给 Selector::set_members 在内存里判断。
    sqlx::query(
        "INSERT INTO selectors (tag, name, mode, members, current_provider, tolerance_ms, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)
         ON CONFLICT(tag) DO UPDATE SET
             name = excluded.name,
             mode = excluded.mode,
             members = excluded.members,
             tolerance_ms = excluded.tolerance_ms,
             updated_at = excluded.updated_at",
    )
    .bind(&input.tag)
    .bind(&input.name)
    .bind(input.mode.as_str())
    .bind(&members)
    .bind(input.tolerance_ms)
    .bind(now_ms())
    .execute(pool)
    .await?;

    get_selector(pool, &input.tag)
        .await?
        .ok_or_else(|| AppError::msg("selector 写入后读取失败"))
}

pub async fn delete_selector(pool: &SqlitePool, tag: &str) -> AppResult<()> {
    let r = sqlx::query("DELETE FROM selectors WHERE tag = ?1")
        .bind(tag)
        .execute(pool)
        .await?;
    if r.rows_affected() == 0 {
        return Err(AppError::SelectorNotFound(tag.to_string()));
    }
    Ok(())
}

/// 持久化热切换的结果。这是「重启后仍记得切到了哪个渠道」的依据。
pub async fn persist_selection(
    pool: &SqlitePool,
    selector_tag: &str,
    provider_tag: &str,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE selectors SET current_provider = ?1, updated_at = ?2 WHERE tag = ?3",
    )
    .bind(provider_tag)
    .bind(now_ms())
    .bind(selector_tag)
    .execute(pool)
    .await?;
    Ok(())
}

/// 确保一个名为 `default` 的 selector 存在，其成员是所有已启用渠道。
///
/// 首次启动时没有任何配置，这一步让「新建渠道后无需再配置路由就能用」。
pub async fn ensure_default_selector(pool: &SqlitePool, providers: &[Provider]) -> AppResult<()> {
    if get_selector(pool, "default").await?.is_some() {
        return Ok(());
    }

    let members: Vec<String> = providers
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.tag.clone())
        .collect();

    upsert_selector(
        pool,
        &SelectorInput {
            tag: "default".into(),
            name: "默认".into(),
            mode: SelectorMode::Selector,
            members,
            tolerance_ms: 50,
        },
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::rule_item::LogicMode;
    use crate::storage::models::{AuthStyle, ProviderKind};
    use indexmap::IndexMap;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn provider(tag: &str) -> Provider {
        Provider {
            id: 1,
            tag: tag.into(),
            name: tag.into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://x".into(),
            api_key: None,
            auth_style: AuthStyle::XApiKey,
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn rule_input(name: &str, action: RouteAction) -> RouteRuleInput {
        RouteRuleInput {
            id: None,
            name: name.into(),
            enabled: true,
            items: vec![RuleItem::Model {
                patterns: vec!["claude-*".into()],
            }],
            action,
        }
    }

    // --- 规则 ---

    #[tokio::test]
    async fn create_and_list_rules() {
        let p = pool().await;
        let r = upsert_rule(&p, &rule_input("r1", RouteAction::Final { selector: "default".into() }))
            .await
            .unwrap();
        assert!(r.id > 0);
        assert_eq!(r.name, "r1");
        assert_eq!(r.items.len(), 1);

        let all = list_rules(&p).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].action, RouteAction::Final { selector: "default".into() });
    }

    #[tokio::test]
    async fn new_rules_append_to_end_in_order() {
        let p = pool().await;
        for n in ["a", "b", "c"] {
            upsert_rule(&p, &rule_input(n, RouteAction::Sniff)).await.unwrap();
        }
        let all = list_rules(&p).await.unwrap();
        assert_eq!(
            all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(all[0].sort_index < all[1].sort_index);
    }

    #[tokio::test]
    async fn update_rule_preserves_id_and_sort_index() {
        let p = pool().await;
        let created = upsert_rule(&p, &rule_input("r1", RouteAction::Sniff)).await.unwrap();

        let mut upd = rule_input("r1-renamed", RouteAction::Final { selector: "s".into() });
        upd.id = Some(created.id);
        let updated = upsert_rule(&p, &upd).await.unwrap();

        assert_eq!(updated.id, created.id);
        assert_eq!(updated.name, "r1-renamed");
        assert_eq!(updated.sort_index, created.sort_index);
        assert_eq!(list_rules(&p).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_missing_rule_is_an_error() {
        let p = pool().await;
        let mut inp = rule_input("x", RouteAction::Sniff);
        inp.id = Some(999);
        assert!(upsert_rule(&p, &inp).await.is_err());
    }

    #[tokio::test]
    async fn delete_rule_works() {
        let p = pool().await;
        let r = upsert_rule(&p, &rule_input("r", RouteAction::Sniff)).await.unwrap();
        delete_rule(&p, r.id).await.unwrap();
        assert!(list_rules(&p).await.unwrap().is_empty());
        assert!(delete_rule(&p, r.id).await.is_err());
    }

    #[tokio::test]
    async fn reorder_rules_applies_given_order() {
        let p = pool().await;
        let mut ids = Vec::new();
        for n in ["a", "b", "c"] {
            ids.push(upsert_rule(&p, &rule_input(n, RouteAction::Sniff)).await.unwrap().id);
        }

        reorder_rules(&p, &[ids[2], ids[0], ids[1]]).await.unwrap();
        let all = list_rules(&p).await.unwrap();
        assert_eq!(
            all.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["c", "a", "b"]
        );
    }

    #[tokio::test]
    async fn reorder_puts_unlisted_rules_at_the_end() {
        let p = pool().await;
        let mut ids = Vec::new();
        for n in ["a", "b", "c"] {
            ids.push(upsert_rule(&p, &rule_input(n, RouteAction::Sniff)).await.unwrap().id);
        }

        // 只提到最后一个
        reorder_rules(&p, &[ids[2]]).await.unwrap();
        let all = list_rules(&p).await.unwrap();
        assert_eq!(all[0].name, "c");
        assert_eq!(all.len(), 3, "未列出的规则不能丢");
    }

    #[tokio::test]
    async fn rules_with_complex_nested_conditions_roundtrip() {
        let p = pool().await;
        let mut inp = rule_input("complex", RouteAction::RouteOptions {
            target_selector: Some("x".into()),
            cache: Some(true),
        });
        inp.items = vec![RuleItem::Logical {
            mode: LogicMode::Or,
            invert: true,
            rules: vec![
                RuleItem::Client { any: vec!["codex".into()] },
                RuleItem::TokenEstimate { min: Some(1000), max: None },
            ],
        }];

        let created = upsert_rule(&p, &inp).await.unwrap();
        let loaded = &list_rules(&p).await.unwrap()[0];
        assert_eq!(loaded.items, created.items);
        assert_eq!(loaded.action, created.action);
    }

    #[tokio::test]
    async fn invalid_rule_is_rejected_before_write() {
        let p = pool().await;
        let mut inp = rule_input("", RouteAction::Sniff);
        inp.name = "".into();
        assert!(upsert_rule(&p, &inp).await.is_err());
        assert!(list_rules(&p).await.unwrap().is_empty(), "不应写入任何东西");
    }

    #[tokio::test]
    async fn corrupt_rule_row_is_skipped_not_fatal() {
        let p = pool().await;
        upsert_rule(&p, &rule_input("good", RouteAction::Sniff)).await.unwrap();
        sqlx::query(
            "INSERT INTO route_rules (sort_index, name, enabled, items, action, updated_at)
             VALUES (99, 'bad', 1, 'not json', 'also not json', 0)",
        )
        .execute(&p)
        .await
        .unwrap();

        let all = list_rules(&p).await.unwrap();
        assert_eq!(all.len(), 1, "坏行应被跳过，好行仍可用");
        assert_eq!(all[0].name, "good");
    }

    // --- 兜底 selector ---

    #[tokio::test]
    async fn final_selector_defaults_to_default() {
        let p = pool().await;
        assert_eq!(final_selector(&p).await.unwrap(), "default");
    }

    #[tokio::test]
    async fn set_final_selector_persists() {
        let p = pool().await;
        set_final_selector(&p, "fast").await.unwrap();
        assert_eq!(final_selector(&p).await.unwrap(), "fast");

        // 幂等：重复设置仍只有一行
        set_final_selector(&p, "slow").await.unwrap();
        assert_eq!(final_selector(&p).await.unwrap(), "slow");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_config")
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    // --- Selector ---

    fn selector_input(tag: &str, members: &[&str]) -> SelectorInput {
        SelectorInput {
            tag: tag.into(),
            name: tag.into(),
            mode: SelectorMode::Selector,
            members: members.iter().map(|m| m.to_string()).collect(),
            tolerance_ms: 50,
        }
    }

    #[tokio::test]
    async fn create_and_read_selector() {
        let p = pool().await;
        let s = upsert_selector(&p, &selector_input("default", &["a", "b"]))
            .await
            .unwrap();
        assert_eq!(s.members, vec!["a", "b"]);
        assert!(s.current_provider.is_none());
        assert_eq!(s.mode, SelectorMode::Selector);
    }

    #[tokio::test]
    async fn update_selector_keeps_current_provider() {
        let p = pool().await;
        upsert_selector(&p, &selector_input("default", &["a", "b"])).await.unwrap();
        persist_selection(&p, "default", "b").await.unwrap();

        // 改名字和容差，成员不变
        let mut upd = selector_input("default", &["a", "b"]);
        upd.name = "主路由".into();
        upd.tolerance_ms = 200;
        upsert_selector(&p, &upd).await.unwrap();

        let s = get_selector(&p, "default").await.unwrap().unwrap();
        assert_eq!(s.name, "主路由");
        assert_eq!(s.current_provider.as_deref(), Some("b"), "当前选择不该被抹掉");
    }

    #[tokio::test]
    async fn persist_selection_is_readable_back() {
        let p = pool().await;
        upsert_selector(&p, &selector_input("s", &["x", "y"])).await.unwrap();
        persist_selection(&p, "s", "y").await.unwrap();

        let s = get_selector(&p, "s").await.unwrap().unwrap();
        assert_eq!(s.current_provider.as_deref(), Some("y"));
    }

    #[tokio::test]
    async fn selector_validation_rejects_duplicates_and_blanks() {
        let p = pool().await;
        assert!(upsert_selector(&p, &selector_input("", &["a"])).await.is_err());
        assert!(upsert_selector(&p, &selector_input("s", &["a", "a"])).await.is_err());
    }

    #[tokio::test]
    async fn delete_missing_selector_is_an_error() {
        let p = pool().await;
        assert!(delete_selector(&p, "nope").await.is_err());
    }

    #[tokio::test]
    async fn ensure_default_selector_is_idempotent() {
        let p = pool().await;
        let providers = vec![provider("a"), provider("b")];

        ensure_default_selector(&p, &providers).await.unwrap();
        ensure_default_selector(&p, &providers).await.unwrap();

        let all = list_selectors(&p).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].members, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn ensure_default_selector_does_not_overwrite_user_config() {
        let p = pool().await;
        let mut inp = selector_input("default", &["custom"]);
        inp.name = "用户改过的".into();
        upsert_selector(&p, &inp).await.unwrap();

        ensure_default_selector(&p, &[provider("a")]).await.unwrap();
        let s = get_selector(&p, "default").await.unwrap().unwrap();
        assert_eq!(s.name, "用户改过的");
        assert_eq!(s.members, vec!["custom"], "不应被自动配置覆盖");
    }
}
