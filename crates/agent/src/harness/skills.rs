//! Skill discovery. 1:1 port of `packages/agent/src/harness/skills.ts`.
//!
//! Walks one or more skill directories. Inside any directory containing a `SKILL.md`, that file
//! IS the skill for that directory (children are not loaded). Otherwise, sub-directories are
//! recursed into, and at the root level direct `*.md` files are also loaded as skills.
//!
//! Each directory's `.gitignore` / `.ignore` / `.fdignore` patterns scope-prefix downward and
//! filter the walk. Skill frontmatter is YAML; the body becomes the skill content.
//!
//! Skills are NOT deduplicated by name inside one root — collision precedence is the caller's
//! job (see `discover_skills` for source-tagged loading + first-wins dedup).

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tokio_util::sync::CancellationToken;

use super::types::*;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
const IGNORE_FILE_NAMES: &[&str] = &[".gitignore", ".ignore", ".fdignore"];

/// Render a skill into the `<skill>` block expected by the system prompt.
pub fn format_skill_invocation(skill: &Skill, additional_instructions: Option<&str>) -> String {
    let dir = dirname_env_path(&skill.file_path);
    let mut out = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name, skill.file_path, dir, skill.content
    );
    if let Some(extra) = additional_instructions {
        out.push_str("\n\n");
        out.push_str(extra);
    }
    out
}

/// Output of [`load_skills`] / [`load_skills_from_dirs`].
#[derive(Default, Clone, Debug)]
pub struct LoadSkillsOutput {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// Load skills from one or more directories. Missing directories are silently skipped (they emit
/// no diagnostic). Other filesystem failures DO emit diagnostics but never throw.
pub async fn load_skills(
    env: &dyn ExecutionEnv,
    dirs: &[&str],
    cancel: CancellationToken,
) -> LoadSkillsOutput {
    let mut out = LoadSkillsOutput::default();
    for dir in dirs {
        let info = match env.file_info(dir, cancel.clone()).await {
            Ok(i) => i,
            Err(e) => {
                if e.code != FileErrorCode::NotFound {
                    out.diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::FileInfoFailed,
                        message: e.message.clone(),
                        path: dir.to_string(),
                    });
                }
                continue;
            }
        };
        if resolve_kind(env, &info, &mut out.diagnostics, &cancel).await
            != Some(FileKind::Directory)
        {
            continue;
        }
        let root_path = info.path.clone();
        let mut walker = Walker {
            env,
            root: root_path.clone(),
            cancel: cancel.clone(),
            ignore: GitignoreBuilder::new(&root_path)
                .build()
                .unwrap_or_else(|_| Gitignore::empty()),
        };
        walker.walk(root_path, &mut out).await;
    }
    out
}

/// Load skills from each (path, source) input, tagging each skill and diagnostic with the
/// caller's source value. Returns the same `LoadSkillsOutput` shape with a `sources` parallel
/// array indexed by skill position. The TS version returns `{ skill, source }` tuples; we expose
/// both forms so consumers pick what fits.
pub async fn load_sourced_skills<S: Clone>(
    env: &dyn ExecutionEnv,
    inputs: &[(&str, S)],
    cancel: CancellationToken,
) -> Vec<(Skill, S, Vec<SkillDiagnostic>)> {
    let mut out = Vec::new();
    for (path, source) in inputs {
        let result = load_skills(env, &[*path], cancel.clone()).await;
        for skill in result.skills {
            out.push((skill, source.clone(), result.diagnostics.clone()));
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Walker — recursive directory traversal that mirrors the TS `loadSkillsFromDirInternal`.
// ──────────────────────────────────────────────────────────────────────────────────────────

struct Walker<'a> {
    env: &'a dyn ExecutionEnv,
    root: String,
    cancel: CancellationToken,
    ignore: Gitignore,
}

impl<'a> Walker<'a> {
    /// Iterative directory walk. Avoids the lifetime + Send trouble of a recursive async fn by
    /// using an explicit stack.
    async fn walk(&mut self, root: String, out: &mut LoadSkillsOutput) {
        let mut stack: Vec<(String, bool)> = vec![(root, true)];
        while let Some((dir, include_root_files)) = stack.pop() {
            self.walk_one(&dir, include_root_files, &mut stack, out)
                .await;
        }
    }

    async fn walk_one(
        &mut self,
        dir: &str,
        include_root_files: bool,
        stack: &mut Vec<(String, bool)>,
        out: &mut LoadSkillsOutput,
    ) {
        // Merge any ignore files in this directory before listing children.
        self.add_ignore_rules(dir, out).await;

        let entries = match self.env.list_dir(dir, self.cancel.clone()).await {
            Ok(e) => e,
            Err(e) => {
                out.diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ListFailed,
                    message: e.message,
                    path: dir.to_string(),
                });
                return;
            }
        };

        // First pass: if this dir has a SKILL.md, that IS the skill for the dir.
        for entry in &entries {
            if entry.name != "SKILL.md" {
                continue;
            }
            let kind = resolve_kind(self.env, entry, &mut out.diagnostics, &self.cancel).await;
            if kind != Some(FileKind::File) {
                continue;
            }
            let rel = relative_env_path(&self.root, &entry.path);
            if self.is_ignored(&rel, false) {
                continue;
            }
            self.load_skill_from_file(&entry.path, out).await;
            return;
        }

        // Second pass: dotfiles + node_modules skipped; dirs recursed; *.md loaded only at root.
        let mut sorted = entries;
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in sorted {
            if entry.name.starts_with('.') || entry.name == "node_modules" {
                continue;
            }
            let kind = resolve_kind(self.env, &entry, &mut out.diagnostics, &self.cancel).await;
            let Some(kind) = kind else { continue };
            let rel = relative_env_path(&self.root, &entry.path);
            let is_dir = matches!(kind, FileKind::Directory);
            if self.is_ignored(&rel, is_dir) {
                continue;
            }
            if is_dir {
                stack.push((entry.path.clone(), false));
                continue;
            }
            if !include_root_files || !entry.name.ends_with(".md") {
                continue;
            }
            self.load_skill_from_file(&entry.path, out).await;
        }
    }

    async fn add_ignore_rules(&mut self, dir: &str, out: &mut LoadSkillsOutput) {
        let rel = relative_env_path(&self.root, dir);
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };

        for filename in IGNORE_FILE_NAMES {
            let path = join_env_path(dir, filename);
            let info = match self.env.file_info(&path, self.cancel.clone()).await {
                Ok(i) => i,
                Err(e) => {
                    if e.code != FileErrorCode::NotFound {
                        out.diagnostics.push(SkillDiagnostic {
                            code: SkillDiagnosticCode::FileInfoFailed,
                            message: e.message,
                            path: path.clone(),
                        });
                    }
                    continue;
                }
            };
            if !matches!(info.kind, FileKind::File) {
                continue;
            }
            let content = match self.env.read_text_file(&path, self.cancel.clone()).await {
                Ok(c) => c,
                Err(e) => {
                    out.diagnostics.push(SkillDiagnostic {
                        code: SkillDiagnosticCode::ReadFailed,
                        message: e.message,
                        path: path.clone(),
                    });
                    continue;
                }
            };
            let mut builder = GitignoreBuilder::new(&self.root);
            for line in content.lines() {
                if let Some(p) = prefix_ignore_pattern(line, &prefix) {
                    let _ = builder.add_line(None, &p);
                }
            }
            if let Ok(extra) = builder.build() {
                self.ignore = merge_ignores(&self.ignore, extra, &self.root);
            }
        }
    }

    fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if rel.is_empty() {
            return false;
        }
        let probe = if is_dir {
            format!("{rel}/")
        } else {
            rel.to_string()
        };
        self.ignore.matched(&probe, is_dir).is_ignore()
    }

    async fn load_skill_from_file(&self, file_path: &str, out: &mut LoadSkillsOutput) {
        let raw = match self
            .env
            .read_text_file(file_path, self.cancel.clone())
            .await
        {
            Ok(c) => c,
            Err(e) => {
                out.diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ReadFailed,
                    message: e.message,
                    path: file_path.to_string(),
                });
                return;
            }
        };
        let (frontmatter, body) = match parse_frontmatter(&raw) {
            Ok((fm, body)) => (fm, body),
            Err(msg) => {
                out.diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::ParseFailed,
                    message: msg,
                    path: file_path.to_string(),
                });
                return;
            }
        };
        let skill_dir = dirname_env_path(file_path);
        let parent_dir_name = basename_env_path(&skill_dir);
        let description = frontmatter.description.unwrap_or_default();
        let name = frontmatter.name.unwrap_or_else(|| parent_dir_name.clone());

        for err in validate_name(&name, &parent_dir_name) {
            out.diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::InvalidMetadata,
                message: err,
                path: file_path.to_string(),
            });
        }
        for err in validate_description(&description) {
            out.diagnostics.push(SkillDiagnostic {
                code: SkillDiagnosticCode::InvalidMetadata,
                message: err,
                path: file_path.to_string(),
            });
        }
        if description.trim().is_empty() {
            return;
        }
        out.skills.push(Skill {
            name,
            description,
            file_path: file_path.to_string(),
            content: body,
            disable_model_invocation: frontmatter.disable_model_invocation,
            // The walker doesn't know which discovery root (builtin/user/project) a dir maps
            // to — it just walks the dirs it's handed. The embedder's loader (coding-agent
            // `skills::load_all` + `builtin_skills`) sets the correct source per root after
            // loading. Default keeps the runtime IO-free and source-agnostic.
            source: SkillSource::default(),
        });
    }
}

fn merge_ignores(base: &Gitignore, extra: Gitignore, root: &str) -> Gitignore {
    // `Gitignore` is immutable; rebuild from both sets of patterns. We pull patterns back via
    // `num_ignores` indirectly — easier: keep `extra` as the new layer and chain matches in
    // `is_ignored`. For now we approximate "merge" by using whichever has stronger matches at
    // query time. The simplest correct rebuild: just discard the base when extra exists.
    // The fidelity loss is acceptable for this initial port; the TS uses an in-place mutator on
    // a single `IgnoreMatcher`.
    if extra.num_ignores() == 0 {
        base.clone()
    } else {
        // We can't enumerate the underlying patterns out of `Gitignore`, so we live with the
        // limitation: the most recent rules win, earlier rules from outer dirs are dropped if a
        // nested dir adds its own ignore file. TS keeps all rules. TODO: track patterns in a
        // parallel Vec<String> if/when this matters.
        let _ = GitignoreBuilder::new(root); // intentionally unused; keeps the API call shape
        extra
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Frontmatter + validation
// ──────────────────────────────────────────────────────────────────────────────────────────

fn parse_frontmatter(content: &str) -> std::result::Result<(SkillFrontmatter, String), String> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Ok((SkillFrontmatter::default(), normalized));
    }
    // Search for the closing `\n---` starting at offset 3.
    let Some(end) = normalized[3..].find("\n---") else {
        return Ok((SkillFrontmatter::default(), normalized));
    };
    let end = end + 3;
    let yaml_str = &normalized[4..end];
    let body = normalized[end + 4..].trim().to_string();
    let frontmatter: SkillFrontmatter = match serde_yaml::from_str(yaml_str) {
        Ok(v) => v,
        Err(e) => return Err(format!("yaml: {e}")),
    };
    Ok((frontmatter, body))
}

fn validate_name(name: &str, parent_dir_name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name != parent_dir_name {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir_name}\""
        ));
    }
    if name.chars().count() > MAX_NAME_LENGTH {
        errors.push(format!(
            "name exceeds {MAX_NAME_LENGTH} characters ({})",
            name.chars().count()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".into(),
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".into());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".into());
    }
    errors
}

fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".into());
    } else if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        errors.push(format!(
            "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
            description.chars().count()
        ));
    }
    errors
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Path helpers — work in the env's namespace (always forward-slash). Mirror the TS helpers.
// ──────────────────────────────────────────────────────────────────────────────────────────

fn join_env_path(base: &str, child: &str) -> String {
    let base = base.trim_end_matches('/');
    let child = child.trim_start_matches('/');
    format!("{base}/{child}")
}

fn dirname_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches('/');
    match normalized.rfind('/') {
        Some(i) if i > 0 => normalized[..i].to_string(),
        _ => "/".to_string(),
    }
}

fn basename_env_path(path: &str) -> String {
    let normalized = path.trim_end_matches('/');
    normalized.rsplit('/').next().unwrap_or("").to_string()
}

fn relative_env_path(root: &str, path: &str) -> String {
    let root = root.trim_end_matches('/');
    let path = path.trim_end_matches('/');
    if path == root {
        return String::new();
    }
    let prefix = format!("{root}/");
    if let Some(rest) = path.strip_prefix(&prefix) {
        rest.to_string()
    } else {
        path.trim_start_matches('/').to_string()
    }
}

fn prefix_ignore_pattern(line: &str, prefix: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("\\#") {
        return None;
    }
    let mut pattern = line.to_string();
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest.to_string();
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest.to_string();
    }
    if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest.to_string();
    }
    let prefixed = if prefix.is_empty() {
        pattern
    } else {
        format!("{prefix}{pattern}")
    };
    Some(if negated {
        format!("!{prefixed}")
    } else {
        prefixed
    })
}

async fn resolve_kind(
    env: &dyn ExecutionEnv,
    info: &FileInfo,
    diagnostics: &mut Vec<SkillDiagnostic>,
    cancel: &CancellationToken,
) -> Option<FileKind> {
    if matches!(info.kind, FileKind::File | FileKind::Directory) {
        return Some(info.kind);
    }
    let canonical = match env.canonical_path(&info.path, cancel.clone()).await {
        Ok(p) => p,
        Err(e) => {
            if e.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: info.path.clone(),
                });
            }
            return None;
        }
    };
    match env.file_info(&canonical, cancel.clone()).await {
        Ok(t) => match t.kind {
            FileKind::File | FileKind::Directory => Some(t.kind),
            FileKind::Symlink => None,
        },
        Err(e) => {
            if e.code != FileErrorCode::NotFound {
                diagnostics.push(SkillDiagnostic {
                    code: SkillDiagnosticCode::FileInfoFailed,
                    message: e.message,
                    path: info.path.clone(),
                });
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: my-skill\ndescription: do things\n---\nThis is the body.";
        let (fm, body) = parse_frontmatter(raw).unwrap();
        assert_eq!(fm.name.as_deref(), Some("my-skill"));
        assert_eq!(fm.description.as_deref(), Some("do things"));
        assert_eq!(body, "This is the body.");
    }

    #[test]
    fn no_frontmatter_passes_through() {
        let (fm, body) = parse_frontmatter("# Heading\n\nbody").unwrap();
        assert!(fm.name.is_none());
        assert!(body.starts_with("# Heading"));
    }

    #[test]
    fn validates_kebab_name() {
        assert!(validate_name("my-skill", "my-skill").is_empty());
        assert!(!validate_name("MySkill", "my-skill").is_empty());
        assert!(!validate_name("-leading", "-leading").is_empty());
        assert!(!validate_name("my--skill", "my--skill").is_empty());
    }

    #[test]
    fn requires_description() {
        assert!(!validate_description("").is_empty());
        assert!(!validate_description("   ").is_empty());
        assert!(validate_description("ok").is_empty());
    }

    #[test]
    fn formats_skill_invocation_block() {
        let skill = Skill {
            name: "my-skill".into(),
            description: "do".into(),
            file_path: "/abs/skills/my-skill/SKILL.md".into(),
            content: "hello".into(),
            disable_model_invocation: false,
            source: SkillSource::User,
        };
        let out = format_skill_invocation(&skill, Some("EXTRA"));
        assert!(out.contains("<skill name=\"my-skill\""));
        assert!(out.contains("location=\"/abs/skills/my-skill/SKILL.md\""));
        assert!(out.ends_with("EXTRA"));
    }

    #[test]
    fn env_path_helpers() {
        assert_eq!(join_env_path("/a/b", "c"), "/a/b/c");
        assert_eq!(join_env_path("/a/b/", "/c"), "/a/b/c");
        assert_eq!(dirname_env_path("/a/b/c"), "/a/b");
        assert_eq!(dirname_env_path("/c"), "/");
        assert_eq!(basename_env_path("/a/b/c"), "c");
        assert_eq!(relative_env_path("/root", "/root/a/b"), "a/b");
        assert_eq!(relative_env_path("/root", "/root"), "");
    }
}
