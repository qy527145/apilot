//! axum 路由：三个协议入口 + 健康检查 + 模型列表。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router as AxumRouter;
use http::{HeaderMap, StatusCode};

use crate::protocol::dto::Protocol;
use crate::shell::AppShell;

use super::pipeline;

/// 构造网关的 axum 路由表。
pub fn build(shell: Arc<AppShell>) -> AxumRouter {
    AxumRouter::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models))
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/responses", post(openai_responses))
        // 客户端版本差异会带上不同前缀（有的用 /messages，有的带 /v1）。
        .route("/messages", post(anthropic_messages))
        .route("/chat/completions", post(openai_chat))
        .route("/responses", post(openai_responses))
        .fallback(fallback)
        .with_state(shell)
}

async fn health(State(shell): State<Arc<AppShell>>) -> Response {
    let status = shell.gateway.status();
    axum::Json(serde_json::json!({
        "status": "ok",
        "running": status.running,
        "port": status.port,
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// 列出当前所有渠道声明的模型。
///
/// 客户端（尤其 Codex）会先调这个接口探测可用模型；返回并集即可，
/// 精确的可用性由路由规则在请求时决定。
async fn list_models(State(shell): State<Arc<AppShell>>) -> Response {
    let mut models: Vec<String> = Vec::new();

    match crate::storage::providers::list_enabled(&shell.db).await {
        Ok(providers) => {
            for p in providers {
                for m in p.model_mapping.keys() {
                    models.push(m.clone());
                }
            }
        }
        Err(e) => {
            tracing::warn!("读取渠道列表失败: {e}");
        }
    }

    // 单价表里配置过的模型也算 —— 用户可能只是还没建渠道。
    models.extend(shell.pricing.load().models());
    models.sort();
    models.dedup();

    let data: Vec<serde_json::Value> = models
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m,
                "object": "model",
                "created": 0,
                "owned_by": "apilot",
            })
        })
        .collect();

    axum::Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

async fn anthropic_messages(
    State(shell): State<Arc<AppShell>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    pipeline::handle(
        shell,
        Protocol::AnthropicMessages,
        "/v1/messages".into(),
        headers,
        body,
    )
    .await
}

async fn openai_chat(
    State(shell): State<Arc<AppShell>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    pipeline::handle(
        shell,
        Protocol::OpenAiChat,
        "/v1/chat/completions".into(),
        headers,
        body,
    )
    .await
}

async fn openai_responses(
    State(shell): State<Arc<AppShell>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    pipeline::handle(
        shell,
        Protocol::OpenAiResponses,
        "/v1/responses".into(),
        headers,
        body,
    )
    .await
}

/// 未匹配的路径：尽力按路径猜协议，猜不出就报错。
///
/// 之所以不直接 404：不同客户端会带各种前缀（`/v1`、`/api/v1`、自定义 base path），
/// 严格匹配会让用户配好 base_url 却收不到请求。按路径特征兜底更实用。
async fn fallback(State(shell): State<Arc<AppShell>>, req: axum::extract::Request) -> Response {
    let path = req.uri().path().to_string();

    let protocol = if path.contains("messages") {
        Some(Protocol::AnthropicMessages)
    } else if path.contains("responses") {
        Some(Protocol::OpenAiResponses)
    } else if path.contains("chat") || path.contains("completions") {
        Some(Protocol::OpenAiChat)
    } else {
        None
    };

    let Some(protocol) = protocol else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": {
                    "type": "not_found",
                    "message": format!("Apilot 不认识路径 {path}；支持的入口是 /v1/messages、/v1/chat/completions、/v1/responses"),
                }
            })),
        )
            .into_response();
    };

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": { "message": format!("读取请求体失败: {e}") }
                })),
            )
                .into_response()
        }
    };

    pipeline::handle(shell, protocol, path, parts.headers, bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_protocol_detection_from_path() {
        // 复刻 fallback 里的判断逻辑
        let detect = |path: &str| -> Option<Protocol> {
            if path.contains("messages") {
                Some(Protocol::AnthropicMessages)
            } else if path.contains("responses") {
                Some(Protocol::OpenAiResponses)
            } else if path.contains("chat") || path.contains("completions") {
                Some(Protocol::OpenAiChat)
            } else {
                None
            }
        };

        assert_eq!(detect("/v1/messages"), Some(Protocol::AnthropicMessages));
        assert_eq!(
            detect("/anthropic/v1/messages"),
            Some(Protocol::AnthropicMessages)
        );
        assert_eq!(detect("/v1/responses"), Some(Protocol::OpenAiResponses));
        assert_eq!(
            detect("/openai/v1/chat/completions"),
            Some(Protocol::OpenAiChat)
        );
        assert_eq!(detect("/completions"), Some(Protocol::OpenAiChat));
        assert_eq!(detect("/totally/unknown"), None);
    }

    #[test]
    fn path_parsing_matches_registered_routes() {
        for p in [
            "/v1/messages",
            "/v1/chat/completions",
            "/v1/responses",
            "/messages",
            "/chat/completions",
            "/responses",
        ] {
            assert!(
                Protocol::from_path(p).is_some(),
                "注册的路径 {p} 必须能被 Protocol::from_path 识别"
            );
        }
    }
}
