//! LiteLLM 目录解析。
//!
//! 顶层是扁平的 `模型名 → 条目`，没有厂商分组，靠条目里的 `litellm_provider`
//! 字段标注归属。价格字段是**美元 / 单个 token** 的浮点（如 `3e-06`），
//! 要乘 1e6 才是本模块约定的「美元 / 100 万 token」。
//!
//! 这个乘数是最容易漏的一步：漏了会得到「$0.000003 / 1M」这种价格，
//! 计费全部趋近于零，而且不报任何错。所以归一化统一放在本模块做，
//! 并且有测试盯着。

use serde::Deserialize;
use serde_json::Value;

use super::CatalogModel;
use crate::error::AppError;

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    litellm_provider: Option<String>,

    input_cost_per_token: Option<f64>,
    output_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    cache_creation_input_token_cost: Option<f64>,
    output_cost_per_reasoning_token: Option<f64>,

    #[serde(default)]
    supports_function_calling: Option<bool>,
    #[serde(default)]
    supports_vision: Option<bool>,
    #[serde(default)]
    supports_pdf_input: Option<bool>,
    #[serde(default)]
    supports_reasoning: Option<bool>,
}

/// 只收 Apilot 会转发的那两类。
///
/// `mode` 分布（实测）：chat 3358、image_generation 409、**responses 170**、
/// embedding 149、audio_transcription 94、realtime 52、video_generation 46、
/// audio_speech 42、无 mode 9。`responses` 必须收 —— 那正是本项目支持的
/// `openai_responses` 协议。其余（画图/嵌入/语音/实时）Apilot 不转发，
/// 收进来只会把价格表搞乱。
///
/// 没有 `mode` 的 9 条一律跳过：它们不是模型，是 fireworks 的**价格档位**
/// （`fireworks-ai-4.1b-to-16b` 这种按参数量分段的计价规则）。
fn is_forwardable(mode: Option<&str>) -> bool {
    matches!(mode, Some("chat") | Some("responses"))
}

/// 美元 / 单 token → 美元 / 100 万 token。
const PER_TOKEN_TO_PER_MILLION: f64 = 1_000_000.0;

pub fn parse(bytes: &[u8]) -> Result<Vec<CatalogModel>, AppError> {
    let raw: serde_json::Map<String, Value> = serde_json::from_slice(bytes)?;
    let mut out = Vec::new();

    for (name, value) in raw {
        // 逐条容错：目录是外部数据，一条形状不对不该拖垮整份。
        let Ok(e) = serde_json::from_value::<Entry>(value) else {
            continue;
        };
        if !is_forwardable(e.mode.as_deref()) {
            continue;
        }

        let per_m = |v: Option<f64>| v.map(|x| x * PER_TOKEN_TO_PER_MILLION);

        out.push(CatalogModel {
            model: name,
            vendor: e.litellm_provider.unwrap_or_default(),
            // 扁平命名空间，每条都是一手
            first_party: true,
            input: per_m(e.input_cost_per_token),
            output: per_m(e.output_cost_per_token),
            cache_read: per_m(e.cache_read_input_token_cost),
            cache_write: per_m(e.cache_creation_input_token_cost),
            reasoning: per_m(e.output_cost_per_reasoning_token),
            supports_reasoning: e.supports_reasoning.unwrap_or(false),
            supports_tools: e.supports_function_calling.unwrap_or(false),
            supports_vision: e.supports_vision.unwrap_or(false),
            supports_pdf: e.supports_pdf_input.unwrap_or(false),
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 字段名与取值都取自实际抓到的条目。
    const FIXTURE: &str = r#"{
      "sample_spec": { "input_cost_per_token": 0 },
      "fireworks-ai-4.1b-to-16b": { "input_cost_per_token": 2e-07 },
      "claude-sonnet-4-5": {
        "max_input_tokens": 1000000,
        "mode": "chat",
        "litellm_provider": "anthropic",
        "input_cost_per_token": 3e-06,
        "output_cost_per_token": 1.5e-05,
        "cache_read_input_token_cost": 3e-07,
        "cache_creation_input_token_cost": 3.75e-06,
        "supports_function_calling": true,
        "supports_reasoning": true,
        "supports_vision": true,
        "supports_pdf_input": true
      },
      "gpt-5": {
        "mode": "responses",
        "litellm_provider": "openai",
        "input_cost_per_token": 1.25e-06,
        "output_cost_per_token": 1e-05
      },
      "dall-e-3": {
        "mode": "image_generation",
        "litellm_provider": "openai",
        "input_cost_per_token": 0
      },
      "text-embedding-3-small": {
        "mode": "embedding",
        "litellm_provider": "openai",
        "input_cost_per_token": 2e-08
      }
    }"#;

    #[test]
    fn per_token_prices_are_scaled_to_per_million() {
        // 漏掉这个 1e6 会让所有价格趋近于零且不报错，是最危险的一步。
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let m = ms.iter().find(|m| m.model == "claude-sonnet-4-5").unwrap();
        assert_eq!(m.input, Some(3.0), "3e-06 $/token = $3 / 1M");
        assert_eq!(m.output, Some(15.0));
        assert_eq!(m.cache_read, Some(0.3));
        assert_eq!(m.cache_write, Some(3.75));
    }

    #[test]
    fn the_same_model_prices_to_the_same_ratios_as_models_dev() {
        // 两个来源对同一模型的换算结果必须一致，否则换来源就会悄悄改价。
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let m = ms.iter().find(|m| m.model == "claude-sonnet-4-5").unwrap();
        let p = super::super::to_pricing(m).unwrap();
        assert_eq!(p.model_ratio, 1.5);
        assert_eq!(p.completion_ratio, 5.0);
        assert_eq!(p.cache_ratio, 0.1);
        assert_eq!(p.cache_create_ratio, 1.25);
    }

    #[test]
    fn responses_mode_is_kept_but_other_modes_are_dropped() {
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let names: Vec<&str> = ms.iter().map(|m| m.model.as_str()).collect();
        assert!(names.contains(&"gpt-5"), "responses 是本项目支持的协议，要收");
        assert!(!names.contains(&"dall-e-3"), "画图模型不该进价格表");
        assert!(!names.contains(&"text-embedding-3-small"));
        assert!(!names.contains(&"sample_spec"));
        assert!(
            !names.contains(&"fireworks-ai-4.1b-to-16b"),
            "没有 mode 的是价格档位不是模型"
        );
    }

    #[test]
    fn capability_flags_map_across() {
        let ms = parse(FIXTURE.as_bytes()).unwrap();
        let m = ms.iter().find(|m| m.model == "claude-sonnet-4-5").unwrap();
        assert!(m.supports_tools && m.supports_reasoning && m.supports_vision && m.supports_pdf);
        assert!(m.first_party, "扁平命名空间里每条都算一手");
        assert_eq!(m.vendor, "anthropic");
    }

    #[test]
    fn one_broken_entry_does_not_sink_the_whole_catalog() {
        // 外部数据里混进一条形状不对的，其余仍要能用。
        let json = r#"{
          "good": { "mode": "chat", "input_cost_per_token": 1e-06 },
          "bad": "not an object",
          "worse": 42
        }"#;
        let ms = parse(json.as_bytes()).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].model, "good");
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse(b"[").is_err());
    }
}
