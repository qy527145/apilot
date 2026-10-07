//! models.dev 目录解析。
//!
//! 顶层是 `厂商 id → 厂商`，每个厂商下面挂 `模型 id → 模型`。
//! 价格字段 `cost` 的单位**已经是美元 / 100 万 token**，不需要乘数。

use std::collections::HashMap;

use serde::Deserialize;

use super::CatalogModel;
use crate::error::AppError;

#[derive(Deserialize)]
struct Provider {
    #[serde(default)]
    models: HashMap<String, Model>,
}

#[derive(Deserialize)]
struct Model {
    /// 形如 `anthropic/claude-sonnet-4-5`。前缀厂商等于所属厂商才算「一手」。
    #[serde(default)]
    canonical_model_id: Option<String>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    tool_call: bool,
    /// 「能不能带附件」。老条目没有 `modalities` 时拿它兜底判断视觉能力。
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    modalities: Option<Modalities>,
    #[serde(default)]
    cost: Option<Cost>,
}

#[derive(Deserialize)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize)]
struct Cost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    /// 思考 token 单价，只有部分模型给。
    reasoning: Option<f64>,
}

pub fn parse(bytes: &[u8]) -> Result<Vec<CatalogModel>, AppError> {
    let providers: HashMap<String, Provider> = serde_json::from_slice(bytes)?;
    let mut out = Vec::new();

    for (vendor, provider) in providers {
        for (name, m) in provider.models {
            let input_mods = m
                .modalities
                .as_ref()
                .map(|x| x.input.as_slice())
                .unwrap_or(&[]);
            let has_mod = |k: &str| input_mods.iter().any(|s| s == k);

            // 一手判定：canonical_model_id 的前缀就是这个条目的厂商。
            // 注意不能要求相等 —— DeepSeek 那条的 canonical 是
            // `deepseek/deepseek-v4-pro-0813`，比模型名多一个日期后缀。
            let first_party = m
                .canonical_model_id
                .as_deref()
                .is_some_and(|c| c.starts_with(&format!("{vendor}/")));

            let cost = m.cost.unwrap_or(Cost {
                input: None,
                output: None,
                cache_read: None,
                cache_write: None,
                reasoning: None,
            });

            out.push(CatalogModel {
                model: name,
                vendor: vendor.clone(),
                first_party,
                input: cost.input,
                output: cost.output,
                cache_read: cost.cache_read,
                cache_write: cost.cache_write,
                reasoning: cost.reasoning,
                supports_reasoning: m.reasoning,
                supports_tools: m.tool_call,
                // 以 modalities 为准；老条目没有这个字段时退回 attachment
                supports_vision: has_mod("image") || (input_mods.is_empty() && m.attachment),
                supports_pdf: has_mod("pdf"),
            });
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与真实文件同形的最小样本（字段值取自实际抓到的 claude-sonnet-4-5 等条目）。
    const FIXTURE: &str = r#"{
      "anthropic": {
        "id": "anthropic",
        "name": "Anthropic",
        "models": {
          "claude-sonnet-4-5": {
            "id": "claude-sonnet-4-5",
            "attachment": true,
            "reasoning": true,
            "tool_call": true,
            "modalities": { "input": ["text", "image", "pdf"], "output": ["text"] },
            "cost": { "input": 3, "output": 15, "cache_read": 0.3, "cache_write": 3.75 },
            "canonical_model_id": "anthropic/claude-sonnet-4-5"
          }
        }
      },
      "nano-gpt": {
        "id": "nano-gpt",
        "name": "NanoGPT",
        "models": {
          "deepseek-chat": {
            "id": "deepseek-chat",
            "tool_call": true,
            "cost": { "input": 0.1, "output": 0.425, "cache_read": 0.05 },
            "canonical_model_id": "deepseek/deepseek-chat"
          }
        }
      },
      "deepseek": {
        "id": "deepseek",
        "name": "DeepSeek",
        "models": {
          "deepseek-v4-pro": {
            "id": "deepseek-v4-pro",
            "reasoning": true,
            "tool_call": true,
            "cost": { "input": 0.66, "output": 1.98, "reasoning": 1.98, "cache_read": 0.022 },
            "canonical_model_id": "deepseek/deepseek-v4-pro-0813"
          }
        }
      }
    }"#;

    fn find<'a>(ms: &'a [CatalogModel], name: &str) -> &'a CatalogModel {
        ms.iter().find(|m| m.model == name).expect("条目应存在")
    }

    #[test]
    fn prices_come_through_in_dollars_per_million() {
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let m = find(&ms, "claude-sonnet-4-5");
        assert_eq!(m.input, Some(3.0));
        assert_eq!(m.output, Some(15.0));
        assert_eq!(m.cache_read, Some(0.3));
        assert_eq!(m.cache_write, Some(3.75));
    }

    #[test]
    fn capabilities_come_from_modalities_and_flags() {
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let m = find(&ms, "claude-sonnet-4-5");
        assert!(m.supports_reasoning && m.supports_tools);
        assert!(m.supports_vision, "modalities.input 里有 image");
        assert!(m.supports_pdf);
    }

    #[test]
    fn only_the_matching_vendor_counts_as_first_party() {
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        // anthropic 自己发的，canonical 前缀就是 anthropic
        assert!(find(&ms, "claude-sonnet-4-5").first_party);
        // nano-gpt 转售的，canonical 前缀是 deepseek
        assert!(!find(&ms, "deepseek-chat").first_party);
        // DeepSeek 自己发的，且 canonical 带日期后缀 —— 只能比前缀，不能比相等
        assert!(find(&ms, "deepseek-v4-pro").first_party);
    }

    #[test]
    fn a_model_without_a_cost_block_parses_with_no_price() {
        let json = r#"{"p":{"models":{"mystery":{"id":"mystery"}}}}"#;
        let ms = parse(json.as_bytes()).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].input, None);
        assert!(to_pricing_is_none(&ms[0]));
    }

    fn to_pricing_is_none(m: &CatalogModel) -> bool {
        super::super::to_pricing(m).is_none()
    }

    #[test]
    fn older_entries_fall_back_to_the_attachment_flag() {
        // 没有 modalities 的老条目：只要 attachment 为真就当作支持图片
        let json = r#"{"p":{"models":{"old":{"attachment":true,"cost":{"input":1}}}}}"#;
        let ms = parse(json.as_bytes()).unwrap();
        assert!(ms[0].supports_vision);
        assert!(!ms[0].supports_pdf, "pdf 只认 modalities，不靠 attachment 猜");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse(b"{ not json").is_err());
    }
}
