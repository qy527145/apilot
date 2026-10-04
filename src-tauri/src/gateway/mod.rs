//! LLM 网关：反向代理服务器、协议入口与转发管线。

pub mod pipeline;
pub mod router;
pub mod server;
pub mod sse;
pub mod stream;

#[cfg(test)]
mod proxy_e2e;
