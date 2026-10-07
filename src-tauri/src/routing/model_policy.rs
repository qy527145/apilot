//! 模型替换的判定：「这一次请求到底该发哪个模型名」。
//!
//! 两级配置（全局 + 客户端）在 [`ModelPolicy::effective`] 里拼成一条规则，本模块
//! 只负责把那条规则算成一个模型名。判定是**纯函数** —— 它需要向外部世界问的两件事
//! （这个模型有没有渠道、跑一段用户脚本）都从 [`ModelEnv`] 注入，所以四种模式
//! 都能脱离数据库与 JS 引擎测到。
//!
//! 它与路由规则的关系是**叠加**而非替代：本模块先决定生效模型，规则链随后在它
//! 之上跑，规则里的 `ModelOverride` 仍可再改。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use crate::config::settings::{
    CustomForm, MatchKind, MappingRow, ModelPolicy, ModelPolicyMode,
};

/// 传给用户脚本的上下文；与 JS 侧的 `ctx` 字段一一对应。
#[derive(Debug, Clone, Copy)]
pub struct ScriptInput<'a> {
    pub model: &'a str,
    pub client: &'a str,
    pub protocol: &'a str,
}

/// 一次判定的请求上下文。
#[derive(Debug, Clone, Copy)]
pub struct RequestCtx<'a> {
    pub client: &'a str,
    pub model: &'a str,
    pub protocol: &'a str,
}

/// 判定需要向外部世界问的两件事。
///
/// 抽成 trait 而不是直接查库 / 直接调 JS 引擎：`resolve` 因此仍是纯函数，
/// 每种模式都能脱离数据库与 JS 引擎测到 —— 与当年把「有没有渠道」抽成参数是
/// 同一个理由，只是现在有两件事要问了。
pub trait ModelEnv {
    /// 请求的模型在 Apilot 里有没有可用渠道。只有兜底模式会问。
    fn has_channels(&self, model: &str) -> bool;
    /// 跑一段用户脚本，返回要用的模型名。任何异常都该折成 `None`，不许冒泡。
    fn run_script(&self, source: &str, input: &ScriptInput<'_>) -> Option<String>;
}

/// 只有兜底模式需要知道「请求的模型有没有可用渠道」。
///
/// 必须按**该客户端实际生效的**模式判断：全局是不改写、而某个客户端被单独设成
/// 兜底时，那一次 SQL 还是得查；反过来，客户端覆盖成自定义规则把全局的兜底盖掉
/// 时，就不该再白查一次库。
pub fn needs_channel_check(policy: &ModelPolicy, client: &str) -> bool {
    policy.effective(client).mode == ModelPolicyMode::Fallback
}

/// 算出该用哪个模型。返回 `None` 表示"不改，用客户端请求的那个"。
pub fn resolve(policy: &ModelPolicy, req: &RequestCtx<'_>, env: &dyn ModelEnv) -> Option<String> {
    let rule = policy.effective(req.client);

    match rule.mode {
        ModelPolicyMode::Passthrough => None,

        // 无条件：**不做**"目标模型有没有渠道"的检查。用户选了个还没配渠道的
        // 模型时，应当看到上游报错，而不是被悄悄改回原样 —— 后者会让人以为
        // 开关没生效。
        ModelPolicyMode::Always => non_blank(rule.model).map(String::from),

        ModelPolicyMode::Fallback => {
            if env.has_channels(req.model) {
                None
            } else {
                non_blank(rule.model).map(String::from)
            }
        }

        ModelPolicyMode::Custom => match rule.custom.form {
            CustomForm::Script => rule.custom.script.as_deref().and_then(|source| {
                env.run_script(
                    source,
                    &ScriptInput {
                        model: req.model,
                        client: req.client,
                        protocol: req.protocol,
                    },
                )
            }),
            // Unknown 只可能来自手改 JSON，`normalized()` 已经把它折成 Table 了。
            CustomForm::Table | CustomForm::Unknown => {
                resolve_table(&rule.custom.table, req.client, req.model)
            }
        },

        // 这两个在 `normalized()` 之后不该出现；真出现了按"不改写"处理 ——
        // 宁可不动，也不要猜用户想要什么。
        ModelPolicyMode::PerClientLegacy | ModelPolicyMode::Unknown => None,
    }
}

/// 供管线调用：判定 + 兜底成"不改就用原值"。
///
/// 调用点在 `gateway/pipeline.rs` 解码之后、构造 `RouteMetadata` 之前 ——
/// 这样规则链看到的就是生效模型。
pub fn effective_model(policy: &ModelPolicy, req: &RequestCtx<'_>, env: &dyn ModelEnv) -> String {
    resolve(policy, req, env).unwrap_or_else(|| req.model.to_string())
}

/// 映射表：自上而下，首个命中生效。
///
/// 命中行若没写目标，就是「保持原样」—— 在此停下且不改写。所以这里是 `and_then`
/// 把那个 `None` 直接吃掉，而不是继续往下找。
fn resolve_table(rows: &[MappingRow], client: &str, requested: &str) -> Option<String> {
    rows.iter()
        .find(|r| {
            client_filter_matches(r.client.as_deref(), client)
                && row_matches(r.match_kind, r.pattern.as_deref(), requested)
        })
        .and_then(|r| non_blank(r.target.as_deref()).map(String::from))
}

/// 行上的客户端过滤。留空 = 任何客户端都适用。
fn client_filter_matches(filter: Option<&str>, client: &str) -> bool {
    filter.is_none_or(|f| f == client)
}

fn row_matches(kind: MatchKind, pattern: Option<&str>, requested: &str) -> bool {
    match kind {
        // 兜底行：无条件命中。
        MatchKind::Any => true,
        MatchKind::Prefix => pattern.is_some_and(|p| requested.starts_with(p)),
        MatchKind::Exact => pattern.is_some_and(|p| requested == p),
        MatchKind::Glob => pattern.is_some_and(|p| wildcard_match(p, requested)),
        MatchKind::Regex => pattern.is_some_and(|p| regex_matches(p, requested)),
        // 认不出的（手改 JSON 写错）永不命中，而不是让整份设置失效。
        MatchKind::Unknown => false,
    }
}

/// `*` 匹配任意长度（含空），`?` 匹配恰好一个字符。
///
/// 手写而不拉 `glob` crate：这里只要这两种语义，而那个 crate 是按文件路径设计的，
/// 路径分隔符、`**`、`[...]` 这些规则在模型名上是纯粹的干扰。
/// 双指针 + 回溯，最坏 O(n·m) —— 两边都是模型名，短得很。
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一个 `*` 在模式里的位置，以及它当时吃到文本的哪个下标。
    let mut star: Option<(usize, usize)> = None;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            // 回溯：让那个 `*` 多吃一个字符再试。
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }

    // 文本走完了，模式剩下的必须全是 `*`。
    p[pi..].iter().all(|c| *c == '*')
}

/// 编译过的正则。
///
/// 映射表是**每请求**求值的，`Regex::new` 每次重编开销太大（微秒到毫秒级，
/// 看表达式）。编译失败也缓存成 `None`，免得一个写坏的表达式每请求都重试一遍。
static REGEX_CACHE: Lazy<Mutex<HashMap<String, Option<regex::Regex>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 上限满了就整表清空：条目数 = 去重后的表达式数量，本来就很小。
const REGEX_CACHE_CAPACITY: usize = 256;

fn regex_matches(pattern: &str, text: &str) -> bool {
    // 本函数是同步的，不存在"跨 await 持锁"。
    let mut cache = REGEX_CACHE.lock().unwrap();
    if cache.len() >= REGEX_CACHE_CAPACITY {
        cache.clear();
    }

    let compiled = cache
        .entry(pattern.to_string())
        .or_insert_with(|| regex::Regex::new(pattern).ok());

    compiled.as_ref().is_some_and(|re| re.is_match(text))
}

/// `normalized()` 已经清过一遍空白，这里是最后一道防线：空串会被当成一个
/// 真实的模型名发往上游，那种错很难查。
fn non_blank(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{
        ClientMode, ClientOverride, ClientRule, CustomRules, MappingRow,
    };
    use std::cell::{Cell, RefCell};

    /// 假的外部世界：要渠道有渠道、要脚本有脚本，两件事都能单独摆布。
    struct FakeEnv {
        has_channels: bool,
        script: Option<String>,
        /// 脚本被调用的次数 —— 用来钉住"映射表模式下根本不该去碰脚本"。
        script_calls: Cell<u32>,
        last_input: RefCell<Option<(String, String, String)>>,
    }

    impl FakeEnv {
        fn new(has_channels: bool) -> Self {
            Self {
                has_channels,
                script: None,
                script_calls: Cell::new(0),
                last_input: RefCell::new(None),
            }
        }

        fn returning(mut self, model: &str) -> Self {
            self.script = Some(model.to_string());
            self
        }

        fn calls(&self) -> u32 {
            self.script_calls.get()
        }
    }

    impl ModelEnv for FakeEnv {
        fn has_channels(&self, _: &str) -> bool {
            self.has_channels
        }

        fn run_script(&self, _: &str, input: &ScriptInput<'_>) -> Option<String> {
            self.script_calls.set(self.script_calls.get() + 1);
            *self.last_input.borrow_mut() = Some((
                input.model.to_string(),
                input.client.to_string(),
                input.protocol.to_string(),
            ));
            self.script.clone()
        }
    }

    fn req<'a>(client: &'a str, model: &'a str) -> RequestCtx<'a> {
        RequestCtx {
            client,
            model,
            protocol: "anthropic",
        }
    }

    /// 全局一条规则，没有客户端覆盖。
    fn global(mode: ModelPolicyMode, model: Option<&str>) -> ModelPolicy {
        ModelPolicy {
            mode,
            active_model: model.map(String::from),
            ..Default::default()
        }
    }

    fn with_client(mut policy: ModelPolicy, client: &str, rule: ClientRule) -> ModelPolicy {
        policy
            .per_client
            .insert(client.to_string(), ClientOverride::Rule(rule));
        policy.normalized()
    }

    fn custom_table(rows: Vec<MappingRow>) -> CustomRules {
        CustomRules {
            form: CustomForm::Table,
            table: rows,
            script: None,
        }
    }

    fn row(kind: MatchKind, pattern: &str, target: Option<&str>) -> MappingRow {
        MappingRow {
            match_kind: kind,
            pattern: Some(pattern.to_string()),
            target: target.map(String::from),
            client: None,
        }
    }

    // ---- 全局四种模式 ----

    #[test]
    fn passthrough_never_rewrites() {
        let p = global(ModelPolicyMode::Passthrough, Some("deepseek-chat"));
        let env = FakeEnv::new(true);
        assert_eq!(resolve(&p, &req("codex", "gpt-5"), &env), None);
        assert_eq!(resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(false)), None);
    }

    #[test]
    fn always_rewrites_regardless_of_channels() {
        let p = global(ModelPolicyMode::Always, Some("deepseek-chat"));
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)).as_deref(),
            Some("deepseek-chat")
        );
        assert_eq!(
            resolve(&p, &req("claude-code", "claude-sonnet-5"), &FakeEnv::new(false)).as_deref(),
            Some("deepseek-chat"),
            "无条件模式不该去检查目标模型有没有渠道"
        );
    }

    #[test]
    fn always_without_a_model_configured_is_a_noop() {
        // 开关打开但没选模型：不改，而不是把模型替换成空字符串。
        let env = FakeEnv::new(true);
        assert_eq!(
            resolve(&global(ModelPolicyMode::Always, None), &req("codex", "gpt-5"), &env),
            None
        );
        assert_eq!(
            resolve(&global(ModelPolicyMode::Always, Some("   ")), &req("codex", "gpt-5"), &env),
            None,
            "空白串等同于没配"
        );
    }

    #[test]
    fn fallback_only_rewrites_when_the_requested_model_is_unserved() {
        let p = global(ModelPolicyMode::Fallback, Some("deepseek-chat"));

        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)),
            None,
            "客户端要的模型有人提供时不该被替换"
        );
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(false)).as_deref(),
            Some("deepseek-chat")
        );
    }

    // ---- 两级：客户端覆盖全局 ----

    #[test]
    fn a_client_entry_overrides_the_global_mode() {
        let p = with_client(
            global(ModelPolicyMode::Always, Some("global-model")),
            "codex",
            ClientRule {
                mode: ClientMode::Passthrough,
                ..Default::default()
            },
        );

        let env = FakeEnv::new(true);
        assert_eq!(
            resolve(&p, &req("claude-code", "claude-sonnet-5"), &env).as_deref(),
            Some("global-model"),
            "没配过的客户端拿全局的"
        );
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env),
            None,
            "客户端把自己设成不改写，就该盖掉全局的强制替换"
        );
    }

    #[test]
    fn an_inherit_client_entry_falls_back_to_the_global_mode() {
        // 「跟随全局」的条目在 normalized() 里就被丢掉了，判定逻辑根本见不到它。
        let p = with_client(
            global(ModelPolicyMode::Always, Some("global-model")),
            "codex",
            ClientRule {
                mode: ClientMode::Inherit,
                ..Default::default()
            },
        );
        assert!(p.per_client.is_empty());
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)).as_deref(),
            Some("global-model")
        );
    }

    #[test]
    fn a_client_rule_without_its_own_model_uses_the_global_one() {
        let p = with_client(
            global(ModelPolicyMode::Passthrough, Some("global-model")),
            "codex",
            ClientRule {
                mode: ClientMode::Always,
                model: None,
                ..Default::default()
            },
        );
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)).as_deref(),
            Some("global-model")
        );
    }

    #[test]
    fn a_client_rule_may_pick_its_own_model() {
        let p = with_client(
            global(ModelPolicyMode::Always, Some("global-model")),
            "codex",
            ClientRule {
                mode: ClientMode::Always,
                model: Some("codex-model".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)).as_deref(),
            Some("codex-model")
        );
    }

    #[test]
    fn needs_channel_check_follows_the_global_mode() {
        assert!(needs_channel_check(
            &global(ModelPolicyMode::Fallback, Some("m")),
            "codex"
        ));
        assert!(!needs_channel_check(
            &global(ModelPolicyMode::Always, Some("m")),
            "codex"
        ));
    }

    #[test]
    fn needs_channel_check_is_true_when_only_a_client_rule_is_fallback() {
        // 全局不改写，但 codex 被单独设成兜底 —— 那一次 SQL 还是得查，
        // 而且只对 codex 查。
        let p = with_client(
            global(ModelPolicyMode::Passthrough, Some("deepseek-chat")),
            "codex",
            ClientRule {
                mode: ClientMode::Fallback,
                ..Default::default()
            },
        );

        assert!(needs_channel_check(&p, "codex"));
        assert!(!needs_channel_check(&p, "claude-code"));
    }

    #[test]
    fn needs_channel_check_is_false_when_a_client_rule_shadows_a_global_fallback() {
        // 反过来的情形：全局是兜底，客户端换成不改写，就不该再白查一次库。
        let p = with_client(
            global(ModelPolicyMode::Fallback, Some("deepseek-chat")),
            "codex",
            ClientRule {
                mode: ClientMode::Passthrough,
                ..Default::default()
            },
        );

        assert!(!needs_channel_check(&p, "codex"));
        assert!(needs_channel_check(&p, "claude-code"));
    }

    // ---- 自定义规则：映射表 ----

    fn table_policy(rows: Vec<MappingRow>) -> ModelPolicy {
        ModelPolicy {
            mode: ModelPolicyMode::Custom,
            custom: custom_table(rows),
            ..Default::default()
        }
    }

    #[test]
    fn table_picks_the_first_matching_row() {
        let p = table_policy(vec![
            row(MatchKind::Prefix, "claude", Some("first")),
            row(MatchKind::Prefix, "claude-sonnet", Some("second")),
        ]);
        assert_eq!(
            resolve(&p, &req("codex", "claude-sonnet-5"), &FakeEnv::new(true)).as_deref(),
            Some("first"),
            "自上而下，先命中的赢"
        );
    }

    #[test]
    fn a_row_without_a_target_stops_the_scan_without_rewriting() {
        let p = table_policy(vec![
            row(MatchKind::Prefix, "claude", None), // 保持原样
            row(MatchKind::Any, "", Some("later")),
        ]);
        assert_eq!(
            resolve(&p, &req("codex", "claude-sonnet-5"), &FakeEnv::new(true)),
            None,
            "命中「保持原样」就该停下，后面的行不该再有机会改写"
        );
    }

    #[test]
    fn table_falls_through_when_no_row_matches() {
        let p = table_policy(vec![row(MatchKind::Exact, "gpt-5", Some("x"))]);
        assert_eq!(resolve(&p, &req("codex", "other"), &FakeEnv::new(true)), None);
    }

    #[test]
    fn a_catch_all_row_rewrites_everything() {
        let p = table_policy(vec![MappingRow {
            match_kind: MatchKind::Any,
            pattern: None,
            target: Some("fallback-model".into()),
            client: None,
        }]);
        assert_eq!(
            resolve(&p, &req("codex", "whatever"), &FakeEnv::new(true)).as_deref(),
            Some("fallback-model")
        );
    }

    #[test]
    fn table_client_filter_skips_rows_belonging_to_other_clients() {
        let mut codex_only = row(MatchKind::Prefix, "claude", Some("for-codex"));
        codex_only.client = Some("codex".into());

        let p = table_policy(vec![codex_only, row(MatchKind::Any, "", Some("for-others"))]);

        assert_eq!(
            resolve(&p, &req("codex", "claude-sonnet-5"), &FakeEnv::new(true)).as_deref(),
            Some("for-codex")
        );
        assert_eq!(
            resolve(&p, &req("gemini-cli", "claude-sonnet-5"), &FakeEnv::new(true)).as_deref(),
            Some("for-others"),
            "限定了客户端的行对别的客户端要当作不存在"
        );
    }

    #[test]
    fn prefix_exact_and_any_matchers() {
        let p = table_policy(vec![
            row(MatchKind::Prefix, "claude", Some("by-prefix")),
            row(MatchKind::Exact, "gpt-5", Some("by-exact")),
            MappingRow {
                match_kind: MatchKind::Any,
                target: Some("by-any".into()),
                ..Default::default()
            },
        ]);

        let env = FakeEnv::new(true);
        let hit = |model: &str| resolve(&p, &req("codex", model), &env);

        assert_eq!(hit("claude-sonnet-5").as_deref(), Some("by-prefix"));
        assert_eq!(hit("gpt-5").as_deref(), Some("by-exact"));
        assert_eq!(hit("gpt-5-mini").as_deref(), Some("by-any"), "精确匹配不认前缀");
    }

    #[test]
    fn wildcard_star_and_question_mark_match() {
        assert!(wildcard_match("claude-*", "claude-sonnet-5"));
        assert!(wildcard_match("*sonnet*", "claude-sonnet-5"));
        assert!(wildcard_match("gpt-?.5-mini", "gpt-5.5-mini"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("", ""));

        assert!(!wildcard_match("claude-*", "gpt-5"));
        assert!(!wildcard_match("", "gpt-5"));
        assert!(!wildcard_match("gpt-?", "gpt-5.5"));
        assert!(!wildcard_match("*-mini", "gpt-5"));
        // 多个 `*` 时回溯要走对。
        assert!(wildcard_match("*-*-mini", "gpt-5.5-mini"));
        assert!(wildcard_match("a*b*c", "azzbzzc"));
        assert!(!wildcard_match("a*b*c", "azzbzz"));
    }

    #[test]
    fn a_regex_row_matches_and_is_cached() {
        let p = table_policy(vec![row(MatchKind::Regex, r"^gpt-\d+$", Some("digit"))]);
        let env = FakeEnv::new(true);

        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env).as_deref(),
            Some("digit")
        );
        // 第二次走缓存，结果必须一模一样。
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env).as_deref(),
            Some("digit")
        );
        assert_eq!(resolve(&p, &req("codex", "gpt-5-mini"), &env), None);
    }

    #[test]
    fn a_broken_regex_row_never_matches() {
        // normalized() 会丢掉这种行；真漏到判定逻辑里也只能不命中，不能 panic。
        assert!(!row_matches(MatchKind::Regex, Some("("), "anything"));
        assert!(!row_matches(MatchKind::Unknown, Some("x"), "x"));
    }

    // ---- 自定义规则：脚本 ----

    fn script_policy(source: Option<&str>) -> ModelPolicy {
        ModelPolicy {
            mode: ModelPolicyMode::Custom,
            custom: CustomRules {
                form: CustomForm::Script,
                script: source.map(String::from),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn script_form_receives_the_ctx_and_uses_the_return() {
        let env = FakeEnv::new(true).returning("from-script");
        let got = resolve(
            &script_policy(Some("function resolve(ctx){}")),
            &req("codex", "gpt-5"),
            &env,
        );

        assert_eq!(got.as_deref(), Some("from-script"));
        assert_eq!(env.calls(), 1);
        assert_eq!(
            env.last_input.borrow().as_ref(),
            Some(&("gpt-5".to_string(), "codex".to_string(), "anthropic".to_string())),
            "脚本要拿到原始的模型名、客户端与协议"
        );
    }

    #[test]
    fn script_form_returning_none_keeps_the_requested_model() {
        let env = FakeEnv::new(true); // script = None
        assert_eq!(
            resolve(&script_policy(Some("function resolve(){return null}")), &req("codex", "gpt-5"), &env),
            None
        );
    }

    #[test]
    fn script_form_without_a_script_is_a_noop() {
        let env = FakeEnv::new(true);
        assert_eq!(resolve(&script_policy(None), &req("codex", "gpt-5"), &env), None);
        assert_eq!(env.calls(), 0);
    }

    #[test]
    fn table_form_never_calls_the_script_runner() {
        // 映射表模式下碰都不该碰脚本 —— 那会把每请求都拖进 JS 引擎。
        let mut p = table_policy(vec![row(MatchKind::Prefix, "claude", Some("hit"))]);
        p.custom.script = Some("function resolve(){ return 'from-script' }".into());

        let env = FakeEnv::new(true);
        assert_eq!(
            resolve(&p, &req("codex", "claude-sonnet-5"), &env).as_deref(),
            Some("hit")
        );
        assert_eq!(env.calls(), 0);
    }

    #[test]
    fn a_client_may_use_its_own_custom_rule() {
        // 客户端级的自定义规则整套由客户端自己带，不跟全局的混。
        let mut p = global(ModelPolicyMode::Always, Some("global-model"));
        p.custom = custom_table(vec![row(MatchKind::Any, "", Some("global-rule"))]);
        let p = with_client(
            p,
            "codex",
            ClientRule {
                mode: ClientMode::Custom,
                custom: custom_table(vec![row(MatchKind::Any, "", Some("codex-rule"))]),
                ..Default::default()
            },
        );

        let env = FakeEnv::new(true);
        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env).as_deref(),
            Some("codex-rule")
        );
        assert_eq!(
            resolve(&p, &req("claude-code", "x"), &env).as_deref(),
            Some("global-model"),
            "全局那条是强制替换，不受 Codex 的自定义规则影响"
        );
    }

    // ---- 跨模块：策略判定 + 真的 JS 引擎 ----
    //
    // 判定与执行器各自单测都过、连起来却不工作，是这类功能最典型的坏法。
    // 下面这几条走的是和管线同一根线：真 policy、真 QuickJS。
    struct RealScripts {
        has_channels: bool,
    }

    impl ModelEnv for RealScripts {
        fn has_channels(&self, _: &str) -> bool {
            self.has_channels
        }

        fn run_script(&self, source: &str, input: &ScriptInput<'_>) -> Option<String> {
            crate::routing::model_script::run(source, input)
        }
    }

    #[test]
    fn a_script_policy_actually_runs_through_the_real_engine() {
        let env = RealScripts {
            has_channels: true,
        };
        let p = script_policy(Some(
            "function resolve(ctx) { return ctx.client === 'codex' ? 'codex-model' : null; }",
        ));

        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env).as_deref(),
            Some("codex-model"),
            "脚本拿得到客户端，返回值也该被用上"
        );
        assert_eq!(
            resolve(&p, &req("claude-code", "claude-sonnet-5"), &env),
            None,
            "返回 null 就是不改写"
        );
    }

    #[test]
    fn a_hanging_script_through_the_real_engine_keeps_the_requested_model() {
        let env = RealScripts {
            has_channels: true,
        };
        let p = script_policy(Some("function resolve(ctx) { while (true) {} }"));

        assert_eq!(
            resolve(&p, &req("codex", "gpt-5"), &env),
            None,
            "脚本死循环也必须按不改写处理，请求不能失败"
        );
    }

    // ---- 兜底 ----

    #[test]
    fn effective_model_falls_back_to_the_request_when_nothing_matches() {
        let env = FakeEnv::new(true);
        let p = table_policy(vec![row(MatchKind::Exact, "nope", Some("x"))]);
        assert_eq!(effective_model(&p, &req("codex", "gpt-5"), &env), "gpt-5");
    }

    #[test]
    fn effective_model_rewrites_when_a_row_matches() {
        let env = FakeEnv::new(true);
        let p = table_policy(vec![row(MatchKind::Prefix, "gpt", Some("deepseek-chat"))]);
        assert_eq!(
            effective_model(&p, &req("codex", "gpt-5"), &env),
            "deepseek-chat"
        );
    }

    #[test]
    fn a_legacy_policy_that_was_never_normalized_stays_a_noop() {
        // load() 会折算它；万一漏了，也不能拿它去猜一个模型名。
        let p = global(ModelPolicyMode::PerClientLegacy, Some("deepseek-chat"));
        assert_eq!(resolve(&p, &req("codex", "gpt-5"), &FakeEnv::new(true)), None);
    }

    #[test]
    fn policy_survives_a_roundtrip_through_settings_json() {
        // 设置整体存成一条 JSON，加字段不能让老设置反序列化失败 —— 那会让
        // 用户的所有设置（含监听端口）在升级后静默回落到默认值。
        let old = r#"{"listen_port":9999,"listen_host":"0.0.0.0"}"#;
        let s: crate::config::settings::AppSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.listen_port, 9999);
        assert_eq!(s.model_policy.mode, ModelPolicyMode::Passthrough);

        let mut p = global(ModelPolicyMode::Custom, Some("m"));
        p.custom = custom_table(vec![row(MatchKind::Glob, "claude-*", Some("target"))]);
        p.per_client.insert(
            "codex".into(),
            ClientOverride::Rule(ClientRule {
                mode: ClientMode::Always,
                model: Some("g".into()),
                ..Default::default()
            }),
        );

        let json = serde_json::to_string(&p).unwrap();
        let back: ModelPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.normalized(), p.normalized());
    }
}
