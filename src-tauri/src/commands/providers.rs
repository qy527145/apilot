//! 渠道管理。

use std::sync::Arc;
use std::time::Instant;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::protocol::dto::Protocol;
use crate::shell::AppShell;
use crate::storage::models::{
    AuthStyle, ChannelProxy, ProtocolEndpoint, Provider, ProviderKind, ProviderModel,
};
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

/// 就地启用 / 停用渠道（表格「状态」那一列的开关）。
///
/// 只改这一个字段，不整份回传 —— 理由见 `storage::providers::set_enabled`。
#[tauri::command]
pub async fn set_provider_enabled(
    shell: State<'_, Arc<AppShell>>,
    id: i64,
    enabled: bool,
) -> AppResult<()> {
    crate::storage::providers::set_enabled(&shell.db, id, enabled).await?;

    // 注册表里只装启用的渠道（`reload_providers` 读的是 `list_enabled`），
    // 不重载的话这次拨动要等重启才生效 —— 而界面上的开关已经翻过去了，
    // 正是最难排查的那种「改了没反应」。
    shell.reload_providers().await?;
    Ok(())
}

/// 给一个出站探测请求补上该渠道的鉴权头与自定义头。
///
/// 连通探测、拉模型列表、协议检测三条路都从这里过：鉴权分支一旦分散成几份，
/// 加一种 `AuthStyle` 时必然只改一处，表现为"测试连通能过、换个检测就 401"
/// 这种莫名其妙的组合。
fn authed(mut req: reqwest::RequestBuilder, provider: &Provider) -> reqwest::RequestBuilder {
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
    let req = authed(client.get(&url), provider);

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
    let req = authed(client.get(&url), &provider);

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

// ---------------------------------------------------------------------------
// 协议自动检测
// ---------------------------------------------------------------------------

/// 要试的协议，顺序即界面上的顺序。
const PROBE_PROTOCOLS: [Protocol; 3] = [
    Protocol::AnthropicMessages,
    Protocol::OpenAiChat,
    Protocol::OpenAiResponses,
];

/// 单次探测的超时。三种协议串行发，最坏情况是这个数的三倍 ——
/// 这是对话框里点一下按钮还能忍受的上限。卡住的渠道该尽快报"无法判断"，
/// 而不是把界面挂在那里等。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 探测请求体。
///
/// **故意是个空对象。** 三种协议都要求 model（Anthropic 还要 messages、
/// Responses 要 input），`{}` 必然在参数校验阶段就被打回。于是这次探测既不花
/// token 也不占配额，而"路径存在、且把请求交给了参数校验"恰好就是我们要的信息。
/// 换成一条最小的真实请求，点一次检测就等于花一次钱。
const PROBE_BODY: &str = "{}";

/// 协议自动检测的入参。
///
/// 不复用 `ProviderInput`：那个要求 name / kind / base_url 等一整套字段，而检测发生在保存
/// **之前** —— 用户往往先把地址与密钥填好、点一下检测、再去起名字。这里只要"怎么连"。
#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolDetectInput {
    /// 编辑已有渠道时传它的 id。
    ///
    /// 存在的唯一理由是密钥：表单不回显密钥（`api_key` 留空的语义是"不改动"），
    /// 用户不重敲密钥就点检测时，请求会**不带鉴权头**发出去，三种协议一律 401，
    /// 结论全成"未知" —— 看着像检测功能坏了。带上 id 就能沿用库里存的那把。
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_bearer")]
    pub auth_style: AuthStyle,
    #[serde(default)]
    pub extra_headers: IndexMap<String, String>,
    #[serde(default)]
    pub proxy: ChannelProxy,
    /// 每种协议要试的路径，`path` 留空表示用协议默认路径。
    ///
    /// 必须把用户填的覆盖路径带上：DeepSeek 的 Anthropic 入口在
    /// `/anthropic/v1/messages`，只按默认路径试会把它判成"没有这个入口"，
    /// 而它其实是支持的。漏报不致命（退回协议转换），但这次检测就白做了。
    #[serde(default)]
    pub paths: Vec<ProtocolEndpoint>,
}

fn default_bearer() -> AuthStyle {
    AuthStyle::Bearer
}

/// 一种协议的判定结果。
///
/// 三态，且用词与能力探测（`storage::capabilities::CapabilityVerdict`）一致 ——
/// 同一个界面上"未知"不该有两种叫法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolVerdict {
    /// 上游认得这个路径。
    Supported,
    /// 上游明确说没有这个路径。
    Unsupported,
    /// 有响应，但说明不了问题（鉴权失败、限流、上游异常、回包不是 JSON）。
    Inconclusive,
}

/// 一种协议的探测结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolDetection {
    pub protocol: Protocol,
    pub verdict: ProtocolVerdict,
    /// 实际打出去的地址。上游报错时第一个要看的就是它。
    pub url: String,
    pub status: Option<u16>,
    pub latency_ms: Option<i64>,
    /// 判定依据。`supported` 之外必须有它 —— 否则用户只知道"不行"，不知道改哪。
    pub note: Option<String>,
}

impl ProtocolDetectInput {
    /// 造一个**不落库**的渠道对象，只为复用 URL 拼接与鉴权头的唯一真源。
    ///
    /// 自己拼一遍 URL、自己塞一次鉴权头看着更省事，但那是第二份真源：
    /// `endpoint` 的 `/v1` 去重、`auth_header` 的三种鉴权方式都会在这里被抄错，
    /// 而抄错的后果恰好是这个功能要避免的 —— "检测说支持，真发请求 404 / 401"。
    ///
    /// `kind` 填什么都不影响结果：检测逐个协议调 `endpoint_for`，不走 `wire_for`。
    fn as_provider(&self) -> Provider {
        Provider {
            id: 0,
            tag: "protocol-detect".into(),
            name: "protocol-detect".into(),
            kind: ProviderKind::OpenAiChat,
            base_url: self.base_url.trim().to_string(),
            // 空串当"没填"：带一个 `Bearer ` 头出去会被上游当成密钥格式错误（401），
            // 于是所有协议都判成"未知"，白测一轮。
            api_key: self
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(str::to_string),
            auth_style: self.auth_style,
            protocols: self.paths.clone(),
            extra_headers: self.extra_headers.clone(),
            param_override: None,
            model_mapping: Default::default(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: PROBE_TIMEOUT.as_millis() as i64,
            proxy: self.proxy.clone(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

/// 上游是不是把错误塞在 200 的响应体里。
///
/// 只看**非 null** 的顶层 `error`：Responses 的成功响应里本来就有 `"error": null`
/// （那是它 schema 的一部分），把 null 当错误会让这条协议的检测永远出不了结论。
fn is_error_envelope(value: &serde_json::Value) -> bool {
    value
        .get("error")
        .map(|e| !e.is_null())
        .unwrap_or(false)
}

/// 从回包判定这条路径上有没有该协议的对话入口。
///
/// **取向是保守的：宁可漏报，不可误报。** 误报（说支持、实际 404）会让 Apilot
/// 把请求直接打到不存在的路径上，用户拿到的是硬错误；漏报只是退回协议转换 ——
/// 功能照旧，只损失一次直通。所以凡说明不了问题的响应一律记 `Inconclusive`，
/// 绝不顺手归到某一侧。
fn judge_protocol(status: u16, body: &[u8]) -> (ProtocolVerdict, Option<String>) {
    match status {
        // 2xx 也可能是"什么都收"的兜底页：不少中转对未知路径照样回 200（有时是 HTML）。
        // 直接认它，一次检测就会把三种协议全勾上，而其中两条路径根本不存在 ——
        // 这是最危险的一类误报，所以 2xx 必须再看一眼体像不像这个接口的回应。
        s if (200..300).contains(&s) => match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(v) if is_error_envelope(&v) => (
                ProtocolVerdict::Inconclusive,
                Some("上游用 200 回了一个错误对象，说明不了这条路径存在".into()),
            ),
            Ok(_) => (ProtocolVerdict::Supported, None),
            Err(_) => (
                ProtocolVerdict::Inconclusive,
                Some(format!(
                    "上游返回 {s} 但响应不是 JSON（{} 字节），更像网页兜底而不是接口",
                    body.len()
                )),
            ),
        },
        // 400 / 422：请求进到了参数校验 —— 路径存在，而且收的正是这个协议的形状。
        // 我们发的体是故意的空对象，被拒才是预期结果。
        400 | 422 => (ProtocolVerdict::Supported, None),
        // 405：路径存在但不接受 POST。Apilot 对这三个入口只会 POST，
        // 所以"不收 POST"对我们来说就是不可用 —— 算明确的不支持，不是存疑。
        405 => (
            ProtocolVerdict::Unsupported,
            Some("该路径不接受 POST，不是对话入口".into()),
        ),
        404 | 501 => (ProtocolVerdict::Unsupported, None),
        // 鉴权失败既可能发生在路由之前也可能之后，无法区分，别猜。
        401 | 403 => (
            ProtocolVerdict::Inconclusive,
            Some("鉴权失败，无法判断该路径是否存在（密钥填对了吗）".into()),
        ),
        429 => (
            ProtocolVerdict::Inconclusive,
            Some("触发限流，这次说明不了问题".into()),
        ),
        s if s >= 500 => (
            ProtocolVerdict::Inconclusive,
            Some(format!("上游异常（{s}）")),
        ),
        s => (
            ProtocolVerdict::Inconclusive,
            Some(format!("上游返回 {s}，无法判断")),
        ),
    }
}

/// 往一种协议的入口发一次探测。
async fn probe_protocol(
    shell: &Arc<AppShell>,
    provider: &Provider,
    protocol: Protocol,
) -> ProtocolDetection {
    let url = provider.endpoint_for(protocol);
    // 用该渠道自己的代理，结论才与真实转发一致。
    let client = shell.registry.client_for(&provider.proxy);

    // content-type 必须自己带（真实流量里这个头是客户端给的）：少了它绝大多数
    // 上游直接 400，而 400 在判定里恰恰是"支持"的证据 —— 于是所有路径都会被判成
    // 存在。协议专有头（anthropic-version 之类）刻意不加：那是客户端在真实请求里
    // 带的东西，缺了顶多换来一个 400，结论不变。
    let req = authed(
        client
            .post(&url)
            .header("content-type", "application/json")
            .body(PROBE_BODY),
        provider,
    );

    let started = Instant::now();
    match req.timeout(PROBE_TIMEOUT).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let latency_ms = started.elapsed().as_millis() as i64;
            // 读体失败不必另外报错：状态码已经拿到了，体只用来区分"200 但其实是
            // 错误页"。读不到就当空体走，2xx 那条会因此记成存疑 —— 保守的方向。
            let body = resp.bytes().await.unwrap_or_default();
            let (verdict, note) = judge_protocol(status, &body);
            ProtocolDetection {
                protocol,
                verdict,
                url,
                status: Some(status),
                latency_ms: Some(latency_ms),
                note,
            }
        }
        Err(e) => ProtocolDetection {
            protocol,
            verdict: ProtocolVerdict::Inconclusive,
            url,
            status: None,
            latency_ms: None,
            note: Some(describe_network_error(&e)),
        },
    }
}

/// 自动检测一个地址支持哪些协议。
///
/// 与 `test_provider` 的分工：那个打 `/v1/models`，只回答"这条渠道通不通"；
/// 这个逐个打三种对话入口，回答"它认不认 Anthropic / Chat / Responses"——
/// 而这正是直通与转换的分界（见 docs/PROTOCOL_MATRIX.md）。
///
/// 只检测，**不写库**：结论怎么用由用户决定。检测会误判（中转的兜底页、
/// 网关在路由之前就做鉴权），自动改配置等于把一个可能错的结论当成用户的决定。
#[tauri::command]
pub async fn detect_provider_protocols(
    shell: State<'_, Arc<AppShell>>,
    input: ProtocolDetectInput,
) -> AppResult<Vec<ProtocolDetection>> {
    if input.base_url.trim().is_empty() {
        return Err(AppError::msg("请先填写 base_url 再检测"));
    }

    let mut provider = input.as_provider();
    // 表单没给密钥时沿用库里存的（编辑已有渠道时就是这样，见 `ProtocolDetectInput::id`）。
    if provider.api_key.is_none() {
        if let Some(id) = input.id {
            if let Some(stored) = crate::storage::providers::get(&shell.db, id).await? {
                provider.api_key = stored.api_key;
            }
        }
    }

    // 串行发三个请求。并发对同一条渠道没有意义（耗时基本由首字节 RTT 决定），
    // 却更容易撞上上游的限流 —— 而限流会让结论变成"未知"，等于白测。
    let mut out = Vec::with_capacity(PROBE_PROTOCOLS.len());
    for protocol in PROBE_PROTOCOLS {
        out.push(probe_protocol(&shell, &provider, protocol).await);
    }
    Ok(out)
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

    // ---- 协议自动检测 ----

    fn judge(status: u16, body: &str) -> (ProtocolVerdict, Option<String>) {
        judge_protocol(status, body.as_bytes())
    }

    fn detect_input(base_url: &str, paths: Vec<ProtocolEndpoint>) -> ProtocolDetectInput {
        ProtocolDetectInput {
            id: None,
            base_url: base_url.into(),
            api_key: Some("sk-test".into()),
            auth_style: AuthStyle::Bearer,
            extra_headers: IndexMap::new(),
            proxy: Default::default(),
            paths,
        }
    }

    #[test]
    fn a_rejected_request_still_proves_the_endpoint_exists() {
        // 我们发的体是故意的空对象，被参数校验打回（400）正是预期结果 ——
        // "路径存在，且收的确实是这个协议"，这才叫支持。
        assert_eq!(
            judge(400, r#"{"error":{"message":"you must provide a model parameter"}}"#).0,
            ProtocolVerdict::Supported
        );
        assert_eq!(judge(422, "{}").0, ProtocolVerdict::Supported);
    }

    #[test]
    fn a_404_means_that_protocol_is_not_there() {
        assert_eq!(judge(404, "<html>404</html>").0, ProtocolVerdict::Unsupported);
        assert_eq!(judge(501, "{}").0, ProtocolVerdict::Unsupported);
    }

    #[test]
    fn an_auth_failure_is_inconclusive_rather_than_unsupported() {
        // 401 既可能是"密钥错"也可能是"路径不存在"。判成不支持，用户会把一条
        // 好渠道的直通能力白白扔掉（而且他完全不知道为什么）。
        assert_eq!(judge(401, "{}").0, ProtocolVerdict::Inconclusive);
        assert_eq!(judge(403, "{}").0, ProtocolVerdict::Inconclusive);
    }

    #[test]
    fn a_catch_all_200_that_is_not_json_is_not_evidence_of_anything() {
        // 不少中转对未知路径一律回 200 的网页。认它就会把三种协议全判成支持，
        // 其中两条路径根本不存在 —— 最危险的一类误报。
        let (verdict, note) = judge(200, "<!doctype html><html>hello</html>");
        assert_eq!(verdict, ProtocolVerdict::Inconclusive);
        assert!(note.unwrap().contains("不是 JSON"));
    }

    #[test]
    fn an_empty_200_body_is_inconclusive() {
        assert_eq!(judge(200, "").0, ProtocolVerdict::Inconclusive);
    }

    #[test]
    fn a_200_wrapping_an_error_object_is_inconclusive() {
        let (verdict, _) = judge(200, r#"{"error":{"message":"no such route"}}"#);
        assert_eq!(verdict, ProtocolVerdict::Inconclusive);
    }

    #[test]
    fn a_responses_success_with_a_null_error_field_is_still_supported() {
        // Responses 的成功响应里 `error` 是必填且为 null 的。把 null 当错误，
        // 会让这条协议的检测永远出不了结论。
        let body = r#"{"id":"r","error":null,"output":[],"status":"completed"}"#;
        assert_eq!(judge(200, body).0, ProtocolVerdict::Supported);
    }

    #[test]
    fn a_method_not_allowed_counts_as_unsupported() {
        // Apilot 对这三个入口只发 POST；"不收 POST"等于不可用。
        let (verdict, note) = judge(405, "{}");
        assert_eq!(verdict, ProtocolVerdict::Unsupported);
        assert!(note.unwrap().contains("POST"));
    }

    #[test]
    fn rate_limits_and_upstream_errors_are_inconclusive() {
        for s in [429, 500, 502, 503] {
            assert_eq!(judge(s, "{}").0, ProtocolVerdict::Inconclusive, "HTTP {s}");
        }
        // 其余 4xx 同样不猜：402 之类说明不了路径存不存在。
        assert_eq!(judge(402, "{}").0, ProtocolVerdict::Inconclusive);
    }

    #[test]
    fn the_draft_provider_reuses_the_shared_url_joiner() {
        // base_url 自带 /v1 时不能拼出 /v1/v1/... —— 预设里 OpenAI / Moonshot
        // 那种写法正是这么踩坑的。
        let p = detect_input("https://api.moonshot.cn/v1", Vec::new()).as_provider();
        assert_eq!(
            p.endpoint_for(Protocol::OpenAiChat),
            "https://api.moonshot.cn/v1/chat/completions"
        );

        // 覆盖路径原样拼接（DeepSeek 的 Anthropic 入口就在子路径下）。
        let p = detect_input(
            "https://api.deepseek.com",
            vec![ProtocolEndpoint {
                protocol: Protocol::AnthropicMessages,
                path: Some("/anthropic/v1/messages".into()),
            }],
        )
        .as_provider();
        assert_eq!(
            p.endpoint_for(Protocol::AnthropicMessages),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
    }

    #[test]
    fn a_blank_key_does_not_produce_an_auth_header() {
        // 前端把空串当"没填"。带一个 `Bearer ` 出去会被上游当成密钥格式错误（401），
        // 于是所有协议都判成"未知" —— 看着像检测坏了。
        let mut input = detect_input("https://api.anthropic.com", Vec::new());
        input.api_key = Some("   ".into());
        assert!(input.as_provider().auth_header().is_none());

        input.api_key = None;
        assert!(input.as_provider().auth_header().is_none());
    }

    #[test]
    fn the_probe_body_is_deliberately_invalid_for_all_three_protocols() {
        // 只要这个体有可能被上游接受，检测就会真花钱、占配额。
        let v: serde_json::Value = serde_json::from_str(PROBE_BODY).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.is_empty(), "不能带任何参数：带一个就可能被真的执行");
    }
}
