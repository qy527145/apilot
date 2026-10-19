//! 按对话聚合请求日志。
//!
//! 落库时只存了原始事实（会话 ID、线指纹、轮数、带没带工具），「哪条是主线」
//! 「哪里压缩过」都在这里现算 —— 理由见 v11 迁移的注释：这两样要看整个会话
//! 才定得下来，存下来会随会话往后走而过时。

use std::collections::HashMap;

use serde::Serialize;
use sqlx::{QueryBuilder, Row, SqlitePool};

use crate::error::AppResult;
use crate::traffic::session::{classify_threads, is_compaction, ThreadKind, ThreadStat};

/// 会话列表里的一行。
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub client: String,
    pub first_ts: i64,
    pub last_ts: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub quota: i64,
    /// 这个对话的标题（用户第一条消息的第一句话）。老会话或取不出时为 `None`，
    /// 界面回落到 `客户端 #会话ID`。来源见 `traffic::session::first_user_line`。
    pub title: Option<String>,
    /// 会话里用到过的生效模型，按首次出现排序。多个就说明中途换过模型。
    pub models: Vec<String>,
    /// 这个对话被固定到的模型。由命令层从内存快照里填，这里不查。
    pub pin: Option<crate::routing::session_pin::SessionPin>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct SessionFilter {
    pub client: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: i64,
}

fn default_limit() -> i64 {
    100
}

/// 最近活跃的会话。按最后一条请求的时间倒序。
pub async fn list(pool: &SqlitePool, f: &SessionFilter) -> AppResult<Vec<SessionSummary>> {
    // 标题 JOIN 进来：`MAX(t.title)` 只是因为分组里所有行同值、SQLite 又不接受裸列，
    // 不是在做"取某一行"。会话没有标题（老数据）时是 NULL，界面自己回落。
    let mut qb: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
        "SELECT l.session_id AS session_id, MAX(t.title) AS title, \
                MIN(l.client) AS client, MIN(l.ts) AS first_ts, MAX(l.ts) AS last_ts, \
                COUNT(*) AS requests, \
                SUM(CASE WHEN l.status_code >= 400 THEN 1 ELSE 0 END) AS errors, \
                SUM(l.input_tokens) AS input_tokens, SUM(l.output_tokens) AS output_tokens, \
                SUM(l.cache_read_tokens) AS cache_read_tokens, SUM(l.quota) AS quota \
         FROM request_logs l LEFT JOIN session_titles t ON t.session_id = l.session_id \
         WHERE l.session_id IS NOT NULL",
    );
    if let Some(c) = &f.client {
        qb.push(" AND l.client = ").push_bind(c.clone());
    }
    if let Some(from) = f.from {
        qb.push(" AND l.ts >= ").push_bind(from);
    }
    if let Some(to) = f.to {
        qb.push(" AND l.ts <= ").push_bind(to);
    }
    qb.push(" GROUP BY l.session_id ORDER BY last_ts DESC LIMIT ")
        .push_bind(f.limit.clamp(1, 1000));

    let rows = qb.build().fetch_all(pool).await?;
    let mut out: Vec<SessionSummary> = rows
        .iter()
        .map(|r| SessionSummary {
            session_id: r.get("session_id"),
            client: r.get("client"),
            first_ts: r.get("first_ts"),
            last_ts: r.get("last_ts"),
            requests: r.get("requests"),
            errors: r.get("errors"),
            input_tokens: r.get("input_tokens"),
            output_tokens: r.get("output_tokens"),
            cache_read_tokens: r.get("cache_read_tokens"),
            quota: r.get("quota"),
            title: r.get("title"),
            models: Vec::new(),
            pin: None,
        })
        .collect();

    // 模型列表单独取：GROUP_CONCAT(DISTINCT) 不保序，而"先用 A 后换 B"的顺序正是要看的。
    //
    // 只数**对话本身**用的模型：带工具的那几条线（主线、子代理）。旁路（起标题）
    // 和 count_tokens 是客户端自己挑的小模型 —— 把它们算进来，一个固定了模型的对话
    // 也会显示"换过模型"，用户会以为固定没生效。会话里一条带工具的都没有（纯聊天
    // 客户端）时，退回数全部对话请求，不然列表就空了。
    if !out.is_empty() {
        let mut qb: QueryBuilder<sqlx::Sqlite> = QueryBuilder::new(
            "SELECT l.session_id, l.model, MIN(l.ts) AS first FROM request_logs l \
             WHERE l.model != '' AND l.thread_key IS NOT NULL \
               AND (l.has_tools = 1 OR NOT EXISTS ( \
                     SELECT 1 FROM request_logs t \
                     WHERE t.session_id = l.session_id AND t.has_tools = 1 AND t.thread_key IS NOT NULL)) \
               AND l.session_id IN (",
        );
        let mut sep = qb.separated(", ");
        for s in &out {
            sep.push_bind(s.session_id.clone());
        }
        qb.push(") GROUP BY l.session_id, l.model ORDER BY first");
        let mut models: HashMap<String, Vec<String>> = HashMap::new();
        for r in qb.build().fetch_all(pool).await? {
            models
                .entry(r.get("session_id"))
                .or_default()
                .push(r.get("model"));
        }
        for s in &mut out {
            s.models = models.remove(&s.session_id).unwrap_or_default();
        }
    }
    Ok(out)
}

/// 记下这个对话的标题。
///
/// **谁先到谁说话**（`INSERT OR IGNORE`）：同一个会话的每一条请求里都还带着那句
/// 开场白，所以正常情况下先落的那条就是对的；反过来「后来者覆盖」会在上下文压缩
/// 之后把标题改成摘要，那才是错的。取不出标题的请求（起标题任务、只有图）不写。
pub async fn save_title(pool: &SqlitePool, session_id: &str, title: &str, ts: i64) -> AppResult<()> {
    sqlx::query("INSERT OR IGNORE INTO session_titles (session_id, title, ts) VALUES (?1, ?2, ?3)")
        .bind(session_id)
        .bind(title)
        .bind(ts)
        .execute(pool)
        .await?;
    Ok(())
}

/// 时间线上的一条请求。
#[derive(Debug, Clone, Serialize)]
pub struct TimelineEntry {
    pub request_id: String,
    pub ts: i64,
    /// `None`：不是对话的一轮（`count_tokens` 等），在链路上展示但不属于任何线。
    pub thread_key: Option<String>,
    pub model: String,
    pub request_model: String,
    pub path: String,
    pub status_code: i32,
    pub error_message: Option<String>,
    pub message_count: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub quota: i64,
    pub latency_ms: i64,
    /// 这条请求相对同一条线的上一条，消息数骤降 —— 上下文在这之前被压缩过。
    pub compacted: bool,
    /// 压缩前那一条带了多少轮。只在 `compacted` 时有值。
    pub compacted_from: Option<i64>,
    /// 被会话固定改写之前的模型名。没被改写过就是 `None`。
    pub pinned_from: Option<String>,
}

/// 会话里的一条线。
#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummary {
    pub thread_key: String,
    pub kind: ThreadKind,
    pub requests: i64,
    pub first_ts: i64,
    pub last_ts: i64,
    /// 这条线上被压缩过几次。
    pub compactions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionTimeline {
    pub session_id: String,
    /// 同 `SessionSummary::title`。
    pub title: Option<String>,
    pub threads: Vec<ThreadSummary>,
    /// 全部请求，按时间正序。
    pub entries: Vec<TimelineEntry>,
    /// 同 `SessionSummary::pin`。
    pub pin: Option<crate::routing::session_pin::SessionPin>,
}

/// 一个会话的完整链路。
pub async fn timeline(pool: &SqlitePool, session_id: &str) -> AppResult<SessionTimeline> {
    let rows = sqlx::query(
        "SELECT request_id, ts, thread_key, model, request_model, path, \
                status_code, error_message, message_count, has_tools, input_tokens, output_tokens, \
                cache_read_tokens, quota, latency_ms, \
                CASE WHEN json_valid(other) THEN json_extract(other, '$.session_pin.from') END AS pinned_from \
         FROM request_logs WHERE session_id = ?1 ORDER BY ts ASC, request_id ASC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    // 每条线上一次的消息数 —— 压缩只能在同一条线内判定：主线 300 轮之后插进来
    // 一条起标题的 1 轮请求，那不是压缩。
    let mut last_count: HashMap<String, i64> = HashMap::new();
    let mut stats: HashMap<String, (ThreadStat, i64, i64, i64)> = HashMap::new();

    for r in &rows {
        let thread_key: Option<String> = r.get("thread_key");
        let ts: i64 = r.get("ts");
        let message_count: Option<i64> = r.get("message_count");
        let status_code: i32 = r.get("status_code");

        let mut compacted = false;
        let mut compacted_from = None;
        if let Some(key) = &thread_key {
            if let (Some(cur), Some(&prev)) = (message_count, last_count.get(key)) {
                if is_compaction(prev, cur) {
                    compacted = true;
                    compacted_from = Some(prev);
                }
            }
            // 失败的请求不推进基线：被 400 掉的请求带的可能是一份残缺 body，拿它当
            // "上一条"会让下一条正常请求看起来像压缩。
            if let (Some(cur), true) = (message_count, status_code < 400) {
                last_count.insert(key.clone(), cur);
            }

            let st = stats.entry(key.clone()).or_insert_with(|| {
                (ThreadStat { thread_key: key.clone(), requests: 0, has_tools: false }, ts, ts, 0)
            });
            st.0.requests += 1;
            st.0.has_tools |= r.get::<i64, _>("has_tools") != 0;
            st.2 = ts;
            if compacted {
                st.3 += 1;
            }
        }

        entries.push(TimelineEntry {
            request_id: r.get("request_id"),
            ts,
            thread_key,
            model: r.get("model"),
            request_model: r.get("request_model"),
            path: r.get("path"),
            status_code,
            error_message: r.get("error_message"),
            message_count,
            input_tokens: r.get("input_tokens"),
            output_tokens: r.get("output_tokens"),
            cache_read_tokens: r.get("cache_read_tokens"),
            quota: r.get("quota"),
            latency_ms: r.get("latency_ms"),
            compacted,
            compacted_from,
            pinned_from: r.get("pinned_from"),
        });
    }

    let stat_list: Vec<ThreadStat> = stats.values().map(|s| s.0.clone()).collect();
    let kinds = classify_threads(&stat_list);
    let mut threads: Vec<ThreadSummary> = stats
        .into_values()
        .map(|(st, first_ts, last_ts, compactions)| ThreadSummary {
            kind: kinds.get(&st.thread_key).copied().unwrap_or(ThreadKind::Side),
            thread_key: st.thread_key,
            requests: st.requests,
            first_ts,
            last_ts,
            compactions,
        })
        .collect();
    // 主线在前，其余按出现先后 —— 和用户脑子里"先聊了什么、中途派了什么活"的顺序一致。
    threads.sort_by_key(|t| (t.kind != ThreadKind::Main, t.first_ts));

    let title = sqlx::query_scalar::<_, String>("SELECT title FROM session_titles WHERE session_id = ?1")
        .bind(session_id)
        .fetch_optional(pool)
        .await?;

    Ok(SessionTimeline { session_id: session_id.to_string(), title, threads, entries, pin: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::logs::{insert, RequestLogRecord};

    fn rec(id: &str, ts: i64, sid: &str, thread: &str, n: i64, tools: bool, model: &str) -> RequestLogRecord {
        RequestLogRecord {
            request_id: id.into(),
            ts,
            client: "claude-code".into(),
            model: model.into(),
            request_model: model.into(),
            path: "/v1/messages".into(),
            status_code: 200,
            usage_source: "upstream".into(),
            quota: 10,
            input_tokens: 5,
            session_id: Some(sid.into()),
            thread_key: Some(thread.into()),
            message_count: Some(n),
            has_tools: tools,
            ..Default::default()
        }
    }

    async fn seed(pool: &SqlitePool, recs: &[RequestLogRecord]) {
        for r in recs {
            insert(pool, r).await.unwrap();
        }
    }

    #[tokio::test]
    async fn sessions_are_kept_apart_and_list_models_in_order() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        seed(&pool, &[
            rec("a1", 1, "A", "main", 1, true, "flash"),
            rec("b1", 2, "B", "main", 1, true, "opus"),
            rec("a2", 3, "A", "main", 3, true, "opus"),
            // 没有会话 ID 的请求不进会话列表。
            RequestLogRecord { request_id: "x".into(), ts: 4, ..Default::default() },
        ]).await;

        let list = list(&pool, &SessionFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "A", "按最近活跃倒序");
        assert_eq!(list[0].requests, 2);
        assert_eq!(list[0].models, vec!["flash", "opus"], "先用后换的顺序要保住");
        assert_eq!(list[1].models, vec!["opus"]);
    }

    #[tokio::test]
    async fn the_title_comes_back_with_both_the_list_and_the_timeline() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        seed(&pool, &[
            rec("a1", 1, "A", "main", 1, true, "flash"),
            rec("b1", 2, "B", "main", 1, true, "flash"),
        ]).await;
        save_title(&pool, "A", "帮我生成一下上周的ReleaseNote", 1).await.unwrap();

        let list = list(&pool, &SessionFilter { limit: 10, ..Default::default() }).await.unwrap();
        let a = list.iter().find(|s| s.session_id == "A").unwrap();
        assert_eq!(a.title.as_deref(), Some("帮我生成一下上周的ReleaseNote"));
        // 没标题的老会话照样出现，只是 `None` —— 界面回落到「客户端 #会话ID」。
        assert_eq!(list.iter().find(|s| s.session_id == "B").unwrap().title, None);

        assert_eq!(timeline(&pool, "A").await.unwrap().title.as_deref(), Some("帮我生成一下上周的ReleaseNote"));
        assert_eq!(timeline(&pool, "B").await.unwrap().title, None);
    }

    #[tokio::test]
    async fn the_first_title_wins_even_if_a_later_one_differs() {
        // 压缩之后请求里带的"第一句话"是摘要 —— 那时再覆盖，标题就变成了摘要。
        let pool = crate::storage::db::open_memory().await.unwrap();
        seed(&pool, &[rec("1", 1, "S", "main", 1, true, "m")]).await;
        save_title(&pool, "S", "重构路由层", 1).await.unwrap();
        save_title(&pool, "S", "This session is being continued…", 2).await.unwrap();

        let list = list(&pool, &SessionFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(list[0].title.as_deref(), Some("重构路由层"));
    }

    #[tokio::test]
    async fn side_tasks_on_another_model_do_not_count_as_switching() {
        // 固定了模型的对话，起标题仍按客户端自己挑的小模型跑 —— 不能因此标"换过模型"，
        // 否则用户会以为固定没生效。
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut count = rec("3", 3, "S", "", 0, true, "haiku");
        count.thread_key = None;
        seed(&pool, &[
            rec("1", 1, "S", "main", 1, true, "flash"),
            rec("2", 2, "S", "title", 1, false, "opus"),
            count,
        ]).await;
        let list = list(&pool, &SessionFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(list[0].models, vec!["flash"]);
    }

    #[tokio::test]
    async fn a_tool_less_session_still_lists_its_models() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        seed(&pool, &[rec("1", 1, "S", "chat", 1, false, "flash")]).await;
        let list = list(&pool, &SessionFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(list[0].models, vec!["flash"]);
    }

    #[tokio::test]
    async fn compaction_is_detected_within_a_thread_only() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        seed(&pool, &[
            rec("1", 1, "S", "main", 200, true, "m"),
            // 旁路的 1 轮请求夹在中间：不是主线的压缩。
            rec("2", 2, "S", "title", 1, false, "m"),
            rec("3", 3, "S", "main", 203, true, "m"),
            // 主线从 203 掉到 3：压缩。
            rec("4", 4, "S", "main", 3, true, "m"),
            rec("5", 5, "S", "main", 6, true, "m"),
        ]).await;

        let t = timeline(&pool, "S").await.unwrap();
        let flags: Vec<bool> = t.entries.iter().map(|e| e.compacted).collect();
        assert_eq!(flags, vec![false, false, false, true, false]);
        assert_eq!(t.entries[3].compacted_from, Some(203));

        assert_eq!(t.threads[0].thread_key, "main", "主线排第一");
        assert_eq!(t.threads.len(), 2);
        assert_eq!(t.threads[0].kind, ThreadKind::Main);
        assert_eq!(t.threads[0].compactions, 1);
        assert_eq!(t.threads[1].kind, ThreadKind::Side);
    }

    #[tokio::test]
    async fn pinned_requests_show_the_model_they_were_rewritten_from() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut pinned = rec("2", 2, "S", "main", 3, true, "opus");
        pinned.other = serde_json::json!({ "session_pin": { "from": "flash", "to": "opus" } });
        seed(&pool, &[rec("1", 1, "S", "main", 1, true, "flash"), pinned]).await;

        let t = timeline(&pool, "S").await.unwrap();
        assert_eq!(t.entries[0].pinned_from, None);
        assert_eq!(t.entries[1].pinned_from.as_deref(), Some("flash"));
    }

    #[tokio::test]
    async fn non_conversation_requests_are_on_the_timeline_but_not_threads() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut count = rec("2", 2, "S", "", 0, true, "m");
        count.path = "/v1/messages/count_tokens".into();
        count.thread_key = None;
        count.message_count = None;
        seed(&pool, &[rec("1", 1, "S", "main", 50, true, "m"), count, rec("3", 3, "S", "main", 52, true, "m")]).await;

        let t = timeline(&pool, "S").await.unwrap();
        assert_eq!(t.entries.len(), 3, "计数请求也在链路上");
        assert_eq!(t.threads.len(), 1, "但不算一条线");
        assert!(t.entries.iter().all(|e| !e.compacted));
    }

    #[tokio::test]
    async fn a_failed_request_does_not_become_the_compaction_baseline() {
        // 读体失败的请求可能只带了残缺的几轮；拿它当基线，下一条正常请求就成了"增长"，
        // 真正的压缩反而漏掉 —— 或者反过来误报。
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut bad = rec("2", 2, "S", "main", 1, true, "m");
        bad.status_code = 400;
        seed(&pool, &[rec("1", 1, "S", "main", 100, true, "m"), bad, rec("3", 3, "S", "main", 101, true, "m")]).await;

        let t = timeline(&pool, "S").await.unwrap();
        assert!(t.entries[1].compacted, "残缺请求自己相对上一条确实骤降，要标出来");
        assert!(!t.entries[2].compacted, "但它不能成为下一条的基线");
    }
}
