//! `SetSkillState` builtin tool (skill-lifecycle task #23, S-A2): enable or disable a loaded
//! skill at runtime without editing its `SKILL.md`.
//!
//! Persistence is the `~/.pie/skills-state.json` overlay (see [`crate::skills_state`]) keyed
//! by `{source, name}` — the user's SKILL.md stays pristine and the choice survives restarts
//! and reloads. Works for ANY source: a builtin or project skill that can't be deleted can
//! still be disabled. (Removal of user-installed skills is the separate `RemoveSkill` tool.)
//!
//! **Authorization model (issue #110 sub-PR 3, post-PR-#108 lift)**: the model-facing tool is
//! no longer disable-only. The narrowing/escalating split moved from `execute` into
//! [`AgentTool::permission_classification`]:
//! - `enabled: false` (disable) is narrowing → `PermissionClassification::Allow`. The model
//!   may disable a skill on its own; the user can always re-enable via `/skills enable`.
//! - `enabled: true` (re-enable) is escalating → `PermissionClassification::Prompt` with a
//!   bounded reason naming the skill. The runtime routes through `on_control_plane_prompt`
//!   (PRs #135 / #137 / #138), and the user explicitly confirms each re-enable through the
//!   embedder prompt card. Replaces the PR #108 hard-block stopgap.
//!
//! Safety:
//! - Two-phase preview: the first call (without `confirm: true`) previews the change (current
//!   vs target enabled state, resolved source) without writing. `confirm: true` applies it.
//!   The preview is now a UX affordance (lets the model surface its intent in chat before the
//!   user sees the prompt card); the actual security gate is `permission_classification`.
//! - Source resolution is unambiguous: `harness.skills()` is deduped by name (project shadows
//!   user), so the active skill for a name has exactly one source. The optional `source` arg,
//!   if given, must match the resolved source.
//! - Audit: `Custom { custom_type: "skill_control_plane" }` records op/name/source/
//!   before+after enabled state + actor. No skill body. Issue #110 sub-PR 1.5 additionally
//!   writes a `control_plane_prompt` audit entry on the runtime side for every prompt
//!   resolution; the two audits complement each other (per-decision vs. per-state-change).

use std::path::PathBuf;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
    SkillSource, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::skills_state;
use crate::tools::skill::SkillHarnessCell;

pub struct SetSkillStateTool {
    harness: SkillHarnessCell,
    /// The pie base dir (`~/.pie`) that holds `skills-state.json`. Injected so tests use a
    /// temp dir instead of the user's real home.
    base_dir: PathBuf,
}

impl SetSkillStateTool {
    pub fn new(harness: SkillHarnessCell) -> Self {
        Self::with_base_dir(harness, default_base_dir())
    }

    pub fn with_base_dir(harness: SkillHarnessCell, base_dir: PathBuf) -> Self {
        Self { harness, base_dir }
    }
}

/// Production pie base dir: `${PIE_DIR:-$HOME/.pie}`. Inlined (not via `crate::config`) so the
/// module can be pulled into integration tests through `#[path]` includes.
pub(crate) fn default_base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PIE_DIR") {
        return PathBuf::from(p);
    }
    directories::BaseDirs::new()
        .map(|d| d.home_dir().join(".pie"))
        .unwrap_or_else(|| PathBuf::from(".pie"))
}

#[derive(Debug, Deserialize)]
struct Input {
    name: String,
    #[serde(default)]
    source: Option<String>,
    enabled: bool,
    #[serde(default)]
    confirm: bool,
}

#[async_trait]
impl AgentTool for SetSkillStateTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "SetSkillState"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // Writes the overlay + reloads the catalog — serialize against other control-plane
        // writes in the same turn.
        Some(ToolExecutionMode::Sequential)
    }

    /// Issue #110 sub-PR 3 classifier — branches on the prepared `enabled` arg:
    /// - `enabled: false` (disable) is **narrowing** — the model cannot use a skill the user
    ///   already disabled. No user confirmation needed; the classifier returns `Allow`.
    /// - `enabled: true` (re-enable) is **escalating** — re-opens a skill an author or user
    ///   intentionally disabled. The model cannot self-authorize this; the runtime routes
    ///   through the `on_control_plane_prompt` channel (PR #135 / #138) for explicit user
    ///   confirmation. The hard-block stopgap from PR #108 has been lifted — re-enabling is
    ///   now allowed once the user approves the prompt.
    fn permission_classification(&self, prepared_args: &Value) -> PermissionClassification {
        let enabled = prepared_args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !enabled {
            return PermissionClassification::Allow;
        }
        let name = prepared_args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        PermissionClassification::Prompt {
            reason: format!("re-enable user-disabled skill `{name}`"),
        }
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let input: Input = serde_json::from_value(params)
            .map_err(|e| AgentToolError::Message(format!("invalid arguments: {e}")))?;

        // PR #108's hard-block on `enabled: true` was a stopgap until ControlPlaneWrite had a
        // real user-Prompt gate. Issue #110 sub-PRs 1 / 1.5 / 2 landed that channel; sub-PR 3
        // moves the gate into `permission_classification` above so a user actually confirms
        // each re-enable through the embedder prompt card. No code here re-rejects enable.

        let harness = self
            .harness
            .get()
            .ok_or_else(|| AgentToolError::from("SetSkillState not yet initialized"))?;

        // Resolve the active skill by name (catalog is deduped by name).
        let skills = harness.skills();
        let Some(skill) = skills.iter().find(|s| s.name == input.name) else {
            let mut names: Vec<&str> = skills
                .iter()
                .filter(|s| s.name.starts_with(&input.name) || s.name.contains(&input.name))
                .map(|s| s.name.as_str())
                .take(5)
                .collect();
            names.dedup();
            let hint = if names.is_empty() {
                String::new()
            } else {
                format!(" Did you mean: {}?", names.join(", "))
            };
            return Err(AgentToolError::Message(format!(
                "no loaded skill named '{}'. Run /skills to list loaded skills.{hint}",
                input.name
            )));
        };
        let resolved_source = skill.source;

        // If the caller pinned a source, it must match the active one.
        if let Some(req) = &input.source {
            let req_src = parse_source(req)?;
            if req_src != resolved_source {
                return Err(AgentToolError::Message(format!(
                    "skill '{}' is active from source '{}', not '{}'. Omit `source` or pass \
                     '{}' (the active source).",
                    input.name,
                    resolved_source.label(),
                    req_src.label(),
                    resolved_source.label()
                )));
            }
        }

        let currently_enabled = !skill.disable_model_invocation;
        let target_enabled = input.enabled;

        if !input.confirm {
            let noop = currently_enabled == target_enabled;
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "preview only — call again with `confirm: true` to apply. \
                     skill={} source={} currently={} target={}{}",
                    input.name,
                    resolved_source.label(),
                    enabled_word(currently_enabled),
                    enabled_word(target_enabled),
                    if noop { " (no change)" } else { "" }
                ))],
                details: json!({
                    "phase": "preview",
                    "name": input.name,
                    "source": resolved_source.label(),
                    "currently_enabled": currently_enabled,
                    "target_enabled": target_enabled,
                    "no_change": noop,
                }),
                terminate: None,
            });
        }

        // Apply: write the overlay, then reload so the catalog reflects the new state.
        skills_state::set_and_save(&self.base_dir, &input.name, resolved_source, target_enabled)
            .await
            .map_err(|e| AgentToolError::Message(format!("persist skill state: {e}")))?;

        let reload = harness
            .reload_skills_from_disk()
            .await
            .map_err(|e| AgentToolError::Message(format!("reload after state change: {e}")))?;

        // Confirm the catalog now reflects the intended state.
        let effective_enabled = reload
            .skills
            .iter()
            .find(|s| s.name == input.name && s.source == resolved_source)
            .map(|s| !s.disable_model_invocation);

        // Audit: control-plane state change. Body is never recorded.
        let audit = json!({
            "op": "set_state",
            "actor": "tool",
            "name": input.name,
            "source": resolved_source.label(),
            "before_enabled": currently_enabled,
            "after_enabled": target_enabled,
        });
        let audit_entry_id = match harness
            .session()
            .append_custom("skill_control_plane", Some(audit))
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(
                    skill = %input.name,
                    error = %e,
                    "skill_control_plane audit write failed; state change itself succeeded"
                );
                None
            }
        };

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "{} skill '{}' (source: {}).",
                if target_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                input.name,
                resolved_source.label()
            ))],
            details: json!({
                "phase": "applied",
                "name": input.name,
                "source": resolved_source.label(),
                "enabled": target_enabled,
                "effective_enabled_after_reload": effective_enabled,
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

fn parse_source(s: &str) -> Result<SkillSource, AgentToolError> {
    match s.to_ascii_lowercase().as_str() {
        "builtin" => Ok(SkillSource::Builtin),
        "user" => Ok(SkillSource::User),
        "project" => Ok(SkillSource::Project),
        // Fixed wording — do not echo the raw arg back (defense-in-depth, Provider/Auth
        // review on PR #108): the model already knows what it passed, and not reflecting
        // arbitrary tool input keeps the redaction discipline uniform.
        _ => Err(AgentToolError::from(
            "invalid `source` (expected one of: builtin, user, project)",
        )),
    }
}

fn enabled_word(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "SetSkillState".into(),
    description: "Enable or disable a loaded skill at runtime without editing its SKILL.md. \
         The choice is recorded in a local overlay (~/.pie/skills-state.json) keyed by \
         source+name and survives restarts. Works for any source — a builtin or project skill \
         that can't be removed can still be disabled. Two-phase: first call previews (current \
         vs target state); call again with `confirm: true` to apply. Disabling prevents the \
         model from auto-invoking the skill via the Skill tool; the skill still appears in \
         the catalog. Re-enabling a previously-disabled skill is a privileged control-plane \
         write and requires explicit user confirmation through the runtime prompt card before \
         it takes effect (issue #110); disabling does not prompt."
        .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Exact skill name as shown in /skills."
            },
            "source": {
                "type": "string",
                "enum": ["builtin", "user", "project"],
                "description": "Optional. The active source is resolved automatically; if given, must match it."
            },
            "enabled": {
                "type": "boolean",
                "description": "Target state. `false` disables (no user prompt). `true` re-enables and triggers a user confirmation prompt before the change applies."
            },
            "confirm": {
                "type": "boolean",
                "default": false,
                "description": "When false (default) returns a preview; when true applies the change."
            }
        },
        "required": ["name", "enabled"],
        "additionalProperties": false
    }),
});

#[cfg(test)]
mod tests {
    use super::*;
    use once_cell::sync::OnceCell as SyncOnceCell;
    use pie_agent_core::{
        AgentHarness, AgentHarnessOptions, MemorySessionStorage, ReloadSkillsFn, Session,
        SessionStorage, Skill,
    };
    use pie_ai::{Api, Model, ModelCost, Provider};
    use std::sync::Arc;

    fn fake_model() -> Model {
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

    fn skill(name: &str, source: SkillSource, disabled: bool) -> Skill {
        Skill {
            name: name.into(),
            description: "d".into(),
            file_path: format!("/tmp/{name}/SKILL.md"),
            content: "body".into(),
            disable_model_invocation: disabled,
            source,
        }
    }

    /// Build a harness whose catalog is `seed` and whose `reload_skills_from_disk` re-derives
    /// the catalog from `seed` with the overlay at `base_dir` applied — mirroring how the real
    /// main.rs reload closure layers the overlay on top of the loaded skills.
    fn build(seed: Vec<Skill>, base_dir: PathBuf) -> (Arc<AgentHarness>, SkillHarnessCell) {
        let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
        let session = Session::new(storage);
        let mut opts = AgentHarnessOptions::new(fake_model(), session);
        opts.skills = seed.clone();
        let seed_for_reload = seed.clone();
        let base_for_reload = base_dir.clone();
        let loader: ReloadSkillsFn = Arc::new(move || {
            let mut skills = seed_for_reload.clone();
            let base = base_for_reload.clone();
            Box::pin(async move {
                let state = skills_state::load(&base).await;
                skills_state::apply(&state, &mut skills);
                pie_agent_core::LoadSkillsOutput {
                    skills,
                    diagnostics: vec![],
                }
            })
        });
        opts.reload_skills_fn = Some(loader);
        let harness = Arc::new(AgentHarness::new(opts));
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        assert!(cell.set(harness.clone()).is_ok());
        (harness, cell)
    }

    async fn exec(
        tool: &SetSkillStateTool,
        params: Value,
    ) -> Result<AgentToolResult, AgentToolError> {
        tool.execute("c1", params, CancellationToken::new(), None)
            .await
    }

    #[tokio::test]
    async fn preview_does_not_write_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let (_h, cell) = build(
            vec![skill("foo", SkillSource::User, false)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());

        let res = exec(&tool, json!({"name": "foo", "enabled": false}))
            .await
            .expect("preview ok");
        assert_eq!(res.details["phase"], "preview");
        assert_eq!(res.details["currently_enabled"], true);
        assert_eq!(res.details["target_enabled"], false);
        // No overlay file written.
        assert!(!skills_state::state_path(dir.path()).exists());
    }

    #[tokio::test]
    async fn disable_then_reload_reflects_state() {
        let dir = tempfile::tempdir().unwrap();
        let (harness, cell) = build(
            vec![skill("foo", SkillSource::User, false)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());

        let res = exec(
            &tool,
            json!({"name": "foo", "enabled": false, "confirm": true}),
        )
        .await
        .expect("apply ok");
        assert_eq!(res.details["phase"], "applied");
        assert_eq!(res.details["enabled"], false);
        assert_eq!(res.details["effective_enabled_after_reload"], false);
        // Overlay persisted.
        let state = skills_state::load(dir.path()).await;
        assert_eq!(
            state.lookup("foo", SkillSource::User).map(|e| e.enabled),
            Some(false)
        );
        // Harness catalog now shows the skill disabled.
        let foo = harness
            .skills()
            .into_iter()
            .find(|s| s.name == "foo")
            .unwrap();
        assert!(foo.disable_model_invocation);
    }

    #[tokio::test]
    async fn classifier_routes_disable_through_allow_and_enable_through_prompt() {
        // Issue #110 sub-PR 3 (Tools-MCP): the old PR #108 hard-block on `enabled: true` is
        // gone. The classifier now narrows disables to `Allow` and routes enables through the
        // `on_control_plane_prompt` channel so a real user (not the model) approves each
        // re-enable. This test asserts the per-arg classification shape; the integration test
        // for "Prompt + Deny actually blocks the tool" lives in the agent crate's
        // `permission_classification_prompt_with_hook_deny_blocks_and_emits_audit_event`.
        let dir = tempfile::tempdir().unwrap();
        let (_harness, cell) = build(
            vec![skill("foo", SkillSource::User, true)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());

        // Disable = narrowing = Allow.
        let disable = tool.permission_classification(&json!({"name": "foo", "enabled": false}));
        assert!(
            matches!(disable, PermissionClassification::Allow),
            "disable must classify as Allow (narrowing), got {disable:?}"
        );

        // Enable = escalating = Prompt with the skill name in the reason. The bounded reason
        // is what the embedder renders on the confirmation card (per §6b.5 prompt card UX).
        let enable = tool.permission_classification(&json!({"name": "foo", "enabled": true}));
        match enable {
            PermissionClassification::Prompt { reason } => {
                assert!(
                    reason.contains("re-enable"),
                    "reason must signal escalation, got: {reason}"
                );
                assert!(
                    reason.contains("`foo`"),
                    "reason must include the bounded skill name, got: {reason}"
                );
            }
            other => panic!("enable must classify as Prompt, got {other:?}"),
        }

        // Missing `enabled` field defaults to `false` (narrowing) — defensive default.
        let missing = tool.permission_classification(&json!({"name": "foo"}));
        assert!(
            matches!(missing, PermissionClassification::Allow),
            "missing enabled must default to narrowing (Allow), got {missing:?}"
        );
    }

    #[tokio::test]
    async fn enable_no_longer_short_circuits_in_execute() {
        // Regression for PR #108's lifted hard-block: `enabled: true` is no longer rejected
        // at execute() entry. (The runtime gate is `permission_classification` + the
        // embedder's prompt hook; if the user denies, the agent loop never calls execute. If
        // the user accepts, execute proceeds and the skill is re-enabled.)
        let dir = tempfile::tempdir().unwrap();
        let (harness, cell) = build(
            vec![skill("foo", SkillSource::User, true)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());

        // Direct execute() of an enable + confirm now succeeds (no model-side reject).
        let result = exec(
            &tool,
            json!({"name": "foo", "enabled": true, "confirm": true}),
        )
        .await
        .expect("execute(enable) must succeed once the prompt-gate stopgap is lifted");
        assert!(
            !result.content.is_empty(),
            "successful enable returns a result message"
        );

        // Skill is now enabled in the live catalog after the reload.
        let foo = harness
            .skills()
            .into_iter()
            .find(|s| s.name == "foo")
            .unwrap();
        assert!(
            !foo.disable_model_invocation,
            "skill is enabled after execute(enable)"
        );
    }

    #[tokio::test]
    async fn writes_skill_control_plane_audit() {
        let dir = tempfile::tempdir().unwrap();
        let (harness, cell) = build(
            vec![skill("foo", SkillSource::User, false)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());

        exec(
            &tool,
            json!({"name": "foo", "enabled": false, "confirm": true}),
        )
        .await
        .expect("apply ok");

        let entries = harness.session().entries().await.unwrap();
        let audit = entries.iter().find_map(|e| match e {
            pie_agent_core::SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "skill_control_plane" => data.clone(),
            _ => None,
        });
        let data = audit.expect("skill_control_plane audit written");
        assert_eq!(data["op"], "set_state");
        assert_eq!(data["name"], "foo");
        assert_eq!(data["source"], "user");
        assert_eq!(data["before_enabled"], true);
        assert_eq!(data["after_enabled"], false);
        // No body leak.
        let s = serde_json::to_string(&data).unwrap();
        assert!(
            !s.contains("body"),
            "audit must not contain skill body: {s}"
        );
    }

    #[tokio::test]
    async fn unknown_skill_is_typed_error_with_hint() {
        let dir = tempfile::tempdir().unwrap();
        let (_h, cell) = build(
            vec![skill("formatter", SkillSource::User, false)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());
        let err = exec(&tool, json!({"name": "format", "enabled": false}))
            .await
            .expect_err("unknown skill errors");
        let AgentToolError::Message(m) = err else {
            panic!("typed error")
        };
        assert!(m.contains("no loaded skill named 'format'"));
    }

    #[tokio::test]
    async fn mismatched_source_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // Active foo is User; caller pins project → reject.
        let (_h, cell) = build(
            vec![skill("foo", SkillSource::User, false)],
            dir.path().into(),
        );
        let tool = SetSkillStateTool::with_base_dir(cell, dir.path().into());
        let err = exec(
            &tool,
            json!({"name": "foo", "source": "project", "enabled": false}),
        )
        .await
        .expect_err("mismatched source errors");
        let AgentToolError::Message(m) = err else {
            panic!("typed error")
        };
        assert!(m.contains("active from source 'user'"), "got: {m}");
    }
}
