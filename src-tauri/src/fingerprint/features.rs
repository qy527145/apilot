//! 特征块：频率分桶、位置分块、位置核与序列统计。
//!
//! 全部对齐 lm-detector 的 `shared/fingerprint-core.js` 与
//! `shared/positional.ts`。这里不做任何"优化"（例如把 0.5 先验合并进常数、
//! 改变求和顺序）—— 排名对浮点误差不敏感，但 golden fixture 要的是可重复的
//! 逐项一致，改顺序只会让排查变难。

use crate::fingerprint::parse::{count_numbers, DIMENSION};

pub const ALPHA: f64 = 0.5;
pub const ORDERED_BLOCK_DIM: usize = 4 * 16 + 10;
pub const POSITION_KERNEL_DIM: usize = 34 * 3;
pub const SEQUENCE_DIM: usize = 220;
pub const POSITIONAL_DIM: usize =
    DIMENSION + ORDERED_BLOCK_DIM + POSITION_KERNEL_DIM + SEQUENCE_DIM;

const LAGS: [usize; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
const POSITIONAL_WEIGHTS: [f64; 4] = [0.6, 0.15, 0.125, 0.125];

/// 一块特征的标准化参数。
#[derive(Debug, Clone, Default)]
pub struct Preprocessing {
    pub mean: Vec<f64>,
    pub scale: Vec<f64>,
}

pub fn mean(values: &[f64]) -> f64 {
    // JS 对空数组返回 NaN；这里保留同样的行为，交给上层的有限性检查。
    if values.is_empty() {
        return f64::NAN;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

pub fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

pub fn norm(values: &[f64]) -> f64 {
    dot(values, values).sqrt()
}

pub fn normalized(values: &[f64]) -> Vec<f64> {
    let scale = norm(values).max(1e-12);
    values.iter().map(|value| value / scale).collect()
}

pub fn standardize(values: &[f64]) -> Vec<f64> {
    let center = mean(values);
    let variance = mean(&values.iter().map(|value| (value - center).powi(2)).collect::<Vec<_>>());
    let scale = variance.sqrt().max(1e-12);
    values.iter().map(|value| (value - center) / scale).collect()
}

pub fn subtract_basis(values: &[f64], basis: &[Vec<f64>]) -> Vec<f64> {
    let mut output = values.to_vec();
    for vector in basis {
        let projection = dot(&output, vector);
        for (index, value) in output.iter_mut().enumerate() {
            *value -= projection * vector[index];
        }
    }
    output
}

/// Hellinger 频率特征：355 维，带 0.5 的加性先验。
pub fn hellinger_feature(counts: &[f64]) -> Vec<f64> {
    let total = counts.iter().sum::<f64>() + ALPHA * DIMENSION as f64;
    counts
        .iter()
        .map(|value| ((value + ALPHA) / total).sqrt())
        .collect()
}

/// 把序列切成 4 段；余数分给前几段（与 JS 的 `splitIntoFour` 一致）。
fn split_into_four(length: usize) -> [(usize, usize); 4] {
    let base = length / 4;
    let remainder = length % 4;
    let mut ranges = [(0usize, 0usize); 4];
    let mut start = 0usize;
    for (index, range) in ranges.iter_mut().enumerate() {
        let size = base + usize::from(index < remainder);
        *range = (start, start + size);
        start += size;
    }
    ranges
}

/// 位置分块：4 段 × 16 个数值桶 + 10 个末位桶，共 74 维。
pub fn ordered_block_feature(numbers: &[i32]) -> Vec<f64> {
    let mut output = Vec::with_capacity(ORDERED_BLOCK_DIM);
    for (start, end) in split_into_four(numbers.len()) {
        let mut bins = [0.5f64; 16];
        for &value in &numbers[start..end] {
            let index = (((value - 1) as f64 / 355.0) * 16.0).floor() as usize;
            bins[index.min(15)] += 1.0;
        }
        let total: f64 = bins.iter().sum();
        output.extend(bins.iter().map(|value| (value / total).sqrt()));
    }

    let mut last_digits = [0.5f64; 10];
    for &value in numbers {
        last_digits[(value % 10) as usize] += 1.0;
    }
    let total: f64 = last_digits.iter().sum();
    output.extend(last_digits.iter().map(|value| (value / total).sqrt()));
    output
}

/// 位置核：24 个数值 RBF + 10 个末位指示，分别乘 3 个相对位置多项式。
/// 输出 102 维。
pub fn position_kernel(numbers: &[i32]) -> Vec<f64> {
    let length = numbers.len();
    let mut output = vec![0.0; POSITION_KERNEL_DIM];
    if length == 0 {
        return output;
    }

    let centres: Vec<f64> = (0..24).map(|index| 1.0 + index as f64 * 354.0 / 23.0).collect();
    let denominator = length as f64;

    for (position, &value) in numbers.iter().enumerate() {
        let t = (position as f64 + 0.5) * 2.0 / denominator - 1.0;
        let basis = [t, 0.5 * (3.0 * t * t - 1.0), 0.5 * (5.0 * t.powi(3) - 3.0 * t)];

        let mut features = [0.0f64; 34];
        for (index, centre) in centres.iter().enumerate() {
            features[index] = (-0.5 * ((value as f64 - centre) / 20.0).powi(2)).exp();
        }
        let digit = value % 10;
        for index in 0..10 {
            features[24 + index] = if index as i32 == digit { 1.0 } else { 0.0 };
        }

        for (feature_index, feature) in features.iter().enumerate() {
            for (basis_index, weight) in basis.iter().enumerate() {
                output[feature_index * 3 + basis_index] += feature * weight / denominator;
            }
        }
    }
    output
}

/// 转移矩阵的"去掉边际"版本：`joint - row * column`。
fn centred_transitions(symbols: &[i32], bins: usize) -> Vec<f64> {
    let mut joint = vec![0.5f64; bins * bins];
    let total = (symbols.len() as f64 - 1.0) + 0.5 * (bins * bins) as f64;
    for pair in symbols.windows(2) {
        joint[pair[0] as usize * bins + pair[1] as usize] += 1.0;
    }
    for value in &mut joint {
        *value /= total;
    }
    let rows: Vec<f64> = (0..bins)
        .map(|row| joint[row * bins..row * bins + bins].iter().sum())
        .collect();
    let columns: Vec<f64> = (0..bins)
        .map(|column| (0..bins).map(|row| joint[row * bins + column]).sum())
        .collect();
    joint
        .iter()
        .enumerate()
        .map(|(index, value)| value - rows[index / bins] * columns[index % bins])
        .collect()
}

fn fraction(length: usize, mut predicate: impl FnMut(usize) -> bool) -> f64 {
    if length == 0 {
        return 0.0;
    }
    let hits = (0..length).filter(|&index| predicate(index)).count();
    hits as f64 / length as f64
}

/// JS `Math.sign`：0 与 -0 都返回 0（Rust 的 `signum` 对 0 返回 1）。
fn js_sign(value: f64) -> f64 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// 序列统计：8 个滞后 × 6 个量 + 两个转移矩阵 + 5 个延续/波动量
/// + 3 个窗口覆盖率，共 220 维。
pub fn sequence_features(numbers: &[i32]) -> Vec<f64> {
    let length = numbers.len();
    let values: Vec<f64> = numbers.iter().map(|&value| value as f64).collect();
    let centre = mean(&values);
    // 注意：TS 原版是 `Math.max(Math.sqrt(...), 1)`，不是 0。真实回答的标准差
    // 约在 100 量级，这个下界通常不生效；但短/退化输入上的行为必须一致。
    let deviation = mean(
        &values
            .iter()
            .map(|value| (value - centre).powi(2))
            .collect::<Vec<_>>(),
    )
    .sqrt()
    .max(1.0);
    let scaled: Vec<f64> = values.iter().map(|value| (value - centre) / deviation).collect();

    let mut output = Vec::with_capacity(SEQUENCE_DIM);
    for &lag in &LAGS {
        let count = length.saturating_sub(lag);
        let differences: Vec<f64> = (0..count)
            .map(|index| values[index + lag] - values[index])
            .collect();
        let products: Vec<f64> = (0..count)
            .map(|index| scaled[index + lag] * scaled[index])
            .collect();
        output.push(mean(&products));
        output.push(mean(&differences.iter().map(|value| value.abs()).collect::<Vec<_>>()) / 355.0);
        output.push(fraction(count, |index| differences[index] > 0.0));
        output.push(fraction(count, |index| differences[index] == 0.0));
        output.push(fraction(count, |index| differences[index].abs() <= 5.0));
        output.push(fraction(count, |index| differences[index].abs() <= 20.0));
    }

    let value_bins: Vec<i32> = numbers
        .iter()
        .map(|&value| (((value - 1) as f64 * 8.0 / 355.0).floor() as i32).min(7))
        .collect();
    output.extend(centred_transitions(&value_bins, 8));

    let digit_bins: Vec<i32> = numbers.iter().map(|&value| value % 10).collect();
    output.extend(centred_transitions(&digit_bins, 10));

    let differences: Vec<f64> = (0..length.saturating_sub(1))
        .map(|index| values[index + 1] - values[index])
        .collect();
    let z_steps: Vec<f64> = (0..length.saturating_sub(1))
        .map(|index| (scaled[index + 1] - scaled[index]).abs())
        .collect();

    output.push(fraction(length.saturating_sub(2), |index| {
        js_sign(differences[index + 1]) == js_sign(differences[index])
    }));
    output.push(fraction(length.saturating_sub(2), |index| {
        differences[index + 1] == differences[index]
    }));
    output.push(fraction(length.saturating_sub(1), |index| {
        numbers[index] % 10 == numbers[index + 1] % 10
    }));
    output.push(fraction(length.saturating_sub(1), |index| z_steps[index] > 1.0));
    output.push(fraction(length.saturating_sub(1), |index| z_steps[index] > 2.0));

    let buckets: Vec<i32> = numbers
        .iter()
        .map(|&value| (((value - 1) as f64 * 16.0 / 355.0).floor() as i32).min(15))
        .collect();
    for width in [4usize, 8, 16] {
        if length < width {
            output.push(0.0);
            continue;
        }
        let mut total = 0.0;
        for start in 0..=length - width {
            let mut seen = [false; 16];
            for &bucket in &buckets[start..start + width] {
                seen[bucket as usize] = true;
            }
            total += seen.iter().filter(|&&value| value).count() as f64 / width as f64;
        }
        output.push(total / (length - width + 1) as f64);
    }

    output
}

/// 把两块特征按 artifact 的预处理参数标准化、单位化并加权拼接。
///
/// 块权重是硬编码的 `sqrt(0.75)` / `sqrt(0.25)`，不是 artifact 里的字段 ——
/// lm-detector 的 `transform` 就是这么写的。
pub fn transform(blocks: &[Vec<f64>], params: &[Preprocessing]) -> Result<Vec<f64>, String> {
    if blocks.len() != params.len() {
        return Err(format!(
            "特征块数量不匹配：{} 块，参数 {} 组",
            blocks.len(),
            params.len()
        ));
    }
    let mut output = Vec::new();
    for (index, (block, parameter)) in blocks.iter().zip(params).enumerate() {
        if block.len() != parameter.mean.len() || block.len() != parameter.scale.len() {
            return Err(format!(
                "特征块 {index} 维度不匹配：{} / {} / {}",
                block.len(),
                parameter.mean.len(),
                parameter.scale.len()
            ));
        }
        let scaled: Vec<f64> = block
            .iter()
            .enumerate()
            .map(|(position, value)| (value - parameter.mean[position]) / parameter.scale[position])
            .collect();
        let unit = normalized(&scaled);
        let weight = if index == 0 { 0.75f64 } else { 0.25f64 }.sqrt();
        output.extend(unit.into_iter().map(|value| value * weight));
    }
    Ok(output)
}

/// 429 维 `full` 特征（频率 + 位置分块），供官方 kNN 与本地 kNN 共用。
pub fn full_feature(
    numbers: &[i32],
    params: &[Preprocessing],
) -> Result<Vec<f64>, String> {
    let blocks = vec![hellinger_feature(&count_numbers(numbers)), ordered_block_feature(numbers)];
    transform(&blocks, params)
}

/// 751 维位置头输入（4 块原始特征），由 `positional` 模块做判别。
pub fn positional_blocks(numbers: &[i32]) -> [Vec<f64>; 4] {
    [
        hellinger_feature(&count_numbers(numbers)),
        ordered_block_feature(numbers),
        position_kernel(numbers),
        sequence_features(numbers),
    ]
}

pub const POSITIONAL_BLOCK_WEIGHTS: [f64; 4] = POSITIONAL_WEIGHTS;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        numbers: Vec<i32>,
        hellinger: Vec<f64>,
        ordered: Vec<f64>,
    }

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }

    fn assert_close(actual: &[f64], expected: &[f64], label: &str) {
        assert_eq!(actual.len(), expected.len(), "{label} 长度");
        for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
            let scale = e.abs().max(1.0);
            assert!(
                (a - e).abs() <= 1e-9 * scale,
                "{label}[{index}]: {a} != {e}"
            );
        }
    }

    #[test]
    fn feature_blocks_match_the_type_script_implementation() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("fixtures/features_cases.json")).unwrap();
        for case in fixture.cases {
            let counts = count_numbers(&case.numbers);
            assert_close(&hellinger_feature(&counts), &case.hellinger, "hellinger");
            assert_close(
                &ordered_block_feature(&case.numbers),
                &case.ordered,
                "ordered",
            );
        }
    }

    #[test]
    fn positional_blocks_have_the_expected_dimensions() {
        let numbers: Vec<i32> = (0..300).map(|index| 1 + (index * 97) % 355).collect();
        let blocks = positional_blocks(&numbers);
        assert_eq!(blocks[0].len(), DIMENSION);
        assert_eq!(blocks[1].len(), ORDERED_BLOCK_DIM);
        assert_eq!(blocks[2].len(), POSITION_KERNEL_DIM);
        assert_eq!(blocks[3].len(), SEQUENCE_DIM);
    }

    #[test]
    fn transform_is_unit_norm_per_block_times_weight() {
        let numbers: Vec<i32> = (0..300).map(|index| 1 + (index * 53) % 355).collect();
        let params = vec![
            Preprocessing {
                mean: vec![0.0; DIMENSION],
                scale: vec![1.0; DIMENSION],
            },
            Preprocessing {
                mean: vec![0.0; ORDERED_BLOCK_DIM],
                scale: vec![1.0; ORDERED_BLOCK_DIM],
            },
        ];
        let blocks = vec![hellinger_feature(&count_numbers(&numbers)), ordered_block_feature(&numbers)];
        let output = transform(&blocks, &params).unwrap();
        assert_eq!(output.len(), DIMENSION + ORDERED_BLOCK_DIM);
        let first: f64 = output[..DIMENSION].iter().map(|value| value * value).sum();
        let second: f64 = output[DIMENSION..].iter().map(|value| value * value).sum();
        assert!((first - 0.75).abs() < 1e-12);
        assert!((second - 0.25).abs() < 1e-12);
    }
}
