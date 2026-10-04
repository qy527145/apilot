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
