//! 重启 Codex 的常驻 app-server。
//!
//! ## 为什么接管必须管这个
//!
//! Codex 的 TUI / 桌面版**不是自己读配置去发请求**的：它们连到一个常驻的 app-server
//! 进程，provider、`base_url`、模型元数据都是**那个进程启动时**读的。所以接管改完
//! `~/.codex/config.toml` 之后：
//!
//! - 光重启客户端没用 —— 它只是重新连上那个老进程；
//! - 现象极具欺骗性：客户端进程明明是刚起的（看 PID 启动时间就知道），行为却完全是
//!   老配置的。实测踩过：接管后工具还是 Responses Lite，查了半天才发现元凶是那个
//!   13:02 就起来、之后一直没死过的守护进程。
//!
//! `codex exec` 不经过它（日志里是 `rpc.transport="in-process"`），所以那边一直是对的
//! —— 只有 TUI / 桌面版会中招，而用户不可能猜到要去重启一个后台进程。
//!
//! ## 只重启**正在跑**的
//!
//! 没在跑时什么都不做。顺手把一个用户没开的进程拉起来是越界：`codex exec` 的用户
//! 全程不需要它，凭什么凭空多一个常驻进程。

use std::process::Command;

/// 一次重启尝试的结果。
///
/// 刻意不是 `Result`：**重启失败不该让接管失败**。配置文件已经写好了，重启只是让它
/// 立刻生效；失败了用户自己重启客户端也能补救，为此把接管整个判失败反而挡住了主流程。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRestart {
    /// 本来在跑，已经重启。
    Restarted,
    /// 本来就没跑 —— 什么都不用做，客户端下次启动自然会读新配置。
    NotRunning,
    /// 没找到 `codex`、或命令失败。附原因（进日志和给用户看的那句话）。
    Failed(String),
}

impl DaemonRestart {
    /// 给用户看的一句话；没什么可说的（本来就没跑）时是 `None`。
    ///
    /// 只有失败那条需要用户动手，所以只有它把话说全：不说的话，用户看到的就是
    /// 「接管成功但行为没变」，比报错还难查。
    pub fn note(&self) -> Option<String> {
        match self {
            Self::Restarted => Some("Codex 的后台进程已一并重启。".to_string()),
            Self::NotRunning => None,
            Self::Failed(reason) => Some(format!(
                "但没能重启 Codex 的后台进程（{reason}）—— 请手动执行 \
                 `codex app-server daemon restart`，否则 TUI / 桌面版会继续用旧配置。"
            )),
        }
    }
}

/// 重启 Codex 的 app-server（仅当它正在运行）。
pub fn restart_if_running() -> DaemonRestart {
    match run(&["app-server", "daemon", "version"]) {
        Ok(stdout) if !is_running(&stdout) => DaemonRestart::NotRunning,
        Ok(_) => match run(&["app-server", "daemon", "restart"]) {
            Ok(_) => DaemonRestart::Restarted,
            Err(e) => DaemonRestart::Failed(e),
        },
        Err(e) => DaemonRestart::Failed(e),
    }
}

/// `daemon version` 的输出是一行 JSON，跑着时 `status` 是 `running`。
///
/// 认不出来的输出按"没在跑"处理：宁可少做一步，也不要把一个语法变了的新版本
/// 当成"在跑"然后去 restart。
fn is_running(stdout: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .and_then(|v| v.get("status")?.as_str().map(String::from))
        .is_some_and(|s| s == "running")
}

fn run(args: &[&str]) -> Result<String, String> {
    let out = spawn(args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr.trim().chars().take(200).collect();
        return Err(if tail.is_empty() {
            format!("codex {} 退出码 {:?}", args.join(" "), out.status.code())
        } else {
            tail
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn spawn(args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("codex");
    cmd.args(args);
    no_window(&mut cmd);

    match cmd.output() {
        Ok(out) => Ok(out),
        // npm 装的 codex 是 `codex.cmd`，而 `CreateProcess` 只帮我们补 `.exe`，
        // 直接起会找不到。退回 `cmd /C` 让 shell 去解析 PATHEXT。
        Err(first) if cfg!(windows) => {
            let mut fallback = Command::new("cmd");
            fallback.arg("/C").arg("codex").args(args);
            no_window(&mut fallback);
            fallback
                .output()
                .map_err(|second| format!("执行 codex 失败: {first} / {second}"))
        }
        Err(e) => Err(format!("执行 codex 失败: {e}")),
    }
}

/// Windows 上从 GUI 进程拉起控制台程序会闪一个黑框；我们也不需要那个窗口。
#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_running_daemon() {
        // 真机上的形状（0.160.x）。
        let running = r#"{"status":"running","backend":"pid","managedCodexVersion":"0.160.1"}"#;
        assert!(is_running(running));
    }

    #[test]
    fn anything_else_counts_as_not_running() {
        // 认不出来时按"没在跑"处理：宁可少做一步，也不要把语法变了的新版本当成
        // "在跑"然后去 restart 一个用户没开的进程。
        for other in [
            r#"{"status":"stopped"}"#,
            r#"{"status":null}"#,
            "not json at all",
            "",
        ] {
            assert!(!is_running(other), "{other:?} 不该被判成在跑");
        }
    }

    #[test]
    fn only_failure_asks_the_user_to_do_something() {
        // 「本来没跑」不该在接管成功的提示里刷存在感 —— 那种情况用户没有任何事要做。
        assert_eq!(DaemonRestart::NotRunning.note(), None);
        assert!(DaemonRestart::Restarted.note().is_some());
        let failed = DaemonRestart::Failed("找不到 codex".into()).note().unwrap();
        // 失败那条必须给出可照做的命令，否则用户只看到"行为没变"。
        assert!(failed.contains("codex app-server daemon restart"));
    }
}
