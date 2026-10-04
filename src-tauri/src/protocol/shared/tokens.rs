//! 无上游 usage 时的本地 token 估算。
//!
//! 这不是精确的 tokenizer，而是**计费兜底**：部分上游（尤其中转）不回传 usage，
//! 完全不计费会漏账。宁可给一个偏保守的估算并标记 `source=LocalEstimate`，
//! 让用户在前端看到「本次为估算值」，也不要凭空记 0。
//!
//! 分档依据（对主流 BPE 分词器的经验逼近）：
//! - CJK 字符：约 1 token/字（高估风险小）
//! - ASCII 字母数字：约 4 字符/token
//! - 空白与标点：约 2 字符/token

use super::super::dto::UnifiedRequest;

/// 估算一段文本的 token 数。
pub fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }

    let mut cjk: u64 = 0;
    let mut ascii_alnum: u64 = 0;
    let mut other: u64 = 0;

    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else if ch.is_ascii_alphanumeric() {
            ascii_alnum += 1;
        } else {
            other += 1;
        }
    }

    // 向上取整，避免短文本被估成 0。
    let alnum_tokens = ascii_alnum.div_ceil(4);
    let other_tokens = other.div_ceil(2);

    cjk + alnum_tokens + other_tokens
}

/// CJK 及全角标点：这些字符在主流分词器里基本各占一个 token。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF      // 日文假名
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 基本区
        | 0xAC00..=0xD7AF    // 韩文
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFF00..=0xFFEF    // 全角标点
        | 0x20000..=0x2FA1F  // CJK 扩展 B~F
    )
}

/// 估算整个请求的输入 token。
///
/// 除正文外还要计入工具定义的 JSON（工具密集的 Agent 会话里这部分占比很高）
/// 与每条消息的固定开销。
pub fn estimate_request_tokens(req: &UnifiedRequest) -> u64 {
    // 每条消息的 role 分隔与格式开销，按 4 token 计。
    const PER_MESSAGE_OVERHEAD: u64 = 4;
    // 每次请求的固定系统开销。
    const REQUEST_OVERHEAD: u64 = 3;

    let mut total = REQUEST_OVERHEAD;

    for block in &req.system {
        if let Some(t) = block.as_text() {
            total += estimate_text_tokens(t);
        }
    }

    for m in &req.messages {
        total += PER_MESSAGE_OVERHEAD;
        for block in &m.content {
            total += estimate_block_tokens(block);
        }
    }

    for tool in &req.tools {
        total += estimate_text_tokens(&tool.name);
        if let Some(d) = &tool.description {
            total += estimate_text_tokens(d);
        }
        total += estimate_text_tokens(&tool.input_schema.to_string());
    }

    total
}

fn estimate_block_tokens(block: &super::super::dto::ContentBlock) -> u64 {
    use super::super::dto::ContentBlock;
    match block {
        ContentBlock::Text { text } => estimate_text_tokens(text),
        ContentBlock::Thinking { text, .. } => estimate_text_tokens(text),
        // 图片按固定块计。真实视觉 token 取决于分辨率，这里给一个保守中值。
        ContentBlock::Image { .. } => 1000,
        ContentBlock::ToolUse { name, input, .. } => {
            estimate_text_tokens(name) + estimate_text_tokens(&input.to_string())
        }
        ContentBlock::ToolResult { content, .. } => {
            content.iter().map(estimate_block_tokens).sum()
        }
        ContentBlock::RedactedThinking { data } => estimate_text_tokens(data),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{ContentBlock, ToolDef, UnifiedMessage, UnifiedRequest};
    use serde_json::json;

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn english_is_roughly_quarter_of_length() {
        let t = estimate_text_tokens("hello world this is a test");
        // 26 个字符，约 7 个 token
        assert!((5..=12).contains(&t), "估算值 {t} 超出合理范围");
    }

    #[test]
    fn cjk_is_roughly_one_per_char() {
        let t = estimate_text_tokens("你好世界");
        assert_eq!(t, 4);
    }

    #[test]
    fn mixed_cjk_and_ascii() {
        let t = estimate_text_tokens("你好 hello");
        // 2 个 CJK + "hello"(5 字符 → 2) + 空格(1 → 1)
        assert!((3..=6).contains(&t), "估算值 {t} 超出合理范围");
    }

    #[test]
    fn short_text_never_estimates_to_zero() {
        assert!(estimate_text_tokens("a") >= 1);
        assert!(estimate_text_tokens(".") >= 1);
    }

    #[test]
    fn request_estimate_includes_system_and_messages() {
        let mut req = UnifiedRequest::new("m");
        req.system = vec![ContentBlock::text("你是助手")];
        req.messages = vec![UnifiedMessage::user_text("你好")];

        let t = estimate_request_tokens(&req);
        // 固定开销 3 + 消息开销 4 + 正文 4 + 2 = 13 左右
        assert!(t >= 10, "估算值 {t} 过低");
        assert!(t <= 25, "估算值 {t} 过高");
    }

    #[test]
    fn tool_definitions_contribute_significantly() {
        let mut with_tools = UnifiedRequest::new("m");
        with_tools.messages = vec![UnifiedMessage::user_text("hi")];
        with_tools.tools = vec![ToolDef {
            name: "get_weather".into(),
            description: Some("Get the current weather for a city".into()),
            input_schema: json!({
                "type": "object",
                "properties": { "city": { "type": "string", "description": "city name" } }
            }),
        }];

        let mut without = UnifiedRequest::new("m");
        without.messages = vec![UnifiedMessage::user_text("hi")];

        assert!(
            estimate_request_tokens(&with_tools) > estimate_request_tokens(&without) + 10,
            "工具定义必须计入估算"
        );
    }

    #[test]
    fn image_block_has_fixed_cost() {
        let mut req = UnifiedRequest::new("m");
        req.messages = vec![UnifiedMessage::new(
            crate::protocol::dto::Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "x".into(),
            }],
        )];
        assert!(estimate_request_tokens(&req) >= 1000);
    }

    #[test]
    fn tool_use_input_counts_toward_estimate() {
        let mut req = UnifiedRequest::new("m");
        req.messages = vec![UnifiedMessage::new(
            crate::protocol::dto::Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: "search".into(),
                input: json!({"query": "a fairly long search query string here"}),
            }],
        )];
        assert!(estimate_request_tokens(&req) > 10);
    }
}
