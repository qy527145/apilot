//! 主动能力探测：往目标渠道真发一次请求，看它怎么回。
//!
//! 和目录（`catalog`）的分工：目录零成本，但只反映**模型宣称**的能力；这里花
//! 真金白银发请求，反映**这条渠道实际**的行为。中转商把 tools 剥掉、自建模型
//! 不在任何目录里 —— 这些只有实测能回答。
//!
//! ## 三种能力的判法刻意不一样
//!
//! 不是偷懒，是各协议的实际行为不同：
//!
//! - **工具 / 图片**：不支持时上游会**返回 4xx**（OpenAI、Anthropic 都是）。
//!   所以"请求被接受"就是"支持"的充分证据。
//! - **思考**：OpenAI 系对不认识的 `reasoning_effort` 是**静默忽略**，不报错。
//!   请求被接受说明不了任何事，只能看**回包里有没有思考块**。Anthropic 倒是会
//!   返 400，所以那边仍然能得出确定结论。
//!
//! 判不出来就记 `Inconclusive` 而不是硬猜 —— 见 [`CapabilityVerdict`] 的说明。

use std::sync::Arc;

use serde_json::json;

use crate::protocol::dto::{
    ContentBlock, ReasoningConfig, ToolChoice, ToolDef, UnifiedMessage, UnifiedRequest,
};
use crate::shell::AppShell;
use crate::storage::capabilities::{Capability, CapabilityVerdict};
use crate::storage::models::Provider;
use crate::upstream::oneshot::{self, OneshotOutcome};

/// 单张探测图片：16×16 纯色 PNG 的 base64。
///
/// 选 16×16 而不是 1×1：有些服务商对图片有最小尺寸限制，1×1 会被拒，而那被拒
/// 的理由跟"不支持图片"完全无关 —— 会得出错误的"不支持"结论。纯色是因为
/// 压缩后只有 108 字节，省得每次探测都传一大坨 base64。
const PROBE_IMAGE_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAFklEQVR42mM4YWNDEmIY1TCqYfhqAACrxkAQIHrJ+wAAAABJRU5ErkJggg==";

/// 探测结果。`evidence` 是**观察到的现象**，不是结论的同义反复 ——
/// 排查"凭什么叫它不支持"时全靠它。
#[derive(Debug, Clone)]
pub struct Probed {
    pub capability: Capability,
    pub verdict: CapabilityVerdict,
    pub evidence: String,
}

/// 探测某渠道某模型的一项能力。
///
/// `model` 必须是**上游认得的名字**（调用方先做 `upstream_model` 映射）。
pub async fn probe_capability(
    shell: &Arc<AppShell>,
    provider: &Provider,
    model: &str,
    capability: Capability,
) -> Probed {
    let req = build_request(model, capability);
    let outcome = oneshot::send(shell, provider, &req).await;
    judge(capability, &outcome)
}

/// 按能力拼出探测请求。
fn build_request(model: &str, capability: Capability) -> UnifiedRequest {
    let mut req = UnifiedRequest::new(model);
    // 探测要快、要省：给个很小的上限，别让模型写一篇作文回来。
    req.max_tokens = Some(256);

    match capability {
        Capability::Tools => {
            req.tools = vec![ToolDef {
                name: "get_time".into(),
                namespace: None,
                description: Some("查询当前时间".into()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"timezone": {"type": "string"}},
                    "required": []
                }),
            }];
            // 用 Required 而不是 Auto：Auto 下模型完全可以不调工具直接作答，
            // 那样就分不清"不支持工具"和"这次没想用"。强制调用能把两者分开。
            req.tool_choice = Some(ToolChoice::Required);
            req.messages = vec![UnifiedMessage::user_text(
                "请调用 get_time 工具查询当前时间。",
            )];
        }
        Capability::Vision => {
            req.messages = vec![UnifiedMessage::new(
                crate::protocol::dto::Role::User,
                vec![
                    ContentBlock::Image {
                        media_type: "image/png".into(),
                        data: PROBE_IMAGE_B64.into(),
                    },
                    ContentBlock::text("这张图是什么颜色？只答颜色。"),
                ],
            )];
        }
        Capability::Reasoning => {
            req.reasoning = Some(ReasoningConfig {
                enabled: true,
                budget_tokens: Some(1024),
                effort: Some("low".into()),
            });
            req.messages = vec![UnifiedMessage::user_text(
                "一个笼子里有鸡和兔，共 8 个头、26 只脚。鸡和兔各几只？只答数字。",
            )];
        }
    }

    req
}

/// 上游明确说"我不支持这个东西"的信号。
///
/// 只在 4xx 时才有意义 —— 5xx 是上游自己出问题、超时是网络问题，都说明不了
/// 能力。少了这层区分，一次上游抽风就会把能力记成"不支持"。
fn rejection_hint(text: &str, capability: Capability) -> bool {
    let t = text.to_lowercase();
    let words: &[&str] = match capability {
        Capability::Tools => &["tool", "function", "tool_choice"],
        Capability::Vision => &["image", "vision", "multimodal", "media"],
        Capability::Reasoning => &["thinking", "reasoning", "reasoning_effort"],
    };
    words.iter().any(|w| t.contains(w))
}

fn judge(capability: Capability, outcome: &OneshotOutcome) -> Probed {
    let evidence = |s: String| Probed {
        capability,
        verdict: CapabilityVerdict::Supported,
        evidence: s,
    };
    let inconclusive = |s: String| Probed {
        capability,
        verdict: CapabilityVerdict::Inconclusive,
        evidence: s,
    };
    let unsupported = |s: String| Probed {
        capability,
        verdict: CapabilityVerdict::Unsupported,
        evidence: s,
    };

    // 没拿到 HTTP 响应（连不上 / 超时）—— 与能力无关。
    let Some(status) = outcome.status else {
        return inconclusive(format!(
            "请求没能送达上游（{}），这次说明不了能力问题",
            outcome.error.as_deref().unwrap_or("未知原因")
        ));
    };

    if !outcome.ok {
        // 4xx 的报错文案里出现了这个能力的字眼 → 才是真的"不支持"。
        // 其它错误（余额不足、密钥错、限流）一律存疑。
        let msg = outcome.error.clone().unwrap_or_default();
        if (400..500).contains(&status) && rejection_hint(&msg, capability) {
            return unsupported(format!("上游返回 {status}：{msg}"));
        }
        return inconclusive(format!("上游返回 {status}：{msg}（与能力无关的失败）"));
    }

    // 到这里请求被接受了。对工具和图片来说这就够了 —— 不支持时上游会返 4xx。
    match capability {
        Capability::Tools => {
            let called = outcome
                .decoded
                .as_ref()
                .map(|r| {
                    r.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                })
                .unwrap_or(false);
            if called {
                evidence("上游接受了工具定义，并且这次确实发起了工具调用".into())
            } else {
                // 接受了但没调用：对 Apilot 来说"请求会被接受"已经成立，
                // 但如实记下没观察到调用，免得用户以为这里验证过模型的调用质量。
                evidence(format!(
                    "上游接受了带 tools 的请求（HTTP {status}），但这次没有发起调用 —— \
                     转发不会失败，模型用不用是另一回事"
                ))
            }
        }
        Capability::Vision => {
            let text = if outcome.text.trim().is_empty() {
                "（没解出文本）".to_string()
            } else {
                outcome.text.chars().take(40).collect()
            };
            evidence(format!("上游接受了图片输入并作出回答：{text}"))
        }
        Capability::Reasoning => {
            // 只有回包里真出现思考块才算数 —— 见模块头的说明。
            let thought = outcome
                .decoded
                .as_ref()
                .map(|r| {
                    r.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::Thinking { .. }))
                })
                .unwrap_or(false);
            if thought {
                evidence("上游返回了思考内容".into())
            } else {
                inconclusive(format!(
                    "上游接受了思考参数（HTTP {status}）但没返回思考内容 —— \
                     OpenAI 系会静默忽略它不认识的思考参数，所以这不能算支持"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dto::{Protocol, UnifiedResponse};

    fn outcome(status: Option<u16>, ok: bool, content: Vec<ContentBlock>) -> OneshotOutcome {
        OneshotOutcome {
            ok,
            status,
            latency_ms: 10,
            wire: Protocol::AnthropicMessages,
            url: "https://x.example.com/v1/messages".into(),
            preview: String::new(),
            text: content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join(""),
            usage: None,
            error: if ok {
                None
            } else {
                Some("上游返回错误".into())
            },
            decoded: Some(UnifiedResponse {
                id: "r".into(),
                model: "m".into(),
                content,
                finish_reason: crate::protocol::dto::FinishReason::Stop,
            }),
        }
    }

    #[test]
    fn a_transport_failure_is_inconclusive_not_unsupported() {
        // 连不上上游跟模型能力毫无关系。记成"不支持"会让用户白删一条好渠道。
        let mut o = outcome(None, false, vec![]);
        o.error = Some("连接超时".into());
        let r = judge(Capability::Tools, &o);
        assert_eq!(r.verdict, CapabilityVerdict::Inconclusive);
    }

    #[test]
    fn a_generic_4xx_is_inconclusive() {
        // 余额不足、密钥错也是 4xx，不能算成"不支持工具"。
        let mut o = outcome(Some(403), false, vec![]);
        o.error = Some("insufficient balance".into());
        assert_eq!(
            judge(Capability::Tools, &o).verdict,
            CapabilityVerdict::Inconclusive
        );
    }

    #[test]
    fn a_4xx_that_names_the_capability_is_unsupported() {
        let mut o = outcome(Some(400), false, vec![]);
        o.error = Some("tools is not supported for this model".into());
        assert_eq!(
            judge(Capability::Tools, &o).verdict,
            CapabilityVerdict::Unsupported
        );
    }

    #[test]
    fn the_rejection_hint_is_capability_specific() {
        // "image" 出现在错误里，不该因此判定"工具不支持"。
        let mut o = outcome(Some(400), false, vec![]);
        o.error = Some("image input is not supported".into());
        assert_eq!(
            judge(Capability::Tools, &o).verdict,
            CapabilityVerdict::Inconclusive
        );
        assert_eq!(
            judge(Capability::Vision, &o).verdict,
            CapabilityVerdict::Unsupported
        );
    }

    #[test]
    fn an_accepted_tools_request_counts_as_supported() {
        // 不支持工具的上游会返 4xx，所以被接受本身就是证据。
        let o = outcome(Some(200), true, vec![ContentBlock::text("好的")]);
        assert_eq!(
            judge(Capability::Tools, &o).verdict,
            CapabilityVerdict::Supported
        );
    }

    #[test]
    fn an_actual_tool_call_is_called_out_in_the_evidence() {
        let o = outcome(
            Some(200),
            true,
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "get_time".into(),
                input: json!({}),
            }],
        );
        let r = judge(Capability::Tools, &o);
        assert_eq!(r.verdict, CapabilityVerdict::Supported);
        assert!(r.evidence.contains("发起了工具调用"));
    }

    #[test]
    fn accepted_without_a_tool_call_says_so_honestly() {
        let o = outcome(Some(200), true, vec![ContentBlock::text("现在是三点")]);
        let r = judge(Capability::Tools, &o);
        assert_eq!(r.verdict, CapabilityVerdict::Supported);
        assert!(r.evidence.contains("没有发起调用"), "别让用户以为验证过调用质量");
    }

    #[test]
    fn reasoning_needs_actual_thinking_content_to_count() {
        // 这是三种能力里唯一"被接受不等于支持"的：OpenAI 系静默忽略思考参数。
        let accepted = outcome(Some(200), true, vec![ContentBlock::text("鸡 3 只兔 5 只")]);
        assert_eq!(
            judge(Capability::Reasoning, &accepted).verdict,
            CapabilityVerdict::Inconclusive
        );

        let thought = outcome(
            Some(200),
            true,
            vec![
                ContentBlock::Thinking {
                    text: "设鸡 x 只…".into(),
                    signature: None,
                },
                ContentBlock::text("鸡 3 只兔 5 只"),
            ],
        );
        assert_eq!(
            judge(Capability::Reasoning, &thought).verdict,
            CapabilityVerdict::Supported
        );
    }

    #[test]
    fn vision_acceptance_is_enough() {
        let o = outcome(Some(200), true, vec![ContentBlock::text("红色")]);
        let r = judge(Capability::Vision, &o);
        assert_eq!(r.verdict, CapabilityVerdict::Supported);
        assert!(r.evidence.contains("红色"), "证据里要带上模型的回答");
    }

    #[test]
    fn the_probe_requests_ask_for_exactly_one_thing() {
        // 每个探测只测一种能力，别让它们互相污染。
        let tools = build_request("m", Capability::Tools);
        assert_eq!(tools.tools.len(), 1);
        assert!(tools.reasoning.is_none());
        assert_eq!(tools.tool_choice, Some(ToolChoice::Required));
        assert_eq!(tools.max_tokens, Some(256), "探测要快、要省");

        let vision = build_request("m", Capability::Vision);
        assert!(vision.tools.is_empty());
        assert!(matches!(vision.messages[0].content[0], ContentBlock::Image { .. }));

        let reasoning = build_request("m", Capability::Reasoning);
        assert!(reasoning.tools.is_empty());
        assert!(reasoning.reasoning.as_ref().unwrap().enabled);
        assert!(
            !reasoning.messages[0].content.iter().any(|b| matches!(b, ContentBlock::Image { .. })),
            "思考探测不该夹带图片"
        );
    }

    #[test]
    fn the_probe_image_is_a_real_png() {
        // 假的 base64 会被上游按"图片格式错误"拒掉，然后被误判成"不支持图片"。
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PROBE_IMAGE_B64)
            .expect("必须是合法 base64");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "必须是 PNG 而不是随便一段字节");
    }
}
