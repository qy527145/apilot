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
        // Codex 专用的模型目录。接管时写进它 provider 配置的 `model_catalog_url`，
        // 见 `crate::codex`。**必须显式注册**：落进 fallback 就会被原样转给上游，
        // 客户端拿到的是上游那份错误说明，而不是我们的目录。
        .route(crate::codex::CATALOG_PATH, get(codex_catalog))
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

/// 返回 Codex 用的模型目录。
///
/// 原样把字节发回去，不重新序列化 —— 内容是 `crate::codex` 那份打过补丁的目录，
/// 这里只负责搬运。
async fn codex_catalog() -> Response {
    match crate::codex::catalog() {
        Ok(bytes) => (
            [(http::header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(e) => {
            // 5xx 而不是空目录：客户端拉不到目录只会退回自己的兜底元数据（能用，
            // 只是没有 apply_patch），而回一份空目录会把"我们这边坏了"藏起来。
            tracing::error!("{e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "error": { "message": e.to_string() }
                })),
            )
                .into_response()
        }
    }
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

/// 三种对话入口的路径后缀，含无 `/v1` 的变体。
///
/// 判据是**后缀**而不是全等：客户端会把 base_url 配成带自定义前缀的形式
/// （`http://127.0.0.1:8787/anthropic` → `/anthropic/v1/messages`），
/// 严格全等会让"配好 base_url 却收不到请求"这个老问题重新出现。
///
/// 反过来说，`/v1/messages/count_tokens` 不以后缀里的任何一条结尾，
/// 于是不会被当成对话请求。
const CHAT_SUFFIXES: &[(&str, Protocol)] = &[
    ("/v1/messages", Protocol::AnthropicMessages),
    ("/messages", Protocol::AnthropicMessages),
    ("/v1/chat/completions", Protocol::OpenAiChat),
    ("/chat/completions", Protocol::OpenAiChat),
    ("/v1/responses", Protocol::OpenAiResponses),
    ("/responses", Protocol::OpenAiResponses),
];

/// 这个路径是不是三种对话入口之一。
pub fn chat_protocol(path: &str) -> Option<Protocol> {
    let path = path.trim_end_matches('/');
    CHAT_SUFFIXES
        .iter()
        .find(|(suffix, _)| path.ends_with(suffix))
        .map(|(_, p)| *p)
}

/// 客户端路径 → 该发给上游的路径。
///
/// 客户端可能带自定义 base path（`/anthropic/v1/messages/count_tokens`），那段前缀
/// 是它自己的事，不该原样带上游。从**最后一个 `/v1/`** 开始截：那是各家 API 的
/// 公共约定，截完剩下的交给 `Provider::endpoint()` 去和 base_url 拼（它会处理
/// base 自带 `/v1` 时的重复）。
///
/// 没有 `/v1/` 时原样返回，让 `endpoint()` 按老规矩补上。
pub(crate) fn upstream_path(client_path: &str) -> String {
    let path = client_path.trim_start_matches('/');
    match path.rfind("/v1/") {
        // 保留 `v1/` 这一段：`endpoint()` 靠它判断有没有带版本号。
        Some(i) => path[i + 1..].to_string(),
        None => path.to_string(),
    }
}

/// 路径能看出是哪种协议族的客户端吗。**只用来填日志里那一栏、以及给路由规则一个
/// 协议可匹配**，不参与"该往上游发什么协议"的决策 —— 后者由渠道自己声明。
pub(crate) fn protocol_family(path: &str) -> Option<Protocol> {
    if path.contains("messages") {
        Some(Protocol::AnthropicMessages)
    } else if path.contains("responses") {
        Some(Protocol::OpenAiResponses)
    } else if path.contains("chat") || path.contains("completions") {
        Some(Protocol::OpenAiChat)
    } else {
        None
    }
}

/// 未匹配的路径。
///
/// 认得出是三种对话入口之一的，按对话请求处理；**其余一律原样转发给上游**。
/// 客户端会发 `/v1/messages/count_tokens` 这类非对话接口 —— 硬解成对话请求再编码
/// 回去，轻则丢字段，重则把上游的解释直接改坏；而"这个服务商不支持这个接口"
/// 正是客户端该自己从上游响应里看到的信息，由我们包装一下反而把它藏了。
async fn fallback(State(shell): State<Arc<AppShell>>, req: axum::extract::Request) -> Response {
    let path = req.uri().path().to_string();
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let headers = parts.headers;

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

    match chat_protocol(&path) {
        Some(protocol) => pipeline::handle(shell, protocol, path, headers, bytes).await,
        None => pipeline::handle_raw(shell, method, path, headers, bytes).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_paths_are_recognised_by_suffix() {
        for (path, want) in [
            ("/v1/messages", Protocol::AnthropicMessages),
            ("/messages", Protocol::AnthropicMessages),
            ("/v1/chat/completions", Protocol::OpenAiChat),
            ("/chat/completions", Protocol::OpenAiChat),
            ("/v1/responses", Protocol::OpenAiResponses),
            ("/responses", Protocol::OpenAiResponses),
            // 客户端会带自定义 base path，这也得认。
            ("/anthropic/v1/messages", Protocol::AnthropicMessages),
            ("/openai/v1/chat/completions", Protocol::OpenAiChat),
            // 尾斜杠不该改变结论。
            ("/v1/messages/", Protocol::AnthropicMessages),
        ] {
            assert_eq!(chat_protocol(path), Some(want), "路径 {path}");
        }
    }

    #[test]
    fn non_chat_paths_are_not_mistaken_for_chat() {
        // 这条是关键：`/v1/messages/count_tokens` 也是 Anthropic 家的接口，
        // 但它不是对话请求，硬解成对话再编码回去会把上游的解释改坏。
        for path in [
            "/v1/messages/count_tokens",
            "/messages/count_tokens",
            "/v1/messages/count_tokens/",
            "/v1/files",
            "/v1/nonexistent",
            "/",
        ] {
            assert_eq!(chat_protocol(path), None, "路径 {path} 不该被当成对话请求");
        }
    }

    #[test]
    fn upstream_path_drops_the_client_own_prefix() {
        // 客户端配的 base path 是它自己的事，不该原样带上游。
        assert_eq!(
            upstream_path("/anthropic/v1/messages/count_tokens"),
            "v1/messages/count_tokens"
        );
        assert_eq!(
            upstream_path("/v1/messages/count_tokens"),
            "v1/messages/count_tokens"
        );
        // 没有 `/v1/` 时原样返回，交给 `endpoint()` 按老规矩补。
        assert_eq!(upstream_path("/messages/count_tokens"), "messages/count_tokens");
        // 多个 `/v1/` 时取最后一个 —— 前缀里的那个不是版本号。
        assert_eq!(upstream_path("/api/v1/foo/v1/bar"), "v1/bar");
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
            assert!(
                chat_protocol(p).is_some(),
                "注册的路径 {p} 必须被认成对话请求"
            );
        }
    }

    #[test]
    fn catalog_path_is_served_here_and_is_not_a_chat_route() {
        // 两者都得上锁。落进 fallback 会被原样转给上游，客户端拿到的是上游的错误
        // 说明而不是目录；被认成对话请求则更糟 —— 管线会拿它当请求体去解码。
        assert_eq!(crate::codex::CATALOG_PATH, "/codex/models");
        assert!(chat_protocol(crate::codex::CATALOG_PATH).is_none());
    }

    #[tokio::test]
    async fn catalog_route_serves_a_parseable_model_catalog() {
        // 客户端实际会拿到的那串字节。它是给客户端**解析**的，所以这里按它的读法
        // 验一遍：JSON 可解析、有 models 数组、每条都关掉了 Lite。
        let resp = codex_catalog().await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(http::header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let models = v["models"].as_array().expect("必须是 models 数组");
        assert!(!models.is_empty());
        for m in models {
            assert_eq!(m["use_responses_lite"], serde_json::json!(false), "{}", m["slug"]);
        }
    }
}
