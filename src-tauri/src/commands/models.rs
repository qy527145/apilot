//! 模型视角的命令：模型目录、渠道选择策略、候选渠道、测速。
//!
//! 与 `commands/providers.rs` 的分工：那边是"这个渠道提供哪些模型"（渠道视角），
//! 这边是"这个模型在哪些渠道上有、优先走哪个"（模型视角）。同一张
//! `provider_models` 表的两个方向，存储层统一维护派生列，不会互相覆盖。

use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::storage::models::{
    ModelCandidate, ModelCatalogEntry, ModelPolicyRecord, ModelStrategy,
};
use crate::storage::providers::ModelCandidateRow;

/// 全局模型替换策略的读写（写在 `config::settings` 里，这里只是转个手）。
#[tauri::command]
pub fn get_model_policy(shell: State<'_, Arc<AppShell>>) -> crate::config::settings::ModelPolicy {
    shell.settings().model_policy.clone()
}

/// 模型目录：所有被声明过的模型 + 各自的候选渠道、策略、当前主渠道。
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
                latency_ms: shell.probe_latency.get(&r.provider_tag).map(|v| *v),
                provider_tag: r.provider_tag,
                provider_name: r.provider_name,
                upstream_model: r.upstream_model,
                priority: r.priority,
                weight: r.weight,
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
/// 加权随机策略返回 `None` —— 它每次请求都重抽，没有"当前"可言，
/// 界面显示成"按权重随机"比显示一个会跳的数字诚实。
fn primary_of(policy: &Option<ModelPolicyRecord>, candidates: &[ModelCandidate]) -> Option<String> {
    let p = policy.as_ref()?;
    if let Some(active) = p.active_provider.as_deref() {
        if candidates.iter().any(|c| c.provider_tag == active) {
            return Some(active.to_string());
        }
    }
    if p.strategy == ModelStrategy::Weight {
        return None;
    }
    // 展示用 roll=0（最高权重那个），请求时才会随机。
    let ranked: Vec<crate::routing::model_select::Candidate> = candidates
        .iter()
        .map(|c| crate::routing::model_select::Candidate {
            tag: c.provider_tag.clone(),
            priority: c.priority,
            weight: c.weight,
            latency_ms: c.latency_ms,
        })
        .collect();
    crate::routing::model_select::order(Some(p), ranked, 0.0)
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
    let existing = crate::storage::model_policies::get(&shell.db, &model).await?;
    let next = ModelPolicyRecord {
        model: model.clone(),
        // 之前没配过就沿用默认策略 —— 用户只是点了个渠道，不该顺便改变选法。
        strategy: existing.map(|p| p.strategy).unwrap_or_default(),
        active_provider: Some(provider_tag),
    };
    crate::storage::model_policies::upsert(&shell.db, &next).await?;
    Ok(next)
}

/// 全量替换某模型的候选渠道（增删渠道、改上游名、调优先级/权重）。
#[tauri::command]
pub async fn set_model_candidates(
    shell: State<'_, Arc<AppShell>>,
    model: String,
    candidates: Vec<ModelCandidateRow>,
) -> AppResult<()> {
    if model.trim().is_empty() {
        return Err(AppError::msg("模型名不能为空"));
    }
    crate::storage::providers::set_model_candidates(&shell.db, &model, &candidates).await
}

/// 对某模型的候选渠道跑一次测速，结果按延迟升序返回并写进缓存。
///
/// 缓存供「按延迟」策略排序使用；不落库，重启后重新测即可。
#[tauri::command]
pub async fn probe_model_candidates(
    shell: State<'_, Arc<AppShell>>,
    model: String,
) -> AppResult<Vec<super::providers::ProbeResult>> {
    let channels = crate::storage::providers::candidate_channels(&shell.db, &model).await?;

    let mut results = Vec::with_capacity(channels.len());
    for c in channels {
        let r = super::providers::probe(shell.inner(), &c.provider).await;
        if let Some(ms) = r.latency_ms {
            shell
                .probe_latency
                .insert(c.provider.tag.clone(), ms);
        } else {
            // 探测失败要**清掉**旧值：留着上一次的延迟会让"按延迟选渠道"
            // 继续把流量导向一个现在已经连不上的渠道。
            shell.probe_latency.remove(&c.provider.tag);
        }
        results.push(r);
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
