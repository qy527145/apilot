//! SQLite 连接池与迁移执行。

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, SqlitePool};

use super::migrations::{MIGRATIONS, SCHEMA_VERSION};
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
    async fn migrations_apply_cleanly_on_memory_db() {
        let pool = open_memory().await.expect("迁移应当成功");

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
}
