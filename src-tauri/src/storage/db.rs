//! SQLite 连接池与迁移执行。

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, SqlitePool};

use super::migrations::{MIGRATIONS, SCHEMA_VERSION, USAGE_HOURLY_V8_DDL};
use crate::error::AppResult;

/// 打开（必要时创建）数据库并跑完所有迁移。
pub async fn open(path: &Path) -> AppResult<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal) // WAL：读写不互相阻塞，网关写入不影响前端查询
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5))
        .disable_statement_logging();

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await?;

    migrate(&pool).await?;
    Ok(pool)
}

/// 内存库，仅供测试。
#[cfg(test)]
pub async fn open_memory() -> AppResult<SqlitePool> {
    use std::str::FromStr;

    let opts = SqliteConnectOptions::from_str("sqlite::memory:")?
        .foreign_keys(true)
        .disable_statement_logging();
    let pool = SqlitePoolOptions::new()
        .max_connections(1) // 内存库每条连接是独立数据库，必须收敛到单连接
        .connect_with(opts)
        .await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// 按 `PRAGMA user_version` 增量执行迁移。
async fn migrate(pool: &SqlitePool) -> AppResult<()> {
    let current: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;

    if current >= SCHEMA_VERSION {
        // 版本号比我们新，不等于结构比我们新：v7/v8 那次拆分把某些库的版本戳
        // 顶到了 8，此后**每一条**新迁移都会被这里跳掉。两道修复各自先看表的
        // 真实形状，健康就立刻返回，所以对正常的库是纯读。
        repair_legacy_model_capabilities(pool).await?;
        ensure_usage_hourly_v8(pool).await?;
        return Ok(());
    }

    for (idx, ddl) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if version <= current {
            continue;
        }

        // 整条迁移在一个事务里执行；DDL 失败则整体回滚，user_version 不前进。
        let mut tx = pool.begin().await?;
        for stmt in split_statements(ddl) {
            sqlx::query(&stmt).execute(&mut *tx).await?;
        }
        // PRAGMA 不接受参数绑定，只能拼接；version 是内部整数，无注入面。
        sqlx::query(&format!("PRAGMA user_version = {version}"))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        tracing::info!(version, "已应用数据库迁移");
    }

    repair_legacy_model_capabilities(pool).await?;
    ensure_usage_hourly_v8(pool).await?;
    Ok(())
}

/// 补跑 v8（`usage_hourly.request_model`）。
///
/// 判据与 `repair_legacy_model_capabilities` 同源：**看表的实际形状，不看版本号**。
/// 版本戳停在 8 而 `SCHEMA_VERSION` 也是 8 的库会走进上面的早返回，v8 那条迁移
/// 永远轮不上 —— 症状很隐：`AggregateBuffer::flush` 每 10 秒报一次 `no such column`
/// 然后被吞掉，统计页从此不再增长，没有任何一处会报错给用户看。
async fn ensure_usage_hourly_v8(pool: &SqlitePool) -> AppResult<()> {
    let has_column: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('usage_hourly') WHERE name = 'request_model'",
    )
    .fetch_one(pool)
    .await?;
    if has_column > 0 {
        return Ok(());
    }
    // 表都不在（理论上到不了这里：早返回意味着迁移全跑过）—— 留给下一轮启动，
    // 别在一个结构未知的库上做重建。
    let table_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'usage_hourly'",
    )
    .fetch_one(pool)
    .await?;
    if table_exists == 0 {
        return Ok(());
    }

    tracing::warn!("usage_hourly 缺少 request_model 列，补跑 v8 迁移");

    let mut tx = pool.begin().await?;
    for stmt in split_statements(USAGE_HOURLY_V8_DDL) {
        sqlx::query(&stmt).execute(&mut *tx).await?;
    }
    let current: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if current != SCHEMA_VERSION {
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&mut *tx)
            .await?;
        tracing::warn!(from = current, to = SCHEMA_VERSION, "版本戳已拉回当前 schema 版本");
    }
    tx.commit().await?;

    Ok(())
}

/// `model_capabilities` 的正确结构。
///
/// 与 `migrations.rs` 里 v7 的建表语句必须一致 —— 有测试把两者钉在一起
/// （`repair_produces_the_same_schema_as_a_fresh_migration`），不靠肉眼比对字符串。
const MODEL_CAPABILITIES_DDL: &str = "CREATE TABLE model_capabilities (
         provider_id INTEGER NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
         model       TEXT NOT NULL,
         capability  TEXT NOT NULL,
         verdict     TEXT NOT NULL,
         source      TEXT NOT NULL,
         evidence    TEXT,
         checked_at  INTEGER NOT NULL,
         PRIMARY KEY (provider_id, model, capability)
     )";

/// 修掉「改了一条已经跑过的迁移」留下的旧表，并把版本戳拉回同步。
///
/// 事故经过：v7 在开发途中被改过一次 —— 最初建成 `supported INTEGER`（布尔），
/// 后来改成三态的 `verdict TEXT`。但迁移按 `user_version` 增量执行，已经跑过 v7
/// 的库**不会重跑**，于是表里留的是旧列，代码读的却是 `verdict`，表现为启动即报
/// `no such column: verdict`。
///
/// 三个不显然的地方：
///
/// 1. **不能靠版本号判断该不该修。** 出事的库 `user_version` 是 8（那次改动还把
///    v7/v8 拆成了两条），比 `SCHEMA_VERSION` 还大，任何新迁移都会被跳过。所以
///    这里只看表的**实际形状**：`verdict` 列存在且声明为 `TEXT` 才算健康。
///
/// 2. **判据得同时覆盖「改名前」和「改名后」。** 中途试过只 RENAME COLUMN，结果
///    SQLite 会把原列类型一起带过来，得到 `verdict INTEGER` —— 名字对了、类型
///    还是错的。名字+类型一起判，才能把这种半修状态也收进来。
///
/// 3. **必须把 `user_version` 拉回 `SCHEMA_VERSION`。** 出事的库停在 8，而当前
///    是 7；迁移判定是 `current >= SCHEMA_VERSION` 就跳过，于是**下一次加迁移
///    时这条库会静默跳过它**。既然这里已经把结构重建成正好等于 `SCHEMA_VERSION`
///    的样子，版本戳就该说 `SCHEMA_VERSION`。
///
/// 只在真的动了手的时候才改版本戳：一个来自更高版本的库（结构比我们新）不该被
/// 降级标记，那会让旧迁移在新结构上重跑。
///
/// 重建而不是 `DROP` + `CREATE`：列和值都要保。旧结构没有「不确定」这一态，
/// 所以 1 → `supported`、其余 → `unsupported` 不会凭空造出第三种。
async fn repair_legacy_model_capabilities(pool: &SqlitePool) -> AppResult<()> {
    let cols: Vec<(String, String)> = sqlx::query("PRAGMA table_info(model_capabilities)")
        .fetch_all(pool)
        .await?
        .iter()
        .map(|r| {
            (
                sqlx::Row::get::<String, _>(r, "name"),
                sqlx::Row::get::<String, _>(r, "type"),
            )
        })
        .collect();

    let healthy = cols
        .iter()
        .any(|(name, ty)| name == "verdict" && ty.eq_ignore_ascii_case("TEXT"));
    if healthy {
        return Ok(());
    }

    tracing::warn!(
        columns = ?cols,
        "model_capabilities 的结构不是当前版本的样子，重建为三态结构"
    );

    let mut tx = pool.begin().await?;

    // 旧结构里能力列叫 supported 且是整数；新结构建好后按名字搬。
    let verdict_expr = if cols.iter().any(|(n, _)| n == "supported") {
        "CASE WHEN supported = 1 THEN 'supported' ELSE 'unsupported' END"
    } else {
        // 名字已经是 verdict 但类型不对：值多半也已经是字符串，原样搬。
        "CASE WHEN verdict = 1 THEN 'supported'
              WHEN verdict = 0 THEN 'unsupported'
              ELSE CAST(verdict AS TEXT) END"
    };

    sqlx::query("DROP TABLE IF EXISTS model_capabilities_repaired")
        .execute(&mut *tx)
        .await?;
    sqlx::query(&MODEL_CAPABILITIES_DDL.replace(
        "CREATE TABLE model_capabilities",
        "CREATE TABLE model_capabilities_repaired",
    ))
    .execute(&mut *tx)
    .await?;
    // 只搬渠道还在的行。
    //
    // 旧表**没有**外键约束，所以里面可能留着渠道已被删掉的孤儿行；新表有
    // `REFERENCES providers(id)`，原样搬会撞外键、让整个修复失败 —— 一个修复
    // 把应用搞得起不来，比它要修的问题严重得多。渠道都没了的能力行本来也没用，
    // 丢掉正是想要的结果。
    sqlx::query(&format!(
        "INSERT INTO model_capabilities_repaired
             (provider_id, model, capability, verdict, source, evidence, checked_at)
         SELECT provider_id, model, capability, {verdict_expr}, source, evidence, checked_at
         FROM model_capabilities
         WHERE provider_id IN (SELECT id FROM providers)"
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query("DROP TABLE model_capabilities")
        .execute(&mut *tx)
        .await?;
    sqlx::query("ALTER TABLE model_capabilities_repaired RENAME TO model_capabilities")
        .execute(&mut *tx)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_model_capabilities_model ON model_capabilities(model)")
        .execute(&mut *tx)
        .await?;

    let current: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if current != SCHEMA_VERSION {
        sqlx::query(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .execute(&mut *tx)
            .await?;
        tracing::warn!(from = current, to = SCHEMA_VERSION, "版本戳已拉回当前 schema 版本");
    }

    tx.commit().await?;
    Ok(())
}

/// 按分号切分 DDL 脚本，跳过注释与空语句。
///
/// 迁移脚本里不含字符串字面量中的分号，因此朴素切分是安全的；
/// 有单测守着这个前提。
fn split_statements(script: &str) -> Vec<String> {
    script
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_cleanly_on_memory_db() {        let pool = open_memory().await.expect("迁移应当成功");

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // 关键表都建出来了
        for table in [
            "providers",
            "provider_models",
            "route_rules",
            "selectors",
            "request_logs",
            "usage_hourly",
            "model_pricing",
            "model_capabilities",
            "response_cache",
            "captures",
            "settings_kv",
        ] {
            let found: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(found, 1, "缺少表 {table}");
        }
    }

    /// 模拟"用户机器上已经躺着一个 v1 的库，新版本启动时升级到 v2"。
    ///
    /// 这条路径用 `open_memory()` 测不到 —— 它一上来就把所有迁移跑完了。
    /// 而真实升级里 ALTER TABLE 必须能加在**已有数据**的表上，老行要拿到
    /// 新列的默认值。加列写错的话，用户一升级就打不开应用，所以单独测。
    #[tokio::test]
    async fn upgrading_an_existing_v1_database_keeps_its_rows() {
        use std::str::FromStr;

        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();

        // 只跑 v1，并把版本号停在 1 —— 这就是老库的样子。
        for stmt in split_statements(MIGRATIONS[0]) {
            sqlx::query(&stmt).execute(&pool).await.unwrap();
        }
        sqlx::query("PRAGMA user_version = 1")
            .execute(&pool)
            .await
            .unwrap();

        // 老库里已经有真实数据，升级必须原样保住。
        sqlx::query(
            "INSERT INTO request_logs
                 (request_id, ts, client, protocol_in, protocol_out, model, request_model)
             VALUES ('old', 1, 'codex', 'openai_responses', 'openai_chat', 'gpt-5', 'gpt-5')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // 老库里的渠道设过一个非默认的优先级。
        sqlx::query(
            "INSERT INTO providers (tag, name, kind, base_url, priority, weight, created_at, updated_at)
             VALUES ('p', 'P', 'openai_chat', 'https://x', 7, 3, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        // v1 时代 provider_models 的 priority/weight 是建表默认的 0/1，
        // 排序用的是渠道自己的值。
        sqlx::query(
            "INSERT INTO provider_models (provider_id, model, upstream_model, client_group,
                 priority, weight, enabled)
             VALUES (1, 'm', NULL, '*', 0, 1, 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        migrate(&pool).await.unwrap();

        // v4 的回填：每模型的 priority/weight 要变成所属渠道当时的值。
        // 不回填的话，0/1 会把用户设过的渠道优先级整个抹平 ——
        // 那种改动不报错，只会让流量悄悄换了渠道。
        let (priority, weight): (i64, i64) =
            sqlx::query_as("SELECT priority, weight FROM provider_models WHERE model = 'm'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (priority, weight),
            (7, 3),
            "每模型的值应回填成升级那一刻所属渠道的值，排序结果才与改动前一致"
        );

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let path: String =
            sqlx::query_scalar("SELECT path FROM request_logs WHERE request_id = 'old'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(path, "", "老日志行应拿到空路径默认值，而不是缺失该列");

        // 老渠道没声明过协议，退化后等价于"只支持 kind 那一种"（见 Provider::endpoints）。
        let protocols: String =
            sqlx::query_scalar("SELECT protocols FROM providers WHERE tag = 'p'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(protocols, "[]");

        // 捕获表也要能接受新列（老库里一行捕获都没有，正好验证空表加列）。
        sqlx::query(
            "INSERT INTO captures (request_id, ts, method, path) VALUES ('old', 1, 'POST', '/v1/x')",
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    /// 版本戳比 `SCHEMA_VERSION` 还大的库，新迁移会被整体跳过 —— 只能靠表的形状补跑。
    ///
    /// 复现的是 v7/v8 那次拆分留下的状态：出事的库 `user_version` 停在 8，而当时
    /// `SCHEMA_VERSION` 是 7，后来涨到 8 之后，`current >= SCHEMA_VERSION` 正好成立，
    /// v8 这条新迁移永远轮不上。少了兜底，`usage_hourly` 会一直缺 `request_model`，
    /// 而症状只是聚合**静默**停止增长 —— 每 10 秒一条 warn，用户那边什么都看不到。
    #[tokio::test]
    async fn a_database_stuck_at_a_newer_version_still_gets_v8() {
        use std::str::FromStr;

        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true)
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();

        // 跑到 v7 为止（v8 是本次新加的），然后**把版本戳顶到 8**。
        for ddl in &MIGRATIONS[..SCHEMA_VERSION as usize - 1] {
            for stmt in split_statements(ddl) {
                sqlx::query(&stmt).execute(&pool).await.unwrap();
            }
        }
        sqlx::query("PRAGMA user_version = 8")
            .execute(&pool)
            .await
            .unwrap();
        // 老账目，重建表不能把它弄丢（这正是修复与"从头再来"的区别）。
        sqlx::query(
            "INSERT INTO usage_hourly (bucket_ts, client, provider_tag, model, request_model, requests, quota)
             VALUES (0, 'codex', 'deepseek', 'deepseek-flash', 'deepseek-flash', 3, 900)",
        )
        .execute(&pool)
        .await
        .unwrap();

        migrate(&pool).await.unwrap();

        let cols: Vec<String> = sqlx::query("PRAGMA table_info(usage_hourly)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| sqlx::Row::get::<String, _>(r, "name"))
            .collect();
        assert!(cols.iter().any(|c| c == "request_model"), "实际列：{cols:?}");

        let (requests, request_model): (i64, String) = sqlx::query_as(
            "SELECT requests, request_model FROM usage_hourly WHERE client = 'codex'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(requests, 3, "老账目必须还在");
        assert_eq!(request_model, "deepseek-flash", "老行回填成生效模型名");
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let pool = open_memory().await.unwrap();
        // 再跑一次不应报错，也不应重复插入 route_config 的默认行
        migrate(&pool).await.unwrap();

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_config")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn route_config_defaults_to_default_selector() {
        let pool = open_memory().await.unwrap();
        let final_selector: String =
            sqlx::query_scalar("SELECT final_selector FROM route_config WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(final_selector, "default");
    }

    #[test]
    fn split_statements_ignores_comments_and_blanks() {
        let script = "-- 注释\nCREATE TABLE a (x INT);\n\n-- 又一条注释\nCREATE TABLE b (y INT);\n";
        let stmts = split_statements(script);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("CREATE TABLE a"));
        assert!(stmts[1].starts_with("CREATE TABLE b"));
    }

    /// 把表换成出事故的那副样子：布尔列 `supported` 而不是三态的 `verdict`。
    async fn make_table_legacy(pool: &SqlitePool) {
        sqlx::query("DROP TABLE model_capabilities")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE model_capabilities (
                 provider_id INTEGER NOT NULL,
                 model       TEXT NOT NULL,
                 capability  TEXT NOT NULL,
                 supported   INTEGER NOT NULL,
                 source      TEXT NOT NULL,
                 evidence    TEXT,
                 checked_at  INTEGER NOT NULL,
                 PRIMARY KEY (provider_id, model, capability)
             )",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    /// 半修状态：列名已经改对，但类型被 `RENAME COLUMN` 带成了 INTEGER。
    async fn make_table_half_repaired(pool: &SqlitePool) {
        make_table_legacy(pool).await;
        sqlx::query("ALTER TABLE model_capabilities RENAME COLUMN supported TO verdict")
            .execute(pool)
            .await
            .unwrap();
    }

    /// 建一个真渠道，好让能力行有合法的 provider_id 可指。
    async fn add_provider(pool: &SqlitePool, id: i64) -> i64 {
        sqlx::query(
            "INSERT INTO providers (id, tag, name, kind, base_url, auth_style, protocols,
                 extra_headers, model_mapping, weight, priority, enabled, timeout_ms,
                 created_at, updated_at)
             VALUES (?1,?2,?2,'anthropic','https://x.example.com','none','[]','{}','{}',1,0,1,60000,0,0)",
        )
        .bind(id)
        .bind(format!("p{id}"))
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn schema_of(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query("PRAGMA table_info(model_capabilities)")
            .fetch_all(pool)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    sqlx::Row::get::<String, _>(r, "name"),
                    sqlx::Row::get::<String, _>(r, "type").to_uppercase(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn legacy_bool_column_is_rebuilt_as_text() {
        let pool = open_memory().await.unwrap();
        make_table_legacy(&pool).await;
        add_provider(&pool, 1).await;
        sqlx::query(
            "INSERT INTO model_capabilities
                 (provider_id, model, capability, supported, source, checked_at)
             VALUES (1,'m','tools',1,'probe',0), (1,'m','vision',0,'probe',0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        repair_legacy_model_capabilities(&pool).await.unwrap();

        let schema = schema_of(&pool).await;
        assert!(
            schema.iter().any(|(n, t)| n == "verdict" && t == "TEXT"),
            "列名和类型都要对，实际是 {schema:?}"
        );
        assert!(!schema.iter().any(|(n, _)| n == "supported"));

        let verdicts: Vec<String> =
            sqlx::query_scalar("SELECT verdict FROM model_capabilities ORDER BY capability")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(verdicts, vec!["supported", "unsupported"], "值要换算且行不能丢");
    }

    #[tokio::test]
    async fn a_half_repaired_table_is_also_fixed() {
        // 只改列名的中间状态：名字对了、类型还是 INTEGER。
        // 这种库光看列名会以为没问题，但类型不对迟早出事（以后加 CHECK 或做
        // 数值比较时就炸），所以判据必须把类型也算上。
        let pool = open_memory().await.unwrap();
        make_table_half_repaired(&pool).await;
        add_provider(&pool, 1).await;
        sqlx::query("INSERT INTO model_capabilities VALUES (1,'m','tools',1,'probe',NULL,0)")
            .execute(&pool)
            .await
            .unwrap();

        repair_legacy_model_capabilities(&pool).await.unwrap();

        let schema = schema_of(&pool).await;
        assert!(schema.iter().any(|(n, t)| n == "verdict" && t == "TEXT"));
        let v: String = sqlx::query_scalar("SELECT verdict FROM model_capabilities")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, "supported", "整数 1 要换算成 supported");
    }

    #[tokio::test]
    async fn repair_produces_the_same_schema_as_a_fresh_migration() {
        // 把 db.rs 里那份 DDL 和 migrations.rs 里 v7 的钉在一起。
        // 两处各写一份是不得已（迁移是 &str 常量数组，没法在编译期拼接），
        // 所以用一条测试守住它们不漂移 —— 漂了就会造出两种结构的库。
        let fresh = open_memory().await.unwrap();
        let broken = open_memory().await.unwrap();
        make_table_legacy(&broken).await;
        repair_legacy_model_capabilities(&broken).await.unwrap();

        assert_eq!(schema_of(&fresh).await, schema_of(&broken).await);
    }

    #[tokio::test]
    async fn the_repair_is_a_no_op_on_a_healthy_table() {
        // 全新安装的库跑完迁移就是新结构，修复不该动它，更不该报错 ——
        // 它每次启动都会跑一遍。
        let pool = open_memory().await.unwrap();
        let before = schema_of(&pool).await;

        repair_legacy_model_capabilities(&pool).await.unwrap();
        repair_legacy_model_capabilities(&pool).await.unwrap();

        assert_eq!(schema_of(&pool).await, before);
    }

    #[tokio::test]
    async fn orphan_rows_are_dropped_instead_of_breaking_the_repair() {
        // 旧表没有外键，可能留着渠道已删的行。新表有外键，原样搬会撞约束、
        // 让修复整个失败 —— 修复把应用搞得起不来，比它要修的问题严重得多。
        let pool = open_memory().await.unwrap();
        make_table_legacy(&pool).await;
        add_provider(&pool, 1).await;
        sqlx::query(
            "INSERT INTO model_capabilities
                 (provider_id, model, capability, supported, source, checked_at)
             VALUES (1,'keep','tools',1,'probe',0), (777,'orphan','tools',1,'probe',0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        repair_legacy_model_capabilities(&pool).await.unwrap();

        let models: Vec<String> =
            sqlx::query_scalar("SELECT model FROM model_capabilities ORDER BY model")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(models, vec!["keep"], "孤儿的丢掉，正常的留下");
    }

    #[tokio::test]
    async fn the_version_stamp_is_pulled_back_into_sync() {
        // 出事的库 user_version = 8，比 SCHEMA_VERSION 还大。迁移判定是
        // `current >= SCHEMA_VERSION` 就跳过 —— 不把版本戳拉回来，下次加迁移
        // 时这条库会**静默跳过**它。
        let pool = open_memory().await.unwrap();
        make_table_legacy(&pool).await;
        sqlx::query("PRAGMA user_version = 99")
            .execute(&pool)
            .await
            .unwrap();

        migrate(&pool).await.unwrap();

        let v: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert!(schema_of(&pool).await.iter().any(|(n, t)| n == "verdict" && t == "TEXT"));
    }

    #[tokio::test]
    async fn a_healthy_database_keeps_its_version_stamp() {
        // 版本戳只在真的重建过表时才动。一个结构正常、版本正常的库不该被碰。
        let pool = open_memory().await.unwrap();
        let before: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();

        migrate(&pool).await.unwrap();

        let after: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after);
    }
}
