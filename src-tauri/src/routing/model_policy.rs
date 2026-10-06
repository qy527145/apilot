//! 全局模型替换：「不管客户端发什么模型，统一换成选中的那个」。
//!
//! 这是给日常使用准备的一条捷径 —— 不必理解 selector 热切换与规则链，选个模型即可。
//! 它与路由规则的关系是**叠加**而非替代：本模块先决定生效模型，
//! 规则链随后在它之上跑，规则里的 `ModelOverride` 仍可再改。

use crate::config::settings::{ModelPolicy, ModelPolicyMode};

/// 只有 `Fallback` 需要知道「请求的模型在 Apilot 里有没有可用渠道」。
///
/// 单独暴露出来，是为了让调用方**只在必要时**查一次库 —— 其余三种模式都是
/// 纯内存判断，白跑一次 SQL 没有意义。
pub fn needs_channel_check(policy: &ModelPolicy) -> bool {
    policy.mode == ModelPolicyMode::Fallback
}

/// 算出该用哪个模型。返回 `None` 表示"不改，用客户端请求的那个"。
///
/// `has_channels` 把「有没有渠道」抽成参数而不是直接查库，判定逻辑因此是纯函数，
/// 可以脱离数据库把每种模式都测到。
pub fn resolve(
    policy: &ModelPolicy,
    client: &str,
    requested: &str,
    has_channels: impl Fn(&str) -> bool,
) -> Option<String> {
    let active = || {
        policy
            .active_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    match policy.mode {
        ModelPolicyMode::Off => None,

        // 无条件：**不做**"目标模型有没有渠道"的检查。用户选了个还没配渠道的模型时，
        // 应当看到上游报错，而不是被悄悄改回原样 —— 后者会让人以为开关没生效。
        ModelPolicyMode::Always => active().map(String::from),

        ModelPolicyMode::Fallback => {
            if has_channels(requested) {
                None
            } else {
                active().map(String::from)
            }
        }

        // 界面上的"跟随全局"存成空白串，filter 把它当作没配。
        ModelPolicyMode::PerClient => policy
            .for_client(client)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(active)
            .map(String::from),
    }
}

/// 供管线调用：判定 + 兜底成"不改就用原值"。
///
/// 调用点在 `gateway/pipeline.rs` 解码之后、构造 `RouteMetadata` 之前 ——
/// 这样规则链看到的就是生效模型，规则里的 `ModelOverride` 仍可在其上再改。
pub fn effective_model(
    policy: &ModelPolicy,
    client: &str,
    requested: &str,
    has_channels: bool,
) -> String {
    resolve(policy, client, requested, |_| has_channels)
        .unwrap_or_else(|| requested.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: ModelPolicyMode, active: Option<&str>) -> ModelPolicy {
        ModelPolicy {
            mode,
            active_model: active.map(String::from),
            per_client: Default::default(),
        }
    }

    /// 所有模型都有渠道。
    fn any(_: &str) -> bool {
        true
    }
    /// 所有模型都没有渠道。
    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn off_never_rewrites() {
        let p = policy(ModelPolicyMode::Off, Some("deepseek-chat"));
        assert_eq!(resolve(&p, "codex", "gpt-5", any), None);
        assert_eq!(resolve(&p, "codex", "gpt-5", none), None);
    }

    #[test]
    fn always_rewrites_regardless_of_channels() {
        let p = policy(ModelPolicyMode::Always, Some("deepseek-chat"));
        assert_eq!(
            resolve(&p, "codex", "gpt-5", any).as_deref(),
            Some("deepseek-chat")
        );
        assert_eq!(
            resolve(&p, "claude-code", "claude-sonnet-5", none).as_deref(),
            Some("deepseek-chat"),
            "无条件模式不该去检查目标模型有没有渠道"
        );
    }

    #[test]
    fn always_without_a_model_configured_is_a_noop() {
        // 开关打开但没选模型：不改，而不是把模型替换成空字符串。
        let p = policy(ModelPolicyMode::Always, None);
        assert_eq!(resolve(&p, "codex", "gpt-5", any), None);

        let p = policy(ModelPolicyMode::Always, Some("   "));
        assert_eq!(resolve(&p, "codex", "gpt-5", any), None, "空白串等同于没配");
    }

    #[test]
    fn fallback_only_rewrites_when_the_requested_model_is_unserved() {
        let p = policy(ModelPolicyMode::Fallback, Some("deepseek-chat"));

        assert_eq!(
            resolve(&p, "codex", "gpt-5", any),
            None,
            "客户端要的模型有人提供时不该被替换"
        );
        assert_eq!(
            resolve(&p, "codex", "gpt-5", none).as_deref(),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn per_client_picks_by_client() {
        let mut p = policy(ModelPolicyMode::PerClient, Some("deepseek-chat"));
        p.per_client.insert("codex".into(), "gpt-5".into());
        p.per_client.insert("claude-code".into(), "claude-sonnet-5".into());

        assert_eq!(resolve(&p, "codex", "whatever", any).as_deref(), Some("gpt-5"));
        assert_eq!(
            resolve(&p, "claude-code", "whatever", any).as_deref(),
            Some("claude-sonnet-5")
        );
        // 没配过的客户端落到全局那个。
        assert_eq!(
            resolve(&p, "cursor", "whatever", any).as_deref(),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn per_client_without_global_falls_back_to_the_request() {
        let mut p = policy(ModelPolicyMode::PerClient, None);
        p.per_client.insert("codex".into(), "gpt-5".into());

        assert_eq!(resolve(&p, "codex", "x", any).as_deref(), Some("gpt-5"));
        assert_eq!(resolve(&p, "cursor", "x", any), None);
    }

    #[test]
    fn per_client_ignores_blank_entries() {
        // 界面上"跟随全局"会存成空白，那种条目要当作没配，而不是替换成空模型名。
        let mut p = policy(ModelPolicyMode::PerClient, Some("deepseek-chat"));
        p.per_client.insert("codex".into(), "".into());
        p.per_client.insert("cursor".into(), "  ".into());

        assert_eq!(
            resolve(&p, "codex", "x", any).as_deref(),
            Some("deepseek-chat")
        );
        assert_eq!(
            resolve(&p, "cursor", "x", any).as_deref(),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn policy_survives_a_roundtrip_through_settings_json() {
        // 设置整体存成一条 JSON，加字段不能让老设置反序列化失败 —— 那会让
        // 用户的所有设置（含监听端口）在升级后静默回落到默认值。
        let old = r#"{"listen_port":9999,"listen_host":"0.0.0.0"}"#;
        let s: crate::config::settings::AppSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.listen_port, 9999);
        assert_eq!(s.model_policy.mode, ModelPolicyMode::Off);

        let mut p = ModelPolicy::default();
        p.mode = ModelPolicyMode::PerClient;
        p.active_model = Some("m".into());
        p.per_client.insert("codex".into(), "g".into());
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<ModelPolicy>(&json).unwrap(), p);
    }
}
