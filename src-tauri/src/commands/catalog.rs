//! 上游目录的拉取与导入（价格 + 能力）。
//!
//! 两个功能共用同一个网络集成不是巧合：公开的模型目录本来就同时维护价格和
//! 能力标志，分成两次下载只是把同一个文件拉两遍。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::billing::pricing::ModelPricing;
use crate::catalog::{
    self, index_by_model, to_pricing, CatalogModel, CatalogSource,
};
use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::storage::capabilities::{
    self, Capability, CapabilityRecord, CapabilityVerdict, SOURCE_CATALOG,
};
use crate::storage::models::Provider;
use crate::storage::{pricing, providers as provider_store};
use crate::util::now_ms;

/// 缓存的新鲜期。目录本身按天更新，十分钟的缓存足够覆盖"预览完再应用"这一串
/// 操作，又不会让用户拿昨天的数据做决定。
const CACHE_TTL: Duration = Duration::from_secs(600);

/// 拿目录，能命中缓存就不重新下载。
async fn catalog_for(
    shell: &Arc<AppShell>,
    source: CatalogSource,
    force_refresh: bool,
) -> AppResult<Arc<Vec<CatalogModel>>> {
    if !force_refresh {
        if let Some(hit) = shell.catalog_cache.get(&source) {
            let (at, models) = hit.value().clone();
            if now_ms() - at < CACHE_TTL.as_millis() as i64 {
                return Ok(models);
            }
        }
    }

    // 走全局代理设置的客户端。这两个域名在没有代理的网络里连 TLS 握手都过不去，
    // 所以这里不是可选项。
    let client = shell.registry.client();
    let models = Arc::new(catalog::fetch(&client, source).await?);
    shell
        .catalog_cache
        .insert(source, (now_ms(), models.clone()));
    Ok(models)
}

/* ================================================================== */
/* 价格                                                               */
/* ================================================================== */

/// 预览结果。`rows` 可能很长（目录有几千个模型），但界面只需要展示要变的那些。
#[derive(Debug, Serialize)]
pub struct CatalogPreview {
    pub source: CatalogSource,
    pub source_label: String,
    /// 目录里的条目总数与其中带价格的。
    pub total: usize,
    pub priced: usize,
    pub rows: Vec<pricing::PriceDiffRow>,
    pub stats: pricing::PriceImportStats,
}

fn count_stats(rows: &[pricing::PriceDiffRow]) -> pricing::PriceImportStats {
    let mut s = pricing::PriceImportStats::default();
    for r in rows {
        match r.action {
            pricing::PriceAction::Insert => s.inserted += 1,
            pricing::PriceAction::Update => s.updated += 1,
            pricing::PriceAction::KeepUserOwned => s.kept_user_owned += 1,
            pricing::PriceAction::Unchanged => s.unchanged += 1,
        }
    }
    s
}

/// 把目录条目折成待写入的单价行。
///
/// 只有**本渠道声明过的模型**才收：目录里躺着几千个本项目根本用不上的模型，
/// 全导进去会把价格页淹没，而且没有意义 —— 没声明过的模型本来就走兜底倍率。
async fn wanted_prices(
    shell: &Arc<AppShell>,
    source: CatalogSource,
    models: &[CatalogModel],
) -> AppResult<Vec<ModelPricing>> {
    let declared: std::collections::HashSet<String> =
        provider_store::all_declared_models(&shell.db).await?.into_iter().collect();

    let tag = format!("catalog:{}", source_label(source));
    Ok(index_by_model(models.to_vec())
        .into_values()
        .filter(|m| declared.contains(&m.model))
        .filter_map(|m| to_pricing(&m))
        .map(|mut p| {
            p.source = Some(tag.clone());
            p
        })
        .collect())
}

fn source_label(s: CatalogSource) -> &'static str {
    match s {
        CatalogSource::ModelsDev => "models.dev",
        CatalogSource::LiteLlm => "litellm",
    }
}

/// 预览「从目录更新价格会改动什么」，不写库。
#[tauri::command]
pub async fn catalog_price_preview(
    app: AppHandle,
    source: CatalogSource,
    refresh: bool,
) -> AppResult<CatalogPreview> {
    let shell = app.state::<Arc<AppShell>>().inner().clone();
    let models = catalog_for(&shell, source, refresh).await?;

    let incoming = wanted_prices(&shell, source, &models).await?;
    let rows = pricing::diff_catalog(&shell.db, &incoming).await?;
    let stats = count_stats(&rows);

    Ok(CatalogPreview {
        source,
        source_label: source_label(source).to_string(),
        total: models.len(),
        priced: incoming.len(),
        rows,
        stats,
    })
}

/// 应用价格更新。**重新算一遍差异而不是信前端传回来的行** —— 预览和落库之间
/// 可能隔了几分钟，期间用户可能手改过某行的价，那时按旧差异写下去就会把他的
/// 改动覆盖掉。
#[tauri::command]
pub async fn catalog_price_apply(
    app: AppHandle,
    source: CatalogSource,
) -> AppResult<pricing::PriceImportStats> {
    let shell = app.state::<Arc<AppShell>>().inner().clone();
    let models = catalog_for(&shell, source, false).await?;

    let incoming = wanted_prices(&shell, source, &models).await?;
    let rows = pricing::diff_catalog(&shell.db, &incoming).await?;
    let stats = pricing::apply_catalog(&shell.db, &rows).await?;

    // 内存里的单价表是热替换的，写完必须重载才生效 —— 少了这步用户会看到
    // "价格更新了但账单还是按旧的算"。
    shell.reload_pricing().await?;
    Ok(stats)
}

/* ================================================================== */
/* 能力                                                               */
/* ================================================================== */

#[derive(Debug, Serialize)]
pub struct CapabilityImportStats {
    pub providers: usize,
    pub written: u64,
    /// 目录里认不出、因而没写的模型数（含本渠道声明了但目录没有的）。
    pub missing: usize,
}

/// 从目录导入能力标志。
///
/// 和价格不同，这里是**逐渠道**做的：同一个模型名在不同渠道下可能表现不同，
/// 而目录只能给出模型层面的断言，所以每个渠道各存一份。
#[tauri::command]
pub async fn catalog_capabilities_import(
    app: AppHandle,
    source: CatalogSource,
    provider_id: Option<i64>,
) -> AppResult<CapabilityImportStats> {
    let shell = app.state::<Arc<AppShell>>().inner().clone();
    let models = catalog_for(&shell, source, false).await?;
    let index = index_by_model(models.to_vec());

    let providers: Vec<Provider> = match provider_id {
        Some(id) => provider_store::get(&shell.db, id).await?.into_iter().collect(),
        None => provider_store::list_enabled(&shell.db).await?,
    };

    let now = now_ms();
    let mut stats = CapabilityImportStats {
        providers: providers.len(),
        written: 0,
        missing: 0,
    };

    for p in &providers {
        let declared = provider_store::list_models(&shell.db, p.id).await?;

        // 先清掉这个渠道上一次的目录结论，再写新的 —— 否则目录里已经删掉的
        // 模型会一直留着旧能力。probe 行不受影响。
        capabilities::clear_source(&shell.db, p.id, SOURCE_CATALOG).await?;

        let mut records = Vec::new();
        for m in &declared {
            let Some(entry) = index.get(m.model.trim()) else {
                stats.missing += 1;
                continue;
            };
            for cap in Capability::ALL {
                let supported = match cap {
                    Capability::Reasoning => entry.supports_reasoning,
                    Capability::Tools => entry.supports_tools,
                    Capability::Vision => entry.supports_vision,
                };
                records.push(CapabilityRecord {
                    provider_id: p.id,
                    model: m.model.clone(),
                    capability: cap,
                    verdict: CapabilityVerdict::from_bool(supported),
                    source: SOURCE_CATALOG.into(),
                    evidence: Some(format!("来自{}目录（{}）", source_label(source), entry.vendor)),
                    checked_at: now,
                });
            }
        }

        stats.written += capabilities::upsert(&shell.db, &records).await?;
    }

    Ok(stats)
}

/// 读取某渠道已有的能力判定，供界面渲染。
#[tauri::command]
pub async fn list_capabilities(
    app: AppHandle,
    provider_id: i64,
) -> AppResult<Vec<CapabilityRecord>> {
    let shell = app.state::<Arc<AppShell>>();
    capabilities::list_for_provider(&shell.db, provider_id).await
}

/// 探测目标渠道上某个模型的能力，结果落库。
///
/// 一次一项能力：界面上是逐项按钮，因为每项都要花一次请求的 token，不该
/// 用户点一下就把三项全发了。
#[tauri::command]
pub async fn probe_capability(
    app: AppHandle,
    provider_id: i64,
    model: String,
    capability: Capability,
) -> AppResult<CapabilityRecord> {
    let shell = app.state::<Arc<AppShell>>().inner().clone();
    let provider = provider_store::get(&shell.db, provider_id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(provider_id.to_string()))?;

    // 探测要打的是**上游认得的名字**，不是客户端发的声明名。
    let upstream = provider.upstream_model(model.trim());

    let probed = crate::probe::probe_capability(&shell, &provider, &upstream, capability).await;

    let record = CapabilityRecord {
        provider_id,
        model: model.trim().to_string(),
        capability: probed.capability,
        verdict: probed.verdict,
        // 来源标 probe，即便结论是"不确定" —— 优先级规则在 storage 层，
        // 那里已经处理好不确定的探测压不过目录。
        source: capabilities::SOURCE_PROBE.into(),
        evidence: Some(probed.evidence),
        checked_at: now_ms(),
    };

    capabilities::upsert(&shell.db, std::slice::from_ref(&record)).await?;

    // 回读一次而不是直接返回构造的 record：upsert 可能因为优先级规则**没写进去**
    // （比如目录的明确结论挡住了"不确定"的探测结果）。直接返回会让界面显示一个
    // 并不存在于库里的结论，刷新一下就变回去，看着像 bug。
    let stored = capabilities::list_for_provider(&shell.db, provider_id)
        .await?
        .into_iter()
        .find(|r| r.model == record.model && r.capability == capability);

    stored.ok_or_else(|| AppError::msg("能力写入后读取失败"))
}

/// 单次模型测试：往这个渠道真发一条最短的对话请求。
#[tauri::command]
pub async fn test_model(
    app: AppHandle,
    provider_id: i64,
    model: String,
) -> AppResult<crate::upstream::oneshot::OneshotOutcome> {
    let shell = app.state::<Arc<AppShell>>().inner().clone();
    let provider = provider_store::get(&shell.db, provider_id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(provider_id.to_string()))?;

    let upstream = provider.upstream_model(model.trim()).to_string();
    let mut req = crate::protocol::dto::UnifiedRequest::new(&upstream);
    // 尽量短：这是连通性/可用性测试，不是评测。max_tokens 给大一点是怕有些
    // 模型（尤其带思考的）先花掉额度再输出，给太小会得到空回答。
    req.max_tokens = Some(64);
    req.messages = vec![crate::protocol::dto::UnifiedMessage::user_text("回复 ok")];

    let started = Instant::now();
    let mut outcome = crate::upstream::oneshot::send(&shell, &provider, &req).await;
    // send 内部已经计了时，这里只是兜底防止极端情况下出现 0
    if outcome.latency_ms == 0 {
        outcome.latency_ms = started.elapsed().as_millis() as i64;
    }
    Ok(outcome)
}
