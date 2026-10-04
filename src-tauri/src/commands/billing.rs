//! 计费与统计。

use std::sync::Arc;

use serde::Deserialize;
use tauri::State;

use crate::billing::pricing::ModelPricing;
use crate::error::AppResult;
use crate::shell::AppShell;
use crate::storage::aggregates::{self, BillingBucket, BillingSummary, GroupBy, HourlyPoint};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TimeRange {
    /// unix 毫秒。
    pub from: i64,
    pub to: i64,
}

impl TimeRange {
    /// 夹住区间：前端可能传反，也可能传一个跨年的区间。
    pub fn normalized(self) -> Self {
        let (from, to) = if self.from <= self.to {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        };
        // 上限 90 天，防止一次查询把整个库扫一遍。
        let cap = 90 * 24 * 3600 * 1000;
        if to - from > cap {
            Self {
                from: to - cap,
                to,
            }
        } else {
            Self { from, to }
        }
    }
}

#[tauri::command]
pub async fn list_pricing(shell: State<'_, Arc<AppShell>>) -> AppResult<Vec<ModelPricing>> {
    crate::storage::pricing::list(&shell.db).await
}

#[tauri::command]
pub async fn upsert_pricing(
    shell: State<'_, Arc<AppShell>>,
    input: ModelPricing,
) -> AppResult<ModelPricing> {
    let saved = crate::storage::pricing::upsert(&shell.db, &input).await?;
    shell.reload_pricing().await?;
    Ok(saved)
}

#[tauri::command]
pub async fn delete_pricing(shell: State<'_, Arc<AppShell>>, model: String) -> AppResult<()> {
    crate::storage::pricing::delete(&shell.db, &model).await?;
    shell.reload_pricing().await?;
    Ok(())
}

#[tauri::command]
pub async fn billing_summary(
    shell: State<'_, Arc<AppShell>>,
    range: TimeRange,
    group_by: String,
) -> AppResult<Vec<BillingBucket>> {
    let range = range.normalized();
    let group = GroupBy::parse(&group_by).unwrap_or(GroupBy::Model);
    aggregates::summary_by(&shell.db, range.from, range.to, group).await
}

#[tauri::command]
pub async fn billing_totals(
    shell: State<'_, Arc<AppShell>>,
    range: TimeRange,
) -> AppResult<BillingSummary> {
    let range = range.normalized();
    aggregates::summary(&shell.db, range.from, range.to).await
}

#[tauri::command]
pub async fn billing_timeseries(
    shell: State<'_, Arc<AppShell>>,
    range: TimeRange,
) -> AppResult<Vec<HourlyPoint>> {
    let range = range.normalized();
    aggregates::timeseries(&shell.db, range.from, range.to).await
}
