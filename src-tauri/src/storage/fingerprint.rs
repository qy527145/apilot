//! 指纹检测记录与本地样本库的持久化。
//!
//! 官方参考库不在这里：它是内置/远端只读资产。这里只存两类用户数据：
//! - 检测记录（含完整结果 JSON，便于"加入样本库"回查原始回答）；
//! - 本地样本（用户显式指定权威模型 label 的回答）。

use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::error::{AppError, AppResult};
use crate::util::now_ms;

#[derive(Debug, Clone)]
pub struct NewDetection {
    pub provider_id: Option<i64>,
    pub provider_tag: String,
    pub declared_model: String,
    pub upstream_model: String,
    pub wire: String,
    pub library_source: String,
    pub library_built_at: String,
    pub status: String,
    pub result_json: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct DetectionRow {
    pub provider_id: Option<i64>,
    pub provider_tag: String,
    pub declared_model: String,
    pub upstream_model: String,
    pub status: String,
    pub result_json: String,
}

#[derive(Debug, Clone)]
pub struct SampleInput {
    pub challenge_id: String,
    pub prompt: String,
    pub expected_count: u32,
    pub numbers: Vec<i32>,
    pub text_hash: String,
    pub provider_id: Option<i64>,
    pub provider_tag: String,
    pub declared_model: String,
    pub upstream_model: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredSample {
    pub id: i64,
    pub label: String,
    pub challenge_id: String,
    pub expected_count: u32,
    pub numbers: Vec<i32>,
    pub provider_tag: String,
    pub declared_model: String,
    pub created_at: i64,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocalLabelSummary {
    pub label: String,
    pub samples: usize,
    pub last_added_at: i64,
}

const DETECTION_COLUMNS: &str =
    "provider_id, provider_tag, declared_model, upstream_model, status, result_json";
const SAMPLE_COLUMNS: &str =
    "id, label, challenge_id, expected_count, numbers, provider_tag, declared_model, created_at, note";

fn detection_from_row(row: &sqlx::sqlite::SqliteRow) -> DetectionRow {
    DetectionRow {
        provider_id: row.get("provider_id"),
        provider_tag: row.get("provider_tag"),
        declared_model: row.get("declared_model"),
        upstream_model: row.get("upstream_model"),
        status: row.get("status"),
        result_json: row.get("result_json"),
    }
}

fn sample_from_row(row: &sqlx::sqlite::SqliteRow) -> StoredSample {
    let raw: String = row.get("numbers");
    // 单行损坏不应该让整个样本列表失败；检测路径会得到空数字，
    // 本地 kNN 自然忽略它。
    let numbers = serde_json::from_str::<Vec<i32>>(&raw).unwrap_or_default();
    StoredSample {
        id: row.get("id"),
        label: row.get("label"),
        challenge_id: row.get("challenge_id"),
        expected_count: row.get::<i64, _>("expected_count").max(0) as u32,
        numbers,
        provider_tag: row.get("provider_tag"),
        declared_model: row.get("declared_model"),
        created_at: row.get("created_at"),
        note: row.get("note"),
    }
}

/// 标签规范化：去首尾空白、合并内部空白、转小写。
///
/// 官方模型 ID 本身是小写；自定义 label 也按小写归并，避免
/// "GPT-5.5" 与 "gpt-5.5" 在本地库里变成两个身份。
pub fn normalize_label(label: &str) -> AppResult<String> {
    let normalized = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if normalized.is_empty() {
        return Err(AppError::msg("权威模型不能为空"));
    }
    if normalized.chars().count() > 128 {
        return Err(AppError::msg("权威模型名称过长（最多 128 个字符）"));
    }
    if normalized.chars().any(char::is_control) {
        return Err(AppError::msg("权威模型名称包含控制字符"));
    }
    Ok(normalized)
}

/// 样本去重键：规范化数字串的 SHA-256。
pub fn sample_hash(numbers: &[i32]) -> String {
    let canonical = numbers
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub async fn insert_detection(pool: &SqlitePool, detection: &NewDetection) -> AppResult<i64> {
    let result = sqlx::query(
        "INSERT INTO fingerprint_detections
             (provider_id, provider_tag, declared_model, upstream_model, wire,
              library_source, library_built_at, status, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(detection.provider_id)
    .bind(&detection.provider_tag)
    .bind(&detection.declared_model)
    .bind(&detection.upstream_model)
    .bind(&detection.wire)
    .bind(&detection.library_source)
    .bind(&detection.library_built_at)
    .bind(&detection.status)
    .bind(&detection.result_json)
    .bind(detection.created_at)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn get_detection(pool: &SqlitePool, id: i64) -> AppResult<Option<DetectionRow>> {
    let query = format!("SELECT {DETECTION_COLUMNS} FROM fingerprint_detections WHERE id = ?1");
    let row = sqlx::query(&query).bind(id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(detection_from_row))
}

pub async fn list_labels(pool: &SqlitePool) -> AppResult<Vec<LocalLabelSummary>> {
    let rows = sqlx::query(
        "SELECT label, COUNT(*) AS samples, MAX(created_at) AS last_added_at
         FROM fingerprint_samples
         GROUP BY label
         ORDER BY last_added_at DESC, label ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| LocalLabelSummary {
            label: row.get("label"),
            samples: row.get::<i64, _>("samples").max(0) as usize,
            last_added_at: row.get("last_added_at"),
        })
        .collect())
}

pub async fn list_samples(
    pool: &SqlitePool,
    label: Option<&str>,
) -> AppResult<Vec<StoredSample>> {
    let rows = match label {
        Some(label) => {
            let query = format!(
                "SELECT {SAMPLE_COLUMNS} FROM fingerprint_samples WHERE label = ?1 ORDER BY created_at DESC, id DESC"
            );
            sqlx::query(&query).bind(label).fetch_all(pool).await?
        }
        None => {
            let query = format!(
                "SELECT {SAMPLE_COLUMNS} FROM fingerprint_samples ORDER BY created_at DESC, id DESC"
            );
            sqlx::query(&query).fetch_all(pool).await?
        }
    };
    Ok(rows.iter().map(sample_from_row).collect())
}

/// 检测时加载全部本地样本；按 id 升序保证重复运行的可复现性。
pub async fn list_all_samples(pool: &SqlitePool) -> AppResult<Vec<StoredSample>> {
    let query = format!("SELECT {SAMPLE_COLUMNS} FROM fingerprint_samples ORDER BY id ASC");
    let rows = sqlx::query(&query).fetch_all(pool).await?;
    Ok(rows.iter().map(sample_from_row).collect())
}

/// 批量插入；`INSERT OR IGNORE` 处理同 label + 同 challenge + 同数字的重复。
pub async fn insert_samples(
    pool: &SqlitePool,
    detection_id: i64,
    label: &str,
    samples: &[SampleInput],
) -> AppResult<usize> {
    let created_at = now_ms();
    let mut transaction = pool.begin().await?;
    let mut inserted = 0usize;
    for sample in samples {
        let numbers = serde_json::to_string(&sample.numbers)?;
        let result = sqlx::query(
            "INSERT OR IGNORE INTO fingerprint_samples
                 (detection_id, label, challenge_id, prompt, expected_count, numbers,
                  text_hash, provider_id, provider_tag, declared_model, upstream_model, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(detection_id)
        .bind(label)
        .bind(&sample.challenge_id)
        .bind(&sample.prompt)
        .bind(sample.expected_count as i64)
        .bind(numbers)
        .bind(&sample.text_hash)
        .bind(sample.provider_id)
        .bind(&sample.provider_tag)
        .bind(&sample.declared_model)
        .bind(&sample.upstream_model)
        .bind(&sample.note)
        .bind(created_at)
        .execute(&mut *transaction)
        .await?;
        inserted += result.rows_affected() as usize;
    }
    transaction.commit().await?;
    Ok(inserted)
}

pub async fn delete_sample(pool: &SqlitePool, id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM fingerprint_samples WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_label(pool: &SqlitePool, label: &str) -> AppResult<usize> {
    let result = sqlx::query("DELETE FROM fingerprint_samples WHERE label = ?1")
        .bind(label)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    fn detection() -> NewDetection {
        NewDetection {
            provider_id: Some(7),
            provider_tag: "test".into(),
            declared_model: "gpt-5.5".into(),
            upstream_model: "openai/gpt-5.5".into(),
            wire: "openai_chat".into(),
            library_source: "embedded".into(),
            library_built_at: "2026-10-17T09:36:07.798950+00:00".into(),
            status: "ok".into(),
            result_json: "{}".into(),
            created_at: 1,
        }
    }

    fn sample(challenge: &str, numbers: Vec<i32>) -> SampleInput {
        SampleInput {
            challenge_id: challenge.into(),
            prompt: "write numbers".into(),
            expected_count: 300,
            text_hash: sample_hash(&numbers),
            numbers,
            provider_id: Some(7),
            provider_tag: "test".into(),
            declared_model: "gpt-5.5".into(),
            upstream_model: "openai/gpt-5.5".into(),
            note: None,
        }
    }

    #[test]
    fn label_normalization_collapses_case_and_whitespace() {
        assert_eq!(normalize_label("  GPT-5.5  ").unwrap(), "gpt-5.5");
        assert_eq!(normalize_label("my   model").unwrap(), "my model");
        assert!(normalize_label("   ").is_err());
        assert!(normalize_label(&"x".repeat(129)).is_err());
    }

    #[tokio::test]
    async fn detection_round_trips() {
        let pool = pool().await;
        let id = insert_detection(&pool, &detection()).await.unwrap();
        let row = get_detection(&pool, id).await.unwrap().unwrap();
        assert_eq!(row.declared_model, "gpt-5.5");
        assert_eq!(row.upstream_model, "openai/gpt-5.5");
        assert_eq!(row.status, "ok");
        assert!(get_detection(&pool, id + 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_samples_are_ignored() {
        let pool = pool().await;
        let id = insert_detection(&pool, &detection()).await.unwrap();
        let numbers = vec![1, 2, 3, 4];
        let samples = vec![sample("query-01", numbers.clone())];
        assert_eq!(
            insert_samples(&pool, id, "gpt-5.5", &samples).await.unwrap(),
            1
        );
        assert_eq!(
            insert_samples(&pool, id, "gpt-5.5", &samples).await.unwrap(),
            0
        );

        // 不同 label 下同一段数字允许保留（用户可能是在纠正标注）。
        assert_eq!(
            insert_samples(&pool, id, "claude-opus-5.5", &samples)
                .await
                .unwrap(),
            1
        );
        let labels = list_labels(&pool).await.unwrap();
        assert_eq!(labels.len(), 2);
    }

    #[tokio::test]
    async fn deleting_a_label_only_removes_its_samples() {
        let pool = pool().await;
        let id = insert_detection(&pool, &detection()).await.unwrap();
        insert_samples(
            &pool,
            id,
            "gpt-5.5",
            &[sample("query-01", vec![1, 2, 3])],
        )
        .await
        .unwrap();
        insert_samples(
            &pool,
            id,
            "claude-opus-5.5",
            &[sample("query-01", vec![4, 5, 6])],
        )
        .await
        .unwrap();

        assert_eq!(delete_label(&pool, "gpt-5.5").await.unwrap(), 1);
        let samples = list_all_samples(&pool).await.unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].label, "claude-opus-5.5");
        assert_eq!(samples[0].numbers, vec![4, 5, 6]);
    }

    #[tokio::test]
    async fn list_samples_filters_by_label() {
        let pool = pool().await;
        let id = insert_detection(&pool, &detection()).await.unwrap();
        insert_samples(&pool, id, "a", &[sample("q1", vec![1])])
            .await
            .unwrap();
        insert_samples(&pool, id, "b", &[sample("q1", vec![2])])
            .await
            .unwrap();
        let only_b = list_samples(&pool, Some("b")).await.unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].numbers, vec![2]);
        assert_eq!(list_samples(&pool, None).await.unwrap().len(), 2);
    }
}
