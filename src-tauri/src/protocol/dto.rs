//! 协议无关的中间表示（IR）。
//!
//! 所有协议转换都经过这里：入站字节 → `decode_*` → IR → `encode_*` → 出站字节。
//! N 个协议只需 2N 个编解码器，而不是 N² 个两两转换器。
//!
//! 未建模的原生字段一律进 `extra`，原样透传 —— 宁可少建模，也不要在转换中悄悄丢功能。

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// 支持的线协议。
///
/// **变体名必须显式写死 JSON 名，不能用 `rename_all`。** 类型名里的 `Ai` 在
/// `snake_case` 规则下会被拆成 `open_ai_chat`，而在 `as_str()`（DB 列、日志、
/// 前端联合类型）里一律是 `openai_chat`。两套名字并存会让 Tauri IPC 的入参
/// 反序列化失败 —— 前端发 `openai_chat`，后端却在等 `open_ai_chat`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    /// Anthropic Messages API（`POST /v1/messages`）
    #[serde(rename = "anthropic")]
    AnthropicMessages,
    /// OpenAI Chat Completions（`POST /v1/chat/completions`）
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// OpenAI Responses API（`POST /v1/responses`）
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
}

impl Protocol {
    /// 用于日志、DB 与前端展示的稳定短名。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
        }
    }

    /// [`Protocol::as_str`] 的逆运算。
    ///
    /// `as_str` 的产物会落到 `request_logs.protocol_in/out`，按它筛日志时就需要
    /// 反向解析。两者必须成对维护 —— 落库的名字改了而这里没改，筛选会静默失效。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::AnthropicMessages),
            "openai_chat" => Some(Self::OpenAiChat),
            "openai_responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }

    /// 依据请求路径推断协议。识别不出时返回 `None`，由调用方决定回落策略。
    pub fn from_path(path: &str) -> Option<Self> {
        let p = path.trim_end_matches('/');
        match p {
            "/v1/messages" | "/messages" => Some(Self::AnthropicMessages),
            "/v1/chat/completions" | "/chat/completions" => Some(Self::OpenAiChat),
            "/v1/responses" | "/responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }

    /// 该协议下"流式结束"的标记（用于日志归因与测试断言）。
    pub fn stream_terminator(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "message_stop",
            Self::OpenAiChat => "[DONE]",
            Self::OpenAiResponses => "response.completed",
        }
    }

    /// 该协议的标准请求路径。渠道按自己的线协议选取，与入站路径无关。
    pub fn default_path(&self) -> &'static str {
        match self {
            Self::AnthropicMessages => "/v1/messages",
            Self::OpenAiChat => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 消息角色。
///
/// 比 Anthropic 多一个 `Tool`，因为 OpenAI Chat 用独立的 `role: "tool"` 消息承载
/// 工具结果；转到 Anthropic 时会被折叠回 user 消息里的 `tool_result` block。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// 消息内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// 图片输入。`data` 是 base64 或 URL，取决于 `media_type` 约定。
    Image {
        media_type: String,
        data: String,
    },
    /// 模型发起的工具调用。
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 回传给模型的工具执行结果。
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    /// 扩展思考。`signature` 是 Anthropic 的签名，跨协议降级时无法保真。
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// 被上游加密的思考内容，只能原样回传。
    RedactedThinking {
        data: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }

    /// 取出文本内容；非文本块返回 `None`。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl UnifiedMessage {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    pub fn user_text(s: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentBlock::text(s)])
    }

    /// 把该消息里所有文本块拼起来，用于估算 token 与生成缓存键。
    pub fn concat_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 统一成 JSON Schema。OpenAI 与 Anthropic 的表述差异在 codec 里抹平。
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReasoningConfig {
    pub enabled: bool,
    /// Anthropic 的 `thinking.budget_tokens`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// OpenAI 系的 `reasoning.effort`（low/medium/high）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// 协议无关的入站请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedRequest {
    pub model: String,
    #[serde(default)]
    pub stream: bool,
    /// Anthropic 把 system 放在顶层，OpenAI 放在 messages 里；这里统一为顶层。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<UnifiedMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// 协议原生但未建模的字段，原样透传。
    #[serde(default, flatten)]
    pub extra: IndexMap<String, serde_json::Value>,
}

impl UnifiedRequest {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            stream: false,
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop: Vec::new(),
            reasoning: None,
            extra: IndexMap::new(),
        }
    }

    /// 请求里出现的全部文本，用于粗略估算输入 token。
    pub fn all_text(&self) -> String {
        let mut out = String::new();
        for b in &self.system {
            if let Some(t) = b.as_text() {
                out.push_str(t);
            }
        }
        for m in &self.messages {
            out.push_str(&m.concat_text());
            // 工具调用的入参也要计入，否则工具密集的会话会被严重低估。
            for b in &m.content {
                if let ContentBlock::ToolUse { input, .. } = b {
                    out.push_str(&input.to_string());
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolUse,
    ContentFilter,
    Other { value: String },
}

impl FinishReason {
    /// 从各家协议的字符串归一。
    pub fn parse(s: &str) -> Self {
        match s {
            "stop" | "end_turn" | "stop_sequence" => Self::Stop,
            "length" | "max_tokens" => Self::Length,
            "tool_use" | "tool_calls" | "function_call" => Self::ToolUse,
            "content_filter" | "refusal" => Self::ContentFilter,
            other => Self::Other {
                value: other.to_string(),
            },
        }
    }

    /// 归一后写回各家协议的字符串。
    pub fn to_wire(&self, proto: Protocol) -> &str {
        match (self, proto) {
            (Self::Stop, Protocol::AnthropicMessages) => "end_turn",
            (Self::ToolUse, Protocol::AnthropicMessages) => "tool_use",
            (Self::Length, Protocol::AnthropicMessages) => "max_tokens",
            (Self::Length, Protocol::OpenAiChat) => "length",
            (Self::ToolUse, Protocol::OpenAiChat) => "tool_calls",
            (Self::Stop, Protocol::OpenAiChat) => "stop",
            _ => match self {
                Self::Stop => "stop",
                Self::Length => "max_tokens",
                Self::ToolUse => "tool_use",
                Self::ContentFilter => "content_filter",
                Self::Other { value } => value.as_str(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub finish_reason: FinishReason,
}

impl UnifiedResponse {
    /// 把所有文本块拼成最终答案，写入监控页与捕获表。
    pub fn concat_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

/// usage 的来源，决定计费可信度。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// 上游如实返回。
    #[default]
    Upstream,
    /// 上游没给，本地估算（计费需标记，便于前端提示）。
    LocalEstimate,
}

/// 归一后的用量。
///
/// **口径约定：`input_tokens` 是不含缓存的 fresh 输入**（即 Anthropic 语义）。
/// OpenAI / Responses / Gemini 的 `prompt_tokens` 是**含**缓存的，由各自 codec 负责折算。
/// 若这条约定被破坏，缓存部分会被重复计费 —— 这是本项目最容易踩的坑。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UnifiedUsage {
    /// 未命中缓存的输入 token。
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    /// 上游自报的总量；缺省时由 `total()` 计算。
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub source: UsageSource,
    /// 上游原生 usage 快照，保留用于排障与对账。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

impl UnifiedUsage {
    pub fn total(&self) -> u64 {
        if self.total_tokens > 0 {
            return self.total_tokens;
        }
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_creation_tokens)
            .saturating_add(self.output_tokens)
    }

    /// 非零字段覆盖式合并。
    ///
    /// 流式响应里 usage 常被拆成多个事件下发（Claude 的 `message_start` 给 input、
    /// `message_delta` 给 output），合并时**零值不得覆盖已有正数**。
    pub fn merge_non_zero(&mut self, other: &UnifiedUsage) {
        if other.input_tokens > 0 {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens > 0 {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_read_tokens > 0 {
            self.cache_read_tokens = other.cache_read_tokens;
        }
        if other.cache_creation_tokens > 0 {
            self.cache_creation_tokens = other.cache_creation_tokens;
        }
        if other.reasoning_tokens > 0 {
            self.reasoning_tokens = other.reasoning_tokens;
        }
        if other.total_tokens > 0 {
            self.total_tokens = other.total_tokens;
        }
        if other.source == UsageSource::LocalEstimate {
            self.source = UsageSource::LocalEstimate;
        }
        if other.raw.is_some() {
            self.raw = other.raw.clone();
        }
    }

    /// 是否拿到了任何真实用量。
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// 流式 usage 的增量（字段为 `Option`，未出现的字段不参与合并）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageDelta {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

impl UsageDelta {
    /// 应用到累积器上：只覆盖出现的字段。
    pub fn apply(&self, acc: &mut UnifiedUsage) {
        if let Some(v) = self.input_tokens {
            acc.input_tokens = v;
        }
        if let Some(v) = self.output_tokens {
            acc.output_tokens = v;
        }
        if let Some(v) = self.cache_read_tokens {
            acc.cache_read_tokens = v;
        }
        if let Some(v) = self.cache_creation_tokens {
            acc.cache_creation_tokens = v;
        }
        if let Some(v) = self.reasoning_tokens {
            acc.reasoning_tokens = v;
        }
        if let Some(v) = self.total_tokens {
            acc.total_tokens = v;
        }
    }
}

/// 流式增量：上游 SSE 解码出的中间表示，再由下游协议的 encoder 编回 SSE。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UnifiedDelta {
    /// 流开始，携带 message id 与 model。
    MessageStart { id: String, model: String },
    /// 新内容块开始。
    BlockStart { index: u32, block: ContentBlock },
    TextDelta { index: u32, text: String },
    ThinkingDelta { index: u32, text: String },
    /// 工具入参的分片 JSON。**未收齐前不可解析**。
    ToolInputDelta { index: u32, partial_json: String },
    BlockStop { index: u32 },
    Usage(UsageDelta),
    Finish(FinishReason),
    Error { code: String, message: String },
}

impl UnifiedDelta {
    pub fn text(index: u32, text: impl Into<String>) -> Self {
        Self::TextDelta {
            index,
            text: text.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空的集合字段会**整个消失**，而不是序列化成空数组。
    ///
    /// 这条契约是给前端用的：`UnifiedRequest` 里 `system` / `tools` / `stop` 都带
    /// `skip_serializing_if`，TS 侧若把它们当成必有的数组去 `.map()`，会在
    /// "这次请求没带工具"这种最常见的场景下直接崩掉。前端类型因此标成可选。
    #[test]
    fn empty_collections_are_omitted_from_the_wire_format() {
        let req = UnifiedRequest::new("m");
        let v = serde_json::to_value(&req).unwrap();

        for k in ["system", "tools", "stop"] {
            assert!(
                v.get(k).is_none(),
                "{k} 为空时不该出现在 JSON 里，否则前端会以为它一定是数组"
            );
        }
        // 这两个即使取默认值也必须在，界面上「对话上下文 0 条」要能显示出来。
        assert!(v.get("messages").is_some());
        assert!(v.get("stream").is_some());
    }

    #[test]
    fn protocol_json_name_matches_frontend_contract() {
        // JSON 名字必须与 `as_str()`、DB 里的 protocol_in / protocol_out / channel_kind 列、
        // 以及前端 `src/lib/api.ts` 的联合类型完全一致。
        //
        // 之前靠 `rename_all = "snake_case"` 推导，它会在每个大写字母前插下划线，
        // 于是 `OpenAiChat` 序列化成 `open_ai_chat` —— 和前端发的 `openai_chat` 对不上，
        // 反序列化直接报 unknown variant。
        for p in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
        ] {
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str()),
                "{p:?} 的 JSON 名必须与 as_str() 一致"
            );
            assert_eq!(
                serde_json::from_str::<Protocol>(&format!("\"{}\"", p.as_str())).unwrap(),
                p
            );
        }
    }

    #[test]
    fn parse_is_the_inverse_of_as_str() {
        // 监控页按 `protocol_in` 筛日志时靠这条解析。落库用的名字改了而这里没改，
        // 筛选会静默失效 —— 下拉里选什么都没反应，也不报错。
        for p in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
        ] {
            assert_eq!(Protocol::parse(p.as_str()), Some(p));
        }
        assert_eq!(Protocol::parse("gemini"), None);
        assert_eq!(Protocol::parse(""), None);
    }
}
