//! 把请求归到「哪个对话的哪条线」上。
//!
//! # 为什么靠客户端给的会话 ID，而不是比对对话历史
//!
//! 上下文压缩会把 `messages` 整个改写（几百条变成一条摘要），任何「新请求是不是
//! 上一条的前缀」「首条消息的哈希」之类的推断在压缩点都会断。而会话 ID 是客户端
//! 自己维护的状态，压缩不动它：
//!
//! - Claude Code：请求头 `x-claude-code-session-id`（body 的 `metadata.user_id`
//!   里还有一份 JSON 编码的 `session_id`，头缺失时兜底用）
//! - Codex：请求头 `session-id` / `thread-id`（与 body 的 `prompt_cache_key` 同值）
//!
//! 拿不到 ID 的请求就是没有会话 —— **不猜**。猜错了会把两个对话混成一条，
//! 比"这条没归属"更误导人。
//!
//! # 同一个会话里不只一条线
//!
//! 实测一个 Claude Code 会话里除了主线，还有起标题、stop hook 求值这类**旁路**
//! 请求，子代理也共用同一个会话 ID。它们混在一起，主线的消息数就会忽高忽低，
//! 压缩点也就认不出来了。所以再按「线」细分：
//!
//! - 线的身份 = **system 提示**的指纹。同一条线每轮都发同一份 system（只有对话
//!   历史在长），换一个子代理或旁路任务它就会变。工具集合**不算** —— 见 [`thread_key`]。
//! - `count_tokens` 这类非对话请求也带会话 ID，但它们拿着各种工具子集来估算，
//!   不是任何一条线上的一轮。它们记在会话里，不参与分线，见 [`is_conversation_path`]。
//! - 主线 / 子代理 / 旁路的**分类**在查询时按整个会话来定（谁的请求最多谁是主线），
//!   单条请求自己说不清 —— 见 [`classify_threads`]。
//!
//! system 里有**每轮都会变的片段**（Claude Code 第一段是带版本号与随机串的计费头），
//! 指纹只取稳定的部分，见 [`system_fingerprint`]。

use std::collections::HashMap;

use http::HeaderMap;
use serde::Serialize;
use serde_json::Value;

/// 从一次请求里认出来的归属信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionTag {
    /// 客户端给的会话 ID。没有就是 `None`，不编。
    pub session_id: Option<String>,
    /// 线的指纹。只在有会话时才有意义。
    pub thread_key: Option<String>,
    /// 这次请求里带的对话轮数（`messages` / `input` 的条数）。
    /// 同一条线里它突然变小，就是一次上下文压缩。
    pub message_count: Option<i64>,
    /// 带没带工具。分主线 / 旁路要用（主 agent 不可能不带工具），见 [`classify_threads`]。
    pub has_tools: bool,
    /// 给这个对话起的标题：用户第一条消息里的第一句话。取不到就是 `None`。
    /// 见 [`first_user_line`] —— 直接取第一段文本拿到的是客户端的提醒块，不能那么取。
    pub title: Option<String>,
}

/// 按请求头优先、body 兜底的顺序找会话 ID。
///
/// 头在前：它是客户端专门为此发的，body 里那份是给上游计费用的副产品，格式更可能变。
pub fn session_id(headers: &HeaderMap, body: &Value) -> Option<String> {
    const HEADERS: &[&str] = &["x-claude-code-session-id", "session-id", "thread-id"];
    for name in HEADERS {
        if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }

    // Claude Code 把 `{"device_id":…,"session_id":…}` 编成字符串塞进 `metadata.user_id`。
    if let Some(uid) = body.pointer("/metadata/user_id").and_then(Value::as_str) {
        if let Ok(inner) = serde_json::from_str::<Value>(uid) {
            if let Some(s) = inner.get("session_id").and_then(Value::as_str) {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }

    // Codex 的 `prompt_cache_key` 与会话 ID 同值（实测）。只在 Responses 形状里出现。
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 是不是对话本身的一轮。只有这类请求才分线、才数轮次。
///
/// 实测 Claude Code 会对 `/v1/messages/count_tokens` 发出好几种工具子集的估算
/// 请求，若把它们当线，一个会话里会凭空多出四五个"子代理"。
pub fn is_conversation_path(path: &str) -> bool {
    crate::gateway::router::chat_protocol(path).is_some()
}

/// 认出这次请求的归属。`body` 解不成 JSON 时只能拿头里的 ID。
pub fn tag(headers: &HeaderMap, path: &str, body: &[u8]) -> SessionTag {
    let json: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let session_id = session_id(headers, &json);
    if session_id.is_none() {
        return SessionTag::default();
    }
    if !is_conversation_path(path) {
        // 归到会话里（链路上看得到），但不落在任何一条线上。
        return SessionTag { session_id, ..Default::default() };
    }

    let message_count = json
        .get("messages")
        .or_else(|| json.get("input"))
        .and_then(Value::as_array)
        .map(|a| a.len() as i64);

    let has_tools = json
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty());

    SessionTag {
        session_id,
        thread_key: Some(thread_key(&json)),
        message_count,
        has_tools,
        title: first_user_line(&json),
    }
}

/// 标题的长度上限（**字符**，不是字节）。
///
/// 列表里那一行显示不到这么多，留着是为了悬停时能看全；再长也没意义 ——
/// 用户的第一句话常有整段粘进来的。
const TITLE_MAX_CHARS: usize = 80;

/// 这个对话的标题：用户第一条消息里的第一句话。
///
/// **不能直接取第一段文本。** 实测（Claude Code）用户消息的第一个块是
/// `<system-reminder>`：CLAUDE.md、git 状态、署名要求各一段，真问题排在它们后面，
/// 中间还可能夹一张图。取"第一段"得到的是这些噪声。
///
/// 另外，会话里**最早的一条**往往是客户端的**起标题请求**（`message_count=1`、
/// `has_tools=0`），整条消息是 `<session>真问题</session>` 加一句英文指令。
/// 那种请求直接放弃：标签删掉后剩下的半句会变成一条谁都不认识的假标题。
///
/// 只看**第一条** user 消息：往后翻就是对话内容了，不是开场白。
pub fn first_user_line(body: &Value) -> Option<String> {
    let messages = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)?;

    for m in messages {
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let blocks = user_texts(m.get("content"));
        // 起标题请求的判据在**清洗之前**：那个 `<session>` 块正是要被删掉的东西。
        if blocks.iter().any(|t| t.trim_start().starts_with("<session>")) {
            return None;
        }
        return blocks.iter().find_map(|t| first_meaningful_line(t));
    }
    None
}

/// 一条 user 消息里的纯文本块。
///
/// Anthropic 的块是 `{"type":"text","text":…}`，Responses 的 `input` 是
/// `{"type":"input_text","text":…}` —— 只认 `text` 字段，两种形状就都覆盖到了。
/// 图片、工具结果之类没有 `text` 的块直接跳过。
fn user_texts(content: Option<&Value>) -> Vec<&str> {
    match content {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// 清洗一段用户文本，取第一句像「人说的话」的。
fn first_meaningful_line(text: &str) -> Option<String> {
    let cleaned = strip_tag_blocks(text);
    let line = cleaned
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("[Image:"))?;

    if line.chars().count() <= TITLE_MAX_CHARS {
        return Some(line.to_string());
    }
    let mut out: String = line.chars().take(TITLE_MAX_CHARS).collect();
    out.push('…');
    Some(out)
}

/// 删掉 `<tag …>…</tag>` 整块。
///
/// 这些块不套娃，所以「找到开标签的名字 → 找同名的闭标签」就够了。配不成对的
/// `<` 原样留着：用户正文明明白白写着 `<3`，不能因为看着像标签就把它咬掉。
fn strip_tag_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let close = format!("</{name}>");
        match after.find(&close).filter(|_| !name.is_empty()) {
            Some(at) => {
                out.push_str(&rest[..start]);
                rest = &after[at + close.len()..];
            }
            None => {
                out.push_str(&rest[..start + 1]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 线的指纹：system 稳定部分的短哈希。
///
/// **不含工具集合**。实测主线会在对话中途多出两个工具（MCP 连上了、技能加载了），
/// 而 system 没变 —— 带上工具的话，一条主线会被劈成"主线 + 一个只有 1 次请求的
/// 子代理"。真正的子代理有自己的 system 提示，靠它就分得开。
fn thread_key(body: &Value) -> String {
    let mut h = Fnv::new();
    h.write(system_fingerprint(body).as_bytes());
    format!("{:016x}", h.finish())
}

/// system 提示里**稳定**的那部分。
///
/// - Anthropic：`system` 是块数组时，跳过 `x-anthropic-billing-header: cc_version=…`
///   这种每次都带版本与随机串的计费头；其余**每一块**都取开头一段拼起来 —— 只取
///   第一块不够：Agent SDK 发的旁路任务（起标题、hook 求值）第一块都是同一句
///   "You are a Claude agent, built on…"，区别在第二块。每块只取开头，是因为块尾
///   常拼着环境信息（当前时间、git 状态），会随轮次变。
/// - Responses：`instructions`；Chat：首条 `role=system` 的消息。
///
/// 每块取前 160 字符：足够区分"主 agent / 起标题 / hook 求值 / 某个子代理"，
/// 又不会被尾部的动态内容带偏。
fn system_fingerprint(body: &Value) -> String {
    const HEAD: usize = 160;
    let head = |s: &str| s.chars().take(HEAD).collect::<String>();

    match body.get("system") {
        Some(Value::String(s)) => return head(s),
        Some(Value::Array(blocks)) => {
            return blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.starts_with("x-anthropic-billing-header"))
                .map(head)
                .collect::<Vec<_>>()
                .join("\u{1}");
        }
        _ => {}
    }
    if let Some(s) = body.get("instructions").and_then(Value::as_str) {
        return head(s);
    }
    body.get("messages")
        .and_then(Value::as_array)
        .and_then(|m| m.first())
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .map(head)
        .unwrap_or_default()
}

/// FNV-1a。只用来做分组指纹，不需要抗碰撞 —— 换成 sha 只是多拉一个依赖。
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// 线的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    /// 用户在对话的那条线。
    Main,
    /// 带工具、但不是主线：子代理。
    Agent,
    /// 不带工具的一次性任务：起标题、hook 求值、摘要。
    Side,
}

/// 给会话里的每条线分类所需的统计。
#[derive(Debug, Clone)]
pub struct ThreadStat {
    pub thread_key: String,
    pub requests: i64,
    /// 这条线里有没有带工具的请求。
    pub has_tools: bool,
}

/// 一个会话里各条线的类别。
///
/// 判据只能在整个会话的尺度上给：**带工具的线里请求最多的是主线**。用户的
/// 那条线每轮都跑，子代理只在被派活时跑几轮，旁路任务更是一两次。
/// 不带工具的线一律是旁路 —— 主线 agent 不可能不带工具。
///
/// 会话里没有任何带工具的线时（纯聊天客户端），请求最多的那条就是主线。
pub fn classify_threads(stats: &[ThreadStat]) -> HashMap<String, ThreadKind> {
    let main = stats
        .iter()
        .filter(|s| s.has_tools)
        .max_by_key(|s| s.requests)
        .or_else(|| stats.iter().max_by_key(|s| s.requests))
        .map(|s| s.thread_key.clone());

    stats
        .iter()
        .map(|s| {
            let kind = if Some(&s.thread_key) == main.as_ref() {
                ThreadKind::Main
            } else if s.has_tools {
                ThreadKind::Agent
            } else {
                ThreadKind::Side
            };
            (s.thread_key.clone(), kind)
        })
        .collect()
}

/// 同一条线里，消息数从 `prev` 变成 `cur` 算不算一次上下文压缩。
///
/// 正常的一轮只会让消息数**增长**（追加用户消息、助手回复、工具结果）。减少
/// 只有两种来源：压缩，或客户端回滚/编辑了某条消息。后者通常只少几条，所以
/// 要求「降到不足一半」并且「至少少了 4 条」—— 两个条件都是为了不把一次
/// 编辑重发当成压缩。
pub fn is_compaction(prev: i64, cur: i64) -> bool {
    cur < prev && prev - cur >= 4 && cur * 2 <= prev
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn claude_code_session_header_is_used() {
        let h = headers(&[("x-claude-code-session-id", "6b75aac2")]);
        let t = tag(&h, "/v1/messages", br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(t.session_id.as_deref(), Some("6b75aac2"));
        assert_eq!(t.message_count, Some(1));
    }

    #[test]
    fn codex_session_header_is_used() {
        let h = headers(&[("session-id", "01a13d80"), ("thread-id", "01a13d80")]);
        let t = tag(&h, "/v1/messages", br#"{"input":[{},{},{}]}"#);
        assert_eq!(t.session_id.as_deref(), Some("01a13d80"));
        assert_eq!(t.message_count, Some(3), "Responses 的轮数在 input 里");
    }

    #[test]
    fn metadata_user_id_is_the_fallback_for_claude_code() {
        // 头被代理剥掉时，body 里那份 JSON 编码的 session_id 还在。
        let body = json!({
            "metadata": { "user_id": "{\"device_id\":\"d\",\"account_uuid\":\"\",\"session_id\":\"s-1\"}" },
            "messages": []
        });
        assert_eq!(session_id(&HeaderMap::new(), &body).as_deref(), Some("s-1"));
    }

    #[test]
    fn prompt_cache_key_is_the_fallback_for_codex() {
        let body = json!({ "prompt_cache_key": "01a13d5e", "input": [] });
        assert_eq!(session_id(&HeaderMap::new(), &body).as_deref(), Some("01a13d5e"));
    }

    #[test]
    fn no_id_means_no_session_rather_than_a_guess() {
        // 猜错会把两个对话混成一条，比"没归属"更误导人。
        let t = tag(&HeaderMap::new(), "/v1/messages", br#"{"messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(t, SessionTag::default());
    }

    #[test]
    fn undecodable_body_still_gets_the_header_session() {
        // 解不出的请求（被我们 400 掉的那种）也要能归到对话里 —— 那正是要排查的。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let t = tag(&h, "/v1/messages", b"{not json");
        assert_eq!(t.session_id.as_deref(), Some("s"));
        assert_eq!(t.message_count, None);
    }

    fn cc_body(system_tail: &str, tools: &[&str], n_msgs: usize) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "system": [
                { "type": "text", "text": format!("x-anthropic-billing-header: cc_version=2.1.{}", n_msgs) },
                { "type": "text", "text": format!("You are an agent.{system_tail}") },
            ],
            "tools": tools.iter().map(|n| json!({ "name": n, "description": format!("动态描述 {n_msgs}") })).collect::<Vec<_>>(),
            "messages": (0..n_msgs).map(|_| json!({"role":"user","content":"x"})).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn same_thread_keeps_its_key_as_the_conversation_grows() {
        // 计费头里的版本号、工具描述里的动态内容、对话历史都在变，线不能因此分裂。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let a = tag(&h, "/v1/messages", &cc_body("", &["Bash", "Read"], 3));
        let b = tag(&h, "/v1/messages", &cc_body("", &["Read", "Bash"], 40));
        assert_eq!(a.thread_key, b.thread_key);
    }

    #[test]
    fn main_thread_survives_tools_being_added_mid_conversation() {
        // 实测：主线中途从 25 个工具变成 27 个（MCP 连上了），system 没变。
        // 不能因此劈出一个只有一次请求的"子代理"。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let before = tag(&h, "/v1/messages", &cc_body("", &["Bash", "Read"], 10));
        let after = tag(&h, "/v1/messages", &cc_body("", &["Bash", "Read", "mcp__ide__diag"], 12));
        assert_eq!(before.thread_key, after.thread_key);
    }

    #[test]
    fn count_tokens_belongs_to_the_session_but_not_to_a_thread() {
        // Claude Code 会拿各种工具子集来估算 token —— 那不是对话的一轮，
        // 当成线会凭空多出一堆"子代理"。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let t = tag(&h, "/v1/messages/count_tokens", &cc_body("", &["Skill"], 1));
        assert_eq!(t.session_id.as_deref(), Some("s"));
        assert_eq!(t.thread_key, None);
        assert_eq!(t.message_count, None, "不参与压缩判定");
    }

    #[test]
    fn compaction_does_not_change_the_thread() {
        // 压缩后消息从几百条变成几条，但工具与 system 不变 —— 仍是同一条线。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let before = tag(&h, "/v1/messages", &cc_body("", &["Bash"], 300));
        let after = tag(&h, "/v1/messages", &cc_body("", &["Bash"], 2));
        assert_eq!(before.session_id, after.session_id);
        assert_eq!(before.thread_key, after.thread_key);
        assert!(is_compaction(300, 2));
    }

    #[test]
    fn side_tasks_and_sub_agents_get_their_own_threads() {
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let main = tag(&h, "/v1/messages", &cc_body("", &["Bash", "Read"], 5));
        let title = tag(&h, "/v1/messages", &serde_json::to_vec(&json!({
            "system": [{ "type": "text", "text": "You are naming a coding session" }],
            "messages": [{"role":"user","content":"x"}],
        })).unwrap());
        let agent = tag(&h, "/v1/messages", &cc_body(" You are a search sub-agent.", &["Grep", "Read"], 2));
        assert_ne!(main.thread_key, title.thread_key);
        assert_ne!(main.thread_key, agent.thread_key);
    }

    #[test]
    fn sdk_side_tasks_sharing_a_preamble_are_still_distinct() {
        // 实测：起标题和 stop hook 求值的第一块 system 一字不差，区别在第二块。
        let h = headers(&[("x-claude-code-session-id", "s")]);
        let body = |task: &str| serde_json::to_vec(&json!({
            "system": [
                { "type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.299" },
                { "type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK." },
                { "type": "text", "text": task },
            ],
            "messages": [{"role":"user","content":"x"}],
        })).unwrap();
        let title = tag(&h, "/v1/messages", &body("You are naming a coding session"));
        let hook = tag(&h, "/v1/messages", &body("You are evaluating a stop-condition hook"));
        assert_ne!(title.thread_key, hook.thread_key);
    }

    #[test]
    fn the_busiest_tool_thread_is_main() {
        let s = |k: &str, n, tools| ThreadStat { thread_key: k.into(), requests: n, has_tools: tools };
        let kinds = classify_threads(&[s("title", 200, false), s("main", 90, true), s("agent", 6, true)]);
        // 起标题的请求再多也不是主线：主 agent 不可能不带工具。
        assert_eq!(kinds["main"], ThreadKind::Main);
        assert_eq!(kinds["agent"], ThreadKind::Agent);
        assert_eq!(kinds["title"], ThreadKind::Side);
    }

    #[test]
    fn a_tool_less_session_still_has_a_main_thread() {
        let s = |k: &str, n| ThreadStat { thread_key: k.into(), requests: n, has_tools: false };
        let kinds = classify_threads(&[s("chat", 10), s("title", 1)]);
        assert_eq!(kinds["chat"], ThreadKind::Main);
        assert_eq!(kinds["title"], ThreadKind::Side);
    }

    #[test]
    fn small_drops_are_edits_not_compactions() {
        assert!(!is_compaction(40, 38), "编辑重发只少几条");
        assert!(!is_compaction(5, 2), "少不到 4 条不算，哪怕降过了一半");
        assert!(is_compaction(6, 2), "正好少 4 条且不足一半：算");
        assert!(!is_compaction(10, 12), "增长不是压缩");
        assert!(is_compaction(10, 1));
        assert!(is_compaction(901, 3));
    }

    /// 实测形状：提醒块在前、图在中间、真问题在最后。
    fn cc_first_message(question: &str) -> Value {
        json!({
            "session": [{"type":"text","text":"You are an agent."}],
            "tools": [{"name": "Bash"}],
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "<system-reminder>\nContents of ~/CLAUDE.md:\n\n# CLAUDE.md\n\n本项目…\n</system-reminder>" },
                    { "type": "text", "text": "<system-reminder>\n# gitStatus\nRecent commits:\nc7e26d4 fix: 上游回 200\n</system-reminder>" },
                    { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "…" } },
                    { "type": "text", "text": question },
                    { "type": "text", "text": "[Image: source: /tmp/1.png]" },
                ],
            }],
        })
    }

    #[test]
    fn title_skips_the_reminder_blocks_in_front_of_the_question() {
        // 取第一段文本拿到的是 CLAUDE.md —— 那能当标题就怪了。
        let body = cc_first_message("帮我生成一下上周的ReleaseNote\n");
        assert_eq!(first_user_line(&body).as_deref(), Some("帮我生成一下上周的ReleaseNote"));

        let t = tag(&headers(&[("x-claude-code-session-id", "s")]), "/v1/messages",
                    &serde_json::to_vec(&body).unwrap());
        assert_eq!(t.title.as_deref(), Some("帮我生成一下上周的ReleaseNote"));
    }

    #[test]
    fn a_request_that_names_the_session_gets_no_title() {
        // 会话里最早的那条往往是客户端的起标题请求：真问题被包在 `<session>` 里，
        // 标签删掉后就只剩一句英文指令 —— 当成标题比没有标题更糟。
        let body = json!({
            "messages": [{ "role": "user", "content": [{
                "type": "text",
                "text": "<session>\n帮我生成一下上周的ReleaseNote\n</session>\n\nWrite the title in the predominant language of the session.",
            }]}],
        });
        assert_eq!(first_user_line(&body), None);
    }

    #[test]
    fn a_message_with_only_an_image_has_no_title_and_we_stop_there() {
        // 往后翻就是对话内容了，不是开场白：开场白是图（或者只有提醒块）就是没有标题。
        let body = json!({ "messages": [
            { "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "…" } },
                { "type": "text", "text": "[Image: source: /tmp/1.png]" },
            ]},
            { "role": "user", "content": "第二句" },
        ]});
        assert_eq!(first_user_line(&body), None);
    }

    #[test]
    fn responses_input_shape_yields_a_title_too() {
        let body = json!({ "input": [
            { "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": "<system-reminder>\n# gitStatus\n</system-reminder>" },
                { "type": "input_text", "text": "重构一下路由层" },
            ]},
        ]});
        assert_eq!(first_user_line(&body).as_deref(), Some("重构一下路由层"));
    }

    #[test]
    fn plain_string_content_yields_a_title_too() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi\nthere" }] });
        assert_eq!(first_user_line(&body).as_deref(), Some("hi"), "只取第一句");
        assert_eq!(first_user_line(&json!({ "messages": [{ "role": "assistant", "content": "x" }] })), None);
        assert_eq!(first_user_line(&json!({ "model": "m" })), None);
    }

    #[test]
    fn a_lone_angle_bracket_in_the_question_is_not_a_tag() {
        // 配不成对就原样留着：正文里写 `<3` 很常见，咬掉它才是错的。
        let body = json!({ "messages": [{ "role": "user", "content": "为什么 a < b 时出错？<3" }] });
        assert_eq!(first_user_line(&body).as_deref(), Some("为什么 a < b 时出错？<3"));
    }

    #[test]
    fn a_long_question_is_cut_by_characters_not_bytes() {
        let long = "长".repeat(200);
        let body = json!({ "messages": [{ "role": "user", "content": long }] });
        let title = first_user_line(&body).unwrap();
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1, "80 个字符 + 省略号");
        assert!(title.ends_with('…'));
    }
}
