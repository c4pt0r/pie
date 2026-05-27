//! Built-in tools. Modeled on `packages/coding-agent/src/core/tools/` (the TS implementation):
//! same names, same parameter shapes, simpler bodies. Each tool implements
//! [`pie_agent_core::AgentTool`].

pub mod bash;
pub mod edit;
pub mod find;
pub mod git;
pub mod grep;
pub mod install_skill;
pub mod ls;
pub mod mcp_adapter;
pub mod memory;
pub mod read;
pub mod remove_skill;
pub mod set_skill_state;
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

/// Build the `InstallSkill` tool. Same harness-cell wiring as `skill_tool` because install
/// must hot-reload the catalog via `AgentHarness::reload_skills_from_disk` after writing.
/// See `install_skill::InstallSkillTool` for the two-phase safety model
/// (preview → confirm) and the security note about the in-flight
/// `PermissionCategory::ControlPlaneWrite` plumbing.
pub fn install_skill_tool(harness_cell: skill::SkillHarnessCell) -> Arc<dyn AgentTool> {
    Arc::new(install_skill::InstallSkillTool::new(harness_cell))
}

/// Build the `SetSkillState` tool (enable/disable a loaded skill at runtime). Same
/// harness-cell wiring as `skill_tool` / `install_skill_tool` — it reads the live catalog,
/// writes the `~/.pie/skills-state.json` overlay, and hot-reloads via
/// `reload_skills_from_disk`. See `set_skill_state::SetSkillStateTool` for the overlay model.
pub fn set_skill_state_tool(harness_cell: skill::SkillHarnessCell) -> Arc<dyn AgentTool> {
    Arc::new(set_skill_state::SetSkillStateTool::new(harness_cell))
}

/// Build the `RemoveSkill` tool (delete a user-installed skill). Same harness-cell wiring;
/// deletes `~/.pie/skills/<name>/`, clears the overlay entry, and hot-reloads. Builtin/project
/// skills are refused (disable instead). See `remove_skill::RemoveSkillTool`.
pub fn remove_skill_tool(harness_cell: skill::SkillHarnessCell) -> Arc<dyn AgentTool> {
    Arc::new(remove_skill::RemoveSkillTool::new(harness_cell))
}

/// Build the session-scoped cron creation tool. This is the model-facing counterpart to
/// `/cron add`: when the user asks in ordinary conversation for a scheduled / recurring
/// job, the model can register it without falling back to a dynamic trigger.
pub fn new_cron_job_tool(harness_cell: skill::SkillHarnessCell) -> Arc<dyn AgentTool> {
    Arc::new(crate::triggers::NewCronJobTool::new(Some(harness_cell)))
}

/// Build the dynamic trigger creation tool. This is model-facing counterpart to the
/// `/new-trigger` slash command: when the user asks in ordinary conversation to create an
/// automation, the model can register the rule without requiring slash-command syntax.
pub fn new_trigger_tool() -> Arc<dyn AgentTool> {
    Arc::new(crate::triggers::NewTriggerTool)
}

/// Build the dynamic trigger listing tool. This is the model-facing counterpart to
/// `/triggers rules`: it lets the assistant inspect current rule ids before answering or
/// removing a rule.
pub fn list_triggers_tool() -> Arc<dyn AgentTool> {
    Arc::new(crate::triggers::ListTriggersTool)
}

/// Build the dynamic trigger removal tool. This is the model-facing counterpart to
/// `/triggers remove`: when the user asks in ordinary conversation to delete a trigger, the
/// model can remove the rule by id or clear all rules when explicitly requested.
pub fn remove_trigger_tool() -> Arc<dyn AgentTool> {
    Arc::new(crate::triggers::RemoveTriggerTool)
}

/// Build the dynamic trigger state tool. This lets the model pause/resume a trigger without
/// deleting the rule and losing its condition/action text.
pub fn set_trigger_state_tool() -> Arc<dyn AgentTool> {
    Arc::new(crate::triggers::SetTriggerStateTool)
}
