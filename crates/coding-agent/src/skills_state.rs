//! Runtime skill enable/disable overlay, persisted at `~/.pie/skills-state.json`.
//!
//! Why an overlay instead of editing `SKILL.md`: a skill's `SKILL.md` is the author's
//! read-only source of truth (often vendored or project-shared). Flipping
//! `disable_model_invocation` by rewriting that file would dirty vendored/project content
//! and make updates harder. Instead, runtime enable/disable is recorded as local control-
//! plane *state* keyed by `{source, name}`, applied at load time. The user's SKILL.md stays
//! pristine; disabling is reversible; and an audit trail can record who changed what.
//!
//! Boundary (locked with Provider/Auth + QA on the #ux skill-lifecycle thread): the overlay
//! stores ONLY `{name, source, enabled}` — never the skill body, never a source URL/token,
//! never any credential.
//!
//! Source-aware keying: entries are keyed by `{source, name}`, not bare name. A skill named
//! `foo` installed in `~/.pie/skills` (User) and a project `foo` (Project) are distinct
//! overlay entries, so disabling one never silently disables the other after a precedence
//! change. `apply` matches against the *active* (post-dedup) source the loader assigned.

use std::path::{Path, PathBuf};

use pie_agent_core::{Skill, SkillSource};
use serde::{Deserialize, Serialize};

/// Filename under the pie base dir (`~/.pie/`).
pub const STATE_FILE: &str = "skills-state.json";

/// One explicit enable/disable override for a `{source, name}` skill. Presence of an entry
/// means the user made an explicit runtime choice that overrides the skill's frontmatter
/// `disable_model_invocation` default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStateEntry {
    pub name: String,
    pub source: SkillSource,
    /// `true` = explicitly enabled (overrides a frontmatter disable); `false` = disabled.
    pub enabled: bool,
}

/// The persisted overlay. Forward-compatible: unknown fields are ignored, missing file = empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsState {
    #[serde(default)]
    pub overrides: Vec<SkillStateEntry>,
}

impl SkillsState {
    /// Look up the explicit override for `{source, name}`, if any.
    pub fn lookup(&self, name: &str, source: SkillSource) -> Option<&SkillStateEntry> {
        self.overrides
            .iter()
            .find(|e| e.name == name && e.source == source)
    }

    /// Upsert an explicit `{source, name} -> enabled` override.
    pub fn set(&mut self, name: &str, source: SkillSource, enabled: bool) {
        if let Some(e) = self
            .overrides
            .iter_mut()
            .find(|e| e.name == name && e.source == source)
        {
            e.enabled = enabled;
        } else {
            self.overrides.push(SkillStateEntry {
                name: name.to_string(),
                source,
                enabled,
            });
        }
    }
}

/// Absolute path to the overlay file under `base_dir` (`~/.pie/`).
pub fn state_path(base_dir: &Path) -> PathBuf {
    base_dir.join(STATE_FILE)
}

/// Load the overlay. A missing file is an empty overlay; a malformed file is treated as empty
/// (the disable/enable state simply isn't applied) rather than failing skill loading entirely.
pub async fn load(base_dir: &Path) -> SkillsState {
    let path = state_path(base_dir);
    match tokio::fs::read_to_string(&path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "malformed skills-state.json; ignoring overlay");
            SkillsState::default()
        }),
        Err(_) => SkillsState::default(),
    }
}

/// Atomically persist the overlay (tempfile + rename within the same dir).
pub async fn save(base_dir: &Path, state: &SkillsState) -> std::io::Result<()> {
    tokio::fs::create_dir_all(base_dir).await?;
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = base_dir.join(format!(
        ".{STATE_FILE}.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    tokio::fs::write(&tmp, json).await?;
    if let Err(e) = tokio::fs::rename(&tmp, state_path(base_dir)).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// Apply the overlay to a freshly-loaded skill catalog: for each skill that has an explicit
/// `{source, name}` override, set `disable_model_invocation = !enabled`. Skills with no
/// override keep their frontmatter value. Called both at startup and on
/// `reload_skills_from_disk` so a disabled skill stays disabled across reloads.
pub fn apply(state: &SkillsState, skills: &mut [Skill]) {
    for skill in skills.iter_mut() {
        if let Some(entry) = state.lookup(&skill.name, skill.source) {
            skill.disable_model_invocation = !entry.enabled;
        }
    }
}

/// Convenience: load → set → save in one call, returning the updated overlay. Used by the
/// `SetSkillState` tool and the `/skills enable|disable` slash command (S-B) so both share
/// one persistence path.
pub async fn set_and_save(
    base_dir: &Path,
    name: &str,
    source: SkillSource,
    enabled: bool,
) -> std::io::Result<SkillsState> {
    let mut state = load(base_dir).await;
    state.set(name, source, enabled);
    save(base_dir, &state).await?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, source: SkillSource, frontmatter_disabled: bool) -> Skill {
        Skill {
            name: name.into(),
            description: "d".into(),
            file_path: format!("/tmp/{name}/SKILL.md"),
            content: "body".into(),
            disable_model_invocation: frontmatter_disabled,
            source,
        }
    }

    #[test]
    fn apply_disables_matching_source_name() {
        let mut state = SkillsState::default();
        state.set("foo", SkillSource::User, false); // explicitly disabled
        let mut skills = vec![skill("foo", SkillSource::User, false)];
        apply(&state, &mut skills);
        assert!(
            skills[0].disable_model_invocation,
            "overlay disable applies"
        );
    }

    #[test]
    fn apply_enable_overrides_frontmatter_disable() {
        let mut state = SkillsState::default();
        state.set("foo", SkillSource::User, true); // explicitly enabled
        let mut skills = vec![skill("foo", SkillSource::User, true)]; // frontmatter disabled
        apply(&state, &mut skills);
        assert!(
            !skills[0].disable_model_invocation,
            "explicit enable overrides frontmatter disable"
        );
    }

    #[test]
    fn apply_is_source_aware() {
        // Disable only the User `foo`; a Project `foo` must be untouched.
        let mut state = SkillsState::default();
        state.set("foo", SkillSource::User, false);
        let mut skills = vec![
            skill("foo", SkillSource::User, false),
            skill("foo", SkillSource::Project, false),
        ];
        apply(&state, &mut skills);
        assert!(skills[0].disable_model_invocation, "user foo disabled");
        assert!(
            !skills[1].disable_model_invocation,
            "project foo must not be affected by a user-scoped disable"
        );
    }

    #[test]
    fn no_override_keeps_frontmatter_value() {
        let state = SkillsState::default();
        let mut skills = vec![
            skill("a", SkillSource::User, false),
            skill("b", SkillSource::User, true),
        ];
        apply(&state, &mut skills);
        assert!(!skills[0].disable_model_invocation);
        assert!(
            skills[1].disable_model_invocation,
            "frontmatter disable preserved"
        );
    }

    #[test]
    fn set_upserts_not_duplicates() {
        let mut state = SkillsState::default();
        state.set("foo", SkillSource::User, false);
        state.set("foo", SkillSource::User, true);
        assert_eq!(state.overrides.len(), 1, "same {{source,name}} upserts");
        assert!(state.overrides[0].enabled);
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = SkillsState::default();
        state.set("foo", SkillSource::Project, false);
        state.set("bar", SkillSource::User, true);
        save(dir.path(), &state).await.unwrap();

        let loaded = load(dir.path()).await;
        assert_eq!(loaded.overrides.len(), 2);
        assert_eq!(
            loaded
                .lookup("foo", SkillSource::Project)
                .map(|e| e.enabled),
            Some(false)
        );
        assert_eq!(
            loaded.lookup("bar", SkillSource::User).map(|e| e.enabled),
            Some(true)
        );
        // No leftover tempfile.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().into_string().unwrap_or_default());
        }
        assert_eq!(names, vec![STATE_FILE.to_string()]);
    }

    #[tokio::test]
    async fn missing_file_is_empty_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load(dir.path()).await;
        assert!(loaded.overrides.is_empty());
    }

    #[tokio::test]
    async fn malformed_file_is_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(state_path(dir.path()), "{ not valid json")
            .await
            .unwrap();
        let loaded = load(dir.path()).await;
        assert!(
            loaded.overrides.is_empty(),
            "malformed overlay must not break skill loading"
        );
    }

    #[tokio::test]
    async fn set_and_save_persists() {
        let dir = tempfile::tempdir().unwrap();
        let state = set_and_save(dir.path(), "foo", SkillSource::User, false)
            .await
            .unwrap();
        assert_eq!(state.overrides.len(), 1);
        let reloaded = load(dir.path()).await;
        assert_eq!(
            reloaded.lookup("foo", SkillSource::User).map(|e| e.enabled),
            Some(false)
        );
    }
}
