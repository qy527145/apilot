//! 缓存键的计算。
//!
//! 键必须**只**取决于会影响输出的因素，否则会返回错误的缓存结果。
//! 反之，把无关因素算进去只会降低命中率，不会出错 —— 所以取舍时一律偏保守。

use sha2::{Digest, Sha256};

use crate::protocol::dto::{Protocol, UnifiedRequest};

/// 计算请求的缓存键（sha256 十六进制）。
///
/// 参与计算：协议、模型、system、消息、工具定义、采样参数、stop。
/// 刻意**不**参与：
/// - `stream`：流式与非流式应当命中同一份结果（编码方式由客户端决定）；
/// - `extra` 里的其余透传字段：无法确知它们是否影响输出，保守起见**算进去**
///   反而更安全？—— 不，这里选择排除元数据类字段，因为它们大多不影响生成。
///
/// 需要说明的是：排除字段是有风险的。因此 [`super::policy::is_cacheable`] 只会
/// 在 `temperature == 0` 时才放行，把"看起来相同但结果不同"的风险压到最低。
///
/// ⚠️ **`req.model` 必须是「生效模型」**（全局模型策略与路由规则改写之后的那个），
/// 不是客户端请求的名字。`pipeline::handle` 在路由之后会把它们统一，
/// 调用方不要自己拿原始请求去算键 —— 否则换上便宜模型后会直接命中上一个模型
/// 生成的答案，那是正确性事故而不只是账目问题。
pub fn cache_key(protocol: Protocol, req: &UnifiedRequest) -> String {
    let mut hasher = Sha256::new();

    hasher.update(protocol.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(req.model.as_bytes());
    hasher.update([0u8]);

    // system 块
    for block in &req.system {
        hasher.update(block_text(block).as_bytes());
        hasher.update([0u8]);
    }
    hasher.update([1u8]);

    // 消息：角色 + 内容，顺序敏感
    for msg in &req.messages {
        hasher.update(msg.role.as_str().as_bytes());
        hasher.update([0u8]);
        for block in &msg.content {
            hasher.update(block_text(block).as_bytes());
            hasher.update([0u8]);
        }
        hasher.update([1u8]);
    }
    hasher.update([2u8]);

    // 工具定义：名称 + schema，顺序敏感（顺序会影响模型行为）
    for tool in &req.tools {
        hasher.update(tool.name.as_bytes());
        hasher.update([0u8]);
        hasher.update(tool.description.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
        hasher.update(tool.input_schema.to_string().as_bytes());
        hasher.update([0u8]);
    }
    hasher.update([3u8]);

    // 采样参数：这些直接决定输出，必须计入
    if let Some(t) = req.temperature {
        hasher.update(t.to_bits().to_le_bytes());
    }
    if let Some(p) = req.top_p {
        hasher.update(p.to_bits().to_le_bytes());
    }
    if let Some(m) = req.max_tokens {
        hasher.update(m.to_le_bytes());
    }
    for s in &req.stop {
        hasher.update(s.as_bytes());
        hasher.update([0u8]);
    }
    if let Some(tc) = &req.tool_choice {
        hasher.update(format!("{tc:?}").as_bytes());
    }

    // 思考配置会显著改变输出（预算越大推理越长），必须计入。
    if let Some(r) = &req.reasoning {
        hasher.update([r.enabled as u8]);
        if let Some(b) = r.budget_tokens {
            hasher.update(b.to_le_bytes());
        }
        if let Some(e) = &r.effort {
            hasher.update(e.as_bytes());
        }
        hasher.update([0u8]);
    }

    format!("{:x}", hasher.finalize())
}

/// 内容块的稳定文本表示。
fn block_text(block: &crate::protocol::dto::ContentBlock) -> String {
    use crate::protocol::dto::ContentBlock;
    match block {
        ContentBlock::Text { text } => text.clone(),
        ContentBlock::Thinking { text, .. } => format!("<thinking>{text}"),
        ContentBlock::RedactedThinking { data } => format!("<redacted>{data}"),
        ContentBlock::Image { media_type, data } => format!("<image:{media_type}>{data}"),
        ContentBlock::ToolUse { id, name, input } => {
            format!("<tool_use:{id}:{name}>{input}")
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let inner: String = content.iter().map(block_text).collect();
            format!("<tool_result:{tool_use_id}:{is_error}>{inner}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{
        ContentBlock, ReasoningConfig, Role, ToolDef, UnifiedMessage, UnifiedRequest,
    };
    use serde_json::json;

    fn base() -> UnifiedRequest {
        let mut r = UnifiedRequest::new("claude-sonnet-5");
        r.messages = vec![UnifiedMessage::user_text("你好")];
        r
    }

    #[test]
    fn key_is_stable_for_identical_requests() {
        let a = cache_key(Protocol::AnthropicMessages, &base());
        let b = cache_key(Protocol::AnthropicMessages, &base());
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha256 十六进制");
    }

    #[test]
    fn different_protocol_yields_different_key() {
        let a = cache_key(Protocol::AnthropicMessages, &base());
        let b = cache_key(Protocol::OpenAiChat, &base());
        assert_ne!(a, b, "不同协议的同一请求不该共享缓存");
    }

    #[test]
    fn message_content_changes_key() {
        let mut r = base();
        r.messages = vec![UnifiedMessage::user_text("你好呀")];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );
    }

    #[test]
    fn message_order_matters() {
        let mut a = base();
        a.messages = vec![
            UnifiedMessage::user_text("a"),
            UnifiedMessage::user_text("b"),
        ];
        let mut b = base();
        b.messages = vec![
            UnifiedMessage::user_text("b"),
            UnifiedMessage::user_text("a"),
        ];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &a),
            cache_key(Protocol::AnthropicMessages, &b)
        );
    }

    #[test]
    fn stream_flag_does_not_change_key() {
        let mut streaming = base();
        streaming.stream = true;
        assert_eq!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &streaming),
            "同一问题流式与非流式应共享缓存，输出编码由客户端决定"
        );
    }

    #[test]
    fn temperature_changes_key() {
        let mut cold = base();
        cold.temperature = Some(0.0);
        let mut warm = base();
        warm.temperature = Some(0.7);
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &cold),
            cache_key(Protocol::AnthropicMessages, &warm)
        );
    }

    #[test]
    fn max_tokens_changes_key() {
        let mut short = base();
        short.max_tokens = Some(100);
        let mut long = base();
        long.max_tokens = Some(4000);
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &short),
            cache_key(Protocol::AnthropicMessages, &long),
            "max_tokens 会影响输出，必须计入"
        );
    }

    #[test]
    fn tools_change_key() {
        let mut with_tool = base();
        with_tool.tools = vec![ToolDef {
            name: "search".into(),
            description: Some("搜索".into()),
            input_schema: json!({"type": "object"}),
        }];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &with_tool)
        );
    }

    #[test]
    fn system_prompt_changes_key() {
        let mut r = base();
        r.system = vec![ContentBlock::text("你是助手")];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );
    }

    #[test]
    fn model_changes_key() {
        // 这条守的是「换模型不能命中旧模型的缓存」。全局模型替换开启后，
        // 客户端请求的模型名可能完全不变，只有生效模型变了 ——
        // 若键跟着请求名走，用户换到便宜模型后会拿到上一个模型生成的答案。
        let mut r = base();
        r.model = "claude-opus-5".into();
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );
    }

    #[test]
    fn reasoning_config_changes_key() {
        // 思考预算会显著影响输出
        let mut r = base();
        r.reasoning = Some(ReasoningConfig {
            enabled: true,
            budget_tokens: Some(4000),
            effort: None,
        });
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );

        let mut other = base();
        other.reasoning = Some(ReasoningConfig {
            enabled: true,
            budget_tokens: Some(8000),
            effort: None,
        });
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &r),
            cache_key(Protocol::AnthropicMessages, &other)
        );
    }

    #[test]
    fn tool_use_and_result_are_distinguished() {
        let mut a = base();
        a.messages.push(UnifiedMessage::new(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "f".into(),
                input: json!({"x": 1}),
            }],
        ));
        let mut b = base();
        b.messages.push(UnifiedMessage::new(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "f".into(),
                input: json!({"x": 2}),
            }],
        ));
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &a),
            cache_key(Protocol::AnthropicMessages, &b),
            "工具入参不同，键必须不同"
        );
    }

    #[test]
    fn stop_sequences_change_key() {
        let mut r = base();
        r.stop = vec!["END".into()];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );
    }

    #[test]
    fn image_content_changes_key() {
        let mut r = base();
        r.messages = vec![UnifiedMessage::new(
            Role::User,
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
        )];
        assert_ne!(
            cache_key(Protocol::AnthropicMessages, &base()),
            cache_key(Protocol::AnthropicMessages, &r)
        );
    }
}
