//! `AppShell` —— 全部共享依赖的唯一所有权根。
//!
//! 由 Tauri 的 `manage` 持有，命令层用 `State<'_, Arc<AppShell>>` 取用；
//! 网关侧用 [`GatewayState`] 暴露给 axum handler。

use std::sync::Arc;

use arc_swap::ArcSwap;
use sqlx::SqlitePool;
use tauri::AppHandle;

use crate::cache::store::ResponseCache;
use crate::config::AppSettings;
use crate::error::AppResult;
use crate::protocol::codec::CodecRegistry;
use crate::routing::selector::{Selector, SelectorManager};
use crate::routing::Router;
use crate::storage::aggregates::AggregateBuffer;
use crate::traffic::events::EventBus;
use crate::traffic::TrafficStats;
use crate::upstream::client;
use crate::upstream::ProviderRegistry;
use crate::util::now_ms;

/// 请求明细日志的保留天数。
///
/// 聚合表（`usage_hourly`）只存小时级汇总，几乎不增长，长期保留；
/// 明细表则会随使用无限膨胀，超过这个期限就清掉。
const LOG_RETENTION_DAYS: i64 = 30;

pub struct AppShell {
    pub db: SqlitePool,
    pub app: AppHandle,

    /// 读多写极少 —— 每请求无锁 `load_full()`，改设置时整体原子替换。
    settings: ArcSwap<AppSettings>,

    pub codecs: Arc<CodecRegistry>,
    pub registry: Arc<ProviderRegistry>,
    pub selectors: Arc<SelectorManager>,
    pub router: Arc<Router>,
    pub pricing: Arc<ArcSwap<crate::billing::PricingTable>>,
    pub cache: Arc<ResponseCache>,
    pub aggregates: Arc<AggregateBuffer>,
    pub traffic: Arc<TrafficStats>,
    pub events: Arc<EventBus>,
    pub gateway: Arc<crate::gateway::server::GatewayServer>,

    /// 最近一次测速的延迟（provider_tag → 毫秒）。
    ///
    /// 只活在内存里：它是易变的遥测数据，重启后重新测即可，落库反而会让人
    /// 对着几天前的数字做决策。由「测速」命令写入，供按延迟选渠道的策略读取。
    pub probe_latency: Arc<dashmap::DashMap<String, i64>>,

    /// 最近拉到的上游模型目录，按来源缓存。
    ///
    /// 缓存而不是每次重拉，是因为「预览差异」和「确认应用」是两次命令调用 ——
    /// 不缓存就得把一个几 MB 的 JSON 下两遍，而用户还可能在界面上反复切换
    /// 来源看差异。缓存值里带抓取时刻，由调用方按 TTL 判断新鲜度。
    pub catalog_cache: Arc<dashmap::DashMap<crate::catalog::CatalogSource, (i64, Arc<Vec<crate::catalog::CatalogModel>>)>>,

    started_at: i64,
}

impl AppShell {
    /// 打开数据库、加载全部配置，构造 AppShell。
    pub async fn bootstrap(app: AppHandle) -> AppResult<Arc<Self>> {
        let db = crate::storage::open(&crate::config::db_path()).await?;
        let settings = AppSettings::load(&db).await?;

        // --- 上游渠道 ---
        let registry = Arc::new(ProviderRegistry::with_pool(Arc::new(client::ClientPool::new())));
        registry.set_global_proxy(settings.proxy.clone());
        let providers = crate::storage::providers::list_enabled(&db).await?;
        registry.reload(&providers);

        // --- 路由 ---
        let rules = crate::storage::routing::list_rules(&db).await?;
        let final_sel = crate::storage::routing::final_selector(&db).await?;
        let router = Arc::new(Router::new(
            rules.into_iter().map(Arc::new).collect(),
            final_sel.clone(),
        ));

        // 首次启动时铺一个默认 selector，让新建渠道后无需再配路由就能用。
        crate::storage::routing::ensure_default_selector(&db, &providers).await?;

        let selectors = Arc::new(SelectorManager::new(registry.clone()));
        Self::load_selectors(&db, &selectors, &registry).await?;

        // 注册表的"默认渠道"是**渠道 tag**，不是 selector tag —— 两者是独立命名空间。
        // 它只在"指定的 selector 不存在"时兜底，取优先级最高的启用渠道。
        registry.set_default(providers.first().map(|p| p.tag.clone()));

        // --- 计费 ---
        let pricing = Arc::new(ArcSwap::from_pointee(
            crate::storage::pricing::load_table(&db).await?,
        ));

        // --- 缓存 ---
        let mut policy = crate::cache::CachePolicy {
            enabled: settings.cache_enabled,
            ttl_secs: settings.cache_ttl_secs,
            max_entries: settings.cache_max_entries,
        };
        policy = policy.normalized();
        let cache = Arc::new(ResponseCache::new(policy));
        cache.restore_counters(&db).await?;

        let shell = Arc::new(Self {
            db,
            app: app.clone(),
            settings: ArcSwap::from_pointee(settings),
            codecs: Arc::new(CodecRegistry::new()),
            registry,
            selectors,
            router,
            pricing,
            cache,
            aggregates: Arc::new(AggregateBuffer::new()),
            traffic: Arc::new(TrafficStats::new()),
            events: Arc::new(EventBus::new(app)),
            gateway: Arc::new(crate::gateway::server::GatewayServer::new()),
            probe_latency: Arc::new(dashmap::DashMap::new()),
            catalog_cache: Arc::new(dashmap::DashMap::new()),
            started_at: now_ms(),
        });

        // selectors 的切换事件转发到前端。
        shell.wire_selector_events();
        // 定时把实时流量与落库进度推给前端。
        shell.spawn_background_tasks();

        Ok(shell)
    }

    /// 启动周期性后台任务：流量推送、聚合落库、缓存计数持久化。
    fn spawn_background_tasks(self: &Arc<Self>) {
        let shell = self.clone();

        tauri::async_runtime::spawn(async move {
            // 1 秒一次足够让前端看起来是"实时"的，又不会把 IPC 打满。
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            let mut last_total = shell.traffic.total_requests();
            let mut last_at = std::time::Instant::now();
            let mut ticks: u64 = 0;

            loop {
                ticker.tick().await;
                ticks += 1;

                let total = shell.traffic.total_requests();
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_at).as_secs_f64().max(0.001);
                let rps = total.saturating_sub(last_total) as f64 / elapsed;
                last_total = total;
                last_at = now;

                shell.events.traffic(&shell.traffic.event(rps));

                // 每 10 秒把内存里的聚合与缓存计数落一次库。
                // 太频繁会白白写盘，太久则崩溃时丢的统计太多。
                if ticks % 10 == 0 {
                    if let Err(e) = shell.aggregates.flush(&shell.db).await {
                        tracing::warn!("聚合落库失败: {e}");
                    }
                    if let Err(e) = shell.cache.persist_counters(&shell.db).await {
                        tracing::warn!("缓存计数落库失败: {e}");
                    }
                    if let Err(e) = shell.cache.purge_expired(&shell.db).await {
                        tracing::warn!("清理过期缓存失败: {e}");
                    }
                }

                // 每 10 分钟清理一次超期日志。明细表会随使用无限增长，
                // 一个活跃的 Agent 每天就能产生上万行。
                if ticks % 600 == 0 {
                    let cutoff = crate::util::now_ms() - LOG_RETENTION_DAYS * 24 * 3600 * 1000;
                    match crate::storage::logs::prune_logs(&shell.db, cutoff).await {
                        Ok(n) if n > 0 => tracing::info!(removed = n, "已清理超期请求日志"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("清理请求日志失败: {e}"),
                    }
                    let _ = shell.cache.persist_counters(&shell.db).await;
                }
            }
        });
    }

    /// 把数据库里的 selector 载入内存，并恢复各自的当前选择。
    async fn load_selectors(
        db: &SqlitePool,
        manager: &Arc<SelectorManager>,
        registry: &Arc<ProviderRegistry>,
    ) -> AppResult<()> {
        for rec in crate::storage::routing::list_selectors(db).await? {
            let sel = Arc::new(Selector::new(
                rec.tag.clone(),
                registry.clone(),
                rec.members.clone(),
                rec.current_provider.clone(),
                rec.mode,
                rec.tolerance_ms,
            ));
            // 持久化的选择可能指向已删除的渠道，restore 会自行回落。
            sel.restore(rec.current_provider.clone());
            manager.insert(sel);
        }
        Ok(())
    }

    /// 订阅所有 selector 的切换事件，转发给前端。
    fn wire_selector_events(&self) {
        for sel in self.selectors.all() {
            let events = self.events.clone();
            let db = self.db.clone();
            let tag = sel.tag().to_string();
            let mut rx = sel.subscribe();

            tauri::async_runtime::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    events.selector_changed(&ev);
                    // 持久化由 SelectorManager 之外的这里做：
                    // Selector 本身保持纯内存，不持有数据库句柄。
                    if let Err(e) = crate::storage::routing::persist_selection(
                        &db,
                        &tag,
                        &ev.provider_tag,
                    )
                    .await
                    {
                        tracing::warn!(selector = %tag, "持久化 selector 选择失败: {e}");
                    }
                }
            });
        }
    }

    // -----------------------------------------------------------------------
    // 配置热重载
    // -----------------------------------------------------------------------

    /// 从数据库重新加载渠道。前端改完渠道后调用。
    pub async fn reload_providers(&self) -> AppResult<()> {
        let providers = crate::storage::providers::list_enabled(&self.db).await?;
        self.registry.reload(&providers);
        // 兜底渠道跟着渠道列表一起更新，否则删掉它之后兜底会指向空。
        self.registry
            .set_default(providers.first().map(|p| p.tag.clone()));
        Ok(())
    }

    /// 重新加载路由规则与兜底 selector。
    pub async fn reload_routing(&self) -> AppResult<()> {
        let rules = crate::storage::routing::list_rules(&self.db).await?;
        let final_sel = crate::storage::routing::final_selector(&self.db).await?;
        self.router
            .reload(rules.into_iter().map(Arc::new).collect(), final_sel);
        Ok(())
    }

    /// 重新加载 selectors（增删改后调用）。
    pub async fn reload_selectors(&self) -> AppResult<()> {
        self.selectors.clear();
        Self::load_selectors(&self.db, &self.selectors, &self.registry).await?;
        self.wire_selector_events();
        Ok(())
    }

    /// 重新加载单价表。
    pub async fn reload_pricing(&self) -> AppResult<()> {
        let table = crate::storage::pricing::load_table(&self.db).await?;
        self.pricing.store(Arc::new(table));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 设置
    // -----------------------------------------------------------------------

    pub fn settings(&self) -> Arc<AppSettings> {
        self.settings.load_full()
    }

    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// 持久化后原子替换内存副本，并同步缓存策略与出站代理。
    pub async fn update_settings(&self, new: AppSettings) -> AppResult<()> {
        let new = new.normalized();
        // 代理变了才重载渠道：reload 会重建全部渠道对象，而「跟随全局」的渠道
        // 必须拿到新代理对应的客户端才能生效。（只换客户端不重载的话，
        // 在途与后续请求都还挂着旧的那个。）
        let proxy_changed = self.settings().proxy != new.proxy;
        new.save(&self.db).await?;

        self.cache.set_policy(crate::cache::CachePolicy {
            enabled: new.cache_enabled,
            ttl_secs: new.cache_ttl_secs,
            max_entries: new.cache_max_entries,
        });

        self.settings.store(Arc::new(new));
        if proxy_changed {
            self.registry.set_global_proxy(self.settings().proxy.clone());
            self.reload_providers().await?;
        }
        Ok(())
    }

    /// 进程退出时的收尾：把内存里尚未落库的聚合与计数器写出去。
    pub async fn shutdown(&self) {
        if let Err(e) = self.aggregates.flush(&self.db).await {
            tracing::warn!("退出时刷新聚合失败: {e}");
        }
        if let Err(e) = self.cache.persist_counters(&self.db).await {
            tracing::warn!("退出时刷新缓存计数失败: {e}");
        }
        self.db.close().await;
    }
}
