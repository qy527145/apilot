//! 接管写入引擎：备份 → 暂存 → 原子替换。
//!
//! 三段式的原因：直接 `fs::write` 一个用户配置文件，一旦写到一半进程被杀，
//! 用户就得到一个截断的 JSON —— Claude Code 下次启动会直接报配置错误。
//! 先写同目录临时文件再 rename，操作系统保证替换是原子的。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::paths;
use crate::error::{AppError, AppResult};

/// 对单个文件的改动。`content` 为 `None` 表示删除该文件。
#[derive(Debug, Clone)]
pub struct FilePatch {
    pub path: PathBuf,
    pub content: Option<Vec<u8>>,
}

/// 一次接管的完整计划。可以先给用户看，再决定是否落盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverPlan {
    pub client: String,
    /// 文件路径 → 改动后的内容（`None` 表示删除）。预览用。
    pub files: Vec<PlannedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFile {
    pub path: String,
    /// 改动后的内容；为空表示删除该文件。
    pub new_content: Option<String>,
    /// 改动前的内容；文件原本不存在时为空。
    pub old_content: Option<String>,
}

/// 执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverResult {
    pub client: String,
    pub applied: bool,
    pub backup_path: Option<String>,
    pub message: String,
}

pub struct TakeoverEngine {
    backup_root: PathBuf,
}

impl TakeoverEngine {
    pub fn new() -> Self {
        Self {
            backup_root: paths::live_first_write_backup_dir(),
        }
    }

    #[cfg(test)]
    pub fn with_backup_root(root: PathBuf) -> Self {
        Self { backup_root: root }
    }

    /// 某个配置文件对应的备份路径。
    ///
    /// 用路径的 sha256 前 12 位做前缀：不同目录下的同名文件（如两处的
    /// `settings.json`）不会互相覆盖。
    fn backup_path_for(&self, path: &Path) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(path.to_string_lossy().as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        self.backup_root.join(format!("{}-{}", &hash[..12], name))
    }

    /// 首次写入前备份原文件。已备份过则跳过。
    ///
    /// 返回备份文件路径；原文件不存在（首次创建）时返回 `None`。
    pub fn ensure_backup(&self, path: &Path) -> AppResult<Option<PathBuf>> {
        let backup = self.backup_path_for(path);

        // 已经备份过就绝不再覆盖 —— 那会把"用户最初的样子"弄丢。
        if backup.exists() {
            return Ok(Some(backup));
        }

        let Some(original) = crate::takeover::patch::read_optional(path)? else {
            // 文件还不存在，没有"原始状态"可备份。
            return Ok(None);
        };

        std::fs::create_dir_all(&self.backup_root)?;
        std::fs::write(&backup, &original)?;

        // 旁挂一个 .source 记录原始路径，方便用户在备份目录里辨认。
        let _ = std::fs::write(
            backup.with_extension("source"),
            path.to_string_lossy().as_bytes(),
        );

        Ok(Some(backup))
    }

    /// 读取某文件被接管前的原始内容。没备份过则返回 `None`。
    pub fn original_content(&self, path: &Path) -> AppResult<Option<Vec<u8>>> {
        let backup = self.backup_path_for(path);
        crate::takeover::patch::read_optional(&backup)
    }

    /// 该文件是否处于"已被我们改过"的状态。
    pub fn is_taken_over(&self, path: &Path) -> bool {
        self.backup_path_for(path).exists()
    }

    /// 删除某文件的备份（含 `.source` 旁挂文件）。
    ///
    /// 备份已不存在时视为成功 —— 还原路径可能被并发或重复调用。
    fn remove_backup(&self, path: &Path) -> AppResult<()> {
        let backup = self.backup_path_for(path);
        match std::fs::remove_file(&backup) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // 旁挂文件只是给人看的，删不掉也不该让还原失败。
        let _ = std::fs::remove_file(backup.with_extension("source"));
        Ok(())
    }

    /// 原子写入：同目录临时文件 → fsync → rename。
    pub fn write_atomic(&self, path: &Path, content: &[u8]) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = dir.join(format!(
            ".{}.apilot.tmp",
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "cfg".into())
        ));

        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content)?;
            // 先落盘再 rename：否则断电后可能 rename 出一个空文件。
            f.sync_all()?;
        }

        rename_with_retry(&tmp, path)?;
        Ok(())
    }

    /// 执行一个计划：逐个文件备份 + 原子写入。
    pub fn commit(&self, plan: &TakeoverPlan, patches: &[FilePatch]) -> AppResult<TakeoverResult> {
        let mut first_backup: Option<PathBuf> = None;

        for p in patches {
            match &p.content {
                Some(content) => {
                    if let Some(b) = self.ensure_backup(&p.path)? {
                        first_backup.get_or_insert(b);
                    }
                    self.write_atomic(&p.path, content)?;
                }
                None => {
                    // 计划要求删除该文件（例如切第三方后清掉 Codex 的登录态）。
                    if p.path.exists() {
                        if let Some(b) = self.ensure_backup(&p.path)? {
                            first_backup.get_or_insert(b);
                        }
                        std::fs::remove_file(&p.path)?;
                    }
                }
            }
        }

        Ok(TakeoverResult {
            client: plan.client.clone(),
            applied: true,
            backup_path: first_backup.map(|p| p.display().to_string()),
            message: format!("已接管 {}，共改动 {} 个文件", plan.client, patches.len()),
        })
    }

    /// 还原：把每个文件写回首次接管前的字节。
    ///
    /// 对从未被改过、也没有备份的文件是空操作。
    pub fn restore(&self, client: &str, paths_to_restore: &[PathBuf]) -> AppResult<TakeoverResult> {
        let mut restored = 0usize;
        let mut backup_path = None;

        for path in paths_to_restore {
            let Some(original) = self.original_content(path)? else {
                // 没备份过说明我们从没改过它，不该凭空"还原"成别的东西。
                continue;
            };
            backup_path.get_or_insert_with(|| self.backup_path_for(path).display().to_string());
            self.write_atomic(path, &original)?;
            // 文件已回到原始字节，备份再无价值；必须删掉它 ——
            // `is_taken_over` 的判据就是"备份存在"，留着它 UI 永远显示"已接管"。
            self.remove_backup(path)?;
            restored += 1;
        }

        let message = if restored == 0 {
            format!("{client} 未被接管过，无需还原")
        } else {
            format!("已还原 {client} 的 {restored} 个配置文件")
        };

        Ok(TakeoverResult {
            client: client.to_string(),
            applied: false,
            backup_path,
            message,
        })
    }
}

/// rename，对 Windows 上杀软/文件占用导致的瞬时失败做有限重试。
fn rename_with_retry(from: &Path, to: &Path) -> AppResult<()> {
    const ATTEMPTS: u32 = 5;
    let mut last_err = None;

    for attempt in 0..ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                // 退避：10ms, 20ms, 40ms, 80ms
                std::thread::sleep(Duration::from_millis(10 << attempt));
            }
        }
    }

    let e = last_err.expect("至少尝试过一次");
    Err(AppError::msg(format!(
        "替换 {} 失败（已重试 {ATTEMPTS} 次）: {e}",
        to.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn engine(dir: &TempDir) -> TakeoverEngine {
        TakeoverEngine::with_backup_root(dir.path().join("backups"))
    }

    #[test]
    fn backup_captures_original_bytes() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let target = dir.path().join("settings.json");
        std::fs::write(&target, br#"{"original":true}"#).unwrap();

        let backup = e.ensure_backup(&target).unwrap().unwrap();
        assert!(backup.exists());
        assert_eq!(
            std::fs::read(&backup).unwrap(),
            br#"{"original":true}"#
        );
        assert!(e.is_taken_over(&target));
    }

    #[test]
    fn backup_is_never_overwritten() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let target = dir.path().join("cfg.json");
        std::fs::write(&target, b"v1").unwrap();
        e.ensure_backup(&target).unwrap();

        // 文件被我们自己改了，再备份不应覆盖第一次的内容
        std::fs::write(&target, b"v2").unwrap();
        e.ensure_backup(&target).unwrap();

        assert_eq!(e.original_content(&target).unwrap().unwrap(), b"v1");
    }

    #[test]
    fn missing_file_has_no_backup() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        assert!(e.ensure_backup(&dir.path().join("nope.json")).unwrap().is_none());
        assert!(!e.is_taken_over(&dir.path().join("nope.json")));
    }

    #[test]
    fn different_paths_get_different_backups() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let a = dir.path().join("x/settings.json");
        let b = dir.path().join("y/settings.json");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, b"AAA").unwrap();
        std::fs::write(&b, b"BBB").unwrap();

        e.ensure_backup(&a).unwrap();
        e.ensure_backup(&b).unwrap();

        assert_eq!(e.original_content(&a).unwrap().unwrap(), b"AAA");
        assert_eq!(e.original_content(&b).unwrap().unwrap(), b"BBB");
    }

    #[test]
    fn write_atomic_replaces_content_without_leaving_temp() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let target = dir.path().join("out.json");
        std::fs::write(&target, b"old").unwrap();

        e.write_atomic(&target, b"new content").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");

        // 临时文件不能残留
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".apilot.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应被 rename 掉");
    }

    #[test]
    fn write_atomic_creates_intermediate_directories() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let target = dir.path().join("deep/nested/dir/cfg.json");

        e.write_atomic(&target, b"x").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
    }

    #[test]
    fn commit_writes_all_files_and_records_backup() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f1 = dir.path().join("a.json");
        let f2 = dir.path().join("b.toml");
        std::fs::write(&f1, b"old-a").unwrap();
        std::fs::write(&f2, b"old-b").unwrap();

        let plan = TakeoverPlan {
            client: "claude".into(),
            files: vec![],
        };
        let patches = vec![
            FilePatch {
                path: f1.clone(),
                content: Some(b"new-a".to_vec()),
            },
            FilePatch {
                path: f2.clone(),
                content: Some(b"new-b".to_vec()),
            },
        ];

        let result = e.commit(&plan, &patches).unwrap();
        assert!(result.applied);
        assert!(result.backup_path.is_some());
        assert_eq!(std::fs::read(&f1).unwrap(), b"new-a");
        assert_eq!(std::fs::read(&f2).unwrap(), b"new-b");
    }

    #[test]
    fn commit_with_none_content_deletes_the_file() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("auth.json");
        std::fs::write(&f, b"token").unwrap();

        let plan = TakeoverPlan {
            client: "codex".into(),
            files: vec![],
        };
        e.commit(
            &plan,
            &[FilePatch {
                path: f.clone(),
                content: None,
            }],
        )
        .unwrap();

        assert!(!f.exists(), "内容为 None 表示删除");
        // 删除前必须留下备份，否则还原不回来
        assert_eq!(e.original_content(&f).unwrap().unwrap(), b"token");
    }

    #[test]
    fn restore_writes_back_original_bytes() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("settings.json");
        std::fs::write(&f, br#"{"user":"original"}"#).unwrap();

        let plan = TakeoverPlan {
            client: "claude".into(),
            files: vec![],
        };
        e.commit(
            &plan,
            &[FilePatch {
                path: f.clone(),
                content: Some(br#"{"taken":"over"}"#.to_vec()),
            }],
        )
        .unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), br#"{"taken":"over"}"#);

        let r = e.restore("claude", &[f.clone()]).unwrap();
        assert!(r.message.contains("已还原"));
        assert_eq!(
            std::fs::read(&f).unwrap(),
            br#"{"user":"original"}"#,
            "必须字节级还原"
        );
    }

    #[test]
    fn restore_is_a_noop_for_files_never_touched() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("untouched.json");
        std::fs::write(&f, b"keep me").unwrap();

        let r = e.restore("claude", &[f.clone()]).unwrap();
        assert!(r.message.contains("无需还原"));
        assert_eq!(std::fs::read(&f).unwrap(), b"keep me", "不该被动过");
    }

    #[test]
    fn restore_after_delete_recreates_the_file() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("auth.json");
        std::fs::write(&f, b"secret").unwrap();

        let plan = TakeoverPlan {
            client: "codex".into(),
            files: vec![],
        };
        e.commit(&plan, &[FilePatch { path: f.clone(), content: None }])
            .unwrap();
        assert!(!f.exists());

        e.restore("codex", &[f.clone()]).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"secret", "被删的文件也要能还原");
    }

    #[test]
    fn restore_clears_taken_over_state() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("settings.json");
        std::fs::write(&f, br#"{"user":"original"}"#).unwrap();

        let plan = TakeoverPlan {
            client: "claude".into(),
            files: vec![],
        };
        e.commit(
            &plan,
            &[FilePatch {
                path: f.clone(),
                content: Some(br#"{"taken":"over"}"#.to_vec()),
            }],
        )
        .unwrap();
        assert!(e.is_taken_over(&f));

        e.restore("claude", &[f.clone()]).unwrap();

        // 备份必须一并删除，否则 UI 永远停在"已接管"（按钮切不回来）。
        assert!(!e.is_taken_over(&f), "还原后不应再算作已接管");
        assert!(e.original_content(&f).unwrap().is_none());
    }

    #[test]
    fn restore_removes_the_source_sidecar() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("cfg.json");
        std::fs::write(&f, b"orig").unwrap();
        e.ensure_backup(&f).unwrap();

        let source = e.backup_path_for(&f).with_extension("source");
        assert!(source.exists());

        e.restore("c", &[f.clone()]).unwrap();
        assert!(!source.exists(), "旁挂的 .source 也要一并清掉");
    }

    #[test]
    fn restore_leaves_unrelated_backups_alone() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        std::fs::write(&a, b"a-orig").unwrap();
        std::fs::write(&b, b"b-orig").unwrap();
        e.ensure_backup(&a).unwrap();
        e.ensure_backup(&b).unwrap();

        e.restore("c", &[a.clone()]).unwrap();

        assert!(!e.is_taken_over(&a));
        assert!(e.is_taken_over(&b), "另一个文件的备份不该被连带删除");
    }

    #[test]
    fn apply_then_restore_roundtrips_byte_exactly() {
        let dir = TempDir::new().unwrap();
        let e = engine(&dir);
        let f = dir.path().join("settings.json");
        let original: &[u8] = b"{\n  \"env\": { \"MY\": \"stuff\" },\n  \"note\": \"\xe4\xb8\xad\xe6\x96\x87\"\n}\n";
        std::fs::write(&f, original).unwrap();

        let plan = TakeoverPlan {
            client: "c".into(),
            files: vec![],
        };
        e.commit(
            &plan,
            &[FilePatch {
                path: f.clone(),
                content: Some(b"{\"changed\":true}".to_vec()),
            }],
        )
        .unwrap();
        e.restore("c", &[f.clone()]).unwrap();

        assert_eq!(std::fs::read(&f).unwrap(), original, "必须逐字节相同");
    }

}
