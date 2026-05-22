//! `Skill` builtin tool. Closes the gap flagged in issue #25: the system prompt registry
//! already tells the model "Use the Skill tool to invoke a skill by name when applicable",
//! but the tool itself was never registered, so model-issued `Skill` calls failed with
//! `no tool named 'Skill'`.
//!
//! Behavior (per issue #25 acceptance):
//! - Looks the requested name up in the live `AgentHarness::skills()` snapshot.
//! - On hit + enabled (`disable_model_invocation = false`): returns the body wrapped by
//!   `pie_agent_core::harness::skills::format_skill_invocation` as the tool result content.
//! - On hit + `disable_model_invocation = true`: returns a typed error regardless of caller
//!   path. The model-facing schema deliberately has NO `force` parameter so the model cannot
//!   bypass the disable flag (a future `/skill --force <name>` is a user-explicit follow-up
//!   that bypasses this tool entirely).
//! - On miss: returns a typed error suggesting `/skills`.
//! - On unset harness cell (chicken-and-egg during startup before `main.rs` finishes wiring):
//!   returns a recoverable typed error. Never panics.

use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use pie_agent_core::{
    AgentHarness, AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
    format_skill_invocation,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Handle the tool uses to reach the live harness. Built before `AgentHarness::new()` runs and
/// filled by `main.rs` immediately after the harness is constructed (see the wiring comment in
/// `main.rs`). The `OnceCell` is the sync `once_cell::sync::OnceCell`, not the async tokio
/// one — set-once happens synchronously during startup; reads are synchronous lookups.
pub type SkillHarnessCell = Arc<OnceCell<Arc<AgentHarness>>>;

pub struct SkillTool {
    harness: SkillHarnessCell,
}

impl SkillTool {
    pub fn new(harness: SkillHarnessCell) -> Self {
        Self { harness }
    }
}

#[async_trait]
impl AgentTool for SkillTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "Skill"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // Skills are read-only lookups against an in-memory snapshot; safe to run in parallel
        // with other tool calls.
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing required arg: name"))?
            .to_string();

        let harness = match self.harness.get() {
            Some(h) => h,
            None => {
                return Err(AgentToolError::from("Skill tool not yet initialized"));
            }
        };

        let skills = harness.skills();
        let Some(skill) = skills.iter().find(|s| s.name == name) else {
            return Err(AgentToolError::Message(format!(
                "no skill named '{name}'. Use /skills to list available skills."
            )));
        };

        if skill.disable_model_invocation {
            // Uniform enforcement: the disable flag refuses the body on every call path,
            // including a `/skill <name>` steering message that ends up here. This is the
            // safety property explicitly affirmed in issue #25 v3.
            return Err(AgentToolError::Message(format!(
                "skill '{name}' is disabled (disable_model_invocation=true); update the frontmatter to enable"
            )));
        }

        let body = format_skill_invocation(skill, None);
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(body)],
            details: json!({ "name": skill.name, "path": skill.file_path }),
            terminate: None,
        })
    }
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "Skill".into(),
    description:
        "Invoke a skill by name. Returns the skill body wrapped in a `<skill>` block for the \
         model to follow. Use this when the skill registry in the system prompt indicates the \
         skill is relevant to the current task. The skill name must match exactly an entry in \
         the registry."
            .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Exact skill name as listed in the system-prompt registry.",
            },
        },
        "required": ["name"],
        "additionalProperties": false,
    }),
});

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::OnceCell as SyncOnceCell;
    use pie_agent_core::{
        AgentHarnessOptions, MemorySessionStorage, Session, SessionStorage, Skill,
    };
    use pie_ai::{Api, Model, ModelCost, Provider};
    use std::sync::Arc;

    fn fake_model() -> Model {
        // The tool itself never invokes the model; any minimally-valid shape is enough so
        // `AgentHarness::new` succeeds. Mirrors the `faux_model` helper used in
        // `crates/agent/tests/harness_e2e.rs`.
        Model {
            id: "faux".into(),
            name: "Faux".into(),
            api: Api::from("faux"),
            provider: Provider::from("faux"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
            compat: None,
        }
    }

    fn build_harness_with_skills(skills: Vec<Skill>) -> Arc<AgentHarness> {
        let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
        let session = Session::new(storage);
        let mut opts = AgentHarnessOptions::new(fake_model(), session);
        opts.skills = skills;
        Arc::new(AgentHarness::new(opts))
    }

    fn make_skill(name: &str, disabled: bool) -> Skill {
        Skill {
            name: name.into(),
            description: format!("description of {name}"),
            file_path: format!("/tmp/skills/{name}/SKILL.md"),
            content: format!("# {name}\n\nBody of the {name} skill."),
            disable_model_invocation: disabled,
        }
    }

    async fn execute(tool: &SkillTool, name: &str) -> Result<AgentToolResult, AgentToolError> {
        tool.execute(
            "call-1",
            json!({ "name": name }),
            CancellationToken::new(),
            None,
        )
        .await
    }

    #[tokio::test]
    async fn hit_returns_wrapped_body() {
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let harness = build_harness_with_skills(vec![make_skill("alpha", false)]);
        assert!(cell.set(harness).is_ok(), "set once");

        let tool = SkillTool::new(cell);
        let result = execute(&tool, "alpha").await.expect("hit should succeed");

        let UserContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text content");
        };
        assert!(text.text.contains("<skill name=\"alpha\""));
        assert!(text.text.contains("Body of the alpha skill."));
    }

    #[tokio::test]
    async fn miss_returns_typed_error() {
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let harness = build_harness_with_skills(vec![make_skill("alpha", false)]);
        assert!(cell.set(harness).is_ok(), "set once");

        let tool = SkillTool::new(cell);
        let err = execute(&tool, "nonexistent")
            .await
            .expect_err("miss should fail");
        let AgentToolError::Message(msg) = err else {
            panic!("expected Message error");
        };
        assert!(msg.contains("no skill named 'nonexistent'"));
        assert!(msg.contains("/skills"));
    }

    #[tokio::test]
    async fn disabled_skill_refuses_body_via_model_path() {
        // Same enforcement applies regardless of caller; the tool itself cannot tell who
        // called it, which is exactly the safety property issue #25 v3 locks in.
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let harness = build_harness_with_skills(vec![make_skill("locked", true)]);
        assert!(cell.set(harness).is_ok(), "set once");

        let tool = SkillTool::new(cell);
        let err = execute(&tool, "locked")
            .await
            .expect_err("disabled skill should refuse");
        let AgentToolError::Message(msg) = err else {
            panic!("expected Message error");
        };
        assert!(msg.contains("disabled"));
        assert!(msg.contains("disable_model_invocation"));
        assert!(msg.contains("frontmatter"));
    }

    #[tokio::test]
    async fn disabled_skill_refuses_body_via_steering_path() {
        // `/skill <name>` enqueues a steering message; the next turn the model invokes the
        // `Skill` tool with the same name, which ends up here. Assert the enforcement is
        // identical — the disable flag is not bypassable by any caller path.
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let harness = build_harness_with_skills(vec![make_skill("locked", true)]);
        assert!(cell.set(harness).is_ok(), "set once");

        let tool = SkillTool::new(cell);
        let err = execute(&tool, "locked")
            .await
            .expect_err("disabled via steering path should still refuse");
        let AgentToolError::Message(msg) = err else {
            panic!("expected Message error");
        };
        assert!(msg.contains("disabled"));
        assert!(msg.contains("disable_model_invocation"));
    }

    #[tokio::test]
    async fn unset_harness_cell_returns_recoverable_error_not_panic() {
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        // Intentionally do NOT set the cell.

        let tool = SkillTool::new(cell);
        let err = execute(&tool, "anything")
            .await
            .expect_err("unset cell should fail with a typed error");
        let AgentToolError::Message(msg) = err else {
            panic!("expected Message error, not a panic");
        };
        assert!(msg.contains("Skill tool not yet initialized"));
    }

    #[tokio::test]
    async fn missing_name_arg_is_a_typed_error() {
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let harness = build_harness_with_skills(Vec::new());
        assert!(cell.set(harness).is_ok(), "set once");

        let tool = SkillTool::new(cell);
        let err = tool
            .execute("call", json!({}), CancellationToken::new(), None)
            .await
            .expect_err("missing arg should fail");
        let AgentToolError::Message(msg) = err else {
            panic!("expected Message error");
        };
        assert!(msg.contains("missing required arg: name"));
    }
}
