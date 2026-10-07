//! 上游模型目录：一次性把**价格**和**能力**拉进来。
//!
//! 两者共用一份数据源不是巧合 —— 公开的模型目录（models.dev、LiteLLM）本来就
//! 同时维护价格和能力标志，分成两个网络集成只会重复解析同一份 JSON。
//!
//! 本模块只负责「拉取 + 解析 + 归一化」，不碰数据库也不碰界面：把两个来源
//! 形状各异的 JSON 折成同一个 [`CatalogModel`]，下游（写 `model_pricing`、
//! 展示能力）就只剩取字段。

pub mod litellm;
pub mod models_dev;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::billing::pricing::ModelPricing;
use crate::error::AppError;

/// 目录来源。
///
/// 两家的数据形状差别很大，不是互为备份的关系：models.dev 按厂商分组、价格是
/// $/1M，覆盖更全更新（kimi / glm / qwen3-max 只有它有，DeepSeek 已是 v4 命名）；
/// LiteLLM 扁平、价格是 $/token，模型更多但偏一手大厂。所以做成可切换而不是二选一。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    ModelsDev,
    LiteLlm,
}

impl CatalogSource {
    pub const DEFAULT: Self = Self::ModelsDev;

    pub fn url(self) -> &'static str {
        match self {
            Self::ModelsDev => "https://models.dev/api.json",
            Self::LiteLlm => {
                "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ModelsDev => "models.dev",
            Self::LiteLlm => "LiteLLM",
        }
    }

    /// 解析该来源的原始响应。
    pub fn parse(self, bytes: &[u8]) -> Result<Vec<CatalogModel>, AppError> {
        match self {
            Self::ModelsDev => models_dev::parse(bytes),
            Self::LiteLlm => litellm::parse(bytes),
        }
    }
}

/// 归一化后的一个模型条目。
///
/// 价格一律折成**每 100 万 token 的美元**。两个来源的单位不同（models.dev 直接
/// 给 $/1M，LiteLLM 给 $/token 的浮点），在这里对齐之后下游只剩一步除法。
/// 不做这层归一化的话，LiteLLM 那边容易漏乘一个 1e6 —— 那会把价格算错六个数量级，
/// 而且错得悄无声息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub model: String,
    /// 一手厂商 id，用于消歧和展示。
    pub vendor: String,
    /// 这个条目是不是「一手厂商」发布的那条，见 [`index_by_model`]。
    pub first_party: bool,

    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// 思考 token 单价，只有部分模型给。缺了不代表不支持思考。
    pub reasoning: Option<f64>,

    pub supports_reasoning: bool,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_pdf: bool,
}

/// 「1 倍率」对应的每 100 万 token 美元数。
///
/// 口径的真源在 `billing::pricing` 的模块注释里，这里只是把它变成常量：
/// `model_ratio = 价格($/1M) / 2`。
const USD_PER_MILLION_PER_UNIT: f64 = 2.0;

/// 把倍率收敛到 6 位小数。
///
/// 不是为了测试好写，是两个实打实的理由：
/// 1. 不收敛的话 `0.3 / 3.0` 会存成 `0.09999999999999999` —— 价格表页面上
///    就是这么一串噪声显示给用户看的。
/// 2. 两个来源对同一模型必须换算出**同一串数字**（LiteLLM 的 `3e-06` 和
///    models.dev 的 `3`），否则换个来源就等于悄悄改了价，还查不出来。
///
/// 6 位小数的分辨率是每 100 万 token $2e-6，远细于任何真实价差。
fn round6(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1e6).round() / 1e6
    } else {
        v
    }
}

/// 目录条目 → 计费倍率。**没有输入价就返回 `None`。**
///
/// 宁可不写库也不拿 0 冒充：`model_ratio == 0` 在计费里是「免费」的意思
/// （见 `ModelPricing::is_free`），用缺失值去填会把一个正常收费的模型
/// 变成白送。真正的免费模型（来源里就是 0）走另一条分支，保留 0。
pub fn to_pricing(m: &CatalogModel) -> Option<ModelPricing> {
    let input = m.input?;
    let mut p = ModelPricing::for_model(&m.model);
    p.model_ratio = round6(input / USD_PER_MILLION_PER_UNIT);

    // 输出/缓存倍率都是**相对输入价**的比值，输入价是 0 时这个比值没有意义，
    // 一律保留默认值。
    if input > 0.0 {
        if let Some(v) = m.output {
            p.completion_ratio = round6(v / input);
        }
        if let Some(v) = m.cache_read {
            p.cache_ratio = round6(v / input);
        }
        if let Some(v) = m.cache_write {
            p.cache_create_ratio = round6(v / input);
        }
    }
    Some(p.normalized())
}

/// 把「厂商 × 模型」的多条条目压平成 `模型名 → 条目`。
///
/// 必须消歧：models.dev 里 1153 个模型名出现在多个厂商下（开源模型谁都能转售），
/// 同一份 `deepseek-chat` 在官方和转售商那儿能差十倍价。规则是**一手优先** ——
/// 条目的 `canonical_model_id` 前缀等于它所属厂商才算一手。都不满足时按厂商 id
/// 字典序取第一个：要的是确定性（每次拉的顺序一样），不是"更对"。
///
/// LiteLLM 是扁平命名空间，每条都标了一手，这条规则对它等于直通。
pub fn index_by_model(models: Vec<CatalogModel>) -> BTreeMap<String, CatalogModel> {
    let mut best: HashMap<String, CatalogModel> = HashMap::new();
    for m in models {
        match best.get(&m.model) {
            None => {
                best.insert(m.model.clone(), m);
            }
            Some(cur) => {
                // 一手永远赢过转售；同为一手（或同为转售）时按厂商名定序，
                // 免得同一份数据两次拉的解析结果不同。
                let wins = match (m.first_party, cur.first_party) {
                    (true, false) => true,
                    (false, true) => false,
                    _ => m.vendor < cur.vendor,
                };
                if wins {
                    best.insert(m.model.clone(), m);
                }
            }
        }
    }
    best.into_iter().collect()
}

/// 拉取并解析目录。
///
/// 用调用方给的 client —— 出站代理设置已经解析在里面了（见 `upstream::client`）。
/// 这两个域名在没有代理的网络里连不上（TLS 握手就失败），所以走代理不是可选项。
pub async fn fetch(
    client: &reqwest::Client,
    source: CatalogSource,
) -> Result<Vec<CatalogModel>, AppError> {
    let url = source.url();
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(45))
        .send()
        .await
        .map_err(|e| AppError::msg(format!("拉取{}目录失败：{e}", source.label())))?;

    if !resp.status().is_success() {
        return Err(AppError::msg(format!(
            "拉取{}目录失败：HTTP {}",
            source.label(),
            resp.status().as_u16()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::msg(format!("读取{}目录失败：{e}", source.label())))?;

    source.parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str, vendor: &str, first_party: bool, input: Option<f64>) -> CatalogModel {
        CatalogModel {
            model: name.into(),
            vendor: vendor.into(),
            first_party,
            input,
            output: None,
            cache_read: None,
            cache_write: None,
            reasoning: None,
            supports_reasoning: false,
            supports_tools: false,
            supports_vision: false,
            supports_pdf: false,
        }
    }

    #[test]
    fn price_is_converted_to_the_two_dollar_per_million_unit() {
        // claude-sonnet-4-5 官方 $3/1M 输入、$15/1M 输出、$0.3 缓存读、$3.75 缓存写。
        let mut m = model("claude-sonnet-4-5", "anthropic", true, Some(3.0));
        m.output = Some(15.0);
        m.cache_read = Some(0.3);
        m.cache_write = Some(3.75);

        let p = to_pricing(&m).expect("有输入价就该出倍率");
        assert_eq!(p.model_ratio, 1.5, "3 美元/1M ÷ 2 = 1.5 倍率");
        assert_eq!(p.completion_ratio, 5.0, "输出倍率是相对输入价的比值");
        assert_eq!(p.cache_ratio, 0.1);
        // 和项目默认的 1.25 一致，说明口径对上了
        assert_eq!(p.cache_create_ratio, 1.25);
    }

    #[test]
    fn a_model_without_a_price_is_skipped_rather_than_free() {
        // 没价就返回 None。绝不能落库成 model_ratio = 0 —— 那在计费里是「免费」，
        // 会把一个正常收费的模型悄悄变成白送。
        let m = model("mystery-model", "someone", true, None);
        assert!(to_pricing(&m).is_none());

        let zero = model("genuinely-free", "someone", true, Some(0.0));
        let p = to_pricing(&zero).expect("0 是有效价格");
        assert!(p.is_free(), "来源里真的是 0 就该是免费");
        assert_eq!(p.completion_ratio, 1.0, "输入价为 0 时比值无意义，保留默认");
    }

    #[test]
    fn first_party_entry_wins_over_a_reseller() {
        // 同一份 deepseek-chat：官方 $0.28/1M，转售商 nano-gpt $0.1/1M。
        let official = model("deepseek-chat", "deepseek", true, Some(0.28));
        let reseller = model("deepseek-chat", "nano-gpt", false, Some(0.1));

        // 两个顺序都要给出一手那条
        for order in [
            vec![reseller.clone(), official.clone()],
            vec![official.clone(), reseller.clone()],
        ] {
            let idx = index_by_model(order);
            assert_eq!(idx["deepseek-chat"].vendor, "deepseek");
        }
    }

    #[test]
    fn ties_are_broken_deterministically() {
        // 同为转售商时按厂商名字典序 —— 要的是每次拉结果一致，不是"更对"。
        let a = model("shared-model", "aaa-host", false, Some(1.0));
        let b = model("shared-model", "zzz-host", false, Some(2.0));
        let idx = index_by_model(vec![b, a]);
        assert_eq!(idx["shared-model"].vendor, "aaa-host");
    }

    #[test]
    fn sources_point_at_distinct_urls() {
        assert_ne!(CatalogSource::ModelsDev.url(), CatalogSource::LiteLlm.url());
        assert!(CatalogSource::ModelsDev.url().starts_with("https://"));
        assert!(CatalogSource::LiteLlm.url().starts_with("https://"));
    }
}
