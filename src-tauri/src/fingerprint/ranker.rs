//! 冻结排名器：频率 LDA、kNN、nuisance baseline、位置头与温度校准。
//!
//! 对应 lm-detector 的 `shared/shared-detector.ts::rankSharedNumbers`。
//! 每一步的求和顺序、标准化对象（每次回答先 z 再按模型取列均值，然后再 z）
//! 都必须与 TypeScript 一致；golden fixture 会逐项比对。

use crate::fingerprint::codec::{Artifact, BankArtifact};
use crate::fingerprint::features::{
    dot, hellinger_feature, mean, median, normalized, ordered_block_feature, standardize,
    subtract_basis, transform,
};
use crate::fingerprint::parse::count_numbers;
use crate::fingerprint::positional::positional_decision;

/// 一个回答到一个模型参考集的最近邻距离：7 个最小平方距离的均值。
/// 参考不足 7 条时取全部（与 JS `slice(0, 7)` 一致）。
pub fn nearest(query: &[f64], references: &[Vec<f64>]) -> f64 {
    if references.is_empty() {
        return f64::NAN;
    }
    let query_norm = dot(query, query);
    let mut distances: Vec<f64> = references
        .iter()
        .map(|reference| {
            let reference_norm = dot(reference, reference);
            (query_norm + reference_norm - 2.0 * dot(query, reference)).max(0.0)
        })
        .collect();
    distances.sort_by(|left, right| left.total_cmp(right));
    let take = distances.len().min(7);
    distances[..take].iter().sum::<f64>() / take as f64
}

fn column_mean(matrix: &[Vec<f64>]) -> Vec<f64> {
    if matrix.is_empty() {
        return Vec::new();
    }
    let columns = matrix[0].len();
    (0..columns)
        .map(|column| mean(&matrix.iter().map(|row| row[column]).collect::<Vec<_>>()))
        .collect()
}

fn centered(values: &[f64], mean_values: &[f64], scale_values: &[f64]) -> Result<Vec<f64>, String> {
    if values.len() != mean_values.len() || values.len() != scale_values.len() {
        return Err(format!(
            "特征维度不匹配：{} / {} / {}",
            values.len(),
            mean_values.len(),
            scale_values.len()
        ));
    }
    Ok(values
        .iter()
        .enumerate()
        .map(|(index, value)| (value - mean_values[index]) / scale_values[index])
        .collect())
}

/// nuisance 投影后的环境模板 + 模型中心融合，输出每个身份的 baseline 分数。
fn baseline(blocks: &[Vec<f64>], bank: &BankArtifact) -> Result<Vec<f64>, String> {
    if blocks.len() != 2 {
        return Err("baseline 需要频率与位置分块两块特征".into());
    }
    let hellinger = &bank.hellinger;
    let ordered = &bank.ordered_blocks;
    let model_count = hellinger.centroids.len();
    if ordered.centroids.len() != model_count {
        return Err("hellinger 与 ordered 的模型数量不一致".into());
    }
    if ordered.environment_centroids.is_empty() {
        return Err("environment_centroids 为空".into());
    }
    for environment in &ordered.environment_centroids {
        if environment.len() != model_count {
            return Err("environment_centroids 行数与模型数量不一致".into());
        }
    }

    let h_centered = centered(&blocks[0], &hellinger.feature_mean, &hellinger.feature_scale)?;
    let h_unit = normalized(&subtract_basis(&h_centered, &hellinger.nuisance_basis));
    let marginal: Vec<f64> = hellinger
        .centroids
        .iter()
        .map(|centroid| dot(&h_unit, centroid))
        .collect();

    let o_centered = centered(&blocks[1], &ordered.feature_mean, &ordered.feature_scale)?;
    let raw = normalized(&o_centered);
    let projected = normalized(&subtract_basis(&o_centered, &ordered.nuisance_basis));
    let templates: Vec<f64> = (0..model_count)
        .map(|model| {
            ordered
                .environment_centroids
                .iter()
                .map(|environment| dot(&raw, &environment[model]))
                .fold(f64::NEG_INFINITY, f64::max)
        })
        .collect();
    let nuisance: Vec<f64> = ordered
        .centroids
        .iter()
        .map(|centroid| dot(&projected, centroid))
        .collect();

    let template_z = standardize(&templates);
    let nuisance_z = standardize(&nuisance);
    let ordered_scores: Vec<f64> = template_z
        .iter()
        .zip(&nuisance_z)
        .map(|(template, nuisance)| 0.5 * template + 0.5 * nuisance)
        .collect();
    let ordered_scores = standardize(&ordered_scores);
    let marginal_z = standardize(&marginal);

    Ok(marginal_z
        .iter()
        .zip(&ordered_scores)
        .map(|(marginal, ordered)| 0.5 * marginal + 0.5 * ordered)
        .collect())
}

/// 对一组回答（通常 3 条）计算每个身份的原始排名分数。
pub fn rank_shared_numbers(
    numbers: &[Vec<i32>],
    artifact: &Artifact,
) -> Result<Vec<f64>, String> {
    if numbers.is_empty() {
        return Err("没有可排名的回答".into());
    }
    let ranker = &artifact.ranker;
    let model_count = artifact.model_ids.len();
    if ranker.references.len() != model_count {
        return Err("参考集数量与模型数量不一致".into());
    }

    let full: Vec<Vec<Vec<f64>>> = numbers
        .iter()
        .map(|answer| {
            vec![
                hellinger_feature(&count_numbers(answer)),
                ordered_block_feature(answer),
            ]
        })
        .collect();
    let full_transformed: Vec<Vec<f64>> = full
        .iter()
        .map(|blocks| transform(blocks, &ranker.full_params))
        .collect::<Result<_, _>>()?;

    let mut lda_rows = Vec::with_capacity(numbers.len());
    for answer in numbers {
        let head = &answer[..answer.len().min(128)];
        let blocks = vec![
            hellinger_feature(&count_numbers(head)),
            ordered_block_feature(head),
        ];
        let features = transform(&blocks, &ranker.head_params)?;
        let decisions: Vec<f64> = ranker
            .lda_weights
            .iter()
            .enumerate()
            .map(|(index, row)| dot(row, &features) + ranker.lda_bias[index])
            .collect();
        lda_rows.push(standardize(&decisions));
    }
    let frequency_lda = standardize(&column_mean(&lda_rows));

    let near: Vec<f64> = (0..model_count)
        .map(|model| {
            let distances: Vec<f64> = full_transformed
                .iter()
                .map(|answer| -nearest(answer, &ranker.references[model]))
                .collect();
            median(&distances)
        })
        .collect();
    let near = standardize(&near);

    let baseline_rows: Vec<Vec<f64>> = full
        .iter()
        .map(|blocks| baseline(blocks, &ranker.bank))
        .collect::<Result<_, _>>()?;
    let baseline_scores = standardize(&column_mean(&baseline_rows));

    let frequency: Vec<f64> = (0..model_count)
        .map(|model| {
            0.5 * frequency_lda[model] + 0.25 * near[model] + 0.25 * baseline_scores[model]
        })
        .collect();
    let frequency = standardize(&frequency);

    let positional_rows: Vec<Vec<f64>> = numbers
        .iter()
        .map(|answer| positional_decision(answer, &ranker.positional).map(|row| standardize(&row)))
        .collect::<Result<_, _>>()?;
    let positional = standardize(&column_mean(&positional_rows));

    let ranking: Vec<f64> = (0..model_count)
        .map(|model| {
            ranker.blend.frequency * frequency[model] + ranker.blend.positional * positional[model]
        })
        .collect();
    if ranking.iter().any(|value| !value.is_finite()) {
        return Err("排名计算产生无效数值".into());
    }
    Ok(ranking)
}

/// 闭集温度 softmax；校准不可用时返回 None（只给排名，不给概率）。
pub fn calibrate_ranking(ranking: &[f64], artifact: &Artifact) -> Option<Vec<f64>> {
    if !artifact.calibration_matches() {
        return None;
    }
    let calibration = artifact.calibration.as_ref()?;
    if ranking.len() != artifact.model_ids.len()
        || ranking.iter().any(|value| !value.is_finite())
    {
        return None;
    }
    let logits: Vec<f64> = ranking.iter().map(|score| calibration.tau * score).collect();
    let maximum = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weights: Vec<f64> = logits.iter().map(|value| (value - maximum).exp()).collect();
    let total: f64 = weights.iter().sum();
    Some(weights.into_iter().map(|weight| weight / total).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::codec::decode_artifact;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Deserialize)]
    struct Candidate {
        model: String,
        score: f64,
        probability: Option<f64>,
    }

    #[derive(Deserialize)]
    struct Analysis {
        results: Vec<Candidate>,
    }

    #[derive(Deserialize)]
    struct Case {
        parsed: Vec<Vec<i32>>,
        analysis: Analysis,
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

    #[test]
    fn ranking_and_calibration_match_the_type_script_implementation() {
        let artifact = artifact();
        let fixture: Fixture =
            serde_json::from_str(include_str!("fixtures/ranking_cases.json")).unwrap();
        for case in fixture.cases {
            let ranking = rank_shared_numbers(&case.parsed, &artifact).unwrap();
            let probabilities = calibrate_ranking(&ranking, &artifact).unwrap();

            let mut order: Vec<usize> = (0..ranking.len()).collect();
            order.sort_by(|left, right| ranking[*right].total_cmp(&ranking[*left]));
            assert_eq!(order.len(), case.analysis.results.len());
            for (position, &model) in order.iter().enumerate() {
                let expected = &case.analysis.results[position];
                assert_eq!(artifact.model_ids[model], expected.model, "第 {position} 名");
                assert!(
                    (ranking[model] - expected.score).abs() <= 1e-9,
                    "score[{model}]: {} != {}",
                    ranking[model],
                    expected.score
                );
                let expected_probability = expected.probability.expect("完整回答应有概率");
                assert!(
                    (probabilities[model] - expected_probability).abs() <= 1e-9,
                    "probability[{model}]: {} != {}",
                    probabilities[model],
                    expected_probability
                );
            }
        }
    }

    #[test]
    fn nearest_is_zero_for_an_exact_reference() {
        let reference = vec![1.0, 2.0, 3.0];
        assert!(nearest(&reference, &[reference.clone()]).abs() < 1e-12);
    }
}
