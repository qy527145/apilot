//! 持久化层：SQLite 连接、迁移、各领域的读写。

pub mod aggregates;
pub mod db;
pub mod logs;
pub mod migrations;
pub mod model_policies;
pub mod models;
pub mod pricing;
pub mod providers;
pub mod routing;

pub use db::open;
