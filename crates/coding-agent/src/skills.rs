//! Skills discovery for the CLI.
//!
//! Loads markdown skills from project (`<cwd>/.pie/skills/`) and user (`~/.pie/skills/`) roots
//! via `pie-agent-core`'s `harness::skills` loader. Project wins on name collision so a repo
//! can override a user-wide skill of the same name.

use std::path::{Path, PathBuf};

use pie_agent_core::{NativeEnv, Skill, SkillDiagnostic, SkillSource, load_skills};
use tokio_util::sync::CancellationToken;

use crate::config::base_dir;

/// Returns (project_root, user_root) in the order they should be consulted.
///
/// Project precedence means project is loaded *second* and overrides a same-name skill from
/// user-global. (See `dedupe_project_wins` for the actual policy.)
pub fn skills_dirs(cwd: &Path) -> (PathBuf, PathBuf) {
    let project = cwd.join(".pie").join("skills");
    let user = base_dir().join("skills");
    (project, user)
}

/// Final loaded skills plus any diagnostics from the walk. The CLI surfaces a summary line
/// from the diagnostics (count + first message) at startup if non-empty.
pub struct LoadedSkills {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Load skills from both roots, with project-local overriding user-global on name collision.
/// Missing directories are silently skipped — most users won't have either initially.
pub async fn load_all(cwd: &Path) -> LoadedSkills {
    let (project, user) = skills_dirs(cwd);
    let env = NativeEnv::new(cwd.to_string_lossy().to_string());
    let cancel = CancellationToken::new();

    let mut combined = Vec::<Skill>::new();
    let mut diagnostics = Vec::<SkillDiagnostic>::new();

    // Load user first so project entries (loaded second) can shadow. The runtime walker
    // leaves `source` at its default; we set the correct source per root here, where the
    // project-vs-user distinction is actually known.
    for (dir, source) in [(user, SkillSource::User), (project, SkillSource::Project)] {
        let s = dir.to_string_lossy().to_string();
        let out = load_skills(&env, &[s.as_str()], cancel.clone()).await;
        diagnostics.extend(out.diagnostics);
        for mut skill in out.skills {
            skill.source = source;
            dedupe_project_wins(&mut combined, skill);
        }
    }

    LoadedSkills {
        skills: combined,
        diagnostics,
    }
}

/// Insert `skill` into `combined`, replacing any existing entry with the same name. Since we
/// load user first and project second, a later (project-side) skill displaces the earlier one.
fn dedupe_project_wins(combined: &mut Vec<Skill>, skill: Skill) {
    if let Some(i) = combined.iter().position(|s| s.name == skill.name) {
        combined[i] = skill;
    } else {
        combined.push(skill);
    }
}
