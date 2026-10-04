//! 保序补丁器：只改指定键，其余内容原样保留。
//!
//! 三种格式各有一个实现，但共用一条铁律：**解析失败立即中止**。
//! 一个解析不了的文件意味着我们读不懂它，此时任何写入都是在赌 ——
//! 最坏情况是把用户几周的配置覆盖成空文档。

use std::path::Path;

use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::error::{AppError, AppResult};

/// 一次 JSON 修改。
#[derive(Debug, Clone)]
pub enum JsonOp {
    /// 设置嵌套路径上的值，中间层不存在则创建。
    Set { path: Vec<String>, value: Value },
    /// 删除嵌套路径。路径不存在时是空操作。
    Remove { path: Vec<String> },
    /// 删除某个对象下所有以 `prefix` 开头的键。
    RemovePrefix { parent: Vec<String>, prefix: String },
}

/// 对 JSON 文本做最小改动。
pub fn patch_json(original: &[u8], ops: &[JsonOp]) -> AppResult<Vec<u8>> {
    let text = std::str::from_utf8(original)
        .map_err(|e| AppError::PatchAborted {
            path: "<json>".into(),
            reason: format!("文件不是合法 UTF-8: {e}"),
        })?;

    let mut doc: Value = serde_json::from_str(text).map_err(|e| AppError::PatchAborted {
        path: "<json>".into(),
        reason: format!("JSON 解析失败（已中止写入以保护原文件）: {e}"),
    })?;

    if !doc.is_object() {
        return Err(AppError::PatchAborted {
            path: "<json>".into(),
            reason: "顶层不是对象，拒绝改写".into(),
        });
    }

    for op in ops {
        apply_json_op(&mut doc, op);
    }

    serde_json::to_vec_pretty(&doc).map_err(AppError::from)
}

fn apply_json_op(doc: &mut Value, op: &JsonOp) {
    match op {
        JsonOp::Set { path, value } => {
            if path.is_empty() {
                return;
            }
            let mut cur = doc;
            for key in &path[..path.len() - 1] {
                if !cur.get(key).map(|v| v.is_object()).unwrap_or(false) {
                    // 中间层不存在或不是对象：建一个空对象顶上。
                    // 强行覆盖非对象会让用户丢失那一层的原有数据。
                    if let Some(obj) = cur.as_object_mut() {
                        obj.insert(key.clone(), Value::Object(Default::default()));
                    }
                }
                cur = cur.get_mut(key).expect("刚刚插入过");
            }
            if let Some(obj) = cur.as_object_mut() {
                obj.insert(path[path.len() - 1].clone(), value.clone());
            }
        }

        JsonOp::Remove { path } => {
            if path.is_empty() {
                return;
            }
            let mut cur = doc;
            for key in &path[..path.len() - 1] {
                match cur.get_mut(key) {
                    Some(v) => cur = v,
                    None => return,
                }
            }
            if let Some(obj) = cur.as_object_mut() {
                obj.remove(&path[path.len() - 1]);
            }
        }

        JsonOp::RemovePrefix { parent, prefix } => {
            let mut cur = doc;
            for key in parent {
                match cur.get_mut(key) {
                    Some(v) => cur = v,
                    None => return,
                }
            }
            if let Some(obj) = cur.as_object_mut() {
                let doomed: Vec<String> = obj
                    .keys()
                    .filter(|k| k.starts_with(prefix.as_str()))
                    .cloned()
                    .collect();
                for k in doomed {
                    obj.remove(&k);
                }
            }
        }
    }
}

/// 一次 TOML 修改。
#[derive(Debug, Clone)]
pub enum TomlOp {
    /// 设置顶层键。
    SetTop { key: String, value: TomlValue },
    /// 设置某个表下的键。点号路径（`a.b`）会被逐级解析。
    SetInTable {
        table: String,
        key: String,
        value: TomlValue,
    },
}

#[derive(Debug, Clone)]
pub enum TomlValue {
    Str(String),
}

impl TomlValue {
    fn into_item(self) -> Item {
        match self {
            Self::Str(s) => toml_edit::value(s),
        }
    }
}

/// 对 TOML 文本做最小改动。用 `toml_edit` 保留注释与键顺序。
pub fn patch_toml(original: &[u8], ops: &[TomlOp]) -> AppResult<Vec<u8>> {
    let text = std::str::from_utf8(original).map_err(|e| AppError::PatchAborted {
        path: "<toml>".into(),
        reason: format!("文件不是合法 UTF-8: {e}"),
    })?;

    let mut doc: DocumentMut = text.parse().map_err(|e| AppError::PatchAborted {
        path: "<toml>".into(),
        reason: format!("TOML 解析失败（已中止写入以保护原文件）: {e}"),
    })?;

    for op in ops {
        match op {
            TomlOp::SetTop { key, value } => {
                doc[key.as_str()] = value.clone().into_item();
            }
            TomlOp::SetInTable { table, key, value } => {
                let t = get_or_create_table(doc.as_table_mut(), &split_path(table));
                t[key.as_str()] = value.clone().into_item();
            }
        }
    }

    Ok(doc.to_string().into_bytes())
}

/// 按 `.` 切分表路径。
///
/// `toml_edit` 的 `Index<&str>` **不**解析点号，`doc["a.b"]` 会造出一个名字里
/// 带点的字面量键。所以所有点号路径都必须自己逐级走。
fn split_path(path: &str) -> Vec<&str> {
    path.split('.').filter(|s| !s.is_empty()).collect()
}

/// 逐级取子表，缺失的层就地创建。
fn get_or_create_table<'a>(table: &'a mut Table, parts: &[&str]) -> &'a mut Table {
    let Some((head, rest)) = parts.split_first() else {
        return table;
    };

    // 已存在但不是表（例如被写成了字符串），也只能覆盖成表 ——
    // 这里没有更好的选择，且这种冲突本就是用户配置有误。
    if !table.get(head).map(|i| i.is_table()).unwrap_or(false) {
        table[*head] = Item::Table(Table::new());
    }

    let sub = table[*head]
        .as_table_mut()
        .expect("上面刚确保过它是表");
    get_or_create_table(sub, rest)
}

/// 一次 dotenv 修改。
#[derive(Debug, Clone)]
pub enum DotenvOp {
    Set { key: String, value: String },
}

/// 对 `.env` 文本做最小改动。
///
/// 逐行处理而非解析成 map 再重建：这样能保留注释、空行与键的顺序。
pub fn patch_dotenv(original: &[u8], ops: &[DotenvOp]) -> AppResult<Vec<u8>> {
    let text = std::str::from_utf8(original).map_err(|e| AppError::PatchAborted {
        path: "<dotenv>".into(),
        reason: format!("文件不是合法 UTF-8: {e}"),
    })?;

    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let had_trailing_newline = text.ends_with('\n');

    for op in ops {
        match op {
            DotenvOp::Set { key, value } => {
                let idx = lines.iter().position(|l| dotenv_key_of(l).as_deref() == Some(key));
                let rendered = format!("{key}={value}");
                match idx {
                    Some(i) => lines[i] = rendered,
                    None => lines.push(rendered),
                }
            }
        }
    }

    let mut out = lines.join("\n");
    if had_trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// 取一行 dotenv 的键名；注释行与空行返回 `None`。
fn dotenv_key_of(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let without_export = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, _) = without_export.split_once('=')?;
    Some(key.trim().to_string())
}

/// 读文件；不存在时返回 `None`（而非报错）。
pub fn read_optional(path: &Path) -> AppResult<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- JSON ---

    #[test]
    fn json_set_preserves_other_keys() {
        let original = br#"{"env":{"MY_VAR":"keep"},"permissions":["a"],"other":1}"#;
        let out = patch_json(
            original,
            &[JsonOp::Set {
                path: vec!["env".into(), "ANTHROPIC_BASE_URL".into()],
                value: json!("http://127.0.0.1:8787"),
            }],
        )
        .unwrap();

        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
        assert_eq!(v["env"]["MY_VAR"], "keep", "用户的其他键必须原样保留");
        assert_eq!(v["permissions"][0], "a");
        assert_eq!(v["other"], 1);
    }

    #[test]
    fn json_set_creates_missing_intermediate_objects() {
        let original = br#"{}"#;
        let out = patch_json(
            original,
            &[JsonOp::Set {
                path: vec!["a".into(), "b".into(), "c".into()],
                value: json!(1),
            }],
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["a"]["b"]["c"], 1);
    }

    #[test]
    fn json_remove_is_noop_when_absent() {
        let original = br#"{"a":1}"#;
        let out = patch_json(
            original,
            &[JsonOp::Remove {
                path: vec!["nope".into()],
            }],
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn json_remove_prefix_deletes_matching_keys_only() {
        let original = br#"{"env":{
            "ANTHROPIC_BASE_URL":"x",
            "ANTHROPIC_DEFAULT_SONNET_MODEL":"m",
            "ANTHROPIC_DEFAULT_OPUS_MODEL":"m",
            "KEEP_ME":"yes"
        }}"#;
        let out = patch_json(
            original,
            &[JsonOp::RemovePrefix {
                parent: vec!["env".into()],
                prefix: "ANTHROPIC_DEFAULT_".into(),
            }],
        )
        .unwrap();

        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["env"].get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none());
        assert!(v["env"].get("ANTHROPIC_DEFAULT_OPUS_MODEL").is_none());
        assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "x");
        assert_eq!(v["env"]["KEEP_ME"], "yes");
    }

    #[test]
    fn json_parse_failure_aborts_without_producing_output() {
        // 这是最重要的一条：绝不能把坏文件"修好"成空文档
        let broken = br#"{"env": {"unclosed": "#;
        let err = patch_json(
            broken,
            &[JsonOp::Set {
                path: vec!["env".into(), "X".into()],
                value: json!("y"),
            }],
        )
        .unwrap_err();
        assert!(matches!(err, AppError::PatchAborted { .. }));
        assert!(err.to_string().contains("已中止写入"));
    }

    #[test]
    fn json_rejects_non_object_toplevel() {
        assert!(patch_json(b"[]", &[]).is_err());
        assert!(patch_json(b"\"just a string\"", &[]).is_err());
    }

    #[test]
    fn json_rejects_non_utf8() {
        assert!(patch_json(&[0xFF, 0xFE], &[]).is_err());
    }

    #[test]
    fn json_key_order_is_preserved() {
        let original = br#"{"z":1,"a":2,"m":3}"#;
        let out = patch_json(
            original,
            &[JsonOp::Set {
                path: vec!["a".into()],
                value: json!(99),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let z = text.find("\"z\"").unwrap();
        let a = text.find("\"a\"").unwrap();
        let m = text.find("\"m\"").unwrap();
        assert!(z < a && a < m, "键顺序不应被打乱: {text}");
    }

    // --- TOML ---

    #[test]
    fn toml_set_top_and_table_preserves_comments() {
        let original = b"# \xe6\x88\x91\xe7\x9a\x84\xe6\xb3\xa8\xe9\x87\x8a\nmodel = \"gpt-5\"\n";
        let out = patch_toml(
            original,
            &[
                TomlOp::SetTop {
                    key: "model_provider".into(),
                    value: TomlValue::Str("apilot".into()),
                },
                TomlOp::SetInTable {
                    table: "model_providers.apilot".into(),
                    key: "base_url".into(),
                    value: TomlValue::Str("http://127.0.0.1:8787/v1".into()),
                },
            ],
        )
        .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("我的注释"), "注释必须保留: {text}");
        assert!(text.contains("model = \"gpt-5\""));
        assert!(text.contains("model_provider = \"apilot\""));
        assert!(text.contains("base_url = \"http://127.0.0.1:8787/v1\""));
    }

    #[test]
    fn toml_parse_failure_aborts() {
        let broken = b"this is = = not toml [[[";
        let err = patch_toml(broken, &[]).unwrap_err();
        assert!(matches!(err, AppError::PatchAborted { .. }));
    }

    // --- dotenv ---

    #[test]
    fn dotenv_set_appends_when_absent() {
        let original = b"# comment\nEXISTING=1\n";
        let out = patch_dotenv(
            original,
            &[DotenvOp::Set {
                key: "GEMINI_API_KEY".into(),
                value: "local".into(),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("# comment"));
        assert!(text.contains("EXISTING=1"));
        assert!(text.contains("GEMINI_API_KEY=local"));
    }

    #[test]
    fn dotenv_set_replaces_in_place() {
        let original = b"GEMINI_API_KEY=old\nOTHER=1\n";
        let out = patch_dotenv(
            original,
            &[DotenvOp::Set {
                key: "GEMINI_API_KEY".into(),
                value: "new".into(),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("GEMINI_API_KEY=new"));
        assert!(!text.contains("old"));
        // 替换而非追加，保证不会出现重复键
        assert_eq!(text.matches("GEMINI_API_KEY").count(), 1);
    }

    #[test]
    fn dotenv_ignores_commented_out_keys() {
        // `# GEMINI_API_KEY=x` 是注释，不该被当成已存在的键而改写
        let original = b"# GEMINI_API_KEY=old\nA=1\n";
        let out = patch_dotenv(
            original,
            &[DotenvOp::Set {
                key: "GEMINI_API_KEY".into(),
                value: "new".into(),
            }],
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("# GEMINI_API_KEY=old"), "注释行必须原样保留");
        assert!(text.contains("GEMINI_API_KEY=new"));
    }

    #[test]
    fn dotenv_handles_export_prefix_and_spaces() {
        assert_eq!(dotenv_key_of("export FOO = 1"), Some("FOO".into()));
        assert_eq!(dotenv_key_of("  BAR=2"), Some("BAR".into()));
        assert_eq!(dotenv_key_of("# NOPE=1"), None);
        assert_eq!(dotenv_key_of(""), None);
        assert_eq!(dotenv_key_of("no equals sign"), None);
    }

    #[test]
    fn dotenv_preserves_trailing_newline_convention() {
        let with = patch_dotenv(b"A=1\n", &[]).unwrap();
        assert!(String::from_utf8(with).unwrap().ends_with('\n'));

        let without = patch_dotenv(b"A=1", &[]).unwrap();
        assert!(!String::from_utf8(without).unwrap().ends_with('\n'));
    }

    #[test]
    fn dotenv_rejects_non_utf8() {
        assert!(patch_dotenv(&[0xFF], &[]).is_err());
    }
}
