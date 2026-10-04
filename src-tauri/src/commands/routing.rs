//! 路由规则、selector 与热切换。

use std::sync::Arc;

use tauri::State;

use super::providers::ProbeResult;
use crate::error::{AppError, AppResult};
use crate::routing::rule::{RouteRule, RouteRuleInput};
use crate::shell::AppShell;
use crate::storage::routing::{SelectorInput, SelectorRecord};

// ---------------------------------------------------------------------------
// 路由规则
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_route_rules(shell: State<'_, Arc<AppShell>>) -> AppResult<Vec<RouteRule>> {
    crate::storage::routing::list_rules(&shell.db).await
}

#[tauri::command]
pub async fn upsert_route_rule(
    shell: State<'_, Arc<AppShell>>,
    input: RouteRuleInput,
) -> AppResult<RouteRule> {
    let saved = crate::storage::routing::upsert_rule(&shell.db, &input).await?;
    shell.reload_routing().await?;
    Ok(saved)
}

#[tauri::command]
pub async fn delete_route_rule(shell: State<'_, Arc<AppShell>>, id: i64) -> AppResult<()> {
    crate::storage::routing::delete_rule(&shell.db, id).await?;
    shell.reload_routing().await?;
    Ok(())
}

/// 按给定顺序重排规则。顺序即优先级 —— 靠前的规则先被求值。
#[tauri::command]
pub async fn reorder_route_rules(
    shell: State<'_, Arc<AppShell>>,
    ids: Vec<i64>,
) -> AppResult<()> {
    crate::storage::routing::reorder_rules(&shell.db, &ids).await?;
    shell.reload_routing().await?;
    Ok(())
}

#[tauri::command]
pub async fn set_final_selector(
    shell: State<'_, Arc<AppShell>>,
    tag: String,
) -> AppResult<()> {
    // 兜底 selector 指向不存在的目标会让所有"没命中规则"的请求全部失败，
    // 所以这里先校验一次。
    if crate::storage::routing::get_selector(&shell.db, &tag)
        .await?
        .is_none()
    {
        return Err(AppError::SelectorNotFound(tag));
    }
    crate::storage::routing::set_final_selector(&shell.db, &tag).await?;
    shell.reload_routing().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Selector
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_selectors(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<Vec<SelectorRecord>> {
    crate::storage::routing::list_selectors(&shell.db).await
}

#[tauri::command]
pub async fn upsert_selector(
    shell: State<'_, Arc<AppShell>>,
    input: SelectorInput,
) -> AppResult<SelectorRecord> {
    // 成员必须是真实存在的渠道，否则运行时会选中一个空壳。
    for m in &input.members {
        if !shell.registry.contains(m) {
            return Err(AppError::ProviderNotFound(format!(
                "{m}（selector 成员必须是已启用的渠道）"
            )));
        }
    }

    let saved = crate::storage::routing::upsert_selector(&shell.db, &input).await?;

    // 内存里的实例要立刻反映新的成员表，否则切换会被旧成员表挡住。
    if let Some(sel) = shell.selectors.get(&saved.tag) {
        sel.set_members(saved.members.clone());
        sel.set_mode(saved.mode);
        sel.set_tolerance_ms(saved.tolerance_ms);
        if let Some(cur) = sel.selected_tag() {
            crate::storage::routing::persist_selection(&shell.db, &saved.tag, &cur).await?;
        }
    } else {
        shell.reload_selectors().await?;
    }

    Ok(saved)
}

#[tauri::command]
pub async fn delete_selector(shell: State<'_, Arc<AppShell>>, tag: String) -> AppResult<()> {
    if tag == "default" {
        return Err(AppError::msg(
            "默认 selector 不能删除；如需停用它，请先把兜底 selector 改成别的",
        ));
    }
    crate::storage::routing::delete_selector(&shell.db, &tag).await?;
    shell.reload_selectors().await?;
    Ok(())
}

/// 一键热切换：新请求立刻走新渠道，不需要重启，也不影响在途请求。
#[tauri::command]
pub async fn switch_selector(
    shell: State<'_, Arc<AppShell>>,
    selector: String,
    provider_tag: String,
) -> AppResult<SelectorRecord> {
    let sel = shell
        .selectors
        .get(&selector)
        .ok_or_else(|| AppError::SelectorNotFound(selector.clone()))?;

    sel.set(&provider_tag, "用户手动切换")?;

    // 立即持久化，保证重启后仍是用户选的那个。
    crate::storage::routing::persist_selection(&shell.db, &selector, &provider_tag).await?;

    crate::storage::routing::get_selector(&shell.db, &selector)
        .await?
        .ok_or_else(|| AppError::SelectorNotFound(selector))
}

/// 对 selector 的每个成员跑一次连通性探测，结果按延迟升序。
#[tauri::command]
pub async fn run_urltest(
    shell: State<'_, Arc<AppShell>>,
    selector: String,
) -> AppResult<Vec<ProbeResult>> {
    let sel = shell
        .selectors
        .get(&selector)
        .ok_or_else(|| AppError::SelectorNotFound(selector.clone()))?;

    let mut results = Vec::new();
    for tag in sel.members() {
        match crate::storage::providers::get_by_tag(&shell.db, &tag).await? {
            Some(p) => results.push(super::providers::probe(shell.inner(), &p).await),
            None => results.push(ProbeResult {
                tag,
                ok: false,
                latency_ms: None,
                error: Some("渠道不存在或已停用".into()),
            }),
        }
    }

    // 可用的排前面，同组内延迟低的排前面。
    results.sort_by_key(|r| (!r.ok, r.latency_ms.unwrap_or(i64::MAX)));
    Ok(results)
}
