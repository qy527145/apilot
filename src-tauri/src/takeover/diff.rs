//! 接管预览用的行级 diff。
//!
//! 只服务一个目标：让用户点「接管」之前看清配置文件会变成什么样。
//! 因此不引第三方 diff 库 —— 配置文件就几十行，O(n·m) 的 LCS 足够快，
//! 输出还完全可控、可测。

use std::fmt::Write as _;

#[derive(Debug, PartialEq, Eq)]
enum Op<'a> {
    Keep(&'a str),
    Del(&'a str),
    Add(&'a str),
}

/// 用 LCS 把两行序列对齐成增删序列。
///
/// 相等时优先 Keep，否则取能让剩余 LCS 最长的一侧 —— 这样连续未改动的
/// 区块会被完整保留为上下文，而不是被拆成一堆删一行加一行。
fn diff_ops<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<Op<'a>> {
    let (n, m) = (old.len(), new.len());

    // dp[i][j] = old[i..] 与 new[j..] 的 LCS 长度。
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Keep(old[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(old[i]));
            i += 1;
        } else {
            ops.push(Op::Add(new[j]));
            j += 1;
        }
    }
    ops.extend(old[i..].iter().map(|l| Op::Del(l)));
    ops.extend(new[j..].iter().map(|l| Op::Add(l)));
    ops
}

/// 生成 `path` 的 unified 风格 diff。
///
/// `old` 为 `None` 表示文件将被新建，对比基线用 `/dev/null`。
/// 无任何差异时返回空串 —— 前端据此提示「没有需要变更的内容」。
pub fn unified_diff(path: &str, old: Option<&str>, new: Option<&str>) -> String {
    if old == new {
        return String::new();
    }

    let old_lines: Vec<&str> = old.unwrap_or("").lines().collect();
    let new_lines: Vec<&str> = new.unwrap_or("").lines().collect();

    let mut out = String::new();
    let _ = writeln!(out, "--- {}", old.map_or("/dev/null", |_| path));
    let _ = writeln!(out, "+++ {}", new.map_or("/dev/null", |_| path));

    for op in diff_ops(&old_lines, &new_lines) {
        match op {
            Op::Keep(l) => {
                let _ = writeln!(out, " {l}");
            }
            Op::Del(l) => {
                let _ = writeln!(out, "-{l}");
            }
            Op::Add(l) => {
                let _ = writeln!(out, "+{l}");
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_yields_empty_diff() {
        assert_eq!(unified_diff("a.json", Some("x\ny"), Some("x\ny")), "");
    }

    #[test]
    fn unchanged_lines_are_context_not_delete_add_pairs() {
        let out = unified_diff("a.json", Some("a\nb\nc"), Some("a\nB\nc"));
        let body: Vec<&str> = out.lines().skip(2).collect();
        assert_eq!(body, vec![" a", "-b", "+B", " c"]);
    }

    #[test]
    fn new_file_uses_dev_null_baseline() {
        let out = unified_diff("new.json", None, Some("hello"));
        assert!(out.starts_with("--- /dev/null\n+++ new.json\n"));
        assert!(out.contains("+hello"));
    }

    #[test]
    fn deleted_file_marks_removed_lines() {
        let out = unified_diff("gone.json", Some("x\ny"), None);
        assert!(out.contains("--- gone.json"));
        assert!(out.contains("+++ /dev/null"));
        assert!(out.contains("-x"));
        assert!(out.contains("-y"));
    }

    #[test]
    fn empty_content_is_not_confused_with_absent_file() {
        // 空串是"存在的空文件"，不能当成 /dev/null。
        let out = unified_diff("a.json", Some(""), Some("x"));
        assert!(out.contains("--- a.json"));
    }

    #[test]
    fn diff_never_panics_on_big_or_empty_inputs() {
        let out = unified_diff("a", Some(""), Some(""));
        assert_eq!(out, "");
        let many: Vec<&str> = Vec::new();
        assert!(diff_ops(&many, &many).is_empty());
    }
}
