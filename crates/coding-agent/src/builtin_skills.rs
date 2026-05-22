//! Built-in skill catalog.
//!
//! Bundles a small, curated set of skills into the `pie` binary so users can opt them in
//! without manually checking out a skill repo into `~/.pie/skills/`. This is the **lowest
//! precedence** skill source — any user (`~/.pie/skills/`) or project (`<cwd>/.pie/skills/`)
//! skill of the same name shadows the built-in version, same as the existing user/project
//! precedence in [`crate::skills::load_all`].
//!
//! **Default behavior is OFF**: a built-in skill is included in the harness skill catalog
//! only when the user explicitly enables it via:
//!
//! - the `--builtin-skill <name>` CLI flag (repeatable, one-time enable for that run)
//! - `~/.pie/config.toml`: `[builtin_skills] enabled = [...]` (persistent enable)
//!
//! Both inputs are unioned + de-duplicated. Unknown names from the CLI flag are a hard error
//! (the user typed a name and we cannot honor it). Unknown names from the config file are a
//! soft startup diagnostic (the config may have drifted from the binary's bundled set, but we
//! do not lock the user out — known names still take effect, unknown names are simply
//! skipped). Either way, an unknown name **never** silently enables anything.
//!
//! See c4pt0r/pie#32 for the spec.

use pie_agent_core::Skill;
use std::collections::BTreeSet;

/// Raw markdown of each built-in skill, vendored verbatim under
/// `crates/coding-agent/skills/<name>/SKILL.md` so the upstream sync path stays a plain
/// file copy. The frontmatter is preserved; `content_after_frontmatter` strips it at runtime.
struct BuiltinSpec {
    /// Stable lowercase-kebab name. Must match the frontmatter `name:` value.
    name: &'static str,
    /// Short description shown in `/skills` and the system-prompt catalog. Must match the
    /// upstream frontmatter `description:` value so the catalog text matches what users see
    /// in the source repo.
    description: &'static str,
    /// Full SKILL.md content including frontmatter. Bundled at compile time.
    raw_markdown: &'static str,
}

/// All built-in skills bundled with this `pie` build. Adding a new one means: vendor the
/// SKILL.md under `crates/coding-agent/skills/<name>/`, then add an entry here.
const BUILTINS: &[BuiltinSpec] = &[BuiltinSpec {
    name: "karpathy-guidelines",
    description: "Behavioral guidelines to reduce common LLM coding mistakes. Use when writing, reviewing, or refactoring code to avoid overcomplication, make surgical changes, surface assumptions, and define verifiable success criteria.",
    raw_markdown: include_str!("../skills/karpathy-guidelines/SKILL.md"),
}];

/// List every built-in name in stable alphabetical order. Used by error/diagnostic messages
/// so the `Available: ...` list is reproducible across runs.
pub fn available_builtin_names() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = BUILTINS.iter().map(|b| b.name).collect();
    out.sort_unstable();
    out
}

/// Outcome of resolving the user's requested set of built-in skills.
#[derive(Debug)]
pub struct ResolvedBuiltins {
    /// Skills to fold into the harness skill catalog. Empty when no built-in was enabled.
    pub skills: Vec<Skill>,
    /// Soft diagnostic strings to print at startup (e.g. unknown names found in
    /// `~/.pie/config.toml`). Each string is a complete user-readable line; the caller emits
    /// them however it normally surfaces non-fatal startup notices.
    pub diagnostics: Vec<String>,
}

/// Build the union of CLI-requested + config-requested built-in skills.
///
/// `cli_requested` is treated as authoritative: an unknown name is an error returned to the
/// caller (the CLI surface is expected to hard-fail with a non-zero exit). `config_requested`
/// is treated permissively: unknown names produce a diagnostic line but do not fail startup.
/// Known names from either source are unioned and de-duplicated; the same name appearing in
/// both, or twice in the CLI list, still produces exactly one catalog entry.
pub fn resolve_builtins(
    cli_requested: &[String],
    config_requested: &[String],
) -> Result<ResolvedBuiltins, UnknownBuiltinError> {
    // CLI path: hard-fail on any unknown name. We collect all unknowns and report them
    // together so the user sees the full list, not a one-at-a-time game of whack-a-mole.
    let known: BTreeSet<&'static str> = available_builtin_names().into_iter().collect();
    let mut unknown_cli: Vec<String> = Vec::new();
    for name in cli_requested {
        if !known.contains(name.as_str()) {
            unknown_cli.push(name.clone());
        }
    }
    if !unknown_cli.is_empty() {
        unknown_cli.sort();
        unknown_cli.dedup();
        return Err(UnknownBuiltinError {
            unknown: unknown_cli,
            available: available_builtin_names()
                .into_iter()
                .map(str::to_string)
                .collect(),
        });
    }

    // Config path: collect unknowns into a diagnostic, but do not block startup.
    let mut diagnostics: Vec<String> = Vec::new();
    let mut unknown_config: Vec<String> = Vec::new();
    for name in config_requested {
        if !known.contains(name.as_str()) {
            unknown_config.push(name.clone());
        }
    }
    if !unknown_config.is_empty() {
        unknown_config.sort();
        unknown_config.dedup();
        diagnostics.push(format!(
            "config: ignoring unknown built-in skill(s) in `[builtin_skills] enabled`: {}. Available: {}.",
            unknown_config.join(", "),
            available_builtin_names().join(", ")
        ));
    }

    // Union + dedup the known names from both sources.
    let mut enabled: BTreeSet<&'static str> = BTreeSet::new();
    for name in cli_requested.iter().chain(config_requested.iter()) {
        if let Some(known_name) = known.get(name.as_str()) {
            enabled.insert(known_name);
        }
    }

    // Build Skill structs in stable alphabetical order so the system prompt catalog is
    // reproducible across runs (matches the existing `format_skills_for_system_prompt` sort).
    let mut skills: Vec<Skill> = Vec::with_capacity(enabled.len());
    for name in enabled {
        let spec = BUILTINS
            .iter()
            .find(|b| b.name == name)
            .expect("name validated against known set above");
        skills.push(spec_to_skill(spec));
    }

    Ok(ResolvedBuiltins {
        skills,
        diagnostics,
    })
}

fn spec_to_skill(spec: &BuiltinSpec) -> Skill {
    Skill {
        name: spec.name.to_string(),
        description: spec.description.to_string(),
        // Synthetic path used in `/skills` listings and audit. Matches the format chosen for
        // built-in tier so users can tell where a skill came from at a glance.
        file_path: format!("<builtin>/{}/SKILL.md", spec.name),
        content: strip_frontmatter(spec.raw_markdown).to_string(),
        disable_model_invocation: false,
    }
}

/// Return the body of a SKILL.md after stripping the leading YAML frontmatter block, if any.
/// Mirrors the behavior the on-disk loader applies to a real SKILL.md.
fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start_matches('\u{feff}');
    let Some(without_open) = trimmed.strip_prefix("---") else {
        return content;
    };
    // Allow the opening `---` to be followed by either a newline (most files) or any
    // whitespace before the newline (be lenient — same posture as the loader's parser).
    let after_open = match without_open.find('\n') {
        Some(i) => &without_open[i + 1..],
        None => return content,
    };
    // Find the closing `---` on its own line.
    let mut search_from = 0usize;
    while let Some(pos) = after_open[search_from..].find("\n---") {
        let absolute = search_from + pos + 1; // skip the leading '\n'
        let after_close = &after_open[absolute + 3..];
        // Closing line ends with newline or EOF.
        if let Some(rest) = after_close.strip_prefix('\n') {
            return rest.trim_start_matches('\n');
        }
        if after_close.is_empty() {
            return "";
        }
        // The "---" we found has trailing text; keep scanning.
        search_from = absolute + 3;
    }
    // No closing marker — return the original content rather than guess.
    content
}

/// Merge the resolved built-in skills with the user/project skills the existing dual-root
/// loader returned. Same-name precedence is **user/project wins over built-in** (built-in is
/// the lowest tier per #32). The returned `Vec<Skill>` preserves built-in skills first, then
/// any user/project skills not already shadowing a built-in. Repeated names within
/// `user_project` follow the loader's existing project-over-user policy and arrive here
/// already collapsed.
///
/// This is extracted out of `main.rs` so the wiring path can be unit-tested without spinning
/// up the full binary (per @CLI-TUI-Dev-Lead's review on PR #34).
pub fn merge_with_user_project(mut builtins: Vec<Skill>, user_project: &[Skill]) -> Vec<Skill> {
    for skill in user_project.iter() {
        if let Some(slot) = builtins.iter_mut().find(|s| s.name == skill.name) {
            // User / project skill of the same name shadows the built-in.
            *slot = skill.clone();
        } else {
            builtins.push(skill.clone());
        }
    }
    builtins
}

/// Parse the contents of `~/.pie/config.toml` and extract the
/// `[builtin_skills] enabled = [...]` list. Missing section / missing key / parse failure all
/// degrade to an empty list — the soft fail-closed posture from #32: the caller treats
/// unknown names as a startup diagnostic, but a malformed config never prevents `pie` from
/// running at all.
///
/// Extracted from `main.rs` so the parser can be unit-tested directly.
pub fn parse_builtin_skills_config(toml_text: &str) -> Vec<String> {
    let Ok(parsed) = toml::from_str::<ConfigFile>(toml_text) else {
        return Vec::new();
    };
    parsed.builtin_skills.map(|s| s.enabled).unwrap_or_default()
}

#[derive(Default, serde::Deserialize)]
struct ConfigFile {
    builtin_skills: Option<ConfigSection>,
}

#[derive(Default, serde::Deserialize)]
struct ConfigSection {
    #[serde(default)]
    enabled: Vec<String>,
}

/// Error returned when the CLI enabled a built-in skill name that this binary does not
/// recognise. The caller is expected to print the message and exit with a non-zero status
/// (hard fail, per #32's CLI-side acceptance).
#[derive(Debug)]
pub struct UnknownBuiltinError {
    pub unknown: Vec<String>,
    pub available: Vec<String>,
}

impl std::fmt::Display for UnknownBuiltinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown built-in skill(s) requested via --builtin-skill: {}. Available: {}.",
            self.unknown.join(", "),
            self.available.join(", ")
        )
    }
}

impl std::error::Error for UnknownBuiltinError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_names_is_sorted_and_contains_karpathy() {
        let names = available_builtin_names();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(
            names, sorted,
            "available names must be alphabetically sorted"
        );
        assert!(names.contains(&"karpathy-guidelines"));
    }

    #[test]
    fn no_request_returns_empty_no_diagnostics() {
        let resolved = resolve_builtins(&[], &[]).expect("empty inputs are always OK");
        assert!(resolved.skills.is_empty());
        assert!(resolved.diagnostics.is_empty());
    }

    #[test]
    fn cli_known_name_enables_skill_with_stripped_body() {
        let resolved =
            resolve_builtins(&["karpathy-guidelines".to_string()], &[]).expect("known name");
        assert_eq!(resolved.skills.len(), 1);
        let s = &resolved.skills[0];
        assert_eq!(s.name, "karpathy-guidelines");
        assert!(s.description.starts_with("Behavioral guidelines"));
        assert_eq!(s.file_path, "<builtin>/karpathy-guidelines/SKILL.md");
        // Frontmatter is stripped — body starts with the H1 header.
        assert!(
            s.content.starts_with("# Karpathy Guidelines"),
            "expected body to start with the H1 header, got: {:?}",
            &s.content[..s.content.len().min(80)]
        );
        // No frontmatter delimiter left in the body.
        assert!(!s.content.starts_with("---"));
        assert!(!s.content.contains("\nlicense: MIT"));
        // Sanity: real guideline text is there.
        assert!(s.content.contains("Think Before Coding"));
        // disable_model_invocation defaults to false (frontmatter has no flag).
        assert!(!s.disable_model_invocation);
    }

    #[test]
    fn cli_unknown_name_hard_fails_with_available_list() {
        let err = resolve_builtins(&["nonexistent-skill".to_string()], &[])
            .expect_err("unknown CLI name must hard fail");
        assert_eq!(err.unknown, vec!["nonexistent-skill".to_string()]);
        assert!(err.available.contains(&"karpathy-guidelines".to_string()));
        // Sorted available list — assert order is stable.
        let mut sorted_available = err.available.clone();
        sorted_available.sort();
        assert_eq!(err.available, sorted_available);
    }

    #[test]
    fn cli_mixes_known_and_unknown_reports_all_unknown_at_once() {
        let err = resolve_builtins(
            &[
                "karpathy-guidelines".to_string(),
                "missing-a".to_string(),
                "missing-b".to_string(),
            ],
            &[],
        )
        .expect_err("any unknown CLI name must hard fail");
        // Whack-a-mole avoidance — both unknowns surface in one error.
        assert!(err.unknown.contains(&"missing-a".to_string()));
        assert!(err.unknown.contains(&"missing-b".to_string()));
        assert_eq!(err.unknown.len(), 2);
    }

    #[test]
    fn config_unknown_name_is_soft_warning_not_fail() {
        let resolved = resolve_builtins(&[], &["nonexistent-skill".to_string()])
            .expect("unknown config name must NOT hard fail");
        assert!(
            resolved.skills.is_empty(),
            "unknown name must not enable anything"
        );
        assert_eq!(resolved.diagnostics.len(), 1);
        let diag = &resolved.diagnostics[0];
        assert!(diag.contains("nonexistent-skill"));
        assert!(diag.contains("Available: karpathy-guidelines"));
    }

    #[test]
    fn config_mixes_known_and_unknown_keeps_known_skips_unknown() {
        let resolved = resolve_builtins(
            &[],
            &["karpathy-guidelines".to_string(), "missing".to_string()],
        )
        .expect("config soft path must not fail on unknown");
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].name, "karpathy-guidelines");
        assert_eq!(resolved.diagnostics.len(), 1);
        assert!(resolved.diagnostics[0].contains("missing"));
    }

    #[test]
    fn cli_and_config_same_name_does_not_duplicate_catalog_entry() {
        let resolved = resolve_builtins(
            &["karpathy-guidelines".to_string()],
            &["karpathy-guidelines".to_string()],
        )
        .expect("known on both sides is fine");
        assert_eq!(
            resolved.skills.len(),
            1,
            "union should dedup the same name across CLI + config"
        );
        assert!(resolved.diagnostics.is_empty());
    }

    #[test]
    fn cli_repeated_same_name_does_not_duplicate_catalog_entry() {
        let resolved = resolve_builtins(
            &[
                "karpathy-guidelines".to_string(),
                "karpathy-guidelines".to_string(),
            ],
            &[],
        )
        .expect("repeated --builtin-skill should be idempotent");
        assert_eq!(resolved.skills.len(), 1);
    }

    fn fake_skill(name: &str, file_path: &str) -> Skill {
        Skill {
            name: name.into(),
            description: format!("desc for {name}"),
            file_path: file_path.into(),
            content: format!("body of {name}"),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn merge_no_user_project_returns_builtins_unchanged() {
        let builtins = vec![fake_skill(
            "karpathy-guidelines",
            "<builtin>/karpathy-guidelines/SKILL.md",
        )];
        let merged = merge_with_user_project(builtins.clone(), &[]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "karpathy-guidelines");
        assert_eq!(
            merged[0].file_path,
            "<builtin>/karpathy-guidelines/SKILL.md"
        );
    }

    #[test]
    fn merge_user_project_skill_shadows_builtin_same_name() {
        // Same-name precedence acceptance from #32: user / project skill of the same name
        // wins over the built-in. The combined catalog still has exactly one entry; the
        // built-in entry is replaced in place, not appended.
        let builtins = vec![fake_skill(
            "karpathy-guidelines",
            "<builtin>/karpathy-guidelines/SKILL.md",
        )];
        let user_project = vec![fake_skill(
            "karpathy-guidelines",
            "/home/me/.pie/skills/karpathy-guidelines/SKILL.md",
        )];
        let merged = merge_with_user_project(builtins, &user_project);
        assert_eq!(merged.len(), 1, "same name must collapse to one entry");
        assert_eq!(merged[0].name, "karpathy-guidelines");
        assert_eq!(
            merged[0].file_path, "/home/me/.pie/skills/karpathy-guidelines/SKILL.md",
            "user / project entry must shadow the built-in"
        );
    }

    #[test]
    fn merge_unrelated_user_project_skills_appended_after_builtins() {
        let builtins = vec![fake_skill(
            "karpathy-guidelines",
            "<builtin>/karpathy-guidelines/SKILL.md",
        )];
        let user_project = vec![fake_skill(
            "my-personal-skill",
            "/home/me/.pie/skills/my-personal-skill/SKILL.md",
        )];
        let merged = merge_with_user_project(builtins, &user_project);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "karpathy-guidelines");
        assert_eq!(merged[1].name, "my-personal-skill");
    }

    #[test]
    fn merge_handles_empty_builtins_with_user_project() {
        let user_project = vec![fake_skill(
            "my-personal-skill",
            "/home/me/.pie/skills/my-personal-skill/SKILL.md",
        )];
        let merged = merge_with_user_project(Vec::new(), &user_project);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "my-personal-skill");
    }

    #[test]
    fn parse_config_extracts_enabled_list() {
        let text = r#"
[builtin_skills]
enabled = ["karpathy-guidelines", "future-other-skill"]
"#;
        let enabled = parse_builtin_skills_config(text);
        assert_eq!(
            enabled,
            vec![
                "karpathy-guidelines".to_string(),
                "future-other-skill".to_string()
            ]
        );
    }

    #[test]
    fn parse_config_missing_section_is_empty_list() {
        let text = r#"
[some_other_section]
key = "value"
"#;
        let enabled = parse_builtin_skills_config(text);
        assert!(enabled.is_empty());
    }

    #[test]
    fn parse_config_missing_enabled_key_is_empty_list() {
        let text = r#"
[builtin_skills]
"#;
        let enabled = parse_builtin_skills_config(text);
        assert!(enabled.is_empty());
    }

    #[test]
    fn parse_config_malformed_toml_degrades_to_empty_not_panic() {
        // Soft fail-closed: a typo in the config never blocks startup.
        let text = "this is not valid toml [ [ [";
        let enabled = parse_builtin_skills_config(text);
        assert!(enabled.is_empty());
    }

    #[test]
    fn parse_config_empty_string_is_empty_list() {
        let enabled = parse_builtin_skills_config("");
        assert!(enabled.is_empty());
    }

    #[test]
    fn vendored_skill_md_frontmatter_matches_hardcoded_metadata() {
        // Self-check: if someone updates the vendored SKILL.md and forgets to update the
        // hardcoded BuiltinSpec, this test catches the drift. Match against the description
        // text we surface in /skills + the system prompt.
        let raw = BUILTINS
            .iter()
            .find(|b| b.name == "karpathy-guidelines")
            .unwrap();
        assert!(raw.raw_markdown.contains("name: karpathy-guidelines"));
        assert!(
            raw.raw_markdown.contains(raw.description),
            "vendored SKILL.md description must byte-match the hardcoded BuiltinSpec.description"
        );
    }
}
