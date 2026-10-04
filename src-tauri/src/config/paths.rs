//! 路径解析。
//!
//! 统一用 `dirs::home_dir()`（Windows 走 SHGetKnownFolderPath，尊重 USERPROFILE），
//! **不读 `HOME` 环境变量** —— 参考 cc-switch 的做法，避免 Git Bash / WSL 下 home 错位。

use std::path::PathBuf;

/// 测试与高级用法可覆盖数据根目录。
const APILOT_HOME_ENV: &str = "APILOT_HOME";

/// Apilot 的数据根目录，默认 `~/.apilot`。
pub fn apilot_home() -> PathBuf {
    resolve_apilot_home(std::env::var_os(APILOT_HOME_ENV))
}

/// `apilot_home` 的纯函数内核。
///
/// 把环境变量作为参数传入，是为了让测试不必读写进程级环境 —— 那会让并行测试互相干扰。
fn resolve_apilot_home(env_override: Option<std::ffi::OsString>) -> PathBuf {
    match env_override {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir().join(".apilot"),
    }
}

/// 用户主目录。
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// SQLite 数据库文件。
pub fn db_path() -> PathBuf {
    apilot_home().join("apilot.db")
}

/// 备份根目录。
pub fn backups_dir() -> PathBuf {
    apilot_home().join("backups")
}

/// 接管写入前的字节级首次备份目录。
///
/// 每个客户端文件仅备份一次，保证「一键还原」始终能回到用户最初的样子。
pub fn live_first_write_backup_dir() -> PathBuf {
    backups_dir().join("live-first-write")
}

// ---------------------------------------------------------------------------
// 客户端配置文件路径
// ---------------------------------------------------------------------------

/// Claude Code 主配置：`~/.claude/settings.json`
pub fn claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

/// Codex 配置：`~/.codex/config.toml`
pub fn codex_config_path() -> PathBuf {
    home_dir().join(".codex").join("config.toml")
}

/// Gemini CLI 环境变量文件：`~/.gemini/.env`
pub fn gemini_env_path() -> PathBuf {
    home_dir().join(".gemini").join(".env")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apilot_home_respects_env_override() {
        // 显式覆盖时必须原样返回，这是测试隔离与自定义安装位置的基础。
        let resolved = resolve_apilot_home(Some("/tmp/apilot-test-home".into()));
        assert_eq!(resolved, PathBuf::from("/tmp/apilot-test-home"));
    }

    #[test]
    fn apilot_home_ignores_empty_override() {
        // 空字符串等同于未设置，避免启动脚本里 `set APILOT_HOME=` 把数据目录
        // 变成当前工作目录。
        let resolved = resolve_apilot_home(Some("".into()));
        assert!(resolved.ends_with(".apilot"));
    }

    #[test]
    fn apilot_home_falls_back_to_dot_apilot() {
        let resolved = resolve_apilot_home(None);
        assert!(resolved.ends_with(".apilot"));
        assert_eq!(resolved, home_dir().join(".apilot"));
    }

    #[test]
    fn client_paths_are_under_home() {
        assert!(claude_settings_path().ends_with(".claude/settings.json")
            || claude_settings_path().ends_with(".claude\\settings.json"));
        assert!(codex_config_path().ends_with("config.toml"));
    }
}
