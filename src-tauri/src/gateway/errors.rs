//! 上游报错的语义分类。
//!
//! 网关最需要看懂的一类错误是**上下文超长**：它不是一个"换个渠道就好了"的失败，
//! 而是"这份请求本身就太长"。客户端看到这个信号才会自动压缩上下文再重试
//! （Claude Code、Codex 都有这个动作），看不到就一直干等或者原地重试。
//!
//! 而这件事的难点在于**各家厂商的说法完全不一样**：OpenAI 是结构化的
//! `context_length_exceeded`，Anthropic 是 "prompt is too long"，火山是
//! "Input exceeds the context limit"，z.ai 是 "Prompt too long"……
//! 中转站还会把这些话混着转发。只认一种就等于大部分渠道上这个能力不存在。
//!
//! 两件事都要对：
//!
//! 1. **认得出**：覆盖足够多的说法（含中文），否则用户的压缩永远不触发。
//! 2. **不误判**：把限流（"Too many tokens, please wait"）当成超长，客户端会
//!    压缩一次、再压缩一次，直到把自己压成一个空请求 —— 比不认出来更糟。

use once_cell::sync::Lazy;
use regex::Regex;

/// 各厂商表达"这份请求超出了模型的上下文"时的说法。
///
/// 收录来源：magpie 的 `tooLongRe`（它逐个核对过 pi 收集的一批），中文那几条
/// 是国内中转站与厂商的常见写法。
static TOO_LONG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?ix)
        context_length_exceeded
      | prompt\ is\ too\ long
      | input\ is\ too\ long
      | (exceeds?|exceeded|over|beyond)(\ the)?(\ model'?s?)?(\ maximum)?\ context
      | context\ (length|limit|window)\ (exceeded|is\ exceeded)
      | maximum\ context\ length
      | too\ many\ (input\ |prompt\ )?tokens
      | 上下文(长度)?(超|过长)
      | 超(过|出)(了)?(模型)?(的)?(最大)?上下文
      | input\ token\ count.*exceeds\ the\ maximum
      | maximum\ prompt\ length\ is\ \d
      | reduce\ the\ length\ of\ the\ messages
      | maximum\ allowed\ input\ length
      | longer\ than\ the\ model'?s\ context\ length
      | available\ context\ size
      | greater\ than\ the\ context\ length
      | prompt\ token\ count\ of\ [\d,]+ exceeds\ the\ limit
      | context\ window\ exceeds\ limit
      | exceeded\ model\ token\ limit
      | prompt\ too\ long
      | prompt\ exceeds\ max\ length
      | context_window_exceeded
      | exceeded\ (max\ |maximum\ )?context\ length
      | range\ of\ input\ length\ should\ be
      | token\ limit\ exceeded
      | input\ tokens?(\ count)?(\ exceeds?|exceeded)(\ the)?(\ max(imum)?)?\ limit
    ",
    )
    .expect("上下文超长的判定正则必须能编译")
});

/// 说的是"整个对话装不下了"吗？
///
/// 认三条：状态码像请求错误、报文里有上面那些话、**而且不是在说限流**。
///
/// 最后一条是必须的。限流也在数 token（Bedrock 的 "Too many tokens, please wait"、
/// 各种 "tokens per minute"），把它当成超长的后果不是少做什么，而是**做错**：
/// 客户端会压缩一次、再压缩一次，直到把上下文压成一个空请求才发现没用。
pub fn is_context_overflow(status: u16, message: &str) -> bool {
    // 只有"请求本身有问题"的码才可能是超长：2xx/3xx 不是错误，5xx 是服务端的事，
    // 429 是限流（就是上面要防的那一类）。
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }

    let lower = message.to_lowercase();

    // 关于 `max_tokens` 的错误说的是"回复的额度"不够，不是"对话太长" ——
    // 压缩上下文帮不上忙，还可能白压一次。
    for key in ["max_tokens", "max_output_tokens", "max_completion_tokens"] {
        if lower.contains(key) {
            return false;
        }
    }

    if PACE.is_match(message) {
        return false;
    }

    TOO_LONG.is_match(message)
}

/// 说的是"多快/多频繁"，不是"一次能带多大"。
static PACE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)rate.?limit|per (minute|hour|day)|\bTP[MD]\b|please wait|try again later|throttl")
        .expect("限流的判定正则必须能编译")
});

/// 给客户端看的超长报文。
///
/// **`prompt is too long:` 前缀不能省**：Anthropic 的客户端（Claude Code）就是
/// 按这几个字判断"该压缩了"的，换个说法它就不动。原文附在后面，用户与我们自己
/// 都还能看到是哪家渠道说了什么。
pub fn overflow_message(vendor_said: &str) -> String {
    let said = vendor_said.trim();
    if said.to_lowercase().contains("prompt is too long") {
        return said.to_string();
    }
    if said.is_empty() {
        return "prompt is too long".to_string();
    }
    format!("prompt is too long: {said}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendors_ways_of_saying_it_are_all_recognized() {
        // 每一条都是一种真实存在的说法。漏掉哪一条，那家渠道上的用户就只能
        // 自己发现"上下文满了"，而客户端的自动压缩永远不触发。
        let said = [
            r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}"#,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 214712 tokens > 200000 maximum"}}"#,
            r#"{"error":{"message":"Input exceeds the context limit"}}"#,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens"}}"#,
            r#"{"error":{"message":"请求的上下文长度超过了模型的最大上下文"}}"#,
            r#"{"error":{"message":"exceeded model token limit"}}"#,
            r#"{"error":{"message":"reduce the length of the messages"}}"#,
            r#"{"error":{"message":"Range of input length should be [1, 65536]"}}"#,
            r#"{"error":{"message":"Prompt too long"}}"#,
        ];
        for body in said {
            // 上游报文到这里已经是 `extract_error_message` 解出来的那条 message
            // （JSON 转义在解析时就解开了），所以判定拿到的是纯文本。
            let said = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .or_else(|| v.pointer("/message"))
                        .and_then(|m| m.as_str().map(String::from))
                })
                .unwrap_or_else(|| body.to_string());
            assert!(
                is_context_overflow(400, &said),
                "认不出这种说法，客户端的自动压缩就不会触发: {body}"
            );
        }
    }

    #[test]
    fn a_rate_limit_is_never_read_as_too_long() {
        // 误判的后果不是"少做一件事"，而是**做错**：客户端会压缩一次、再压缩一次，
        // 直到把上下文压成一个空请求才发现没用。
        for body in [
            r#"{"error":{"message":"Too many tokens, please wait"}}"#,
            r#"{"error":{"message":"Rate limit reached: 100000 tokens per minute"}}"#,
            r#"{"error":{"message":"Too many requests, throttled"}}"#,
        ] {
            assert!(
                !is_context_overflow(400, body),
                "限流不是超长: {body}"
            );
        }
        // 429 无论说什么都不是超长。
        assert!(!is_context_overflow(
            429,
            "context length exceeded, please retry"
        ));
    }

    #[test]
    fn a_max_tokens_complaint_is_not_about_the_prompt() {
        // 说的是"回复的额度"不够，不是"对话太长"。压缩上下文帮不上忙。
        assert!(!is_context_overflow(
            400,
            r#"{"error":{"message":"max_tokens: 200000 > 64000, context limit exceeded"}}"#
        ));
    }

    #[test]
    fn a_server_error_is_not_the_requests_fault() {
        for status in [500, 502, 503, 529] {
            assert!(!is_context_overflow(
                status,
                "prompt is too long: exceeds maximum context length"
            ));
        }
    }

    #[test]
    fn a_chinese_complaint_is_read() {
        // 国内渠道与中转站常常用中文说这件事。
        assert!(is_context_overflow(400, "请求的上下文长度超过了模型的最大上下文"));
        assert!(is_context_overflow(400, "上下文超长，请缩短输入"));
    }

    #[test]
    fn an_overflow_message_keeps_the_prefix_claude_code_looks_for() {
        // Claude Code 就是按 "prompt is too long" 这几个字判断该压缩的。
        let m = overflow_message("Input exceeds the context limit");
        assert!(m.starts_with("prompt is too long"), "{m}");
        assert!(m.contains("Input exceeds the context limit"), "原文要留着: {m}");

        // 上游本来就这么说的，就不要叠成两遍。
        let same = overflow_message("prompt is too long: 214712 tokens > 200000 maximum");
        assert_eq!(same.matches("prompt is too long").count(), 1, "{same}");
    }
}
