//! 指纹模块对外的数据结构。
//!
//! 分三类：
//! - 参考库元数据（`LibraryMetadata`/`ModelInfo`）—— 内置与远端共用；
//! - 官方检测器的中间结果（`Analysis`/`Candidate`/…）—— 与 lm-detector 的返回口径对齐；
//! - 命令层返回给前端的聚合结果（`FingerprintDetection`/`FingerprintLibraryInfo`/…）。
//!
//! 检测结果要落库再回读（`add_fingerprint_samples` 依赖它），因此聚合类型同时
//! 派生 `Serialize` 与 `Deserialize`。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 参考库元数据
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub family_name: String,
    #[serde(default)]
    pub response_count: u64,
    #[serde(default)]
    pub valid_number_count: u64,
    #[serde(default)]
    pub sources: BTreeMap<String, u64>,
    #[serde(default)]
    pub conditions: BTreeMap<String, u64>,
}

impl ModelInfo {
    pub fn display(&self) -> &str {
        if self.display_name.is_empty() {
            &self.id
        } else {
            &self.display_name
        }
    }

    pub fn family_label(&self) -> (&str, &str) {
        let family = if self.family.is_empty() { "other" } else { &self.family };
        let name = if self.family_name.is_empty() { "其他" } else { &self.family_name };
        (family, name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryMetadata {
    #[serde(default)]
    pub schema: Option<String>,
    pub built_at: String,
    #[serde(default)]
    pub source_commit: Option<String>,
    pub models: Vec<ModelInfo>,
}

impl LibraryMetadata {
    pub fn find(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.find(id).is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibrarySource {
    Embedded,
    Remote,
}

impl LibrarySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Remote => "remote",
        }
    }
}

// ---------------------------------------------------------------------------
// 官方检测器结果
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub text: String,
    pub expected_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub index: usize,
    pub parsed_numbers: usize,
    pub minimum_numbers: usize,
    pub accepted: bool,
    pub raw_out_of_range_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceShape {
    pub autocorrelation: f64,
    pub small_step_share: f64,
    pub model_like: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub model: String,
    pub display_name: String,
    pub family: String,
    pub family_name: String,
    pub score: f64,
    pub probability: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub insufficient: bool,
    pub label: String,
    pub reason: String,
    pub threshold: Option<f64>,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationInfo {
    pub method: String,
    pub run: String,
    pub tau: f64,
    pub sha256: Option<String>,
    pub status: String,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelVersion {
    pub base_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub prediction: String,
    pub prediction_name: String,
    pub family_prediction: Option<String>,
    pub family_prediction_name: String,
    pub probability: Option<f64>,
    pub probability_status: String,
    pub probability_scope: Option<String>,
    pub decision: String,
    pub used_outputs: usize,
    pub diagnostics: Vec<Diagnostic>,
    pub sequence: Option<SequenceShape>,
    pub results: Vec<Candidate>,
    pub ranking_score: Option<f64>,
    pub calibration: Option<CalibrationInfo>,
    pub method: String,
    pub evidence: Evidence,
    pub model_version: ModelVersion,
}

// ---------------------------------------------------------------------------
// 命令层聚合结果
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryInfoLite {
    pub source: LibrarySource,
    pub built_at: String,
    pub model_count: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptResult {
    pub index: usize,
    pub challenge_id: String,
    /// 实际发送给模型的提示词，供结果详情与"加入样本库"回溯。
    #[serde(default)]
    pub challenge_prompt: String,
    pub expected_count: u32,
    pub ok: bool,
    pub status: Option<u16>,
    pub latency_ms: i64,
    pub wire: String,
    pub url: String,
    pub error: Option<String>,
    pub parsed_numbers: usize,
    pub minimum_numbers: usize,
    pub accepted: bool,
    pub text_preview: String,
    /// 原始回答全文；用于历史详情回看与单条重试。老记录可能为空。
    #[serde(default)]
    pub text: String,
    /// 解析后的整数；加入本地样本库时由后端从这里取，前端不需要展示。
    #[serde(default)]
    pub numbers: Vec<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeclaredRank {
    pub model: String,
    pub rank: usize,
    pub score: f64,
    pub probability: Option<f64>,
    /// 命中方式：`declared`（声明名）或 `upstream`（上游映射名）。
    pub matched_as: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfficialResult {
    pub decision: String,
    pub used_outputs: usize,
    pub calibrated: bool,
    pub probability_status: String,
    pub diagnostics: Vec<Diagnostic>,
    pub sequence: Option<SequenceShape>,
    pub candidates: Vec<Candidate>,
    pub declared: Option<DeclaredRank>,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalCandidate {
    pub label: String,
    pub samples: usize,
    /// 查询回答到该 label 的最近邻距离 / 该 label 自身的 leave-one-out 离散度。
    pub distance_ratio: f64,
    pub score: f64,
    pub probability: Option<f64>,
    pub official: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalResult {
    pub candidates: Vec<LocalCandidate>,
    /// 本地温度是否拟合成功；false 时所有 probability 都是 None。
    pub calibrated: bool,
    /// 样本数 ≥6、参与概率拟合的 label 数。
    pub eligible_labels: usize,
    /// 样本数 ≥3、参与本地排名的 label 数。
    pub total_labels: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conclusion {
    pub model: Option<String>,
    pub display_name: Option<String>,
    /// `official` / `agreement` / `conflict` / `local_only` / `insufficient`
    pub basis: String,
    /// `high` / `medium` / `low`
    pub level: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintDetection {
    /// 落库后的检测记录 id；加入本地样本库时用它回查原始回答。
    pub id: i64,
    pub declared_model: String,
    pub upstream_model: String,
    pub wire: String,
    pub status: String,
    pub library: LibraryInfoLite,
    pub attempts: Vec<AttemptResult>,
    pub official: Option<OfficialResult>,
    pub local: Option<LocalResult>,
    pub conclusion: Conclusion,
    pub elapsed_ms: i64,
}

/// 挑战库对前端展示的形态。用户手动选择时用它渲染完整提示词。
#[derive(Debug, Clone, Serialize)]
pub struct FingerprintChallenge {
    pub challenge_id: String,
    pub condition: String,
    pub expected_count: u32,
    pub prompt: String,
    pub system: String,
}

/// 检测过程中的实时进度，经 `apilot://fingerprint` 推给前端。
///
/// 一次检测只有 3 条请求；前端按 `challenge_index` 把每条请求的状态挂到对应
/// 卡片上。`phase` 依次是 `started` → `requesting` → `answered` →
/// `analyzing` → `done`。`run_id` 用来过滤上一次检测的迟到事件。
#[derive(Debug, Clone, Serialize)]
pub struct FingerprintProgress {
    pub run_id: String,
    pub phase: String,
    pub provider_id: i64,
    pub provider_tag: String,
    pub declared_model: String,
    pub upstream_model: String,
    pub total: usize,
    pub completed: usize,
    pub accepted: usize,
    pub parallel: bool,
    pub challenge_index: Option<usize>,
    pub challenge_id: Option<String>,
    pub expected_count: Option<u32>,
    pub challenge_prompt: Option<String>,
    pub message: String,
    pub ok: Option<bool>,
    pub status: Option<u16>,
    pub latency_ms: Option<i64>,
    pub parsed_numbers: Option<usize>,
    pub minimum_numbers: Option<usize>,
    pub wire: Option<String>,
    pub url: Option<String>,
    pub error: Option<String>,
    pub preview: Option<String>,
    pub detection_id: Option<i64>,
    pub result_status: Option<String>,
}

/// 检测记录列表里的一行。完整结果在 `result_json`，这里只放列表需要的摘要。
#[derive(Debug, Clone, Serialize)]
pub struct FingerprintDetectionSummary {
    pub id: i64,
    pub provider_id: Option<i64>,
    pub provider_tag: String,
    pub declared_model: String,
    pub upstream_model: String,
    pub wire: String,
    pub library_source: String,
    pub library_built_at: String,
    pub status: String,
    pub created_at: i64,
    pub official_top: Option<String>,
    pub official_probability: Option<f64>,
    pub local_top: Option<String>,
    pub local_probability: Option<f64>,
    pub conclusion: Conclusion,
}

#[derive(Debug, Clone, Serialize)]
pub struct FingerprintLibraryInfo {
    pub source: LibrarySource,
    pub built_at: String,
    pub model_count: usize,
    pub sha256: String,
    pub source_url: Option<String>,
    pub source_commit: Option<String>,
    pub models: Vec<ModelInfo>,
    pub challenges: usize,
    /// 检测器加载失败时也把原因带回来，页面可以提示"参考库不可用"。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FingerprintLocalLabel {
    pub label: String,
    pub samples: usize,
    pub last_added_at: i64,
    /// 这个 label 是否也是官方参考库里的身份。
    pub official: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FingerprintLocalSample {
    pub id: i64,
    pub label: String,
    pub challenge_id: String,
    pub expected_count: u32,
    pub numbers_count: usize,
    pub provider_tag: String,
    pub declared_model: String,
    pub created_at: i64,
    pub note: Option<String>,
}
