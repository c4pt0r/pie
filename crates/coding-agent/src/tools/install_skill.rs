//! `InstallSkill` builtin tool (issue #87 sub-PR B).
//!
//! Lets the agent install a new skill into the user-global skills directory
//! (`~/.pie/skills/<name>/SKILL.md`) from one of three sources: an `https://` URL, a local
//! path, or inline content. Hot-reloads the running harness's catalog so the next prompt
//! sees the new skill without a pie restart.
//!
//! Safety model — the agent should NEVER auto-install third-party skill bodies. Two layers:
//!
//! 1. **Schema-level two phase**. The first tool call (without `confirm: true`) is
//!    read-only: fetch + parse + validate + return a preview JSON (`{name, description,
//!    target_path, content_hash, size, existing, overwrite_required}`). The body is NOT
//!    promoted to the catalog and is NOT echoed verbatim into the tool result. The agent
//!    must explicitly call again with `confirm: true` (and `overwrite: true` if a same-name
//!    skill already exists) for the install to actually run. This means even if the
//!    permission layer runs `Allow`, the model can't silently install on a single
//!    tool-call sequence.
//! 2. **Permission category** — `InstallSkill` should opt into
//!    [`pie_agent_core::PermissionCategory::ControlPlaneWrite`] so the harness hook can
//!    prompt the user. As of this PR the harness `before_tool_call` plumbing doesn't yet
//!    route tools through a non-default category (see PermissionCategory docs:
//!    "Tools-MCP / CLI-TUI's follow-up PRs add the danger classifier + Prompt path
//!    here"). PR-C (`/skills install <url>`) provides the user-facing prompt at the CLI
//!    layer; once the runtime Prompt path is wired, this tool's writes will additionally
//!    require user confirmation through the BeforeToolCallHook chain.
//!
//! Resource protection — per EdHuang on #skill-loader (2026-05-23), skill body itself has
//! NO artificial size cap (real skills like `https://db9.ai/skill.md` exceed any
//! reasonable small cap). Defense lives at network + memory boundaries instead:
//!
//! - URL: `https://` only — no `http`, no `file://`, no `data:`. Loopback / RFC1918 /
//!   link-local / `.localhost` hosts pre-flight rejected as SSRF guard. 15s connect/read
//!   timeout. 5 redirects max.
//! - Stream-read with an OOM guard (`SKILL_FETCH_OOM_GUARD_BYTES`) — well above any
//!   realistic skill (>10 MiB) but bounded so a hostile server can't stream forever.
//! - Path: must be absolute and a regular file. No 64 KiB artifact cap; the same
//!   in-memory guard applies before reading unexpectedly huge local files.
//! - Inline content: no length check; bounded in practice by the LLM provider's context
//!   window and JSON-RPC frame size.
//! - Skill name: must come from the frontmatter `name:` field, must match
//!   `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$` and be ≤ 64 chars (matches `validate_name` in
//!   `pie_agent_core::harness::skills`). No path traversal characters reach the target
//!   path.
//! - Skill description: missing, empty, or oversized `description:` is normalized to a
//!   bounded fallback and surfaced as a warning, not a hard install failure. The installed
//!   `SKILL.md` stays loadable by the runtime skill loader.
//!
//! Preview / audit / tool result remain bounded even with large skill bodies: only
//! metadata (name, description, hash, size, target path) is echoed; the body itself never
//! enters the tool result text or the `skill_install` Custom audit entry.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
    ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde::Deserialize;
use serde_json::{Value, json};
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::tools::skill::SkillHarnessCell;

/// Pure OOM guard on the URL stream-read path, NOT a per-skill artifact cap. Set well
/// above any realistic skill size (real-world skills are kilobytes, sometimes hundreds of
/// kilobytes; we accept up to 16 MiB before refusing). The agent and the LLM provider's
/// context window will impose smaller effective limits in practice. Per EdHuang's
/// 2026-05-23 directive on #skill-loader, the install path does NOT gate on a small
/// skill-body cap — only on memory safety.
const SKILL_FETCH_OOM_GUARD_BYTES: usize = 16 * 1024 * 1024;
/// Bound on the URL fetch round-trip so a hostile server can't hang the install path.
const HTTP_TIMEOUT_SECS: u64 = 15;
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 1024;

pub struct InstallSkillTool {
    harness: SkillHarnessCell,
    /// Resolved at construction time to whatever `base_dir()` returns in production
    /// (`~/.pie`). Stored explicitly so tests can construct the tool with a temp dir
    /// instead of mutating the user's real home directory.
    skills_root: PathBuf,
}

impl InstallSkillTool {
    pub fn new(harness: SkillHarnessCell) -> Self {
        Self::with_skills_root(harness, default_skills_root())
    }

    /// Construct with an explicit skills root. Used by tests so atomic-write and
    /// preview/overwrite-detection paths exercise a temp dir, not the real
    /// `~/.pie/skills/`.
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

/// Production skills root: `${PIE_DIR:-$HOME/.pie}/skills`. Inlined so this module can be
/// included by integration tests that pull `tools/mod.rs` via `#[path = ...]` and don't have
/// access to `crate::config`.
fn default_skills_root() -> PathBuf {
    if let Ok(p) = std::env::var("PIE_DIR") {
        return PathBuf::from(p).join("skills");
    }
    directories::BaseDirs::new()
        .map(|d| d.home_dir().join(".pie").join("skills"))
        .unwrap_or_else(|| PathBuf::from(".pie").join("skills"))
}

#[async_trait]
impl AgentTool for InstallSkillTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "InstallSkill"
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        // Install path writes to the global skills directory and triggers a harness
        // reload — request sequential execution so it doesn't race other tool calls in
        // the same turn (e.g. a second InstallSkill, or reads of the skill catalog).
        Some(ToolExecutionMode::Sequential)
    }

    /// Issue #110 sub-PR 3 classifier — every install is a persistent control-plane write
    /// that grows the model's tool surface, so always route through the
    /// `on_control_plane_prompt` channel. The bounded reason names the source kind only
    /// (whitelisted to `url` / `path` / `content`); the URL / path / content itself is
    /// potentially secret-bearing (e.g. tokenized URLs) and is kept out of the prompt label
    /// per the Provider/Auth URL audit-redaction discipline (PR `742dd6c`).
    ///
    /// The source-kind value is normalized through a fixed whitelist so a hostile or
    /// malformed `source.type` (e.g. a model-supplied string containing payload) cannot
    /// leak through the reason. Anything outside the whitelist becomes `"<unknown source>"`.
    fn permission_classification(&self, prepared_args: &Value) -> PermissionClassification {
        let raw_kind = prepared_args
            .get("source")
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str());
        let normalized = match raw_kind {
            Some("url" | "https") => "url",
            Some("path") => "path",
            Some("content") => "content",
            _ => "<unknown source>",
        };
        PermissionClassification::Prompt {
            reason: format!("install user skill from {normalized}"),
        }
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let input: InstallInput = serde_json::from_value(params)
            .map_err(|e| AgentToolError::Message(format!("invalid arguments: {e}")))?;

        // Phase 1: fetch + parse + validate. Pure read; no fs writes happen here.
        let fetched = fetch_source(&input.source, &cancel).await?;
        let parsed = parse_and_validate(&fetched)?;
        let target_path = self.target_path(&parsed.name);
        // Hash the actual on-disk bytes (same algorithm we use on the new content) so the
        // idempotent re-install case (same content already installed) doesn't spuriously
        // require `overwrite: true`. If the target doesn't exist yet, existing=false.
        let existing_hash = on_disk_skill_hash(&target_path).await;
        let existing = existing_hash.is_some();
        let overwrite_required = existing && existing_hash.as_deref() != Some(&parsed.content_hash);

        if !input.confirm {
            return Ok(AgentToolResult {
                content: vec![UserContentBlock::text(format!(
                    "preview only — call again with `confirm: true` to install. \
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
                    "warnings": parsed.warnings,
                    "target_path": target_path.display().to_string(),
                    "content_hash": parsed.content_hash,
                    "size": parsed.size,
                    "existing": existing,
                    "overwrite_required": overwrite_required,
                }),
                terminate: None,
            });
        }

        // Phase 2: install. Refuse silent overwrite unless caller explicitly asked.
        if overwrite_required && !input.overwrite {
            return Err(AgentToolError::Message(format!(
                "skill '{}' already exists with different content. Call again with \
                 `overwrite: true` to replace it (existing hash differs from new content).",
                parsed.name
            )));
        }

        atomic_write_skill(&target_path, &parsed.normalized_content).await?;

        // Hot-reload via the runtime API (PR-A). On success the harness already swapped its
        // skill catalog and rebuilt the system prompt; the next turn sees the new skill.
        let harness = self
            .harness
            .get()
            .ok_or_else(|| AgentToolError::from("InstallSkill not yet initialized"))?;
        let reload = harness
            .reload_skills_from_disk()
            .await
            .map_err(|e| AgentToolError::Message(format!("reload after install: {e}")))?;

        // Did the new skill actually surface in the reloaded catalog?
        let installed = reload.skills.iter().any(|s| s.name == parsed.name);
        let mut warnings = parsed.warnings.clone();
        warnings.extend(
            reload
                .diagnostics
                .iter()
                .filter(|d| {
                    d.path.contains(&parsed.name) || d.path == target_path.display().to_string()
                })
                .map(|d| format!("{:?}: {}", d.code, d.message)),
        );

        // Persistent audit: append `Custom { custom_type: "skill_install" }` to the session
        // so `--resume`, bug-report, and post-hoc forensics can see model-driven skill
        // installs. Body is NOT included — only metadata + hashes. Best-effort: if the
        // session write fails, the install itself already succeeded on disk + in the
        // catalog, so we log a tracing warning and surface the missing audit id in the
        // tool result rather than rolling back.
        let source_kind = match &input.source {
            Source::Url { .. } => "url",
            Source::Path { .. } => "path",
            Source::Content { .. } => "content",
        };
        let source_redacted = audit_source_reference(&input.source);
        let audit_payload = json!({
            "status": "installed",
            "name": parsed.name,
            "target_path": target_path.display().to_string(),
            "source_kind": source_kind,
            "source": source_redacted,
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
                    "skill_install audit write failed; install itself succeeded"
                );
                None
            }
        };

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "installed skill '{}' to {} ({}B). catalog now has {} skill(s).",
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

// ──────────────────────────────────────────────────────────────────────────────────────────
// Input
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct InstallInput {
    source: Source,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Source {
    #[serde(alias = "https")]
    Url {
        url: String,
    },
    Path {
        path: String,
    },
    Content {
        content: String,
    },
}

fn audit_source_reference(source: &Source) -> Value {
    match source {
        Source::Url { url } => audit_url_reference(url),
        Source::Path { path } => json!(path),
        // Inline content body is never echoed into the audit; we just record that the
        // source was inline so resume can distinguish from URL/path origin.
        Source::Content { .. } => json!(null),
    }
}

fn audit_url_reference(url: &str) -> Value {
    match reqwest::Url::parse(url) {
        Ok(parsed) => {
            let mut hasher = Sha256::new();
            hasher.update(parsed.path().as_bytes());
            json!({
                "scheme": parsed.scheme(),
                "host": parsed.host_str().unwrap_or(""),
                "path_hash": format!("{:x}", hasher.finalize()),
                "redacted": true,
            })
        }
        Err(_) => json!({ "redacted": true }),
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Fetch
// ──────────────────────────────────────────────────────────────────────────────────────────

struct Fetched {
    content: String,
}

async fn fetch_source(
    source: &Source,
    cancel: &CancellationToken,
) -> Result<Fetched, AgentToolError> {
    match source {
        Source::Url { url } => fetch_url(url, cancel).await,
        Source::Path { path } => fetch_path(path).await,
        Source::Content { content } => Ok(fetch_inline(content)),
    }
}

async fn fetch_url(url: &str, cancel: &CancellationToken) -> Result<Fetched, AgentToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| AgentToolError::Message(format!("invalid url: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(AgentToolError::Message(
            "url must use https:// (http, file, data, and other schemes are refused)".into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| AgentToolError::from("url must have a host"))?;
    if is_private_or_local_host(host) {
        return Err(AgentToolError::Message(format!(
            "refusing to fetch from local/private host '{host}' (SSRF guard)"
        )));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(format!("pie/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AgentToolError::Message(format!("http client init: {e}")))?;

    let fut = client.get(parsed).send();
    let mut resp = tokio::select! {
        r = fut => r.map_err(|e| AgentToolError::Message(format!("fetch failed: {e}")))?,
        _ = cancel.cancelled() => return Err(AgentToolError::Message("cancelled".into())),
    };
    if !resp.status().is_success() {
        return Err(AgentToolError::Message(format!(
            "fetch returned non-success status: {}",
            resp.status()
        )));
    }
    // Stream-read with cap so a hostile server can't OOM the agent.
    let mut buf = Vec::<u8>::new();
    loop {
        let chunk = tokio::select! {
            r = resp.chunk() => r,
            _ = cancel.cancelled() => return Err(AgentToolError::Message("cancelled".into())),
        };
        match chunk {
            Ok(Some(c)) => {
                if buf.len() + c.len() > SKILL_FETCH_OOM_GUARD_BYTES {
                    // Pure OOM guard, not a per-skill artifact cap. See module-level docs.
                    return Err(AgentToolError::Message(format!(
                        "fetched skill body exceeds {SKILL_FETCH_OOM_GUARD_BYTES}-byte \
                         in-memory guard ({} bytes received so far); refusing to install \
                         from a stream this large",
                        buf.len()
                    )));
                }
                buf.extend_from_slice(&c);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(AgentToolError::Message(format!("read body: {e}")));
            }
        }
    }
    let content = String::from_utf8(buf)
        .map_err(|e| AgentToolError::Message(format!("skill body is not valid utf-8: {e}")))?;
    Ok(Fetched { content })
}

async fn fetch_path(path: &str) -> Result<Fetched, AgentToolError> {
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return Err(AgentToolError::from(
            "path must be absolute (relative paths are ambiguous in agent context)",
        ));
    }
    let meta = tokio::fs::metadata(&p)
        .await
        .map_err(|e| AgentToolError::Message(format!("stat {}: {e}", p.display())))?;
    if !meta.is_file() {
        return Err(AgentToolError::Message(format!(
            "{} is not a regular file",
            p.display()
        )));
    }
    // Local fs source is user-trusted (they pointed at this path) — same OOM guard as
    // the URL stream-read, just to keep memory bounded if the path points at something
    // unexpectedly huge.
    if meta.len() as usize > SKILL_FETCH_OOM_GUARD_BYTES {
        return Err(AgentToolError::Message(format!(
            "{} ({} bytes) exceeds {SKILL_FETCH_OOM_GUARD_BYTES}-byte in-memory guard",
            p.display(),
            meta.len()
        )));
    }
    let content = tokio::fs::read_to_string(&p)
        .await
        .map_err(|e| AgentToolError::Message(format!("read {}: {e}", p.display())))?;
    Ok(Fetched { content })
}

fn fetch_inline(content: &str) -> Fetched {
    Fetched {
        content: content.to_string(),
    }
}

/// Reject hostnames that point at the loopback / private RFC1918 / link-local space.
/// Pre-flight check: refuses the request before the HTTP client gets a chance to follow a
/// DNS rebinding or hit a local service. Not airtight (a hostile DNS could still resolve a
/// public name to a private IP), but raises the bar.
fn is_private_or_local_host(host: &str) -> bool {
    let host_lower = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost" | "ip6-localhost" | "ip6-loopback" | "broadcasthost"
    ) {
        return true;
    }
    if host_lower.ends_with(".localhost") || host_lower.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host_lower.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unspecified() || v6.segments()[0] & 0xfe00 == 0xfc00
            }
        };
    }
    false
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Parse + validate
// ──────────────────────────────────────────────────────────────────────────────────────────

struct ParsedSkill {
    name: String,
    description: String,
    normalized_content: String,
    content_hash: String,
    size: usize,
    warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

fn parse_and_validate(fetched: &Fetched) -> Result<ParsedSkill, AgentToolError> {
    let normalized = fetched.content.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.starts_with("---") {
        return Err(AgentToolError::from(
            "skill body missing YAML frontmatter (must start with `---` followed by name/description)",
        ));
    }
    let end = normalized[3..]
        .find("\n---")
        .ok_or_else(|| AgentToolError::from("skill frontmatter missing closing `\\n---`"))?;
    let yaml = &normalized[4..end + 3];
    let frontmatter: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|e| AgentToolError::Message(format!("invalid frontmatter yaml: {e}")))?;

    let name = frontmatter
        .name
        .ok_or_else(|| AgentToolError::from("frontmatter missing required field: name"))?;
    validate_name(&name)?;

    let (description, warnings, rewrite_description) =
        normalize_description(frontmatter.description);
    let normalized_content = if rewrite_description {
        normalize_skill_content(&normalized, end + 3, &description)?
    } else {
        normalized
    };

    let mut hasher = Sha256::new();
    hasher.update(normalized_content.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let size = normalized_content.len();

    Ok(ParsedSkill {
        name,
        description,
        normalized_content,
        content_hash: hash,
        size,
        warnings,
    })
}

fn normalize_description(description: Option<String>) -> (String, Vec<String>, bool) {
    let Some(description) = description else {
        return (
            fallback_description(),
            vec!["description missing; using generated fallback".to_string()],
            true,
        );
    };
    let trimmed = description.trim().to_string();
    if trimmed.is_empty() {
        return (
            fallback_description(),
            vec!["description empty; using generated fallback".to_string()],
            true,
        );
    }
    if trimmed.chars().count() > MAX_DESCRIPTION_LEN {
        return (
            fallback_description(),
            vec![format!(
                "description exceeds {MAX_DESCRIPTION_LEN} characters; using generated fallback"
            )],
            true,
        );
    }
    (trimmed, Vec::new(), false)
}

fn fallback_description() -> String {
    "No description provided.".to_string()
}

fn normalize_skill_content(
    normalized: &str,
    yaml_end: usize,
    description: &str,
) -> Result<String, AgentToolError> {
    let yaml = &normalized[4..yaml_end];
    let mut frontmatter: YamlValue = serde_yaml::from_str(yaml)
        .map_err(|e| AgentToolError::Message(format!("invalid frontmatter yaml: {e}")))?;
    let mapping = frontmatter
        .as_mapping_mut()
        .ok_or_else(|| AgentToolError::from("skill frontmatter must be a YAML mapping"))?;
    mapping.insert(
        YamlValue::String("description".to_string()),
        YamlValue::String(description.to_string()),
    );
    let frontmatter = serde_yaml::to_string(&frontmatter)
        .map_err(|e| AgentToolError::Message(format!("failed to normalize frontmatter: {e}")))?;

    Ok(format!(
        "---\n{}{}",
        frontmatter.trim_start_matches("---\n"),
        &normalized[yaml_end..]
    ))
}

fn validate_name(name: &str) -> Result<(), AgentToolError> {
    if name.is_empty() {
        return Err(AgentToolError::from("skill name must not be empty"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AgentToolError::Message(format!(
            "skill name exceeds {MAX_NAME_LEN} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AgentToolError::from(
            "skill name must contain only lowercase a-z, 0-9, and hyphens",
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(AgentToolError::from(
            "skill name must not start or end with a hyphen",
        ));
    }
    if name.contains("--") {
        return Err(AgentToolError::from(
            "skill name must not contain consecutive hyphens",
        ));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Target path + atomic write
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Hash the on-disk SKILL.md bytes at `target_path` using the same SHA256 + line-ending
/// normalization the new-content hash uses, so an idempotent re-install (same bytes already
/// on disk) does not require `overwrite: true`. Returns `None` if the file doesn't exist or
/// can't be read.
async fn on_disk_skill_hash(target_path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(target_path).await.ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    Some(hex::encode(hasher.finalize()))
}

async fn atomic_write_skill(target: &Path, content: &str) -> Result<(), AgentToolError> {
    let parent = target
        .parent()
        .ok_or_else(|| AgentToolError::from("target path has no parent directory"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|e| AgentToolError::Message(format!("create {}: {e}", parent.display())))?;

    // Write to a sibling tempfile in the SAME directory so rename(2) is atomic (cross-fs
    // rename would not be). PID + nanos collision-resistance for the rare case of two
    // installs racing on the same skill name.
    let tmp_name = format!(
        ".SKILL.md.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = parent.join(tmp_name);

    tokio::fs::write(&tmp, content)
        .await
        .map_err(|e| AgentToolError::Message(format!("write {}: {e}", tmp.display())))?;
    if let Err(e) = tokio::fs::rename(&tmp, target).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(AgentToolError::Message(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            target.display()
        )));
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Tool definition
// ──────────────────────────────────────────────────────────────────────────────────────────

static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "InstallSkill".into(),
    description:
        "Install a new skill into the user-global skills directory (~/.pie/skills/<name>/) \
         and hot-reload the catalog so the next turn can use it. Two-phase: first call \
         without `confirm` returns a preview (name, description, target path, hash, size). \
         Second call with `confirm: true` writes atomically and reloads. Same-name skill \
         requires `overwrite: true` when the new content hash differs. Source is one of: \
         https URL, absolute local path, or inline content. Body is never echoed back into \
         the tool result — only metadata + preview info."
            .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "source": {
                "type": "object",
                "description": "Where to fetch the SKILL.md from.",
                "oneOf": [
                    {
                        "properties": {
                            "type": {
                                "enum": ["url", "https"],
                                "description": "Use \"url\" for HTTPS URLs. \"https\" is accepted as a compatibility alias."
                            },
                            "url": {
                                "type": "string",
                                "description": "https:// URL. http/file/data schemes are rejected; loopback and RFC1918 hosts are rejected."
                            }
                        },
                        "required": ["type", "url"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "type": { "const": "path" },
                            "path": {
                                "type": "string",
                                "description": "Absolute path to a local SKILL.md file."
                            }
                        },
                        "required": ["type", "path"],
                        "additionalProperties": false
                    },
                    {
                        "properties": {
                            "type": { "const": "content" },
                            "content": {
                                "type": "string",
                                "description": "Inline SKILL.md content (frontmatter + body)."
                            }
                        },
                        "required": ["type", "content"],
                        "additionalProperties": false
                    }
                ]
            },
            "confirm": {
                "type": "boolean",
                "default": false,
                "description": "When false (default), returns a preview without writing. When true, performs the install."
            },
            "overwrite": {
                "type": "boolean",
                "default": false,
                "description": "Required when a skill of the same name already exists with different content."
            }
        },
        "required": ["source"],
        "additionalProperties": false
    }),
});

// ──────────────────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────────────────

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

    /// Build a harness whose `reload_skills_from_disk` rescans a single test directory.
    /// Returns the harness handle, the cell to plug into the tool, and the temp dir so
    /// callers can construct `InstallSkillTool::with_skills_root(cell, dir.path().into())`
    /// and exercise the install path against the same dir the harness reloads from.
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
        // `OnceCell::set` returns `Err(T)` on collision and `T = Arc<AgentHarness>` isn't
        // `Debug`, so use `is_ok()` + assert instead of `.expect(...)`.
        assert!(cell.set(harness.clone()).is_ok(), "set once");
        (harness, cell, dir)
    }

    fn make_skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
    }

    async fn execute(
        tool: &InstallSkillTool,
        params: Value,
    ) -> Result<AgentToolResult, AgentToolError> {
        tool.execute("call-1", params, CancellationToken::new(), None)
            .await
    }

    fn test_tool(cell: SkillHarnessCell, dir: &tempfile::TempDir) -> InstallSkillTool {
        InstallSkillTool::with_skills_root(cell, dir.path().to_path_buf())
    }

    /// Preview path is read-only — must NOT write anything to the configured skills dir.
    /// Asserts both the absence of side effects AND the preview payload shape.
    #[tokio::test]
    async fn preview_returns_metadata_without_writing() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let skill_md = make_skill_md("alpha", "a useful skill", "do alpha things");

        let result = execute(
            &tool,
            json!({ "source": { "type": "content", "content": skill_md } }),
        )
        .await
        .expect("preview should succeed");

        assert_eq!(result.details["phase"], "preview");
        assert_eq!(result.details["name"], "alpha");
        assert_eq!(result.details["description"], "a useful skill");
        assert_eq!(result.details["existing"], false);
        assert_eq!(result.details["overwrite_required"], false);
        // Body must not be echoed verbatim. Hash + size carry the integrity info.
        let preview_text = match &result.content[0] {
            UserContentBlock::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(
            !preview_text.contains("do alpha things"),
            "preview must not echo skill body, got: {preview_text}"
        );
        // No file should have been created in the test dir.
        assert!(
            !dir.path().join("alpha").exists(),
            "preview must not create any files"
        );
    }

    /// Path traversal / invalid name in frontmatter must be refused at parse time, BEFORE
    /// any fs path resolution. Belt-and-suspenders: even if validate_name regressed, the
    /// target path is derived strictly from the validated name field, never from a source
    /// path component.
    #[tokio::test]
    async fn rejects_traversal_in_skill_name() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let evil = "---\nname: ../etc/passwd\ndescription: x\n---\nbody";
        let err = execute(
            &tool,
            json!({"source": {"type": "content", "content": evil}}),
        )
        .await
        .expect_err("traversal name must fail");
        let AgentToolError::Message(m) = err else {
            panic!("expected typed error");
        };
        assert!(
            m.contains("invalid characters") || m.contains("must contain"),
            "expected name validation error, got: {m}"
        );
    }

    /// http:// (and any non-https scheme) is refused before the request goes out.
    #[tokio::test]
    async fn rejects_http_url() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let err = execute(
            &tool,
            json!({"source": {"type": "url", "url": "http://example.com/skill.md"}}),
        )
        .await
        .expect_err("http must fail");
        let AgentToolError::Message(m) = err else {
            panic!("expected typed error");
        };
        assert!(m.contains("https"), "expected https-only error, got: {m}");
    }

    /// The model naturally tried `{type: "https", url: ...}` in the wild. Keep the public
    /// schema canonical (`url`) but accept `https` as a compatibility alias so the retry
    /// path succeeds instead of bouncing on argument decoding.
    #[tokio::test]
    async fn accepts_https_source_alias_for_url() {
        let input: InstallInput = serde_json::from_value(json!({
            "source": { "type": "https", "url": "https://example.com/skill.md" }
        }))
        .expect("https alias should decode");

        match input.source {
            Source::Url { url } => assert_eq!(url, "https://example.com/skill.md"),
            _ => panic!("https alias should decode as Source::Url"),
        }
    }

    /// SSRF guard: loopback / RFC1918 / `.localhost` hostnames are refused.
    #[tokio::test]
    async fn rejects_private_and_loopback_hosts() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        for host in [
            "https://127.0.0.1/skill.md",
            "https://localhost/skill.md",
            "https://10.0.0.1/skill.md",
            "https://192.168.1.1/skill.md",
            "https://api.localhost/skill.md",
        ] {
            let result = execute(&tool, json!({"source": {"type": "url", "url": host}})).await;
            assert!(
                result.is_err(),
                "host {host} must be refused, got: {result:?}"
            );
            if let Err(AgentToolError::Message(m)) = result {
                assert!(
                    m.contains("SSRF") || m.contains("local") || m.contains("private"),
                    "host {host}: expected SSRF/local/private error, got: {m}"
                );
            }
        }
    }

    /// Real skills can exceed the old 64 KiB cap (`https://db9.ai/skill.md` was ~95 KiB
    /// when this regression was added). Inline/local skill bodies are no longer rejected
    /// by a small fixed artifact-size cap; preview remains metadata-only and bounded.
    #[tokio::test]
    async fn accepts_db9_sized_skill_body_without_echoing_body() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let marker = "large-skill-body-marker";
        let body = format!("{marker}\n{}", "x".repeat(128 * 1024));
        let skill_md = make_skill_md("large-skill", "large desc", &body);

        let preview = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md.clone()}}),
        )
        .await
        .expect("large skill preview should succeed");

        assert_eq!(preview.details["phase"], "preview");
        assert_eq!(preview.details["name"], "large-skill");
        assert_eq!(preview.details["size"], skill_md.len());
        let preview_text = match &preview.content[0] {
            UserContentBlock::Text(t) => &t.text,
            _ => panic!("expected text"),
        };
        assert!(
            !preview_text.contains(marker),
            "preview must not echo large skill body, got: {preview_text}"
        );
        let preview_details = serde_json::to_string(&preview.details).unwrap();
        assert!(
            !preview_details.contains(marker),
            "preview details must not echo large skill body, got: {preview_details}"
        );
    }

    /// Malformed frontmatter / missing name → error before any write. Missing description is
    /// recoverable and covered below.
    #[tokio::test]
    async fn rejects_skill_missing_frontmatter() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        for bad in [
            "no frontmatter at all",
            "---\ndescription: only-desc\n---\nbody",
            "---\nname: foo\n",
        ] {
            let result = execute(
                &tool,
                json!({"source": {"type": "content", "content": bad}}),
            )
            .await;
            assert!(result.is_err(), "input {bad:?} must be refused");
        }
    }

    #[tokio::test]
    async fn installs_skill_missing_description_with_warning() {
        let (harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let skill_md = "---\nname: only-name\n---\n# Heading\nBody body.";

        let preview = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md}}),
        )
        .await
        .expect("missing description should preview with warning");
        assert_eq!(preview.details["phase"], "preview");
        assert_eq!(preview.details["name"], "only-name");
        assert_eq!(preview.details["description"], "No description provided.");
        assert!(
            preview.details["warnings"][0]
                .as_str()
                .unwrap()
                .contains("description missing"),
            "expected missing-description warning, got {:?}",
            preview.details["warnings"]
        );

        let installed = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md}, "confirm": true}),
        )
        .await
        .expect("missing description should install with fallback");
        assert_eq!(installed.details["phase"], "installed");
        assert_eq!(installed.details["installed_visible_in_catalog"], true);
        assert!(
            installed.details["warnings"][0]
                .as_str()
                .unwrap()
                .contains("description missing"),
            "expected missing-description install warning, got {:?}",
            installed.details["warnings"]
        );

        let written = tokio::fs::read_to_string(dir.path().join("only-name").join("SKILL.md"))
            .await
            .expect("SKILL.md was written");
        assert!(written.contains("description: No description provided."));
        assert!(
            harness
                .skills()
                .iter()
                .any(|s| { s.name == "only-name" && s.description == "No description provided." }),
            "installed skill should be visible after reload"
        );
    }

    #[tokio::test]
    async fn previews_recoverable_description_format_with_warning() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let oversized = "x".repeat(MAX_DESCRIPTION_LEN + 1);
        for (skill_md, expected_warning) in [
            (
                "---\nname: empty-desc\ndescription: '   '\n---\nBody.".to_string(),
                "description empty",
            ),
            (
                format!("---\nname: long-desc\ndescription: {oversized}\n---\nBody."),
                "description exceeds",
            ),
        ] {
            let preview = execute(
                &tool,
                json!({"source": {"type": "content", "content": skill_md}}),
            )
            .await
            .expect("recoverable description should preview with warning");
            assert_eq!(preview.details["phase"], "preview");
            assert_eq!(preview.details["description"], "No description provided.");
            assert!(
                preview.details["warnings"][0]
                    .as_str()
                    .unwrap()
                    .contains(expected_warning),
                "expected {expected_warning:?} warning, got {:?}",
                preview.details["warnings"]
            );
        }
    }

    #[tokio::test]
    async fn installs_block_scalar_oversized_description_with_warning() {
        let (harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let oversized = "x".repeat(MAX_DESCRIPTION_LEN + 1);
        let skill_md = format!(
            "---\nname: block-desc\ndescription: |\n  {oversized}\nx-custom: true\n---\n# Heading\nBody."
        );

        let installed = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md}, "confirm": true}),
        )
        .await
        .expect("block scalar oversized description should install with fallback");
        assert_eq!(installed.details["phase"], "installed");
        assert_eq!(installed.details["installed_visible_in_catalog"], true);
        assert!(
            installed.details["warnings"][0]
                .as_str()
                .unwrap()
                .contains("description exceeds"),
            "expected oversized-description install warning, got {:?}",
            installed.details["warnings"]
        );

        let written = tokio::fs::read_to_string(dir.path().join("block-desc").join("SKILL.md"))
            .await
            .expect("SKILL.md was written");
        assert!(written.contains("description: No description provided."));
        assert!(!written.contains(&format!("  {oversized}")));
        assert!(written.contains("x-custom: true"));
        assert!(
            harness
                .skills()
                .iter()
                .any(|s| { s.name == "block-desc" && s.description == "No description provided." }),
            "installed block scalar skill should be visible after reload"
        );
    }

    #[tokio::test]
    async fn accepts_unknown_extra_frontmatter_fields() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let skill_md = "---\nname: extra-field\ndescription: useful\nx-custom: true\n---\nBody.";

        let preview = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md}}),
        )
        .await
        .expect("unknown frontmatter fields should be ignored");
        assert_eq!(preview.details["phase"], "preview");
        assert_eq!(preview.details["name"], "extra-field");
        assert_eq!(preview.details["warnings"].as_array().unwrap().len(), 0);
    }

    /// Existing skill, same on-disk content → not overwrite_required (idempotent re-install OK).
    /// Existing skill, different on-disk content → overwrite_required=true; without `overwrite`
    /// the install rejects with a clear "use overwrite: true" message.
    #[tokio::test]
    async fn overwrite_required_when_hash_differs() {
        let (_harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        // Pre-write an existing skill at the target path with old content.
        let old_md = make_skill_md("alpha", "desc", "old body");
        atomic_write_skill(&dir.path().join("alpha").join("SKILL.md"), &old_md)
            .await
            .unwrap();

        let new_md = make_skill_md("alpha", "desc", "new body");

        // Preview must signal existing + overwrite_required.
        let preview = execute(
            &tool,
            json!({"source": {"type": "content", "content": new_md.clone()}}),
        )
        .await
        .expect("preview ok");
        assert_eq!(preview.details["existing"], true);
        assert_eq!(preview.details["overwrite_required"], true);

        // Confirm without overwrite must fail with a clear hint.
        let err = execute(
            &tool,
            json!({"source": {"type": "content", "content": new_md.clone()}, "confirm": true}),
        )
        .await
        .expect_err("install without overwrite must fail");
        let AgentToolError::Message(m) = err else {
            panic!("expected typed error");
        };
        assert!(
            m.contains("overwrite: true"),
            "expected overwrite-required hint, got: {m}"
        );

        // Same-bytes re-install is idempotent: existing=true, overwrite_required=false.
        let same_preview = execute(
            &tool,
            json!({"source": {"type": "content", "content": old_md.clone()}}),
        )
        .await
        .expect("idempotent preview ok");
        assert_eq!(same_preview.details["existing"], true);
        assert_eq!(same_preview.details["overwrite_required"], false);
    }

    /// Full happy path: phase 1 preview → phase 2 install via the tool itself →
    /// fs has SKILL.md at the right path with the right content → harness reload picks it up.
    #[tokio::test]
    async fn install_writes_atomic_and_reloads_catalog() {
        let (harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let skill_md = make_skill_md("beta", "beta desc", "beta body");

        // Phase 2 directly (Phase 1 preview is exercised by another test).
        let install = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md.clone()}, "confirm": true}),
        )
        .await
        .expect("install ok");
        assert_eq!(install.details["phase"], "installed");
        assert_eq!(install.details["name"], "beta");
        // Atomic write produced the SKILL.md.
        let written = tokio::fs::read_to_string(dir.path().join("beta").join("SKILL.md"))
            .await
            .expect("SKILL.md was written");
        assert_eq!(written, skill_md);
        // Harness catalog now contains the new skill (install path called
        // reload_skills_from_disk internally).
        assert!(
            harness.skills().iter().any(|s| s.name == "beta"),
            "harness catalog must reflect new skill after install"
        );
        // total_skills_after is reported.
        assert!(install.details["total_skills_after"].as_u64().unwrap_or(0) >= 1);
        // Persistent audit was written (QA acceptance — `--resume`/bug-report path).
        let audit_id = install.details["audit_entry_id"].as_str();
        assert!(
            audit_id.is_some_and(|s| !s.is_empty()),
            "audit_entry_id must be set after a successful install, got: {install:?}"
        );
    }

    /// Audit entry shape: persistent `Custom { custom_type: "skill_install" }` records
    /// the metadata QA acceptance asks for (name, target_path, source_kind, before/after
    /// hash, size, overwrite/idempotent flags). Body is NOT included. Read the session
    /// jsonl back through the harness to confirm.
    #[tokio::test]
    async fn install_writes_skill_install_audit_entry() {
        let (harness, cell, dir) = build_test_harness(vec![]);
        let tool = test_tool(cell, &dir);
        let skill_md = make_skill_md("delta", "delta desc", "delta body");

        let _ = execute(
            &tool,
            json!({"source": {"type": "content", "content": skill_md.clone()}, "confirm": true}),
        )
        .await
        .expect("install ok");

        // Walk the session entries and find the `skill_install` Custom record.
        let session = harness.session();
        let entries = session.entries().await.expect("read session entries");
        let custom = entries.iter().find_map(|e| match e {
            pie_agent_core::SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "skill_install" => data.clone(),
            _ => None,
        });
        let data = custom.expect("skill_install audit entry must be written");

        assert_eq!(data["status"], "installed");
        assert_eq!(data["name"], "delta");
        assert_eq!(data["source_kind"], "content");
        // Inline content source MUST NOT echo the body into the audit (QA invariant).
        assert!(
            data["source"].is_null(),
            "inline content source must not echo body into audit, got: {}",
            data["source"]
        );
        assert!(
            data["after_hash"].as_str().is_some_and(|s| s.len() == 64),
            "after_hash should be a 64-char SHA256 hex digest"
        );
        assert_eq!(data["before_hash"], Value::Null);
        assert_eq!(data["overwrote"], false);
        assert_eq!(data["idempotent"], false);
        assert_eq!(data["installed_visible_in_catalog"], true);
        // Body must not leak verbatim.
        let serialized = serde_json::to_string(&data).unwrap();
        assert!(
            !serialized.contains("delta body"),
            "audit must not contain skill body, got: {serialized}"
        );
    }

    #[test]
    fn install_permission_reason_uses_whitelisted_source_kind_only() {
        // Provider/Auth + QA gate on PR #139: the prompt reason is a UI/audit-facing field.
        // It must NOT echo any model-supplied substring — `source.type` is normalized
        // through a fixed whitelist (`url` / `https` → "url", `path`, `content`); anything
        // else becomes "<unknown source>".
        let cell: SkillHarnessCell = Arc::new(SyncOnceCell::new());
        let tool = InstallSkillTool::with_skills_root(
            cell,
            std::env::temp_dir().join("install-skill-pc-test"),
        );

        // Known kinds normalize to their bounded label.
        for (input_type, expected) in [
            ("url", "url"),
            ("https", "url"),
            ("path", "path"),
            ("content", "content"),
        ] {
            let cls = tool.permission_classification(&json!({
                "source": { "type": input_type, "url": "ignored-by-reason" },
            }));
            let PermissionClassification::Prompt { reason } = cls else {
                panic!("InstallSkill must always Prompt; got {cls:?}");
            };
            assert!(
                reason.contains(expected),
                "reason must name normalized source kind {expected}; got: {reason}"
            );
            assert!(
                !reason.contains("ignored-by-reason"),
                "reason must not echo source value; got: {reason}"
            );
        }

        // Hostile / model-smuggled source.type collapses to <unknown source>.
        let evil = json!({
            "source": {
                "type": "https://hub.example/api?token=ABCDEFGHIJKLMNOPQRSTUVWXYZ_super_secret",
                "url": "ignored",
            },
        });
        let cls = tool.permission_classification(&evil);
        let PermissionClassification::Prompt { reason } = cls else {
            panic!("InstallSkill must always Prompt; got {cls:?}");
        };
        assert!(
            reason.contains("<unknown source>"),
            "non-whitelisted source.type must normalize to <unknown source>; got: {reason}"
        );
        assert!(
            !reason.contains("token=") && !reason.contains("super_secret"),
            "reason must NOT echo any payload smuggled through source.type; got: {reason}"
        );
    }

    #[test]
    fn url_audit_reference_redacts_secret_bearing_parts() {
        let reference = audit_url_reference(
            "https://user:pass@example.com/token-path/skill.md?api_key=SECRET#frag",
        );
        let serialized = serde_json::to_string(&reference).unwrap();

        assert_eq!(reference["scheme"], "https");
        assert_eq!(reference["host"], "example.com");
        assert_eq!(reference["redacted"], true);
        assert!(
            reference["path_hash"]
                .as_str()
                .is_some_and(|s| s.len() == 64),
            "path hash should be a SHA256 hex digest: {reference}"
        );
        for forbidden in ["user", "pass", "token-path", "api_key", "SECRET", "frag"] {
            assert!(
                !serialized.contains(forbidden),
                "url audit reference leaked {forbidden}: {serialized}"
            );
        }
    }

    /// Atomic write guarantee: a successful write leaves no `.tmp` sibling in the parent dir.
    #[tokio::test]
    async fn atomic_write_leaves_no_temp_artifact_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("gamma").join("SKILL.md");
        atomic_write_skill(&target, "---\nname: gamma\ndescription: g\n---\nbody\n")
            .await
            .unwrap();
        let mut rd = tokio::fs::read_dir(target.parent().unwrap()).await.unwrap();
        let mut entries = Vec::new();
        while let Some(e) = rd.next_entry().await.unwrap() {
            entries.push(e.file_name().into_string().unwrap_or_default());
        }
        assert_eq!(
            entries,
            vec!["SKILL.md".to_string()],
            "atomic write must not leave a tempfile sibling, got: {entries:?}"
        );
    }
}
