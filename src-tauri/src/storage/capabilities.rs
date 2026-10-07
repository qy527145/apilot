//! 模型能力的读写。
//!
//! 能力按 **(渠道, 模型)** 存，不是只按模型名：同一个模型名挂在不同中转/代理
//! 后面行为可能不同，而"这条渠道实际给不给过 tools"才是用户要的答案。
//!
//! 每个键只有一行，优先级在写入时解决 —— 见 [`RANK_EXPR`]。放在 SQL 里而不是
//! 读的时候判：那样只有一处真源，界面和以后可能的路由决策拿到的必然一致。

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::error::AppResult;
use crate::util::now_ms;

/// 来源标记。
///
/// `probe` 是往这条渠道真发一次请求测出来的，反映**这条渠道的真实行为**；
/// `catalog` 是从公开模型目录拉的，零成本但只反映**模型本身宣称的**能力。
/// 中转商把 tools 剥掉这种事只有前者看得见。
pub const SOURCE_PROBE: &str = "probe";
pub const SOURCE_CATALOG: &str = "catalog";

/// Apilot 关心的能力维度。
///
/// 只列这三种是因为它们**会导致请求失败或结果不对**：不支持 tools 却被要求
/// 传 tools、不支持图片却收到图片，上游都会返错。像"温度是否可调"这种无害的
/// 差异不在此列 —— 能力表不是模型规格书。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 思考 / 推理链。
    Reasoning,
    /// 工具（函数）调用。
    Tools,
    /// 图片输入。
    Vision,
}

impl Capability {
    pub const ALL: [Capability; 3] = [Self::Reasoning, Self::Tools, Self::Vision];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Tools => "tools",
            Self::Vision => "vision",
        }
    }

    /// 未知值返回 `None` 而不是报错 —— 老版本写进去的、或以后删掉的能力维度
    /// 不该让整个列表读不出来。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reasoning" => Some(Self::Reasoning),
            "tools" => Some(Self::Tools),
            "vision" => Some(Self::Vision),
            _ => None,
        }
    }
}

/// 判定结果。**刻意是三态。**
///
/// 探测「支不支持工具」时，模型完全可能只是那一次没调工具 —— 模型不是每次都调。
/// 把它记成「不支持」是撒谎，用户会据此把一条本来能用的渠道判死刑。所以
/// 「不确定」必须是一个能存下来、能显示出来的独立状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityVerdict {
    Supported,
    Unsupported,
    /// 测了，但这次观察到的现象说明不了问题。
    Inconclusive,
}

impl CapabilityVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// 目录只给布尔值，没有"不确定"这一说。
    pub fn from_bool(v: bool) -> Self {
        if v {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "supported" => Some(Self::Supported),
            "unsupported" => Some(Self::Unsupported),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRecord {
    pub provider_id: i64,
    pub model: String,
    pub capability: Capability,
    pub verdict: CapabilityVerdict,
    pub source: String,
    /// 判定依据。探测时是**具体观察到的现象**（上游回了什么、有没有思考块），
    /// 排查"凭什么叫它不支持"时全靠它。
    pub evidence: Option<String>,
    pub checked_at: i64,
}

/// 覆盖优先级，从低到高。
///
/// - 1：探测但不确定 —— 比目录还弱。目录好歹是个明确断言，而"我测了但看不出来"
///   不该因为晚测一次就把目录的结论顶掉。
/// - 2：目录断言。
/// - 3：探测且明确 —— 反映这条渠道的真实行为，压过一切。
///
/// `prefix` 必须是 `""`（指已有行）或 `"excluded."`（指这次要写入的行）。
/// **两个 rank 要用不同的前缀**：SQLite 的 upsert 里，`DO UPDATE ... WHERE` 的
/// 裸列名一律指已有行，把它当新行用会让比较退化成"自己和自己比"，`>=` 恒真，
/// 保护规则整个失效 —— 而且不报错。
fn rank_expr(prefix: &str) -> String {
    format!(
        "CASE WHEN {prefix}source = 'probe' AND {prefix}verdict != 'inconclusive' THEN 3 \
         WHEN {prefix}source = 'catalog' THEN 2 ELSE 1 END"
    )
}

fn row_to_record(r: &sqlx::sqlite::SqliteRow) -> Option<CapabilityRecord> {
    Some(CapabilityRecord {
        provider_id: r.get("provider_id"),
        model: r.get("model"),
        capability: Capability::parse(&r.get::<String, _>("capability"))?,
        verdict: CapabilityVerdict::parse(&r.get::<String, _>("verdict"))?,
        source: r.get("source"),
        evidence: r.get("evidence"),
        checked_at: r.get("checked_at"),
    })
}

const COLUMNS: &str = "provider_id, model, capability, verdict, source, evidence, checked_at";

/// 写入一批能力判定，返回真正落库的行数。
///
/// **优先级规则就在这条 SQL 的 `WHERE` 里。** 写入端的 `WHERE` 让低优先级的
/// 写入碰不动高优先级的已有行；表里每个键因此只有一行、只有一种真值。
pub async fn upsert(pool: &SqlitePool, records: &[CapabilityRecord]) -> AppResult<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let now = now_ms();
    let mut tx = pool.begin().await?;
    let mut n = 0u64;

    for r in records {
        let res = sqlx::query(&format!(
            "INSERT INTO model_capabilities
                 (provider_id, model, capability, verdict, source, evidence, checked_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(provider_id, model, capability) DO UPDATE SET
                 verdict    = excluded.verdict,
                 source     = excluded.source,
                 evidence   = excluded.evidence,
                 checked_at = excluded.checked_at
             WHERE {new_rank} >= {old_rank}",
            new_rank = rank_expr("excluded."),
            old_rank = rank_expr(""),
        ))
        .bind(r.provider_id)
        .bind(&r.model)
        .bind(r.capability.as_str())
        .bind(r.verdict.as_str())
        .bind(&r.source)
        .bind(&r.evidence)
        .bind(if r.checked_at > 0 { r.checked_at } else { now })
        .execute(&mut *tx)
        .await?;
        n += res.rows_affected();
    }

    tx.commit().await?;
    Ok(n)
}

pub async fn list_for_provider(
    pool: &SqlitePool,
    provider_id: i64,
) -> AppResult<Vec<CapabilityRecord>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM model_capabilities WHERE provider_id = ?1 ORDER BY model, capability"
    ))
    .bind(provider_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().filter_map(row_to_record).collect())
}

/// 清掉某渠道下指定来源的记录，返回删除行数。
///
/// 重新导入目录前用它清掉上一次的 catalog 结论 —— 否则目录里已经删掉的模型
/// 会一直留着旧能力。**probe 行不受影响**：那是花过 token 测出来的，
/// 不能因为点了一次"更新能力"就丢掉。
pub async fn clear_source(pool: &SqlitePool, provider_id: i64, source: &str) -> AppResult<u64> {
    let r = sqlx::query("DELETE FROM model_capabilities WHERE provider_id = ?1 AND source = ?2")
        .bind(provider_id)
        .bind(source)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::storage::db::open_memory().await.unwrap()
    }

    /// 能力表有外键指向 providers，测试里必须先建一个渠道。
    async fn provider(pool: &SqlitePool, tag: &str) -> i64 {
        sqlx::query(
            "INSERT INTO providers (tag, name, kind, base_url, auth_style, protocols,
                 extra_headers, model_mapping, weight, priority, enabled, timeout_ms,
                 created_at, updated_at)
             VALUES (?1,?1,'anthropic','https://x.example.com','none','[]','{}','{}',1,0,1,60000,0,0)",
        )
        .bind(tag)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    fn rec(
        pid: i64,
        model: &str,
        cap: Capability,
        verdict: CapabilityVerdict,
        source: &str,
    ) -> CapabilityRecord {
        CapabilityRecord {
            provider_id: pid,
            model: model.into(),
            capability: cap,
            verdict,
            source: source.into(),
            evidence: None,
            checked_at: 0,
        }
    }

    #[tokio::test]
    async fn insert_and_read_back() {
        let p = pool().await;
        let pid = provider(&p, "a").await;
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verdict, CapabilityVerdict::Supported);
        assert_eq!(got[0].capability, Capability::Tools);
        assert!(got[0].checked_at > 0, "没给时间就该填当前时间");
    }

    #[tokio::test]
    async fn catalog_never_overwrites_a_definite_probe_result() {
        // 整个能力表最重要的一条：中转商偷偷剥掉 tools 只有实测看得见，
        // 一次目录刷新不能把它抹掉。
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Unsupported, SOURCE_PROBE)],
        )
        .await
        .unwrap();
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got.len(), 1, "同一个键不该出现两行");
        assert_eq!(got[0].verdict, CapabilityVerdict::Unsupported, "实测赢");
        assert_eq!(got[0].source, SOURCE_PROBE);
    }

    #[tokio::test]
    async fn probe_overwrites_a_catalog_result() {
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[rec(pid, "m", Capability::Vision, CapabilityVerdict::Supported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();
        upsert(
            &p,
            &[rec(pid, "m", Capability::Vision, CapabilityVerdict::Unsupported, SOURCE_PROBE)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got[0].verdict, CapabilityVerdict::Unsupported);
        assert_eq!(got[0].source, SOURCE_PROBE);
    }

    #[tokio::test]
    async fn an_inconclusive_probe_does_not_displace_a_catalog_answer() {
        // 目录明确说支持，而这次探测只是"看不出来" —— 不该因为晚测一次就把
        // 明确结论降级成"未知"。否则反复按探测按钮，答案只会越来越糊。
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Inconclusive, SOURCE_PROBE)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got[0].verdict, CapabilityVerdict::Supported);
        assert_eq!(got[0].source, SOURCE_CATALOG);
    }

    #[tokio::test]
    async fn an_inconclusive_probe_still_records_something_out_of_nothing() {
        // 但库里什么都没有时，"测了但看不出来"也比"完全没测过"强 ——
        // 界面能显示"测过，未知"，而不是一片空白让人以为没这功能。
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[rec(pid, "m", Capability::Reasoning, CapabilityVerdict::Inconclusive, SOURCE_PROBE)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].verdict, CapabilityVerdict::Inconclusive);
    }

    #[tokio::test]
    async fn a_catalog_row_can_be_refreshed_by_a_later_catalog_import() {
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Unsupported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_CATALOG)],
        )
        .await
        .unwrap();

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got[0].verdict, CapabilityVerdict::Supported);
    }

    #[tokio::test]
    async fn clearing_one_source_leaves_the_other() {
        let p = pool().await;
        let pid = provider(&p, "a").await;

        upsert(
            &p,
            &[
                rec(pid, "m1", Capability::Tools, CapabilityVerdict::Supported, SOURCE_CATALOG),
                rec(pid, "m1", Capability::Reasoning, CapabilityVerdict::Supported, SOURCE_PROBE),
            ],
        )
        .await
        .unwrap();

        let n = clear_source(&p, pid, SOURCE_CATALOG).await.unwrap();
        assert_eq!(n, 1);
        let left = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].source, SOURCE_PROBE, "花过 token 的实测结果不该被清掉");
    }

    #[tokio::test]
    async fn deleting_a_provider_cascades() {
        let p = pool().await;
        let pid = provider(&p, "gone").await;
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_PROBE)],
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM providers WHERE id = ?1")
            .bind(pid)
            .execute(&p)
            .await
            .unwrap();

        assert!(
            list_for_provider(&p, pid).await.unwrap().is_empty(),
            "渠道删了能力也该跟着走"
        );
    }

    #[tokio::test]
    async fn unrecognized_rows_are_skipped_not_fatal() {
        // 以后删掉某个能力维度、或改了 verdict 的取值时，老行不该让整个列表读不出来。
        let p = pool().await;
        let pid = provider(&p, "a").await;
        upsert(
            &p,
            &[rec(pid, "m", Capability::Tools, CapabilityVerdict::Supported, SOURCE_PROBE)],
        )
        .await
        .unwrap();
        // 两行各用不同的 model，免得撞上已有那行的主键 —— 那样测的就变成了
        // 主键约束，而不是"认不出的行被跳过"。
        for (model, cap, verdict) in [("m", "telepathy", "supported"), ("m2", "tools", "sort of")] {
            sqlx::query(
                "INSERT INTO model_capabilities
                     (provider_id, model, capability, verdict, source, checked_at)
                 VALUES (?1,?2,?3,?4,'probe',0)",
            )
            .bind(pid)
            .bind(model)
            .bind(cap)
            .bind(verdict)
            .execute(&p)
            .await
            .unwrap();
        }

        let got = list_for_provider(&p, pid).await.unwrap();
        assert_eq!(got.len(), 1, "认不出的那两行跳过就好");
        assert_eq!(got[0].capability, Capability::Tools);
    }

    #[test]
    fn names_roundtrip() {
        for c in Capability::ALL {
            assert_eq!(Capability::parse(c.as_str()), Some(c));
        }
        for v in [
            CapabilityVerdict::Supported,
            CapabilityVerdict::Unsupported,
            CapabilityVerdict::Inconclusive,
        ] {
            assert_eq!(CapabilityVerdict::parse(v.as_str()), Some(v));
        }
        assert_eq!(Capability::parse("nonsense"), None);
        assert_eq!(CapabilityVerdict::parse("nonsense"), None);
    }

    #[test]
    fn the_catalog_only_ever_asserts_definite_answers() {
        assert_eq!(CapabilityVerdict::from_bool(true), CapabilityVerdict::Supported);
        assert_eq!(CapabilityVerdict::from_bool(false), CapabilityVerdict::Unsupported);
    }
}
