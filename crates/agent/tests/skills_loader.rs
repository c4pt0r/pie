//! End-to-end skill discovery against a real tempdir. Validates the SKILL.md walking, YAML
//! frontmatter parsing, name/parent-dir matching, and the system-prompt block format.

use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, MemorySessionStorage, NativeEnv, Session, SessionStorage,
    SkillDiagnosticCode, format_skills_for_system_prompt, load_skills,
};
use std::sync::Arc;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn discovers_skill_with_matching_parent_dir() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let skill_dir = root.join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: tells you things\n---\nBody body.",
    )
    .unwrap();

    let env = NativeEnv::new(root.to_string_lossy().to_string());
    let out = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;

    assert!(
        out.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        out.diagnostics
    );
    assert_eq!(out.skills.len(), 1);
    let s = &out.skills[0];
    assert_eq!(s.name, "my-skill");
    assert_eq!(s.description, "tells you things");
    assert_eq!(s.content, "Body body.");
}

#[tokio::test]
async fn missing_description_emits_diagnostic_and_skips() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let skill_dir = root.join("nodesc");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "---\nname: nodesc\n---\nBody.").unwrap();

    let env = NativeEnv::new(root.to_string_lossy().to_string());
    let out = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
    assert!(
        out.skills.is_empty(),
        "skills without description should be skipped"
    );
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == SkillDiagnosticCode::InvalidMetadata
                && d.message.contains("description is required")),
        "expected invalid_metadata diagnostic; got {:?}",
        out.diagnostics
    );
}

#[tokio::test]
async fn name_must_match_parent_dir() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let skill_dir = root.join("real-name");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: different\ndescription: x\n---\nBody.",
    )
    .unwrap();

    let env = NativeEnv::new(root.to_string_lossy().to_string());
    let out = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
    // skill still loads (TS keeps it; only emits a warning), but a diagnostic flags the mismatch.
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.message.contains("does not match parent directory")),
        "expected name-mismatch diagnostic; got {:?}",
        out.diagnostics
    );
}

#[tokio::test]
async fn system_prompt_block_lists_each_skill() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    for name in ["alpha", "beta"] {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: it does {name}\n---\nbody"),
        )
        .unwrap();
    }
    let env = NativeEnv::new(root.to_string_lossy().to_string());
    let out = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
    assert_eq!(out.skills.len(), 2);
    let block = format_skills_for_system_prompt(&out.skills);
    assert!(block.starts_with("<skills>\n"));
    assert!(block.contains("- name: alpha"));
    assert!(block.contains("- name: beta"));
    assert!(block.ends_with("</skills>"));
}

#[tokio::test]
async fn disable_model_invocation_accepts_both_kebab_and_snake() {
    // Issue #25 PR A locks in `disable_model_invocation=true` as the contract refused by the
    // `Skill` builtin tool. The frontmatter accepts both `disable-model-invocation` (the
    // existing kebab-case key) AND `disable_model_invocation` (the snake-case spelling used
    // in the issue body, the PR description, and the `Skill` tool's error message). Users
    // following either spelling must end up with `Skill.disable_model_invocation = true`.
    for (label, frontmatter_key) in [
        ("kebab", "disable-model-invocation"),
        ("snake", "disable_model_invocation"),
    ] {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let skill_dir = root.join("locked");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: locked\ndescription: refuses model invocation\n{frontmatter_key}: true\n---\nBody body."
            ),
        )
        .unwrap();
        let env = NativeEnv::new(root.to_string_lossy().to_string());
        let out = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
        assert!(
            out.diagnostics.is_empty(),
            "[{label}] unexpected diagnostics: {:?}",
            out.diagnostics
        );
        assert_eq!(out.skills.len(), 1, "[{label}] expected one skill");
        assert!(
            out.skills[0].disable_model_invocation,
            "[{label}] frontmatter key {frontmatter_key} must set disable_model_invocation=true"
        );
    }
}

/// Issue #25 PR C: prove the `<skills>` block in the system prompt is fully reconstructable
/// from disk. A user who closes pie and resumes (or restarts the daemon) must see the same
/// skill catalog, regardless of when the harness was built. The block is a pure function of
/// `format_skills_for_system_prompt(loaded_skills)`, so this asserts that two independent
/// `load_skills` runs against the same tempdir produce byte-identical output.
#[tokio::test]
async fn resume_rebuilds_skill_block_byte_identical_from_same_directory() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    // Write three skills into the tempdir so the block has non-trivial ordering.
    for (name, description, body, disabled) in [
        ("alpha", "first skill", "alpha body", false),
        ("beta", "second skill", "beta body", true),
        ("gamma", "third skill", "gamma body", false),
    ] {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut frontmatter = format!("name: {name}\ndescription: {description}\n");
        if disabled {
            frontmatter.push_str("disable_model_invocation: true\n");
        }
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .unwrap();
    }

    let env = NativeEnv::new(root.to_string_lossy().to_string());

    // First load (initial session start).
    let first = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
    assert!(
        first.diagnostics.is_empty(),
        "unexpected diagnostics on first load: {:?}",
        first.diagnostics
    );
    let first_block = format_skills_for_system_prompt(&first.skills);

    // Second load (simulates `--resume` / daemon restart against the same directory).
    let second = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
    assert!(
        second.diagnostics.is_empty(),
        "unexpected diagnostics on resume load: {:?}",
        second.diagnostics
    );
    let second_block = format_skills_for_system_prompt(&second.skills);

    // Byte-identical reconstruction is the actual acceptance: any divergence (ordering
    // jitter, frontmatter rewrite, etc.) would cause the resumed session to see a different
    // system prompt than the original, breaking conversation determinism for the LLM.
    assert_eq!(
        first_block, second_block,
        "skill block diverged across reloads"
    );

    // Sanity: the block contains all three skills. The byte-identical check above already
    // proves the relative ordering is *stable* across reloads (whatever order the loader
    // chose, both runs produced it); we don't pin a specific alphabetical/reverse ordering
    // here because the contract is "stable", not "alphabetical".
    assert!(first_block.starts_with("<skills>\n"));
    assert!(first_block.contains("- name: alpha\n"));
    assert!(first_block.contains("- name: beta\n"));
    assert!(first_block.contains("- name: gamma\n"));
    assert!(first_block.ends_with("</skills>"));
    // Stability: both loads produced an identical block, so the *relative* positions of
    // alpha/beta/gamma in `first_block` equal those in `second_block` by definition (the
    // strings are equal). Make that explicit so a future regression in the byte-identical
    // assertion above does not also silently hide an order-stability regression.
    let positions = |block: &str| {
        (
            block.find("- name: alpha\n").unwrap(),
            block.find("- name: beta\n").unwrap(),
            block.find("- name: gamma\n").unwrap(),
        )
    };
    assert_eq!(
        positions(&first_block),
        positions(&second_block),
        "relative skill positions diverged across reloads"
    );

    // disable_model_invocation does not affect the system-prompt block (per issue #25 v3:
    // the flag is enforced at Skill-tool execute time, not in the catalog rendering). The
    // block must still list `beta` even though it's disabled.
    assert!(
        second
            .skills
            .iter()
            .any(|s| s.name == "beta" && s.disable_model_invocation),
        "loaded skills must include the disabled beta with disable_model_invocation=true"
    );
}

/// Issue #25 PR C harness-level acceptance: two `AgentHarness` instances built against the
/// same skills directory and the same non-empty `base system_prompt` must expose
/// byte-identical `system_prompt()`. This is the actual `--resume` scenario — `AgentHarness`
/// composes `base + format_skills_for_system_prompt(skills)`, so any drift in either path
/// (skill load ordering, formatter rendering, or `build_system_prompt` concatenation) would
/// surface as a divergent system prompt and break LLM determinism on resume.
#[tokio::test]
async fn resume_rebuilds_harness_system_prompt_byte_identical() {
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

    let dir = tempdir().unwrap();
    let root = dir.path();
    for (name, description, body, disabled) in [
        ("alpha", "first skill", "alpha body", false),
        ("beta", "second skill", "beta body", true),
        ("gamma", "third skill", "gamma body", false),
    ] {
        let skill_dir = root.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let mut frontmatter = format!("name: {name}\ndescription: {description}\n");
        if disabled {
            frontmatter.push_str("disable_model_invocation: true\n");
        }
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{frontmatter}---\n{body}"),
        )
        .unwrap();
    }

    let env = NativeEnv::new(root.to_string_lossy().to_string());

    let base_system_prompt =
        "You are a careful coding assistant. Use the tools you have, never invent state.";

    let build_harness_system_prompt = || async {
        let load = load_skills(&env, &[root.to_str().unwrap()], CancellationToken::new()).await;
        assert!(
            load.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            load.diagnostics
        );
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage as Arc<dyn SessionStorage>);
        let mut opts = AgentHarnessOptions::new(faux_model(), session);
        opts.system_prompt = base_system_prompt.to_string();
        opts.skills = load.skills;
        let harness = AgentHarness::new(opts);
        harness.system_prompt()
    };

    let first_prompt = build_harness_system_prompt().await;
    let second_prompt = build_harness_system_prompt().await;

    // The actual --resume acceptance: harness-level system_prompt is byte-identical across
    // two independent constructions from the same skills directory.
    assert_eq!(
        first_prompt, second_prompt,
        "AgentHarness::system_prompt diverged across reloads — would break LLM determinism on resume"
    );

    // Covers the base + `<skills>` concatenation path inside `build_system_prompt`.
    assert!(
        first_prompt.starts_with(base_system_prompt),
        "system prompt must start with the supplied base prompt"
    );
    assert!(
        first_prompt.contains("<skills>"),
        "system prompt must include the skills catalog block"
    );
    assert!(
        first_prompt.contains("- name: alpha\n"),
        "system prompt must list alpha"
    );
    assert!(
        first_prompt.contains("- name: beta\n"),
        "system prompt must list beta even though disable_model_invocation=true"
    );
    assert!(
        first_prompt.ends_with("</skills>"),
        "system prompt must end with the closing skills tag"
    );
}
