//! 响应缓存管理。

use std::sync::Arc;

use tauri::State;

use crate::cache::policy::{CachePolicy, CacheScope};
use crate::cache::store::CacheStats;
use crate::error::AppResult;
use crate::shell::AppShell;

#[tauri::command]
pub async fn cache_stats(shell: State<'_, Arc<AppShell>>) -> AppResult<CacheStats> {
    shell.cache.stats(&shell.db).await
}

#[tauri::command]
pub async fn clear_cache(
    shell: State<'_, Arc<AppShell>>,
    scope: CacheScope,
) -> AppResult<u64> {
    let removed = shell.cache.clear(&shell.db, &scope).await?;
    if let Ok(stats) = shell.cache.stats(&shell.db).await {
        shell.events.cache(&stats);
    }
    Ok(removed)
}

#[tauri::command]
pub fn get_cache_policy(shell: State<'_, Arc<AppShell>>) -> CachePolicy {
    shell.cache.policy()
}

#[tauri::command]
pub async fn set_cache_policy(
    shell: State<'_, Arc<AppShell>>,
    policy: CachePolicy,
) -> AppResult<CachePolicy> {
    let policy = policy.normalized();
    shell.cache.set_policy(policy.clone());

    // 同步回设置表，避免重启后又变回旧值。
    let mut settings = shell.settings().as_ref().clone();
    settings.cache_enabled = policy.enabled;
    settings.cache_ttl_secs = policy.ttl_secs;
    settings.cache_max_entries = policy.max_entries;
    shell.update_settings(settings).await?;

    Ok(policy)
}
