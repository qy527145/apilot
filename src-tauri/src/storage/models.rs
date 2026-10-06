//! 数据库行结构体。

use serde::{Deserialize, Serialize};

use crate::protocol::dto::Protocol;

/// 渠道类型 = 该渠道说哪种线协议。
///
/// JSON 名显式写死，理由同 `Protocol`：`rename_all = "snake_case"` 会把
/// `OpenAiChat` 拆成 `open_ai_chat`，与 `as_str()`（DB 的 kind 列）和前端
/// 联合类型里的 `openai_chat` 对不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
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

/// 服务商在某种协议下的接入点。
///
/// 一个服务商常常同时提供多种协议（比如同一家既给 `/v1/messages` 也给
/// `/v1/chat/completions`）。声明出来之后，客户端说什么协议就直接说什么协议，
/// 省掉一次编解码，也避开转换带来的字段损耗；只有声明里没有的入站协议才转换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolEndpoint {
    pub protocol: Protocol,
    /// 该协议在该服务商下的请求路径，`None` 表示用协议默认路径。
    ///
    /// 存在的意义：服务商路径不遵循 `/v1/<name>` 约定时（挂在网关子路径下、
    /// 或版本号不同），用户能直接指定，不必让 Apilot 去猜 —— 猜错的后果是 404。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl ProtocolEndpoint {
    /// 声明"支持该协议但不覆盖路径"。
    pub fn plain(protocol: Protocol) -> Self {
        Self {
            protocol,
            path: None,
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
    /// **首选协议**：探测连通性、拉 `/v1/models`、以及入站协议不在 `protocols` 里时
    /// 转换的目标协议。实际用哪种协议由 [`Provider::wire_for`] 按入站请求决定。
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
    pub auth_style: AuthStyle,
    /// 该服务商支持哪些协议、各自的请求路径。空表示"只支持 `kind` 那一种"。
    #[serde(default)]
    pub protocols: Vec<ProtocolEndpoint>,
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

    /// 实际可用的协议列表。
    ///
    /// 没声明过协议时退化成"只有 `kind` 那一种" —— 老数据、预设、以及没动过这一项的
    /// 渠道行为完全不变，不会因为引入这个字段而改变既有路由结果。
    pub fn endpoints(&self) -> Vec<ProtocolEndpoint> {
        if self.protocols.is_empty() {
            vec![ProtocolEndpoint::plain(self.kind.wire_protocol())]
        } else {
            self.protocols.clone()
        }
    }

    pub fn supports(&self, protocol: Protocol) -> bool {
        self.endpoints().iter().any(|e| e.protocol == protocol)
    }

    /// 针对入站协议选一个上游协议。
    ///
    /// 能同协议就同协议 —— 此时管线会走直通（原始字节转发，只旁路统计用量），
    /// 既不丢字段也不多一次编解码；对不上才回落到首选协议，由管线做跨协议转换。
    pub fn wire_for(&self, incoming: Protocol) -> Protocol {
        if self.supports(incoming) {
            incoming
        } else {
            self.kind.wire_protocol()
        }
    }

    /// 某种协议对应的出站 URL。
    ///
    /// 是个纯映射：给什么协议就拼什么协议的路径，不在声明里就用该协议的默认路径。
    /// 「该用哪种协议」是 [`Provider::wire_for`] 的职责，两者分开，
    /// 免得这里偷偷换协议、调用方还以为自己拿的是想要的那个 URL。
    pub fn endpoint_for(&self, protocol: Protocol) -> String {
        let override_path = self
            .endpoints()
            .into_iter()
            .find(|e| e.protocol == protocol)
            .and_then(|e| e.path);

        match override_path {
            Some(path) => self.endpoint_verbatim(&path),
            None => self.endpoint(protocol.default_path()),
        }
    }

    /// 该渠道要注入上游的鉴权头；不该注入时返回 `None`。
    ///
    /// 出站转发（`upstream/channel.rs::prepare`）和探测 / 拉取模型
    /// （`commands/providers.rs`）都走这里，避免「加了一种鉴权方式只改了其中一处」——
    /// 那种漏改会表现为"测试连通失败但真发请求能过"，很难排查。
    pub fn auth_header(&self) -> Option<(&'static str, String)> {
        let key = self.api_key.as_deref().filter(|k| !k.is_empty())?;
        match self.auth_style {
            AuthStyle::Bearer => Some(("authorization", format!("Bearer {key}"))),
            AuthStyle::XApiKey => Some(("x-api-key", key.to_string())),
            AuthStyle::None => None,
        }
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

    /// 拼接一个**用户显式给出**的路径。
    ///
    /// 与 [`Provider::endpoint`] 的区别：只做「两边都带 /v1 时去掉重复的那一次」，
    /// 绝不替用户补 `/v1`。`endpoint` 会补，是因为它拿到的都是协议默认路径
    /// （硬编码 `/v1/...`）；而用户手写的路径可能是 `/api/chat` 这种，
    /// 被补成 `/v1/api/chat` 就是一个必然 404 —— 「我写什么就发什么」比猜得聪明重要。
    fn endpoint_verbatim(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');

        if base.ends_with("/v1") {
            if let Some(rest) = path.strip_prefix("v1/") {
                return format!("{base}/{rest}");
            }
        }
        format!("{base}/{path}")
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
            protocols: Vec::new(),
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
    fn auth_header_covers_every_style() {
        let mut p = provider("https://x");

        p.auth_style = AuthStyle::Bearer;
        p.api_key = Some("sk-1".into());
        assert_eq!(
            p.auth_header(),
            Some(("authorization", "Bearer sk-1".into()))
        );

        p.auth_style = AuthStyle::XApiKey;
        assert_eq!(p.auth_header(), Some(("x-api-key", "sk-1".into())));

        // none 表示鉴权由 extra_headers 或上游自身负责，不能凭空塞一个头。
        p.auth_style = AuthStyle::None;
        assert_eq!(p.auth_header(), None);
    }

    #[test]
    fn auth_header_is_none_without_usable_key() {
        let mut p = provider("https://x");
        p.auth_style = AuthStyle::Bearer;

        p.api_key = None;
        assert_eq!(p.auth_header(), None);

        // 空字符串是前端"留空表示不修改"残留的形态，等同于没配。
        p.api_key = Some(String::new());
        assert_eq!(p.auth_header(), None);
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

    #[test]
    fn provider_kind_json_name_matches_db_and_frontend() {
        // 前端 `ProviderKind` 联合类型、DB 的 kind 列、`as_str()` 必须是同一套名字。
        // `rename_all = "snake_case"` 会把 OpenAiChat 变成 open_ai_chat，与它们对不上，
        // 后果是前端保存 OpenAI 系渠道时 ProviderInput 直接反序列化失败。
        for (k, name) in [
            (ProviderKind::Anthropic, "anthropic"),
            (ProviderKind::OpenAiChat, "openai_chat"),
            (ProviderKind::OpenAiResponses, "openai_responses"),
        ] {
            assert_eq!(k.as_str(), name);
            assert_eq!(serde_json::to_string(&k).unwrap(), format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<ProviderKind>(&format!("\"{name}\"")).unwrap(), k);
        }
    }

    #[test]
    fn endpoints_fall_back_to_kind_when_nothing_declared() {
        // 没声明过的渠道必须保持"只有 kind 那一种协议"的既有行为。
        let p = provider("https://x");
        let es = p.endpoints();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].protocol, Protocol::AnthropicMessages);
        assert!(es[0].path.is_none());
    }

    #[test]
    fn endpoints_use_declared_list_when_present() {
        let mut p = provider("https://x");
        p.protocols = vec![
            ProtocolEndpoint::plain(Protocol::AnthropicMessages),
            ProtocolEndpoint::plain(Protocol::OpenAiChat),
        ];
        assert_eq!(p.endpoints().len(), 2);
        assert!(p.supports(Protocol::AnthropicMessages));
        assert!(p.supports(Protocol::OpenAiChat));
        assert!(!p.supports(Protocol::OpenAiResponses));
    }

    #[test]
    fn wire_for_prefers_the_incoming_protocol() {
        let mut p = provider("https://x");
        p.kind = ProviderKind::OpenAiChat;
        p.protocols = vec![
            ProtocolEndpoint::plain(Protocol::OpenAiChat),
            ProtocolEndpoint::plain(Protocol::AnthropicMessages),
        ];

        // 声明里有就用它 —— 同协议直通，不转换。
        assert_eq!(
            p.wire_for(Protocol::AnthropicMessages),
            Protocol::AnthropicMessages
        );
        // 声明里没有才回落到首选协议。
        assert_eq!(p.wire_for(Protocol::OpenAiResponses), Protocol::OpenAiChat);
    }

    #[test]
    fn endpoint_for_uses_default_path_and_honours_override() {
        let mut p = provider("https://api.example.com");
        // 未覆盖时用协议默认路径。
        assert_eq!(
            p.endpoint_for(Protocol::OpenAiChat),
            "https://api.example.com/v1/chat/completions"
        );

        p.protocols = vec![ProtocolEndpoint {
            protocol: Protocol::OpenAiChat,
            path: Some("/api/chat".into()),
        }];
        assert_eq!(p.endpoint_for(Protocol::OpenAiChat), "https://api.example.com/api/chat");
        // 没在声明里的协议仍然拼得出来（用它的默认路径），不会 panic ——
        // endpoint_for 是纯映射，"该不该用这个协议"由 wire_for 决定。
        assert_eq!(
            p.endpoint_for(Protocol::AnthropicMessages),
            "https://api.example.com/v1/messages"
        );
    }

    #[test]
    fn endpoint_for_never_invents_a_v1_prefix_for_custom_paths() {
        // 用户手写的路径直接拼在 base_url 后面。"/api/chat" 被补成
        // "/v1/api/chat" 就是一次必然 404，而按协议默认路径来的 "/v1/..." 又
        // 不能重复拼出 "/v1/v1/..."，两种意图必须分开处理。
        let mut p = provider("https://gw.example.com");
        p.protocols = vec![ProtocolEndpoint {
            protocol: Protocol::OpenAiChat,
            path: Some("api/chat".into()),
        }];
        assert_eq!(p.endpoint_for(Protocol::OpenAiChat), "https://gw.example.com/api/chat");
    }

    #[test]
    fn endpoint_for_does_not_duplicate_v1_with_override() {
        // base_url 自带 /v1、覆盖路径也带 /v1 时，去重逻辑要同样生效。
        let mut p = provider("https://api.example.com/v1");
        p.protocols = vec![ProtocolEndpoint {
            protocol: Protocol::OpenAiChat,
            path: Some("/v1/chat/completions".into()),
        }];
        assert_eq!(
            p.endpoint_for(Protocol::OpenAiChat),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn protocols_roundtrip_through_json() {
        // 前端存的 JSON 与后端解析的必须是同一形状（path 可省略）。
        let raw = r#"[{"protocol":"anthropic"},{"protocol":"openai_chat","path":"/api/chat"}]"#;
        let parsed: Vec<ProtocolEndpoint> = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].protocol, Protocol::AnthropicMessages);
        assert_eq!(parsed[0].path, None);
        assert_eq!(parsed[1].path.as_deref(), Some("/api/chat"));

        // 没有 path 的条目序列化后不该带上 null，免得前端多一层判断。
        assert_eq!(serde_json::to_string(&parsed[0]).unwrap(), r#"{"protocol":"anthropic"}"#);
    }
}
