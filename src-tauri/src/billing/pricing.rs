//! 模型单价系数。
//!
//! 沿用 new-api 的多级倍率模型：单价 = 基准倍率 × 各类 token 的细分倍率。
//! 「1 倍率」对应每 100 万 token 2 美元 —— 但真正决定金额的是
//! [`super::engine`] 里的结算公式，这里的数值只是它的输入。

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// 单个模型的全部单价系数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub model: String,

    /// 输入基准倍率。1.0 表示「每 100 万输入 token 计 2 美元」。
    pub model_ratio: f64,
    /// 输出倍率，通常大于 1（输出更贵）。
    pub completion_ratio: f64,
    /// 缓存**读**的倍率。命中的缓存便宜，通常 0.1~1.0。
    pub cache_ratio: f64,
    /// 缓存**写**的倍率。写缓存比普通输入更贵，通常 > 1。
    pub cache_create_ratio: f64,
    /// 分组倍率，用于按客户端 / 渠道整体打折或加价。
    pub group_ratio: f64,
    pub image_ratio: f64,
    pub audio_ratio: f64,
    /// 每次工具调用的附加额度（固定值，不是倍率）。
    pub tool_call_surcharge: i64,
    /// 其它叠加倍率（按渠道、时段等），最终连乘。
    pub other_ratios: HashMap<String, f64>,
    pub currency: String,
    pub updated_at: i64,
}

impl Default for ModelPricing {
    fn default() -> Self {
        Self {
            model: String::new(),
            model_ratio: 1.0,
            completion_ratio: 1.0,
            cache_ratio: 1.0,
            cache_create_ratio: 1.25,
            group_ratio: 1.0,
            image_ratio: 1.0,
            audio_ratio: 1.0,
            tool_call_surcharge: 0,
            other_ratios: HashMap::new(),
            currency: "USD".to_string(),
            updated_at: 0,
        }
    }
}

impl ModelPricing {
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    /// 所有细分倍率的乘积，用于快速判断"这个模型是不是免费的"。
    pub fn is_free(&self) -> bool {
        self.model_ratio == 0.0
    }

    /// 把用户输入夹到合理区间。
    ///
    /// 负倍率会让计费变成负数（等于倒贴），必须挡在入库前。
    pub fn normalized(mut self) -> Self {
        let clamp = |v: f64| -> f64 {
            if !v.is_finite() || v < 0.0 {
                0.0
            } else {
                v.min(1_000_000.0)
            }
        };
        self.model_ratio = clamp(self.model_ratio);
        self.completion_ratio = clamp(self.completion_ratio);
        self.cache_ratio = clamp(self.cache_ratio);
        self.cache_create_ratio = clamp(self.cache_create_ratio);
        self.group_ratio = clamp(self.group_ratio);
        self.image_ratio = clamp(self.image_ratio);
        self.audio_ratio = clamp(self.audio_ratio);
        self.tool_call_surcharge = self.tool_call_surcharge.max(0);
        self.other_ratios.retain(|_, v| v.is_finite() && *v >= 0.0);
        self
    }
}

/// 单价表。整体热替换，读侧无锁。
pub struct PricingTable {
    entries: ArcSwap<HashMap<String, ModelPricing>>,
    /// 未配置过的模型用这份倍率兜底。
    fallback: ArcSwap<ModelPricing>,
}

impl PricingTable {
    pub fn new(entries: Vec<ModelPricing>, fallback: ModelPricing) -> Self {
        let map = entries
            .into_iter()
            .map(|p| (p.model.clone(), p))
            .collect();
        Self {
            entries: ArcSwap::from_pointee(map),
            fallback: ArcSwap::from_pointee(fallback),
        }
    }

    /// 取某模型的单价。未配置时返回兜底倍率并继承模型名。
    pub fn get(&self, model: &str) -> ModelPricing {
        match self.entries.load().get(model) {
            Some(p) => p.clone(),
            None => {
                // 先查无版本号的近似匹配：`claude-sonnet-5-20260101` 能命中
                // 用户为 `claude-sonnet-5` 配的价格。反之不成立。
                if let Some(p) = self.entries.load().get(strip_date_suffix(model)) {
                    return p.clone();
                }
                let mut fb = (**self.fallback.load()).clone();
                fb.model = model.to_string();
                fb
            }
        }
    }

    pub fn is_configured(&self, model: &str) -> bool {
        let e = self.entries.load();
        e.contains_key(model) || e.contains_key(strip_date_suffix(model))
    }

    pub fn len(&self) -> usize {
        self.entries.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.load().is_empty()
    }

    pub fn models(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.load().keys().cloned().collect();
        v.sort();
        v
    }

    pub fn all(&self) -> Vec<ModelPricing> {
        let mut v: Vec<ModelPricing> = self.entries.load().values().cloned().collect();
        v.sort_by(|a, b| a.model.cmp(&b.model));
        v
    }

    pub fn fallback(&self) -> ModelPricing {
        (**self.fallback.load()).clone()
    }

    pub fn reload(&self, entries: Vec<ModelPricing>, fallback: ModelPricing) {
        let map = entries
            .into_iter()
            .map(|p| (p.model.clone(), p))
            .collect();
        self.entries.store(Arc::new(map));
        self.fallback.store(Arc::new(fallback));
    }
}

impl Default for PricingTable {
    fn default() -> Self {
        Self::new(Vec::new(), ModelPricing::default())
    }
}

/// 去掉模型名尾部的日期版本号。
///
/// `claude-sonnet-5-20260101` → `claude-sonnet-5`；无日期后缀时原样返回。
/// 这样用户只需为一个模型家族配一次价格。
fn strip_date_suffix(model: &str) -> &str {
    // 倒数第二段必须是 8 位纯数字日期
    match model.rsplit_once('-') {
        Some((head, tail)) if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing(model: &str, ratio: f64) -> ModelPricing {
        ModelPricing {
            model: model.into(),
            model_ratio: ratio,
            ..Default::default()
        }
    }

    #[test]
    fn defaults_are_sane() {
        let d = ModelPricing::default();
        assert_eq!(d.model_ratio, 1.0);
        assert_eq!(d.completion_ratio, 1.0);
        assert_eq!(d.cache_create_ratio, 1.25, "写缓存比普通输入贵");
        assert_eq!(d.tool_call_surcharge, 0);
        assert!(!d.is_free());
    }

    #[test]
    fn lookup_returns_configured_entry() {
        let t = PricingTable::new(vec![pricing("claude-sonnet-5", 3.0)], ModelPricing::default());
        assert_eq!(t.get("claude-sonnet-5").model_ratio, 3.0);
        assert!(t.is_configured("claude-sonnet-5"));
    }

    #[test]
    fn lookup_falls_back_for_unknown_model() {
        let mut fb = ModelPricing::default();
        fb.model_ratio = 7.0;
        let t = PricingTable::new(vec![pricing("a", 3.0)], fb);

        let got = t.get("unknown-model");
        assert_eq!(got.model_ratio, 7.0, "应使用兜底倍率");
        assert_eq!(got.model, "unknown-model", "模型名应被填充");
        assert!(!t.is_configured("unknown-model"));
    }

    #[test]
    fn dated_model_matches_undated_price() {
        // 用户只需配一次家族价格，带日期的快照自动命中
        let t = PricingTable::new(
            vec![pricing("claude-sonnet-5", 3.0)],
            ModelPricing::default(),
        );
        assert_eq!(t.get("claude-sonnet-5-20260101").model_ratio, 3.0);
        assert!(t.is_configured("claude-sonnet-5-20260101"));
    }

    #[test]
    fn exact_match_wins_over_dated_stripping() {
        let t = PricingTable::new(
            vec![
                pricing("claude-sonnet-5", 3.0),
                pricing("claude-sonnet-5-20260101", 99.0),
            ],
            ModelPricing::default(),
        );
        assert_eq!(t.get("claude-sonnet-5-20260101").model_ratio, 99.0);
    }

    #[test]
    fn non_date_suffix_is_not_stripped() {
        let t = PricingTable::new(vec![pricing("gpt-4o", 1.0)], ModelPricing::default());
        // "gpt-4o-mini" 不应误命中 "gpt-4o"
        assert!(!t.is_configured("gpt-4o-mini"));
        assert_eq!(strip_date_suffix("gpt-4o-mini"), "gpt-4o-mini");
        assert_eq!(strip_date_suffix("claude-3-5-sonnet-20241022"), "claude-3-5-sonnet");
        assert_eq!(strip_date_suffix("claude-sonnet-5"), "claude-sonnet-5");
    }

    #[test]
    fn reload_swaps_the_whole_table() {
        let t = PricingTable::new(vec![pricing("a", 1.0)], ModelPricing::default());
        assert!(t.is_configured("a"));

        t.reload(vec![pricing("b", 2.0)], ModelPricing::default());
        assert!(!t.is_configured("a"), "旧的应被整体替换掉");
        assert!(t.is_configured("b"));
    }

    #[test]
    fn models_and_all_are_sorted() {
        let t = PricingTable::new(
            vec![pricing("z", 1.0), pricing("a", 1.0)],
            ModelPricing::default(),
        );
        assert_eq!(t.models(), vec!["a", "z"]);
        assert_eq!(t.all()[0].model, "a");
    }

    #[test]
    fn normalize_rejects_negative_and_non_finite() {
        let bad = ModelPricing {
            model_ratio: -5.0,
            completion_ratio: f64::NAN,
            cache_ratio: f64::INFINITY,
            tool_call_surcharge: -100,
            ..Default::default()
        }
        .normalized();

        assert_eq!(bad.model_ratio, 0.0, "负倍率等于倒贴，必须归零");
        assert_eq!(bad.completion_ratio, 0.0);
        assert_eq!(bad.cache_ratio, 0.0);
        assert_eq!(bad.tool_call_surcharge, 0);
    }

    #[test]
    fn normalize_drops_bad_other_ratios() {
        let mut p = ModelPricing::for_model("m");
        p.other_ratios.insert("good".into(), 1.5);
        p.other_ratios.insert("neg".into(), -2.0);
        p.other_ratios.insert("nan".into(), f64::NAN);

        let n = p.normalized();
        assert_eq!(n.other_ratios.len(), 1);
        assert_eq!(n.other_ratios.get("good"), Some(&1.5));
    }

    #[test]
    fn zero_ratio_means_free() {
        let p = ModelPricing {
            model_ratio: 0.0,
            ..Default::default()
        };
        assert!(p.is_free());
    }

    #[test]
    fn empty_table_uses_fallback() {
        let t = PricingTable::default();
        assert!(t.is_empty());
        assert_eq!(t.get("anything").model_ratio, 1.0);
    }
}
