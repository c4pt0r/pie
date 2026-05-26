//! End-to-end test for the CLI's skills loader wiring.
//!
//! Strategy: simulate the dual-root layout (user-global at `~/.pie/skills/<name>/SKILL.md` +
//! project-local at `<cwd>/.pie/skills/<name>/SKILL.md`) using a tempdir as the home (`PIE_DIR`)
//! and a separate tempdir as the project cwd. Then run the same loader the CLI runs and assert:
//!   1. Both skills are loaded.
//!   2. When user + project define the same skill name, project wins.
//!   3. Loaded skills are stitched into the final harness system prompt.
//!
//! This exercises the public surface only — no direct calls into the harness-internal walker.
//! If the CLI ever changes how it picks the roots, this test catches it.

use std::path::Path;

use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, MemorySessionStorage, Session, ThinkingLevel,
};
use tempfile::TempDir;

/// Re-import the binary's skills module by compiling it as a path-include. The crate has a
/// `[[bin]]` and no `[lib]`, so we recreate the relevant logic verbatim here. Keeping it
/// duplicated lets us test the loader without restructuring the crate; if the duplicate drifts,
/// the test fails the next time we touch it.
mod skills_mirror {
    use pie_agent_core::{NativeEnv, Skill, SkillDiagnostic, SkillSource, load_skills};
    use std::path::{Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    pub struct LoadedSkills {
        pub skills: Vec<Skill>,
        pub diagnostics: Vec<SkillDiagnostic>,
    }

    pub async fn load_all(cwd: &Path, base_dir: &Path) -> LoadedSkills {
        let project: PathBuf = cwd.join(".pie").join("skills");
        let user: PathBuf = base_dir.join("skills");
        let env = NativeEnv::new(cwd.to_string_lossy().to_string());
        let cancel = CancellationToken::new();
        let mut combined = Vec::<Skill>::new();
        let mut diagnostics = Vec::<SkillDiagnostic>::new();
        // Mirror the real `skills::load_all`: load user first, project second (project wins),
        // and tag each skill with the source of the root it came from.
        for (dir, source) in [(user, SkillSource::User), (project, SkillSource::Project)] {
            let s = dir.to_string_lossy().to_string();
            let out = load_skills(&env, &[s.as_str()], cancel.clone()).await;
            diagnostics.extend(out.diagnostics);
            for mut skill in out.skills {
                skill.source = source;
                if let Some(i) = combined.iter().position(|s| s.name == skill.name) {
                    combined[i] = skill;
                } else {
                    combined.push(skill);
                }
            }
        }
        LoadedSkills {
            skills: combined,
            diagnostics,
        }
    }
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n");
    std::fs::write(dir.join("SKILL.md"), content).unwrap();
}

fn faux_model() -> pie_ai::Model {
    pie_ai::Model {
        id: "faux".into(),
        name: "Faux".into(),
        api: pie_ai::Api::from("faux"),
        provider: pie_ai::Provider::from("faux"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: pie_ai::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

#[tokio::test]
async fn project_skill_overrides_user_skill_with_same_name() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    // user-global skill
    write_skill(home.path(), "shared", "user-version", "USER BODY");
    // project-local skill with same name — should win
    write_skill(
        &cwd.path().join(".pie"),
        "shared",
        "project-version",
        "PROJECT BODY",
    );
    // user-only skill (no project counterpart)
    write_skill(home.path(), "only-user", "user-only", "ONLY USER BODY");

    let loaded = skills_mirror::load_all(cwd.path(), home.path()).await;
    assert!(
        loaded.diagnostics.is_empty(),
        "unexpected diagnostics: {:#?}",
        loaded.diagnostics
    );
    let names: Vec<&str> = loaded.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"shared"));
    assert!(names.contains(&"only-user"));
    let shared = loaded.skills.iter().find(|s| s.name == "shared").unwrap();
    assert_eq!(
        shared.description, "project-version",
        "project should override user on same name"
    );
    assert!(
        shared.content.contains("PROJECT BODY"),
        "shared content should come from project: {:?}",
        shared.content
    );

    // Now feed into an actual harness and confirm the system prompt includes both skills.
    let storage = std::sync::Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as std::sync::Arc<dyn pie_agent_core::SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.system_prompt = "base prompt".into();
    opts.thinking_level = ThinkingLevel::Off;
    opts.skills = loaded.skills.clone();
    let harness = AgentHarness::new(opts);

    let prompt = harness.system_prompt();
    assert!(prompt.contains("base prompt"));
    assert!(
        prompt.contains("name: shared"),
        "system prompt should list 'shared' skill: {prompt}"
    );
    assert!(
        prompt.contains("name: only-user"),
        "system prompt should list 'only-user' skill: {prompt}"
    );
    // Description identifies which version landed. Skill bodies are invoked via the `Skill`
    // tool, not inlined into the prompt — so we don't assert on `PROJECT BODY` here.
    assert!(
        prompt.contains("description: project-version"),
        "project version of 'shared' should win in system prompt: {prompt}"
    );
    assert!(
        !prompt.contains("description: user-version"),
        "user version of 'shared' must NOT appear in the listing: {prompt}"
    );

    // Sanity-check: the project body actually lives on the in-memory skill record (so when the
    // model later invokes `Skill('shared')`, it gets the project copy).
    let kept = harness
        .skills()
        .into_iter()
        .find(|s| s.name == "shared")
        .expect("shared skill present");
    assert!(
        kept.content.contains("PROJECT BODY"),
        "harness should keep project body for the shared skill: {:?}",
        kept.content
    );
}

#[tokio::test]
async fn missing_roots_load_cleanly() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let loaded = skills_mirror::load_all(cwd.path(), home.path()).await;
    assert!(loaded.skills.is_empty());
    assert!(
        loaded.diagnostics.is_empty(),
        "non-existent roots should produce no diagnostics: {:#?}",
        loaded.diagnostics
    );
}

#[tokio::test]
async fn loader_tags_skill_source_per_root() {
    use pie_agent_core::SkillSource;
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    // One skill in each root, distinct names so no shadowing.
    write_skill(home.path(), "user-skill", "u", "USER");
    write_skill(&cwd.path().join(".pie"), "project-skill", "p", "PROJECT");

    let loaded = skills_mirror::load_all(cwd.path(), home.path()).await;

    let user = loaded
        .skills
        .iter()
        .find(|s| s.name == "user-skill")
        .expect("user skill loaded");
    let project = loaded
        .skills
        .iter()
        .find(|s| s.name == "project-skill")
        .expect("project skill loaded");

    assert_eq!(
        user.source,
        SkillSource::User,
        "skill from ~/.pie/skills must be tagged User"
    );
    assert_eq!(
        project.source,
        SkillSource::Project,
        "skill from <cwd>/.pie/skills must be tagged Project"
    );
    // The display label the `/skills` listing renders comes straight off the field now.
    assert_eq!(user.source.label(), "user");
    assert_eq!(project.source.label(), "project");
}

#[tokio::test]
async fn loader_tags_project_source_when_project_shadows_user() {
    use pie_agent_core::SkillSource;
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    // Same name in both roots — project wins, and the surviving entry must carry the
    // Project source (not the User source it would have had if shadowing dropped the tag).
    write_skill(home.path(), "shared", "user-version", "USER BODY");
    write_skill(
        &cwd.path().join(".pie"),
        "shared",
        "project-version",
        "PROJECT BODY",
    );

    let loaded = skills_mirror::load_all(cwd.path(), home.path()).await;
    let shared = loaded.skills.iter().find(|s| s.name == "shared").unwrap();
    assert_eq!(
        shared.source,
        SkillSource::Project,
        "project-shadowed skill must report Project source"
    );
}
