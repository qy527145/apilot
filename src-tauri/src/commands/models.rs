//! 模型视角的命令：模型目录、渠道强制选择、候选渠道、测速。
//!
//! 与 `commands/providers.rs` 的分工：那边是"这个渠道提供哪些模型"（渠道视角），
//! 这边是"这个模型在哪些渠道上有、优先走哪个"（模型视角）。同一张
//! `provider_models` 表的两个方向，存储层统一维护派生列，不会互相覆盖。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::storage::models::{ModelCandidate, ModelCatalogEntry, ModelPolicyRecord};
use crate::storage::providers::ModelCandidateRow;

/// 全局模型替换策略的读写（写在 `config::settings` 里，这里只是转个手）。
#[tauri::command]
pub fn get_model_policy(shell: State<'_, Arc<AppShell>>) -> crate::config::settings::ModelPolicy {
    shell.settings().model_policy.clone()
}

/// 模型目录：所有被声明过的模型 + 各自的候选渠道、强制渠道、当前主渠道。
#[tauri::command]
pub async fn list_model_catalog(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<Vec<ModelCatalogEntry>> {
    let db = &shell.db;
    let models = crate::storage::providers::all_declared_models(db).await?;
    let policies = crate::storage::model_policies::all(db).await?;

    let mut out = Vec::with_capacity(models.len());
    for model in models {
        // 用 candidates_for_model 而不是 candidate_channels：模型页要把停用的
        // 渠道也列出来，否则停用之后就再也启用不回来了。
        let rows = crate::storage::providers::candidates_for_model(db, &model).await?;
        let channels = crate::storage::providers::candidate_channels(db, &model).await?;

        let candidates: Vec<ModelCandidate> = rows
            .into_iter()
            .map(|r| ModelCandidate {
                provider_tag: r.provider_tag,
                provider_name: r.provider_name,
                upstream_model: r.upstream_model,
                priority: r.priority,
                enabled: r.enabled,
            })
            .collect();

        let policy = policies.iter().find(|p| p.model == model).cloned();

        // 主渠道只在**可用**的候选里选，与请求时的行为一致。
        let usable: Vec<ModelCandidate> =
            candidates.iter().filter(|c| c.enabled).cloned().collect();
        let primary = primary_of(&policy, &usable).or_else(|| {
            // 没配策略时，展示 selector 会选谁 —— 让用户看得见"现在走的是哪个"。
            channels.first().map(|c| c.provider.tag.clone())
        });

        out.push(ModelCatalogEntry {
            model,
            candidates,
            policy,
            primary,
        });
    }

    Ok(out)
}

/// 当前会走哪个渠道。
///
/// 有策略行就按候选顺序算（点名的渠道在前）；没有行返回 `None`，由调用方
/// 回落成"selector 会选谁"。与请求时的行为同口径。
fn primary_of(policy: &Option<ModelPolicyRecord>, candidates: &[ModelCandidate]) -> Option<String> {
    let p = policy.as_ref()?;
    let ranked: Vec<crate::routing::model_select::Candidate> = candidates
        .iter()
        .map(|c| crate::routing::model_select::Candidate {
            tag: c.provider_tag.clone(),
            priority: c.priority,
        })
        .collect();
    crate::routing::model_select::order(Some(p), ranked)
        .into_iter()
        .next()
}

/// 设某个模型的渠道选择策略。
#[tauri::command]
pub async fn upsert_model_policy(
    shell: State<'_, Arc<AppShell>>,
    input: ModelPolicyRecord,
) -> AppResult<ModelPolicyRecord> {
    if input.model.trim().is_empty() {
        return Err(AppError::msg("模型名不能为空"));
    }
    crate::storage::model_policies::upsert(&shell.db, &input).await?;
    crate::storage::model_policies::get(&shell.db, &input.model)
        .await?
        .ok_or_else(|| AppError::msg("策略写入后读不回来"))
}

/// 交回路由规则与 selector 管。
#[tauri::command]
pub async fn reset_model_policy(
    shell: State<'_, Arc<AppShell>>,
    model: String,
) -> AppResult<()> {
    crate::storage::model_policies::delete(&shell.db, &model).await
}

/// 手动切换某模型的主渠道。
///
/// 等价于把 `active_provider` 设成它。单独开一个命令是因为这是最高频的操作
/// （就像切换代理节点），不该要求前端回传整个策略对象。
#[tauri::command]
pub async fn switch_model_channel(
    shell: State<'_, Arc<AppShell>>,
    model: String,
    provider_tag: String,
) -> AppResult<ModelPolicyRecord> {
    let next = ModelPolicyRecord {
        model: model.clone(),
        active_provider: Some(provider_tag),
    };
    crate::storage::model_policies::upsert(&shell.db, &next).await?;
    Ok(next)
}

/// 全量替换某模型的候选渠道（增删渠道、改上游名、调优先级）。
#[tauri::command]
pub async fn set_model_candidates(
    shell: State<'_, Arc<AppShell>>,
    model: String,
    candidates: Vec<ModelCandidateRow>,
) -> AppResult<()> {
    if model.trim().is_empty() {
        return Err(AppError::msg("模型名不能为空"));
    }
    crate::storage::providers::set_model_candidates(&shell.db, &model, &candidates).await?;

    // 上游模型名改了要重载渠道快照：请求改写读的是内存里那份 Provider，
    // 不重载的话新名要等重启才生效。
    shell.reload_providers().await?;
    Ok(())
}

/// 对某模型的候选渠道跑一次测速，结果按延迟升序返回。
///
/// 不再写进任何缓存：选渠道已经不看延迟了，这里只是给用户看"这条到底通不通、
/// 大概多快"，所以结果随响应返回即可。
#[tauri::command]
pub async fn probe_model_candidates(
    shell: State<'_, Arc<AppShell>>,
    model: String,
) -> AppResult<Vec<super::providers::ProbeResult>> {
    let channels = crate::storage::providers::candidate_channels(&shell.db, &model).await?;

    let mut results = Vec::with_capacity(channels.len());
    for c in channels {
        results.push(super::providers::probe(shell.inner(), &c.provider).await);
    }

    results.sort_by_key(|r| (!r.ok, r.latency_ms.unwrap_or(i64::MAX)));
    Ok(results)
}

/// 供前端确认渠道 tag 是否有效（模型页的下拉要用）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelOption {
    pub model: String,
    pub provider_count: usize,
}

#[tauri::command]
pub async fn list_model_options(shell: State<'_, Arc<AppShell>>) -> AppResult<Vec<ModelOption>> {
    let catalog = list_model_catalog(shell).await?;
    Ok(catalog
        .into_iter()
        .map(|e| ModelOption {
            model: e.model,
            provider_count: e.candidates.len(),
        })
        .collect())
}
