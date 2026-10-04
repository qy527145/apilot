//! 统一错误类型。
//!
//! `AppError` 同时服务于两个边界：
//! - Tauri 命令（需要 `Serialize`，前端拿到结构化错误）
//! - axum handler（实现 `IntoResponse`，网关侧返回 JSON 错误体）

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 解析错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML 解析错误: {0}")]
    Toml(#[from] toml_edit::TomlError),

    #[error("网络错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("网关未运行")]
    GatewayNotRunning,

    #[error("网关已在运行")]
    GatewayAlreadyRunning,

    #[error("端口 {0} 已被占用")]
    PortInUse(u16),

    #[error("配置解析失败，已中止写入以保护原文件: {path} — {reason}")]
    PatchAborted { path: String, reason: String },

    #[error("找不到渠道: {0}")]
    ProviderNotFound(String),

    #[error("找不到 selector: {0}")]
    SelectorNotFound(String),

    #[error("未识别的客户端: {0}")]
    UnknownClient(String),

    #[error(
        "还没有可用的模型：请先在「渠道管理」添加上游渠道，并从上游获取或手工指定至少一个模型，再接管客户端"
    )]
    NoUsableModel,

    #[error("{0}")]
    Msg(String),
}

impl AppError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Msg(s.into())
    }

    /// 给前端用的稳定错误码，便于 UI 分支处理。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Db(_) => "db",
            Self::Io(_) => "io",
            Self::Json(_) | Self::Toml(_) => "parse",
            Self::Http(_) => "http",
            Self::GatewayNotRunning => "gateway_not_running",
            Self::GatewayAlreadyRunning => "gateway_already_running",
            Self::PortInUse(_) => "port_in_use",
            Self::PatchAborted { .. } => "patch_aborted",
            Self::ProviderNotFound(_) => "provider_not_found",
            Self::SelectorNotFound(_) => "selector_not_found",
            Self::UnknownClient(_) => "unknown_client",
            Self::NoUsableModel => "no_usable_model",
            Self::Msg(_) => "error",
        }
    }
}

/// Tauri 命令的错误出口：序列化成 `{ code, message }`。
impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AppError", 2)?;
        s.serialize_field("code", self.code())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}

/// 网关侧的 HTTP 错误出口。
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::PortInUse(_) | Self::GatewayAlreadyRunning => StatusCode::CONFLICT,
            Self::ProviderNotFound(_) | Self::SelectorNotFound(_) | Self::UnknownClient(_) => {
                StatusCode::NOT_FOUND
            }
            Self::PatchAborted { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::GatewayNotRunning => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = serde_json::json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
            }
        });

        // 网关出错时已记账的请求不应被吞掉，这里保持纯响应构造，不触发任何 I/O。
        (status, axum::Json(body)).into_response()
    }
}
