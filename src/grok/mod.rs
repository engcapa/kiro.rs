//! Grok Build / xAI Responses API 适配层。
//!
//! 此模块与 Kiro 的 AWS event-stream 链路保持隔离：它维护自己的 xAI
//! OAuth/API-token 凭据池，并将 Anthropic Messages 请求转换为 xAI
//! Responses API 请求。这样 `/grok` 的故障不会影响已有的 `/` 接口。

pub mod admin;
pub mod converter;
pub mod credentials;
pub mod files;
pub mod handlers;
pub mod media;
pub mod model_catalog;
pub mod oauth;
pub mod provider;
mod router;
pub mod stream;
pub mod token_manager;

pub use router::create_router_with_provider;
