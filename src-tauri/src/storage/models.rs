//! 数据库行结构体。

use serde::{Deserialize, Serialize};

use crate::protocol::dto::Protocol;

/// 渠道类型 = 该渠道说哪种线协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    OpenAiChat,
    OpenAiResponses,
}

impl ProviderKind {
    /// 该渠道对上游使用哪种线协议。协议转换的目标由它决定。
    pub fn wire_protocol(&self) -> Protocol {
        match self {
            Self::Anthropic => Protocol::AnthropicMessages,
            Self::OpenAiChat => Protocol::OpenAiChat,
            Self::OpenAiResponses => Protocol::OpenAiResponses,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            "openai_chat" | "openai" => Some(Self::OpenAiChat),
            "openai_responses" | "responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }
}

/// 鉴权头风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>`（Anthropic 官方）
    XApiKey,
    /// 不注入鉴权头（上游自带，或走 extra_headers）。
    None,
}

impl AuthStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
            Self::None => "none",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "x-api-key" => Self::XApiKey,
            "none" => Self::None,
            _ => Self::Bearer,
        }
    }
}

/// 一个上游渠道。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: i64,
    /// 路由与 selector 引用的稳定标识。
    pub tag: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub auth_style: AuthStyle,
    /// 渠道级额外请求头。
    pub extra_headers: indexmap::IndexMap<String, String>,
    /// 请求体字段覆盖（如强制 temperature、加 top_k）。
    pub param_override: Option<serde_json::Value>,
    /// 入站模型名 → 上游模型名。
    pub model_mapping: indexmap::IndexMap<String, String>,
    pub weight: i64,
    pub priority: i64,
    pub enabled: bool,
    pub timeout_ms: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Provider {
    /// 把入站模型名映射为上游模型名；没有映射时原样返回。
    pub fn upstream_model<'a>(&'a self, model: &'a str) -> &'a str {
        self.model_mapping
            .get(model)
            .map(|s| s.as_str())
            .unwrap_or(model)
    }

    /// 拼接出站 URL。`path` 形如 `/v1/messages`。
    ///
    /// base_url 可能已经带 `/v1` 后缀（用户常这么填），需要去重，
    /// 否则会拼出 `/v1/v1/messages`。
    pub fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');

        let base_has_v1 = base.ends_with("/v1");
        let path_has_v1 = path.starts_with("v1/");

        let joined = match (base_has_v1, path_has_v1) {
            (true, true) => format!("{base}/{}", &path[3..]),
            (false, false) => format!("{base}/v1/{path}"),
            (true, false) => format!("{base}/{path}"),
            (false, true) => format!("{base}/{path}"),
        };
        joined
    }
}

/// 某模型在某渠道下的可用性条目（等价于 new-api 的 abilities）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: i64,
    pub provider_id: i64,
    pub model: String,
    pub upstream_model: Option<String>,
    pub client_group: String,
    pub priority: i64,
    pub weight: i64,
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base_url: &str) -> Provider {
        Provider {
            id: 1,
            tag: "t".into(),
            name: "n".into(),
            kind: ProviderKind::Anthropic,
            base_url: base_url.into(),
            api_key: None,
            auth_style: AuthStyle::XApiKey,
            extra_headers: Default::default(),
            param_override: None,
            model_mapping: Default::default(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn endpoint_joins_bare_base_url() {
        assert_eq!(
            provider("https://api.anthropic.com").endpoint("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_does_not_duplicate_v1() {
        // 用户把 base_url 填成带 /v1 的形态很常见，不能拼成 /v1/v1/...
        assert_eq!(
            provider("https://api.anthropic.com/v1").endpoint("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_handles_trailing_slash() {
        assert_eq!(
            provider("https://api.deepseek.com/").endpoint("chat/completions"),
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_respects_custom_paths() {
        assert_eq!(
            provider("https://gw.example.com").endpoint("/custom/path"),
            "https://gw.example.com/v1/custom/path"
        );
    }

    #[test]
    fn upstream_model_uses_mapping_when_present() {
        let mut p = provider("https://x");
        p.model_mapping.insert("gpt-4o".into(), "deepseek-chat".into());
        assert_eq!(p.upstream_model("gpt-4o"), "deepseek-chat");
        assert_eq!(p.upstream_model("other"), "other");
    }

    #[test]
    fn provider_kind_maps_to_wire_protocol() {
        assert_eq!(
            ProviderKind::Anthropic.wire_protocol(),
            Protocol::AnthropicMessages
        );
        assert_eq!(ProviderKind::OpenAiChat.wire_protocol(), Protocol::OpenAiChat);
        assert_eq!(
            ProviderKind::OpenAiResponses.wire_protocol(),
            Protocol::OpenAiResponses
        );
    }

    #[test]
    fn provider_kind_parse_accepts_aliases() {
        assert_eq!(ProviderKind::parse("openai"), Some(ProviderKind::OpenAiChat));
        assert_eq!(
            ProviderKind::parse("responses"),
            Some(ProviderKind::OpenAiResponses)
        );
        assert_eq!(ProviderKind::parse("nope"), None);
    }
}
