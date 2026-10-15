//! 对话级模型固定：某个会话里的请求一律改用指定模型。
//!
//! # 在管线里的位置
//!
//! 在全局 / 客户端模型策略**之后**、路由规则链之前（见 `gateway/pipeline.rs`）：
//!
//! ```text
//! 客户端请求的模型 → 模型策略（全局 + 客户端覆盖）→ 会话固定 → 规则链 ModelOverride
//! ```
//!
//! 放在策略之后是因为它是**更具体**的意图 —— 用户点名"这个对话用 X"，不该被一条
//! "所有请求都走 Y"的全局策略盖掉。规则链仍在它之上，那是"例外规则"的既有用法。
//!
//! # 内存里一份
//!
//! 每个请求都要查一次，所以表整份装进 `ArcSwap`，改的时候整体替换。
//! 行数等于用户手动固定过的对话数，量级是几十，全量重载没有成本问题。

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// 固定对哪些请求生效。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinScope {
    /// 只改带工具的请求：主线与子代理。
    ///
    /// 默认是它：起标题、hook 求值这类旁路是客户端特意用小模型跑的，跟着换成
    /// 大模型纯属浪费，而且它们不影响对话本身的质量。
    #[default]
    Tools,
    /// 会话里的对话请求全改，旁路也算。
    All,
}

impl PinScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::All => "all",
        }
    }

    /// 认不出的值当默认值：库里被手改坏的一行不该让整张表加载失败。
    pub fn parse(s: &str) -> Self {
        match s {
            "all" => Self::All,
            _ => Self::Tools,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPin {
    pub session_id: String,
    pub model: String,
    pub scope: PinScope,
    pub created_at: i64,
}

/// 所有固定的内存快照。
#[derive(Default)]
pub struct SessionPins {
    by_session: ArcSwap<HashMap<String, SessionPin>>,
}

impl SessionPins {
    pub fn new(pins: Vec<SessionPin>) -> Self {
        let me = Self::default();
        me.replace(pins);
        me
    }

    /// 整体替换。改完库之后调。
    pub fn replace(&self, pins: Vec<SessionPin>) {
        let map = pins.into_iter().map(|p| (p.session_id.clone(), p)).collect();
        self.by_session.store(Arc::new(map));
    }

    pub fn get(&self, session_id: &str) -> Option<SessionPin> {
        self.by_session.load().get(session_id).cloned()
    }

    /// 这次请求该改用的模型；不改时为 `None`。
    ///
    /// `is_conversation`：是不是对话的一轮。`count_tokens` 这类不是 —— 但它**也要改**：
    /// 客户端拿它估算上下文占多少 token，按 A 计数、按 B 对话，两个数就对不上了
    /// （与 `handle_raw` 里照样过模型策略是同一个理由）。所以它跟着「全部」走，
    /// 也跟着「带工具」走（估算请求带的工具就是主线那套）。
    pub fn model_for(
        &self,
        session_id: Option<&str>,
        has_tools: bool,
        is_conversation: bool,
    ) -> Option<String> {
        let pin = self.by_session.load().get(session_id?)?.clone();
        let applies = match pin.scope {
            PinScope::All => true,
            PinScope::Tools => has_tools || !is_conversation,
        };
        applies.then_some(pin.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(scope: PinScope) -> SessionPins {
        SessionPins::new(vec![SessionPin {
            session_id: "A".into(),
            model: "opus".into(),
            scope,
            created_at: 0,
        }])
    }

    #[test]
    fn only_the_pinned_session_is_rewritten() {
        let p = pins(PinScope::All);
        assert_eq!(p.model_for(Some("A"), true, true).as_deref(), Some("opus"));
        assert_eq!(p.model_for(Some("B"), true, true), None, "另一个对话不受影响");
        assert_eq!(p.model_for(None, true, true), None, "没有会话 ID 的请求不受影响");
    }

    #[test]
    fn default_scope_leaves_side_tasks_on_their_own_model() {
        // 起标题是客户端特意用小模型跑的，跟着换成大模型纯属浪费。
        let p = pins(PinScope::Tools);
        assert_eq!(p.model_for(Some("A"), true, true).as_deref(), Some("opus"), "主线 / 子代理");
        assert_eq!(p.model_for(Some("A"), false, true), None, "旁路");
    }

    #[test]
    fn all_scope_includes_side_tasks() {
        let p = pins(PinScope::All);
        assert_eq!(p.model_for(Some("A"), false, true).as_deref(), Some("opus"));
    }

    #[test]
    fn token_counting_follows_the_pin_under_either_scope() {
        // 按 A 计数、按 B 对话，客户端的上下文估算就是错的。
        assert_eq!(pins(PinScope::Tools).model_for(Some("A"), false, false).as_deref(), Some("opus"));
        assert_eq!(pins(PinScope::All).model_for(Some("A"), false, false).as_deref(), Some("opus"));
    }

    #[test]
    fn replacing_drops_removed_pins() {
        let p = pins(PinScope::All);
        p.replace(vec![]);
        assert_eq!(p.model_for(Some("A"), true, true), None);
    }

    #[test]
    fn unknown_scope_falls_back_to_the_default() {
        assert_eq!(PinScope::parse("all"), PinScope::All);
        assert_eq!(PinScope::parse("tools"), PinScope::Tools);
        assert_eq!(PinScope::parse("garbage"), PinScope::Tools);
    }
}
