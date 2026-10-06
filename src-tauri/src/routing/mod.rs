//! 路由：全局模型策略、规则链、selector 热切换与健康探测。

pub mod engine;
pub mod metadata;
pub mod model_policy;
pub mod rule;
pub mod rule_item;
pub mod selector;
pub use engine::{RouteOutcome, Router};
pub use metadata::RouteMetadata;
