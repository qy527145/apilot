//! 工具调用的跨协议语义处理。
//!
//! 三种协议表达工具的方式差异很大：
//! - Anthropic：assistant 消息里的 `tool_use` 块，user 消息里的 `tool_result` 块
//! - OpenAI Chat：assistant 消息的 `tool_calls` 数组，独立的 `role: "tool"` 消息
//! - OpenAI Responses：扁平的 `function_call` / `function_call_output` item
//!
//! 转换的要点是**保留 id 关联**：模型发出的调用 id 必须原样出现在结果里，
//! 否则上游会拒绝或把结果错配到别的调用上。

use serde_json::Value;

use super::super::dto::ContentBlock;

/// 工具入参的 JSON 文本可能是分片累积的，收齐后解析。
///
/// 解析失败时返回空对象而非报错 —— 上游偶尔会发出不完整的 JSON，
/// 此时保留空入参比让整个请求失败更合理。
pub fn parse_tool_input(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({}))
}

/// 把工具结果压成纯文本。
///
/// OpenAI 的 tool 消息只接受字符串内容，Anthropic 允许块数组。
/// 从 Anthropic 转向 OpenAI 时需要把块数组拍平。
pub fn flatten_tool_result_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Image { .. } => out.push_str("[image]"),
            // 嵌套的工具调用不该出现在结果里，忽略以免递归。
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_tool_input_handles_valid_json() {
        assert_eq!(parse_tool_input(r#"{"a":1}"#), json!({"a": 1}));
    }

    #[test]
    fn parse_tool_input_falls_back_to_empty_object() {
        // 分片未收齐时是不完整 JSON，不应让整个请求失败
        assert_eq!(parse_tool_input(r#"{"a":"#), json!({}));
        assert_eq!(parse_tool_input(""), json!({}));
        assert_eq!(parse_tool_input("   "), json!({}));
    }

    #[test]
    fn flatten_tool_result_joins_text_blocks() {
        let blocks = vec![
            ContentBlock::text("line1"),
            ContentBlock::text("line2"),
        ];
        assert_eq!(flatten_tool_result_text(&blocks), "line1line2");
    }

    #[test]
    fn flatten_tool_result_marks_images() {
        let blocks = vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "x".into(),
        }];
        assert_eq!(flatten_tool_result_text(&blocks), "[image]");
    }

}
