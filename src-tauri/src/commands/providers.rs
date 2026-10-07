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

/// 构造一个已注入该渠道鉴权与自定义头的 GET 请求。
///
/// 探测与拉模型都从这里出发：鉴权分支一旦分散成两份，加一种 `AuthStyle` 时
/// 必然只改一处，表现为"测试连通能过、拉列表 401"这种莫名其妙的组合。
fn authed_get(
    client: &reqwest::Client,
    provider: &Provider,
    url: &str,
) -> reqwest::RequestBuilder {
    let mut req = client.get(url);
    if let Some((name, value)) = provider.auth_header() {
        req = req.header(name, value);
    }
    for (k, v) in &provider.extra_headers {
        req = req.header(k, v);
    }
    req
}

/// 探测渠道连通性。
///
/// 打的是 `/v1/models`：它是只读的、不消耗配额，且几乎所有兼容实现都有。
/// 返回非 2xx 一律算失败 —— 「能连上但密钥错了」对用户来说同样是"不可用"，
/// 只报「网络通」会让人白排查半天。
pub async fn probe(shell: &Arc<AppShell>, provider: &Provider) -> ProbeResult {
    let started = Instant::now();

    let url = provider.endpoint("/v1/models");
    // 用该渠道自己的代理：探测结论必须与真实请求一致，否则会出现
    //「测试连通失败，但网关转发能过」这种自相矛盾的提示。
    let client = shell.registry.client_for(&provider.proxy);
    let req = authed_get(&client, provider, &url);

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
            error: Some(describe_network_error(&e)),
        },
    }
}

/// 把 reqwest 的错误说成人话。探测与拉模型共用，保证同一故障在两处说法一致。
fn describe_network_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "连接超时".to_string()
    } else if e.is_connect() {
        format!("无法连接: {e}")
    } else {
        e.to_string()
    }
}

/// 解析上游 `/v1/models` 的响应体，返回模型 id（去重、排序）。
///
/// 只认两种形状：OpenAI / Anthropic 的 `data[].id`，以及 Gemini 原生风格的
/// `models[].name`（形如 `models/gemini-2.0-flash`，前缀要剥掉）。
/// 形状不认识时返回 `Err` 而不是空表 —— 「拉到了 0 个模型」和「返回的不是模型列表」
/// 对用户的下一步操作完全不同，不能混为一谈。
fn parse_models_response(body: &[u8]) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("响应不是合法 JSON: {e}"))?;

    let items = value
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| (arr, "id"))
        .or_else(|| {
            value
                .get("models")
                .and_then(|v| v.as_array())
                .map(|arr| (arr, "name"))
        })
        .ok_or_else(|| "无法识别上游返回的模型列表格式（既没有 data 也没有 models 数组）".to_string())?;

    let (arr, key) = items;
    let mut out: Vec<String> = Vec::new();
    for item in arr {
        // 有的实现把 id 放在 name 里，反之亦然；两个都试，非字符串一律跳过。
        let raw = item
            .get(key)
            .and_then(|v| v.as_str())
            .or_else(|| item.get("id").and_then(|v| v.as_str()))
            .or_else(|| item.get("name").and_then(|v| v.as_str()));
        let Some(raw) = raw else { continue };

        // Gemini 原生用 "models/<id>" 作 name，带前缀直接发给上游会 404。
        let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
        if !id.is_empty() {
            out.push(id.to_string());
        }
    }

    out.sort();
    out.dedup();
    Ok(out)
}

/// 拉取上游声明的模型列表（`GET {base_url}/v1/models`）。
///
/// 与 `probe` 打的是同一个端点，区别是这个把响应体解析出来给用户勾选，
/// 而不是只回报"通不通"。超时放得比 probe 宽：OpenRouter 这类服务商一次返回几百个模型。
#[tauri::command]
pub async fn fetch_provider_models(
    shell: State<'_, Arc<AppShell>>,
    id: i64,
) -> AppResult<Vec<String>> {
    let provider = crate::storage::providers::get(&shell.db, id)
        .await?
        .ok_or_else(|| AppError::ProviderNotFound(id.to_string()))?;

    let url = provider.endpoint("/v1/models");
    let client = shell.registry.client_for(&provider.proxy);
    let req = authed_get(&client, &provider, &url);

    let resp = req
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| AppError::msg(format!("拉取模型列表失败：{}", describe_network_error(&e))))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(AppError::msg(format!(
            "拉取模型列表失败 — 上游返回 {}：{}",
            status.as_u16(),
            status_hint(status.as_u16())
        )));
    }

    let body = resp
        .bytes()
        .await
        .map_err(|e| AppError::msg(format!("读取模型列表响应失败：{e}")))?;

    parse_models_response(&body).map_err(AppError::msg)
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

/// 全量替换某渠道**声明提供**的模型名。
///
/// 只收模型名：上游重定向名归模型页管（`set_model_candidates`），
/// 从这个入口写不了，已有的那份会被原样保留。
#[tauri::command]
pub async fn set_provider_models(
    shell: State<'_, Arc<AppShell>>,
    provider_id: i64,
    models: Vec<String>,
) -> AppResult<()> {
    // 先确认渠道存在，否则外键会在写入时才报错，错误信息不够直白。
    if crate::storage::providers::get(&shell.db, provider_id)
        .await?
        .is_none()
    {
        return Err(AppError::ProviderNotFound(provider_id.to_string()));
    }

    crate::storage::providers::set_models(&shell.db, provider_id, &models).await?;

    // 映射会同步进 providers.model_mapping，而请求改写读的是内存里 Channel 持有的
    // Provider 快照 —— 不重载的话新映射要等重启才生效。
    shell.reload_providers().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Vec<String>, String> {
        parse_models_response(s.as_bytes())
    }

    #[test]
    fn parses_openai_style_data_ids() {
        let body = r#"{"object":"list","data":[
            {"id":"deepseek-chat","object":"model"},
            {"id":"deepseek-reasoner","object":"model"}]}"#;
        assert_eq!(parse(body).unwrap(), vec!["deepseek-chat", "deepseek-reasoner"]);
    }

    #[test]
    fn parses_anthropic_style_and_ignores_display_name() {
        // Anthropic 的 data[].id 就是模型名，display_name 只是给人看的。
        let body = r#"{"data":[{"id":"claude-sonnet-4-5","display_name":"Claude Sonnet 4.5"}]}"#;
        assert_eq!(parse(body).unwrap(), vec!["claude-sonnet-4-5"]);
    }

    #[test]
    fn parses_gemini_models_name_and_strips_prefix() {
        // Gemini 原生用 "models/<id>"，带前缀原样发回上游会 404。
        let body = r#"{"models":[{"name":"models/gemini-2.0-flash"},{"name":"models/gemini-1.5-pro"}]}"#;
        assert_eq!(parse(body).unwrap(), vec!["gemini-1.5-pro", "gemini-2.0-flash"]);
    }

    #[test]
    fn dedups_and_sorts() {
        let body = r#"{"data":[{"id":"b"},{"id":"a"},{"id":"b"}]}"#;
        assert_eq!(parse(body).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn skips_entries_without_a_usable_string_id() {
        // 非字符串 id、空串、非对象项都要跳过，不能因为一条脏数据整批失败。
        let body = r#"{"data":[{"id":123},{"id":""},{"id":"ok"},null,42]}"#;
        assert_eq!(parse(body).unwrap(), vec!["ok"]);
    }

    #[test]
    fn falls_back_to_name_when_id_is_absent() {
        // 部分 OpenAI 兼容实现只给 name；漏掉它会让这些服务商"拉不到任何模型"。
        let body = r#"{"data":[{"name":"from-name"},{"id":"from-id"}]}"#;
        assert_eq!(parse(body).unwrap(), vec!["from-id", "from-name"]);
    }

    #[test]
    fn trims_surrounding_whitespace() {
        // 名字里带空白直接发出去会 404。
        assert_eq!(parse(r#"{"data":[{"id":"  m  "}]}"#).unwrap(), vec!["m"]);
    }

    #[test]
    fn empty_data_is_a_valid_empty_list() {
        // "上游一个模型都没有"是合法结果，不该报错。
        assert_eq!(parse(r#"{"data":[]}"#).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse("not json").is_err());
    }

    #[test]
    fn unrecognized_shape_is_an_error() {
        // 报错而不是空表：用户才能区分"拉到了空列表"和"这个地址根本不是模型接口"。
        assert!(parse(r#"{"foo":"bar"}"#).is_err());
        assert!(parse(r#"{"data":{"id":"x"}}"#).is_err());
    }

    #[test]
    fn model_endpoint_does_not_duplicate_v1_prefix() {
        // 预设里 OpenAI/Moonshot 的 base_url 自带 /v1，拼出 /v1/v1/models 就会 404。
        use crate::storage::models::{AuthStyle, Provider, ProviderKind};
        let p = Provider {
            id: 1,
            tag: "t".into(),
            name: "n".into(),
            kind: ProviderKind::OpenAiChat,
            base_url: "https://api.moonshot.cn/v1".into(),
            api_key: None,
            auth_style: AuthStyle::Bearer,
            protocols: Vec::new(),
            extra_headers: Default::default(),
            param_override: None,
            model_mapping: Default::default(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 60_000,
            proxy: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(p.endpoint("/v1/models"), "https://api.moonshot.cn/v1/models");
    }
}
