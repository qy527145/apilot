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

/// 取出响应里模型发起的全部工具调用名（按出现顺序，可重复）。
///
/// 只给**工具频率统计**用，数的是响应、不是请求：Agent 每轮都会把全部历史
/// 重发一遍，从请求里数会把同一个调用重复计 N 次。空名字要跳过 —— 流被中断时
/// 累加器会留下一个还没拿到 `name` 的半截 `ToolUse`。
///
/// 名字原样返回，**不做跨客户端归一化**：`Bash` 与 `exec_command` 语义相近但
/// 不等价，硬映射成一个名字只会让统计表变成一张谁都要重新解释的表。
///
/// 与 `gateway::pipeline::body_patch::count_tool_uses` 不是一回事：那个数的是
/// **请求历史**里的调用，用于计费（一条请求里带了多少次工具调用）。两者口径
/// 不同，别合并。
pub fn tool_call_names(content: &[ContentBlock]) -> Vec<String> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { name, .. } if !name.is_empty() => Some(name.clone()),
            _ => None,
        })
        .collect()
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

    #[test]
    fn tool_call_names_keeps_order_and_repeats() {
        let content = vec![
            ContentBlock::text("让我看看"),
            ContentBlock::ToolUse {
                id: "1".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "2".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "3".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            },
        ];
        // 并行读文件是常态：同名工具要出现两次，不能去重成调用次数。
        assert_eq!(tool_call_names(&content), vec!["Read", "Read", "Bash"]);
    }

    #[test]
    fn tool_call_names_skips_empty_names_and_other_blocks() {
        let content = vec![
            ContentBlock::ToolUse {
                id: "1".into(),
                name: String::new(),
                input: serde_json::json!({}),
            },
            ContentBlock::Thinking {
                text: "想了想".into(),
                signature: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "1".into(),
                content: vec![ContentBlock::text("ok")],
                is_error: false,
            },
        ];
        assert!(
            tool_call_names(&content).is_empty(),
            "流被中断时留下的空名调用不该计成一个工具"
        );
    }

}
