//! Source adapters for the runtime `NotificationHook` trait. Each module here wraps a
//! transport (MCP push, cron, file-watch, ...) and pushes normalized
//! [`Trigger`](pie_agent_core::Trigger) envelopes into a shared sink.
//!
//! The runtime side of the trait + envelope lives in `pie_agent_core::harness`. The
//! supervisor that owns hook registration (driver + pump task pair) shipped in
//! `AgentHarness::register_notification_hook`; `mcp_loader` constructs one
//! `McpNotificationHook` per configured MCP server and registers it with the harness
//! after the harness is built. Adapters are written without reference to the supervisor
//! so they can be unit-tested against a synthetic `TriggerSink` (a
//! `mpsc::unbounded_channel` receiver).

pub mod cron;
pub mod dynamic;
pub mod mcp_notification_hook;

#[allow(unused_imports)]
pub use cron::{
    CronJob, CronNotificationHook, ListCronJobsTool, NewCronJobTool, RemoveCronJobTool,
    SetCronJobStateTool, cron_action_hook, cron_harness_listener, global_cron_registry,
};
#[allow(unused_imports)]
pub use dynamic::{
    DynamicTriggerCheckHook, ListTriggersTool, NewTriggerTool, RemoveTriggerTool,
    SetTriggerStateTool, before_trigger_action_hook, direct_inject_action_hook,
    fire_once_harness_listener, global_registry,
};
#[allow(unused_imports)]
pub use mcp_notification_hook::McpNotificationHook;
