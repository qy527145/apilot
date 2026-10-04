//! Anthropic Messages API 编解码器。

mod request;
mod response;
mod stream;

pub use stream::{AnthropicStreamDecoder, AnthropicStreamEncoder};

use super::codec::{Codec, ConvertError, StreamDecoder, StreamEncoder};
use super::dto::{Protocol, UnifiedRequest, UnifiedResponse, UnifiedUsage};

/// Claude Code 等客户端使用的 `/v1/messages` 协议。
pub struct AnthropicCodec;

impl Codec for AnthropicCodec {
    fn protocol(&self) -> Protocol {
        Protocol::AnthropicMessages
    }

    fn decode_request(&self, raw: &[u8]) -> Result<UnifiedRequest, ConvertError> {
        request::decode_request(raw)
    }

    fn encode_request(&self, req: &UnifiedRequest) -> Result<Vec<u8>, ConvertError> {
        request::encode_request(req)
    }

    fn decode_response(&self, raw: &[u8]) -> Result<(UnifiedResponse, UnifiedUsage), ConvertError> {
        response::decode_response(raw)
    }

    fn encode_response(
        &self,
        resp: &UnifiedResponse,
        usage: &UnifiedUsage,
    ) -> Result<Vec<u8>, ConvertError> {
        response::encode_response(resp, usage)
    }

    fn new_stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicStreamDecoder::new())
    }

    fn new_stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(AnthropicStreamEncoder::new())
    }
}
