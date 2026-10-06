//! 协议层：中间表示（IR）与各协议的编解码器。
//!
//! 对外入口是 [`codec::CodecRegistry`]：任意两种协议之间的转换都经它调度。

pub mod anthropic;
pub mod codec;
pub mod dto;
pub mod inspect;
pub mod oai_chat;
pub mod oai_responses;
pub mod shared;
