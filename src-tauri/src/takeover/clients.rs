//! 各客户端的接管逻辑。
//!
//! 每个客户端只知道"怎么把 base_url 指到本地网关"，还原由引擎按备份统一处理 ——
//! 这样新增客户端时不必再写一遍还原逻辑，也就不会写出还原不干净的新 bug。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::engine::{FilePatch, TakeoverEngine};
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
            TomlOp::SetInTable {
                table,
                key: "experimental_bearer_token".into(),
                value: TomlValue::Str(floor::LOCAL_PLACEHOLDER_KEY.into()),
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
