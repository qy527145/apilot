//! 各客户端的接管逻辑。
//!
//! 每个客户端只知道"怎么把 base_url 指到本地网关"，还原由引擎按备份统一处理 ——
//! 这样新增客户端时不必再写一遍还原逻辑，也就不会写出还原不干净的新 bug。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::engine::{FilePatch, TakeoverEngine, TakeoverPlan};
use super::floor;
use super::patch::{self, DotenvOp, JsonOp, TomlOp, TomlValue};
use crate::config::paths;
use crate::error::AppResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientId {
    ClaudeCode,
    Codex,
    GeminiCli,
}

impl ClientId {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude-code" | "claude" | "claude_code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "gemini-cli" | "gemini" | "gemini_cli" => Some(Self::GeminiCli),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::GeminiCli => "gemini-cli",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    /// 该客户端会被我们改动的所有文件。
    pub fn config_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::ClaudeCode => vec![paths::claude_settings_path()],
            // auth.json 不列入：我们不动它。
            Self::Codex => vec![paths::codex_config_path()],
            Self::GeminiCli => vec![paths::gemini_env_path()],
        }
    }

    /// 该客户端的主配置文件 —— 界面展示、「打开」按钮都指它。
    ///
    /// 取第一个是约定：目前每个客户端只有一个待写文件（Codex 的 auth.json 我们不动），
    /// 真出现多文件的那天，这里的"主"要重新定义。
    pub fn primary_config_path(&self) -> Option<PathBuf> {
        self.config_paths().into_iter().next()
    }

    /// 这个 base_url 会被**以什么形式**记进该客户端的配置里。
    ///
    /// 与 `plan_*` 写进去的值必须逐字一致 —— 判断「客户端是不是已经指着这个地址」
    /// 全靠它。Codex 那边带 `/v1`：它会在 base_url 后面自己拼 `/responses`（见 `plan_codex`）。
    pub fn stored_base_url(&self, base_url: &str) -> String {
        match self {
            Self::Codex => format!("{}/v1", base_url.trim_end_matches('/')),
            Self::ClaudeCode | Self::GeminiCli => base_url.to_string(),
        }
    }

    /// [`ClientId::stored_base_url`] 的逆运算：把配置里记着的那份取回成 base_url。
    ///
    /// 只在「网关没在跑」时用（见 [`reapply_taken_over`]）：那时没有真实地址可写，
    /// 而地址这一项本来就不该在策略变更里变，拿配置里现有的值回写等于保持原样。
    /// 两边必须成对维护 —— 有往返测试钉着。
    pub fn base_url_from_stored(&self, stored: &str) -> String {
        match self {
            // 那个 `/v1` 后缀是 `stored_base_url` 加上去的，取回来时要去掉。
            Self::Codex => match stored.trim_end_matches('/').strip_suffix("/v1") {
                Some(rest) => rest.to_string(),
                None => stored.to_string(),
            },
            // 另外两个存的就是 base_url 本身。
            Self::ClaudeCode | Self::GeminiCli => stored.to_string(),
        }
    }

    /// 该客户端是否已安装（配置文件存在即认为装了）。
    pub fn detect(&self) -> bool {
        self.config_paths().iter().any(|p| p.exists())
    }

    /// 读当前生效的 base_url，供 UI 展示"现在指向哪"。
    pub fn current_base_url(&self) -> Option<String> {
        match self {
            Self::ClaudeCode => read_claude_base_url(),
            Self::Codex => read_codex_base_url(),
            Self::GeminiCli => read_gemini_base_url(),
        }
    }

    /// 生成接管改动。
    ///
    /// `plan` 是接管时要写进它配置的东西（见 [`ClientPlan`]）。
    pub fn plan_apply(&self, base_url: &str, plan: ClientPlan<'_>) -> AppResult<Vec<FilePatch>> {
        match self {
            Self::ClaudeCode => plan_claude(base_url),
            Self::Codex => plan_codex(base_url, plan),
            Self::GeminiCli => plan_gemini(base_url),
        }
    }

    /// 该客户端是否处于被接管状态（有首次写入备份）。
    pub fn is_taken_over(&self, engine: &TakeoverEngine) -> bool {
        self.config_paths().iter().any(|p| engine.is_taken_over(p))
    }

    /// 该客户端会不会消费 [`ClientPlan`]（模型名 / 目录）。
    ///
    /// 目前只有 Codex —— 另外两个的 `plan_*` 压根不接这个参数（Claude 那边我们反而
    /// 在**清掉**模型覆盖键）。策略变更后的重写据此过滤：少了它，改一次 Codex 的策略
    /// 会顺带把 Claude 配置里用户手加的 `ANTHROPIC_DEFAULT_*` 清掉一遍。
    pub fn consumes_client_plan(&self) -> bool {
        matches!(self, Self::Codex)
    }
}

/// 接管时要写进客户端配置的东西。
///
/// 由 `AppSettings` 按客户端算好再传进来 —— 这里不直接依赖设置，接管计划才能拿固定
/// 输入测试。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientPlan<'a> {
    /// 写进配置的模型名；`None` = 不碰客户端自己选的模型。
    ///
    /// **目前只有 Codex 消费它** —— 那儿的动机是绕开 Responses Lite（见 `plan_codex`）；
    /// Claude Code 那边我们反而是**清掉**模型覆盖键的，真要给它注入得先想清楚和那个
    /// 动作的关系。
    pub model: Option<&'a str>,
    /// 是否把网关的模型目录地址（+ 让目录生效的那两个开关）也写进配置。
    ///
    /// 目录是 Codex 独有的东西，所以同样只有它消费。
    pub catalog: bool,
}

impl<'a> ClientPlan<'a> {
    /// 从设置里读：同一个客户端，接管和重新指向两条路都要用一致的值。
    pub fn from_settings(settings: &'a crate::config::AppSettings, client: &str) -> Self {
        Self {
            model: settings.client_model(client),
            catalog: settings.inject_catalog(),
        }
    }
}

/// 客户端的展示信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {    pub id: String,
    pub name: String,
    pub config_path: String,
    pub detected: bool,
    pub taken_over: bool,
    pub current_base_url: Option<String>,
}

pub fn all_clients() -> Vec<ClientId> {
    vec![ClientId::ClaudeCode, ClientId::Codex, ClientId::GeminiCli]
}

/// 汇总所有客户端的状态。
pub fn describe_all(engine: &TakeoverEngine) -> Vec<ClientInfo> {
    all_clients()
        .into_iter()
        .map(|c| ClientInfo {
            id: c.as_str().to_string(),
            name: c.display_name().to_string(),
            config_path: c
                .primary_config_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            detected: c.detect(),
            taken_over: c.is_taken_over(engine),
            current_base_url: c.current_base_url(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 跟随网关地址
// ---------------------------------------------------------------------------

/// 把**已接管**的客户端重新指向网关当前的地址。
///
/// 网关换了监听地址（改端口、换网卡、端口填 0 时由系统分配）之后，客户端配置里
/// 还留着老地址，用户看到的现象是"改完端口，客户端全连不上"。
///
/// 只碰已被我们改过的客户端：没接管的那些，base_url 是用户自己写的，网关监听在
/// 哪儿跟它没关系 —— 顺手改掉就是越界，而且会毁掉用户手写的配置。
///
/// 返回**真正被改动**的客户端；没有变化时是空数组（绝大多数调用都是这种）。
///
/// 给的是 [`ClientId`] 而不是展示名：调用方要据此判断「Codex 被改了吗 —— 那它的
/// 常驻 app-server 也得重启」（见 `takeover::codex_daemon`），拿名字做字符串比对
/// 太脆。
///
/// `settings` 用来取每个客户端各自要写的模型名（`AppSettings::client_model` 是按客户端
/// 算的 —— 模型策略允许给单个客户端单独指定）。判据**只看地址**：策略变了而地址没变
/// 时这里不动 —— 那是 [`reapply_taken_over`] 的活。
pub fn repoint_taken_over(
    base_url: &str,
    settings: &crate::config::AppSettings,
) -> AppResult<Vec<ClientId>> {
    let engine = TakeoverEngine::new();
    let mut changed = Vec::new();

    for id in all_clients() {
        if !id.is_taken_over(&engine) {
            continue;
        }

        // 判据只看**地址对不对**，不看整份文件。
        //
        // 拿内容逐字节比会把用户自己加的东西（模型覆盖、密钥……）也算成"不一致"，
        // 然后被 `plan_apply` 顺手抹掉 —— 一次启动就悄悄回退掉用户的手改，
        // 比不改还糟。地址一样就什么都不做。
        if id.current_base_url().as_deref() == Some(id.stored_base_url(base_url).as_str()) {
            continue;
        }

        // 重跑一遍接管计划：改动只落在 floor keys 上，用户自己写的键原样保留。
        let plan = ClientPlan::from_settings(settings, id.as_str());
        let patches = id.plan_apply(base_url, plan)?;
        let plan = TakeoverPlan {
            client: id.display_name().to_string(),
            files: Vec::new(),
        };
        // 走 commit 而不是直接写：备份是「还原」的唯一依据，这里必须和接管同一条路径，
        // 否则重新指向之后再点还原就找不到原始文件了。已备份过时 commit 不会覆盖备份。
        engine.commit(&plan, &patches)?;
        changed.push(id);
    }

    Ok(changed)
}

// ---------------------------------------------------------------------------
// 策略变更后重写
// ---------------------------------------------------------------------------

/// 这次设置改动会不会改变「往客户端写什么」。
///
/// 只看 [`ClientPlan`] 真正读的那两样：接管模式，以及每个客户端的模型名
/// （`AppSettings::client_model`）。不看整份设置 —— 改端口、超时、缓存都不该
/// 触发客户端配置重写。
pub fn plan_inputs_differ(
    before: &crate::config::AppSettings,
    after: &crate::config::AppSettings,
) -> bool {
    before.client_model_mode != after.client_model_mode
        || all_clients()
            .iter()
            .any(|c| before.client_model(c.as_str()) != after.client_model(c.as_str()))
}

/// 策略变更后，把**已接管**的客户端按新策略重写一遍。
///
/// 与 [`repoint_taken_over`] 的分工：那个是"网关换地址了"，判据只看地址；这个是
/// "用户改了接管策略 / 模型替换策略" —— 必须在改完那一刻落地。否则用户看到的是
/// 「我选了两个都写，客户端一点变化都没有」，而界面上并没有第二个「接管」按钮
/// （已接管的行显示的是「还原」），等于没有补救手段。
///
/// 只碰**会消费 `ClientPlan`** 的客户端（见 [`ClientId::consumes_client_plan`]），
/// 且产出与现状一致时一个字节都不写 —— 策略开关可能被来回拨，每次都重写一遍会让
/// "这文件是谁改的"变得没法解释。
///
/// `base_url` 是网关**真正在跑**的地址；传 `None`（网关没起）时退回客户端配置里
/// 现存的那个 —— 地址本来就不该在这次改动里变，拿它回写等于保持原样，总好过让
/// 整次改动落空。
pub fn reapply_taken_over(
    base_url: Option<&str>,
    settings: &crate::config::AppSettings,
) -> AppResult<Vec<ClientId>> {
    let engine = TakeoverEngine::new();
    let mut changed = Vec::new();

    for id in all_clients() {
        if !id.consumes_client_plan() || !id.is_taken_over(&engine) {
            continue;
        }

        let target = match base_url {
            Some(url) => url.to_string(),
            None => match id.current_base_url() {
                Some(stored) => id.base_url_from_stored(&stored),
                // 接管过却读不出地址：配置文件被改坏了。不猜地址，跳过。
                None => continue,
            },
        };

        let plan = ClientPlan::from_settings(settings, id.as_str());
        let patches = id.plan_apply(&target, plan)?;

        // 逐字节比：`plan_*` 是**按我们拥有的键**打补丁（其余内容原样保留），所以
        // "产出 ≠ 现状"等价于"我们写进去的某个值不对"，不会把用户自己加的东西
        // 误判成需要重写。
        let mut stale = false;
        for p in &patches {
            if patch::read_optional(&p.path)?.as_deref() != p.content.as_deref() {
                stale = true;
                break;
            }
        }
        if !stale {
            continue;
        }

        let commit_plan = TakeoverPlan {
            client: id.display_name().to_string(),
            files: Vec::new(),
        };
        // 与接管走同一条 commit：备份是「还原」的唯一依据，已备份过时不会覆盖。
        engine.commit(&commit_plan, &patches)?;
        changed.push(id);
    }

    Ok(changed)
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

fn plan_claude(base_url: &str) -> AppResult<Vec<FilePatch>> {
    let path = paths::claude_settings_path();
    // 文件不存在时从空对象开始 —— 这是"新建"而非"解析失败"，可以安全创建。
    let original = patch::read_optional(&path)?.unwrap_or_else(|| b"{}".to_vec());

    let content = patch::patch_json(
        &original,
        &[
            // 先清掉会让客户端绕过网关的键，再写入我们的值。
            JsonOp::Remove {
                path: vec!["env".into(), "ANTHROPIC_API_KEY".into()],
            },
            JsonOp::RemovePrefix {
                parent: vec!["env".into()],
                prefix: "ANTHROPIC_DEFAULT_".into(),
            },
            JsonOp::Remove {
                path: vec!["apiBaseUrl".into()],
            },
            JsonOp::Remove {
                path: vec!["apiKey".into()],
            },
            JsonOp::Set {
                path: vec!["env".into(), "ANTHROPIC_BASE_URL".into()],
                value: json!(base_url),
            },
            // 占位密钥：真实上游密钥在渠道里配，客户端只需别去读环境变量。
            JsonOp::Set {
                path: vec!["env".into(), "ANTHROPIC_AUTH_TOKEN".into()],
                value: json!(floor::LOCAL_PLACEHOLDER_KEY),
            },
        ],
    )?;

    Ok(vec![FilePatch {
        path,
        content: Some(content),
    }])
}

fn read_claude_base_url() -> Option<String> {
    let raw = patch::read_optional(&paths::claude_settings_path()).ok()??;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("env")?
        .get("ANTHROPIC_BASE_URL")?
        .as_str()
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

fn plan_codex(base_url: &str, plan: ClientPlan<'_>) -> AppResult<Vec<FilePatch>> {
    let path = paths::codex_config_path();
    let original = patch::read_optional(&path)?.unwrap_or_default();

    // 刻意不动 ~/.codex/auth.json：那是用户的登录态，删掉会让人下次要重新登录。
    // 自定义 provider 走 experimental_bearer_token，不依赖 auth.json。
    Ok(vec![FilePatch {
        path,
        content: Some(codex_config(&original, base_url, plan)?),
    }])
}

/// [`plan_codex`] 的纯内核：原文件字节 → 改写后的字节。
///
/// 拆出来是为了测试能给固定输入。直接读 `~/.codex/config.toml` 的测试会随**本机**
/// 配置变化 —— 断言看着通过，其实什么都没验。
fn codex_config(
    original: &[u8],
    base_url: &str,
    plan: ClientPlan<'_>,
) -> AppResult<Vec<u8>> {
    let table = format!("model_providers.{}", floor::CODEX_PROVIDER_NAME);
    // Codex 会在 base_url 后面拼 `/responses`，所以这里带上 /v1。
    let provider_base = format!("{}/v1", base_url.trim_end_matches('/'));

    let mut ops = vec![
        TomlOp::SetTop {
            key: "model_provider".into(),
            value: TomlValue::Str(floor::CODEX_PROVIDER_NAME.into()),
        },
        TomlOp::SetInTable {
            table: table.clone(),
            key: "name".into(),
            value: TomlValue::Str("Apilot".into()),
        },
        TomlOp::SetInTable {
            table: table.clone(),
            key: "base_url".into(),
            value: TomlValue::Str(provider_base),
        },
        // Codex 用 Responses 协议跟模型对话；我们网关会做协议转换。
        TomlOp::SetInTable {
            table: table.clone(),
            key: "wire_api".into(),
            value: TomlValue::Str("responses".into()),
        },
        // 网关只讲 HTTP。留着 true 的话，哪天线上一默认打开 websocket，客户端会先
        // 拿 ws 连一次、失败再回落 —— 每次开会话白等一轮。
        TomlOp::SetInTable {
            table: table.clone(),
            key: "supports_websockets".into(),
            value: TomlValue::Bool(false),
        },
        TomlOp::SetInTable {
            // 后面两半还要用这个名字，所以这里只克隆。
            table: table.clone(),
            key: "experimental_bearer_token".into(),
            value: TomlValue::Str(floor::LOCAL_PLACEHOLDER_KEY.into()),
        },
    ];

    // 写模型名是**唯一**能让 Codex 别走 Responses Lite 的省事办法：GPT 系名字在它
    // 内置目录里是 Lite，工具会被塞进 `input[].additional_tools`，而这形状对不少上游
    // 是静默失效的（收下请求、返回 200、工具一个不认）。换成一个它不认识的名字，元数据
    // 退回兜底那份 —— 经典顶层 `tools`，不用联网、不依赖任何开关。
    //
    // 代价要认：兜底元数据不带 `apply_patch`，模型会改用 shell 写文件；而且按模型名配的
    // 路由规则会跟着变。所以这是**用户显式选**的模式，不是默认行为。
    //
    // 反过来的情况（模式不再要名字了）**不删这个键**：那是用户原本选的名字，被我们
    // 覆盖过之后已经拿不回来了，删掉只会退回 Codex 的默认模型 —— 也就是又走 Lite。
    // 想恢复原值只有「还原」（备份里有）。
    if let Some(model) = plan.model {
        ops.push(TomlOp::SetTop {
            key: "model".into(),
            value: TomlValue::Str(model.to_string()),
        });
    }

    // 目录那三行：让 Codex 来取我们这份元数据（**含 `apply_patch`**）。取到就有完整
    // 元数据，取不到它会静默退回内置目录 —— 所以这个模式要和「写模型名」一起用才有
    // 兜底，理由见 `crate::codex`。
    if plan.catalog {
        ops.extend([
            TomlOp::SetInTable {
                table: table.clone(),
                key: "model_catalog_url".into(),
                value: TomlValue::Str(crate::codex::catalog_url(base_url)),
            },
            // 拉 `model_catalog_url` 挂在 `api_key_model_discovery` 后面，不开就永远
            // 不去取，上面那行等于白写（实测才发现，0.160.x 里它还是开发中特性）。
            TomlOp::SetInTable {
                table: "features".into(),
                key: "api_key_model_discovery".into(),
                value: TomlValue::Bool(true),
            },
            // 上一条会被归到「开发中特性」，每开一次会话都提示「可能行为不可预期」。
            // 这个开关只关掉警告本身，不影响它提示别的开发中特性。
            TomlOp::SetTop {
                key: "suppress_unstable_features_warning".into(),
                value: TomlValue::Bool(true),
            },
        ]);
    } else {
        // 模式关掉了就把它们清掉。留着不是"无害的残留"：客户端还在拉我们的目录、一个
        // 开发中特性还开着、一个全局的「别警告我」开关还挂着，而用户在界面上已经关掉了
        // 它 —— 静默的副作用。
        //
        // 取舍说明白：这三个键是我们写进去的，所以关掉时也由我们清掉。万一用户在我们
        // 接管**之前**就自己设过同名键，切回 off 会把他的值一并清掉 —— 这种撞车极罕见，
        // 而且比"留着我们塞进去的东西"更容易解释；真要精确回到原样，用「还原」（它写的
        // 是接管前的原始字节）。
        ops.extend([
            TomlOp::RemoveInTable {
                table,
                key: "model_catalog_url".into(),
            },
            TomlOp::RemoveInTable {
                table: "features".into(),
                key: "api_key_model_discovery".into(),
            },
            TomlOp::RemoveTop {
                key: "suppress_unstable_features_warning".into(),
            },
        ]);
    }

    patch::patch_toml(original, &ops)
}

fn read_codex_base_url() -> Option<String> {
    let raw = patch::read_optional(&paths::codex_config_path()).ok()??;
    let text = String::from_utf8(raw).ok()?;
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    doc.get("model_providers")?
        .get(floor::CODEX_PROVIDER_NAME)?
        .get("base_url")?
        .as_str()
        .map(String::from)
}

// ---------------------------------------------------------------------------
// Gemini CLI
// ---------------------------------------------------------------------------

fn plan_gemini(base_url: &str) -> AppResult<Vec<FilePatch>> {
    let path = paths::gemini_env_path();
    let original = patch::read_optional(&path)?.unwrap_or_default();

    let content = patch::patch_dotenv(
        &original,
        &[
            DotenvOp::Set {
                key: "GOOGLE_GEMINI_BASE_URL".into(),
                value: base_url.to_string(),
            },
            DotenvOp::Set {
                key: "GEMINI_API_KEY".into(),
                value: floor::LOCAL_PLACEHOLDER_KEY.to_string(),
            },
        ],
    )?;

    Ok(vec![FilePatch {
        path,
        content: Some(content),
    }])
}

fn read_gemini_base_url() -> Option<String> {
    let raw = patch::read_optional(&paths::gemini_env_path()).ok()??;
    let text = String::from_utf8(raw).ok()?;
    text.lines().find_map(|l| {
        let t = l.trim();
        t.strip_prefix("GOOGLE_GEMINI_BASE_URL=")
            .map(|v| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_parses_aliases() {
        assert_eq!(ClientId::parse("claude-code"), Some(ClientId::ClaudeCode));
        assert_eq!(ClientId::parse("claude"), Some(ClientId::ClaudeCode));
        assert_eq!(ClientId::parse("codex"), Some(ClientId::Codex));
        assert_eq!(ClientId::parse("gemini-cli"), Some(ClientId::GeminiCli));
        assert_eq!(ClientId::parse("nope"), None);
    }

    #[test]
    fn client_ids_are_unique_and_stable() {
        let ids: Vec<&str> = all_clients().iter().map(|c| c.as_str()).collect();
        assert_eq!(ids, vec!["claude-code", "codex", "gemini-cli"]);
        for id in &ids {
            assert!(ClientId::parse(id).is_some());
        }
    }

    // --- Claude ---

    #[test]
    fn claude_plan_points_base_url_at_gateway() {
        let patches = plan_claude("http://127.0.0.1:8787").unwrap();
        let text = String::from_utf8(patches[0].content.clone().unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(v["env"]["ANTHROPIC_AUTH_TOKEN"], floor::LOCAL_PLACEHOLDER_KEY);
    }

    #[test]
    fn claude_plan_strips_keys_that_would_bypass_the_gateway() {
        let patches = plan_claude("http://127.0.0.1:8787").unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(patches[0].content.as_ref().unwrap()).unwrap();

        assert!(v["env"].get("ANTHROPIC_API_KEY").is_none(), "必须清掉");
        assert!(v.get("apiBaseUrl").is_none(), "必须清掉");
        assert!(v.get("apiKey").is_none(), "必须清掉");
    }

    #[test]
    fn claude_plan_removes_model_override_prefixes() {
        let original = br#"{"env":{"ANTHROPIC_DEFAULT_SONNET_MODEL":"x","KEEP":"y"}}"#;
        let content = patch::patch_json(
            original,
            &[JsonOp::RemovePrefix {
                parent: vec!["env".into()],
                prefix: "ANTHROPIC_DEFAULT_".into(),
            }],
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&content).unwrap();
        assert!(v["env"].get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none());
        assert_eq!(v["env"]["KEEP"], "y");
    }

    // --- Codex ---

    /// 一份像样的用户配置：顶层有模型和思考档位，另有一个别的 provider。
    /// 固定输入是刻意的 —— 直接读本机 `~/.codex/config.toml` 的测试会随环境变化，
    /// 断言看着通过、其实什么都没验。
    const CODEX_ORIGINAL: &[u8] = br#"model_provider = "kt"
model = "gpt-6-sol"
model_reasoning_effort = "medium"

[model_providers.kt]
name = "kt"
base_url = "http://192.168.31.1:3000/v1"
"#;

    fn codex_doc(original: &[u8], plan: ClientPlan<'_>) -> toml_edit::DocumentMut {
        let out = codex_config(original, "http://127.0.0.1:8787", plan).unwrap();
        String::from_utf8(out).unwrap().parse().unwrap()
    }

    /// 只写模型名、不写目录。
    fn model_named(name: &str) -> ClientPlan<'_> {
        ClientPlan {
            model: Some(name),
            catalog: false,
        }
    }

    /// 只写目录、不碰模型名。
    fn with_catalog() -> ClientPlan<'static> {
        ClientPlan {
            model: None,
            catalog: true,
        }
    }

    #[test]
    fn codex_plan_sets_provider_and_wire_api() {
        let doc = codex_doc(CODEX_ORIGINAL, ClientPlan::default());

        assert_eq!(
            doc["model_provider"].as_str(),
            Some(floor::CODEX_PROVIDER_NAME)
        );
        let provider = &doc["model_providers"][floor::CODEX_PROVIDER_NAME];
        assert_eq!(provider["base_url"].as_str(), Some("http://127.0.0.1:8787/v1"));
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        // 网关只讲 HTTP：声明成支持 websocket 会让客户端先白试一次 ws 再回落。
        assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some(floor::LOCAL_PLACEHOLDER_KEY)
        );
        // 开关关着时（这是默认）一个字都不碰模型设置。
        assert_eq!(doc["model"].as_str(), Some("gpt-6-sol"));
    }

    #[test]
    fn codex_plan_writes_the_model_that_keeps_lite_off() {
        // 开关打开时写的这个名字，是**唯一**能省事绕开 Responses Lite 的东西：
        // GPT 系名字在 Codex 内置目录里是 Lite，工具会被塞进 `input[].additional_tools`，
        // 而这形状对不少上游是**静默失效**的（收下请求、返回 200、工具一个不认）。
        // 换成一个它不认识的名字，元数据就退回兜底那份 —— 经典顶层 `tools`，
        // 不联网、不依赖任何客户端开关。
        let doc = codex_doc(CODEX_ORIGINAL, model_named("deepseek-flash"));
        assert_eq!(doc["model"].as_str(), Some("deepseek-flash"));
    }

    #[test]
    fn codex_plan_model_injection_keeps_neighbouring_keys() {
        // 铁律：只动「我拥有」的键。往顶层写 `model` 不能顺手抹掉旁边的设置，
        // 也不能碰用户别的 provider。
        let doc = codex_doc(CODEX_ORIGINAL, model_named("deepseek-flash"));

        assert_eq!(doc["model_reasoning_effort"].as_str(), Some("medium"));
        assert_eq!(
            doc["model_providers"]["kt"]["base_url"].as_str(),
            Some("http://192.168.31.1:3000/v1")
        );
    }

    #[test]
    fn catalog_mode_writes_the_three_keys_and_leaves_the_name_alone() {
        // 「下发目录」是给"想保留客户端自己的模型名、但要有 apply_patch"的人用的：
        // 这三行一个都不能少 —— 少了 `api_key_model_discovery`，Codex 根本不去取那个
        // 地址，上一行等于白写（实测过）。
        let doc = codex_doc(CODEX_ORIGINAL, with_catalog());

        let provider = &doc["model_providers"][floor::CODEX_PROVIDER_NAME];
        assert_eq!(
            provider["model_catalog_url"].as_str(),
            Some("http://127.0.0.1:8787/codex/models")
        );
        assert_eq!(
            doc["features"]["api_key_model_discovery"].as_bool(),
            Some(true)
        );
        assert_eq!(
            doc["suppress_unstable_features_warning"].as_bool(),
            Some(true)
        );
        assert_eq!(doc["model"].as_str(), Some("gpt-6-sol"), "不该动客户端的模型名");
    }

    #[test]
    fn turning_the_catalog_back_off_removes_its_keys() {
        // 留着不是"无害的残留"：客户端还在拉我们的目录、一个开发中特性还开着，而用户
        // 在界面上已经关掉了它。用户自己写在同一张表里的其他键不能被顺手清掉。
        let already = br#"model_provider = "apilot"
model_catalog_url_present = "noop"

[features]
api_key_model_discovery = true
keep_me = true
"#;
        // 先按「开」写进去，模拟用户之前开着。
        let on = codex_config(already, "http://127.0.0.1:8787", with_catalog()).unwrap();
        let then_off = codex_config(&on, "http://127.0.0.1:8787", ClientPlan::default()).unwrap();
        let doc: toml_edit::DocumentMut =
            String::from_utf8(then_off).unwrap().parse().unwrap();

        assert!(doc["model_providers"][floor::CODEX_PROVIDER_NAME]
            .get("model_catalog_url")
            .is_none());
        assert!(doc["features"].get("api_key_model_discovery").is_none());
        assert!(doc.get("suppress_unstable_features_warning").is_none());
        assert_eq!(doc["features"]["keep_me"].as_bool(), Some(true));
        // 顶层的无关键也不能被删模式扫掉。
        assert_eq!(doc["model_catalog_url_present"].as_str(), Some("noop"));
    }

    #[test]
    fn codex_plan_only_touches_config_toml() {
        let patches = plan_codex("http://127.0.0.1:8787", ClientPlan::default()).unwrap();
        assert_eq!(patches.len(), 1, "auth.json 必须不被触碰");
        assert!(patches[0].path.ends_with("config.toml"));
    }

    #[test]
    fn codex_plan_trailing_slash_does_not_double_up() {
        let out = codex_config(CODEX_ORIGINAL, "http://127.0.0.1:8787/", ClientPlan::default()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("http://127.0.0.1:8787/v1"));
        assert!(!text.contains("8787//v1"));
    }

    // --- Gemini ---

    #[test]
    fn gemini_plan_writes_env_vars() {
        let patches = plan_gemini("http://127.0.0.1:8787").unwrap();
        let text = String::from_utf8(patches[0].content.clone().unwrap()).unwrap();
        assert!(text.contains("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:8787"));
        assert!(text.contains(&format!("GEMINI_API_KEY={}", floor::LOCAL_PLACEHOLDER_KEY)));
    }

    #[test]
    fn gemini_plan_keeps_unrelated_env_entries() {
        let original = b"MY_TOKEN=abc\nOTHER=1\n";
        let out = patch::patch_dotenv(
            original,
            &[DotenvOp::Set {
                key: "GOOGLE_GEMINI_BASE_URL".into(),
                value: "http://x".into(),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("MY_TOKEN=abc"));
        assert!(text.contains("OTHER=1"));
    }

    // --- 跟随网关地址 ---

    #[test]
    fn codex_stored_base_url_keeps_the_v1_suffix() {
        // Codex 自己会再拼 `/responses`，所以接管时写进去的是带 /v1 的。
        // 判据也得按带 /v1 的算，否则每次都判定"地址不一致"，白白重写一遍。
        assert_eq!(
            ClientId::Codex.stored_base_url("http://127.0.0.1:8787"),
            "http://127.0.0.1:8787/v1"
        );
        assert_eq!(
            ClientId::Codex.stored_base_url("http://127.0.0.1:8787/"),
            "http://127.0.0.1:8787/v1",
            "尾部斜杠不该拼出 //v1"
        );
        assert_eq!(
            ClientId::ClaudeCode.stored_base_url("http://127.0.0.1:8787"),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            ClientId::GeminiCli.stored_base_url("http://127.0.0.1:8787"),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn stored_base_url_matches_what_the_plans_actually_write() {
        // 「要不要重新指向」的判据就是这个等式。两边一旦漂移，要么每次网关启动
        // 都无谓地重写一遍用户的配置，要么该跟着换的时候不换。
        let url = "http://127.0.0.1:8787";

        let claude = plan_claude(url).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(claude[0].content.as_ref().unwrap()).unwrap();
        assert_eq!(
            v["env"]["ANTHROPIC_BASE_URL"].as_str(),
            Some(ClientId::ClaudeCode.stored_base_url(url).as_str())
        );

        let codex = codex_config(CODEX_ORIGINAL, url, ClientPlan::default()).unwrap();
        let doc: toml_edit::DocumentMut =
            String::from_utf8(codex).unwrap().parse().unwrap();
        assert_eq!(
            doc["model_providers"][floor::CODEX_PROVIDER_NAME]["base_url"].as_str(),
            Some(ClientId::Codex.stored_base_url(url).as_str())
        );

        let gemini = String::from_utf8(plan_gemini(url).unwrap()[0].content.clone().unwrap()).unwrap();
        assert!(gemini.contains(&format!(
            "GOOGLE_GEMINI_BASE_URL={}",
            ClientId::GeminiCli.stored_base_url(url)
        )));
    }

    // --- 路径 ---

    #[test]
    fn config_paths_are_under_home() {
        for c in all_clients() {
            let paths = c.config_paths();
            assert!(!paths.is_empty());
            for p in paths {
                assert!(p.is_absolute() || p.to_string_lossy().contains("Users"));
            }
        }
    }

    #[test]
    fn claude_uses_settings_json() {
        let p = &ClientId::ClaudeCode.config_paths()[0];
        assert!(p.to_string_lossy().contains(".claude"));
        assert!(p.to_string_lossy().ends_with("settings.json"));
    }

    #[test]
    fn codex_uses_config_toml() {
        let p = &ClientId::Codex.config_paths()[0];
        assert!(p.to_string_lossy().contains(".codex"));
        assert!(p.to_string_lossy().ends_with("config.toml"));
    }

    // --- 策略变更后的自动重写 ---

    use crate::config::settings::{ClientModelMode, ModelPolicyMode};

    /// 造一份只关心「往客户端写什么」的设置。
    fn plan_settings(
        model: Option<&str>,
        mode: ModelPolicyMode,
        client_mode: ClientModelMode,
    ) -> crate::config::AppSettings {
        crate::config::AppSettings {
            client_model_mode: client_mode,
            model_policy: crate::config::settings::ModelPolicy {
                mode,
                active_model: model.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn base_url_survives_the_stored_round_trip() {
        // 网关没在跑时 `reapply_taken_over` 靠它把配置里那份取回来当 base_url 用。
        // 取回来的值与原值不一致 = 一次策略变更会把客户端指向错地址。
        // 网关的 base_url 由 host:port 拼成，不带尾斜杠。
        let url = "http://127.0.0.1:8787";
        for id in [ClientId::ClaudeCode, ClientId::Codex, ClientId::GeminiCli] {
            let stored = id.stored_base_url(url);
            assert_eq!(id.base_url_from_stored(&stored), url, "{id:?}");
        }
    }

    #[test]
    fn only_codex_consumes_the_client_plan() {
        // 策略（模型名 / 目录）只写进 Codex 的配置。钉住"策略变更只重写 Codex"，
        // 免得以后给 Claude 注入模型时忘了同时改 `plan_claude` 与这里。
        assert!(ClientId::Codex.consumes_client_plan());
        assert!(!ClientId::ClaudeCode.consumes_client_plan());
        assert!(!ClientId::GeminiCli.consumes_client_plan());
    }

    #[test]
    fn plan_inputs_only_differ_on_what_actually_gets_written() {
        let base = plan_settings(
            Some("deepseek-chat"),
            ModelPolicyMode::Fallback,
            ClientModelMode::Both,
        );

        // 与客户端配置无关的改动：不触发重写。
        let mut unrelated = base.clone();
        unrelated.listen_port = 9999;
        unrelated.cache_enabled = !base.cache_enabled;
        assert!(!plan_inputs_differ(&base, &unrelated));

        // 接管模式变了 → 重写。
        let mut mode = base.clone();
        mode.client_model_mode = ClientModelMode::Off;
        assert!(plan_inputs_differ(&base, &mode));

        // 模型名变了 → 重写（它写进 Codex 的 `model` 键）。
        let mut renamed = base.clone();
        renamed.model_policy.active_model = Some("deepseek-reasoner".into());
        assert!(plan_inputs_differ(&base, &renamed));
    }

    #[test]
    fn a_model_that_is_never_written_does_not_count_as_a_change() {
        // passthrough 下没有"Apilot 指定的模型名"可写（`client_model` 返回 None），
        // 改 active_model 不该触发重写 —— 否则在模型页随便动一下都会重写客户端配置。
        let before = plan_settings(Some("a"), ModelPolicyMode::Passthrough, ClientModelMode::Both);
        let mut after = before.clone();
        after.model_policy.active_model = Some("b".into());
        assert!(!plan_inputs_differ(&before, &after));
    }

    #[test]
    fn reapplying_the_same_plan_is_a_no_op() {
        // 「产出与现状一致就不写」靠的是计划幂等：不幂等的话，改一次策略之后每次
        // 保存设置都会重写一遍客户端配置，而文件里看不出是谁改的。
        let plan = ClientPlan {
            model: Some("m"),
            catalog: true,
        };
        let once = codex_config(b"", "http://127.0.0.1:8787", plan).unwrap();
        let twice = codex_config(&once, "http://127.0.0.1:8787", plan).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn switching_the_strategy_changes_the_bytes() {
        // 反过来：换策略必须产出不同的字节，否则自动重写就是空转。
        let on = codex_config(
            b"",
            "http://127.0.0.1:8787",
            ClientPlan {
                model: Some("m"),
                catalog: true,
            },
        )
        .unwrap();
        let off = codex_config(
            &on,
            "http://127.0.0.1:8787",
            ClientPlan {
                model: Some("m"),
                catalog: false,
            },
        )
        .unwrap();
        assert_ne!(on, off, "关掉目录必须改文件");
    }
}
