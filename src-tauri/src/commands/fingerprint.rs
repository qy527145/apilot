//! 指纹检测与两级样本库的命令层。
//!
//! 检测请求走 `upstream::oneshot::send`，与「模型测试」一样**绕过网关管线**：
//! 不写监控日志、不计费、不查缓存、不做故障转移。探测要的正是这条渠道本身的
//! 回答，换渠道或混进用户流量都会让结论失去意义。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::{FuturesUnordered, StreamExt};
use tauri::State;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::fingerprint::analyze::analyze_shared_outputs;
use crate::fingerprint::challenges::{select_challenges, Challenge};
use crate::fingerprint::local;
use crate::fingerprint::parse::parse_numbers;
use crate::fingerprint::types::{
    Analysis, AttemptResult, Candidate, Conclusion, DeclaredRank, FingerprintChallenge,
    FingerprintDetection, FingerprintDetectionSummary, FingerprintLibraryInfo,
    FingerprintLocalLabel, FingerprintLocalSample, FingerprintProgress, LibraryInfoLite,
    LocalResult, OfficialResult, Output,
};
use crate::fingerprint::ActiveLibrary;
use crate::protocol::dto::{ContentBlock, UnifiedMessage, UnifiedRequest};
use crate::shell::AppShell;
use crate::storage::fingerprint as store;
use crate::storage::models::Provider;
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

/// 单次检测：默认随机抽 3 条挑战，也可以由用户手动指定；官方排名 + 本地证据，
/// 最后落一条检测记录。`parallel` 默认 true；串行模式留给容易限流的渠道。
#[tauri::command]
pub async fn detect_model_fingerprint(
    shell: State<'_, Arc<AppShell>>,
    provider_id: i64,
    model: String,
    parallel: Option<bool>,
    challenge_ids: Option<Vec<String>>,
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
    let challenges = select_challenges(
        &library.challenges,
        challenge_ids.as_deref(),
        now_ms() as u64,
    )
    .map_err(AppError::msg)?;
    let parallel = parallel.unwrap_or(true);
    let total = challenges.len();
    let run_id = Uuid::new_v4().to_string();
    let started = Instant::now();

    let base = FingerprintProgress {
        run_id: run_id.clone(),
        phase: "started".into(),
        provider_id,
        provider_tag: provider.tag.clone(),
        declared_model: declared.clone(),
        upstream_model: upstream.clone(),
        total,
        completed: 0,
        accepted: 0,
        parallel,
        challenge_index: None,
        challenge_id: None,
        expected_count: None,
        challenge_prompt: None,
        message: format!(
            "开始检测：{total} 条挑战（{}）",
            if parallel { "并行" } else { "串行" }
        ),
        ok: None,
        status: None,
        latency_ms: None,
        parsed_numbers: None,
        minimum_numbers: None,
        wire: None,
        url: None,
        error: None,
        preview: None,
        detection_id: None,
        result_status: None,
    };
    shell.events.fingerprint(&base);

    let mut attempts: Vec<Option<AttemptResult>> = (0..total).map(|_| None).collect();
    let mut outputs: Vec<Output> = challenges
        .iter()
        .map(|challenge| Output {
            text: String::new(),
            expected_count: challenge.expected_count,
        })
        .collect();
    let mut completed = 0usize;
    let mut accepted_count = 0usize;

    if parallel {
        // 并行用 FuturesUnordered 而不是 join_all：谁先回来就先推进度，
        // 用户能看到"第 2 条已完成、第 1 条还在跑"，而不是三条一起转圈。
        let mut pending = FuturesUnordered::new();
        for (index, challenge) in challenges.iter().cloned().enumerate() {
            let task_shell = shell.clone();
            let task_provider = provider.clone();
            let task_upstream = upstream.clone();
            let task_base = base.clone();
            pending.push(async move {
                task_shell
                    .events
                    .fingerprint(&requesting_event(&task_base, index, &challenge));
                let outcome =
                    send_challenge(&task_shell, &task_provider, &task_upstream, &challenge)
                        .await;
                (index, challenge, outcome)
            });
        }
        while let Some((index, challenge, outcome)) = pending.next().await {
            let (attempt, output) = attempt_and_output(index, &challenge, &outcome);
            completed += 1;
            if attempt.accepted {
                accepted_count += 1;
            }
            shell
                .events
                .fingerprint(&answered_event(&base, completed, accepted_count, &attempt));
            attempts[index] = Some(attempt);
            outputs[index] = output;
        }
    } else {
        for (index, challenge) in challenges.iter().enumerate() {
            shell
                .events
                .fingerprint(&requesting_event(&base, index, challenge));
            let outcome = send_challenge(&shell, &provider, &upstream, challenge).await;
            let (attempt, output) = attempt_and_output(index, challenge, &outcome);
            completed += 1;
            if attempt.accepted {
                accepted_count += 1;
            }
            shell
                .events
                .fingerprint(&answered_event(&base, completed, accepted_count, &attempt));
            attempts[index] = Some(attempt);
            outputs[index] = output;
        }
    }

    let attempts: Vec<AttemptResult> = attempts
        .into_iter()
        .enumerate()
        .map(|(index, attempt)| {
            attempt.unwrap_or_else(|| fallback_attempt(index, &challenges[index], "请求未完成".into()))
        })
        .collect();

    shell.events.fingerprint(&phase_event(
        &base,
        "analyzing",
        completed,
        accepted_count,
        format!("{total} 条请求完成，正在计算排名…"),
    ));

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

    let mut done = phase_event(&base, "done", completed, accepted_count, "检测完成".into());
    done.detection_id = Some(id);
    done.result_status = Some(result.status.clone());
    shell.events.fingerprint(&done);
    Ok(result)
}

async fn send_challenge(
    shell: &Arc<AppShell>,
    provider: &Provider,
    upstream: &str,
    challenge: &Challenge,
) -> oneshot::OneshotOutcome {
    let mut request = UnifiedRequest::new(upstream);
    request.max_tokens = Some(FINGERPRINT_MAX_TOKENS);
    request.messages = vec![UnifiedMessage::user_text(challenge.prompt.clone())];
    if !challenge.system.trim().is_empty() {
        request.system = vec![ContentBlock::text(challenge.system.clone())];
    }
    oneshot::send(shell, provider, &request).await
}

fn attempt_and_output(
    index: usize,
    challenge: &Challenge,
    outcome: &oneshot::OneshotOutcome,
) -> (AttemptResult, Output) {
    let numbers = parse_numbers(&outcome.text);
    let minimum = minimum_numbers(challenge.expected_count);
    let accepted = numbers.len() >= minimum;
    let preview: String = outcome.preview.chars().take(PREVIEW_CHARS).collect();
    let attempt = AttemptResult {
        index,
        challenge_id: challenge.challenge_id.clone(),
        challenge_prompt: challenge.prompt.clone(),
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
        text: outcome.text.clone(),
        numbers,
    };
    let output = Output {
        text: outcome.text.clone(),
        expected_count: challenge.expected_count,
    };
    (attempt, output)
}

fn fallback_attempt(index: usize, challenge: &Challenge, error: String) -> AttemptResult {
    let minimum = minimum_numbers(challenge.expected_count);
    AttemptResult {
        index,
        challenge_id: challenge.challenge_id.clone(),
        challenge_prompt: challenge.prompt.clone(),
        expected_count: challenge.expected_count,
        ok: false,
        status: None,
        latency_ms: 0,
        wire: String::new(),
        url: String::new(),
        error: Some(error),
        parsed_numbers: 0,
        minimum_numbers: minimum,
        accepted: false,
        text_preview: String::new(),
        text: String::new(),
        numbers: Vec::new(),
    }
}

fn phase_event(
    base: &FingerprintProgress,
    phase: &str,
    completed: usize,
    accepted: usize,
    message: String,
) -> FingerprintProgress {
    let mut event = base.clone();
    event.phase = phase.to_string();
    event.completed = completed;
    event.accepted = accepted;
    event.challenge_index = None;
    event.challenge_id = None;
    event.expected_count = None;
    event.challenge_prompt = None;
    event.message = message;
    event
}

fn requesting_event(
    base: &FingerprintProgress,
    index: usize,
    challenge: &Challenge,
) -> FingerprintProgress {
    let mut event = phase_event(
        base,
        "requesting",
        base.completed,
        base.accepted,
        format!("正在请求挑战 {}/{}", index + 1, base.total),
    );
    event.challenge_index = Some(index);
    event.challenge_id = Some(challenge.challenge_id.clone());
    event.expected_count = Some(challenge.expected_count);
    event.challenge_prompt = Some(challenge.prompt.clone());
    event
}

fn answered_event(
    base: &FingerprintProgress,
    completed: usize,
    accepted: usize,
    attempt: &AttemptResult,
) -> FingerprintProgress {
    let mut event = phase_event(
        base,
        "answered",
        completed,
        accepted,
        format!("挑战 {} 完成", attempt.index + 1),
    );
    event.challenge_index = Some(attempt.index);
    event.challenge_id = Some(attempt.challenge_id.clone());
    event.expected_count = Some(attempt.expected_count);
    event.challenge_prompt = Some(attempt.challenge_prompt.clone());
    event.ok = Some(attempt.ok);
    event.status = attempt.status;
    event.latency_ms = Some(attempt.latency_ms);
    event.parsed_numbers = Some(attempt.parsed_numbers);
    event.minimum_numbers = Some(attempt.minimum_numbers);
    event.wire = Some(attempt.wire.clone());
    event.url = Some(attempt.url.clone());
    event.error = attempt.error.clone();
    event.preview = Some(attempt.text_preview.clone());
    event
}

/// 单独重试一次检测里的某条挑战，保留其余两条结果并重新计算排名。
///
/// 用于某条请求失败/数字不足，或者用户就是想再确认一次。重试结果覆盖原检测
/// 记录；原始回答（`AttemptResult.text`）也一起存进去，历史详情可以回看。
#[tauri::command]
pub async fn retry_fingerprint_attempt(
    shell: State<'_, Arc<AppShell>>,
    detection_id: i64,
    attempt_index: usize,
) -> AppResult<FingerprintDetection> {
    let shell = shell.inner().clone();
    let row = store::get_detection(&shell.db, detection_id)
        .await?
        .ok_or_else(|| AppError::msg("找不到该检测记录"))?;
    let provider_id = row
        .provider_id
        .ok_or_else(|| AppError::msg("这条检测记录没有关联渠道，无法重试"))?;
    let provider = provider_store::get(&shell.db, provider_id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(provider_id.to_string()))?;

    let mut result: FingerprintDetection = serde_json::from_str(&row.result_json)
        .map_err(|_| AppError::msg("该检测记录的结果已损坏"))?;
    result.id = row.id;
    let attempt = result
        .attempts
        .get(attempt_index)
        .cloned()
        .ok_or_else(|| AppError::msg("回答索引越界"))?;
    let library = shell
        .fingerprint
        .active()
        .map_err(|error| AppError::msg(error.to_string()))?;
    let challenge = match library
        .challenges
        .iter()
        .find(|challenge| challenge.challenge_id == attempt.challenge_id)
    {
        Some(challenge) => challenge.clone(),
        None if !attempt.challenge_prompt.is_empty() => Challenge {
            condition: String::new(),
            system: String::new(),
            user_prefix: String::new(),
            transport: String::new(),
            challenge_id: attempt.challenge_id.clone(),
            expected_count: attempt.expected_count,
            prompt: attempt.challenge_prompt.clone(),
        },
        None => {
            return Err(AppError::msg(
                "找不到这条挑战的提示词，无法重试",
            ))
        }
    };

    let started = Instant::now();
    let outcome = send_challenge(&shell, &provider, &result.upstream_model, &challenge).await;
    let (new_attempt, _) = attempt_and_output(attempt_index, &challenge, &outcome);
    result.attempts[attempt_index] = new_attempt;

    let outputs: Vec<Output> = result
        .attempts
        .iter()
        .map(|attempt| Output {
            // 新记录存了原文；老记录没有，就用解析后的数字重建一份，
            // 足够让分析器复算特征。
            text: if attempt.text.is_empty() {
                attempt
                    .numbers
                    .iter()
                    .map(|number| number.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                attempt.text.clone()
            },
            expected_count: attempt.expected_count,
        })
        .collect();

    let analysis =
        analyze_shared_outputs(&outputs, &library.metadata, &library.artifact, true)
            .map_err(AppError::msg)?;
    let official = build_official(&analysis, &result.declared_model, &result.upstream_model);
    let local = if analysis.decision == "not_model" {
        None
    } else {
        build_local(&shell, &library, &result.attempts).await?
    };
    result.conclusion = build_conclusion(official.as_ref(), local.as_ref(), &library);
    result.official = official;
    result.local = local;
    result.status = analysis.decision;
    result.elapsed_ms += started.elapsed().as_millis() as i64;
    result.library = LibraryInfoLite {
        source: library.source,
        built_at: library.built_at.clone(),
        model_count: library.metadata.models.len(),
        sha256: library.sha256.clone(),
    };

    let result_json = serde_json::to_string(&result)?;
    store::update_detection_result(&shell.db, detection_id, &result.status, &result_json).await?;
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

/// 挑战库全文，供前端手动选择。随机模式不需要它，但手动模式要看到提示词。
#[tauri::command]
pub fn list_fingerprint_challenges(
    shell: State<'_, Arc<AppShell>>,
) -> AppResult<Vec<FingerprintChallenge>> {
    let library = shell
        .fingerprint
        .active()
        .map_err(|error| AppError::msg(error.to_string()))?;
    Ok(library
        .challenges
        .iter()
        .map(|challenge| FingerprintChallenge {
            challenge_id: challenge.challenge_id.clone(),
            condition: challenge.condition.clone(),
            expected_count: challenge.expected_count,
            prompt: challenge.prompt.clone(),
            system: challenge.system.clone(),
        })
        .collect())
}

fn summary_from_row(row: store::DetectionRow) -> FingerprintDetectionSummary {
    let parsed: Option<FingerprintDetection> = serde_json::from_str(&row.result_json).ok();
    let official = parsed.as_ref().and_then(|result| result.official.as_ref());
    let local = parsed.as_ref().and_then(|result| result.local.as_ref());
    let official_top = official
        .and_then(|result| result.candidates.first())
        .map(|candidate| candidate.model.clone());
    let official_probability = official
        .and_then(|result| result.candidates.first())
        .and_then(|candidate| candidate.probability);
    let local_top = local
        .and_then(|result| result.candidates.first())
        .map(|candidate| candidate.label.clone());
    let local_probability = local
        .and_then(|result| result.candidates.first())
        .and_then(|candidate| candidate.probability);
    let conclusion = parsed
        .as_ref()
        .map(|result| result.conclusion.clone())
        .unwrap_or_else(|| Conclusion {
            model: None,
            display_name: None,
            basis: "insufficient".into(),
            level: "low".into(),
            note: "检测记录的结果无法解析。".into(),
        });

    FingerprintDetectionSummary {
        id: row.id,
        provider_id: row.provider_id,
        provider_tag: row.provider_tag,
        declared_model: row.declared_model,
        upstream_model: row.upstream_model,
        wire: row.wire,
        library_source: row.library_source,
        library_built_at: row.library_built_at,
        status: row.status,
        created_at: row.created_at,
        official_top,
        official_probability,
        local_top,
        local_probability,
        conclusion,
    }
}

/// 最近的检测记录。完整结果用 `get_fingerprint_detection` 按 id 取。
#[tauri::command]
pub async fn list_fingerprint_detections(
    shell: State<'_, Arc<AppShell>>,
    limit: Option<i64>,
) -> AppResult<Vec<FingerprintDetectionSummary>> {
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let rows = store::list_detections(&shell.db, limit).await?;
    Ok(rows.into_iter().map(summary_from_row).collect())
}

#[tauri::command]
pub async fn get_fingerprint_detection(
    shell: State<'_, Arc<AppShell>>,
    detection_id: i64,
) -> AppResult<FingerprintDetection> {
    let row = store::get_detection(&shell.db, detection_id)
        .await?
        .ok_or_else(|| AppError::msg("找不到该检测记录"))?;
    let mut result: FingerprintDetection = serde_json::from_str(&row.result_json)
        .map_err(|_| AppError::msg("该检测记录的结果已损坏"))?;
    // 落库时还不知道自增 id，存储的 JSON 里是 0；回读时补上真实 id，
    // 这样历史记录里的「加入样本库」也能按 id 回查原始回答。
    result.id = row.id;
    Ok(result)
}

#[tauri::command]
pub async fn delete_fingerprint_detection(
    shell: State<'_, Arc<AppShell>>,
    detection_id: i64,
) -> AppResult<()> {
    store::delete_detection(&shell.db, detection_id).await
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
        // 新记录直接带提示词；老记录（这个字段加之前落库的）回查挑战库兜底。
        let prompt = if attempt.challenge_prompt.is_empty() {
            library
                .as_ref()
                .and_then(|library| {
                    library
                        .challenges
                        .iter()
                        .find(|challenge| challenge.challenge_id == attempt.challenge_id)
                })
                .map(|challenge| challenge.prompt.clone())
                .unwrap_or_default()
        } else {
            attempt.challenge_prompt.clone()
        };
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
