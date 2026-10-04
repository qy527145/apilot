//! 渠道管理。

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::shell::AppShell;
use crate::storage::models::{Provider, ProviderModel};
use crate::storage::providers::ProviderInput;

/// 连通性探测结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub tag: String,
    pub ok: bool,
    pub latency_ms: Option<i64>,
    pub error: Option<String>,
}

#[tauri::command]
pub async fn list_providers(shell: State<'_, Arc<AppShell>>) -> AppResult<Vec<Provider>> {
    crate::storage::providers::list(&shell.db).await
}

#[tauri::command]
pub async fn upsert_provider(
    shell: State<'_, Arc<AppShell>>,
    input: ProviderInput,
) -> AppResult<Provider> {
    let saved = crate::storage::providers::upsert(&shell.db, &input).await?;

    // 记下用的是哪个密钥（打码）—— 排查"为什么 401"时，
    // 能一眼看出是配置里根本没 key、还是 key 填错了。
    match saved.api_key.as_deref() {
        Some(k) if !k.is_empty() => tracing::info!(
            tag = %saved.tag,
            key = %crate::util::mask_secret(k),
            "渠道已保存"
        ),
        _ => tracing::info!(tag = %saved.tag, "渠道已保存（未配置密钥）"),
    }

    // 改完立刻生效，不必重启网关。
    shell.reload_providers().await?;

    // 新增渠道自动并入 default selector，否则它不会有任何流量。
    let mut sel_inputs = crate::storage::routing::list_selectors(&shell.db).await?;
    if let Some(default) = sel_inputs.iter_mut().find(|s| s.tag == "default") {
        if !default.members.contains(&saved.tag) {
            default.members.push(saved.tag.clone());
            crate::storage::routing::upsert_selector(
                &shell.db,
                &crate::storage::routing::SelectorInput {
                    tag: default.tag.clone(),
                    name: default.name.clone(),
                    mode: default.mode,
                    members: default.members.clone(),
                    tolerance_ms: default.tolerance_ms,
                },
            )
            .await?;
            shell.reload_selectors().await?;
        }
    }
    let _ = sel_inputs;

    Ok(saved)
}

#[tauri::command]
pub async fn delete_provider(shell: State<'_, Arc<AppShell>>, id: i64) -> AppResult<()> {
    // 先取出 tag：删完之后就查不到了，但 selector 里还留着它。
    let tag = crate::storage::providers::get(&shell.db, id)
        .await?
        .map(|p| p.tag);

    crate::storage::providers::delete(&shell.db, id).await?;

    // 把所有 selector 里的这个成员摘掉，避免选中已被删除的渠道。
    if let Some(tag) = tag {
        for sel in crate::storage::routing::list_selectors(&shell.db).await? {
            if sel.members.contains(&tag) {
                let members: Vec<String> = sel
                    .members
                    .iter()
                    .filter(|m| **m != tag)
                    .cloned()
                    .collect();
                crate::storage::routing::upsert_selector(
                    &shell.db,
                    &crate::storage::routing::SelectorInput {
                        tag: sel.tag.clone(),
                        name: sel.name.clone(),
                        mode: sel.mode,
                        members,
                        tolerance_ms: sel.tolerance_ms,
                    },
                )
                .await?;
            }
        }
    }

    shell.reload_providers().await?;
    shell.reload_selectors().await?;
    Ok(())
}

/// 探测渠道连通性。
///
/// 打的是 `/v1/models`：它是只读的、不消耗配额，且几乎所有兼容实现都有。
/// 返回非 2xx 一律算失败 —— 「能连上但密钥错了」对用户来说同样是"不可用"，
/// 只报「网络通」会让人白排查半天。
pub async fn probe(shell: &Arc<AppShell>, provider: &Provider) -> ProbeResult {
    let started = Instant::now();

    let url = provider.endpoint("/v1/models");
    let mut req = shell.registry.client().get(&url);

    if let Some(key) = provider.api_key.as_deref() {
        if !key.is_empty() {
            req = match provider.auth_style {
                crate::storage::models::AuthStyle::XApiKey => req.header("x-api-key", key),
                crate::storage::models::AuthStyle::None => req,
                _ => req.bearer_auth(key),
            };
        }
    }
    for (k, v) in &provider.extra_headers {
        req = req.header(k, v);
    }

    match req
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(resp) => {
            let latency = started.elapsed().as_millis() as i64;
            let status = resp.status();
            if status.is_success() {
                ProbeResult {
                    tag: provider.tag.clone(),
                    ok: true,
                    latency_ms: Some(latency),
                    error: None,
                }
            } else {
                ProbeResult {
                    tag: provider.tag.clone(),
                    ok: false,
                    latency_ms: Some(latency),
                    error: Some(format!(
                        "上游返回 {} — {}",
                        status.as_u16(),
                        status_hint(status.as_u16())
                    )),
                }
            }
        }
        Err(e) => ProbeResult {
            tag: provider.tag.clone(),
            ok: false,
            latency_ms: None,
            error: Some(if e.is_timeout() {
                "连接超时".to_string()
            } else if e.is_connect() {
                format!("无法连接: {e}")
            } else {
                e.to_string()
            }),
        },
    }
}

fn status_hint(status: u16) -> &'static str {
    match status {
        401 | 403 => "密钥无效或权限不足",
        404 => "该地址下没有 /v1/models，请检查 base_url",
        429 => "触发限流",
        s if s >= 500 => "上游服务异常",
        _ => "请检查 base_url 与密钥",
    }
}

#[tauri::command]
pub async fn test_provider(shell: State<'_, Arc<AppShell>>, id: i64) -> AppResult<ProbeResult> {
    let provider = crate::storage::providers::get(&shell.db, id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(id.to_string()))?;
    Ok(probe(shell.inner(), &provider).await)
}

#[tauri::command]
pub async fn list_provider_models(
    shell: State<'_, Arc<AppShell>>,
    provider_id: i64,
) -> AppResult<Vec<ProviderModel>> {
    crate::storage::providers::list_models(&shell.db, provider_id).await
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelMappingInput {
    pub model: String,
    #[serde(default)]
    pub upstream_model: Option<String>,
}

#[tauri::command]
pub async fn set_provider_models(
    shell: State<'_, Arc<AppShell>>,
    provider_id: i64,
    models: Vec<ModelMappingInput>,
) -> AppResult<()> {
    // 先确认渠道存在，否则外键会在写入时才报错，错误信息不够直白。
    if crate::storage::providers::get(&shell.db, provider_id)
        .await?
        .is_none()
    {
        return Err(AppError::ProviderNotFound(provider_id.to_string()));
    }

    let pairs: Vec<(String, Option<String>)> = models
        .into_iter()
        .map(|m| (m.model, m.upstream_model.filter(|s| !s.is_empty())))
        .collect();

    crate::storage::providers::set_models(&shell.db, provider_id, &pairs).await?;
    Ok(())
}
