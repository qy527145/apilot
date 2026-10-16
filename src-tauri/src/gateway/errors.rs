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
//!
//! 另一类要认出来的是**"2xx 但根本不是 API 的回复"**：中转站挂了、Cloudflare
//! 弹了验证页、登录过期跳了登录页 —— 这些都以 200 返回，内容却是网页或空。
//! 不认的话它会被当成正常答案转给客户端（Codex 收到一页 HTML 后只会报
//! "stream closed before response.completed"），而且**下一个渠道永远不会被试**：
//! 既然"成功了"，就没有换渠道的理由。

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

/// 一个 2xx 的回复看起来根本不是 API 的答案吗？返回原因，`None` 表示看着正常。
///
/// `status` 一并传进来是为了排除 `204 No Content` —— 那是个明确的"没有正文"，
/// 不是坏掉的回复。
///
/// `head` 是能看到的正文开头：`Some` 表示正文在手（可以判空、判开头），
/// `None` 表示只凭响应头判 —— 流式响应就是这样，它的正文还没读，也不该为了
/// 判断而把整个流缓冲下来。判据来自 magpie 的 `notAnAPIReply`：
///
/// - `Content-Type: text/html`：登录页、验证页、中转站自己的错误页；
/// - 正文以 `<html` / `<!doctype html` / `<head` / `<body` 开头（而声明不是 XML）；
/// - 正文**完全是空的**；
/// - 声明是 JSON 却以 `{` `[` 之外的字符开头。
///
/// 只要开头几个字节就够判，是因为流式响应本来就要等第一个字节能到 —— 不必为了
/// 判断把整个流缓冲下来。读不到（连接就断）时一律放行：认不出来不等于有罪，
/// 而误判成 502 会让一个本来正常的流被丢掉。
pub fn not_an_api_reply(status: u16, content_type: &str, head: Option<&[u8]>) -> Option<String> {
    // 只管 2xx，且 204 是明确合法的"无内容"。
    if !(200..300).contains(&status) || status == 204 {
        return None;
    }

    let ct = content_type.to_ascii_lowercase();
    let ct_main = ct.split(';').next().unwrap_or("").trim().to_string();

    // 这一条只看响应头，两种情形都适用 —— 也正是最常见的那种：
    // 中转站挂了、登录过期、Cloudflare 挑战，都以 200 回一页网页。
    if ct_main.starts_with("text/html") {
        return Some(web_page_reason(head.unwrap_or(&[]), &ct_main));
    }

    // 下面几条要看到正文，流式那条路上没有正文可看。
    let Some(head) = head else {
        return None;
    };

    // 去掉开头的空白与 BOM 再看 —— 有些服务会先把它们写进去。
    let trimmed: Vec<u8> = head
        .iter()
        .copied()
        .skip_while(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .collect();
    let trimmed = trimmed.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&trimmed);

    if trimmed.is_empty() {
        // 空正文：200 却什么都没有，不可能是模型的答案。
        return Some("上游回了 200 但正文是空的，不是 API 的回复".to_string());
    }

    // 声明是 XML 时以 `<` 开头是正常的（有些网关就回 XML）。
    if !ct_main.contains("xml") && starts_like_a_page(trimmed) {
        return Some(web_page_reason(head, &ct_main));
    }

    if ct_main.contains("json") && !matches!(trimmed[0], b'{' | b'[') {
        let sample = String::from_utf8_lossy(&trimmed[..trimmed.len().min(200)]);
        return Some(format!(
            "上游声明是 JSON 却回了一段不是 JSON 的内容，不是 API 的回复：{}",
            clip(&sample, 200)
        ));
    }

    None
}

/// 正文开头像不像一个网页。
fn starts_like_a_page(head: &[u8]) -> bool {
    let lower: Vec<u8> = head
        .iter()
        .take(64)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    let s = String::from_utf8_lossy(&lower);
    s.starts_with("<!doctype")
        || s.starts_with("<html")
        || s.starts_with("<head")
        || s.starts_with("<body")
        || s.starts_with("<meta")
}

/// 网页的原因，尽量带上标题 —— 用户一眼就知道自己撞上了什么（验证页还是登录页）。
fn web_page_reason(head: &[u8], ct: &str) -> String {
    let shown = if ct.is_empty() {
        "没给 Content-Type"
    } else {
        ct
    };
    let mut why = format!("上游回了 200 但内容是一个网页（{shown}），不是 API 的回复");
    if let Some(title) = page_title(head) {
        why.push_str(&format!("，标题是「{title}」"));
    }
    why
}

/// 从网页片段里抠出 `<title>`。
fn page_title(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    let lower = text.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let after = lower[start..].find('>')? + start + 1;
    let end = lower[after..].find("</title>")? + after;
    let title = text[after..end].trim();
    if title.is_empty() {
        return None;
    }
    Some(clip(title, 120))
}

/// 按**字符**（而不是字节）截断，免得把一个多字节字符切成两半。
fn clip(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.trim().to_string();
    }
    let head: String = chars[..max_chars].iter().collect();
    format!("{}…", head.trim())
}

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

    #[test]
    fn a_sign_in_page_served_200_is_not_an_answer() {
        // 中转站挂了或登录过期时常以 200 回一页 HTML。当成正常答案转给客户端，
        // 客户端的报错会是"流在完成前断了"，完全指不到真正的原因。
        let page = b"<!DOCTYPE html><html><head><title>Sign in</title></head><body>...</body></html>";
        let why = not_an_api_reply(200, "text/html; charset=utf-8", Some(page)).expect("必须认出来");
        assert!(why.contains("网页"), "{why}");
        assert!(why.contains("Sign in"), "标题能帮用户一眼认出是什么页: {why}");
    }

    #[test]
    fn a_page_without_a_content_type_is_still_refused() {
        // 有些服务连 Content-Type 都不给。
        let page = b"<html><body>Just a moment...</body></html>";
        assert!(not_an_api_reply(200, "", Some(page)).is_some());
        assert!(not_an_api_reply(200, "application/octet-stream", Some(page)).is_some());
    }

    #[test]
    fn an_empty_body_is_not_an_answer() {
        assert!(not_an_api_reply(200, "application/json", Some(b"")).is_some());
        assert!(not_an_api_reply(200, "application/json", Some(b"   \n")).is_some());
    }

    #[test]
    fn json_that_is_declared_json_but_is_not_json_is_refused() {
        let why = not_an_api_reply(200, "application/json", Some(b"upstream is busy, try later")).unwrap();
        assert!(why.contains("不是 JSON"), "{why}");
        // 认得出是什么内容，用户才知道该找谁。
        assert!(why.contains("upstream is busy"), "{why}");
    }

    #[test]
    fn a_real_reply_is_never_refused() {
        // 误判成 502 会让一个本来正常的流被丢掉 —— 比认不出来更糟。
        for (ct, body) in [
            ("application/json", &b"{\"id\":\"m\",\"choices\":[]}"[..]),
            ("text/event-stream", b"event: message_start\ndata: {}\n\n"),
            ("application/json; charset=utf-8", b"[{\"type\":\"x\"}]"),
            // XML 网关以 `<` 开头是正常的。
            ("text/xml", b"<?xml version=\"1.0\"?><r/>"),
            // 没有 Content-Type 但内容是 JSON：不要凭猜拒掉。
            ("", b"{\"id\":\"m\"}"),
        ] {
            assert!(
                not_an_api_reply(200, ct, Some(body)).is_none(),
                "正常的回复不该被拒: {ct}"
            );
        }
    }

    #[test]
    fn a_204_is_an_answer_of_its_own() {
        // 204 是明确的"没有内容"，不是坏掉的回复。
        assert!(not_an_api_reply(204, "text/html", Some(b"<html></html>")).is_none());
        assert!(not_an_api_reply(204, "application/json", Some(b"")).is_none());
    }

    #[test]
    fn only_2xx_is_judged() {
        // 非 2xx 由别的路径处理（错误分类、超长判定），不在这里重复判。
        for status in [400, 404, 429, 500, 502] {
            assert!(
                not_an_api_reply(status, "text/html", Some(b"<html></html>")).is_none(),
                "{status} 不该走这条路"
            );
        }
    }

    #[test]
    fn a_stream_header_alone_can_condemn_a_page() {
        // 流式那条路上正文还没读（也不该为了判断而把整个流缓冲下来），
        // 但 Content-Type 已经足够认出"这是一页网页"。
        assert!(not_an_api_reply(200, "text/html", None).is_some());
        // 反过来，光凭响应头看不出问题的（声明是 json），就不该在流上乱判。
        assert!(not_an_api_reply(200, "application/json", None).is_none());
        assert!(not_an_api_reply(200, "text/event-stream", None).is_none());
    }

    #[test]
    fn leading_whitespace_and_a_bom_do_not_hide_a_page() {
        let page = b"\xEF\xBB\xBF\n  <html><head><title>Attention Required</title></head></html>";
        let why = not_an_api_reply(200, "application/json", Some(page)).expect("必须认出来");
        assert!(why.contains("Attention Required"), "{why}");
    }
}
