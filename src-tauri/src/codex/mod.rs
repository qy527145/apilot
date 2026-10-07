//! 发给 Codex 客户端的模型目录。
//!
//! ## 先说清楚：接管**不会**自动用它
//!
//! 默认方案是**给客户端换一个它不认识的模型名**（`takeover/clients.rs::codex_config`）——
//! 一行配置、不用联网、不依赖任何客户端开关，效果是元数据退回 Codex 自己的兜底那份，
//! 也就不是 Lite。代价是兜底元数据不带 `apply_patch`。
//!
//! 这个模块是给「想要完整元数据、包括 `apply_patch`」的人**手动**用的：自己在该
//! provider 下配 `model_catalog_url`（见 [`CATALOG_PATH`]）。早先接管流程自动写过一版，
//! 要同时注入两个 feature 开关才生效、侵入性太大，撤了。
//!
//! ## 目录分两半，缺一不可
//!
//! - **内置那半**（`assets/codex/models.json`，逐条关掉 Lite）：管 GPT 系名字。
//! - **Apilot 自己那半**（[`entry`]，为 Apilot 会给客户端用的模型名各生成一条）：
//!   管「接管时写入当前模型」写的那些名字。
//!
//! 目录是**按名字**生效的 —— 客户端用哪个名字，就查哪条。少了第二半，只要客户端用的
//! 不是 GPT 系名字，就会落到 Codex 自己的兜底元数据上（没有 `apply_patch`），于是
//! 「配了目录」和「换了模型名」互相抵消。
//!
//! ## 为什么需要它
//!
//! Codex 用不用「Responses Lite」**只由模型元数据里的 `use_responses_lite` 决定**：
//! 为 `true` 时它把工具全塞进 `input[].additional_tools`（形状是 `namespace > custom`），
//! 顶层 `tools` 置空。而这份元数据来自 Codex 自己的模型目录，`gpt-6-sol` 这类 GPT 系
//! 名字内置就是 `true`。
//!
//! 麻烦在于：不少上游对这个形状**收下请求、返回 200，却完全不解析**（DeepSeek 的
//! `/v1/responses` 就是）。网关这边直通，看不出任何异常；模型那侧手里一个工具都没有，
//! 只能把调用写成 `<｜DSML｜｜ calls>` 这样的正文吐出来 —— 工具调用退化成纯文本，
//! 客户端看起来就是「模型不肯用工具」。
//!
//! 这个模块把目录接管过来：内置目录逐条搬来、把 Lite 关掉，再回给客户端
//! （见 [`catalog`]）。客户端因此换成经典顶层 `tools`，连 DeepSeek 都能正常收下。
//!
//! ## 三个字段必须一起改
//!
//! 只改一个都会坏，都是实测踩出来的：
//!
//! - **`use_responses_lite = false`** —— 工具改走顶层 `tools`。
//! - **`tool_mode = "direct"`** —— 原本被 Lite 关住的那批条目，工具集是 *code mode* 的
//!   （`exec` 是个自由格式工具）。只关 Lite 不改模式的话，这个 `custom` 工具会跑到
//!   顶层；而上游普遍只接受顶层 `custom` 里的 `apply_patch`，其余直接 400。
//! - **提示词换成 [`GENERIC_PROMPT`]** —— 那批条目的提示词是为 code mode 写的（里面在
//!   教模型调 `functions.exec`），工具集一换就和工具对不上了。`prompt.md` 本来就是配
//!   经典工具集用的，也是 Codex 遇到不认识的模型时自己用的那份。
//!
//! 另外把 `apply_patch_tool_type` 钉成 `freeform`：提示词在教模型用 `apply_patch`，
//! 工具就得在。上游对顶层 `custom` 的 `apply_patch` 是普遍接受的（实测 DeepSeek 也收）。
//!
//! ## 不动的东西
//!
//! 托管工具（`web_search`）**故意留着**：它是上游自己执行的服务端工具，能不能用由
//! 上游决定。不认识它的上游（DeepSeek）会像忽略 `additional_tools` 一样忽略它，
//! 认识它的中转则白捡一个能力，两种都不需要我们插手。

use std::collections::HashSet;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

/// 内置的那份 Codex 模型目录。随 Codex 版本更新，见 `assets/codex/PROVENANCE.md`。
const MODELS_JSON: &str = include_str!("../../assets/codex/models.json");

/// Codex 配经典工具集用的通用提示词。
const GENERIC_PROMPT: &str = include_str!("../../assets/codex/prompt.md");

/// 客户端来取目录的路径。
///
/// 刻意不复用 `/v1/models`：那个是 OpenAI 家列模型的公共接口，客户端会按自己的
/// 理解解析；这里是 Codex 专用的元数据，混在一起迟早有一头被改坏。
///
/// **接管流程不会把这个地址写进客户端配置。** 早先写过一版：自动写入
/// `model_catalog_url`，外加 `features.api_key_model_discovery` 和
/// `suppress_unstable_features_warning` 两个开关才让它生效 —— 一次接管多三行、
/// 其中一个还是 Codex 的「开发中特性」，侵入性太大，改成了让用户自己选：
/// 默认走「换一个 Codex 不认识的模型名」（见 `takeover/clients.rs::codex_config`），
/// 想要 `apply_patch` 的人再手动配这里。
///
/// ```toml
/// [model_providers.apilot]
/// model_catalog_url = "http://127.0.0.1:8787/codex/models"
///
/// [features]
/// api_key_model_discovery = true
/// ```
///
/// 手动配上它换来的是「模型元数据完整」（含 `apply_patch`），代价是那两个开关，
/// 以及客户端启动时必须够得着网关 —— 取不到会静默退回它内置那份（也就是 Lite）。
pub const CATALOG_PATH: &str = "/codex/models";

/// 交给客户端的 `model_catalog_url`。
///
/// 和 `ClientId::stored_base_url` 一样，是「网关地址 → 写进客户端配置的地址」这条唯一
/// 路径的一部分：网关换地址时 `repoint_taken_over` 会重新生成，所以这里不接受任何别处
/// 来的地址。
pub fn catalog_url(base_url: &str) -> String {
    format!("{}{CATALOG_PATH}", base_url.trim_end_matches('/'))
}

/// 打过补丁的内置目录（`{"models": [...]}`），首次访问时构建。
///
/// 用 `OnceLock` 而不是每次重算：四百多 KB 的解析没必要每个请求做一遍。**只有内置
/// 那半份**能这样缓存 —— Apilot 自己的模型名来自数据库，运行时会变，见 [`catalog`]。
fn base() -> AppResult<&'static Value> {
    static BASE: OnceLock<Result<Value, String>> = OnceLock::new();
    match BASE.get_or_init(build_base) {
        Ok(v) => Ok(v),
        Err(reason) => Err(AppError::Msg(format!("内置 Codex 模型目录不可用: {reason}"))),
    }
}

/// 交给客户端的目录：内置那份 **+ 为 Apilot 自己的模型名各补一条**。
///
/// `extra` 是 Apilot 会给客户端用的模型名（见 `gateway/router.rs`）。补这半份是因为
/// 目录是**按名字**生效的：客户端用哪个名字发请求，就去目录里查哪个。少了这半份，
/// 只要客户端用的不是 GPT 系名字（比如「接管时写入当前模型」写的那个），就会落到
/// Codex 自己的兜底元数据上 —— 而那上面没有 `apply_patch`。
///
/// 两半合起来才让「目录」和「换模型名」互补而不是互相抵消：
///
/// | 情况 | 结果 |
/// |---|---|
/// | 取到目录 | 完整元数据，**带 `apply_patch`** |
/// | 取不到（网关没起 / 目录被撤） | Codex 兜底：经典工具集，没有 `apply_patch`，至少不是 Lite |
pub fn catalog(extra: &[String]) -> AppResult<Vec<u8>> {
    let mut models = base()?["models"].as_array().cloned().unwrap_or_default();
    // 去重是**强制**的：Codex 校验目录时，slug 重复会让它**整份拒收** —— 那比少一条
    // 糟得多，因为客户端会静默退回内置目录（也就是 Lite）。
    let mut seen: HashSet<String> = models
        .iter()
        .filter_map(|m| m["slug"].as_str().map(String::from))
        .collect();

    for name in extra {
        let name = name.trim();
        if name.is_empty() || !seen.insert(name.to_string()) {
            continue;
        }
        models.push(entry(name));
    }

    serde_json::to_vec(&json!({ "models": models }))
        .map_err(|e| AppError::Msg(format!("序列化模型目录失败: {e}")))
}

/// 给 Apilot 自己的模型名生成一条元数据。
///
/// 刻意贴着 **Codex 自己的兜底元数据**写（`models-manager/src/model_info.rs`）：
/// 那些值就是同一个模型名在「没有目录」时的待遇，照抄等于不引入变化，只动手脚动在
/// 我们真正要改的三处 —— 关 Lite、开 `apply_patch`、提示词用通用那份（那份本来就是
/// 配经典工具集、且教模型用 `apply_patch` 的）。
///
/// `context_window` 也照抄（272k）。Apilot 没有「上游模型的真实窗口」这份数据，编一个
/// 数只会更危险：报大了客户端来不及压缩、报小了白白浪费上下文。想改得先有数据源。
fn entry(name: &str) -> Value {
    json!({
        "slug": name,
        "display_name": name,
        "description": "通过 Apilot 提供",
        "default_reasoning_level": Value::Null,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 99,
        "availability_nux": Value::Null,
        "upgrade": Value::Null,
        "support_verbosity": false,
        "default_verbosity": Value::Null,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": { "mode": "bytes", "limit": 10_000 },
        "experimental_supported_tools": [],
        "context_window": 272_000,
        "max_context_window": 272_000,
        "use_responses_lite": false,
        "tool_mode": "direct",
        "supports_search_tool": false,
        "prefer_websockets": false,
        "model_messages": { "instructions_template": GENERIC_PROMPT },
        "base_instructions": GENERIC_PROMPT,
    })
}

fn build_base() -> Result<Value, String> {
    let mut catalog: Value =
        serde_json::from_str(MODELS_JSON).map_err(|e| format!("内置目录不是合法 JSON: {e}"))?;
    let models = catalog
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "内置目录缺少 models 数组".to_string())?;

    for model in models.iter_mut() {
        let was_lite = model
            .get("use_responses_lite")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // 只有原本被 Lite 关住的条目才需要换提示词：它们的提示词是写给 code mode 的。
        // 本来就跑经典工具集的条目（如 gpt-5.5）保留自己那份，别多此一举。
        if was_lite {
            model["model_messages"] = json!({ "instructions_template": GENERIC_PROMPT });
        }

        model["use_responses_lite"] = json!(false);
        model["tool_mode"] = json!("direct");
        model["apply_patch_tool_type"] = json!("freeform");
        // `tool_search` 是 OpenAI 的服务端扩展工具，Apilot 的上游不认它。
        model["supports_search_tool"] = json!(false);
        // 网关只讲 HTTP，留着它会先白试一次 Responses websocket 再回落。
        model["prefer_websockets"] = json!(false);

        // 老客户端读的是这个已弃用的顶层字段（Codex 自己序列化时也镜像一份）。
        // 取不到就说明上面两步没生效，宁可留空也不要塞进错的提示词。
        let prompt = model
            .get("model_messages")
            .and_then(|m| m.get("instructions_template"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        model["base_instructions"] = json!(prompt);
    }

    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apilot 自己那半份用的名字。**故意让所有不变量测试都带上它** —— 动态那半份和
    /// 内置那半份要守同一套规矩，分开测迟早有一边漏掉。
    const APILOT_MODEL: &str = "deepseek-flash";

    fn names() -> Vec<String> {
        vec![APILOT_MODEL.to_string()]
    }

    fn models() -> Vec<Value> {
        let bytes = catalog(&names()).expect("目录必须可用");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["models"].as_array().unwrap().clone()
    }

    fn by_slug(slug: &str) -> Value {
        models()
            .into_iter()
            .find(|m| m["slug"] == slug)
            .unwrap_or_else(|| panic!("目录里应该有 {slug}"))
    }

    #[test]
    fn apilot_model_names_are_in_the_catalog() {
        // 这是这一半存在的理由：客户端用 `deepseek-flash` 这类名字时，目录里必须有
        // 对应条目，否则它落到 Codex 兜底元数据上 —— 那里没有 apply_patch。
        let entry = by_slug(APILOT_MODEL);
        assert_eq!(entry["apply_patch_tool_type"], json!("freeform"));
        assert_eq!(entry["use_responses_lite"], json!(false));
        assert_eq!(entry["tool_mode"], json!("direct"));
    }

    #[test]
    fn apilot_entries_carry_every_field_codex_requires() {
        // Codex 反序列化时这些字段没有默认值 —— 缺一个会让它**整份拒收**目录，
        // 客户端于是静默退回内置那份（也就是 Lite）。比少一条糟得多。
        let entry = by_slug(APILOT_MODEL);
        for key in [
            "slug",
            "display_name",
            "description",
            "supported_reasoning_levels",
            "shell_type",
            "visibility",
            "supported_in_api",
            "priority",
            "availability_nux",
            "upgrade",
            "support_verbosity",
            "default_verbosity",
            "apply_patch_tool_type",
            "truncation_policy",
            "experimental_supported_tools",
        ] {
            assert!(
                entry.get(key).is_some(),
                "{key} 是 Codex 的必填字段，缺了整份目录会被拒收"
            );
        }
    }

    #[test]
    fn a_name_codex_already_covers_is_not_duplicated() {
        // slug 重复会让 Codex 整份拒收目录。客户端完全可能把某个模型映射成 gpt-6-sol
        // 这类名字，所以这条必须挡住。
        let list = catalog(&["gpt-6-sol".to_string(), "gpt-6-sol".to_string()]).unwrap();
        let v: Value = serde_json::from_slice(&list).unwrap();
        let mut slugs: Vec<&str> = v["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["slug"].as_str().unwrap())
            .collect();
        let total = slugs.len();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), total, "目录里有重复 slug");
    }

    #[test]
    fn blank_names_are_ignored() {
        // 模型策略没配时 `effective().model` 可能是空的/空白，别往目录里塞空 slug ——
        // 空 slug 同样会让 Codex 整份拒收。
        let list = catalog(&["".to_string(), "   ".to_string()]).unwrap();
        let v: Value = serde_json::from_slice(&list).unwrap();
        for m in v["models"].as_array().unwrap() {
            assert!(!m["slug"].as_str().unwrap().trim().is_empty());
        }
    }

    #[test]
    fn every_model_speaks_the_classic_tool_shape() {
        // 这是整个模块存在的理由：漏掉任何一条，那条模型走 Lite 时上游就会静默
        // 把工具丢掉，客户端看到的是模型把调用当正文吐出来的 DSML 文本。
        let all = models();
        assert!(!all.is_empty());
        for m in &all {
            assert_eq!(m["use_responses_lite"], json!(false), "{} 还在用 Lite", m["slug"]);
            assert_eq!(m["tool_mode"], json!("direct"), "{} 还在用 code mode", m["slug"]);
        }
    }

    #[test]
    fn code_mode_entries_get_the_generic_prompt() {
        // gpt-6-sol 原本是 Lite + code mode，提示词里在教模型用 `functions.exec`。
        // 关掉 code mode 后那份提示词就指向不存在的工具了，必须换掉。
        let sol = by_slug("gpt-6-sol");
        let prompt = sol["model_messages"]["instructions_template"].as_str().unwrap();
        assert_eq!(prompt, GENERIC_PROMPT);
        assert!(!prompt.contains("functions.exec"));
    }

    #[test]
    fn prompts_are_never_empty() {
        // Codex 反序列化时要求条目要么有 `base_instructions`、要么有
        // `model_messages.instructions_template`；两个都空会让**客户端起不来**。
        for m in models() {
            let template = m["model_messages"]["instructions_template"].as_str().unwrap_or_default();
            assert!(!template.is_empty(), "{} 没有提示词", m["slug"]);
            assert_eq!(m["base_instructions"].as_str().unwrap(), template);
        }
    }

    #[test]
    fn apply_patch_stays_available() {
        // 提示词在教模型用 apply_patch，工具就必须在；否则模型会照着提示词去调一个
        // 不存在的工具，白烧几轮。上游对顶层 custom 的 apply_patch 是接受的。
        for m in models() {
            assert_eq!(m["apply_patch_tool_type"], json!("freeform"), "{}", m["slug"]);
        }
    }

    #[test]
    fn original_slugs_are_all_preserved() {
        // 目录是**整体替换**客户端内置那份的，漏掉一个 slug，那个模型名就会掉进
        // Codex 的兜底元数据（没有 apply_patch、提示词也变成通用那份）。
        let slugs: Vec<String> = models()
            .iter()
            .map(|m| m["slug"].as_str().unwrap().to_string())
            .collect();
        for expected in ["gpt-6-sol", "gpt-5.5", "codex-auto-review"] {
            assert!(slugs.iter().any(|s| s == expected), "少了 {expected}");
        }
    }
}
