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
pub(crate) async fn catalog_for(
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
    let declared = provider_store::all_declared_models(&shell.db).await?;
    Ok(prices_for_models(models, source, &declared))
}

/// 从目录里挑出**指定模型名**的单价行。纯函数，便于脱离网络测筛选与定价换算。
///
/// 只收调用方点名的模型：目录里躺着几千个本项目根本用不上的模型，全导进去会把
/// 价格页淹没，而且没有意义 —— 没声明过的模型本来就走兜底倍率。
fn prices_for_models(
    models: &[CatalogModel],
    source: CatalogSource,
    wanted: &[String],
) -> Vec<ModelPricing> {
    let wanted: std::collections::HashSet<&str> =
        wanted.iter().map(|m| m.trim()).collect();
    let tag = format!("catalog:{}", source_label(source));

    index_by_model(models.to_vec())
        .into_values()
        .filter(|m| wanted.contains(m.model.as_str()))
        .filter_map(|m| to_pricing(&m))
        .map(|mut p| {
            // 标记来源，让下一次同来源导入能覆盖它、而用户手填的行（source=None）
            // 在 `diff_catalog` 里永远被保留。
            p.source = Some(tag.clone());
            p
        })
        .collect()
}

/// 给一批指定模型同步目录价格，并把结果写进库。
///
/// 渠道绑定了目录来源时（保存渠道、保存模型声明）自动调用。`diff_catalog` +
/// `apply_catalog` 会保留用户手填的行，所以这里不必先判断哪些行"归目录管"。
///
/// 目录本身走 [`catalog_for`] 的缓存：同一轮操作里保存多个渠道不会重复下载。
pub(crate) async fn apply_catalog_prices_for(
    shell: &Arc<AppShell>,
    source: CatalogSource,
    models: &[String],
) -> AppResult<pricing::PriceImportStats> {
    // 一个模型都没声明时不值得联网：目录有几千条，但落库的会是零行。
    if models.is_empty() {
        return Ok(pricing::PriceImportStats::default());
    }

    let catalog = catalog_for(shell, source, false).await?;
    let incoming = prices_for_models(&catalog, source, models);
    let rows = pricing::diff_catalog(&shell.db, &incoming).await?;
    let stats = pricing::apply_catalog(&shell.db, &rows).await?;

    // 内存单价表是热替换的，写完必须重载才生效 —— 少了这步用户会看到
    // "价格更新了但账单还是按旧的算"。
    shell.reload_pricing().await?;
    Ok(stats)
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

/// 启动时补一次价格。
///
/// 从老版本升级上来的库 `model_pricing` 可能是空的，而渠道默认
/// `metadata_source = auto`。这里只在整个价格表为空时跑 —— 那是"从来没同步过"
/// 的明确信号；之后新增/改模型由保存渠道那条路负责，不会每次启动都联网。
///
/// 网络失败只 warn：目录拉不下来不影响网关启动，下次启动再试。
pub(crate) async fn backfill_prices_on_startup(shell: &Arc<AppShell>) {
    match crate::storage::pricing::list(&shell.db).await {
        Ok(rows) if rows.is_empty() => {}
        Ok(_) => return, // 已经有价格了，不重复下载
        Err(e) => {
            tracing::warn!(error = %e, "读取价格表失败，跳过启动时补价");
            return;
        }
    }

    let providers = match provider_store::list(&shell.db).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "读取渠道失败，跳过启动时补价");
            return;
        }
    };

    // 按来源合并：同一份目录只下载一次，哪怕有好几条渠道都绑定了它。
    let mut wanted: std::collections::HashMap<CatalogSource, Vec<String>> =
        std::collections::HashMap::new();
    for p in &providers {
        let Some(source) = p.metadata_source.catalog_source() else {
            continue; // off：用户明确说别碰价格
        };
        match provider_store::list_models(&shell.db, p.id).await {
            Ok(models) => {
                let bucket = wanted.entry(source).or_default();
                bucket.extend(models.into_iter().map(|m| m.model));
            }
            Err(e) => tracing::warn!(tag = %p.tag, error = %e, "读取渠道模型失败"),
        }
    }

    for (source, mut models) in wanted {
        models.sort();
        models.dedup();
        if models.is_empty() {
            continue;
        }
        match apply_catalog_prices_for(shell, source, &models).await {
            Ok(stats) => tracing::info!(
                source = source.label(),
                inserted = stats.inserted,
                updated = stats.updated,
                "启动时已补齐模型价格"
            ),
            Err(e) => tracing::warn!(
                source = source.label(),
                error = %e,
                "启动时补价失败（下次启动再试）"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(model: &str, input: Option<f64>, output: Option<f64>) -> CatalogModel {
        CatalogModel {
            model: model.into(),
            vendor: "test".into(),
            first_party: true,
            input,
            output,
            cache_read: None,
            cache_write: None,
            reasoning: None,
            supports_reasoning: false,
            supports_tools: false,
            supports_vision: false,
            supports_pdf: false,
        }
    }

    #[test]
    fn only_the_named_models_are_picked_out_of_the_catalog() {
        // 目录有几千条，全导进去会把价格页淹没；渠道没声明过的模型本来就走兜底倍率。
        let catalog = vec![
            entry("wanted", Some(3.0), Some(15.0)),
            entry("not-declared", Some(1.0), None),
        ];

        let prices = prices_for_models(&catalog, CatalogSource::ModelsDev, &["wanted".into()]);
        assert_eq!(prices.len(), 1);
        assert_eq!(prices[0].model, "wanted");
        assert_eq!(prices[0].model_ratio, 1.5, "$3/1M ÷ 2 = 1.5 倍率");
        assert_eq!(prices[0].completion_ratio, 5.0);
    }

    #[test]
    fn a_model_without_a_price_is_skipped_instead_of_being_written_as_free() {
        // `to_pricing` 返回 None 时不能拿 0 填：0 在计费里是"免费"。
        let catalog = vec![entry("unknown-price", None, None)];
        assert!(prices_for_models(
            &catalog,
            CatalogSource::ModelsDev,
            &["unknown-price".into()]
        )
        .is_empty());
    }

    #[test]
    fn imported_prices_are_tagged_with_the_catalog_that_produced_them() {
        // 这个标记是下次导入能否覆盖的依据，也是"用户手填的行不被覆盖"的边界。
        let catalog = vec![entry("m", Some(2.0), None)];

        let from_dev = prices_for_models(&catalog, CatalogSource::ModelsDev, &["m".into()]);
        assert_eq!(from_dev[0].source.as_deref(), Some("catalog:models.dev"));

        let from_litellm = prices_for_models(&catalog, CatalogSource::LiteLlm, &["m".into()]);
        assert_eq!(from_litellm[0].source.as_deref(), Some("catalog:litellm"));
    }

    #[test]
    fn declared_model_names_are_trimmed_before_matching() {
        // 前端提交的模型名带空白时，不该因此漏掉一条本来能匹配上的价格。
        let catalog = vec![entry("m", Some(2.0), None)];
        let prices = prices_for_models(&catalog, CatalogSource::ModelsDev, &[" m ".into()]);
        assert_eq!(prices.len(), 1);
    }
}
