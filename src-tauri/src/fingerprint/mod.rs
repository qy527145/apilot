//! 指纹检测：lm-detector 的 Rust 移植、两级参考库与本地证据层。
//!
//! 官方参考库有两种来源：
//! - 内置快照：`src-tauri/assets/fingerprint/`，随安装包分发，离线可用；
//! - 远端更新：`~/.apilot/fingerprint/remote/`，用户点「检查更新」时从
//!   lm-detector 的 GitHub 原始文件拉取（检测器 + `unified_bank.json`）。
//!
//! 两者都经过同一套结构校验；远端失败时回落到内置，内置也坏了只记录错误，
//! 不影响 Apilot 的其余功能。

pub mod analyze;
pub mod challenges;
pub mod codec;
pub mod features;
pub mod local;
pub mod parse;
pub mod positional;
pub mod ranker;
pub mod types;

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::paths;
use crate::fingerprint::challenges::Challenge;
use crate::fingerprint::codec::{decode_artifact, Artifact, CodecError};
use crate::fingerprint::types::{
    FingerprintLibraryInfo, LibraryMetadata, LibrarySource, ModelInfo,
};
use crate::util::now_ms;

pub const DEFAULT_REMOTE_BASE: &str =
    "https://raw.githubusercontent.com/Ikaleio/lm-detector/main/data";
const REMOTE_SCHEMA: &str = "apilot-fingerprint-remote-v1";
const MAX_DOWNLOAD_BYTES: usize = 30 * 1024 * 1024;
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

const EMBEDDED_ARTIFACT: &[u8] =
    include_bytes!("../../assets/fingerprint/shared_detector.bin.zst");
const EMBEDDED_MODELS: &str = include_str!("../../assets/fingerprint/models.json");

#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    #[error("检测器数据错误: {0}")]
    Codec(#[from] CodecError),
    #[error("检测器结构不合法: {0}")]
    Invalid(String),
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("网络错误: {0}")]
    Http(#[from] reqwest::Error),
    #[error("参考库更新失败: {0}")]
    Update(String),
    #[error("排名计算失败: {0}")]
    Algorithm(String),
}

/// 当前生效的参考库。`artifact` 与 `metadata` 在加载时已经校验过一致性。
pub struct ActiveLibrary {
    pub source: LibrarySource,
    pub artifact: Arc<Artifact>,
    pub metadata: Arc<LibraryMetadata>,
    pub challenges: Arc<Vec<Challenge>>,
    pub built_at: String,
    pub sha256: String,
    pub source_url: Option<String>,
    pub source_commit: Option<String>,
}

pub struct FingerprintEngine {
    active: ArcSwapOption<ActiveLibrary>,
    error: Mutex<Option<String>>,
}

impl FingerprintEngine {
    /// 启动时加载：远端有效就用远端，否则内置。加载失败不返回 Err ——
    /// 参考库坏掉不应该让整个应用起不来。
    pub fn load() -> Self {
        let (active, error) = load_active();
        Self {
            active: ArcSwapOption::new(active.map(Arc::new)),
            error: Mutex::new(error),
        }
    }

    pub fn active(&self) -> Result<Arc<ActiveLibrary>, FingerprintError> {
        self.active.load_full().ok_or_else(|| {
            FingerprintError::Invalid(
                self.error
                    .lock()
                    .clone()
                    .unwrap_or_else(|| "指纹参考库未加载".into()),
            )
        })
    }

    pub fn info(&self) -> FingerprintLibraryInfo {
        match self.active.load_full() {
            Some(active) => FingerprintLibraryInfo {
                source: active.source,
                built_at: active.built_at.clone(),
                model_count: active.metadata.models.len(),
                sha256: active.sha256.clone(),
                source_url: active.source_url.clone(),
                source_commit: active.source_commit.clone(),
                models: active.metadata.models.clone(),
                challenges: active.challenges.len(),
                error: None,
            },
            None => FingerprintLibraryInfo {
                source: LibrarySource::Embedded,
                built_at: String::new(),
                model_count: 0,
                sha256: String::new(),
                source_url: None,
                source_commit: None,
                models: Vec::new(),
                challenges: 0,
                error: self.error.lock().clone(),
            },
        }
    }

    /// 重新读取远端/内置库；失败时保留当前生效的库。
    pub fn reload(&self) -> Result<(), FingerprintError> {
        let (active, error) = load_active();
        match active {
            Some(active) => {
                self.active.store(Some(Arc::new(active)));
                *self.error.lock() = None;
                Ok(())
            }
            None => {
                let message = error.unwrap_or_else(|| "指纹参考库加载失败".into());
                *self.error.lock() = Some(message.clone());
                Err(FingerprintError::Invalid(message))
            }
        }
    }

    /// 下载远端检测器与指纹库，校验、原子写入，然后热切换。
    pub async fn update(
        &self,
        client: &reqwest::Client,
        base_url: Option<&str>,
    ) -> Result<(), FingerprintError> {
        let base = base_url.unwrap_or(DEFAULT_REMOTE_BASE);
        let base = normalize_remote_base(base)?;
        let artifact_bytes = download(client, &format!("{base}/shared_detector.bin")).await?;
        let bank_bytes = download(client, &format!("{base}/unified_bank.json")).await?;

        let artifact = decode_artifact(&artifact_bytes)?;
        let bank: LeanBank = serde_json::from_slice(&bank_bytes)?;
        if bank.schema != "robust-number-fingerprint-bank" {
            return Err(FingerprintError::Update(format!(
                "指纹库 schema 不支持：{}",
                bank.schema
            )));
        }
        if bank.built_at != artifact.bank_built_at {
            return Err(FingerprintError::Update(
                "指纹库与检测器不是同一次构建".into(),
            ));
        }
        if bank.models.len() != artifact.model_ids.len() {
            return Err(FingerprintError::Update(
                "指纹库与检测器的模型数量不一致".into(),
            ));
        }
        for (index, model) in bank.models.iter().enumerate() {
            if model.id != artifact.model_ids[index]
                || model.response_count != artifact.response_counts[index] as u64
            {
                return Err(FingerprintError::Update(
                    "指纹库与检测器的模型顺序/样本数不一致".into(),
                ));
            }
        }

        let metadata = LibraryMetadata {
            schema: Some("apilot-fingerprint-models-v1".into()),
            built_at: bank.built_at.clone(),
            source_commit: None,
            models: bank
                .models
                .iter()
                .map(|model| ModelInfo {
                    id: model.id.clone(),
                    display_name: model
                        .display_name
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| model.id.clone()),
                    family: model
                        .family
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "other".into()),
                    family_name: model
                        .family_name
                        .clone()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "其他".into()),
                    response_count: model.response_count,
                    valid_number_count: model.valid_number_count,
                    sources: model.sources.clone(),
                    conditions: model.conditions.clone(),
                })
                .collect(),
        };
        validate_library(&artifact, &metadata)?;

        let artifact_sha = sha256(&artifact_bytes);
        let bank_sha = sha256(&bank_bytes);
        let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
        let manifest = RemoteManifest {
            schema: REMOTE_SCHEMA.into(),
            source_base: base.clone(),
            source_commit: None,
            built_at: bank.built_at,
            model_count: metadata.models.len(),
            artifact_sha256: artifact_sha,
            bank_sha256: bank_sha,
            downloaded_at: now_ms(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        write_remote(
            &artifact_bytes,
            &bank_bytes,
            &metadata_bytes,
            &manifest_bytes,
        )
        .await?;
        self.reload()
    }

    /// 停用远端库，回到内置快照。保留下载内容（改名为 `remote.disabled`），
    /// 方便用户排查或以后恢复。
    pub async fn reset(&self) -> Result<(), FingerprintError> {
        let remote = paths::fingerprint_remote_dir();
        if remote.exists() {
            let disabled = remote.with_file_name("remote.disabled");
            if disabled.exists() {
                tokio::fs::remove_dir_all(&disabled).await?;
            }
            tokio::fs::rename(&remote, &disabled).await?;
        }
        self.reload()
    }
}

// ---------------------------------------------------------------------------
// 加载
// ---------------------------------------------------------------------------

fn load_active() -> (Option<ActiveLibrary>, Option<String>) {
    match load_remote() {
        Ok(Some(active)) => return (Some(active), None),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(error = %error, "远端指纹参考库不可用，回落到内置");
        }
    }
    match load_embedded() {
        Ok(active) => (Some(active), None),
        Err(error) => {
            tracing::error!(error = %error, "内置指纹参考库加载失败");
            (None, Some(error.to_string()))
        }
    }
}

fn load_embedded() -> Result<ActiveLibrary, FingerprintError> {
    let artifact_bytes = zstd::stream::decode_all(Cursor::new(EMBEDDED_ARTIFACT))?;
    let artifact = decode_artifact(&artifact_bytes)?;
    let metadata: LibraryMetadata = serde_json::from_str(EMBEDDED_MODELS)?;
    validate_library(&artifact, &metadata)?;
    let challenges = challenges::embedded()?;
    Ok(ActiveLibrary {
        source: LibrarySource::Embedded,
        built_at: metadata.built_at.clone(),
        sha256: sha256(&artifact_bytes),
        source_url: None,
        source_commit: metadata.source_commit.clone(),
        artifact: Arc::new(artifact),
        metadata: Arc::new(metadata),
        challenges: Arc::new(challenges),
    })
}

fn load_remote() -> Result<Option<ActiveLibrary>, FingerprintError> {
    let directory = paths::fingerprint_remote_dir();
    let manifest_path = directory.join("manifest.json");
    if !manifest_path.exists() {
        return Ok(None);
    }

    let manifest: RemoteManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    if manifest.schema != REMOTE_SCHEMA {
        return Err(FingerprintError::Invalid(format!(
            "远端参考库 manifest schema 不支持：{}",
            manifest.schema
        )));
    }
    let artifact_bytes = std::fs::read(directory.join("shared_detector.bin"))?;
    if sha256(&artifact_bytes) != manifest.artifact_sha256 {
        return Err(FingerprintError::Invalid(
            "远端检测器文件与 manifest 哈希不一致".into(),
        ));
    }
    let metadata: LibraryMetadata =
        serde_json::from_slice(&std::fs::read(directory.join("models.json"))?)?;
    let artifact = decode_artifact(&artifact_bytes)?;
    validate_library(&artifact, &metadata)?;
    if manifest.built_at != artifact.bank_built_at
        || manifest.model_count != artifact.model_ids.len()
    {
        return Err(FingerprintError::Invalid(
            "远端 manifest 与检测器版本不一致".into(),
        ));
    }

    Ok(Some(ActiveLibrary {
        source: LibrarySource::Remote,
        built_at: artifact.bank_built_at.clone(),
        sha256: manifest.artifact_sha256,
        source_url: Some(manifest.source_base),
        source_commit: manifest.source_commit.or(metadata.source_commit.clone()),
        artifact: Arc::new(artifact),
        metadata: Arc::new(metadata),
        challenges: Arc::new(challenges::embedded()?),
    }))
}

fn validate_library(
    artifact: &Artifact,
    metadata: &LibraryMetadata,
) -> Result<(), FingerprintError> {
    if artifact.schema != codec::SCHEMA {
        return Err(FingerprintError::Invalid(format!(
            "检测器 schema 不支持：{}",
            artifact.schema
        )));
    }
    if metadata.built_at != artifact.bank_built_at {
        return Err(FingerprintError::Invalid(
            "元数据与检测器不是同一次构建".into(),
        ));
    }
    if metadata.models.len() != artifact.model_ids.len() {
        return Err(FingerprintError::Invalid(
            "元数据与检测器的模型数量不一致".into(),
        ));
    }
    for (index, model) in metadata.models.iter().enumerate() {
        if model.id != artifact.model_ids[index]
            || model.response_count != artifact.response_counts[index] as u64
        {
            return Err(FingerprintError::Invalid(
                "元数据与检测器的模型顺序/样本数不一致".into(),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 远端更新
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LeanBank {
    schema: String,
    built_at: String,
    models: Vec<LeanModel>,
}

#[derive(Debug, Deserialize)]
struct LeanModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    family_name: Option<String>,
    response_count: u64,
    #[serde(default)]
    valid_number_count: u64,
    #[serde(default)]
    sources: BTreeMap<String, u64>,
    #[serde(default)]
    conditions: BTreeMap<String, u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoteManifest {
    schema: String,
    #[serde(default)]
    source_base: String,
    #[serde(default)]
    source_commit: Option<String>,
    built_at: String,
    model_count: usize,
    artifact_sha256: String,
    #[serde(default)]
    bank_sha256: String,
    #[serde(default)]
    downloaded_at: i64,
}

fn normalize_remote_base(base: &str) -> Result<String, FingerprintError> {
    let parsed = Url::parse(base)
        .map_err(|error| FingerprintError::Update(format!("参考库地址无效：{error}")))?;
    let allowed = parsed.scheme() == "https"
        || (parsed.scheme() == "http"
            && matches!(parsed.host_str(), Some("127.0.0.1") | Some("localhost")));
    if !allowed {
        return Err(FingerprintError::Update(
            "参考库地址必须是 https（本机镜像可用 http://127.0.0.1 或 http://localhost）".into(),
        ));
    }
    Ok(base.trim_end_matches('/').to_string())
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, FingerprintError> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .send()
        .await?
        .error_for_status()?;
    if let Some(length) = response.content_length() {
        if length as usize > MAX_DOWNLOAD_BYTES {
            return Err(FingerprintError::Update(format!(
                "参考库文件太大：{length} 字节"
            )));
        }
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_DOWNLOAD_BYTES {
        return Err(FingerprintError::Update(format!(
            "参考库文件太大：{} 字节",
            bytes.len()
        )));
    }
    Ok(bytes.to_vec())
}

async fn write_remote(
    artifact: &[u8],
    bank: &[u8],
    metadata: &[u8],
    manifest: &[u8],
) -> Result<(), FingerprintError> {
    let directory = paths::fingerprint_remote_dir();
    let parent = directory
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::fingerprint_dir());
    tokio::fs::create_dir_all(&parent).await?;

    let temporary = parent.join(format!("remote.tmp-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&temporary).await?;
    tokio::fs::write(temporary.join("shared_detector.bin"), artifact).await?;
    tokio::fs::write(temporary.join("unified_bank.json"), bank).await?;
    tokio::fs::write(temporary.join("models.json"), metadata).await?;
    tokio::fs::write(temporary.join("manifest.json"), manifest).await?;

    let previous = parent.join("remote.previous");
    if previous.exists() {
        tokio::fs::remove_dir_all(&previous).await?;
    }
    if directory.exists() {
        tokio::fs::rename(&directory, &previous).await?;
    }
    if let Err(error) = tokio::fs::rename(&temporary, &directory).await {
        // 切换失败时把旧版本放回去；放不回去也只能报告原始错误。
        let _ = tokio::fs::rename(&previous, &directory).await;
        let _ = tokio::fs::remove_dir_all(&temporary).await;
        return Err(FingerprintError::Update(format!(
            "写入远端参考库失败：{error}"
        )));
    }
    let _ = tokio::fs::remove_dir_all(&previous).await;
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_library_loads_and_matches_its_metadata() {
        let library = load_embedded().expect("内置参考库应能加载");
        assert_eq!(library.source, LibrarySource::Embedded);
        assert_eq!(library.artifact.model_ids.len(), 60);
        assert_eq!(library.metadata.models.len(), 60);
        assert_eq!(library.challenges.len(), 36);
        assert_eq!(library.built_at, library.artifact.bank_built_at);
        assert_eq!(library.sha256.len(), 64);
    }

    #[test]
    fn rejects_http_remote_bases_except_loopback() {
        assert!(normalize_remote_base("https://example.com/data").is_ok());
        assert!(normalize_remote_base("http://127.0.0.1:8080/data").is_ok());
        assert!(normalize_remote_base("http://localhost:8080/data").is_ok());
        assert!(normalize_remote_base("http://example.com/data").is_err());
        assert!(normalize_remote_base("file:///tmp/data").is_err());
    }

    #[test]
    fn metadata_mismatch_is_rejected() {
        let bytes = zstd::stream::decode_all(Cursor::new(EMBEDDED_ARTIFACT)).unwrap();
        let artifact = decode_artifact(&bytes).unwrap();
        let mut metadata: LibraryMetadata = serde_json::from_str(EMBEDDED_MODELS).unwrap();
        metadata.models[0].id = "definitely-not-in-the-artifact".into();
        assert!(validate_library(&artifact, &metadata).is_err());
    }
}
