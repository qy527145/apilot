//! 工具调用频率统计。
//!
//! 数的是**响应里模型发起的调用**，不是请求体里的历史记录。Agent 每一轮都会把
//! 全部历史重发一遍，按请求体去数会把第 1 轮的调用重复计 N 次 —— 一个 50 轮的
//! 会话会让曲线尾巴翘起来，看上去像「越聊越爱用工具」，其实只是同一个调用被
//! 数了 50 遍。
//!
//! 两张表分工：
//! - `tool_usage_hourly`：(小时, 客户端, 模型, 工具名) → 调用次数与「多少条响应用过它」
//! - `tool_usage_totals`：(小时, 客户端, 模型) → 响应当分母
//!
//! 没有分母就只能看绝对次数，而绝对次数随流量漂移 —— 跨客户端对比时尤其误导
//! （「Bash 500 次、exec_command 20 次」很可能只是那边流量小）。
//!
//! **工具名不跨客户端归一化。** Claude Code 的 `Bash` 与 Codex 的 `exec_command`
//! 语义相近但不等价，硬映射成同一个名字只会制造一张谁都要重新解释的表；按客户端
//! 分开看才是用户真正要的信息。名字也原样保留 MCP 的 `mcp__server__tool`
//! （界面可以折叠成 `mcp__server`，但库里保留全名，否则多个 server 会并成一行）。
//!
//! **模型也进主键。** 工具调用率跟模型强相关（同一个 harness 换个模型，Bash 与
//! Read 的比例可能整个变过来）。它只在主键里、不在界面上单独成轴，代价是每次
//! 查询多一个 `SUM`，换来的是「换模型之后为什么工具分布变了」将来能答。

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::error::AppResult;
use crate::util::hour_bucket;

// ---------------------------------------------------------------------------
// 对外结构
// ---------------------------------------------------------------------------

/// 一行 (客户端, 工具) 的调用量。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsageCell {
    pub client: String,
    pub tool: String,
    /// 调用次数（同一响应里调两次算两次）。
    pub calls: i64,
    /// 至少调用过一次这个工具的响应条数。
    ///
    /// 与 `calls` 分开是因为它们的分布形状不同：`calls` 高但 `requests` 低
    /// 说明这个工具「一用就连着用好几次」（并行读文件），两者一起看才讲得清。
    pub requests: i64,
}

/// 一个客户端的量，作为占比与人均值的分母。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsageClient {
    pub client: String,
    pub calls: i64,
    /// 这个客户端在区间里解出过内容的响应条数。
    pub responses: i64,
    /// 其中带工具调用的条数。
    pub tool_responses: i64,
}

/// 按小时的趋势点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsagePoint {
    pub bucket_ts: i64,
    /// 空串表示「不分客户端」的合计行 —— 只有请求方选了单个客户端时才出现，
    /// 用来让曲线仍然是一条连续的线。
    pub client: String,
    pub calls: i64,
    pub responses: i64,
}

/// 工具统计面板一次要的全部数据。
///
/// 一次返回而不是拆成几个命令：面板上的几个图看的是同一段时间，
/// 分开发会让它们在刷新时短暂地对不上，用户会看到两块互相矛盾的数字。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsageReport {
    pub total_calls: i64,
    /// 区间内解出过内容的响应条数。所有比例的分母。
    pub total_responses: i64,
    /// 其中带工具调用的条数。
    pub tool_responses: i64,
    /// 出现过的不同工具名数量（跨客户端去重）。
    pub tool_count: i64,
    pub clients: Vec<ToolUsageClient>,
    /// 区间内出现过的全部客户端。**不受筛选影响** —— 它是下拉框的候选值，
    /// 按筛选后的结果算的话，选了 codex 之后就再也切不回 claude-code 了。
    pub available_clients: Vec<String>,
    pub cells: Vec<ToolUsageCell>,
    pub series: Vec<ToolUsagePoint>,
}

// ---------------------------------------------------------------------------
// 内存缓冲
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ToolKey {
    bucket_ts: i64,
    client: String,
    model: String,
    tool: String,
}

#[derive(Debug, Clone, Default)]
struct ToolRow {
    bucket_ts: i64,
    client: String,
    model: String,
    tool: String,
    calls: i64,
    requests: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TotalsKey {
    bucket_ts: i64,
    client: String,
    model: String,
}

#[derive(Debug, Clone, Default)]
struct TotalsRow {
    bucket_ts: i64,
    client: String,
    model: String,
    responses: i64,
    tool_responses: i64,
}

#[derive(Default)]
struct Inner {
    tools: HashMap<ToolKey, ToolRow>,
    totals: HashMap<TotalsKey, TotalsRow>,
}

/// 工具调用的内存聚合缓冲。
///
/// 与 `AggregateBuffer` 同一个 flush 节奏（每 10 秒 + 退出时各一次），但用独立的
/// `Mutex`：两者互不依赖，合用一个锁只会让统计热路径白白多等别人的临界区。
#[derive(Default)]
pub struct ToolUsageBuffer {
    inner: Mutex<Inner>,
}

impl ToolUsageBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条响应的工具调用。
    ///
    /// `names` 为空也要记 —— 那是分母里「这条响应没动手」的那一半。只记有调用的
    /// 那部分，算出来的比例永远是 100%。
    pub fn record(&self, ts_ms: i64, client: &str, model: &str, names: &[String]) {
        let bucket_ts = hour_bucket(ts_ms / 1000);
        let mut guard = self.inner.lock();

        let totals = guard
            .totals
            .entry(TotalsKey {
                bucket_ts,
                client: client.to_string(),
                model: model.to_string(),
            })
            .or_insert_with(|| TotalsRow {
                bucket_ts,
                client: client.to_string(),
                model: model.to_string(),
                ..Default::default()
            });
        totals.responses += 1;
        if !names.is_empty() {
            totals.tool_responses += 1;
        }

        // 一条响应里同一个工具可能被调多次（并行读文件）。`calls` 按次数累加，
        // `requests` 只加一次 —— 两者一起才看得出「高频但分散」还是「一次连发」。
        let mut seen: HashSet<&str> = HashSet::new();
        for name in names {
            let row = guard
                .tools
                .entry(ToolKey {
                    bucket_ts,
                    client: client.to_string(),
                    model: model.to_string(),
                    tool: name.clone(),
                })
                .or_insert_with(|| ToolRow {
                    bucket_ts,
                    client: client.to_string(),
                    model: model.to_string(),
                    tool: name.clone(),
                    ..Default::default()
                });
            row.calls += 1;
            if seen.insert(name.as_str()) {
                row.requests += 1;
            }
        }
    }

    /// 当前缓冲里有几组 (工具) 行。测试与诊断用。
    pub fn len(&self) -> usize {
        self.inner.lock().tools.len()
    }

    pub fn is_empty(&self) -> bool {
        // 必须一次性取出两个字段：`self.inner.lock()` 写两遍时，第一个守卫要活到
        // 整个表达式结束（临时值的作用域规则），第二个 `lock()` 就会死锁 ——
        // parking_lot 的锁不可重入。
        let guard = self.inner.lock();
        guard.tools.is_empty() && guard.totals.is_empty()
    }

    /// 落库。与 `AggregateBuffer::flush` 一样用 `x = x + excluded.x` 做增量累加，
    /// 同一小时被多次 flush 也不会互相覆盖。
    pub async fn flush(&self, pool: &SqlitePool) -> AppResult<usize> {
        let (tools, totals) = {
            let mut guard = self.inner.lock();
            let tools: Vec<ToolRow> = std::mem::take(&mut guard.tools).into_values().collect();
            let totals: Vec<TotalsRow> = std::mem::take(&mut guard.totals).into_values().collect();
            (tools, totals)
        };
        if tools.is_empty() && totals.is_empty() {
            return Ok(0);
        }

        let mut tx = pool.begin().await?;
        for r in &tools {
            sqlx::query(
                "INSERT INTO tool_usage_hourly (bucket_ts, client, model, tool, calls, requests)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(bucket_ts, client, model, tool) DO UPDATE SET
                     calls    = calls + excluded.calls,
                     requests = requests + excluded.requests",
            )
            .bind(r.bucket_ts)
            .bind(&r.client)
            .bind(&r.model)
            .bind(&r.tool)
            .bind(r.calls)
            .bind(r.requests)
            .execute(&mut *tx)
            .await?;
        }
        for r in &totals {
            sqlx::query(
                "INSERT INTO tool_usage_totals
                     (bucket_ts, client, model, responses, tool_responses)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(bucket_ts, client, model) DO UPDATE SET
                     responses      = responses + excluded.responses,
                     tool_responses = tool_responses + excluded.tool_responses",
            )
            .bind(r.bucket_ts)
            .bind(&r.client)
            .bind(&r.model)
            .bind(r.responses)
            .bind(r.tool_responses)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        Ok(tools.len() + totals.len())
    }
}

// ---------------------------------------------------------------------------
// 查询
// ---------------------------------------------------------------------------

fn range_to_bucket(from_ms: i64, to_ms: i64) -> (i64, i64) {
    (hour_bucket(from_ms / 1000), hour_bucket(to_ms / 1000))
}

/// 工具统计面板的全部数据。`client` 为 `None` 表示不限客户端。
pub async fn report(
    pool: &SqlitePool,
    from_ms: i64,
    to_ms: i64,
    client: Option<&str>,
) -> AppResult<ToolUsageReport> {
    let (from, to) = range_to_bucket(from_ms, to_ms);

    // --- (客户端, 工具) 的调用量 ---
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT client, tool, SUM(calls) AS calls, SUM(requests) AS requests
         FROM tool_usage_hourly WHERE bucket_ts >= ",
    );
    qb.push_bind(from).push(" AND bucket_ts <= ").push_bind(to);
    if let Some(c) = client {
        qb.push(" AND client = ").push_bind(c.to_string());
    }
    qb.push(" GROUP BY client, tool ORDER BY SUM(calls) DESC");

    let cells: Vec<ToolUsageCell> = qb
        .build()
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| ToolUsageCell {
            client: r.get("client"),
            tool: r.get("tool"),
            calls: r.get("calls"),
            requests: r.get("requests"),
        })
        .collect();

    // --- 分母 ---
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT client, SUM(responses) AS responses, SUM(tool_responses) AS tool_responses
         FROM tool_usage_totals WHERE bucket_ts >= ",
    );
    qb.push_bind(from).push(" AND bucket_ts <= ").push_bind(to);
    if let Some(c) = client {
        qb.push(" AND client = ").push_bind(c.to_string());
    }
    qb.push(" GROUP BY client ORDER BY SUM(responses) DESC");

    let mut clients: Vec<ToolUsageClient> = qb
        .build()
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| ToolUsageClient {
            client: r.get("client"),
            calls: 0,
            responses: r.get("responses"),
            tool_responses: r.get("tool_responses"),
        })
        .collect();

    // 每个客户端的总调用量由上面按 (客户端, 工具) 的结果加出来，不必再查一次。
    for c in &mut clients {
        c.calls = cells
            .iter()
            .filter(|cell| cell.client == c.client)
            .map(|cell| cell.calls)
            .sum();
    }

    // --- 按小时的趋势 ---
    //
    // 两张表各查一遍再在内存里拼，而不是 JOIN：`tool_usage_hourly` 与
    // `tool_usage_totals` 的粒度不同（后者没有工具这一维），JOIN 之后
    // `responses` 会被每个工具重复相加，比例直接算错。
    let (mut calls_by_hour, mut responses_by_hour) = (
        HashMap::<(i64, String), i64>::new(),
        HashMap::<(i64, String), (i64, i64)>::new(),
    );

    let mut qb = sqlx::QueryBuilder::new(
        "SELECT bucket_ts, client, SUM(calls) AS calls
         FROM tool_usage_hourly WHERE bucket_ts >= ",
    );
    qb.push_bind(from).push(" AND bucket_ts <= ").push_bind(to);
    if let Some(c) = client {
        qb.push(" AND client = ").push_bind(c.to_string());
    }
    qb.push(" GROUP BY bucket_ts, client");
    for r in qb.build().fetch_all(pool).await? {
        *calls_by_hour
            .entry((r.get("bucket_ts"), r.get("client")))
            .or_default() += r.get::<i64, _>("calls");
    }

    let mut qb = sqlx::QueryBuilder::new(
        "SELECT bucket_ts, client, SUM(responses) AS responses,
                SUM(tool_responses) AS tool_responses
         FROM tool_usage_totals WHERE bucket_ts >= ",
    );
    qb.push_bind(from).push(" AND bucket_ts <= ").push_bind(to);
    if let Some(c) = client {
        qb.push(" AND client = ").push_bind(c.to_string());
    }
    qb.push(" GROUP BY bucket_ts, client");
    for r in qb.build().fetch_all(pool).await? {
        let e = responses_by_hour
            .entry((r.get("bucket_ts"), r.get("client")))
            .or_default();
        e.0 += r.get::<i64, _>("responses");
        e.1 += r.get::<i64, _>("tool_responses");
    }

    let mut keys: Vec<(i64, String)> = responses_by_hour.keys().cloned().collect();
    keys.sort();
    let series: Vec<ToolUsagePoint> = keys
        .into_iter()
        .map(|(bucket_ts, client)| ToolUsagePoint {
            calls: calls_by_hour
                .get(&(bucket_ts, client.clone()))
                .copied()
                .unwrap_or(0),
            responses: responses_by_hour
                .get(&(bucket_ts, client.clone()))
                .map(|v| v.0)
                .unwrap_or(0),
            bucket_ts,
            client,
        })
        .collect();

    let total_calls = cells.iter().map(|c| c.calls).sum();
    let total_responses = clients.iter().map(|c| c.responses).sum();
    let tool_responses = clients.iter().map(|c| c.tool_responses).sum();
    let tool_count = cells
        .iter()
        .map(|c| c.tool.as_str())
        .collect::<HashSet<_>>()
        .len() as i64;

    // 下拉候选值：单独查一次、**不带筛选**。与上面的查询共用同一个时间范围，
    // 所以切到某个客户端之后，列表里另外几个还在，用户能切回去。
    let available_clients: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT client FROM tool_usage_totals
         WHERE bucket_ts >= ?1 AND bucket_ts <= ?2 ORDER BY client",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await?;

    Ok(ToolUsageReport {
        total_calls,
        total_responses,
        tool_responses,
        tool_count,
        clients,
        available_clients,
        cells,
        series,
    })
}

// ---------------------------------------------------------------------------
// 历史回填
// ---------------------------------------------------------------------------

/// 从现存的捕获里补一份历史。
///
/// 工具统计上线那天，库里已经有一批捕获（`response_content` 是流式响应重建出的
/// `UnifiedResponse`）。不回填的话面板第一天是空的，用户只能等几天才有图。
///
/// **只跑一次**，判据是 `settings_kv` 里的标记而不是「表里有没有行」—— 后者在
/// 用户清过聚合表之后会误判成没跑过，于是把同一批捕获再计一遍。
///
/// 只读旧捕获：那些请求已经结束、不会再被记第二遍，所以不会重复计数。
/// 覆盖不到的范围是已知且可接受的：捕获受 `capture_max_entries` 与保留天数
/// 两道裁剪，回填只能补上「此刻还留着的那些」。
pub async fn backfill_from_captures(pool: &SqlitePool) -> AppResult<u64> {
    const FLAG_KEY: &str = "tool_usage_backfilled_v1";

    let done: Option<String> =
        sqlx::query_scalar("SELECT value FROM settings_kv WHERE key = ?1")
            .bind(FLAG_KEY)
            .fetch_optional(pool)
            .await?;
    if done.is_some() {
        return Ok(0);
    }

    // 以小时 × 客户端 × 模型 × 工具名分组一次算完，全程留在 SQLite 里。
    //
    // - `json_valid` 挡住被截断 / 坏掉的 `response_content`，否则一次
    //   `malformed JSON` 会让整个回填报错。
    // - 空名字要排除：流被中断时累加器会留下一个还没拿到 `name` 的 ToolUse。
    // - `status_code < 400` 与线上口径对齐：在线记录只在成功收尾时发生，
    //   失败的请求不进分母也不计调用。不排的话，回填会把一批 499/502 的
    //   捕获塞进分母，面板上的「动手占比」在回填当天偏低、之后又自己回升。
    let rows = sqlx::query(
        "SELECT (l.ts / 1000) - ((l.ts / 1000) % 3600) AS bucket_ts,
                l.client AS client, l.model AS model,
                json_extract(je.value, '$.name') AS tool,
                COUNT(*) AS calls,
                COUNT(DISTINCT c.request_id) AS requests
         FROM captures c
         JOIN request_logs l ON l.request_id = c.request_id
         JOIN json_each(json_extract(c.response_content, '$.content')) je
         WHERE c.response_content IS NOT NULL
           AND json_valid(c.response_content)
           AND l.status_code < 400
           AND json_extract(je.value, '$.type') = 'tool_use'
           AND COALESCE(json_extract(je.value, '$.name'), '') <> ''
         GROUP BY bucket_ts, l.client, l.model, tool",
    )
    .fetch_all(pool)
    .await?;

    // 分母：每条捕获算一个响应，带过工具的算 `tool_responses`。
    // 与上面同源（`response_content` 非空且合法、且请求成功），两个数的口径才一致。
    let totals = sqlx::query(
        "SELECT (l.ts / 1000) - ((l.ts / 1000) % 3600) AS bucket_ts,
                l.client AS client, l.model AS model,
                COUNT(DISTINCT c.request_id) AS responses,
                COUNT(DISTINCT CASE WHEN EXISTS (
                    SELECT 1 FROM json_each(json_extract(c.response_content, '$.content')) j2
                    WHERE json_extract(j2.value, '$.type') = 'tool_use'
                      AND COALESCE(json_extract(j2.value, '$.name'), '') <> ''
                ) THEN c.request_id END) AS tool_responses
         FROM captures c
         JOIN request_logs l ON l.request_id = c.request_id
         WHERE c.response_content IS NOT NULL
           AND json_valid(c.response_content)
           AND l.status_code < 400
         GROUP BY bucket_ts, l.client, l.model",
    )
    .fetch_all(pool)
    .await?;

    let mut tx = pool.begin().await?;
    for r in &rows {
        sqlx::query(
            "INSERT INTO tool_usage_hourly (bucket_ts, client, model, tool, calls, requests)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(bucket_ts, client, model, tool) DO UPDATE SET
                 calls    = calls + excluded.calls,
                 requests = requests + excluded.requests",
        )
        .bind(r.get::<i64, _>("bucket_ts"))
        .bind(r.get::<String, _>("client"))
        .bind(r.get::<String, _>("model"))
        .bind(r.get::<String, _>("tool"))
        .bind(r.get::<i64, _>("calls"))
        .bind(r.get::<i64, _>("requests"))
        .execute(&mut *tx)
        .await?;
    }
    for r in &totals {
        sqlx::query(
            "INSERT INTO tool_usage_totals (bucket_ts, client, model, responses, tool_responses)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(bucket_ts, client, model) DO UPDATE SET
                 responses      = responses + excluded.responses,
                 tool_responses = tool_responses + excluded.tool_responses",
        )
        .bind(r.get::<i64, _>("bucket_ts"))
        .bind(r.get::<String, _>("client"))
        .bind(r.get::<String, _>("model"))
        .bind(r.get::<i64, _>("responses"))
        .bind(r.get::<i64, _>("tool_responses"))
        .execute(&mut *tx)
        .await?;
    }
    // 标记与数据在同一个事务里：中途失败时两样一起回滚，下次启动重来一遍，
    // 不会出现「标记写了、数据只补了一半」。
    sqlx::query(
        "INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(FLAG_KEY)
    .bind(crate::util::now_ms().to_string())
    .bind(crate::util::now_ms())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn one_response_counts_its_calls_and_itself_as_denominator() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let now = crate::util::now_ms();

        b.record(now, "claude-code", "m", &names(&["Bash", "Read"]));
        b.flush(&p).await.unwrap();

        let r = report(&p, now - 3_600_000, now + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 2);
        assert_eq!(r.total_responses, 1, "一条响应就是一个分母");
        assert_eq!(r.tool_responses, 1);
        assert_eq!(r.tool_count, 2);
    }

    #[tokio::test]
    async fn a_response_without_tool_calls_still_counts_in_the_denominator() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let now = crate::util::now_ms();

        b.record(now, "claude-code", "m", &names(&["Bash"]));
        b.record(now, "claude-code", "m", &[]);
        b.record(now, "claude-code", "m", &[]);
        b.flush(&p).await.unwrap();

        let r = report(&p, now - 3_600_000, now + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 1);
        assert_eq!(r.total_responses, 3, "没动手的响应也是分母");
        assert_eq!(r.tool_responses, 1, "但不算「用过工具的响应」");
    }

    #[tokio::test]
    async fn the_same_tool_twice_in_one_response_is_two_calls_but_one_request() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let now = crate::util::now_ms();

        // 并行读文件：一次响应里 Read 出现两次。
        b.record(now, "claude-code", "m", &names(&["Read", "Read"]));
        b.flush(&p).await.unwrap();

        let r = report(&p, now - 3_600_000, now + 3_600_000, None)
            .await
            .unwrap();
        let cell = r.cells.iter().find(|c| c.tool == "Read").unwrap();
        assert_eq!(cell.calls, 2);
        assert_eq!(cell.requests, 1, "只有一条响应用过它");
    }

    #[tokio::test]
    async fn clients_are_not_mixed_together() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let now = crate::util::now_ms();

        b.record(now, "claude-code", "m", &names(&["Bash"]));
        b.record(now, "codex", "m", &names(&["exec_command"]));
        b.flush(&p).await.unwrap();

        let r = report(&p, now - 3_600_000, now + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.clients.len(), 2);
        assert_eq!(r.tool_count, 2, "Bash 与 exec_command 不归一化");

        let only = report(&p, now - 3_600_000, now + 3_600_000, Some("codex"))
            .await
            .unwrap();
        assert_eq!(only.total_calls, 1);
        assert_eq!(only.cells[0].tool, "exec_command");
        assert_eq!(only.total_responses, 1);
    }

    #[tokio::test]
    async fn repeated_flushes_accumulate_instead_of_overwriting() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let now = crate::util::now_ms();

        b.record(now, "c", "m", &names(&["Bash"]));
        b.flush(&p).await.unwrap();
        b.record(now, "c", "m", &names(&["Bash"]));
        b.flush(&p).await.unwrap();

        let r = report(&p, now - 3_600_000, now + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 2, "同一小时两次 flush 必须累加");
        assert_eq!(r.total_responses, 2);
    }

    #[tokio::test]
    async fn an_empty_buffer_flushes_to_nothing() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        assert_eq!(b.flush(&p).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn series_keeps_calls_and_responses_on_their_own_scale() {
        let p = pool().await;
        let b = ToolUsageBuffer::new();
        let base = 1_700_000_000_000i64; // 固定时刻，避免跨小时抖动

        b.record(base, "c", "m", &names(&["Bash", "Read"]));
        b.record(base + 3_600_000, "c", "m", &names(&["Bash"]));
        b.flush(&p).await.unwrap();

        let r = report(&p, base - 3_600_000, base + 2 * 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.series.len(), 2, "跨小时应分成两点");
        assert_eq!(r.series[0].calls, 2);
        assert_eq!(r.series[0].responses, 1);
        assert_eq!(r.series[1].calls, 1);
        assert_eq!(r.series[1].responses, 1);
    }

    #[tokio::test]
    async fn backfill_reads_existing_captures_and_runs_only_once() {
        let p = pool().await;
        let ts = 1_700_000_000_000i64;

        let mut rec = crate::storage::logs::RequestLogRecord {
            request_id: "r1".into(),
            ts,
            client: "claude-code".into(),
            model: "m".into(),
            ..Default::default()
        };
        rec.protocol_in = "anthropic".into();
        crate::storage::logs::insert(&p, &rec).await.unwrap();

        let mut cap = crate::storage::logs::CaptureRecord {
            request_id: "r1".into(),
            ts,
            ..Default::default()
        };
        cap.response_content = Some(
            serde_json::json!({
                "id": "r1", "model": "m", "finish_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "Bash", "input": {}}
                ]
            })
            .to_string(),
        );
        crate::storage::logs::save_capture(&p, &cap).await.unwrap();

        assert_eq!(backfill_from_captures(&p).await.unwrap(), 1);
        let r = report(&p, ts - 3_600_000, ts + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 2);
        assert_eq!(r.total_responses, 1);
        assert_eq!(r.cells[0].requests, 1);

        // 第二次必须什么都不做：否则每次启动都会把同一批捕获再计一遍。
        assert_eq!(backfill_from_captures(&p).await.unwrap(), 0);
        let again = report(&p, ts - 3_600_000, ts + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(again.total_calls, 2);
    }

    #[tokio::test]
    async fn backfill_skips_broken_and_nameless_entries() {
        let p = pool().await;
        let ts = 1_700_000_000_000i64;

        for (id, content) in [
            ("bad", "{ 坏掉的 JSON"),
            // 流被中断时留下的半截调用：还没有名字。
            (
                "nameless",
                r#"{"id":"n","model":"m","finish_reason":"stop","content":[{"type":"tool_use","id":"t","name":"","input":{}}]}"#,
            ),
        ] {
            let mut rec = crate::storage::logs::RequestLogRecord {
                request_id: id.into(),
                ts,
                client: "c".into(),
                model: "m".into(),
                ..Default::default()
            };
            rec.protocol_in = "anthropic".into();
            crate::storage::logs::insert(&p, &rec).await.unwrap();
            let mut cap = crate::storage::logs::CaptureRecord {
                request_id: id.into(),
                ts,
                ..Default::default()
            };
            cap.response_content = Some(content.to_string());
            crate::storage::logs::save_capture(&p, &cap).await.unwrap();
        }

        backfill_from_captures(&p).await.unwrap();
        let r = report(&p, ts - 3_600_000, ts + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 0, "坏 JSON 与空名字都不该被计数");
        assert_eq!(r.total_responses, 1, "合法但没工具的那条照样进分母");
    }

    #[tokio::test]
    async fn backfill_skips_failed_requests_like_the_live_path_does() {
        // 线上只在成功收尾时记一笔；回填不排掉失败的话，一次按键 Esc 留下的
        // 499 捕获会被当成一次「没动手的响应」，把当天的手占比压低。
        let p = pool().await;
        let ts = 1_700_000_000_000i64;

        for (id, status) in [("ok", 200), ("failed", 499)] {
            let mut rec = crate::storage::logs::RequestLogRecord {
                request_id: id.into(),
                ts,
                client: "c".into(),
                model: "m".into(),
                status_code: status,
                ..Default::default()
            };
            rec.protocol_in = "anthropic".into();
            crate::storage::logs::insert(&p, &rec).await.unwrap();
            let mut cap = crate::storage::logs::CaptureRecord {
                request_id: id.into(),
                ts,
                ..Default::default()
            };
            // 失败那条也留着一个工具调用 —— 真实场景里中断前确实可能已经调过一次。
            cap.response_content = Some(
                r#"{"id":"x","model":"m","finish_reason":"tool_use","content":[{"type":"tool_use","id":"t","name":"Bash","input":{}}]}"#
                    .to_string(),
            );
            crate::storage::logs::save_capture(&p, &cap).await.unwrap();
        }

        backfill_from_captures(&p).await.unwrap();
        let r = report(&p, ts - 3_600_000, ts + 3_600_000, None)
            .await
            .unwrap();
        assert_eq!(r.total_calls, 1, "只有成功那条计入调用");
        assert_eq!(r.total_responses, 1, "失败那条也不进分母");
    }
}