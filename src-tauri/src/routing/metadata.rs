//! 贯穿路由链的请求元数据。
//!
//! 规则按顺序求值时会把这份数据往下传：非终结动作（如 `model_override`）
//! 就地改写它，后续规则看到的就是改写后的值。这正是 sing-box `matchRule`
//! 的模型 —— 先采集特征、再决策，而不是一次算完。

use http::HeaderMap;

use crate::protocol::dto::Protocol;

/// 非终结动作写入的槽位，供后续规则与转发层读取。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteOptions {
    /// 强制指定 selector，覆盖路由链最终选出的那个。
    pub target_selector: Option<String>,
    /// 强制开启 / 关闭响应缓存。
    pub cache: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct RouteMetadata {
    /// 当前生效的模型名。可被 `ModelOverride` 改写。
    pub model: String,
    /// 客户端标识，如 `claude-code` / `codex` / `gemini-cli` / `unknown`。
    pub client: String,
    /// 入站协议。
    pub protocol: Protocol,
    /// 入站请求路径。
    pub path: String,
    pub headers: HeaderMap,
    pub stream: bool,
    /// 入站请求的估算输入 token，供 `token_estimate` 规则分流。
    pub est_input_tokens: u64,
    /// 非终结动作累积的选项。
    pub options: RouteOptions,
}

impl RouteMetadata {
    pub fn new(
        model: impl Into<String>,
        protocol: Protocol,
        path: impl Into<String>,
        headers: HeaderMap,
    ) -> Self {
        Self {
            model: model.into(),
            client: "unknown".to_string(),
            protocol,
            path: path.into(),
            headers,
            stream: false,
            est_input_tokens: 0,
            options: RouteOptions::default(),
        }
    }

    /// 取某个请求头的字符串值。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> RouteMetadata {
        let mut h = HeaderMap::new();
        h.insert("x-apilot-client", "codex".parse().unwrap());
        RouteMetadata::new("gpt-5", Protocol::OpenAiResponses, "/v1/responses", h)
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let m = meta();
        assert_eq!(m.header("x-apilot-client"), Some("codex"));
        assert_eq!(m.header("X-Apilot-Client"), Some("codex"));
        assert_eq!(m.header("missing"), None);
    }

    #[test]
    fn options_default_to_all_none() {
        let m = meta();
        assert!(m.options.target_selector.is_none());
        assert!(m.options.cache.is_none());
    }
}
