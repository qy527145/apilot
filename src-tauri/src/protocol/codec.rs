//! 编解码器抽象与注册表。
//!
//! 每个协议实现一个 `Codec`，提供「字节 ↔ IR」的双向转换，外加一对流式编解码器。
//! 任意两种协议之间的转换都走 `decode(A) → encode(B)`，不需要两两配对实现。

use std::collections::HashMap;
use std::sync::Arc;

use super::dto::{Protocol, UnifiedDelta, UnifiedRequest, UnifiedResponse, UnifiedUsage};
use crate::gateway::sse::SseEvent;

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("{proto} 请求解析失败: {reason}")]
    DecodeRequest { proto: &'static str, reason: String },

    #[error("{proto} 响应解析失败: {reason}")]
    DecodeResponse { proto: &'static str, reason: String },

    #[error("{proto} 编码失败: {reason}")]
    Encode { proto: &'static str, reason: String },

    #[error("该协议不支持此操作: {0}")]
    Unsupported(String),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

impl ConvertError {
    pub fn decode_request(proto: Protocol, reason: impl std::fmt::Display) -> Self {
        Self::DecodeRequest {
            proto: proto.as_str(),
            reason: reason.to_string(),
        }
    }

    pub fn decode_response(proto: Protocol, reason: impl std::fmt::Display) -> Self {
        Self::DecodeResponse {
            proto: proto.as_str(),
            reason: reason.to_string(),
        }
    }

    pub fn encode(proto: Protocol, reason: impl std::fmt::Display) -> Self {
        Self::Encode {
            proto: proto.as_str(),
            reason: reason.to_string(),
        }
    }
}

/// 单一协议的编解码器。
pub trait Codec: Send + Sync {
    fn protocol(&self) -> Protocol;

    // --- 非流式 ---

    /// 入站请求字节 → IR。
    fn decode_request(&self, raw: &[u8]) -> Result<UnifiedRequest, ConvertError>;

    /// IR → 出站请求字节。
    fn encode_request(&self, req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError>;

    /// 上游响应字节 → IR + 归一用量。
    fn decode_response(&self, raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError>;

    /// IR → 返回给客户端的响应字节。
    fn encode_response(
        &self,
        resp: &UnifiedResponse,
        usage: &UnifiedUsage,
    ) -> Result<Vec<u8>, ConvertError>;

    // --- 流式 ---

    /// 上游 SSE → IR delta 的解码器。
    fn new_stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// IR delta → 下游 SSE 的编码器。
    fn new_stream_encoder(&self) -> Box<dyn StreamEncoder>;

    // --- 错误 ---

    /// 从错误响应体里提取人类可读的错误信息。默认实现尝试常见的错误结构。
    fn extract_error_message(&self, raw: &[u8]) -> String {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw) {
            for path in [
                &["error", "message"][..],
                &["message"][..],
                &["error"][..],
                &["detail"][..],
            ] {
                if let Some(s) = dig_str(&v, path) {
                    return s;
                }
            }
        }
        String::from_utf8_lossy(raw).chars().take(500).collect()
    }
}

fn dig_str(v: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(String::from)
}

/// 上游 SSE → IR delta。
pub trait StreamDecoder: Send {
    /// 处理一个 SSE 事件，产出 0..n 个 IR delta。
    fn on_event(&mut self, ev: &SseEvent) -> Result<Vec<UnifiedDelta>, ConvertError>;

    /// 上游流结束时的兜底：补齐 finish 事件与 usage。
    fn finish(&mut self) -> Vec<UnifiedDelta>;

    /// 累积至今的归一用量。
    fn usage(&self) -> UnifiedUsage;

    /// 拼接至今的最终文本，用于监控展示与缓存。
    fn text(&self) -> String {
        String::new()
    }

    /// 是否已见到上游的终止标记。
    fn is_finished(&self) -> bool {
        false
    }
}

/// IR delta → 下游 SSE。
pub trait StreamEncoder: Send {
    /// 把一个 IR delta 编成 0..n 个 SSE 事件。
    fn on_delta(&mut self, d: &UnifiedDelta) -> Vec<SseEvent>;

    /// 产出终止事件（`message_stop` / `[DONE]` / `response.completed`）。
    fn finish(&mut self) -> Vec<SseEvent>;
}

/// 协议编解码器注册表。
pub struct CodecRegistry {
    codecs: HashMap<Protocol, Arc<dyn Codec>>,
}

impl CodecRegistry {
    pub fn new() -> Self {
        let mut codecs: HashMap<Protocol, Arc<dyn Codec>> = HashMap::new();
        codecs.insert(
            Protocol::AnthropicMessages,
            Arc::new(super::anthropic::AnthropicCodec),
        );
        codecs.insert(Protocol::OpenAiChat, Arc::new(super::oai_chat::OpenAiChatCodec));
        codecs.insert(
            Protocol::OpenAiResponses,
            Arc::new(super::oai_responses::OpenAiResponsesCodec),
        );
        Self { codecs }
    }

    /// 取指定协议的 codec。三种协议都已注册，`expect` 不会触发。
    pub fn codec(&self, p: Protocol) -> Arc<dyn Codec> {
        self.codecs
            .get(&p)
            .cloned()
            .unwrap_or_else(|| panic!("协议 {p} 未注册 codec"))
    }

    /// 是否已注册该协议。
    pub fn has(&self, p: Protocol) -> bool {
        self.codecs.contains_key(&p)
    }

    /// 把 `from` 协议的响应字节转成 `to` 协议的响应字节，并返回归一用量。
    pub fn convert_response(
        &self,
        from: Protocol,
        to: Protocol,
        raw: &[u8],
    ) -> Result<(Vec<u8>, UnifiedUsage), ConvertError> {
        let (resp, usage) = self.codec(from).decode_response(raw)?;
        if from == to {
            return Ok((raw.to_vec(), usage));
        }
        let out = self.codec(to).encode_response(&resp, &usage)?;
        Ok((out, usage))
    }

    /// 把 `from` 协议的请求字节转成 `to` 协议的请求字节。
    pub fn convert_request(
        &self,
        from: Protocol,
        to: Protocol,
        raw: &[u8],
    ) -> Result<Vec<u8>, ConvertError> {
        if from == to {
            return Ok(raw.to_vec());
        }
        let req = self.codec(from).decode_request(raw)?;
        self.codec(to).encode_request(&req)
    }
}

impl Default for CodecRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all_three_protocols() {
        let r = CodecRegistry::new();
        assert!(r.has(Protocol::AnthropicMessages));
        assert!(r.has(Protocol::OpenAiChat));
        assert!(r.has(Protocol::OpenAiResponses));
    }

    #[test]
    fn codec_reports_its_own_protocol() {
        let r = CodecRegistry::new();
        for p in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiChat,
            Protocol::OpenAiResponses,
        ] {
            assert_eq!(r.codec(p).protocol(), p);
        }
    }

    #[test]
    fn same_protocol_request_conversion_is_passthrough() {
        let r = CodecRegistry::new();
        let raw = br#"{"model":"x","messages":[]}"#;
        let out = r
            .convert_request(Protocol::OpenAiChat, Protocol::OpenAiChat, raw)
            .unwrap();
        // 同协议不做往返，避免无谓的字段丢失
        assert_eq!(out, raw);
    }

    #[test]
    fn codex_responses_lite_tools_survive_conversion_to_chat() {
        // 这条链路是真实的 Codex 场景：客户端说 Responses，渠道是 DeepSeek 这类
        // 只说 Chat Completions 的服务商。工具藏在 additional_tools 条目里，
        // 一旦在解码阶段丢掉，上游收到的请求就没有 tools，Codex 直接瘫掉。
        let r = CodecRegistry::new();
        let raw = r#"{"model":"gpt-5.6-sol","tools":null,"input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"function","name":"shell","parameters":{"type":"object"}}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"list files"}]}
        ]}"#;

        let out = r
            .convert_request(
                Protocol::OpenAiResponses,
                Protocol::OpenAiChat,
                raw.as_bytes(),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        let tools = v["tools"].as_array().expect("转换后必须带上 tools");
        assert_eq!(tools[0]["function"]["name"], "shell");
        assert_eq!(v["messages"][0]["content"][0]["text"], "list files");
    }

    #[test]
    fn namespace_tools_reach_a_chat_upstream_under_their_bare_names() {
        // Codex 真实报文里工具全在 namespace 里（functions / clock / collaboration）。
        // Chat Completions 没有分组的概念，只能按裸名平铺 —— 带点的 `functions.exec`
        // 会被上游按函数名格式直接拒掉。组名因此只能留在 IR 上给监控看。
        let r = CodecRegistry::new();
        let raw = r#"{"model":"gpt-6","tools":null,"input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"namespace","name":"functions","tools":[
                    {"type":"function","name":"exec","parameters":{"type":"object"}},
                    {"type":"function","name":"wait","parameters":{"type":"object"}}]}]},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}
        ]}"#;

        let out = r
            .convert_request(
                Protocol::OpenAiResponses,
                Protocol::OpenAiChat,
                raw.as_bytes(),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["exec", "wait"]);
        assert!(
            !out.windows(9).any(|w| w == b"namespace"),
            "namespace 是 IR 上的展示字段，不该被编码出去"
        );
    }

    #[test]
    fn extract_error_message_handles_common_shapes() {
        let r = CodecRegistry::new();
        let c = r.codec(Protocol::AnthropicMessages);
        assert_eq!(
            c.extract_error_message(br#"{"error":{"message":"boom"}}"#),
            "boom"
        );
        assert_eq!(c.extract_error_message(br#"{"message":"plain"}"#), "plain");
        assert_eq!(
            c.extract_error_message(br#"{"detail":"d"}"#),
            "d"
        );
        // 非 JSON 时退化为原文
        assert_eq!(c.extract_error_message(b"not json"), "not json");
    }
}
