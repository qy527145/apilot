//! 渠道（provider）的读写。

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

use super::models::{
    AuthStyle, ChannelProxy, ProtocolEndpoint, Provider, ProviderKind, ProviderModel,
};
use crate::error::{AppError, AppResult};
use crate::util::now_ms;

/// 新建 / 更新渠道的入参。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInput {
    /// 更新时必填；新建时忽略。
    pub id: Option<i64>,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "default_auth_style")]
    pub auth_style: AuthStyle,
    /// 该服务商支持哪些协议。空表示"只支持 `kind` 那一种"，与老数据行为一致。
    #[serde(default)]
    pub protocols: Vec<ProtocolEndpoint>,
    #[serde(default)]
    pub extra_headers: IndexMap<String, String>,
    #[serde(default)]
    pub param_override: Option<Value>,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_ms: i64,
    /// 该渠道走不走代理。省略表示跟随全局。
    #[serde(default)]
    pub proxy: ChannelProxy,
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
        if let Some(url) = self.proxy.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
            if !crate::upstream::client::is_supported_proxy_url(url) {
                return Err(format!(
                    "代理地址「{url}」不受支持，只能是 http:// 、https:// 或 socks5:// 开头"
                ));
            }
        }
        Ok(())
    }

}

/// 由名称摊出一个候选渠道标识：小写、非 ASCII 字母数字的字符转连字符、去掉空段。
///
/// 标识只在内部流动（selector 成员、注册表索引、`active_provider`、日志），
/// 界面既不显示也不接受输入，所以只要求"合法、稳定、能唯一"，不要求好看。
/// 名字全是非 ASCII 时（"智谱"）会摊成空串，退化成 `channel`；重名由
/// [`unique_tag`] 加序号解决。
fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "channel".into()
    } else {
        slug
    }
}

/// 给 `base` 找一个没被占用的标识：占了就试 `base-2`、`base-3`……
///
/// 存在的理由：用户能看到的只有名称，两条渠道同名（"DeepSeek 官方" 再建一条
/// 做成备用）是完全正常的配置。若让自动生成的标识直接撞唯一约束，用户看到的是
/// 一个自己既看不见、也改不了的字段在报错，而正确的做法是内部把序号补上。
async fn unique_tag(pool: &SqlitePool, base: &str) -> AppResult<String> {
    let taken: std::collections::HashSet<String> = sqlx::query_scalar("SELECT tag FROM providers")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();

    if !taken.contains(base) {
        return Ok(base.to_string());
    }
    // 序号从 2 开始：`x` 与 `x-2` 并存比 `x` 与 `x-1` 更符合直觉。
    for n in 2..10_000u32 {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(AppError::msg("渠道标识冲突，请重试"))
}

const SELECT_COLUMNS: &str = "id, tag, name, kind, base_url, api_key, auth_style, \
     protocols, extra_headers, param_override, model_mapping, weight, priority, enabled, \
     timeout_ms, proxy, created_at, updated_at";

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
        // 坏数据退化成空列表（= 只用 kind 那一种协议），不让一条脏 JSON 让整个渠道打不开。
        protocols: serde_json::from_str(&row.get::<String, _>("protocols")).unwrap_or_default(),
        extra_headers: json_map(row.get("extra_headers")),
        param_override: row
            .get::<Option<String>, _>("param_override")
            .and_then(|s| serde_json::from_str(&s).ok()),
        model_mapping: json_map(row.get("model_mapping")),
        weight: row.get("weight"),
        priority: row.get("priority"),
        enabled: row.get::<i64, _>("enabled") != 0,
        timeout_ms: row.get("timeout_ms"),
        // NULL / 坏 JSON 都退化成"跟随全局" —— 一条脏数据不该让渠道打不开。
        proxy: row
            .get::<Option<String>, _>("proxy")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
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
    let protocols = serde_json::to_string(&input.protocols)?;
    let proxy = serde_json::to_string(&input.proxy.clone().normalized())?;
    let param_override = match &input.param_override {
        Some(v) => Some(serde_json::to_string(v)?),
        None => None,
    };

    let id = match input.id {
        Some(id) => {
            // 留空 api_key 表示"不改动已存的密钥"，避免前端因为不回显密钥而把 key 抹掉。
            // **不写 tag**：它是 selector 成员、注册表索引与 `active_provider`
            // 共同引用的稳定标识。跟着名字重算的话，用户改一次名，这些引用就全指向
            // 一个不存在的渠道了 —— 界面上表现为"渠道莫名不参与路由"。
            let result = sqlx::query(
                "UPDATE providers SET name=?1, kind=?2, base_url=?3,
                     api_key = COALESCE(?4, api_key), auth_style=?5, protocols=?6,
                     extra_headers=?7, param_override=?8, weight=?9,
                     priority=?10, enabled=?11, timeout_ms=?12, updated_at=?13,
                     proxy=?14
                 WHERE id=?15",
            )
            .bind(&input.name)
            .bind(input.kind.as_str())
            .bind(&input.base_url)
            .bind(&input.api_key)
            .bind(input.auth_style.as_str())
            .bind(&protocols)
            .bind(&extra)
            .bind(&param_override)
            .bind(input.weight)
            .bind(input.priority)
            .bind(input.enabled as i64)
            .bind(input.timeout_ms)
            .bind(now)
            .bind(&proxy)
            .bind(id)
            .execute(pool)
            .await?;

            if result.rows_affected() == 0 {
                return Err(AppError::ProviderNotFound(id.to_string()));
            }
            id
        }
        None => {
            let tag = unique_tag(pool, &slugify(&input.name)).await?;
            let result = sqlx::query(
                "INSERT INTO providers (tag, name, kind, base_url, api_key, auth_style,
                     protocols, extra_headers, param_override, weight, priority,
                     enabled, timeout_ms, created_at, updated_at, proxy)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14,?15)",
            )
            .bind(&tag)
            .bind(&input.name)
            .bind(input.kind.as_str())
            .bind(&input.base_url)
            .bind(&input.api_key)
            .bind(input.auth_style.as_str())
            .bind(&protocols)
            .bind(&extra)
            .bind(&param_override)
            .bind(input.weight)
            .bind(input.priority)
            .bind(input.enabled as i64)
            .bind(input.timeout_ms)
            .bind(now)
            .bind(&proxy)
            .execute(pool)
            .await
            .map_err(|e| {
                // `unique_tag` 已经避开了一次性查出来的冲突，走到这里只剩
                // 「两次保存之间的窗口」这一种可能（同名单并发插入）。
                if e.to_string().contains("UNIQUE") {
                    AppError::msg(format!("渠道标识「{tag}」已被占用，请重试"))
                } else {
                    AppError::Db(e)
                }
            })?;
            result.last_insert_rowid()
        }
    };

    // `model_mapping` 是派生列，真源是 `provider_models`。这里不接收调用方传来的映射，
    // 而是照真源重算一遍 —— 否则编辑渠道对话框会拿它打开时的旧快照
    // 把别处刚改好的上游模型名覆盖回去。
    let mut conn = pool.acquire().await?;
    rebuild_model_mapping(&mut conn, id).await?;
    drop(conn);

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

/// 只翻转启用位。
///
/// 刻意不走 `upsert`：那条路要求调用方把 tag / base_url / 协议声明 / 密钥等一整套
/// 回传，而表格里的开关手上只有列表刷新出来的那几列 —— **密钥根本不回显**。
/// 少传一项就是一次静默的数据丢失：关一下渠道，密钥或协议声明被抹成默认值，
/// 而且不会有任何报错，用户下次发请求才发现。
pub async fn set_enabled(pool: &SqlitePool, id: i64, enabled: bool) -> AppResult<()> {
    let r = sqlx::query("UPDATE providers SET enabled = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(enabled as i64)
        .bind(now_ms())
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

/// 把 `providers.model_mapping` 从该渠道的 `provider_models` 行重新派生一遍。
///
/// 「入站名 → 上游名」那份派生读模型是请求改写（`Provider::upstream_model`）与
/// 网关 `GET /v1/models` 的依据，而 `provider_models` 才是真源。
/// **凡改动后者的地方都必须调它** —— 漏调的后果是"模型页显示映射生效了，
/// 实际请求仍按原名发出去"，而且不会报错。
///
/// 同名条目也写进 mapping（不是只写有差异的）：`/v1/models` 广告的就是
/// `model_mapping.keys()`，只写差异项会让纯 DeepSeek 这类"入站名 == 上游名"
/// 的配置探测不到任何模型。
async fn rebuild_model_mapping(
    conn: &mut sqlx::SqliteConnection,
    provider_id: i64,
) -> AppResult<()> {
    let rows = sqlx::query(
        "SELECT model, upstream_model FROM provider_models WHERE provider_id = ?1 ORDER BY model ASC",
    )
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut mapping: IndexMap<String, String> = IndexMap::new();
    for r in &rows {
        let model: String = r.get("model");
        let upstream: Option<String> = r.get("upstream_model");
        mapping.insert(model.clone(), upstream.unwrap_or(model));
    }

    let json = serde_json::to_string(&mapping)?;
    sqlx::query("UPDATE providers SET model_mapping = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(&json)
        .bind(now_ms())
        .bind(provider_id)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

/// 全量替换某渠道**声明提供**的模型（渠道视角：这个渠道支持哪些模型）。
///
/// 只写"有哪些模型"，不碰上游名 —— 面板上那一列已经去掉了，上游名归模型页管
/// （`set_model_candidates`）。已有的上游名按模型名原样保留，否则在渠道面板
/// 点一次保存就会把别处配好的重定向全抹掉。
///
/// 写的优先级/权重直接取该渠道自己的值 —— 每模型的 priority/weight 是
/// `channels_for_model` 的排序依据，默认应当等于渠道级的值，
/// 否则"没单独配过"的模型会因为 0/1 的默认值被排到别的渠道后面。
pub async fn set_models(
    pool: &SqlitePool,
    provider_id: i64,
    models: &[String],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    let (base_priority, base_weight): (i64, i64) = sqlx::query_as(
        "SELECT priority, weight FROM providers WHERE id = ?1",
    )
    .bind(provider_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::ProviderNotFound(provider_id.to_string()))?;

    // 删之前先把**不归这个入口管**的列捞出来：上游名和启用状态都归模型页
    // （`set_model_candidates`）。按渠道全量重插时若不还回去，
    // 在渠道面板点一次保存就会把别处配好的东西抹掉。
    let existing: std::collections::HashMap<String, (Option<String>, i64)> = sqlx::query(
        "SELECT model, upstream_model, enabled FROM provider_models WHERE provider_id = ?1",
    )
    .bind(provider_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|r| (r.get("model"), (r.get("upstream_model"), r.get("enabled"))))
    .collect();

    sqlx::query("DELETE FROM provider_models WHERE provider_id = ?1")
        .bind(provider_id)
        .execute(&mut *tx)
        .await?;

    for model in models {
        let (upstream, enabled) = existing
            .get(model)
            .cloned()
            .unwrap_or((None, 1));

        sqlx::query(
            "INSERT INTO provider_models (provider_id, model, upstream_model, client_group,
                 priority, weight, enabled)
             VALUES (?1, ?2, ?3, '*', ?4, ?5, ?6)",
        )
        .bind(provider_id)
        .bind(model)
        .bind(upstream)
        .bind(base_priority)
        .bind(base_weight)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
    }

    rebuild_model_mapping(&mut *tx, provider_id).await?;
    tx.commit().await?;
    Ok(())
}

/// 一条候选渠道（模型视角：这个模型在某个渠道上叫什么、排第几）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCandidateRow {
    pub provider_tag: String,
    pub upstream_model: Option<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_weight")]
    pub weight: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// 按**模型**维度写候选渠道。
///
/// 与 [`set_models`] 是同一张表的两个视角：那边是"这个渠道提供哪些模型"，
/// 这边是"这个模型在哪些渠道上有"。两者都全量替换自己负责的那一维，
/// 最后都由 `rebuild_model_mapping` 把派生列修回来，所以不会互相覆盖。
pub async fn set_model_candidates(
    pool: &SqlitePool,
    model: &str,
    rows: &[ModelCandidateRow],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;

    // 先记下所有涉及到的渠道：既要删旧的，也要（在改完后）重建派生列。
    let mut touched: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT provider_id FROM provider_models WHERE model = ?1",
    )
    .bind(model)
    .fetch_all(&mut *tx)
    .await?;

    sqlx::query("DELETE FROM provider_models WHERE model = ?1")
        .bind(model)
        .execute(&mut *tx)
        .await?;

    for row in rows {
        let provider_id: i64 = sqlx::query_scalar("SELECT id FROM providers WHERE tag = ?1")
            .bind(&row.provider_tag)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::ProviderNotFound(row.provider_tag.clone()))?;

        sqlx::query(
            "INSERT INTO provider_models (provider_id, model, upstream_model, client_group,
                 priority, weight, enabled)
             VALUES (?1, ?2, ?3, '*', ?4, ?5, ?6)",
        )
        .bind(provider_id)
        .bind(model)
        .bind(&row.upstream_model)
        .bind(row.priority)
        .bind(row.weight)
        .bind(row.enabled as i64)
        .execute(&mut *tx)
        .await?;

        if !touched.contains(&provider_id) {
            touched.push(provider_id);
        }
    }

    // 增删都动过 `provider_models`，所以每个涉及的渠道都要重建。
    for provider_id in touched {
        rebuild_model_mapping(&mut *tx, provider_id).await?;
    }

    tx.commit().await?;
    Ok(())
}

/// 某个模型在所有渠道上的候选，**含被停用的**。
///
/// 与 [`candidate_channels`] 是两种用途，别合并：
/// - 那个给路由用，只返回启用中的渠道，且按优先级排好序；
/// - 这个给模型页展示用，必须把停用的也带出来 —— 否则用户停用一个渠道之后
///   它在模型页里直接消失，就再也启用不回来了。
pub async fn candidates_for_model(
    pool: &SqlitePool,
    model: &str,
) -> AppResult<Vec<ProviderModelCandidate>> {
    let rows = sqlx::query(
        "SELECT p.tag, p.name, m.upstream_model, m.priority, m.weight,
                (m.enabled = 1 AND p.enabled = 1) AS effective_enabled
         FROM provider_models m
         JOIN providers p ON p.id = m.provider_id
         WHERE m.model = ?1
         ORDER BY m.priority DESC, m.weight DESC, p.id ASC",
    )
    .bind(model)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| ProviderModelCandidate {
            provider_tag: r.get("tag"),
            provider_name: r.get("name"),
            upstream_model: r.get("upstream_model"),
            priority: r.get("priority"),
            weight: r.get("weight"),
            // 渠道停用或这一条被停用，都算不可用 —— 界面只关心"它现在会不会被选中"。
            enabled: r.get::<i64, _>("effective_enabled") != 0,
        })
        .collect())
}

/// 模型页看到的一条候选渠道（读模型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelCandidate {
    pub provider_tag: String,
    pub provider_name: String,
    pub upstream_model: Option<String>,
    pub priority: i64,
    pub weight: i64,
    pub enabled: bool,
}

/// 所有被声明过的模型名（去重、排序）。
pub async fn all_declared_models(pool: &SqlitePool) -> AppResult<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar("SELECT DISTINCT model FROM provider_models")
        .fetch_all(pool)
        .await?;
    let mut models = rows;
    models.sort();
    models.dedup();
    Ok(models)
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
/// 候选渠道 + 它在**这个模型上**的优先级/权重。
///
/// 排序要用的是每模型的值（模型页调的就是它们），而 `Provider` 上带的是渠道级的
/// 那两个，取值口径不同，所以必须一起返回，不能让调用方自己去猜。
#[derive(Debug, Clone)]
pub struct CandidateChannel {
    pub provider: Provider,
    pub priority: i64,
    pub weight: i64,
}

/// 提供该模型的启用渠道，按优先级排好序。
///
/// 排序用的是 **`provider_models` 上的每模型 priority/weight**（迁移 v4 已把
/// 既有行按渠道的值回填了一次，所以老配置的顺序与改动前一致），
/// 没声明过模型的"通吃"渠道回落到渠道自身的值。
///
/// 子查询而不是 JOIN：`SELECT_COLUMNS` 里的列名没有限定前缀，与 `provider_models`
/// 一起查会撞上同名的 `id`。
pub async fn candidate_channels(
    pool: &SqlitePool,
    model: &str,
) -> AppResult<Vec<CandidateChannel>> {
    let rows = sqlx::query(&format!(
        "SELECT {SELECT_COLUMNS},
                COALESCE(
                  (SELECT m.priority FROM provider_models m
                   WHERE m.provider_id = p.id AND m.model = ?1 AND m.enabled = 1),
                  p.priority) AS eff_priority,
                COALESCE(
                  (SELECT m.weight FROM provider_models m
                   WHERE m.provider_id = p.id AND m.model = ?1 AND m.enabled = 1),
                  p.weight) AS eff_weight
         FROM providers p
         WHERE p.enabled = 1
           AND (
             NOT EXISTS (SELECT 1 FROM provider_models m WHERE m.provider_id = p.id)
             OR EXISTS (
               SELECT 1 FROM provider_models m
               WHERE m.provider_id = p.id AND m.model = ?1 AND m.enabled = 1
             )
           )
         ORDER BY eff_priority DESC, eff_weight DESC, p.id ASC"
    ))
    .bind(model)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| CandidateChannel {
            provider: row_to_provider(r),
            priority: r.get("eff_priority"),
            weight: r.get("eff_weight"),
        })
        .collect())
}

/// 提供该模型的启用渠道（只要渠道本身）。
pub async fn channels_for_model(pool: &SqlitePool, model: &str) -> AppResult<Vec<Provider>> {
    Ok(candidate_channels(pool, model)
        .await?
        .into_iter()
        .map(|c| c.provider)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::ChannelProxyMode;

    /// 渠道标识（tag）改由名称自动生成，所以这里的参数就是**名称**；
    /// 传的都是小写 ASCII，摊出来的 slug 与它逐字相同，下面按 tag 的断言据此成立。
    fn input(name: &str) -> ProviderInput {
        ProviderInput {
            id: None,
            name: name.into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            api_key: Some("sk-test".into()),
            auth_style: AuthStyle::XApiKey,
            protocols: Vec::new(),
            extra_headers: IndexMap::new(),
            param_override: None,
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            proxy: Default::default(),
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
    async fn same_name_channels_are_saved_with_distinct_tags() {
        let p = pool().await;
        let first = upsert(&p, &input("dup")).await.unwrap();
        let second = upsert(&p, &input("dup")).await.unwrap();

        assert_eq!(first.tag, "dup");
        assert_eq!(second.tag, "dup-2", "同名渠道不该被内部标识挡回去");
        assert_eq!(second.name, "dup", "名称照旧，用户看到的就是它");
    }

    #[tokio::test]
    async fn a_name_without_usable_ascii_still_gets_a_tag() {
        let p = pool().await;
        let mut inp = input("x");
        inp.name = "智谱 GLM".into();
        let created = upsert(&p, &inp).await.unwrap();
        assert_eq!(created.tag, "glm");
    }

    #[tokio::test]
    async fn renaming_a_channel_keeps_its_tag() {
        let p = pool().await;
        let created = upsert(&p, &input("stable")).await.unwrap();

        let mut upd = input("stable");
        upd.id = Some(created.id);
        upd.name = "换个名字".into();
        let after = upsert(&p, &upd).await.unwrap();

        assert_eq!(after.tag, "stable", "改名不该动标识：selector 与手动切换都指着它");
    }

    #[tokio::test]
    async fn validation_rejects_bad_input() {
        let p = pool().await;

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
        set_models(&p, c.id, &["m1".into()]).await.unwrap();

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
        inp.param_override = Some(serde_json::json!({ "temperature": 0.2 }));

        let created = upsert(&p, &inp).await.unwrap();
        assert_eq!(created.extra_headers.get("X-Custom").unwrap(), "v");
        assert_eq!(created.param_override.unwrap()["temperature"], 0.2);
    }

    #[tokio::test]
    async fn upsert_rebuilds_model_mapping_from_the_real_source() {
        // model_mapping 是派生列。编辑渠道时若把它当真值写回去，
        // 对话框里的旧快照会盖掉模型页刚配好的重定向。
        let p = pool().await;
        let c = upsert(&p, &input("derived")).await.unwrap();
        set_model_candidates(&p, "alias", &[cand("derived", Some("up"), 0)])
            .await
            .unwrap();

        let mut edit = input("derived");
        edit.id = Some(c.id);
        edit.name = "改个名字".into();
        let after = upsert(&p, &edit).await.unwrap();

        assert_eq!(
            after.upstream_model("alias"),
            "up",
            "改渠道不能把它派生的映射冲掉"
        );
    }

    #[tokio::test]
    async fn channels_for_model_prefers_declared_then_falls_back_to_wildcard() {
        let p = pool().await;

        // c1 只声明支持 model-a
        let c1 = upsert(&p, &input("c1")).await.unwrap();
        set_models(&p, c1.id, &["model-a".into()]).await.unwrap();

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
            // 声明过的模型都进 mapping —— /v1/models 广告的就是它的 keys。
            // 没配过重定向的名，映射成同名。
            &["claude-sonnet-4-5".into(), "deepseek-reasoner".into()],
        )
        .await
        .unwrap();

        let after = get(&p, c.id).await.unwrap().unwrap();
        assert_eq!(after.upstream_model("claude-sonnet-4-5"), "claude-sonnet-4-5");
        assert_eq!(after.upstream_model("deepseek-reasoner"), "deepseek-reasoner");
        assert_eq!(after.model_mapping.len(), 2);
    }

    #[tokio::test]
    async fn set_models_keeps_upstream_names_set_from_the_model_page() {
        // 渠道面板只管"支持哪些模型"，上游名和单模型的启用状态都归模型页管。
        // 它保存时把这些冲成默认值的话，用户配好的东西会莫名其妙消失。
        let p = pool().await;
        let c = upsert(&p, &input("keep")).await.unwrap();

        let mut off = cand("keep", Some("deepseek-chat"), 0);
        off.enabled = false;
        set_model_candidates(&p, "claude-sonnet-4-5", &[off]).await.unwrap();

        // 面板里再加一个模型后保存。
        set_models(
            &p,
            c.id,
            &["claude-sonnet-4-5".into(), "deepseek-reasoner".into()],
        )
        .await
        .unwrap();

        let after = get(&p, c.id).await.unwrap().unwrap();
        assert_eq!(
            after.upstream_model("claude-sonnet-4-5"),
            "deepseek-chat",
            "已有的上游名必须原样保留"
        );
        assert_eq!(after.upstream_model("deepseek-reasoner"), "deepseek-reasoner");

        let rows = candidates_for_model(&p, "claude-sonnet-4-5").await.unwrap();
        assert!(!rows[0].enabled, "已有的启用状态也必须原样保留");

        // 新加的那行则按默认值来。
        let rows = candidates_for_model(&p, "deepseek-reasoner").await.unwrap();
        assert!(rows[0].enabled, "没配过的模型默认启用");
    }

    #[tokio::test]
    async fn set_models_with_empty_list_restores_wildcard() {
        let p = pool().await;
        let c = upsert(&p, &input("clear")).await.unwrap();
        set_models(&p, c.id, &["m".into()]).await.unwrap();

        // 清空声明 == 恢复"通吃"，这也是用户撤销操作的路径。
        set_models(&p, c.id, &[]).await.unwrap();

        let after = get(&p, c.id).await.unwrap().unwrap();
        assert!(after.model_mapping.is_empty());
        assert_eq!(channels_for_model(&p, "随便什么").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_models_on_missing_provider_is_an_error() {
        let p = pool().await;
        assert!(set_models(&p, 999, &["m".into()]).await.is_err());
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

        set_models(&p, c.id, &["m".into()]).await.unwrap();
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

    // -----------------------------------------------------------------------
    // 按模型维度选渠道
    // -----------------------------------------------------------------------

    async fn add(p: &SqlitePool, tag: &str, priority: i64) -> i64 {
        let mut i = input(tag);
        i.priority = priority;
        upsert(p, &i).await.unwrap().id
    }

    fn cand(tag: &str, upstream: Option<&str>, priority: i64) -> ModelCandidateRow {
        ModelCandidateRow {
            provider_tag: tag.into(),
            upstream_model: upstream.map(String::from),
            priority,
            weight: 1,
            enabled: true,
        }
    }

    #[tokio::test]
    async fn set_model_candidates_writes_across_providers() {
        let p = pool().await;
        add(&p, "a", 0).await;
        add(&p, "b", 0).await;

        set_model_candidates(
            &p,
            "m",
            &[cand("a", Some("up-a"), 10), cand("b", None, 5)],
        )
        .await
        .unwrap();

        let got = candidates_for_model(&p, "m").await.unwrap();
        assert_eq!(got.len(), 2);
        // 按优先级降序返回。
        assert_eq!(got[0].provider_tag, "a");
        assert_eq!(got[0].upstream_model.as_deref(), Some("up-a"));
        assert_eq!(got[1].provider_tag, "b");
        assert_eq!(got[1].upstream_model, None, "留空表示与入站名同名");
    }

    #[tokio::test]
    async fn set_model_candidates_rebuilds_every_affected_providers_mapping() {
        // model_mapping 是派生读模型：请求改写与 GET /v1/models 都读它。
        // 漏了重建的表现是"模型页显示改好了，实际仍按原名发出去"，且不报错。
        let p = pool().await;
        add(&p, "a", 0).await;
        add(&p, "b", 0).await;

        set_model_candidates(&p, "m", &[cand("a", None, 0), cand("b", None, 0)])
            .await
            .unwrap();
        for tag in ["a", "b"] {
            let provider = get_by_tag(&p, tag).await.unwrap().unwrap();
            assert!(
                provider.model_mapping.contains_key("m"),
                "{tag} 的派生映射应包含 m"
            );
        }

        // 把 b 从这个模型上摘掉，它的映射要跟着少掉 m。
        set_model_candidates(&p, "m", &[cand("a", None, 0)])
            .await
            .unwrap();

        let a = get_by_tag(&p, "a").await.unwrap().unwrap();
        let b = get_by_tag(&p, "b").await.unwrap().unwrap();
        assert!(a.model_mapping.contains_key("m"));
        assert!(
            !b.model_mapping.contains_key("m"),
            "被摘掉 m 的渠道不该还留着这条映射"
        );
    }

    #[tokio::test]
    async fn set_model_candidates_touches_only_the_named_model() {
        // 是全量替换**这一个模型**的候选，不能顺手把同渠道的别的模型删掉。
        let p = pool().await;
        add(&p, "a", 0).await;

        set_model_candidates(&p, "m1", &[cand("a", None, 0)]).await.unwrap();
        set_model_candidates(&p, "m2", &[cand("a", None, 0)]).await.unwrap();
        set_model_candidates(&p, "m1", &[]).await.unwrap();

        assert!(candidates_for_model(&p, "m1").await.unwrap().is_empty());
        assert_eq!(candidates_for_model(&p, "m2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_model_candidates_rejects_an_unknown_provider() {
        let p = pool().await;
        let err = set_model_candidates(&p, "m", &[cand("nope", None, 0)]).await;
        assert!(err.is_err());
        // 事务回滚：不该留下半条。
        assert!(candidates_for_model(&p, "m").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn channels_for_model_orders_by_the_per_model_priority() {
        let p = pool().await;
        // 渠道自身的优先级是 a > b，但模型页把 m 在 b 上排得更靠前。
        add(&p, "a", 10).await;
        add(&p, "b", 1).await;

        set_model_candidates(&p, "m", &[cand("a", None, 1), cand("b", None, 9)])
            .await
            .unwrap();

        let order: Vec<String> = channels_for_model(&p, "m")
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.tag)
            .collect();
        assert_eq!(order, vec!["b", "a"], "模型的优先级应盖过渠道自身的");
    }

    #[tokio::test]
    async fn channels_for_model_keeps_provider_order_for_undeclared_ones() {
        // 没声明过模型的"通吃"渠道回落到渠道自身的优先级。
        let p = pool().await;
        add(&p, "wild-low", 1).await;
        add(&p, "wild-high", 9).await;

        let order: Vec<String> = channels_for_model(&p, "anything")
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.tag)
            .collect();
        assert_eq!(order, vec!["wild-high", "wild-low"]);
    }

    #[tokio::test]
    async fn set_models_inherits_the_provider_priority() {
        // 渠道视角写模型时，每模型的值要取渠道自己的 ——
        // 否则新声明的模型会带着 0/1 的默认值被排到别的渠道后面。
        let p = pool().await;
        add(&p, "a", 42).await;

        set_models(&p, 1, &["m".into()]).await.unwrap();

        let rows = candidates_for_model(&p, "m").await.unwrap();
        assert_eq!(rows[0].priority, 42, "没单独配过的模型应继承渠道的优先级");
    }

    #[tokio::test]
    async fn all_declared_models_is_the_distinct_sorted_union() {
        let p = pool().await;
        add(&p, "a", 0).await;
        add(&p, "b", 0).await;

        set_model_candidates(&p, "z", &[cand("a", None, 0)]).await.unwrap();
        set_model_candidates(&p, "a", &[cand("a", None, 0), cand("b", None, 0)])
            .await
            .unwrap();

        assert_eq!(all_declared_models(&p).await.unwrap(), vec!["a", "z"]);
    }

    // -----------------------------------------------------------------------
    // 代理
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_new_channel_inherits_the_global_proxy() {
        let p = pool().await;
        let created = upsert(&p, &input("a")).await.unwrap();
        assert_eq!(created.proxy, ChannelProxy::default());
    }

    #[tokio::test]
    async fn the_channel_proxy_roundtrips_through_the_database() {
        let p = pool().await;
        let mut i = input("a");
        i.proxy = ChannelProxy {
            mode: ChannelProxyMode::Manual,
            url: Some("socks5://127.0.0.1:1080".into()),
        };
        let created = upsert(&p, &i).await.unwrap();
        assert_eq!(created.proxy.mode, ChannelProxyMode::Manual);

        let read = get(&p, created.id).await.unwrap().unwrap();
        assert_eq!(
            read.proxy.url.as_deref(),
            Some("socks5://127.0.0.1:1080"),
            "重新读出来必须还是那个代理，否则重启后渠道会悄悄改走直连"
        );
    }

    #[tokio::test]
    async fn an_update_can_switch_a_channel_back_to_direct() {
        let p = pool().await;
        let mut i = input("a");
        i.proxy = ChannelProxy {
            mode: ChannelProxyMode::Direct,
            url: None,
        };
        let created = upsert(&p, &i).await.unwrap();

        let mut upd = input("a");
        upd.id = Some(created.id);
        upd.proxy = ChannelProxy::default();
        let saved = upsert(&p, &upd).await.unwrap();
        assert_eq!(saved.proxy.mode, ChannelProxyMode::Inherit);
    }

    #[tokio::test]
    async fn an_unsupported_proxy_scheme_is_rejected_before_it_reaches_the_database() {
        // 拖到构造客户端才发现写错，表现是"请求全失败"而没有任何指向配置的提示。
        let p = pool().await;
        let mut i = input("a");
        i.proxy = ChannelProxy {
            mode: ChannelProxyMode::Manual,
            url: Some("ftp://127.0.0.1:21".into()),
        };
        let err = upsert(&p, &i).await.unwrap_err().to_string();
        assert!(err.contains("代理地址"), "错误要指明是代理写错了: {err}");
    }

    #[tokio::test]
    async fn a_row_predating_the_proxy_column_reads_as_inherit() {
        // 迁移只加列不回填，老库里的行是 NULL。
        let p = pool().await;
        let created = upsert(&p, &input("a")).await.unwrap();
        sqlx::query("UPDATE providers SET proxy = NULL WHERE id = ?1")
            .bind(created.id)
            .execute(&p)
            .await
            .unwrap();

        let read = get(&p, created.id).await.unwrap().unwrap();
        assert_eq!(read.proxy.mode, ChannelProxyMode::Inherit);
    }

    #[tokio::test]
    async fn toggling_enabled_touches_nothing_but_the_flag() {
        // 开关走的是独立的 UPDATE 而不是 upsert：后者要求把密钥、协议声明等整套
        // 回传，而前端手上没有（密钥不回显），少传一项就是一次静默的数据丢失。
        let p = pool().await;
        let mut i = input("a");
        i.protocols = vec![ProtocolEndpoint {
            protocol: crate::protocol::dto::Protocol::AnthropicMessages,
            path: Some("/anthropic/v1/messages".into()),
        }];
        let created = upsert(&p, &i).await.unwrap();

        set_enabled(&p, created.id, false).await.unwrap();

        let read = get(&p, created.id).await.unwrap().unwrap();
        assert!(!read.enabled);
        assert_eq!(read.api_key.as_deref(), Some("sk-test"), "密钥不能被抹掉");
        assert_eq!(read.protocols, created.protocols, "协议声明不能被重置");
        assert_eq!(read.base_url, created.base_url);
        assert_eq!(read.weight, created.weight);
    }

    #[tokio::test]
    async fn a_disabled_channel_drops_out_of_list_enabled() {
        // 网关的注册表读的是 list_enabled：停用必须就此从路由里消失，
        // 而列表页仍要看得到它（否则用户没法把它再打开）。
        let p = pool().await;
        let created = upsert(&p, &input("a")).await.unwrap();
        assert_eq!(list_enabled(&p).await.unwrap().len(), 1);

        set_enabled(&p, created.id, false).await.unwrap();
        assert!(list_enabled(&p).await.unwrap().is_empty());
        assert_eq!(list(&p).await.unwrap().len(), 1);

        set_enabled(&p, created.id, true).await.unwrap();
        assert_eq!(list_enabled(&p).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn toggling_an_unknown_channel_errors_instead_of_lying() {
        // 静默成功会让界面停留在一个并不存在的变化上（乐观更新已经先改了本地状态）。
        let p = pool().await;
        let err = set_enabled(&p, 999, false).await.unwrap_err().to_string();
        assert!(err.contains("999"), "错误要指名道姓: {err}");
    }
}
