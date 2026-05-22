//! Opinionated assembly around the bare `Agent`. 1:1 mirror of
//! `packages/agent/src/harness/`. Add a new file here for any new harness concern (session
//! type, compaction variant, env adapter); keep the bare `Agent` in `../agent.rs` IO-free.

pub mod agent_harness;
pub mod compaction;
pub mod cost;
pub mod messages;
pub mod notification_hook;
pub mod permission;
pub mod prompt_templates;
pub mod session;
pub mod skills;
pub mod system_prompt;
pub mod trigger;
pub mod types;
pub mod utils;

#[cfg(feature = "native-env")]
pub mod env;
