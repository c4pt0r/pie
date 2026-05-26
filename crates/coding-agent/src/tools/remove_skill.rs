//! `RemoveSkill` builtin tool (skill-lifecycle task #23, S-A2b): delete a **user-installed**
//! skill from `~/.pie/skills/` and hot-reload the catalog.
//!
//! Scope guard (locked with Provider/Auth + QA on the #ux skill-lifecycle thread): only
//! `SkillSource::User` skills can be removed. A `Builtin` skill is compiled into the binary;
//! a `Project` skill belongs to the repo, not this user. Removing those isn't meaningful here
//! — the tool returns a bounded error pointing at `SetSkillState` / `/skills disable` instead.
//! This keeps "remove" strictly a deletion of something the user installed.
//!
//! Safety:
//! - Two-phase: first call (without `confirm: true`) previews the target path; `confirm: true`
//!   deletes. Same `ControlPlaneWrite` tier + interim two-phase guard as the other skill
//!   control-plane tools (the runtime user-Prompt path is the shared follow-up).
//! - The deletion target is derived from the resolved skill's `file_path` and must be a direct
//!   child of `~/.pie/skills/` — never a caller-supplied path component — so a hostile name
//!   can't escape the skills root.
//! - After deleting, the skill's `{User, name}` overlay entry is cleared so a later reinstall
//!   of the same name doesn't inherit a stale disabled state.
//! - Audit: `Custom { custom_type: "skill_control_plane" }`, op `remove`, with name/source/
//!   bounded path preview — no skill body.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, SkillSource, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::skills_state;
use crate::tools::set_skill_state::default_base_dir;
use crate::tools::skill::SkillHarnessCell;

pub struct RemoveSkillTool {
    harness: SkillHarnessCell,
    /// The pie base dir (`~/.pie`). The skills root is `base_dir/skills`. Injected so tests
    /// operate on a temp dir, never the user's real home.
    base_dir: PathBuf,
}

impl RemoveSkillTool {
    pub fn new(harness: SkillHarnessCell) -> Self {
        Self::with_base_dir(harness, default_base_dir())
    }

    pub fn with_base_dir(harness: SkillHarnessCell, base_dir: PathBuf) -> Self {
        Self { harness, base_dir }
    }

    fn skills_root(&self) -> PathBuf {
        self.base_dir.join("skills")
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    confirm: bool,
}

#[async_trait]
impl AgentTool for RemoveSkillTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "RemoveSkill"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // Deletes a directory + reloads the catalog — serialize against other control-plane
        // writes in the same turn.
        Some(ToolExecutionMode::Sequential)
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

        let harness = self
            .harness
            .get()
            .ok_or_else(|| AgentToolError::from("RemoveSkill not yet initialized"))?;

        // Resolve the active skill by name (catalog deduped by name).
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
        let source = skill.source;

        // Scope guard: only user-installed skills are removable.
        if source != SkillSource::User {
            return Err(AgentToolError::Message(format!(
                "'{}' is a {} skill and cannot be removed (builtin skills are compiled in; \
                 project skills belong to the repo). Disable it instead with SetSkillState \
                 or `/skills disable {}`.",
                input.name,
                source.label(),
                input.name
            )));
        }

        // Optional source pin must be `user` (the only removable source).
        if let Some(req) = &input.source {
            let req_src = parse_source(req)?;
            if req_src != SkillSource::User {
                return Err(AgentToolError::Message(format!(
                    "only user-installed skills can be removed; '{}' is a user skill, not '{}'.",
                    input.name,
                    req_src.label()
                )));
            }
        }

        // Derive the deletion target strictly from the resolved skill's file_path, which must
        // sit under the skills root. The target is the direct child of the skills root on that
        // path (the `<name>/` dir for a `<name>/SKILL.md` layout, or the bare `<x>.md` file for
        // a root-level skill). Never built from the caller-supplied name.
        let skills_root = self.skills_root();
        let target = match deletion_target(&skills_root, Path::new(&skill.file_path)) {
            Some(t) => t,
            None => {
                return Err(AgentToolError::Message(format!(
                    "refusing to remove '{}': its file ({}) is not under the user skills root \
                     ({}).",
                    input.name,
                    skill.file_path,
                    skills_root.display()
                )));
            }
        };

        if !input.confirm {
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "preview only — call again with `confirm: true` to delete. \
                     skill={} source=user target={}",
                    input.name,
                    target.display()
                ))],
                details: json!({
                    "phase": "preview",
                    "name": input.name,
                    "source": "user",
                    "target_path": target.display().to_string(),
                }),
                terminate: None,
            });
        }

        // Delete the skill from disk.
        let removed_meta = tokio::fs::symlink_metadata(&target).await;
        match removed_meta {
            Ok(meta) if meta.is_dir() => {
                tokio::fs::remove_dir_all(&target).await.map_err(|e| {
                    AgentToolError::Message(format!("remove {}: {e}", target.display()))
                })?;
            }
            Ok(_) => {
                tokio::fs::remove_file(&target).await.map_err(|e| {
                    AgentToolError::Message(format!("remove {}: {e}", target.display()))
                })?;
            }
            Err(_) => {
                // Already gone on disk — treat as success (idempotent), still clean overlay +
                // reload so the catalog drops it.
            }
        }

        // Forget any disabled-state overlay entry for this skill so a future reinstall of the
        // same name starts fresh.
        if let Err(e) = skills_state::remove_and_save(&self.base_dir, &input.name, source).await {
            tracing::warn!(
                skill = %input.name,
                error = %e,
                "failed to clear skills-state overlay entry after remove"
            );
        }

        let reload = harness
            .reload_skills_from_disk()
            .await
            .map_err(|e| AgentToolError::Message(format!("reload after remove: {e}")))?;

        // The removed skill must not survive in the reloaded catalog (no stale entry).
        let still_present = reload
            .skills
            .iter()
            .any(|s| s.name == input.name && s.source == SkillSource::User);

        let audit = json!({
            "op": "remove",
            "actor": "tool",
            "name": input.name,
            "source": "user",
            "target_path": target.display().to_string(),
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
                    "skill_control_plane audit write failed; removal itself succeeded"
                );
                None
            }
        };

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "removed skill '{}' (user). catalog now has {} skill(s).",
                input.name,
                reload.skills.len()
            ))],
            details: json!({
                "phase": "removed",
                "name": input.name,
                "source": "user",
                "target_path": target.display().to_string(),
                "still_present_after_reload": still_present,
                "total_skills_after": reload.skills.len(),
                "audit_entry_id": audit_entry_id,
            }),
            terminate: None,
        })
    }
}

/// Compute what to delete for a skill whose SKILL.md is `file_path`, given the skills root.
/// Returns the direct child of `skills_root` on the path (a `<name>/` dir or a root-level
/// `<x>.md` file), or `None` if `file_path` is not under `skills_root`. The returned path is
/// always `skills_root.join(<first component>)`, so it can never escape the root regardless of
/// what the skill record claims.
fn deletion_target(skills_root: &Path, file_path: &Path) -> Option<PathBuf> {
    let rel = file_path.strip_prefix(skills_root).ok()?;
    let first = rel.components().next()?;
    match first {
        std::path::Component::Normal(c) => Some(skills_root.join(c)),
        // Any non-normal leading component (`..`, root, prefix) means the path isn't a clean
        // child of the skills root — refuse.
        _ => None,
    }
}

fn parse_source(s: &str) -> Result<SkillSource, AgentToolError> {
    match s.to_ascii_lowercase().as_str() {
        "builtin" => Ok(SkillSource::Builtin),
        "user" => Ok(SkillSource::User),
        "project" => Ok(SkillSource::Project),
        _ => Err(AgentToolError::from(
            "invalid `source` (expected one of: builtin, user, project)",
        )),
    }
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "RemoveSkill".into(),
    description:
        "Delete a user-installed skill (from ~/.pie/skills/) and hot-reload the catalog. Only \
         user-installed skills can be removed — builtin skills are compiled into pie and \
         project skills belong to the repo; for those, disable instead via SetSkillState. \
         Two-phase: first call previews the target path; call again with `confirm: true` to \
         delete. Removing also clears any disabled-state overlay entry for the skill."
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
                "description": "Optional. Must be `user` if given — only user-installed skills are removable."
            },
            "confirm": {
                "type": "boolean",
                "default": false,
                "description": "When false (default) returns a preview; when true performs the deletion."
            }
        },
        "required": ["name"],
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

    /// Write a `<base>/skills/<name>/SKILL.md` on disk and return the absolute SKILL.md path.
    async fn write_user_skill(base: &Path, name: &str) -> String {
        let dir = base.join("skills").join(name);
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let content = format!("---\nname: {name}\ndescription: d\n---\nbody of {name}\n");
        let path = dir.join("SKILL.md");
        tokio::fs::write(&path, content).await.unwrap();
        path.to_string_lossy().to_string()
    }

    fn skill(name: &str, source: SkillSource, file_path: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "d".into(),
            file_path: file_path.into(),
            content: "body".into(),
            disable_model_invocation: false,
            source,
        }
    }

    /// Harness whose `reload_skills_from_disk` re-scans `<base>/skills` from disk (so a removed
    /// dir actually disappears from the reloaded catalog) and applies the overlay — mirroring
    /// the real main.rs reload closure.
    fn build(seed: Vec<Skill>, base: PathBuf) -> (Arc<AgentHarness>, SkillHarnessCell) {
        let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
        let session = Session::new(storage);
        let mut opts = AgentHarnessOptions::new(fake_model(), session);
        opts.skills = seed;
        let base_for_reload = base.clone();
        let loader: ReloadSkillsFn = Arc::new(move || {
            let base = base_for_reload.clone();
            Box::pin(async move {
                let env = pie_agent_core::NativeEnv::new(
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default(),
                );
                let dir = base.join("skills");
                let mut out = pie_agent_core::load_skills(
                    &env,
                    &[dir.to_string_lossy().as_ref()],
                    CancellationToken::new(),
                )
                .await;
                for s in out.skills.iter_mut() {
                    s.source = SkillSource::User;
                }
                let state = skills_state::load(&base).await;
                skills_state::apply(&state, &mut out.skills);
                out
            })
        });
        opts.reload_skills_fn = Some(loader);
        let harness = Arc::new(AgentHarness::new(opts));
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        assert!(cell.set(harness.clone()).is_ok());
        (harness, cell)
    }

    async fn exec(
        tool: &RemoveSkillTool,
        params: Value,
    ) -> Result<AgentToolResult, AgentToolError> {
        tool.execute("c1", params, CancellationToken::new(), None)
            .await
    }

    #[test]
    fn deletion_target_is_direct_child_of_skills_root() {
        let root = Path::new("/home/u/.pie/skills");
        // <name>/SKILL.md → remove the <name> dir.
        assert_eq!(
            deletion_target(root, Path::new("/home/u/.pie/skills/foo/SKILL.md")),
            Some(PathBuf::from("/home/u/.pie/skills/foo"))
        );
        // root-level <x>.md → remove the file.
        assert_eq!(
            deletion_target(root, Path::new("/home/u/.pie/skills/bar.md")),
            Some(PathBuf::from("/home/u/.pie/skills/bar.md"))
        );
        // outside the root → None (refused).
        assert_eq!(deletion_target(root, Path::new("/etc/passwd")), None);
    }

    #[tokio::test]
    async fn preview_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let fp = write_user_skill(dir.path(), "foo").await;
        let (_h, cell) = build(
            vec![skill("foo", SkillSource::User, &fp)],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());

        let res = exec(&tool, json!({"name": "foo"}))
            .await
            .expect("preview ok");
        assert_eq!(res.details["phase"], "preview");
        assert_eq!(res.details["source"], "user");
        // File still on disk.
        assert!(Path::new(&fp).exists(), "preview must not delete");
    }

    #[tokio::test]
    async fn confirm_deletes_and_reload_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let fp = write_user_skill(dir.path(), "foo").await;
        let (harness, cell) = build(
            vec![skill("foo", SkillSource::User, &fp)],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());

        let res = exec(&tool, json!({"name": "foo", "confirm": true}))
            .await
            .expect("remove ok");
        assert_eq!(res.details["phase"], "removed");
        assert_eq!(res.details["still_present_after_reload"], false);
        // Skill dir gone.
        assert!(!dir.path().join("skills").join("foo").exists());
        // Catalog no longer has it (reload re-scanned disk).
        assert!(
            !harness.skills().iter().any(|s| s.name == "foo"),
            "removed skill must not survive reload"
        );
    }

    #[tokio::test]
    async fn builtin_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (_h, cell) = build(
            vec![skill("kp", SkillSource::Builtin, "<builtin>/kp/SKILL.md")],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());
        let err = exec(&tool, json!({"name": "kp", "confirm": true}))
            .await
            .expect_err("builtin remove rejected");
        let AgentToolError::Message(m) = err else {
            panic!("typed error")
        };
        assert!(
            m.contains("builtin skill") && m.contains("disable"),
            "got: {m}"
        );
    }

    #[tokio::test]
    async fn project_cannot_be_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (_h, cell) = build(
            vec![skill(
                "p",
                SkillSource::Project,
                "/repo/.pie/skills/p/SKILL.md",
            )],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());
        let err = exec(&tool, json!({"name": "p", "confirm": true}))
            .await
            .expect_err("project remove rejected");
        let AgentToolError::Message(m) = err else {
            panic!("typed error")
        };
        assert!(
            m.contains("project skill") && m.contains("disable"),
            "got: {m}"
        );
    }

    #[tokio::test]
    async fn remove_clears_overlay_entry() {
        let dir = tempfile::tempdir().unwrap();
        let fp = write_user_skill(dir.path(), "foo").await;
        // Pre-existing disabled overlay entry for foo.
        skills_state::set_and_save(dir.path(), "foo", SkillSource::User, false)
            .await
            .unwrap();
        let (_h, cell) = build(
            vec![skill("foo", SkillSource::User, &fp)],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());

        exec(&tool, json!({"name": "foo", "confirm": true}))
            .await
            .expect("remove ok");

        // Overlay no longer carries the stale entry.
        let state = skills_state::load(dir.path()).await;
        assert!(
            state.lookup("foo", SkillSource::User).is_none(),
            "remove must clear the overlay entry so reinstall starts fresh"
        );
    }

    #[tokio::test]
    async fn writes_remove_audit() {
        let dir = tempfile::tempdir().unwrap();
        let fp = write_user_skill(dir.path(), "foo").await;
        let (harness, cell) = build(
            vec![skill("foo", SkillSource::User, &fp)],
            dir.path().into(),
        );
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());

        exec(&tool, json!({"name": "foo", "confirm": true}))
            .await
            .expect("remove ok");

        let entries = harness.session().entries().await.unwrap();
        let audit = entries.iter().find_map(|e| match e {
            pie_agent_core::SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "skill_control_plane" => data.clone(),
            _ => None,
        });
        let data = audit.expect("skill_control_plane audit written");
        assert_eq!(data["op"], "remove");
        assert_eq!(data["name"], "foo");
        assert_eq!(data["source"], "user");
        let s = serde_json::to_string(&data).unwrap();
        assert!(
            !s.contains("body of foo"),
            "audit must not contain skill body: {s}"
        );
    }

    #[tokio::test]
    async fn unknown_skill_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_h, cell) = build(vec![], dir.path().into());
        let tool = RemoveSkillTool::with_base_dir(cell, dir.path().into());
        let err = exec(&tool, json!({"name": "ghost", "confirm": true}))
            .await
            .expect_err("unknown skill errors");
        let AgentToolError::Message(m) = err else {
            panic!("typed error")
        };
        assert!(m.contains("no loaded skill named 'ghost'"));
    }
}
