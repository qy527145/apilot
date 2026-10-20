//! 候选渠道的组装与排序。
//!
//! 「用哪个渠道」有两条优先级：模型策略 > selector。这个文件是它在出站前的落点。

use std::sync::Arc;

use crate::protocol::dto::Protocol;
use crate::routing::model_policy;
use crate::shell::AppShell;
use crate::upstream::outbound::Outbound;

/// 组装候选渠道：主渠道 + 其余（用于故障转移）。
///
/// 主渠道由谁决定，取决于这个模型有没有配过渠道策略
/// （`model_policies` 表，界面上在**路由页**的「模型的渠道选择」里改）：
///
/// - **配过** → 按策略在声明了这个模型的所有渠道里排序，第一个当主渠道。
///   这就是"像切换代理一样切换渠道"的落点。
/// - **没配过** → 沿用 `selector` 选出来的那个（现有行为，一键不动）。
///
/// 这条界线是整个方案不破坏既有配置的基础，改动时别把它弄丢了。
///
/// 注意它是**模型策略 > selector**：模型配了策略，规则链选出来的 selector
/// 就只剩"兜底"的意义了。这是刻意的 —— 别顺手把优先级反过来，
/// 那会改变线上实际发请求的行为。
pub(super) async fn build_candidates(
    shell: &Arc<AppShell>,
    primary: &Arc<dyn Outbound>,
    model: &str,
    protocol_in: Protocol,
) -> Vec<Arc<dyn Outbound>> {
    let channels = crate::storage::providers::candidate_channels(&shell.db, model)
        .await
        .unwrap_or_default();

    let policy = crate::storage::model_policies::get(&shell.db, model)
        .await
        .ok()
        .flatten();

    let mut out = match policy {
        Some(ref p) => {
            // 延迟策略要有测速结果；没测过的候选在 order() 里会被排到最后。
            let ranked: Vec<crate::routing::model_select::Candidate> = channels
                .iter()
                .map(|c| crate::routing::model_select::Candidate {
                    tag: c.provider.tag.clone(),
                    priority: c.priority,
                    weight: c.weight,
                    latency_ms: shell.probe_latency.get(&c.provider.tag).map(|v| *v),
                })
                .collect();

            let ordered =
                crate::routing::model_select::order(Some(p), ranked, crate::util::rand_unit());

            let mut cs: Vec<Arc<dyn Outbound>> = Vec::new();
            for tag in ordered {
                if let Some(o) = shell.registry.get(&tag) {
                    cs.push(o);
                }
            }

            // 模型页配的渠道一个都用不了（全被停用/删了）时不要就此失败，
            // 落回 selector 选出的那个，让请求还有救。
            if cs.is_empty() {
                vec![primary.clone()]
            } else {
                cs
            }
        }
        None => {
            // 没有模型策略时：如果 candidate_channels 返回了声明支持该模型的渠道，
            // 优先用那些渠道 —— selector 的 primary 是按路由规则选的，可能根本不服务
            // 这个模型（用户配了 A 渠道服务模型 a、B 渠道服务模型 b，请求模型 b 时
            // selector 仍选 A，就会打到不认识 b 的渠道）。
            //
            // 逻辑：
            //   - channels 里有 primary → 和原来一样，primary 排第一（稳定）。
            //   - channels 非空但不含 primary → channels 里第一个是主渠道，
            //     selector primary 追加到末尾作兜底（万一 channels 全挂掉）。
            //   - channels 为空（通吃渠道 / 没有任何声明） → 原来的兜底逻辑不变。
            let primary_in_channels = channels.iter().any(|c| c.provider.tag == primary.tag());

            if !channels.is_empty() && !primary_in_channels {
                // 有明确的模型候选，但 selector primary 不在其中：按模型候选排，
                // selector primary 只作最后的兜底。
                let mut cs: Vec<Arc<dyn Outbound>> = Vec::new();
                for c in &channels {
                    if let Some(o) = shell.registry.get(&c.provider.tag) {
                        cs.push(o);
                    }
                }
                if cs.is_empty() {
                    // 渠道全挂或全停用，才落回 selector。
                    vec![primary.clone()]
                } else {
                    // 追加 selector primary 作兜底：模型候选全部失败时还有退路。
                    if let Some(o) = shell.registry.get(primary.tag()) {
                        cs.push(o);
                    }
                    cs
                }
            } else {
                // channels 为空（通吃渠道）或 primary 本身就在 channels 里：原逻辑。
                let mut cs = vec![primary.clone()];
                for c in &channels {
                    if c.provider.tag != primary.tag() {
                        if let Some(o) = shell.registry.get(&c.provider.tag) {
                            cs.push(o);
                        }
                    }
                }
                cs
            }
        }
    };

    prefer_native_protocol(&mut out, primary, protocol_in);
    out
}

/// 管线侧的 [`model_policy::ModelEnv`]。
///
/// 渠道检查用预先查好的结果（要不要查由 `needs_channel_check` 决定），
/// 脚本则交给 `model_script` 的全局引擎 —— memo 缓存也在那边，这里看不见。
/// 这是**唯一**把那个 trait 接到真实引擎上的地方，测试里换掉它就完全不碰 JS。
pub(super) struct ShellModelEnv {
    pub(super) has_channels: bool,
}

impl model_policy::ModelEnv for ShellModelEnv {
    fn has_channels(&self, _: &str) -> bool {
        self.has_channels
    }

    fn run_script(&self, source: &str, input: &model_policy::ScriptInput<'_>) -> Option<String> {
        crate::routing::model_script::run(source, input)
    }
}

/// 把候选渠道按「能原生说客户端协议」重排：能直通的不转换。
///
/// `sort_by_key` 是稳定排序，同一组内部保持调用方排好的 priority / weight 顺序
/// —— 只在"协议匹配与否"这一维上重新分组，不推翻用户的优先级配置。
///
/// 首选渠道不参与排序：它是 selector 选出来的，位置由用户的路由规则决定，
/// 不能因为我们更想用别的协议就把它顶掉。
fn prefer_native_protocol(
    candidates: &mut [Arc<dyn Outbound>],
    primary: &Arc<dyn Outbound>,
    protocol_in: Protocol,
) {
    let primary_tag = primary.tag();
    candidates.sort_by_key(|o| o.tag() != primary_tag && !o.supports(protocol_in));
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use http::HeaderMap;
    use crate::upstream::outbound::UpstreamError;


    /// 只关心"支不支持某种协议"的假渠道，用于验证候选排序。
    struct FakeOutbound {
        tag: String,
        supports: Vec<Protocol>,
    }

    impl FakeOutbound {
        fn new(tag: &str, supports: Vec<Protocol>) -> Arc<dyn Outbound> {
            Arc::new(Self {
                tag: tag.into(),
                supports,
            })
        }
    }

    #[async_trait::async_trait]
    impl Outbound for FakeOutbound {
        fn tag(&self) -> &str {
            &self.tag
        }
        fn wire(&self) -> Protocol {
            Protocol::OpenAiChat
        }
        fn wire_for(&self, incoming: Protocol) -> Protocol {
            if self.supports(incoming) {
                incoming
            } else {
                Protocol::OpenAiChat
            }
        }
        fn supports(&self, protocol: Protocol) -> bool {
            self.supports.contains(&protocol)
        }
        fn provider(&self) -> &crate::storage::models::Provider {
            unimplemented!("排序测试不读渠道配置")
        }
        fn prepare_at(
            &self,
            _wire: Protocol,
            _incoming: &HeaderMap,
            _body: Bytes,
            _stream: bool,
            _path: Option<&str>,
        ) -> Result<crate::upstream::outbound::PreparedRequest, UpstreamError> {
            unimplemented!("排序测试不发请求")
        }
        async fn dial(
            &self,
            _req: crate::upstream::outbound::PreparedRequest,
        ) -> Result<crate::upstream::outbound::UpstreamResponse, UpstreamError> {
            unimplemented!("排序测试不发请求")
        }
    }

    fn tags(cs: &[Arc<dyn Outbound>]) -> Vec<String> {
        cs.iter().map(|c| c.tag().to_string()).collect()
    }

    #[test]
    fn candidates_speaking_the_client_protocol_come_first() {
        // 首选(selector 选的)不支持 Anthropic，后面两个支持 —— 支持的提到前面，
        // 但首选仍留在第一位，它的位置是路由规则定的。
        let primary = FakeOutbound::new("primary", vec![Protocol::OpenAiChat]);
        let mut cs = vec![
            primary.clone(),
            FakeOutbound::new("conv", vec![Protocol::OpenAiChat]),
            FakeOutbound::new("native", vec![Protocol::AnthropicMessages]),
        ];

        prefer_native_protocol(&mut cs, &primary, Protocol::AnthropicMessages);

        assert_eq!(tags(&cs), vec!["primary", "native", "conv"]);
    }

    #[test]
    fn candidate_order_within_a_group_is_preserved() {
        // 稳定排序：同组内保持调用方排好的优先级，不能被打乱。
        let primary = FakeOutbound::new("a-native", vec![Protocol::AnthropicMessages]);
        let mut cs = vec![
            primary.clone(),
            FakeOutbound::new("b-conv", vec![Protocol::OpenAiChat]),
            FakeOutbound::new("c-native", vec![Protocol::AnthropicMessages]),
            FakeOutbound::new("d-conv", vec![Protocol::OpenAiChat]),
        ];

        prefer_native_protocol(&mut cs, &primary, Protocol::AnthropicMessages);

        assert_eq!(tags(&cs), vec!["a-native", "c-native", "b-conv", "d-conv"]);
    }
}
