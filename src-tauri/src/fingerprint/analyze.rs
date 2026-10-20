//! 把回答、诊断、排名和校准组装成一次检测结果。
//!
//! 对应 lm-detector 的 `shared/shared-detector.ts::analyzeSharedOutputs`。
//! 状态机与 TypeScript 一致：
//!
//! ```text
//! used == 0                  → unscorable（没有有效回答）
//! allowPartial && 1..=2 有效 → partial（有排名、无置信度）
//! 3 条不齐                   → unscorable
//! 序列形态不像语言模型       → not_model（不排名）
//! 其余                       → ok（排名 + 校准概率）
//! ```

use crate::fingerprint::codec::Artifact;
use crate::fingerprint::features::{mean, median};
use crate::fingerprint::parse::{parse_numbers, raw_numbers};
use crate::fingerprint::ranker::{calibrate_ranking, rank_shared_numbers};
use crate::fingerprint::types::{
    Analysis, CalibrationInfo, Candidate, Diagnostic, Evidence, LibraryMetadata, ModelVersion,
    Output, SequenceShape,
};

/// 序列形态门槛：参考库三条回答组的 99.5% 分位数。
const AUTOCORRELATION_LIMIT: f64 = -0.23;
const SMALL_STEP_SHARE_LIMIT: f64 = 0.05;

fn sequence_shape(answers: &[Vec<i32>]) -> SequenceShape {
    let mut autocorrelations = Vec::with_capacity(answers.len());
    let mut shares = Vec::with_capacity(answers.len());

    for answer in answers {
        let values: Vec<f64> = answer.iter().map(|&value| value as f64).collect();
        if values.len() < 2 {
            autocorrelations.push(1.0);
            shares.push(0.0);
            continue;
        }
        let centre = mean(&values);
        let variance = mean(
            &values
                .iter()
                .map(|value| (value - centre).powi(2))
                .collect::<Vec<_>>(),
        );
        let steps: Vec<f64> = (0..values.len() - 1)
            .map(|index| values[index + 1] - values[index])
            .collect();
        let autocorrelation = if variance == 0.0 {
            1.0
        } else {
            let products: Vec<f64> = (0..values.len() - 1)
                .map(|index| (values[index + 1] - centre) * (values[index] - centre))
                .collect();
            mean(&products) / variance
        };
        let share = mean(
            &steps
                .iter()
                .map(|step| if step.abs() <= 20.0 { 1.0 } else { 0.0 })
                .collect::<Vec<_>>(),
        );
        autocorrelations.push(autocorrelation);
        shares.push(share);
    }

    let autocorrelation = median(&autocorrelations);
    let small_step_share = median(&shares);
    SequenceShape {
        autocorrelation,
        small_step_share,
        model_like: autocorrelation <= AUTOCORRELATION_LIMIT
            && small_step_share <= SMALL_STEP_SHARE_LIMIT,
    }
}

fn candidate(
    index: usize,
    score: f64,
    probability: Option<f64>,
    artifact: &Artifact,
    metadata: &LibraryMetadata,
) -> Candidate {
    let id = &artifact.model_ids[index];
    let (display_name, family, family_name) = match metadata.find(id) {
        Some(info) => {
            let (family, family_name) = info.family_label();
            (
                info.display().to_string(),
                family.to_string(),
                family_name.to_string(),
            )
        }
        None => (id.clone(), "other".to_string(), "其他".to_string()),
    };
    Candidate {
        model: id.clone(),
        display_name,
        family,
        family_name,
        score,
        probability,
    }
}

fn sorted_indices(ranking: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ranking.len()).collect();
    // JS 的 `sort((i, j) => ranking[j] - ranking[i])`；V8 的排序是稳定的，
    // Rust 的 `sort_by` 也是稳定排序，因此同分时都保留模型顺序。
    order.sort_by(|left, right| ranking[*right].total_cmp(&ranking[*left]));
    order
}

fn model_version(artifact: &Artifact) -> ModelVersion {
    ModelVersion {
        base_sha256: artifact.base_sha256.clone(),
    }
}

fn base_diagnostics(outputs: &[Output]) -> (Vec<Vec<i32>>, Vec<Diagnostic>) {
    let parsed: Vec<Vec<i32>> = outputs
        .iter()
        .map(|output| parse_numbers(&output.text))
        .collect();
    let diagnostics = outputs
        .iter()
        .enumerate()
        .map(|(index, output)| {
            let minimum = ((output.expected_count as f64) * 0.55)
                .ceil()
                .max(80.0) as usize;
            let raw = raw_numbers(&output.text);
            Diagnostic {
                index,
                parsed_numbers: parsed[index].len(),
                minimum_numbers: minimum,
                accepted: parsed[index].len() >= minimum,
                raw_out_of_range_count: raw
                    .iter()
                    .filter(|&&value| !(1..=355).contains(&value))
                    .count(),
            }
        })
        .collect();
    (parsed, diagnostics)
}

/// 与 `analyzeSharedOutputs` 同口径。`allow_partial` 控制 1–2 条有效回答时
/// 是否给"只有排名、没有概率"的部分结果；渠道测试默认开启。
pub fn analyze_shared_outputs(
    outputs: &[Output],
    metadata: &LibraryMetadata,
    artifact: &Artifact,
    allow_partial: bool,
) -> Result<Analysis, String> {
    let (parsed, diagnostics) = base_diagnostics(outputs);
    let accepted: Vec<Vec<i32>> = parsed
        .iter()
        .enumerate()
        .filter(|(index, _)| diagnostics[*index].accepted)
        .map(|(_, numbers)| numbers.clone())
        .collect();
    let used = accepted.len();
    let sequence = if used > 0 {
        Some(sequence_shape(&accepted))
    } else {
        None
    };
    let version = model_version(artifact);

    let common_evidence = |label: &str, reason: String, method: &str| Evidence {
        insufficient: true,
        label: label.to_string(),
        reason,
        threshold: None,
        method: method.to_string(),
    };

    if allow_partial && used > 0 && used < 3 && outputs.len() <= 3 {
        if !sequence.as_ref().is_some_and(|shape| shape.model_like) {
            return Ok(Analysis {
                prediction: String::new(),
                prediction_name: "不像语言模型回答".into(),
                family_prediction: None,
                family_prediction_name: String::new(),
                probability: None,
                probability_status: "unavailable".into(),
                probability_scope: None,
                decision: "not_model".into(),
                used_outputs: used,
                diagnostics,
                sequence,
                results: Vec::new(),
                ranking_score: None,
                calibration: None,
                method: "shared-detector-v2".into(),
                evidence: common_evidence(
                    "不像语言模型回答",
                    "相邻数字的变化方式与参考库中的语言模型回答不同，常见于程序生成、排序或打乱顺序的数字。未生成排名。".into(),
                    "sequence-shape",
                ),
                model_version: version,
            });
        }

        let ranking = rank_shared_numbers(&accepted, artifact)?;
        let order = sorted_indices(&ranking);
        let results: Vec<Candidate> = order
            .iter()
            .map(|&index| candidate(index, ranking[index], None, artifact, metadata))
            .collect();
        let first = order[0];
        return Ok(Analysis {
            prediction: results[0].model.clone(),
            prediction_name: results[0].display_name.clone(),
            family_prediction: Some(results[0].family.clone()),
            family_prediction_name: results[0].family_name.clone(),
            probability: None,
            probability_status: "unavailable".into(),
            probability_scope: None,
            decision: "partial".into(),
            used_outputs: used,
            diagnostics,
            sequence,
            results,
            ranking_score: Some(ranking[first]),
            calibration: None,
            method: "shared-ranker-partial-v1".into(),
            evidence: common_evidence(
                "部分样本排名",
                format!("使用 {used}/3 条有效回答生成排名。补齐三条有效回答后才能计算置信度。"),
                "partial-sample-ranking",
            ),
            model_version: version,
        });
    }

    if outputs.len() != 3 || used != 3 {
        return Ok(Analysis {
            prediction: String::new(),
            prediction_name: "暂不可评分".into(),
            family_prediction: None,
            family_prediction_name: String::new(),
            probability: None,
            probability_status: "unavailable".into(),
            probability_scope: None,
            decision: "unscorable".into(),
            used_outputs: used,
            diagnostics,
            sequence,
            results: Vec::new(),
            ranking_score: None,
            calibration: None,
            method: "shared-detector-v2".into(),
            evidence: common_evidence(
                "需要三条完整回答",
                format!(
                    "当前有 {used}/{} 条有效回答。请补齐原来的三条回答后重新检测。",
                    outputs.len()
                ),
                "complete-three-answers",
            ),
            model_version: version,
        });
    }

    if !sequence.as_ref().is_some_and(|shape| shape.model_like) {
        return Ok(Analysis {
            prediction: String::new(),
            prediction_name: "不像语言模型回答".into(),
            family_prediction: None,
            family_prediction_name: String::new(),
            probability: None,
            probability_status: "unavailable".into(),
            probability_scope: None,
            decision: "not_model".into(),
            used_outputs: used,
            diagnostics,
            sequence,
            results: Vec::new(),
            ranking_score: None,
            calibration: None,
            method: "shared-detector-v2".into(),
            evidence: common_evidence(
                "不像语言模型回答",
                "相邻数字的变化方式与参考库中的语言模型回答不同，常见于程序生成、排序或打乱顺序的数字。未生成排名。".into(),
                "sequence-shape",
            ),
            model_version: version,
        });
    }

    let ranking = rank_shared_numbers(&parsed, artifact)?;
    let probabilities = calibrate_ranking(&ranking, artifact);
    let order = sorted_indices(&ranking);
    let results: Vec<Candidate> = order
        .iter()
        .map(|&index| {
            candidate(
                index,
                ranking[index],
                probabilities.as_ref().map(|values| values[index]),
                artifact,
                metadata,
            )
        })
        .collect();
    let first = order[0];
    let calibration = probabilities.as_ref().and_then(|_| {
        artifact.calibration.as_ref().map(|calibration| CalibrationInfo {
            method: calibration.method.clone(),
            run: calibration.calibration_run.clone(),
            tau: calibration.tau,
            sha256: artifact.calibration_sha256.clone(),
            status: "reference_calibrated".into(),
            scope: Some("reference-closed-set".into()),
        })
    });

    Ok(Analysis {
        prediction: results[0].model.clone(),
        prediction_name: results[0].display_name.clone(),
        family_prediction: Some(results[0].family.clone()),
        family_prediction_name: results[0].family_name.clone(),
        probability: results[0].probability,
        probability_status: if probabilities.is_some() {
            "reference_calibrated".into()
        } else {
            "unavailable".into()
        },
        probability_scope: probabilities
            .as_ref()
            .map(|_| "reference-closed-set".to_string()),
        decision: "ok".into(),
        used_outputs: used,
        diagnostics,
        sequence,
        results,
        ranking_score: Some(ranking[first]),
        calibration,
        method: "shared-detector-v2".into(),
        evidence: if probabilities.is_some() {
            common_evidence(
                "库内校准排名",
                "置信度是参考库内的闭集相对概率，不包含库外模型。".into(),
                "reference-calibrated-ranking",
            )
        } else {
            common_evidence(
                "未校准排名",
                "校准参数与当前检测器不匹配，置信度不可用。候选顺序由排名分数决定。".into(),
                "uncalibrated-ranking",
            )
        },
        model_version: version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::codec::decode_artifact;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Deserialize)]
    struct FixtureOutput {
        text: String,
        expected_count: u32,
    }

    #[derive(Deserialize)]
    struct FixtureAnalysis {
        prediction: String,
        probability: Option<f64>,
        used_outputs: usize,
        results: Vec<Candidate>,
        diagnostics: Vec<Diagnostic>,
        sequence: Option<SequenceShape>,
    }

    #[derive(Deserialize)]
    struct Case {
        outputs: Vec<FixtureOutput>,
        analysis: FixtureAnalysis,
    }

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }

    fn artifact() -> Artifact {
        let bytes = zstd::stream::decode_all(Cursor::new(include_bytes!(
            "../../assets/fingerprint/shared_detector.bin.zst"
        )))
        .unwrap();
        decode_artifact(&bytes).unwrap()
    }

    fn metadata() -> LibraryMetadata {
        serde_json::from_str(include_str!("../../assets/fingerprint/models.json")).unwrap()
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("fixtures/ranking_cases.json")).unwrap()
    }

    #[test]
    fn full_analysis_matches_the_type_script_implementation() {
        let artifact = artifact();
        let metadata = metadata();
        for case in fixture().cases {
            let outputs: Vec<Output> = case
                .outputs
                .iter()
                .map(|output| Output {
                    text: output.text.clone(),
                    expected_count: output.expected_count,
                })
                .collect();
            let actual = analyze_shared_outputs(&outputs, &metadata, &artifact, true).unwrap();
            assert_eq!(actual.decision, "ok");
            assert_eq!(actual.prediction, case.analysis.prediction);
            assert_eq!(actual.used_outputs, case.analysis.used_outputs);
            assert!(
                (actual.probability.unwrap() - case.analysis.probability.unwrap()).abs() <= 1e-9
            );
            assert_eq!(actual.results.len(), case.analysis.results.len());
            for (position, (a, e)) in actual.results.iter().zip(&case.analysis.results).enumerate() {
                assert_eq!(a.model, e.model, "第 {position} 名");
                assert!((a.score - e.score).abs() <= 1e-9, "score 第 {position} 名");
                assert!(
                    (a.probability.unwrap() - e.probability.unwrap()).abs() <= 1e-9,
                    "probability 第 {position} 名"
                );
            }
            assert_eq!(actual.diagnostics.len(), case.analysis.diagnostics.len());
            for (a, e) in actual.diagnostics.iter().zip(&case.analysis.diagnostics) {
                assert_eq!(a.parsed_numbers, e.parsed_numbers);
                assert_eq!(a.minimum_numbers, e.minimum_numbers);
                assert_eq!(a.accepted, e.accepted);
                assert_eq!(a.raw_out_of_range_count, e.raw_out_of_range_count);
            }
            let a = actual.sequence.unwrap();
            let e = case.analysis.sequence.unwrap();
            assert!((a.autocorrelation - e.autocorrelation).abs() <= 1e-9);
            assert!((a.small_step_share - e.small_step_share).abs() <= 1e-9);
            assert_eq!(a.model_like, e.model_like);
        }
    }

    fn random_answer(seed: u32, count: usize) -> String {
        let mut state = seed;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            values.push(1 + (state % 355));
        }
        values
            .iter()
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn programmatic_numbers_are_rejected_before_ranking() {
        let artifact = artifact();
        let metadata = metadata();
        let outputs: Vec<Output> = (0..3)
            .map(|index| Output {
                text: random_answer(100 + index, 300),
                expected_count: 300,
            })
            .collect();
        let analysis = analyze_shared_outputs(&outputs, &metadata, &artifact, true).unwrap();
        assert_eq!(analysis.decision, "not_model");
        assert!(analysis.results.is_empty());
        assert!(analysis.probability.is_none());
    }

    #[test]
    fn two_valid_answers_give_a_partial_ranking_without_probability() {
        let artifact = artifact();
        let metadata = metadata();
        let case = fixture().cases.into_iter().next().unwrap();
        let outputs = vec![
            Output {
                text: case.outputs[0].text.clone(),
                expected_count: case.outputs[0].expected_count,
            },
            Output {
                text: case.outputs[1].text.clone(),
                expected_count: case.outputs[1].expected_count,
            },
            Output {
                text: String::new(),
                expected_count: case.outputs[2].expected_count,
            },
        ];
        let analysis = analyze_shared_outputs(&outputs, &metadata, &artifact, true).unwrap();
        assert_eq!(analysis.decision, "partial");
        assert_eq!(analysis.used_outputs, 2);
        assert!(analysis.probability.is_none());
        assert!(!analysis.results.is_empty());
        assert!(analysis.results.iter().all(|item| item.probability.is_none()));
    }

    #[test]
    fn missing_answers_are_unscorable_without_partial_mode() {
        let artifact = artifact();
        let metadata = metadata();
        let case = fixture().cases.into_iter().next().unwrap();
        let outputs = vec![Output {
            text: case.outputs[0].text.clone(),
            expected_count: case.outputs[0].expected_count,
        }];
        let analysis = analyze_shared_outputs(&outputs, &metadata, &artifact, false).unwrap();
        assert_eq!(analysis.decision, "unscorable");
        assert_eq!(analysis.used_outputs, 1);
    }
}
