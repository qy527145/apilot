//! 指纹检测与两级样本库的命令层。
//!
//! 检测请求走 `upstream::oneshot::send`，与「模型测试」一样**绕过网关管线**：
//! 不写监控日志、不计费、不查缓存、不做故障转移。探测要的正是这条渠道本身的
//! 回答，换渠道或混进用户流量都会让结论失去意义。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::fingerprint::analyze::analyze_shared_outputs;
use crate::fingerprint::challenges::pick_challenges;
use crate::fingerprint::local;
use crate::fingerprint::parse::parse_numbers;
use crate::fingerprint::types::{
    Analysis, AttemptResult, Candidate, Conclusion, DeclaredRank, FingerprintDetection,
    FingerprintLibraryInfo, FingerprintLocalLabel, FingerprintLocalSample, LibraryInfoLite,
    LocalResult, OfficialResult, Output,
};
use crate::fingerprint::ActiveLibrary;use crate::protocol::dto::{ContentBlock, UnifiedMessage, UnifiedRequest};
use crate::shell::AppShell;
use crate::storage::fingerprint as store;
use crate::storage::providers as provider_store;
use crate::upstream::oneshot;
use crate::util::now_ms;

/// 参考采集用的是 8192。给太小会让带思考的模型在写数字前就用完额度，
/// 得到"有效数字不足"而不是真实的数字分布。
const FINGERPRINT_MAX_TOKENS: u32 = 8192;
/// 回包预览截断长度；完整回答只用于解析数字，不落库。
const PREVIEW_CHARS: usize = 200;

fn minimum_numbers(expected_count: u32) -> usize {
    ((expected_count as f64) * 0.55).ceil().max(80.0) as usize
}

/// 单次检测：串行发 3 条挑战，官方排名 + 本地证据，最后落一条检测记录。
#[tauri::command]
pub async fn detect_model_fingerprint(
    shell: State<'_, Arc<AppShell>>,
    provider_id: i64,
    model: String,
) -> AppResult<FingerprintDetection> {
    let shell = shell.inner().clone();
    let declared = model.trim().to_string();
    if declared.is_empty() {
        return Err(AppError::msg("请先选择或填写要检测的模型"));
    }
    let provider = provider_store::get(&shell.db, provider_id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(provider_id.to_string()))?;
    let upstream = provider.upstream_model(&declared).to_string();
    let library = shell
        .fingerprint
        .active()
        .map_err(|error| AppError::msg(error.to_string()))?;
    let challenges = pick_challenges(&library.challenges, 3, now_ms() as u64);

    let started = Instant::now();
    let mut attempts: Vec<AttemptResult> = Vec::with_capacity(challenges.len());
    let mut outputs: Vec<Output> = Vec::with_capacity(challenges.len());

    // 串行发送。并发对一条渠道没有意义（耗时由单次生成决定），却更容易撞上
    // 限流；限流会让结论变成"有效回答不足"，等于白测。同一理由见
    // `detect_provider_protocols`。
    for (index, challenge) in challenges.iter().enumerate() {
        let mut request = UnifiedRequest::new(&upstream);
        request.max_tokens = Some(FINGERPRINT_MAX_TOKENS);
        request.messages = vec![UnifiedMessage::user_text(challenge.prompt.clone())];
        if !challenge.system.trim().is_empty() {
            request.system = vec![ContentBlock::text(challenge.system.clone())];
        }

        let outcome = oneshot::send(&shell, &provider, &request).await;
        let numbers = parse_numbers(&outcome.text);
        let minimum = minimum_numbers(challenge.expected_count);
        let accepted = numbers.len() >= minimum;
        let preview: String = outcome.preview.chars().take(PREVIEW_CHARS).collect();

        attempts.push(AttemptResult {
            index,
            challenge_id: challenge.challenge_id.clone(),
            expected_count: challenge.expected_count,
            ok: outcome.ok,
            status: outcome.status,
            latency_ms: outcome.latency_ms,
            wire: outcome.wire.as_str().to_string(),
            url: outcome.url.clone(),
            error: outcome.error.clone(),
            parsed_numbers: numbers.len(),
            minimum_numbers: minimum,
            accepted,
            text_preview: preview,
            numbers,
        });
        outputs.push(Output {
            text: outcome.text,
            expected_count: challenge.expected_count,
        });
    }

    let analysis =
        analyze_shared_outputs(&outputs, &library.metadata, &library.artifact, true)
            .map_err(AppError::msg)?;
    let official = build_official(&analysis, &declared, &upstream);
    // 序列形态门槛失败说明这组数字不像语言模型写的；此时用它们做本地匹配
    // 同样没有意义，直接把本地层留空，避免结论卡与官方诊断自相矛盾。
    let local = if analysis.decision == "not_model" {
        None
    } else {
        build_local(&shell, &library, &attempts).await?
    };
    let conclusion = build_conclusion(official.as_ref(), local.as_ref(), &library);

    let wire = attempts
        .first()
        .map(|attempt| attempt.wire.clone())
        .unwrap_or_else(|| provider.kind.wire_protocol().as_str().to_string());
    let mut result = FingerprintDetection {
        id: 0,
        declared_model: declared.clone(),
        upstream_model: upstream.clone(),
        wire: wire.clone(),
        status: analysis.decision.clone(),
        library: LibraryInfoLite {
            source: library.source,
            built_at: library.built_at.clone(),
            model_count: library.metadata.models.len(),
            sha256: library.sha256.clone(),
        },
        attempts,
        official,
        local,
        conclusion,
        elapsed_ms: started.elapsed().as_millis() as i64,
    };

    let result_json = serde_json::to_string(&result)?;
    let id = store::insert_detection(
        &shell.db,
        &store::NewDetection {
            provider_id: Some(provider_id),
            provider_tag: provider.tag.clone(),
            declared_model: declared,
            upstream_model: upstream,
            wire,
            library_source: library.source.as_str().to_string(),
            library_built_at: library.built_at.clone(),
            status: result.status.clone(),
            result_json,
            created_at: now_ms(),
        },
    )
    .await?;
    result.id = id;
    Ok(result)
}

fn build_official(analysis: &Analysis, declared: &str, upstream: &str) -> Option<OfficialResult> {
    if analysis.decision == "unscorable" || analysis.decision == "not_model" {
        return None;
    }
    let declared_rank = find_declared(&analysis.results, declared, "declared")
        .or_else(|| find_declared(&analysis.results, upstream, "upstream"));
    Some(OfficialResult {
        decision: analysis.decision.clone(),
        used_outputs: analysis.used_outputs,
        calibrated: analysis.probability_status == "reference_calibrated",
        probability_status: analysis.probability_status.clone(),
        diagnostics: analysis.diagnostics.clone(),
        sequence: analysis.sequence.clone(),
        candidates: analysis.results.clone(),
        declared: declared_rank,
        evidence: analysis.evidence.clone(),
    })
}

fn find_declared(candidates: &[Candidate], name: &str, matched_as: &str) -> Option<DeclaredRank> {
    candidates
        .iter()
        .enumerate()
        .find(|(_, candidate)| {
            candidate.model.eq_ignore_ascii_case(name)
                || candidate.display_name.eq_ignore_ascii_case(name)
        })
        .map(|(index, candidate)| DeclaredRank {
            model: candidate.model.clone(),
            rank: index + 1,
            score: candidate.score,
            probability: candidate.probability,
            matched_as: matched_as.to_string(),
        })
}

async fn build_local(
    shell: &Arc<AppShell>,
    library: &Arc<ActiveLibrary>,
    attempts: &[AttemptResult],
) -> AppResult<Option<LocalResult>> {
    let accepted: Vec<Vec<i32>> = attempts
        .iter()
        .filter(|attempt| attempt.accepted)
        .map(|attempt| attempt.numbers.clone())
        .collect();
    if accepted.is_empty() {
        return Ok(None);
    }
    let stored = store::list_all_samples(&shell.db).await?;
    let samples: Vec<(String, Vec<i32>)> = stored
        .into_iter()
        .filter(|sample| !sample.numbers.is_empty())
        .map(|sample| (sample.label, sample.numbers))
        .collect();
    if samples.is_empty() {
        return Ok(None);
    }
    local::analyze(&accepted, &library.artifact, &samples)
        .map(Some)
        .map_err(AppError::msg)
}

fn level_for(probability: Option<f64>) -> &'static str {
    match probability {
        Some(value) if value >= 0.8 => "high",
        Some(value) if value >= 0.5 => "medium",
        _ => "low",
    }
}

fn build_conclusion(
    official: Option<&OfficialResult>,
    local: Option<&LocalResult>,
    library: &ActiveLibrary,
) -> Conclusion {
    let official_top = official.and_then(|result| result.candidates.first());
    let local_top = local.and_then(|result| result.candidates.first());
    let display = |label: &str| -> String {
        library
            .metadata
            .find(label)
            .map(|model| model.display().to_string())
            .unwrap_or_else(|| label.to_string())
    };

    match (official_top, local_top) {
        (None, None) => Conclusion {
            model: None,
            display_name: None,
            basis: "insufficient".into(),
            level: "low".into(),
            note: "没有足够有效的回答，无法给出参考结论。".into(),
        },
        (None, Some(local_top)) => Conclusion {
            model: Some(local_top.label.clone()),
            display_name: Some(display(&local_top.label)),
            basis: "local_only".into(),
            level: if local_top.samples >= 6 && local_top.probability.unwrap_or(0.0) >= 0.8 {
                "medium".into()
            } else {
                "low".into()
            },
            note: "官方检测器这次无法评分，只有本地样本库可参考。".into(),
        },
        (Some(official_top), None) => Conclusion {
            model: Some(official_top.model.clone()),
            display_name: Some(official_top.display_name.clone()),
            basis: "official".into(),
            level: level_for(official_top.probability).into(),
            note: "本地样本不足（同类少于 3 条或样本库为空），结果仅来自官方参考库。".into(),
        },
        (Some(official_top), Some(local_top)) => {
            if official_top
                .model
                .eq_ignore_ascii_case(&local_top.label)
            {
                let high = official_top.probability.unwrap_or(0.0) >= 0.8
                    && local_top.samples >= 6
                    && local_top.probability.unwrap_or(0.0) >= 0.6;
                Conclusion {
                    model: Some(official_top.model.clone()),
                    display_name: Some(official_top.display_name.clone()),
                    basis: "agreement".into(),
                    level: if high {
                        "high".into()
                    } else if official_top.probability.unwrap_or(0.0) >= 0.5 {
                        "medium".into()
                    } else {
                        "low".into()
                    },
                    note: "官方参考库与本地样本一致。".into(),
                }
            } else {
                let local_strong = local_top.samples >= 6
                    && local_top.probability.unwrap_or(0.0) >= 0.8;
                let official_weak = official_top.probability.unwrap_or(0.0) < 0.5;
                if local_strong && official_weak {
                    Conclusion {
                        model: Some(local_top.label.clone()),
                        display_name: Some(display(&local_top.label)),
                        basis: "conflict".into(),
                        level: "medium".into(),
                        note: format!(
                            "本地样本强支持 {}；官方参考库更接近 {}。",
                            local_top.label, official_top.model
                        ),
                    }
                } else {
                    let mut note = format!(
                        "官方参考库更接近 {}；本地样本更支持 {}。",
                        official_top.model, local_top.label
                    );
                    if !library.metadata.contains(&local_top.label) {
                        note.push_str("（本地 Top 不在官方参考库中）");
                    }
                    Conclusion {
                        model: Some(official_top.model.clone()),
                        display_name: Some(official_top.display_name.clone()),
                        basis: "conflict".into(),
                        level: "low".into(),
                        note,
                    }
                }
            }
        }
    }
}

#[tauri::command]
pub fn fingerprint_library_info(shell: State<'_, Arc<AppShell>>) -> FingerprintLibraryInfo {
    shell.fingerprint.info()
}

#[tauri::command]
pub async fn update_fingerprint_library(
    shell: State<'_, Arc<AppShell>>,
    base_url: Option<String>,
) -> AppResult<FingerprintLibraryInfo> {
    let client = shell.registry.client();
    shell
        .fingerprint
        .update(&client, base_url.as_deref())
        .await
        .map_err(|error| AppError::msg(error.to_string()))?;
    Ok(shell.fingerprint.info())
}

#[tauri::command]
pub async fn reset_fingerprint_library(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<FingerprintLibraryInfo> {
    shell
        .fingerprint
        .reset()
        .await
        .map_err(|error| AppError::msg(error.to_string()))?;
    Ok(shell.fingerprint.info())
}

#[tauri::command]
pub async fn list_fingerprint_labels(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<Vec<FingerprintLocalLabel>> {
    let labels = store::list_labels(&shell.db).await?;
    let library = shell.fingerprint.active().ok();
    Ok(labels
        .into_iter()
        .map(|summary| FingerprintLocalLabel {
            official: library
                .as_ref()
                .map(|library| library.metadata.contains(&summary.label))
                .unwrap_or(false),
            label: summary.label,
            samples: summary.samples,
            last_added_at: summary.last_added_at,
        })
        .collect())
}

#[tauri::command]
pub async fn list_fingerprint_samples(
    shell: State<'_, Arc<AppShell>>,
    label: Option<String>,
) -> AppResult<Vec<FingerprintLocalSample>> {
    let label = match label {
        Some(label) if !label.trim().is_empty() => Some(store::normalize_label(&label)?),
        _ => None,
    };
    let samples = store::list_samples(&shell.db, label.as_deref()).await?;
    Ok(samples
        .into_iter()
        .map(|sample| FingerprintLocalSample {
            id: sample.id,
            label: sample.label,
            challenge_id: sample.challenge_id,
            expected_count: sample.expected_count,
            numbers_count: sample.numbers.len(),
            provider_tag: sample.provider_tag,
            declared_model: sample.declared_model,
            created_at: sample.created_at,
            note: sample.note,
        })
        .collect())
}

/// 把一次检测里的有效回答加入本地样本库。`sample_indexes` 指向检测记录里的
/// 回答下标；数字由后端从落库的结果 JSON 取，不信任前端传值。
#[tauri::command]
pub async fn add_fingerprint_samples(
    shell: State<'_, Arc<AppShell>>,
    detection_id: i64,
    label: String,
    sample_indexes: Vec<usize>,
    note: Option<String>,
) -> AppResult<usize> {
    let detection = store::get_detection(&shell.db, detection_id)
        .await?
        .ok_or_else(|| AppError::msg("找不到这次检测记录"))?;
    if detection.status == "not_model" || detection.status == "unscorable" {
        return Err(AppError::msg("这次检测没有可加入的有效回答"));
    }
    let result: FingerprintDetection = serde_json::from_str(&detection.result_json)?;
    let label = store::normalize_label(&label)?;
    if sample_indexes.is_empty() {
        return Err(AppError::msg("请至少选择一条回答"));
    }

    let library = shell.fingerprint.active().ok();
    let mut seen = HashSet::new();
    let mut samples = Vec::with_capacity(sample_indexes.len());
    for index in sample_indexes {
        if !seen.insert(index) {
            return Err(AppError::msg("回答索引重复"));
        }
        let attempt = result
            .attempts
            .get(index)
            .ok_or_else(|| AppError::msg("回答索引越界"))?;
        if !attempt.accepted {
            return Err(AppError::msg(format!(
                "挑战 {} 的有效数字不足，不能加入样本库",
                attempt.challenge_id
            )));
        }
        let prompt = library
            .as_ref()
            .and_then(|library| {
                library
                    .challenges
                    .iter()
                    .find(|challenge| challenge.challenge_id == attempt.challenge_id)
            })
            .map(|challenge| challenge.prompt.clone())
            .unwrap_or_default();
        let numbers = attempt.numbers.clone();
        samples.push(store::SampleInput {
            challenge_id: attempt.challenge_id.clone(),
            prompt,
            expected_count: attempt.expected_count,
            text_hash: store::sample_hash(&numbers),
            numbers,
            provider_id: detection.provider_id,
            provider_tag: detection.provider_tag.clone(),
            declared_model: detection.declared_model.clone(),
            upstream_model: detection.upstream_model.clone(),
            note: note.clone(),
        });
    }

    store::insert_samples(&shell.db, detection_id, &label, &samples).await
}

#[tauri::command]
pub async fn delete_fingerprint_sample(
    shell: State<'_, Arc<AppShell>>,
    id: i64,
) -> AppResult<()> {
    store::delete_sample(&shell.db, id).await
}

#[tauri::command]
pub async fn delete_fingerprint_label(
    shell: State<'_, Arc<AppShell>>,
    label: String,
) -> AppResult<usize> {
    let label = store::normalize_label(&label)?;
    store::delete_label(&shell.db, &label).await
}
