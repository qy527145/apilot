//! 位置/顺序判别头：751 维特征 → 每个身份的判别值。
//!
//! 与 lm-detector 的 `shared/positional.ts::positionalDecision` 对齐。这里的
//! `params`/`weights`/`bias` 都来自 artifact；块权重是固定值
//! `[.6,.15,.125,.125]`，训练时与导出时用的是同一组。

use crate::fingerprint::codec::PositionalHead;
use crate::fingerprint::features::{
    dot, normalized, positional_blocks, POSITIONAL_BLOCK_WEIGHTS, POSITIONAL_DIM,
};

pub fn positional_decision(
    numbers: &[i32],
    head: &PositionalHead,
) -> Result<Vec<f64>, String> {
    let blocks = positional_blocks(numbers);
    if head.params.len() != blocks.len() {
        return Err(format!(
            "位置头参数块数不匹配：{} / {}",
            head.params.len(),
            blocks.len()
        ));
    }
    if head.weights.len() != head.bias.len() {
        return Err("位置头权重与偏置长度不一致".into());
    }

    let mut features = Vec::with_capacity(POSITIONAL_DIM);
    for (index, block) in blocks.iter().enumerate() {
        let parameter = &head.params[index];
        if block.len() != parameter.mean.len() || block.len() != parameter.scale.len() {
            return Err(format!(
                "位置头第 {index} 块维度不匹配：{} / {} / {}",
                block.len(),
                parameter.mean.len(),
                parameter.scale.len()
            ));
        }
        let scaled: Vec<f64> = block
            .iter()
            .enumerate()
            .map(|(position, value)| {
                (value - parameter.mean[position]) / parameter.scale[position]
            })
            .collect();
        let unit = normalized(&scaled);
        let weight = POSITIONAL_BLOCK_WEIGHTS[index].sqrt();
        features.extend(unit.into_iter().map(|value| value * weight));
    }
    if features.len() != POSITIONAL_DIM {
        return Err(format!(
            "位置头特征维度应为 {POSITIONAL_DIM}，实际 {}",
            features.len()
        ));
    }

    let mut decision = Vec::with_capacity(head.weights.len());
    for (index, row) in head.weights.iter().enumerate() {
        if row.len() != features.len() {
            return Err(format!(
                "位置头权重第 {index} 行维度不匹配：{} / {}",
                row.len(),
                features.len()
            ));
        }
        decision.push(dot(row, &features) + head.bias[index]);
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::codec::decode_artifact;
    use serde::Deserialize;
    use std::io::Cursor;

    #[derive(Deserialize)]
    struct Case {
        numbers: Vec<i32>,
        positional: Vec<Option<f64>>,
    }

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }

    fn artifact() -> crate::fingerprint::codec::Artifact {
        let bytes = zstd::stream::decode_all(Cursor::new(include_bytes!(
            "../../assets/fingerprint/shared_detector.bin.zst"
        )))
        .unwrap();
        decode_artifact(&bytes).unwrap()
    }

    #[test]
    fn matches_the_type_script_positional_head() {
        let artifact = artifact();
        let fixture: Fixture =
            serde_json::from_str(include_str!("fixtures/features_cases.json")).unwrap();
        // 第二个 fixture 只有 4 个数，远离真实检测的输入范围；位置头在那种
        // 输入上本来就会产生 NaN（JS 也一样），因此只比对 300 个数的那条。
        let case = fixture
            .cases
            .into_iter()
            .find(|case| case.numbers.len() >= 80)
            .expect("fixture 里应有一条长回答");
        let expected: Vec<f64> = case
            .positional
            .into_iter()
            .map(|value| value.expect("长回答的位置头输出不应有 null"))
            .collect();
        let actual = positional_decision(&case.numbers, &artifact.ranker.positional).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (index, (a, e)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (a - e).abs() <= 1e-9 * e.abs().max(1.0),
                "positional[{index}]: {a} != {e}"
            );
        }
    }
}
