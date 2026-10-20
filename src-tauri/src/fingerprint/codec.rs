//! `shared_detector.bin` 解码。
//!
//! 格式与 lm-detector 的 `shared/detector-codec.ts` 完全一致：
//!
//! ```text
//! u32 LE 头长 | UTF-8 JSON 头 | 4 字节对齐填充 | float32 数值区
//! ```
//!
//! 数值区**按字节位置分组**（先所有浮点的第 0 字节，再第 1 字节……），
//! 这样 zstd 压得比交错排列好得多。头里每个数值数组写成
//! `{"$f32":offset,"n":length}`，标量保持原样。
//!
//! 解码器对坏数据只返回错误，不 panic：远端参考库是用户可替换的输入。

use serde::Deserialize;
use thiserror::Error;

use crate::fingerprint::features::Preprocessing;

pub const SCHEMA: &str = "shared-detector-v2";

#[derive(Debug, Clone)]
pub struct Artifact {
    pub schema: String,
    pub source_run: String,
    pub base_sha256: String,
    pub source_reference_sha256: String,
    pub bank_built_at: String,
    pub model_ids: Vec<String>,
    pub response_counts: Vec<u32>,
    pub ranker: Ranker,
    pub calibration: Option<Calibration>,
    pub calibration_sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Ranker {
    pub head_params: Vec<Preprocessing>,
    pub full_params: Vec<Preprocessing>,
    pub lda_weights: Vec<Vec<f64>>,
    pub lda_bias: Vec<f64>,
    /// 每个身份一批参考回答的 429 维特征。
    pub references: Vec<Vec<Vec<f64>>>,
    pub bank: BankArtifact,
    pub blend: Blend,
    pub positional: PositionalHead,
}

#[derive(Debug, Clone)]
pub struct BankArtifact {
    pub hellinger: FeatureBank,
    pub ordered_blocks: OrderedBlockBank,
}

#[derive(Debug, Clone)]
pub struct FeatureBank {
    pub feature_mean: Vec<f64>,
    pub feature_scale: Vec<f64>,
    pub nuisance_basis: Vec<Vec<f64>>,
    pub centroids: Vec<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub struct OrderedBlockBank {
    pub weight: f64,
    pub feature_mean: Vec<f64>,
    pub feature_scale: Vec<f64>,
    pub nuisance_basis: Vec<Vec<f64>>,
    pub centroids: Vec<Vec<f64>>,
    /// `[0]` 是全体环境的模板，其余是逐环境模板。
    pub environment_centroids: Vec<Vec<Vec<f64>>>,
}

#[derive(Debug, Clone)]
pub struct Blend {
    pub frequency: f64,
    pub positional: f64,
}

#[derive(Debug, Clone)]
pub struct PositionalHead {
    pub params: Vec<Preprocessing>,
    pub weights: Vec<Vec<f64>>,
    pub bias: Vec<f64>,
}

#[derive(Debug, Clone)]
pub struct Calibration {
    pub schema: String,
    pub method: String,
    pub tau: f64,
    pub calibration_run: String,
    pub binding: CalibrationBinding,
}

#[derive(Debug, Clone)]
pub struct CalibrationBinding {
    pub base_sha256: String,
    pub reference_sha256: String,
    pub model_ids: Vec<String>,
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("检测器文件太短")]
    TooShort,
    #[error("检测器头部长度不合法")]
    BadHeaderLength,
    #[error("检测器数值区长度不合法")]
    BadFloatArea,
    #[error("检测器头部不是有效 JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("检测器数组越界: offset={offset}, n={n}, total={total}")]
    OutOfBounds {
        offset: usize,
        n: usize,
        total: usize,
    },
    #[error("检测器结构不合法: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NumArray {
    Packed {
        #[serde(rename = "$f32")]
        offset: usize,
        n: usize,
    },
    Inline(Vec<f64>),
}

#[derive(Debug, Deserialize)]
struct RawArtifact {
    schema: String,
    #[serde(default)]
    source_run: String,
    #[serde(default)]
    base_sha256: String,
    #[serde(default)]
    source_reference_sha256: String,
    #[serde(default)]
    bank_built_at: String,
    model_ids: Vec<String>,
    response_counts: NumArray,
    ranker: RawRanker,
    #[serde(default)]
    calibration: Option<RawCalibration>,
    #[serde(default)]
    calibration_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRanker {
    head_params: Vec<RawPreprocessing>,
    full_params: Vec<RawPreprocessing>,
    lda_weights: Vec<NumArray>,
    lda_bias: NumArray,
    references: Vec<Vec<NumArray>>,
    bank: RawBank,
    blend: RawBlend,
    positional: RawPositional,
}

#[derive(Debug, Deserialize)]
struct RawPreprocessing {
    mean: NumArray,
    scale: NumArray,
}

#[derive(Debug, Deserialize)]
struct RawBank {
    hellinger: RawFeatureBank,
    ordered_blocks: RawOrderedBlockBank,
}

#[derive(Debug, Deserialize)]
struct RawFeatureBank {
    feature_mean: NumArray,
    feature_scale: NumArray,
    nuisance_basis: Vec<NumArray>,
    centroids: Vec<NumArray>,
}

#[derive(Debug, Deserialize)]
struct RawOrderedBlockBank {
    #[serde(default)]
    weight: f64,
    feature_mean: NumArray,
    feature_scale: NumArray,
    nuisance_basis: Vec<NumArray>,
    centroids: Vec<NumArray>,
    environment_centroids: Vec<Vec<NumArray>>,
}

#[derive(Debug, Deserialize)]
struct RawBlend {
    frequency: f64,
    positional: f64,
}

#[derive(Debug, Deserialize)]
struct RawPositional {
    params: Vec<RawPreprocessing>,
    weights: Vec<NumArray>,
    bias: NumArray,
}

#[derive(Debug, Deserialize)]
struct RawCalibration {
    schema: String,
    method: String,
    tau: f64,
    #[serde(default)]
    calibration_run: String,
    binding: RawBinding,
}

#[derive(Debug, Deserialize)]
struct RawBinding {
    base_sha256: String,
    reference_sha256: String,
    model_ids: Vec<String>,
}

fn invalid<T>(message: impl Into<String>) -> Result<T, CodecError> {
    Err(CodecError::Invalid(message.into()))
}

fn materialize(array: &NumArray, floats: &[f32]) -> Result<Vec<f64>, CodecError> {
    match array {
        NumArray::Packed { offset, n } => {
            let end = offset
                .checked_add(*n)
                .ok_or(CodecError::OutOfBounds {
                    offset: *offset,
                    n: *n,
                    total: floats.len(),
                })?;
            if end > floats.len() {
                return Err(CodecError::OutOfBounds {
                    offset: *offset,
                    n: *n,
                    total: floats.len(),
                });
            }
            Ok(floats[*offset..end]
                .iter()
                .map(|&value| value as f64)
                .collect())
        }
        NumArray::Inline(values) => Ok(values.clone()),
    }
}

fn materialize_matrix(array: &[NumArray], floats: &[f32]) -> Result<Vec<Vec<f64>>, CodecError> {
    array
        .iter()
        .map(|row| materialize(row, floats))
        .collect()
}

fn preprocessing(raw: &RawPreprocessing, floats: &[f32]) -> Result<Preprocessing, CodecError> {
    let mean = materialize(&raw.mean, floats)?;
    let scale = materialize(&raw.scale, floats)?;
    if mean.len() != scale.len() {
        return invalid(format!(
            "预处理均值/尺度长度不一致：{} / {}",
            mean.len(),
            scale.len()
        ));
    }
    Ok(Preprocessing { mean, scale })
}

fn equal_dims(actual: usize, expected: usize, label: &str) -> Result<(), CodecError> {
    if actual != expected {
        return invalid(format!("{label} 维度应为 {expected}，实际 {actual}"));
    }
    Ok(())
}

fn finite(values: &[f64], label: &str) -> Result<(), CodecError> {
    if values.iter().any(|value| !value.is_finite()) {
        return invalid(format!("{label} 含非有限数值"));
    }
    Ok(())
}

/// 解码并做结构校验。远端下载的数据也走这里，因此校验必须完整。
pub fn decode_artifact(bytes: &[u8]) -> Result<Artifact, CodecError> {
    if bytes.len() < 8 {
        return Err(CodecError::TooShort);
    }
    let header_length = u32::from_le_bytes(bytes[0..4].try_into().expect("4 字节")) as usize;
    if header_length == 0 || 4 + header_length > bytes.len() {
        return Err(CodecError::BadHeaderLength);
    }
    let start = (4 + header_length + 3) & !3;
    if start > bytes.len() || (bytes.len() - start) % 4 != 0 {
        return Err(CodecError::BadFloatArea);
    }
    let count = (bytes.len() - start) / 4;
    if count == 0 {
        return Err(CodecError::BadFloatArea);
    }

    let mut values = vec![0f32; count];
    for index in 0..count {
        let mut raw = [0u8; 4];
        for byte in 0..4 {
            raw[byte] = bytes[start + byte * count + index];
        }
        values[index] = f32::from_le_bytes(raw);
    }

    let raw: RawArtifact =
        serde_json::from_slice(&bytes[4..4 + header_length]).map_err(CodecError::from)?;
    if raw.schema != SCHEMA {
        return invalid(format!("检测器 schema 不支持：{}", raw.schema));
    }
    if raw.model_ids.is_empty() {
        return invalid("检测器没有模型身份");
    }
    let unique: std::collections::HashSet<&String> = raw.model_ids.iter().collect();
    if unique.len() != raw.model_ids.len() {
        return invalid("检测器模型身份重复");
    }

    let response_offsets = materialize(&raw.response_counts, &values)?;
    if response_offsets.len() != raw.model_ids.len() {
        return invalid("response_counts 长度与模型数量不一致");
    }
    let response_counts: Vec<u32> = response_offsets
        .iter()
        .map(|&value| {
            if !value.is_finite() || value < 0.0 || value > u32::MAX as f64 {
                return u32::MAX;
            }
            value as u32
        })
        .collect();
    if response_counts.iter().any(|&count| count == u32::MAX) {
        return invalid("response_counts 含非法值");
    }

    let full_params: Vec<Preprocessing> = raw
        .ranker
        .full_params
        .iter()
        .map(|item| preprocessing(item, &values))
        .collect::<Result<_, _>>()?;
    let head_params: Vec<Preprocessing> = raw
        .ranker
        .head_params
        .iter()
        .map(|item| preprocessing(item, &values))
        .collect::<Result<_, _>>()?;
    equal_dims(full_params.len(), 2, "full_params")?;
    equal_dims(head_params.len(), 2, "head_params")?;
    equal_dims(full_params[0].mean.len(), 355, "full_params[0]")?;
    equal_dims(full_params[1].mean.len(), 74, "full_params[1]")?;
    equal_dims(head_params[0].mean.len(), 355, "head_params[0]")?;
    equal_dims(head_params[1].mean.len(), 74, "head_params[1]")?;

    let full_dim = full_params.iter().map(|item| item.mean.len()).sum::<usize>();
    let positional_dim = 355 + 74 + 102 + 220;

    let lda_weights = materialize_matrix(&raw.ranker.lda_weights, &values)?;
    let lda_bias = materialize(&raw.ranker.lda_bias, &values)?;
    equal_dims(lda_weights.len(), raw.model_ids.len(), "lda_weights")?;
    equal_dims(lda_bias.len(), raw.model_ids.len(), "lda_bias")?;
    for row in &lda_weights {
        equal_dims(row.len(), full_dim, "lda_weights 行")?;
        finite(row, "lda_weights")?;
    }
    finite(&lda_bias, "lda_bias")?;

    if raw.ranker.references.len() != raw.model_ids.len() {
        return invalid("references 的数量与模型数量不一致");
    }
    let mut references = Vec::with_capacity(raw.ranker.references.len());
    for (index, model) in raw.ranker.references.iter().enumerate() {
        if model.is_empty() {
            return invalid(format!("模型 {} 没有参考回答", raw.model_ids[index]));
        }
        let rows = materialize_matrix(model, &values)?;
        for row in &rows {
            equal_dims(row.len(), full_dim, "reference")?;
            finite(row, "reference")?;
        }
        references.push(rows);
    }

    let hellinger = &raw.ranker.bank.hellinger;
    let ordered = &raw.ranker.bank.ordered_blocks;
    let h_mean = materialize(&hellinger.feature_mean, &values)?;
    let h_scale = materialize(&hellinger.feature_scale, &values)?;
    let h_basis = materialize_matrix(&hellinger.nuisance_basis, &values)?;
    let h_centroids = materialize_matrix(&hellinger.centroids, &values)?;
    equal_dims(h_mean.len(), 355, "hellinger.feature_mean")?;
    equal_dims(h_scale.len(), 355, "hellinger.feature_scale")?;
    equal_dims(h_centroids.len(), raw.model_ids.len(), "hellinger.centroids")?;
    for basis in &h_basis {
        equal_dims(basis.len(), 355, "hellinger.nuisance_basis")?;
        finite(basis, "hellinger.nuisance_basis")?;
    }
    for centroid in &h_centroids {
        equal_dims(centroid.len(), 355, "hellinger.centroid")?;
        finite(centroid, "hellinger.centroid")?;
    }
    finite(&h_mean, "hellinger.feature_mean")?;
    finite(&h_scale, "hellinger.feature_scale")?;

    let o_mean = materialize(&ordered.feature_mean, &values)?;
    let o_scale = materialize(&ordered.feature_scale, &values)?;
    let o_basis = materialize_matrix(&ordered.nuisance_basis, &values)?;
    let o_centroids = materialize_matrix(&ordered.centroids, &values)?;
    equal_dims(o_mean.len(), 74, "ordered.feature_mean")?;
    equal_dims(o_scale.len(), 74, "ordered.feature_scale")?;
    equal_dims(o_centroids.len(), raw.model_ids.len(), "ordered.centroids")?;
    for basis in &o_basis {
        equal_dims(basis.len(), 74, "ordered.nuisance_basis")?;
        finite(basis, "ordered.nuisance_basis")?;
    }
    for centroid in &o_centroids {
        equal_dims(centroid.len(), 74, "ordered.centroid")?;
        finite(centroid, "ordered.centroid")?;
    }
    finite(&o_mean, "ordered.feature_mean")?;
    finite(&o_scale, "ordered.feature_scale")?;

    if ordered.environment_centroids.is_empty() {
        return invalid("ordered_blocks.environment_centroids 为空");
    }
    let mut environment_centroids = Vec::with_capacity(ordered.environment_centroids.len());
    for environment in &ordered.environment_centroids {
        let rows = materialize_matrix(environment, &values)?;
        equal_dims(
            rows.len(),
            raw.model_ids.len(),
            "ordered.environment_centroids 行数",
        )?;
        for row in &rows {
            equal_dims(row.len(), 74, "ordered.environment_centroid")?;
            finite(row, "ordered.environment_centroid")?;
        }
        environment_centroids.push(rows);
    }

    let positional_params: Vec<Preprocessing> = raw
        .ranker
        .positional
        .params
        .iter()
        .map(|item| preprocessing(item, &values))
        .collect::<Result<_, _>>()?;
    equal_dims(positional_params.len(), 4, "positional.params")?;
    for (index, expected) in [355usize, 74, 102, 220].iter().enumerate() {
        equal_dims(
            positional_params[index].mean.len(),
            *expected,
            "positional.params",
        )?;
    }
    let positional_weights = materialize_matrix(&raw.ranker.positional.weights, &values)?;
    let positional_bias = materialize(&raw.ranker.positional.bias, &values)?;
    equal_dims(
        positional_weights.len(),
        raw.model_ids.len(),
        "positional.weights",
    )?;
    equal_dims(
        positional_bias.len(),
        raw.model_ids.len(),
        "positional.bias",
    )?;
    for row in &positional_weights {
        equal_dims(row.len(), positional_dim, "positional.weights 行")?;
        finite(row, "positional.weights")?;
    }
    finite(&positional_bias, "positional.bias")?;

    if !raw.ranker.blend.frequency.is_finite() || !raw.ranker.blend.positional.is_finite() {
        return invalid("blend 含非有限数值");
    }

    let calibration = raw
        .calibration
        .as_ref()
        .map(|item| -> Result<Calibration, CodecError> {
            if item.schema != "shared-confidence-v2" {
                return invalid(format!("校准 schema 不支持：{}", item.schema));
            }
            if item.method != "ranking-temperature" {
                return invalid(format!("校准方法不支持：{}", item.method));
            }
            if !item.tau.is_finite() || !(0.001..=1000.0).contains(&item.tau) {
                return invalid("校准温度越界");
            }
            if item.binding.model_ids != raw.model_ids {
                return invalid("校准绑定的模型顺序不一致");
            }
            Ok(Calibration {
                schema: item.schema.clone(),
                method: item.method.clone(),
                tau: item.tau,
                calibration_run: item.calibration_run.clone(),
                binding: CalibrationBinding {
                    base_sha256: item.binding.base_sha256.clone(),
                    reference_sha256: item.binding.reference_sha256.clone(),
                    model_ids: item.binding.model_ids.clone(),
                },
            })
        })
        .transpose()?;

    // 校准必须绑定到当前 ranker 与参考数据；不匹配时保留 artifact，但上层会
    // 通过 `calibration_matches` 判定是否给出概率。
    if let Some(item) = &calibration {
        if item.binding.base_sha256 != raw.base_sha256
            || item.binding.reference_sha256 != raw.source_reference_sha256
        {
            // 这里不直接报错：lm-detector 在绑定时不匹配时只降级为"未校准排名"。
            // 保留 artifact，让上层按同一个口径处理。
        }
    }

    Ok(Artifact {
        schema: raw.schema,
        source_run: raw.source_run,
        base_sha256: raw.base_sha256,
        source_reference_sha256: raw.source_reference_sha256,
        bank_built_at: raw.bank_built_at,
        model_ids: raw.model_ids,
        response_counts,
        ranker: Ranker {
            head_params,
            full_params,
            lda_weights,
            lda_bias,
            references,
            bank: BankArtifact {
                hellinger: FeatureBank {
                    feature_mean: h_mean,
                    feature_scale: h_scale,
                    nuisance_basis: h_basis,
                    centroids: h_centroids,
                },
                ordered_blocks: OrderedBlockBank {
                    weight: ordered.weight,
                    feature_mean: o_mean,
                    feature_scale: o_scale,
                    nuisance_basis: o_basis,
                    centroids: o_centroids,
                    environment_centroids,
                },
            },
            blend: Blend {
                frequency: raw.ranker.blend.frequency,
                positional: raw.ranker.blend.positional,
            },
            positional: PositionalHead {
                params: positional_params,
                weights: positional_weights,
                bias: positional_bias,
            },
        },
        calibration,
        calibration_sha256: raw.calibration_sha256,
    })
}

impl Artifact {
    /// 校准是否可用于当前 artifact（对应 JS `calibrateRanking` 的绑定检查）。
    pub fn calibration_matches(&self) -> bool {
        let Some(calibration) = &self.calibration else {
            return false;
        };
        calibration.schema == "shared-confidence-v2"
            && calibration.method == "ranking-temperature"
            && calibration.tau.is_finite()
            && (0.001..=1000.0).contains(&calibration.tau)
            && calibration.binding.base_sha256 == self.base_sha256
            && calibration.binding.reference_sha256 == self.source_reference_sha256
            && calibration.binding.model_ids == self.model_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn embedded() -> Vec<u8> {
        zstd::stream::decode_all(Cursor::new(include_bytes!(
            "../../assets/fingerprint/shared_detector.bin.zst"
        )))
        .expect("内置检测器应能解压")
    }

    #[test]
    fn decodes_the_embedded_artifact() {
        let artifact = decode_artifact(&embedded()).expect("内置检测器应能解码");
        assert_eq!(artifact.schema, SCHEMA);
        assert_eq!(artifact.model_ids.len(), 60);
        assert_eq!(artifact.response_counts.iter().sum::<u32>(), 2236);
        assert!(artifact.calibration_matches());
        assert_eq!(artifact.ranker.references.len(), 60);
        assert_eq!(artifact.ranker.references[0][0].len(), 429);
        assert_eq!(artifact.ranker.positional.weights[0].len(), 751);
    }

    #[test]
    fn rejects_short_or_truncated_files() {
        assert!(matches!(decode_artifact(&[0, 0, 0]), Err(CodecError::TooShort)));
        // 头部长度声明为 10，但文件只剩 4 字节。
        assert!(matches!(
            decode_artifact(&[10, 0, 0, 0, 0, 0, 0, 0]),
            Err(CodecError::BadHeaderLength)
        ));
    }

    #[test]
    fn rejects_out_of_bounds_float_arrays() {
        let header = br#"{"schema":"shared-detector-v2","model_ids":["a"],"response_counts":{"$f32":10,"n":1},"ranker":{"head_params":[],"full_params":[],"lda_weights":[],"lda_bias":{"$f32":0,"n":1},"references":[],"bank":{"hellinger":{"feature_mean":{"$f32":0,"n":1},"feature_scale":{"$f32":0,"n":1},"nuisance_basis":[],"centroids":[]},"ordered_blocks":{"feature_mean":{"$f32":0,"n":1},"feature_scale":{"$f32":0,"n":1},"nuisance_basis":[],"centroids":[],"environment_centroids":[]}},"blend":{"frequency":0.5,"positional":0.5},"positional":{"params":[],"weights":[],"bias":{"$f32":0,"n":1}}}}"#;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(header);
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        bytes.extend_from_slice(&1f32.to_le_bytes());
        assert!(matches!(
            decode_artifact(&bytes),
            Err(CodecError::OutOfBounds { .. })
        ));
    }
}
