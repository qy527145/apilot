//! Tauri 命令层。
//!
//! 按域分文件。注意：`#[tauri::command]` 会额外生成 `__cmd__*` 宏项，
//! 它**不能**通过 `pub use` 转发，所以 `generate_handler!` 里一律用完整路径
//! （`commands::providers::foo`），不要在这里做 re-export。

pub mod app;
pub mod billing;
pub mod cache;
pub mod gateway;
pub mod logs;
pub mod models;
pub mod providers;
pub mod routing;
pub mod takeover;
