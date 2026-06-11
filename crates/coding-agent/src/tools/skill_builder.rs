//! `SkillBuilder` builtin tool (issue #21).
//!
//! Authors a new user-global skill from structured fields. Where `InstallSkill` ingests a
//! complete, externally sourced `SKILL.md`, `SkillBuilder` owns the format: the model
//! supplies `name` / `description` / `instructions` (+ optional `examples`) and the tool
//! renders the canonical template, so every produced skill is loadable by construction and
//! the model never hand-assembles frontmatter.
//!
//! Safety model is inherited from `InstallSkill` and shares its code paths:
//!
//! - Two-phase `confirm` flow — the first call only validates and previews; nothing is
//!   written until an explicit `confirm: true` call (+ `overwrite: true` when a same-name
//!   skill exists with different content).
//! - Rendered content runs through the same `parse_and_validate_skill_md` used by
//!   `InstallSkill`, then the same atomic tempfile+rename write and catalog hot-reload.
//! - `PermissionClassification::Prompt` (control-plane write) with a bounded reason; the
//!   skill name enters the reason only after passing the kebab-case charset check.
//! - The audit entry (`skill_install`, `source_kind: "builder"`) carries metadata + hashes
//!   only — never the body.

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

use crate::tools::install_skill::{
    atomic_write_skill, default_skills_root, on_disk_skill_hash, parse_and_validate_skill_md,
};
use crate::tools::skill::SkillHarnessCell;

pub struct SkillBuilderTool {
    harness: SkillHarnessCell,
    /// `${PIE_DIR:-~/.pie}/skills` in production; explicit so tests target a temp dir.
    skills_root: PathBuf,
}

impl SkillBuilderTool {
    pub fn new(harness: SkillHarnessCell) -> Self {
        Self::with_skills_root(harness, default_skills_root())
    }

    pub fn with_skills_root(harness: SkillHarnessCell, skills_root: PathBuf) -> Self {
        Self {
            harness,
            skills_root,
        }
    }

    fn target_path(&self, name: &str) -> PathBuf {
        self.skills_root.join(name).join("SKILL.md")
    }
}

#[derive(Debug, Deserialize)]
struct BuildInput {
    name: String,
    description: String,
    instructions: String,
    #[serde(default)]
    examples: Option<String>,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    overwrite: bool,
}

/// Render the canonical `SKILL.md`. Frontmatter goes through `serde_yaml` so special
/// characters in `description` are escaped correctly; the description is collapsed to a
/// single line first (it is the catalog trigger line, not body text).
fn render_skill_md(
    name: &str,
    description: &str,
    instructions: &str,
    examples: Option<&str>,
) -> Result<String, AgentToolError> {
    let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if description.is_empty() {
        return Err(AgentToolError::from("description must not be empty"));
    }
    if instructions.trim().is_empty() {
        return Err(AgentToolError::from("instructions must not be empty"));
    }

    let mut frontmatter = serde_yaml::Mapping::new();
    frontmatter.insert("name".into(), name.into());
    frontmatter.insert("description".into(), description.into());
    let yaml = serde_yaml::to_string(&frontmatter)
        .map_err(|e| AgentToolError::Message(format!("render frontmatter: {e}")))?;

    let mut out = format!(
        "---\n{yaml}---\n\n# {}\n\n## Instructions\n\n{}\n",
        title_from_name(name),
        instructions.trim()
    );
    if let Some(examples) = examples.map(str::trim).filter(|e| !e.is_empty()) {
        out.push_str(&format!("\n## Examples\n\n{examples}\n"));
    }
    Ok(out)
}

/// `code-review-checklist` → `Code Review Checklist`.
fn title_from_name(name: &str) -> String {
    name.split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl AgentTool for SkillBuilderTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "SkillBuilder"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // Writes to the global skills directory and triggers a harness reload.
        Some(ToolExecutionMode::Sequential)
    }

    /// The preview phase is a pure read (render + validate, no fs writes), so it runs
    /// under `Allow` — the user-initiated "summarize recent work into a skill" flow costs
    /// exactly one approval, on the `confirm: true` write. That write is a persistent
    /// control-plane change growing the model's skill surface, so it always prompts. The
    /// name is model-supplied and only enters the bounded reason after passing the same
    /// charset shape the validator enforces. (Unlike InstallSkill — which prompts on
    /// preview too because it fetches untrusted external content — SkillBuilder's preview
    /// input is model-authored from the visible conversation.)
    fn permission_classification(&self, prepared_args: &Value) -> PermissionClassification {
        let confirm = prepared_args
            .get("confirm")
            .and_then(|c| c.as_bool())
            .unwrap_or(false);
        if !confirm {
            return PermissionClassification::Allow;
        }
        let name = prepared_args
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| {
                !n.is_empty()
                    && n.len() <= 64
                    && n.chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            })
            .unwrap_or("<invalid name>");
        PermissionClassification::Prompt {
            reason: format!("create user skill `{name}`"),
        }
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let input: BuildInput = serde_json::from_value(params)
            .map_err(|e| AgentToolError::Message(format!("invalid arguments: {e}")))?;

        // Phase 1: render + validate. The rendered content goes through the exact
        // validation InstallSkill applies, so authored skills can never diverge from what
        // the loader accepts. Pure read; no fs writes happen here.
        let rendered = render_skill_md(
            &input.name,
            &input.description,
            &input.instructions,
            input.examples.as_deref(),
        )?;
        let parsed = parse_and_validate_skill_md(&rendered)?;
        if parsed.name != input.name {
            return Err(AgentToolError::Message(format!(
                "skill name `{}` did not survive rendering; use lowercase kebab-case",
                input.name
            )));
        }
        let target_path = self.target_path(&parsed.name);
        let existing_hash = on_disk_skill_hash(&target_path).await;
        let existing = existing_hash.is_some();
        let overwrite_required = existing && existing_hash.as_deref() != Some(&parsed.content_hash);

        // Shadow warnings come from the live catalog: a same-name project skill takes
        // precedence over the new user skill; a same-name builtin is shadowed by it.
        let mut warnings = parsed.warnings.clone();
        if let Some(harness) = self.harness.get() {
            for skill in harness.skills() {
                if skill.name == parsed.name {
                    match skill.source {
                        SkillSource::Project => warnings.push(format!(
                            "a project skill named '{}' exists and will shadow this user skill",
                            parsed.name
                        )),
                        SkillSource::Builtin => warnings.push(format!(
                            "this will shadow the builtin skill '{}'",
                            parsed.name
                        )),
                        SkillSource::User => {}
                    }
                }
            }
        }

        if !input.confirm {
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "preview only — call again with `confirm: true` to create the skill. \
                     name={} target={} size={}B existing={} overwrite_required={}",
                    parsed.name,
                    target_path.display(),
                    parsed.size,
                    existing,
                    overwrite_required
                ))],
                details: json!({
                    "phase": "preview",
                    "name": parsed.name,
                    "description": parsed.description,
                    "warnings": warnings,
                    "target_path": target_path.display().to_string(),
                    "content_hash": parsed.content_hash,
                    "size": parsed.size,
                    "existing": existing,
                    "overwrite_required": overwrite_required,
                }),
                terminate: None,
            });
        }

        // Phase 2: write. Refuse silent overwrite unless caller explicitly asked.
        if overwrite_required && !input.overwrite {
            return Err(AgentToolError::Message(format!(
                "skill '{}' already exists with different content. Call again with \
                 `overwrite: true` to replace it.",
                parsed.name
            )));
        }

        atomic_write_skill(&target_path, &parsed.normalized_content).await?;

        let harness = self
            .harness
            .get()
            .ok_or_else(|| AgentToolError::from("SkillBuilder not yet initialized"))?;
        let reload = harness
            .reload_skills_from_disk()
            .await
            .map_err(|e| AgentToolError::Message(format!("reload after build: {e}")))?;

        let installed = reload.skills.iter().any(|s| s.name == parsed.name);
        warnings.extend(
            reload
                .diagnostics
                .iter()
                .filter(|d| {
                    d.path.contains(&parsed.name) || d.path == target_path.display().to_string()
                })
                .map(|d| format!("{:?}: {}", d.code, d.message)),
        );

        // Persistent audit: same `skill_install` channel as InstallSkill so resume and
        // forensics see one uniform record of model-driven skill writes. `source_kind:
        // "builder"` distinguishes authored skills; the body is never included.
        let audit_payload = json!({
            "status": "installed",
            "name": parsed.name,
            "target_path": target_path.display().to_string(),
            "source_kind": "builder",
            "source": Value::Null,
            "before_hash": existing_hash,
            "after_hash": parsed.content_hash,
            "size": parsed.size,
            "overwrote": overwrite_required,
            "idempotent": existing && !overwrite_required,
            "installed_visible_in_catalog": installed,
            "diagnostics_count": reload.diagnostics.len(),
            "warnings": warnings.clone(),
        });
        let audit_entry_id = match harness
            .session()
            .append_custom("skill_install", Some(audit_payload))
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(
                    skill = %parsed.name,
                    error = %e,
                    "skill_install audit write failed; the skill itself was created"
                );
                None
            }
        };

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "created skill '{}' at {} ({}B). catalog now has {} skill(s).",
                parsed.name,
                target_path.display(),
                parsed.size,
                reload.skills.len()
            ))],
            details: json!({
                "phase": "installed",
                "name": parsed.name,
                "target_path": target_path.display().to_string(),
                "content_hash": parsed.content_hash,
                "size": parsed.size,
                "overwrote": overwrite_required,
                "total_skills_after": reload.skills.len(),
                "diagnostics_count": reload.diagnostics.len(),
                "warnings": warnings,
                "installed_visible_in_catalog": installed,
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "SkillBuilder".into(),
    description:
        "Create a NEW user skill from structured fields and hot-reload the catalog. Use this \
         when the user asks to create, save, or codify a reusable skill, workflow, checklist, \
         or convention — including \"summarize the recent work / this conversation into a \
         skill\": distill the generalizable workflow from the conversation (steps actually \
         performed, commands used, pitfalls hit) and write instructions for the general case, \
         not a transcript of this one instance. Use InstallSkill instead when installing an \
         existing SKILL.md from a URL, file, or pasted content. The tool renders canonical \
         SKILL.md (frontmatter + sections) from name/description/instructions — do not \
         hand-write frontmatter. Two-phase: first call without `confirm` validates and \
         returns a preview (target path, hash, size, shadow warnings); show the user the \
         planned name/description and get their go-ahead, then call again with `confirm: \
         true` to write atomically to ~/.pie/skills/<name>/SKILL.md and reload. A same-name \
         skill with different content additionally requires `overwrite: true`."
            .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Skill name: lowercase kebab-case (a-z, 0-9, hyphens), max 64 chars. Becomes the directory name and the /skill lookup key."
            },
            "description": {
                "type": "string",
                "description": "One-line summary of what the skill does AND when to use it (max 1024 chars). This is the trigger line the model sees in the catalog — include concrete cue phrases."
            },
            "instructions": {
                "type": "string",
                "description": "Markdown body: the steps, conventions, and guidance the skill teaches. Rendered under an '## Instructions' heading."
            },
            "examples": {
                "type": "string",
                "description": "Optional markdown examples, rendered under an '## Examples' heading."
            },
            "confirm": {
                "type": "boolean",
                "default": false,
                "description": "When false (default), validates and returns a preview without writing. When true, writes the skill and reloads the catalog."
            },
            "overwrite": {
                "type": "boolean",
                "default": false,
                "description": "Required when a skill of the same name already exists with different content."
            }
        },
        "required": ["name", "description", "instructions"],
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

    fn build_test_harness(
        seed: Vec<Skill>,
    ) -> (Arc<AgentHarness>, SkillHarnessCell, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf();
        let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
        let session = Session::new(storage);
        let mut opts = AgentHarnessOptions::new(fake_model(), session);
        opts.skills = seed;
        let dir_clone = dir_path.clone();
        let loader: ReloadSkillsFn = Arc::new(move || {
            let dir_for_fut = dir_clone.clone();
            Box::pin(async move {
                let env = pie_agent_core::NativeEnv::new(
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                );
                pie_agent_core::load_skills(
                    &env,
                    &[dir_for_fut.to_string_lossy().as_ref()],
                    CancellationToken::new(),
                )
                .await
            })
        });
        opts.reload_skills_fn = Some(loader);
        let harness = Arc::new(AgentHarness::new(opts));
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        assert!(cell.set(harness.clone()).is_ok(), "set once");
        (harness, cell, dir)
    }

    fn test_tool(cell: SkillHarnessCell, dir: &tempfile::TempDir) -> SkillBuilderTool {
        SkillBuilderTool::with_skills_root(cell, dir.path().to_path_buf())
    }

    async fn execute(
        tool: &SkillBuilderTool,
        params: Value,
    ) -> Result<AgentToolResult, AgentToolError> {
        tool.execute("call-1", params, CancellationToken::new(), None)
            .await
    }

    fn build_args(name: &str, confirm: bool) -> Value {
        json!({
            "name": name,
            "description": "review rust code for unwrap abuse; use when reviewing rust PRs",
            "instructions": "1. grep for unwrap\n2. flag each in non-test code",
            "confirm": confirm,
        })
    }

    #[test]
    fn render_produces_loadable_canonical_template() {
        let rendered = render_skill_md(
            "code-review-checklist",
            "review code; use when asked to review",
            "step one\nstep two",
            None,
        )
        .expect("render");
        assert!(rendered.starts_with("---\n"), "{rendered}");
        assert!(rendered.contains("# Code Review Checklist"), "{rendered}");
        assert!(rendered.contains("## Instructions"), "{rendered}");
        assert!(rendered.contains("step one\nstep two"), "{rendered}");
        assert!(
            !rendered.contains("## Examples"),
            "examples section must be omitted when not provided: {rendered}"
        );
        let parsed = parse_and_validate_skill_md(&rendered).expect("rendered skill must load");
        assert_eq!(parsed.name, "code-review-checklist");
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    #[test]
    fn render_includes_examples_section_when_provided() {
        let rendered = render_skill_md(
            "alpha",
            "desc; when",
            "body",
            Some("```\npie session export\n```"),
        )
        .expect("render");
        assert!(rendered.contains("## Examples"), "{rendered}");
        assert!(rendered.contains("pie session export"), "{rendered}");
    }

    #[test]
    fn render_escapes_yaml_specials_and_folds_newlines_in_description() {
        let rendered = render_skill_md(
            "alpha",
            "tricky: contains #yaml \"specials\"\nand a second line",
            "body",
            None,
        )
        .expect("render");
        let parsed = parse_and_validate_skill_md(&rendered)
            .expect("description with yaml specials must stay loadable");
        assert_eq!(
            parsed.description,
            "tricky: contains #yaml \"specials\" and a second line"
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    }

    /// Preview is a pure read — it must not consume a user confirmation. Only the
    /// `confirm: true` write phase routes through the control-plane prompt, so the
    /// "summarize recent work into a skill" flow costs the user exactly one approval.
    #[test]
    fn preview_is_allowed_and_only_confirm_prompts() {
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let tool = SkillBuilderTool::with_skills_root(cell, PathBuf::from("/tmp"));

        let preview = tool.permission_classification(&build_args("alpha", false));
        assert!(
            matches!(preview, PermissionClassification::Allow),
            "preview must not prompt: {preview:?}"
        );

        let confirm = tool.permission_classification(&build_args("alpha", true));
        match confirm {
            PermissionClassification::Prompt { reason } => {
                assert!(reason.contains("alpha"), "{reason}");
            }
            other => panic!("confirm must prompt, got {other:?}"),
        }

        let bad_name = tool.permission_classification(&build_args("../etc", true));
        match bad_name {
            PermissionClassification::Prompt { reason } => {
                assert!(reason.contains("<invalid name>"), "{reason}");
            }
            other => panic!("confirm must prompt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn preview_returns_metadata_without_writing() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);

        let result = execute(&tool, build_args("alpha", false))
            .await
            .expect("preview should succeed");

        assert_eq!(result.details["phase"], "preview");
        assert_eq!(result.details["name"], "alpha");
        assert_eq!(result.details["existing"], false);
        assert_eq!(result.details["overwrite_required"], false);
        assert!(
            result.details["target_path"]
                .as_str()
                .unwrap()
                .ends_with("alpha/SKILL.md")
        );
        assert!(
            !dir.path().join("alpha").exists(),
            "preview must not create any files"
        );
    }

    #[tokio::test]
    async fn confirm_writes_skill_and_reloads_catalog() {
        let (harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);

        let result = execute(&tool, build_args("alpha", true))
            .await
            .expect("build should succeed");

        assert_eq!(result.details["phase"], "installed");
        assert_eq!(result.details["installed_visible_in_catalog"], true);
        assert!(
            result.details["audit_entry_id"].as_str().is_some(),
            "audit entry must be recorded: {}",
            result.details
        );
        let on_disk = std::fs::read_to_string(dir.path().join("alpha/SKILL.md")).unwrap();
        assert!(on_disk.starts_with("---\nname: alpha\n"), "{on_disk}");
        assert!(
            harness.skills().iter().any(|s| s.name == "alpha"),
            "catalog must contain the new skill after reload"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_name_before_any_write() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);

        let err = execute(&tool, build_args("../escape", true))
            .await
            .expect_err("traversal name must be refused");
        let msg = format!("{err:?}");
        assert!(msg.contains("name"), "{msg}");
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "nothing may be written for an invalid name"
        );
    }

    #[tokio::test]
    async fn overwrite_requires_explicit_flag() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);

        execute(&tool, build_args("alpha", true)).await.unwrap();

        // Same name, different instructions → must refuse without overwrite.
        let mut changed = build_args("alpha", true);
        changed["instructions"] = json!("totally different body");
        let err = execute(&tool, changed.clone())
            .await
            .expect_err("differing content must require overwrite");
        assert!(format!("{err:?}").contains("overwrite"), "{err:?}");

        changed["overwrite"] = json!(true);
        let result = execute(&tool, changed).await.expect("overwrite succeeds");
        assert_eq!(result.details["overwrote"], true);
        let on_disk = std::fs::read_to_string(dir.path().join("alpha/SKILL.md")).unwrap();
        assert!(on_disk.contains("totally different body"), "{on_disk}");
    }

    #[tokio::test]
    async fn preview_warns_when_project_skill_shadows_new_name() {
        let project_skill = Skill {
            name: "alpha".into(),
            description: "project one".into(),
            file_path: "/proj/.pie/skills/alpha/SKILL.md".into(),
            content: "body".into(),
            disable_model_invocation: false,
            source: SkillSource::Project,
        };
        let (_harness, cell, dir) = build_test_harness(vec![project_skill]);
        let tool = test_tool(cell, &dir);

        let result = execute(&tool, build_args("alpha", false)).await.unwrap();
        let warnings = result.details["warnings"].to_string();
        assert!(
            warnings.contains("project"),
            "must warn that a project skill shadows the new user skill: {warnings}"
        );
    }
}
