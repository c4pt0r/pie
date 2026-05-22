//! Source adapters for the runtime `NotificationHook` trait. Each module here wraps a
//! transport (MCP push, Cloudflare hub, cron, file-watch, ...) and pushes normalized
//! [`Trigger`](pie_agent_core::Trigger) envelopes into a shared sink.
//!
//! The runtime side of the trait + envelope lives in `pie_agent_core::harness`; supervising,
//! permission, and rule-matching are deferred to RFC 1 sub-PR 2 / RFC 4. Adapters are
//! intentionally written without reference to the supervisor so they can be unit-tested
//! against a synthetic `TriggerSink` (a `mpsc::unbounded_channel` receiver).
//!
//! The `#[allow(dead_code)]` below is intentional and temporary: nothing in `main.rs`
//! references this module yet because RFC 1 sub-PR 2 (which lands the supervisor that
//! registers hooks) has not landed. The module ships now so the per-method dedup /
//! replacement-policy contract from RFC 1 §4.2.3 has unit-test coverage independently of
//! the supervisor's API shape.

#![allow(dead_code, unused_imports)]

pub mod mcp_notification_hook;

pub use mcp_notification_hook::McpNotificationHook;
