//! 把捕获到的报文解成 IR，供监控页做语义化展示。
//!
//! 这里刻意**不造视图模型**：`UnifiedRequest` / `UnifiedResponse` 已经是协议无关的，
//! 且字段齐备（系统提示词、消息、工具、参数、思考块、工具调用及其入参）。捕获的报文
//! 用对应协议的 codec 解一次就得到它 —— 三种协议、甚至跨协议转换过的报文，解出来
//! 都是同一套结构，前端因此只需要写一份渲染逻辑。
//!
//! 解码失败只填 `error`，不抛错：报文可能是被截断的流、上游返回的错误体、或压根
//! 不是这个协议的形状，这些都该让界面退回「格式化 / 原始」视图，而不是让详情打不开。

use serde::Serialize;

use super::codec::CodecRegistry;
use super::dto::{Protocol, UnifiedRequest, UnifiedResponse, UnifiedUsage};
use crate::storage::logs::RequestDetail;

#[derive(Debug, Clone, Serialize)]
pub struct DecodedRequest {
    /// 用哪个协议解的，用于在界面上说明"这份报文说的是什么协议"。
    pub protocol: String,
    pub value: Option<UnifiedRequest>,
    /// 解不出来时的原因，直接展示给用户。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedResponse {
    pub protocol: String,
    pub value: Option<UnifiedResponse>,
    /// 上游自报的用量。与日志行里的结算用量可能不同，两个都留着便于对账。
    pub usage: Option<UnifiedUsage>,
    pub error: Option<String>,
}

/// 一次请求在四个方向上的语义化视图。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DetailViews {
    /// 客户端 → Apilot，用入站协议解。
    pub inbound_request: Option<DecodedRequest>,
    /// Apilot → 上游，用出站协议解。转换过的报文在这里才看得出转换做了什么。
    pub upstream_request: Option<DecodedRequest>,
    /// 上游 → Apilot。
    pub upstream_response: Option<DecodedResponse>,
    /// Apilot → 客户端。
    pub client_response: Option<DecodedResponse>,
    /// 流式响应：上游发的是 SSE，解不出单个响应，用增量重建出的内容代替。
    pub streamed_response: Option<DecodedResponse>,
}

/// 从日志里存的短名解析回协议；认不出返回 `None`（老数据或外部写入的脏值）。
fn parse_protocol(s: &str) -> Option<Protocol> {
    match s {
        "anthropic" => Some(Protocol::AnthropicMessages),
        "openai_chat" => Some(Protocol::OpenAiChat),
        "openai_responses" => Some(Protocol::OpenAiResponses),
        _ => None,
    }
}

/// 解析详情里所有能解析的报文。
///
/// 只在打开详情时调用，不进请求热路径 —— 一次最多解四份报文，其中请求体可能上百 KB。
pub fn inspect(codecs: &CodecRegistry, d: &RequestDetail) -> DetailViews {
    let protocol_in = parse_protocol(&d.protocol_in);
    let protocol_out = parse_protocol(&d.protocol_out);

    DetailViews {
        inbound_request: protocol_in.and_then(|p| {
            decode_request(codecs, p, d.request_body.as_deref())
        }),
        upstream_request: protocol_out.and_then(|p| {
            decode_request(codecs, p, d.upstream_body.as_deref())
        }),
        upstream_response: protocol_out.and_then(|p| {
            decode_response(codecs, p, d.upstream_response_body.as_deref())
        }),
        client_response: protocol_in.and_then(|p| {
            decode_response(codecs, p, d.response_body.as_deref())
        }),
        streamed_response: decode_streamed(d),
    }
}

fn decode_request(
    codecs: &CodecRegistry,
    protocol: Protocol,
    body: Option<&str>,
) -> Option<DecodedRequest> {
    let body = body?;
    if !codecs.has(protocol) {
        return None;
    }

    // 捕获里的报文都是 JSON / SSE，本来就是 UTF-8；`get_detail` 的
    // `from_utf8_lossy` 不会在这里丢信息，所以没必要为了拿字节去改存储层的返回类型。
    match codecs.codec(protocol).decode_request(body.as_bytes()) {
        Ok(value) => Some(DecodedRequest {
            protocol: protocol.as_str().to_string(),
            value: Some(value),
            error: None,
        }),
        Err(e) => Some(DecodedRequest {
            protocol: protocol.as_str().to_string(),
            value: None,
            error: Some(e.to_string()),
        }),
    }
}

fn decode_response(
    codecs: &CodecRegistry,
    protocol: Protocol,
    body: Option<&str>,
) -> Option<DecodedResponse> {
    let body = body?;
    if !codecs.has(protocol) {
        return None;
    }

    match codecs.codec(protocol).decode_response(body.as_bytes()) {
        Ok((value, usage)) => Some(DecodedResponse {
            protocol: protocol.as_str().to_string(),
            value: Some(value),
            usage: Some(usage),
            error: None,
        }),
        Err(e) => Some(DecodedResponse {
            protocol: protocol.as_str().to_string(),
            value: None,
            usage: None,
            error: Some(e.to_string()),
        }),
    }
}

/// 流式响应：从落库的 `response_content` 还原。
///
/// 它由 `ContentAccumulator` 在请求过程中重建，里面同时有文本、思考与工具调用
/// （入参已由分片拼好并解析）—— 这是流式请求唯一能拿到"模型调了什么工具"的来源。
///
/// 用量取自日志行而不是这份内容：日志行里的数字是**结算口径**，与计费页一致；
/// 拿流里解析出来的量会让同一个请求在两个页面显示不同的 token 数。
fn decode_streamed(d: &RequestDetail) -> Option<DecodedResponse> {
    let raw = d.response_content.as_deref()?;
    let value: UnifiedResponse = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return Some(DecodedResponse {
                protocol: d.protocol_in.clone(),
                value: None,
                usage: None,
                error: Some(format!("流式响应的结构化内容无法解析: {e}")),
            })
        }
    };

    Some(DecodedResponse {
        protocol: d.protocol_in.clone(),
        value: Some(value),
        usage: Some(UnifiedUsage {
            input_tokens: d.input_tokens,
            output_tokens: d.output_tokens,
            cache_read_tokens: d.cache_read_tokens,
            cache_creation_tokens: d.cache_creation_tokens,
            reasoning_tokens: 0,
            total_tokens: d.input_tokens + d.output_tokens,
            source: crate::protocol::dto::UsageSource::Upstream,
            raw: None,
        }),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::stream::ContentAccumulator;
    use crate::protocol::dto::{ContentBlock, FinishReason, UnifiedDelta};

    fn detail() -> RequestDetail {
        RequestDetail {
            request_id: "r1".into(),
            ts: 0,
            client: "codex".into(),
            protocol_in: "openai_responses".into(),
            protocol_out: "openai_chat".into(),
            provider_tag: Some("p".into()),
            model: "m".into(),
            request_model: "m".into(),
            is_stream: false,
            status_code: 200,
            error_message: None,
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 30,
            cache_creation_tokens: 5,
            usage_source: "upstream".into(),
            quota: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            ttfb_ms: None,
            cache_hit: false,
            saved_quota: 0,
            method: "POST".into(),
            path: "/v1/responses".into(),
            request_headers: serde_json::json!({}),
            request_body: None,
            upstream_url: None,
            upstream_model: None,
            upstream_status: None,
            upstream_headers: serde_json::json!({}),
            upstream_body: None,
            upstream_response_headers: serde_json::json!({}),
            upstream_response_body: None,
            response_headers: serde_json::json!({}),
            response_body: None,
            stream_text: None,
            stream_events: 0,
            response_content: None,
            upstream_stream_raw: None,
            client_stream_raw: None,
            stream_raw_truncated: false,
            stream_timings: None,
        }
    }

    #[test]
    fn anthropic_request_decodes_system_messages_and_tools() {
        let mut d = detail();
        d.protocol_in = "anthropic".into();
        d.request_body = Some(
            serde_json::json!({
                "model": "claude-sonnet-5",
                "max_tokens": 1024,
                "system": "你是助手",
                "messages": [
                    {"role": "user", "content": "看看目录"},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
                    ]}
                ],
                "tools": [{
                    "name": "Bash",
                    "description": "跑命令",
                    "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
                }]
            })
            .to_string(),
        );

        let views = inspect(&CodecRegistry::new(), &d);
        let req = views.inbound_request.unwrap();
        assert!(req.error.is_none(), "{:?}", req.error);
        let req = req.value.unwrap();

        assert_eq!(req.system.len(), 1);
        assert_eq!(req.system[0].as_text(), Some("你是助手"));
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.max_tokens, Some(1024));
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "Bash");
        // Schema 原样保留 —— 界面上要展示的就是它。
        assert_eq!(req.tools[0].input_schema["properties"]["cmd"]["type"], "string");
        // 工具调用要能看出名字与已解析的入参。
        assert!(matches!(
            req.messages[1].content[0],
            ContentBlock::ToolUse { ref name, ref input, .. }
                if name == "Bash" && input["cmd"] == "ls"
        ));
    }

    #[test]
    fn chat_request_decodes_to_the_same_shape_as_anthropic() {
        // 三种协议解出同一套结构，前端才只需要一份渲染逻辑。
        let mut d = detail();
        d.protocol_in = "openai_chat".into();
        d.request_body = Some(
            serde_json::json!({
                "model": "gpt-4o",
                "temperature": 0.2,
                "messages": [
                    {"role": "system", "content": "你是助手"},
                    {"role": "user", "content": "看看目录"}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "Bash", "description": "跑命令",
                    "parameters": {"type": "object"}
                }}]
            })
            .to_string(),
        );

        let views = inspect(&CodecRegistry::new(), &d);
        let req = views.inbound_request.unwrap().value.unwrap();
        assert_eq!(req.system[0].as_text(), Some("你是助手"));
        assert_eq!(req.messages.len(), 1, "system 应被提到顶层，不留在 messages 里");
        assert_eq!(req.tools[0].name, "Bash");
        assert_eq!(req.temperature, Some(0.2));
    }

    #[test]
    fn malformed_json_only_sets_error() {
        let mut d = detail();
        d.request_body = Some("这不是 JSON".into());

        let views = inspect(&CodecRegistry::new(), &d);
        let decoded = views.inbound_request.unwrap();
        assert!(decoded.value.is_none());
        assert!(decoded.error.is_some(), "解不出来必须给出原因");
    }

    #[test]
    fn valid_json_of_the_wrong_protocol_only_sets_error() {
        // 形状对不上协议时也要走 error 分支，而不是解出一个字段全空的假对象 ——
        // 那会让界面显示"这个请求什么都没问"，比报错更误导。
        let mut d = detail();
        d.protocol_in = "anthropic".into();
        d.request_body = Some(serde_json::json!({"hello": "world"}).to_string());

        let views = inspect(&CodecRegistry::new(), &d);
        let decoded = views.inbound_request.unwrap();
        assert!(decoded.error.is_some());
    }

    #[test]
    fn missing_bodies_yield_no_view_rather_than_empty_ones() {
        // 没开捕获时不该伪造出空视图，界面据此显示"未捕获"。
        let views = inspect(&CodecRegistry::new(), &detail());
        assert!(views.inbound_request.is_none());
        assert!(views.upstream_request.is_none());
        assert!(views.upstream_response.is_none());
        assert!(views.client_response.is_none());
        assert!(views.streamed_response.is_none());
    }

    #[test]
    fn unknown_protocol_name_is_skipped() {
        let mut d = detail();
        d.protocol_in = "gemini".into();
        d.request_body = Some("{}".to_string());

        let views = inspect(&CodecRegistry::new(), &d);
        assert!(views.inbound_request.is_none());
    }

    #[test]
    fn chat_response_decodes_content_and_usage() {
        let mut d = detail();
        d.protocol_out = "openai_chat".into();
        d.upstream_response_body = Some(
            serde_json::json!({
                "id": "c1",
                "model": "deepseek-chat",
                "choices": [{
                    "message": {"role": "assistant", "content": "你好"},
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 1000,
                    "completion_tokens": 20,
                    "prompt_tokens_details": {"cached_tokens": 800}
                }
            })
            .to_string(),
        );

        let views = inspect(&CodecRegistry::new(), &d);
        let resp = views.upstream_response.unwrap();
        assert!(resp.error.is_none(), "{:?}", resp.error);

        let value = resp.value.unwrap();
        assert_eq!(value.content[0].as_text(), Some("你好"));
        assert_eq!(value.finish_reason, FinishReason::Stop);

        // 用量按统一口径：input 是不含缓存的 fresh 输入。
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 200, "1000 - 800");
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.output_tokens, 20);
    }

    #[test]
    fn streamed_response_is_rebuilt_from_response_content() {
        let mut d = detail();
        d.is_stream = true;
        d.response_content = Some(
            serde_json::json!({
                "id": "r1",
                "model": "m",
                "content": [
                    {"type": "thinking", "text": "先看看"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"cmd": "ls"}}
                ],
                "finish_reason": {"type": "tool_use"}
            })
            .to_string(),
        );

        let views = inspect(&CodecRegistry::new(), &d);
        let s = views.streamed_response.unwrap();
        assert!(s.error.is_none(), "{:?}", s.error);
        let value = s.value.unwrap();

        assert_eq!(value.content.len(), 2);
        assert!(matches!(value.content[0], ContentBlock::Thinking { .. }));
        assert!(matches!(
            value.content[1],
            ContentBlock::ToolUse { ref name, .. } if name == "Bash"
        ));
        assert_eq!(value.finish_reason, FinishReason::ToolUse);

        // 用量取日志行的结算口径，与计费页保持一致。
        let usage = s.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 30);
    }

    #[test]
    fn streamed_response_from_a_real_accumulator_keeps_tool_input() {
        // 与真实链路一致：内容块由 ContentAccumulator 从增量重建，
        // 工具入参是分片 JSON 拼好后再解析的。
        let mut acc = ContentAccumulator::new();
        acc.apply(&UnifiedDelta::BlockStart {
            index: 0,
            block: ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            },
        });
        acc.apply(&UnifiedDelta::ToolInputDelta {
            index: 0,
            partial_json: "{\"cmd\":\"ls".into(),
        });
        acc.apply(&UnifiedDelta::ToolInputDelta {
            index: 0,
            partial_json: " -la\"}".into(),
        });

        let mut d = detail();
        d.is_stream = true;
        d.response_content = Some(
            serde_json::to_string(&UnifiedResponse {
                id: "r1".into(),
                model: "m".into(),
                content: acc.finish(),
                finish_reason: FinishReason::ToolUse,
            })
            .unwrap(),
        );

        let views = inspect(&CodecRegistry::new(), &d);
        let value = views.streamed_response.unwrap().value.unwrap();
        assert!(
            matches!(
                value.content[0],
                ContentBlock::ToolUse { ref input, .. } if input["cmd"] == "ls -la"
            ),
            "分片入参必须拼回完整 JSON: {:?}",
            value.content[0]
        );
    }

    #[test]
    fn corrupt_streamed_content_reports_error_without_panicking() {
        let mut d = detail();
        d.response_content = Some("{ 坏掉的 JSON".into());
        let views = inspect(&CodecRegistry::new(), &d);
        assert!(views.streamed_response.unwrap().error.is_some());
    }
}
