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
mod replay;
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

            // 老库升级上来时价格表可能是空的；渠道默认 auto，这里在后台补一次。
            // 只在整个表为空时联网，成功之后不再触发（见函数注释）。
            let shell_for_prices = shell.clone();
            tauri::async_runtime::spawn(async move {
                crate::commands::catalog::backfill_prices_on_startup(&shell_for_prices).await;
            });

            // 按设置自动拉起网关。失败不阻断启动 —— 用户可能只是端口被占了，
            // 应当能进界面改成别的端口，而不是应用直接起不来。
            if shell.settings().autostart_gateway {
                let shell_for_start = shell.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = shell_for_start.gateway.start(shell_for_start.clone()).await {
                        tracing::warn!("自动启动网关失败: {e}");
                        let status = crate::gateway::server::GatewayStatus {
                            running: false,
                            host: shell_for_start.settings().bind_host().to_string(),
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
            commands::app::validate_rewrite_script,
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
            commands::providers::discover_upstream_models,
            commands::providers::set_provider_enabled,
            commands::providers::detect_provider_protocols,
            commands::providers::sync_provider_metadata,
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
            commands::logs::list_sessions,
            commands::logs::get_session_timeline,
            commands::logs::pin_session_model,
            commands::logs::unpin_session_model,
            commands::logs::validate_log_expr,
            commands::logs::get_request_detail,
            commands::logs::get_inflight_requests,
            commands::logs::clear_logs,
            commands::logs::delete_log,
            // --- 回放 / A-B 对比 ---
            commands::replay::replay_request,
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
/// 日志写到哪里、保留多久。
///
/// 打包后的窗口程序（Windows 的 `windows_subsystem = "windows"`）没有控制台，
/// stderr 里的日志等于直接扔掉 —— 用户报「它突然不好用了」时，我们连一句现场
/// 都拿不到。所以文件日志不是锦上添花：它是打包形态下**唯一**可用的排查入口。
///
/// 同时保留 stdout 一份：`tauri dev` 与终端启动时仍然能直接看到。
const LOG_FILE_PREFIX: &str = "apilot";
const LOG_KEEP_DAYS: usize = 7;

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("apilot_lib=debug,apilot=debug,warn"));

    let stdout_layer = fmt::layer().with_target(true);

    // 文件这一层失败不该挡住应用启动：没有日志目录权限时退化成只写 stdout，
    // 比因为日志起不来而整个打不开应用好得多。
    let writer = match file_writer() {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("日志文件初始化失败，仅输出到控制台: {e}");
            None
        }
    };

    // 文件层在这里就地构造，而不是从 helper 返回一个 `Layer`：`fmt::Layer`
    // 的泛型参数跟着"最终由哪些层组合成 subscriber"走，提前固定成某个具体
    // Subscriber 类型反而拼不上去。
    match writer {
        Some(writer) => tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .with(
                fmt::layer()
                    .with_target(true)
                    // 文件里不需要 ANSI 颜色码 —— 那些字节会原样躺在日志里。
                    .with_ansi(false)
                    .with_writer(writer),
            )
            .init(),
        None => tracing_subscriber::registry()
            .with(filter)
            .with(stdout_layer)
            .init(),
    }

    // panic 在默认 hook 里只打到 stderr，窗口程序里等于丢失。这里把它连同
    // backtrace 一起写进 tracing（因此也会落进文件）。`force_capture` 不依赖
    // RUST_BACKTRACE，release 包里也能拿到。
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, backtrace = %backtrace, "进程 panic");
    }));
}

/// 建按天滚动的文件写入器。
///
/// `WorkerGuard` 必须活到进程结束，否则非阻塞写入器会在 drop 时把最后一批
/// 日志丢掉 —— 而最后一批往往正是崩溃前的那几行。放进 `OnceLock` 里挂着。
fn file_writer(
) -> Result<tracing_appender::non_blocking::NonBlocking, Box<dyn std::error::Error>> {
    let dir = crate::config::paths::log_dir();
    std::fs::create_dir_all(&dir)?;

    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix("log")
        .max_log_files(LOG_KEEP_DAYS)
        .build(&dir)?;

    let (writer, guard) = tracing_appender::non_blocking(appender);
    // 只在第一次初始化时生效；重复调用（测试/热重载）不会拿掉已经活着的 guard。
    let _ = LOG_GUARD.set(guard);
    Ok(writer)
}

static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// 供集成测试与外部调用复用的启动入口。
pub use error::{AppError, AppResult};
pub use shell::AppShell as Shell;

#[allow(dead_code)]
fn _assert_send_sync() {
    // AppShell 必须能跨线程共享，否则 Tauri 的 manage / spawn 都不成立。
    fn assert<T: Send + Sync>() {}
    assert::<Arc<AppShell>>();
}
