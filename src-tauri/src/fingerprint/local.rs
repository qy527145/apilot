//! 本地样本库的独立 kNN 证据层。
//!
//! 它**不修改**官方冻结检测器：官方排名与校准概率保持原样，这里只用用户确认过
//! 的本地样本，在同一套 429 维 `full` 特征空间里做最近邻。比较的是"到该 label
//! 的距离 / 该 label 自身的 leave-one-out 离散度"——比值约等于 1 表示"和这个
//! label 自己的样本一样近"。
//!
//! 概率只在每个 label ≥6 条样本、且至少两个 label 时才拟合：用 leave-one-out
//! 的样本做一维温度网格搜索，并通过"top-1 高于随机、NLL 低于均匀分布"的门槛。
//! 门槛不过就只给排名和距离比，绝不编造置信度。

use std::collections::BTreeMap;

use crate::fingerprint::codec::Artifact;
use crate::fingerprint::features::{full_feature, mean, median, standardize};
use crate::fingerprint::ranker::nearest;
use crate::fingerprint::types::{LocalCandidate, LocalResult};

const MIN_RANK_SAMPLES: usize = 3;
const MIN_PROBABILITY_SAMPLES: usize = 6;
const TAU_MIN: f64 = 0.05;
const TAU_MAX: f64 = 50.0;
const TAU_STEPS: usize = 200;

struct LabelFeatures {
    label: String,
    samples: Vec<Vec<f64>>,
    spread: f64,
    official: bool,
}

fn leave_one_out_spread(features: &[Vec<f64>]) -> f64 {
    if features.len() < 2 {
        return f64::NAN;
    }
    let values: Vec<f64> = (0..features.len())
        .map(|index| {
            let others: Vec<Vec<f64>> = features
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != index)
                .map(|(_, feature)| feature.clone())
                .collect();
            nearest(&features[index], &others)
        })
        .collect();
    median(&values)
}

fn ratio_score(distance: f64, spread: f64) -> (f64, f64) {
    let ratio = distance / spread.max(1e-12);
    (ratio, -(ratio.max(1e-9)).ln())
}

fn log_sum_exp(values: &[f64]) -> f64 {
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    maximum + values.iter().map(|value| (value - maximum).exp()).sum::<f64>().ln()
}

fn softmax(logits: &[f64]) -> Vec<f64> {
    let maximum = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = logits.iter().map(|value| (value - maximum).exp()).collect();
    let total: f64 = weights.iter().sum();
    weights.into_iter().map(|weight| weight / total).collect()
}

/// 用 leave-one-out 记录拟合温度；返回 (tau, 是否通过门槛)。
fn fit_temperature(eligible: &[&LabelFeatures]) -> Option<(f64, bool)> {
    if eligible.len() < 2 {
        return None;
    }
    let mut records: Vec<(Vec<f64>, usize)> = Vec::new();

    for (truth, label) in eligible.iter().enumerate() {
        for sample_index in 0..label.samples.len() {
            let query = &label.samples[sample_index];
            let scores: Vec<f64> = eligible
                .iter()
                .enumerate()
                .map(|(other, candidate)| {
                    if other == truth {
                        // 自己的 label 必须排除被查询的这条，否则距离恒为 0，
                        // 留出拟合就变成了"背答案"。
                        let refs: Vec<Vec<f64>> = candidate
                            .samples
                            .iter()
                            .enumerate()
                            .filter(|(index, _)| *index != sample_index)
                            .map(|(_, feature)| feature.clone())
                            .collect();
                        let spread = leave_one_out_spread(&refs);
                        let distance = nearest(query, &refs);
                        ratio_score(distance, spread).1
                    } else {
                        let distance = nearest(query, &candidate.samples);
                        ratio_score(distance, candidate.spread).1
                    }
                })
                .collect();
            records.push((standardize(&scores), truth));
        }
    }
    if records.len() < 4 {
        return None;
    }

    let correct = records
        .iter()
        .filter(|(scores, truth)| {
            let mut best = 0usize;
            for index in 1..scores.len() {
                if scores[index] > scores[best] {
                    best = index;
                }
            }
            best == *truth
        })
        .count();
    let accuracy = correct as f64 / records.len() as f64;
    let chance = 1.0 / eligible.len() as f64;
    let uniform_nll = (eligible.len() as f64).ln();

    let mut best_tau = TAU_MIN;
    let mut best_nll = f64::INFINITY;
    for step in 0..TAU_STEPS {
        let fraction = step as f64 / (TAU_STEPS - 1) as f64;
        let tau = (TAU_MIN.ln() + fraction * (TAU_MAX.ln() - TAU_MIN.ln())).exp();
        let loss = mean(
            &records
                .iter()
                .map(|(scores, truth)| {
                    let logits: Vec<f64> = scores.iter().map(|score| tau * score).collect();
                    log_sum_exp(&logits) - logits[*truth]
                })
                .collect::<Vec<_>>(),
        );
        if loss < best_nll {
            best_nll = loss;
            best_tau = tau;
        }
    }

    Some((best_tau, accuracy > chance && best_nll < uniform_nll))
}

/// 本地证据层入口。`samples` 是 `(label, 解析后的整数)`，label 由存储层规范化。
pub fn analyze(
    queries: &[Vec<i32>],
    artifact: &Artifact,
    samples: &[(String, Vec<i32>)],
) -> Result<LocalResult, String> {
    if queries.is_empty() {
        return Err("没有可用于本地匹配的回答".into());
    }
    let parameters = &artifact.ranker.full_params;
    let query_features: Vec<Vec<f64>> = queries
        .iter()
        .map(|numbers| full_feature(numbers, parameters))
        .collect::<Result<_, _>>()?;

    let mut grouped: BTreeMap<String, Vec<Vec<i32>>> = BTreeMap::new();
    for (label, numbers) in samples {
        grouped.entry(label.clone()).or_default().push(numbers.clone());
    }

    let mut labels: Vec<LabelFeatures> = Vec::new();
    for (label, numbers) in grouped {
        if numbers.len() < MIN_RANK_SAMPLES {
            continue;
        }
        let features: Vec<Vec<f64>> = numbers
            .iter()
            .map(|item| full_feature(item, parameters))
            .collect::<Result<_, _>>()?;
        let spread = leave_one_out_spread(&features);
        let official = artifact
            .model_ids
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&label));
        labels.push(LabelFeatures {
            label,
            samples: features,
            spread,
            official,
        });
    }

    if labels.is_empty() {
        return Ok(LocalResult {
            candidates: Vec::new(),
            calibrated: false,
            eligible_labels: 0,
            total_labels: 0,
        });
    }

    let eligible: Vec<&LabelFeatures> = labels
        .iter()
        .filter(|label| label.samples.len() >= MIN_PROBABILITY_SAMPLES)
        .collect();
    let fitted = if eligible.len() >= 2 {
        fit_temperature(&eligible)
    } else {
        None
    };
    let (calibrated, tau) = match fitted {
        Some((tau, true)) => (true, Some(tau)),
        _ => (false, None),
    };

    let mut probabilities: BTreeMap<String, f64> = BTreeMap::new();
    if let (true, Some(tau)) = (calibrated, tau) {
        let scores: Vec<f64> = eligible
            .iter()
            .map(|label| {
                let distances: Vec<f64> = query_features
                    .iter()
                    .map(|query| nearest(query, &label.samples))
                    .collect();
                ratio_score(median(&distances), label.spread).1
            })
            .collect();
        let normalized = standardize(&scores);
        let logits: Vec<f64> = normalized.iter().map(|score| tau * score).collect();
        for (label, probability) in eligible.iter().zip(softmax(&logits)) {
            probabilities.insert(label.label.clone(), probability);
        }
    }

    let mut candidates: Vec<LocalCandidate> = labels
        .iter()
        .map(|label| {
            let distances: Vec<f64> = query_features
                .iter()
                .map(|query| nearest(query, &label.samples))
                .collect();
            let (ratio, score) = ratio_score(median(&distances), label.spread);
            LocalCandidate {
                label: label.label.clone(),
                samples: label.samples.len(),
                distance_ratio: ratio,
                score,
                probability: probabilities.get(&label.label).copied(),
                official: label.official,
            }
        })
        .collect();
    candidates.sort_by(|left, right| right.score.total_cmp(&left.score));

    Ok(LocalResult {
        candidates,
        calibrated,
        eligible_labels: eligible.len(),
        total_labels: labels.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::codec::decode_artifact;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Deserialize)]
    struct LabelCase {
        label: String,
        samples: Vec<Vec<i32>>,
    }

    #[derive(Deserialize)]
    struct Fixture {
        labels: Vec<LabelCase>,
        queries: Vec<Vec<i32>>,
    }

    fn artifact() -> Artifact {
        let bytes = zstd::stream::decode_all(Cursor::new(include_bytes!(
            "../../assets/fingerprint/shared_detector.bin.zst"
        )))
        .unwrap();
        decode_artifact(&bytes).unwrap()
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("fixtures/local_cases.json")).unwrap()
    }

    fn samples(fixture: &Fixture) -> Vec<(String, Vec<i32>)> {
        fixture
            .labels
            .iter()
            .flat_map(|label| {
                label
                    .samples
                    .iter()
                    .map(|numbers| (label.label.clone(), numbers.clone()))
            })
            .collect()
    }

    #[test]
    fn the_query_label_ranks_first_and_gets_a_probability() {
        let fixture = fixture();
        let result = analyze(&fixture.queries, &artifact(), &samples(&fixture)).unwrap();
        assert_eq!(result.total_labels, 2);
        assert_eq!(result.eligible_labels, 2);
        assert_eq!(result.candidates[0].label, "gpt-5.5");
        assert!(result.calibrated, "6+ 样本/类应能拟合出本地温度");
        let probability = result.candidates[0].probability.unwrap();
        assert!(probability > 0.5, "本地 top-1 概率应明显高于随机：{probability}");
    }

    #[test]
    fn fewer_than_six_samples_gives_ranking_but_no_probability() {
        let fixture = fixture();
        let reduced: Vec<(String, Vec<i32>)> = fixture
            .labels
            .iter()
            .flat_map(|label| {
                label
                    .samples
                    .iter()
                    .take(3)
                    .map(|numbers| (label.label.clone(), numbers.clone()))
            })
            .collect();
        let result = analyze(&fixture.queries, &artifact(), &reduced).unwrap();
        assert_eq!(result.total_labels, 2);
        assert_eq!(result.eligible_labels, 0);
        assert!(!result.calibrated);
        assert_eq!(result.candidates[0].label, "gpt-5.5");
        assert!(result.candidates.iter().all(|item| item.probability.is_none()));
    }

    #[test]
    fn labels_with_fewer_than_three_samples_are_not_used() {
        let fixture = fixture();
        let mut reduced = samples(&fixture);
        reduced.push(("singleton".into(), fixture.queries[0].clone()));
        let result = analyze(&fixture.queries, &artifact(), &reduced).unwrap();
        assert_eq!(result.total_labels, 2);
        assert!(result.candidates.iter().all(|item| item.label != "singleton"));
    }
}
