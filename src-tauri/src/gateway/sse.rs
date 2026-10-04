//! SSE 增量解析与编码。
//!
//! 两个容易出错的点，这里的实现都是刻意处理的：
//! 1. 分隔符既有 `\r\n\r\n` 也有 `\n\n`，必须取**最早出现**的那个，不能固定优先级；
//! 2. 一个多字节 UTF-8 字符可能被 TCP/HTTP chunk 从中间切开，必须把不完整的尾部
//!    字节留到下一块再拼，否则中文与 emoji 会变成乱码。

use bytes::Bytes;

/// 去掉 `field: ` 或 `field:` 前缀。
pub fn strip_sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    line.strip_prefix(&format!("{field}: "))
        .or_else(|| line.strip_prefix(&format!("{field}:")))
}

/// 从缓冲区取出一个完整的 SSE 块（以空行结束），并从缓冲区中移除。
pub fn take_sse_block(buffer: &mut String) -> Option<String> {
    let mut best: Option<(usize, usize)> = None;

    for (delimiter, len) in [("\r\n\r\n", 4usize), ("\n\n", 2usize)] {
        if let Some(pos) = buffer.find(delimiter) {
            if best.is_none_or(|(best_pos, _)| pos < best_pos) {
                best = Some((pos, len));
            }
        }
    }

    let (pos, len) = best?;
    let block = buffer[..pos].to_string();
    buffer.drain(..pos + len);
    Some(block)
}

/// 把原始字节安全地追加到 UTF-8 缓冲区，正确处理跨 chunk 的多字节字符。
///
/// `remainder` 保存上一块遗留的不完整序列字节（正常最多 3 字节）。
pub fn append_utf8_safe(buffer: &mut String, remainder: &mut Vec<u8>, new_bytes: &[u8]) {
    let input: Vec<u8> = if remainder.is_empty() {
        new_bytes.to_vec()
    } else if remainder.len() > 3 {
        // 防御：正常 UTF-8 流不会留下超过 3 字节的不完整序列。
        // 出现了说明上游在发无效字节，丢弃并重新开始，避免无限累积。
        buffer.push_str(&String::from_utf8_lossy(remainder));
        remainder.clear();
        new_bytes.to_vec()
    } else {
        let mut combined = std::mem::take(remainder);
        combined.extend_from_slice(new_bytes);
        combined
    };

    let mut pos = 0;
    loop {
        match std::str::from_utf8(&input[pos..]) {
            Ok(s) => {
                buffer.push_str(s);
                return;
            }
            Err(e) => {
                let valid_up_to = pos + e.valid_up_to();
                buffer.push_str(&String::from_utf8_lossy(&input[pos..valid_up_to]));
                if let Some(invalid_len) = e.error_len() {
                    // 真正非法的字节：用替换字符占位后继续。
                    buffer.push('\u{FFFD}');
                    pos = valid_up_to + invalid_len;
                } else {
                    // 尾部不完整，留到下一块。
                    *remainder = input[valid_up_to..].to_vec();
                    return;
                }
            }
        }
    }
}

/// 一个解析后的 SSE 事件。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    /// 多行 `data:` 按 `\n` 拼接后的内容。
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    pub fn data(data: impl Into<String>) -> Self {
        Self {
            event: None,
            data: data.into(),
            id: None,
        }
    }

    pub fn named(event: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            event: Some(event.into()),
            data: data.into(),
            id: None,
        }
    }

    /// 上游流结束标记。OpenAI 用字面量 `[DONE]`，而非 JSON。
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }

    /// 把 `data` 当 JSON 解析。
    pub fn json(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.data)
    }

    /// 事件名：优先取 `event:` 字段，缺失时回落到 JSON 里的 `type` 字段。
    ///
    /// OpenAI Responses 只用 `data` 里的 `type` 区分事件，不带 `event:` 行。
    pub fn resolved_name(&self) -> Option<String> {
        if let Some(e) = &self.event {
            return Some(e.clone());
        }
        self.json()
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
    }
}

/// 解析一个 SSE 块为事件。不符合 SSE 结构（无 `data`）时返回 `None`。
pub fn parse_event(block: &str) -> Option<SseEvent> {
    let mut ev = SseEvent::default();
    let mut has_data = false;

    for line in block.lines() {
        // 注释行（以 `:` 开头）按规范应忽略。
        if line.starts_with(':') {
            continue;
        }
        if let Some(v) = strip_sse_field(line, "event") {
            ev.event = Some(v.to_string());
        } else if let Some(v) = strip_sse_field(line, "data") {
            if has_data {
                ev.data.push('\n');
            }
            ev.data.push_str(v);
            has_data = true;
        } else if let Some(v) = strip_sse_field(line, "id") {
            ev.id = Some(v.to_string());
        }
        // `retry:` 与未知字段按规范忽略。
    }

    has_data.then_some(ev)
}

/// 把一个事件编码成 SSE 帧。
pub fn encode_event(ev: &SseEvent) -> Bytes {
    let mut out = String::new();
    if let Some(id) = &ev.id {
        out.push_str("id: ");
        out.push_str(id);
        out.push('\n');
    }
    if let Some(name) = &ev.event {
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
    }
    // data 内的换行必须拆成多个 data: 行，否则接收端会把后续行当未知字段丢掉。
    for line in ev.data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Bytes::from(out)
}

/// 编码终止帧。OpenAI 系用 `[DONE]`，Anthropic 由 `message_stop` 事件收尾，
/// 不需要额外的 `[DONE]`。
pub fn done_event() -> SseEvent {
    SseEvent::data("[DONE]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_sse_field_accepts_optional_space() {
        assert_eq!(
            strip_sse_field("data: {\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(
            strip_sse_field("data:{\"ok\":true}", "data"),
            Some("{\"ok\":true}")
        );
        assert_eq!(strip_sse_field("id:1", "data"), None);
    }

    #[test]
    fn take_sse_block_supports_lf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\n\nrest".to_string();
        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    #[test]
    fn take_sse_block_supports_crlf_delimiters() {
        let mut buffer = "data: {\"ok\":true}\r\n\r\nrest".to_string();
        assert_eq!(
            take_sse_block(&mut buffer),
            Some("data: {\"ok\":true}".to_string())
        );
        assert_eq!(buffer, "rest");
    }

    #[test]
    fn take_sse_block_takes_the_earliest_delimiter_not_the_fixed_priority() {
        // LF 分隔符在前，CRLF 在后：必须取 LF 那个，否则会把两个事件粘成一个。
        let mut buffer = "data: a\n\ndata: b\r\n\r\n".to_string();
        assert_eq!(take_sse_block(&mut buffer), Some("data: a".to_string()));
        assert_eq!(take_sse_block(&mut buffer), Some("data: b".to_string()));
        assert!(buffer.is_empty());
    }

    #[test]
    fn take_sse_block_returns_none_when_incomplete() {
        let mut buffer = "data: partial\n".to_string();
        assert_eq!(take_sse_block(&mut buffer), None);
        // 不应消耗缓冲区内容
        assert_eq!(buffer, "data: partial\n");
    }

    #[test]
    fn ascii_passthrough() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, b"hello world");
        assert_eq!(buf, "hello world");
        assert!(rem.is_empty());
    }

    #[test]
    fn multibyte_char_split_across_chunks_is_reassembled() {
        // "中" 的 UTF-8 是 E4 B8 AD，从中间切开
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &[0xE4, 0xB8]);
        assert_eq!(buf, "", "不完整序列不应产生输出");
        assert_eq!(rem, vec![0xE4, 0xB8]);

        append_utf8_safe(&mut buf, &mut rem, &[0xAD]);
        assert_eq!(buf, "中");
        assert!(rem.is_empty());
    }

    #[test]
    fn four_byte_emoji_split_across_three_chunks() {
        // "😀" = F0 9F 98 80
        let mut buf = String::new();
        let mut rem = Vec::new();
        append_utf8_safe(&mut buf, &mut rem, &[0xF0]);
        append_utf8_safe(&mut buf, &mut rem, &[0x9F]);
        append_utf8_safe(&mut buf, &mut rem, &[0x98]);
        assert_eq!(buf, "");
        append_utf8_safe(&mut buf, &mut rem, &[0x80]);
        assert_eq!(buf, "😀");
        assert!(rem.is_empty());
    }

    #[test]
    fn mixed_content_with_split_multibyte() {
        let payload = "事件: 你好😀";
        let bytes = payload.as_bytes();
        // 逐字节喂入，模拟最恶劣的切片
        let mut buf = String::new();
        let mut rem = Vec::new();
        for b in bytes {
            append_utf8_safe(&mut buf, &mut rem, std::slice::from_ref(b));
        }
        assert_eq!(buf, payload);
        assert!(rem.is_empty());
    }

    #[test]
    fn genuinely_invalid_bytes_become_replacement_char() {
        let mut buf = String::new();
        let mut rem = Vec::new();
        // 0xFF 在任何位置都非法
        append_utf8_safe(&mut buf, &mut rem, &[b'a', 0xFF, b'b']);
        assert_eq!(buf, "a\u{FFFD}b");
        assert!(rem.is_empty());
    }

    #[test]
    fn parse_event_collects_fields() {
        let ev = parse_event("event: message_start\nid: 42\ndata: {\"a\":1}").unwrap();
        assert_eq!(ev.event.as_deref(), Some("message_start"));
        assert_eq!(ev.id.as_deref(), Some("42"));
        assert_eq!(ev.data, "{\"a\":1}");
    }

    #[test]
    fn parse_event_joins_multiline_data() {
        let ev = parse_event("data: line1\ndata: line2").unwrap();
        assert_eq!(ev.data, "line1\nline2");
    }

    #[test]
    fn parse_event_ignores_comments_and_unknown_fields() {
        let ev = parse_event(": keep-alive\nretry: 3000\ndata: x").unwrap();
        assert_eq!(ev.data, "x");
        assert!(ev.event.is_none());
    }

    #[test]
    fn parse_event_without_data_is_none() {
        assert!(parse_event("event: ping").is_none());
    }

    #[test]
    fn is_done_matches_literal_marker() {
        assert!(SseEvent::data("[DONE]").is_done());
        assert!(SseEvent::data(" [DONE] ").is_done());
        assert!(!SseEvent::data("{\"type\":\"done\"}").is_done());
    }

    #[test]
    fn resolved_name_falls_back_to_json_type() {
        // Responses API 不带 event: 行，只有 data 里的 type
        let ev = SseEvent::data("{\"type\":\"response.completed\"}");
        assert_eq!(ev.resolved_name().as_deref(), Some("response.completed"));

        let ev = SseEvent::named("message_stop", "{}");
        assert_eq!(ev.resolved_name().as_deref(), Some("message_stop"));
    }

    #[test]
    fn encode_event_roundtrips_through_parse() {
        let ev = SseEvent::named("message_delta", "{\"x\":1}");
        let encoded = encode_event(&ev);
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(text.ends_with("\n\n"));

        let block = text.trim_end_matches("\n\n");
        let parsed = parse_event(block).unwrap();
        assert_eq!(parsed, ev);
    }

    #[test]
    fn encode_event_splits_embedded_newlines_into_multiple_data_lines() {
        let ev = SseEvent::data("line1\nline2");
        let text = String::from_utf8(encode_event(&ev).to_vec()).unwrap();
        assert_eq!(text, "data: line1\ndata: line2\n\n");
        // 往返后内容不变
        assert_eq!(parse_event("data: line1\ndata: line2").unwrap().data, ev.data);
    }

    #[test]
    fn done_event_is_the_openai_marker() {
        assert_eq!(encode_event(&done_event()).as_ref(), b"data: [DONE]\n\n");
    }
}
