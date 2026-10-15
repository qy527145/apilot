//! 对话级模型固定的读写。判定逻辑在 `routing::session_pin`。

use sqlx::{Row, SqlitePool};

use crate::error::AppResult;
use crate::routing::session_pin::{PinScope, SessionPin};

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<SessionPin>> {
    let rows = sqlx::query("SELECT session_id, model, scope, created_at FROM session_model_pins")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| SessionPin {
            session_id: r.get("session_id"),
            model: r.get("model"),
            scope: PinScope::parse(r.get::<String, _>("scope").as_str()),
            created_at: r.get("created_at"),
        })
        .collect())
}

/// 固定（已固定过就改成新的模型与范围）。
pub async fn upsert(pool: &SqlitePool, pin: &SessionPin) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO session_model_pins (session_id, model, scope, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO UPDATE SET model = excluded.model, scope = excluded.scope",
    )
    .bind(&pin.session_id)
    .bind(&pin.model)
    .bind(pin.scope.as_str())
    .bind(pin.created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, session_id: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM session_model_pins WHERE session_id = ?1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(model: &str, scope: PinScope) -> SessionPin {
        SessionPin { session_id: "S".into(), model: model.into(), scope, created_at: 7 }
    }

    #[tokio::test]
    async fn upsert_replaces_model_and_scope_but_keeps_created_at() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        upsert(&pool, &pin("flash", PinScope::Tools)).await.unwrap();
        let mut again = pin("opus", PinScope::All);
        again.created_at = 99;
        upsert(&pool, &again).await.unwrap();

        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].model, "opus");
        assert_eq!(all[0].scope, PinScope::All);
        assert_eq!(all[0].created_at, 7, "首次固定的时间不该被改写");
    }

    #[tokio::test]
    async fn delete_removes_the_pin() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        upsert(&pool, &pin("flash", PinScope::Tools)).await.unwrap();
        delete(&pool, "S").await.unwrap();
        assert!(list(&pool).await.unwrap().is_empty());
        // 删一个不存在的不是错误：界面上连点两次撤销不该报错。
        delete(&pool, "S").await.unwrap();
    }
}
