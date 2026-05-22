//! `utils/` module index. Mirrors `packages/ai/src/utils/`.
//!
//! These helpers are dependency-light on purpose — every provider imports from here, so adding
//! a transitive SDK dep would balloon cold-start cost.

pub mod abort;
pub mod aws_eventstream;
pub mod diagnostics;
pub mod event_stream;
pub mod hash;
pub mod headers;
pub mod json_parse;
pub mod node_http_proxy;
pub mod oauth;
pub mod overflow;
pub mod retry;
pub mod sanitize_unicode;
pub mod sse;
pub mod typebox_helpers;
pub mod validation;
