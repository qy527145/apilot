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
    pub fn plan_apply(&self, base_url: &str) -> AppResult<Vec<FilePatch>> {
        match self {
            Self::ClaudeCode => plan_claude(base_url),
            Self::Codex => plan_codex(base_url),
            Self::GeminiCli => plan_gemini(base_url),
        }
    }

    /// 该客户端是否处于被接管状态（有首次写入备份）。
    pub fn is_taken_over(&self, engine: &TakeoverEngine) -> bool {
        self.config_paths().iter().any(|p| engine.is_taken_over(p))
    }
}

/// 客户端的展示信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub id: String,
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
/// 返回**真正被改动**的客户端名；没有变化时是空数组（绝大多数调用都是这种）。
pub fn repoint_taken_over(base_url: &str) -> AppResult<Vec<String>> {
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
        let patches = id.plan_apply(base_url)?;
        let plan = TakeoverPlan {
            client: id.display_name().to_string(),
            files: Vec::new(),
        };
        // 走 commit 而不是直接写：备份是「还原」的唯一依据，这里必须和接管同一条路径，
        // 否则重新指向之后再点还原就找不到原始文件了。已备份过时 commit 不会覆盖备份。
        engine.commit(&plan, &patches)?;
        changed.push(id.display_name().to_string());
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

fn plan_codex(base_url: &str) -> AppResult<Vec<FilePatch>> {
    let path = paths::codex_config_path();
    let original = patch::read_optional(&path)?.unwrap_or_default();

    let table = format!("model_providers.{}", floor::CODEX_PROVIDER_NAME);
    // Codex 会在 base_url 后面拼 `/responses`，所以这里带上 /v1。
    let provider_base = format!("{}/v1", base_url.trim_end_matches('/'));

    let content = patch::patch_toml(
        &original,
        &[
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
            // 让 Codex 来我们这儿取模型元数据，而不是用它内置那份 —— 内置那份把
            // gpt-6-sol 一类的模型标成「Responses Lite」，工具会被塞进
            // `input[].additional_tools`，而这形状对上游来说是**静默失效**的：
            // 收下请求、返回 200，工具一个不认，模型只能把调用写成 DSML 正文。
            // 详见 `crate::codex`。
            TomlOp::SetInTable {
                table: table.clone(),
                key: "model_catalog_url".into(),
                value: TomlValue::Str(crate::codex::catalog_url(base_url)),
            },
            // 留 false 的话 Codex 会先拿 websocket 连一次，失败再回落 —— 每次开
            // 会话都白等一轮。网关只讲 HTTP。
            TomlOp::SetInTable {
                table: table.clone(),
                key: "supports_websockets".into(),
                value: TomlValue::Bool(false),
            },
            TomlOp::SetInTable {
                table,
                key: "experimental_bearer_token".into(),
                value: TomlValue::Str(floor::LOCAL_PLACEHOLDER_KEY.into()),
            },
            // 拉 `model_catalog_url` 这件事在 Codex 里挂在 `api_key_model_discovery`
            // 这个开关后面，不开就永远不去取，上面的地址等于白写。
            TomlOp::SetInTable {
                table: "features".into(),
                key: "api_key_model_discovery".into(),
                value: TomlValue::Bool(true),
            },
            // 上一条会被 Codex 归到「开发中特性」，每开一次会话都提示一遍
            // 「可能行为不可预期」。这个开关不开用户就得天天看这句与己无关的警告；
            // 开了就只关掉警告本身，不影响它提示别的开发中特性。
            TomlOp::SetTop {
                key: "suppress_unstable_features_warning".into(),
                value: TomlValue::Bool(true),
            },
        ],
    )?;

    // 刻意不动 ~/.codex/auth.json：那是用户的登录态，删掉会让人下次要重新登录。
    // 自定义 provider 走 experimental_bearer_token，不依赖 auth.json。
    Ok(vec![FilePatch {
        path,
        content: Some(content),
    }])
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

    #[test]
    fn codex_plan_sets_provider_and_wire_api() {
        let patches = plan_codex("http://127.0.0.1:8787").unwrap();
        let text = String::from_utf8(patches[0].content.clone().unwrap()).unwrap();
        let doc: toml_edit::DocumentMut = text.parse().unwrap();

        assert_eq!(
            doc["model_provider"].as_str(),
            Some(floor::CODEX_PROVIDER_NAME)
        );
        let provider = &doc["model_providers"][floor::CODEX_PROVIDER_NAME];
        assert_eq!(provider["base_url"].as_str(), Some("http://127.0.0.1:8787/v1"));
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(
            provider["experimental_bearer_token"].as_str(),
            Some(floor::LOCAL_PLACEHOLDER_KEY)
        );
    }

    #[test]
    fn codex_plan_makes_the_client_stop_using_responses_lite() {
        // 接管的核心目的之一：让 Codex 来取我们这份目录，而目录里所有模型都关掉了
        // Responses Lite。它内置那份把 gpt-6-sol 标成 Lite，工具就被塞进
        // `input[].additional_tools` —— 那个形状对不少上游是**静默失效**的
        // （收下请求、返回 200，工具一个不认），模型只能把调用写成 DSML 正文。
        let patches = plan_codex("http://127.0.0.1:8787").unwrap();
        let text = String::from_utf8(patches[0].content.clone().unwrap()).unwrap();
        let doc: toml_edit::DocumentMut = text.parse().unwrap();

        let provider = &doc["model_providers"][floor::CODEX_PROVIDER_NAME];
        // 目录不在 /v1 底下 —— 那是对话接口的前缀，带上会拼成 /v1/codex/models。
        assert_eq!(
            provider["model_catalog_url"].as_str(),
            Some("http://127.0.0.1:8787/codex/models")
        );
        assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
        // 不打开这个开关，Codex 根本不会去取 model_catalog_url，上面那行等于白写。
        assert_eq!(
            doc["features"]["api_key_model_discovery"].as_bool(),
            Some(true)
        );
        assert_eq!(
            doc["suppress_unstable_features_warning"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn codex_plan_keeps_the_users_own_feature_flags() {
        // 铁律：只动「我拥有」的键。用户自己开过的 feature 不能被我们往
        // `[features]` 里写的那一个键顺手清掉。
        let original = b"[features]\nsome_other_flag = true\n";
        let out = patch::patch_toml(
            original,
            &[TomlOp::SetInTable {
                table: "features".into(),
                key: "api_key_model_discovery".into(),
                value: TomlValue::Bool(true),
            }],
        )
        .unwrap();
        let doc: toml_edit::DocumentMut = String::from_utf8(out).unwrap().parse().unwrap();
        assert_eq!(doc["features"]["some_other_flag"].as_bool(), Some(true));
        assert_eq!(
            doc["features"]["api_key_model_discovery"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn codex_plan_only_touches_config_toml() {
        let patches = plan_codex("http://127.0.0.1:8787").unwrap();
        assert_eq!(patches.len(), 1, "auth.json 必须不被触碰");
        assert!(patches[0].path.ends_with("config.toml"));
    }

    #[test]
    fn codex_plan_trailing_slash_does_not_double_up() {
        let patches = plan_codex("http://127.0.0.1:8787/").unwrap();
        let text = String::from_utf8(patches[0].content.clone().unwrap()).unwrap();
        assert!(text.contains("http://127.0.0.1:8787/v1"));
        assert!(!text.contains("8787//v1"));
        // 目录地址同理：尾斜杠不能拼出 `//codex`，那在客户端是 404。
        assert!(text.contains("http://127.0.0.1:8787/codex/models"));
        assert!(!text.contains("8787//codex"));
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

        let codex = plan_codex(url).unwrap();
        let doc: toml_edit::DocumentMut =
            String::from_utf8(codex[0].content.clone().unwrap()).unwrap().parse().unwrap();
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
}
