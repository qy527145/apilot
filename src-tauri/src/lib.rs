//! Apilot —— 本地 LLM API 智能网关助手。

mod billing;
mod cache;
mod catalog;
mod codex;
mod commands;
mod config;
mod error;
mod gateway;
mod probe;
mod protocol;
mod routing;
mod shell;
mod storage;
mod takeover;
mod traffic;
mod upstream;
mod util;

use std::sync::Arc;

use tauri::Manager;

use shell::AppShell;

pub fn run() {
    init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // setup 跑在主线程、tokio runtime 之外，block_on 是安全的。
            let handle = app.handle().clone();
            let shell = tauri::async_runtime::block_on(AppShell::bootstrap(handle))?;
            app.manage(shell.clone());

            // 按设置自动拉起网关。失败不阻断启动 —— 用户可能只是端口被占了，
            // 应当能进界面改成别的端口，而不是应用直接起不来。
            if shell.settings().autostart_gateway {
                let shell_for_start = shell.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = shell_for_start.gateway.start(shell_for_start.clone()).await {
                        tracing::warn!("自动启动网关失败: {e}");
                        let status = crate::gateway::server::GatewayStatus {
                            running: false,
                            host: shell_for_start.settings().listen_host.clone(),
                            port: 0,
                            error: Some(e.to_string()),
                        };
                        shell_for_start.events.gateway(&status);
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // --- 应用 ---
            commands::app::app_info,
            commands::app::get_settings,
            commands::app::update_settings,
            commands::app::set_model_policy,
            commands::app::validate_model_script,
            // --- 网关 ---
            commands::gateway::gateway_start,
            commands::gateway::gateway_stop,
            commands::gateway::gateway_status,
            // --- 渠道 ---
            commands::providers::list_providers,
            commands::providers::upsert_provider,
            commands::providers::delete_provider,
            commands::providers::test_provider,
            commands::providers::list_provider_models,
            commands::providers::set_provider_models,
            commands::providers::fetch_provider_models,
            commands::providers::set_provider_enabled,
            commands::providers::detect_provider_protocols,
            // --- 上游目录（价格与能力共用同一个网络集成） ---
            commands::catalog::catalog_price_preview,
            commands::catalog::catalog_price_apply,
            commands::catalog::catalog_capabilities_import,
            commands::catalog::list_capabilities,
            commands::catalog::probe_capability,
            commands::catalog::test_model,
            // --- 模型（模型视角：这个模型在哪些渠道上有、优先走哪个） ---
            commands::models::get_model_policy,
            commands::models::list_model_catalog,
            commands::models::list_model_options,
            commands::models::upsert_model_policy,
            commands::models::reset_model_policy,
            commands::models::switch_model_channel,
            commands::models::set_model_candidates,
            commands::models::probe_model_candidates,
            // --- 路由 ---
            commands::routing::list_route_rules,
            commands::routing::upsert_route_rule,
            commands::routing::delete_route_rule,
            commands::routing::reorder_route_rules,
            commands::routing::set_final_selector,
            commands::routing::list_selectors,
            commands::routing::upsert_selector,
            commands::routing::delete_selector,
            commands::routing::switch_selector,
            commands::routing::run_urltest,
            // --- 计费 ---
            commands::billing::list_pricing,
            commands::billing::upsert_pricing,
            commands::billing::delete_pricing,
            commands::billing::billing_summary,
            commands::billing::billing_totals,
            commands::billing::billing_timeseries,
            // --- 缓存 ---
            commands::cache::cache_stats,
            commands::cache::clear_cache,
            commands::cache::get_cache_policy,
            commands::cache::set_cache_policy,
            // --- 客户端接管 ---
            commands::takeover::detect_clients,
            commands::takeover::takeover_status,
            commands::takeover::preview_takeover,
            commands::takeover::apply_takeover,
            commands::takeover::restore_client,
            commands::takeover::open_client_config,
            commands::takeover::takeover_readiness,
            // --- 日志 ---
            commands::logs::query_logs,
            commands::logs::list_log_facets,
            commands::logs::get_request_detail,
            commands::logs::clear_logs,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // 退出前把内存里的统计落库并停掉网关，避免丢数据与占用端口。
            if let tauri::RunEvent::ExitRequested { .. } = event {
                if let Some(shell) = app.try_state::<Arc<AppShell>>() {
                    let shell = shell.inner().clone();
                    tauri::async_runtime::block_on(async move {
                        let _ = shell.gateway.stop(Some(&shell)).await;
                        shell.shutdown().await;
                    });
                }
            }
        });
}

/// 初始化日志。`RUST_LOG` 可覆盖默认级别。
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("apilot_lib=debug,apilot=debug,warn"));

    fmt().with_env_filter(filter).with_target(true).init();
}

/// 供集成测试与外部调用复用的启动入口。
pub use error::{AppError, AppResult};
pub use shell::AppShell as Shell;

#[allow(dead_code)]
fn _assert_send_sync() {
    // AppShell 必须能跨线程共享，否则 Tauri 的 manage / spawn 都不成立。
    fn assert<T: Send + Sync>() {}
    assert::<Arc<AppShell>>();
}
