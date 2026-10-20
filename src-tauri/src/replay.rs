//! 请求回放：把一条捕获过的请求重发到指定渠道 / 模型，供 A/B 对比。
//!
//! 设计约束：
//! - 走 `upstream::oneshot`，不经过管线 —— 回放是用户主动的对比实验，
//!   **不写日志、不计费、不查缓存**，不能污染真实用量；
//! - 一律按非流式发，拿到完整回答才能与原始响应并排比较；
//! - 目标模型按渠道的 `model_mapping` 映射后再发；线协议按 `wire_for(入站协议)`
//!   选，与真实路由同口径 —— 否则对比的就不是"同一条请求换个渠道"了。

use std::sync::Arc;

use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::protocol::dto::{ContentBlock, Protocol, UnifiedUsage};
use crate::shell::AppShell;
use crate::storage::models::Provider;

#[derive(Debug, Clone, Serialize)]
pub struct ReplayOutcome {
    pub ok: bool,
    pub request_id: String,
    pub provider_tag: String,
    pub provider_name: String,
    /// 入站模型名（客户端视角）。
    pub model: String,
    /// 渠道映射后真正发出去的名字。
    pub upstream_model: String,
    pub wire: Protocol,
    pub url: String,
    pub status: Option<u16>,
    pub latency_ms: i64,
    pub text: String,
    /// 解出来的完整内容块（含思考 / 工具调用）。
    pub content: Vec<ContentBlock>,
    pub usage: Option<UnifiedUsage>,
    pub error: Option<String>,
}

impl ReplayOutcome {
    /// 请求还没发出去就失败（比如请求体解不出来）时的骨架。
    ///
    /// 仍然把"本来要发到哪"填进去：用户排错时第一眼要看的正是目标渠道 / 模型 /
    /// 线协议，只回一句错误说明会让界面空得像什么都没选。
    fn failed(
        request_id: &str,
        provider: &Provider,
        model: &str,
        upstream_model: &str,
        wire: Protocol,
        error: String,
    ) -> Self {
        Self {
            ok: false,
            request_id: request_id.to_string(),
            provider_tag: provider.tag.clone(),
            provider_name: provider.name.clone(),
            model: model.to_string(),
            upstream_model: upstream_model.to_string(),
            wire,
            url: String::new(),
            status: None,
            latency_ms: 0,
            text: String::new(),
            content: Vec::new(),
            usage: None,
            error: Some(error),
        }
    }
}

pub async fn replay(
    shell: &Arc<AppShell>,
    request_id: &str,
    provider_tag: Option<String>,
    model: Option<String>,
) -> AppResult<ReplayOutcome> {
    // get 拿元数据（协议 / 路径 / 生效模型 / 渠道），get_detail 拿捕获的请求体。
    // 分两次查是因为 get_detail 还要读几 MB 的捕获 BLOB，而元数据本身始终可读。
    let Some(log) = crate::storage::logs::get(&shell.db, request_id).await? else {
        return Err(AppError::msg("找不到这条请求"));
    };
    let Some(detail) = crate::storage::logs::get_detail(&shell.db, request_id).await? else {
        // 两次查询之间被后台裁剪删掉了，对用户来说与"记录不存在"没有区别。
        return Err(AppError::msg("找不到这条请求"));
    };
    let Some(body) = detail.request_body else {
        return Err(AppError::msg(
            "这条请求没有捕获到请求体（可能已被裁剪），无法回放",
        ));
    };

    let protocol_in = inbound_protocol(&log.protocol_in, &log.path)?;
    let target_provider = target_provider(provider_tag.as_deref(), log.provider_tag.as_deref())?;
    let target_model = target_model(model.as_deref(), &log.model)?;

    let Some(outbound) = shell.registry.get(&target_provider) else {
        return Err(AppError::msg(format!(
            "渠道 {target_provider} 不存在或已停用"
        )));
    };
    let provider = outbound.provider().clone();

    // 管线在发请求前也会做这一步，回放必须照做，否则拿"客户端视角的模型名"
    // 去请求上游，映射过的渠道会直接 404。
    let upstream_model = provider.upstream_model(&target_model).to_string();
    let wire = provider.wire_for(protocol_in);

    // 解不出来不算调用方错误：请求确实存在，只是报文不合协议。把它放进 outcome
    // 的 error 里，让界面照常显示目标信息。
    let mut req = match shell.codecs.codec(protocol_in).decode_request(body.as_bytes()) {
        Ok(r) => r,
        Err(e) => {
            return Ok(ReplayOutcome::failed(
                request_id,
                &provider,
                &target_model,
                &upstream_model,
                wire,
                format!("请求体解不出来（{e}），无法回放"),
            ))
        }
    };
    req.model = upstream_model.clone();
    // 回放要拿到完整回答才能并排比较；流式会让调用方拿到增量，也更容易碰上
    // 上游中途断开。这里改的是 IR，编码出去时 `stream` 自然就是 false。
    req.stream = false;

    let outcome = crate::upstream::oneshot::send_on(shell, &provider, &req, wire).await;

    Ok(ReplayOutcome {
        ok: outcome.ok,
        request_id: request_id.to_string(),
        provider_tag: provider.tag.clone(),
        provider_name: provider.name.clone(),
        model: target_model,
        upstream_model,
        wire: outcome.wire,
        url: outcome.url,
        status: outcome.status,
        latency_ms: outcome.latency_ms,
        text: outcome.text,
        content: outcome.decoded.map(|r| r.content).unwrap_or_default(),
        usage: outcome.usage,
        error: outcome.error,
    })
}

/// 日志里记的入站协议优先；记不出东西时（老数据 / 手工写入）按路径兜底。
///
/// 两者都认不出来才报错 —— 猜错协议会把请求体按错误的形状解，得到的错误
/// 跟真实原因（服务商 / 路径不对）毫无关系，不如直接说"认不出"。
fn inbound_protocol(logged: &str, path: &str) -> AppResult<Protocol> {
    Protocol::parse(logged.trim())
        .or_else(|| crate::gateway::router::protocol_family(path))
        .ok_or_else(|| {
            AppError::msg(format!(
                "认不出这条请求的入站协议（日志记的是「{logged}」，路径是「{path}」）"
            ))
        })
}

/// 目标渠道：显式指定的优先，否则沿用日志里那条。
fn target_provider(requested: Option<&str>, logged: Option<&str>) -> AppResult<String> {
    requested
        .and_then(non_empty)
        .or_else(|| logged.and_then(non_empty))
        .map(str::to_string)
        .ok_or_else(|| AppError::msg("这条请求没有记录渠道，请指定要回放到的渠道"))
}

/// 目标模型（入站视角的名字）：显式指定的优先，否则用日志里的生效模型。
fn target_model(requested: Option<&str>, logged: &str) -> AppResult<String> {
    requested
        .and_then(non_empty)
        .or_else(|| non_empty(logged))
        .map(str::to_string)
        .ok_or_else(|| AppError::msg("这条请求没有记录模型，请指定要回放的模型"))
}

/// 前端可能把清空后的输入框原样传成空串，那不是"指定了目标"。
fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_provider_and_model_win() {
        assert_eq!(target_provider(Some("backup"), Some("main")).unwrap(), "backup");
        assert_eq!(target_model(Some("gpt-5"), "claude-sonnet-5").unwrap(), "gpt-5");
    }

    #[test]
    fn blank_overrides_fall_back_to_the_logged_values() {
        // 界面上没选就是空串，等同于"沿用原值"，不能被当成模型名发出去。
        assert_eq!(target_provider(Some("  "), Some("main")).unwrap(), "main");
        assert_eq!(target_model(Some(""), "claude-sonnet-5").unwrap(), "claude-sonnet-5");
    }

    #[test]
    fn targets_are_trimmed() {
        assert_eq!(target_provider(Some(" backup "), None).unwrap(), "backup");
        assert_eq!(target_model(Some(" gpt-5 "), "").unwrap(), "gpt-5");
    }

    #[test]
    fn no_provider_anywhere_is_a_caller_error() {
        let e = target_provider(None, None).unwrap_err();
        assert!(e.to_string().contains("渠道"));
        assert!(target_provider(Some(""), Some("  ")).is_err());
    }

    #[test]
    fn no_model_anywhere_is_a_caller_error() {
        // 生效模型是空串说明日志本身就不完整，不能拿空名字去请求上游。
        assert!(target_model(None, "").is_err());
        assert!(target_model(Some(" "), "  ").is_err());
    }

    #[test]
    fn inbound_protocol_prefers_the_logged_column() {
        assert_eq!(
            inbound_protocol("openai_chat", "/v1/messages").unwrap(),
            Protocol::OpenAiChat,
            "日志列是真实解码时用的协议，路径只是兜底"
        );
        // 日志里可能带空白（手工修过的库），别因此掉到路径兜底。
        assert_eq!(
            inbound_protocol(" anthropic ", "/v1/responses").unwrap(),
            Protocol::AnthropicMessages
        );
    }

    #[test]
    fn inbound_protocol_falls_back_to_the_path() {
        assert_eq!(
            inbound_protocol("", "/v1/messages").unwrap(),
            Protocol::AnthropicMessages
        );
        assert_eq!(
            inbound_protocol("unknown", "/v1/chat/completions").unwrap(),
            Protocol::OpenAiChat
        );
        assert_eq!(
            inbound_protocol("unknown", "/v1/responses").unwrap(),
            Protocol::OpenAiResponses
        );
    }

    #[test]
    fn inbound_protocol_errors_when_both_are_unknown() {
        let e = inbound_protocol("unknown", "/v1/embeddings").unwrap_err();
        assert!(e.to_string().contains("入站协议"));
    }

    #[test]
    fn a_failed_outcome_still_carries_the_target() {
        // 解不出请求体时界面仍要显示"本来要发到哪"，否则看起来像用户没选目标。
        let provider = Provider {
            id: 1,
            tag: "backup".into(),
            name: "备用渠道".into(),
            kind: crate::storage::models::ProviderKind::OpenAiChat,
            base_url: "https://api.example.com".into(),
            api_key: None,
            auth_style: crate::storage::models::AuthStyle::Bearer,
            protocols: Vec::new(),
            extra_headers: Default::default(),
            param_override: None,
            model_mapping: Default::default(),
            weight: 0,
            priority: 0,
            enabled: true,
            timeout_ms: 60_000,
            proxy: Default::default(),
            metadata_source: Default::default(),
            created_at: 0,
            updated_at: 0,
        };

        let o = ReplayOutcome::failed(
            "r1",
            &provider,
            "gpt-5",
            "gpt-5-upstream",
            Protocol::OpenAiChat,
            "坏了".into(),
        );
        assert!(!o.ok);
        assert_eq!(o.provider_tag, "backup");
        assert_eq!(o.model, "gpt-5");
        assert_eq!(o.upstream_model, "gpt-5-upstream");
        assert_eq!(o.wire, Protocol::OpenAiChat);
        assert_eq!(o.error.as_deref(), Some("坏了"));
        assert!(o.content.is_empty() && o.usage.is_none());
    }
}
