//! Built-in tools. Modeled on `packages/coding-agent/src/core/tools/` (the TS implementation):
//! same names, same parameter shapes, simpler bodies. Each tool implements
//! [`pie_agent_core::AgentTool`].

pub mod bash;
pub mod edit;
pub mod find;
pub mod git;
pub mod grep;
pub mod ls;
pub mod mcp_adapter;
pub mod memory;
pub mod read;
pub mod skill;
pub mod task;
pub mod truncate;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::sync::Arc;

use pie_agent_core::AgentTool;

/// Default tool set the coding agent ships with. Order matches the TS `createCodingTools()`
/// + the read-only quartet (`grep`/`find`/`ls`) the TS exposes via `createAllTools()`.
pub fn default_tools(memory_dir: std::path::PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool),
        Arc::new(write::WriteTool),
        Arc::new(edit::EditTool),
        Arc::new(bash::BashTool),
        Arc::new(ls::LsTool),
        Arc::new(grep::GrepTool),
        Arc::new(find::FindTool),
        Arc::new(web_fetch::WebFetchTool),
        Arc::new(web_search::WebSearchTool::new()),
        Arc::new(git::GitTool),
        Arc::new(memory::MemoryTool::new(memory_dir)),
    ]
}

/// Read-only tool set used by spawned subagents (issue #11). No `write`/`edit`/`bash` — a
/// subagent should not mutate the workspace; if it needs to, the parent agent should run the
/// write itself.
pub fn subagent_read_only_tools() -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool),
        Arc::new(ls::LsTool),
        Arc::new(grep::GrepTool),
        Arc::new(find::FindTool),
        Arc::new(web_fetch::WebFetchTool),
        Arc::new(git::GitTool),
    ]
}

/// Build the Task tool. Separate from `default_tools` because Task needs the model handle to
/// spawn its inner harness; the caller wires it in at construction time.
pub fn task_tool(
    model: pie_ai::Model,
    stream_fn: Option<pie_agent_core::StreamFn>,
) -> Arc<dyn AgentTool> {
    Arc::new(task::TaskTool::new(
        model,
        stream_fn,
        Arc::new(subagent_read_only_tools),
    ))
}

/// Build the `Skill` tool. Separate from `default_tools` because the tool needs to reach the
/// live `AgentHarness::skills()` snapshot, and the harness does not exist yet when this is
/// called (we are still assembling the tool list that will be passed to `AgentHarness::new`).
///
/// The caller (`main.rs`) builds an `Arc<OnceCell<Arc<AgentHarness>>>`, passes it here, and —
/// crucially — sets the cell immediately after the harness is constructed and *before* the
/// REPL accepts any input. If the cell is unset at execute time the tool returns a recoverable
/// `AgentToolError`, never a panic.
pub fn skill_tool(harness_cell: skill::SkillHarnessCell) -> Arc<dyn AgentTool> {
    Arc::new(skill::SkillTool::new(harness_cell))
}
