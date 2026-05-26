//! Harness-specific types. 1:1 port of `packages/agent/src/harness/types.ts`.
//!
//! The harness layer exposes:
//! - `Skill` / `PromptTemplate` — discovered resources
//! - `ExecutionEnv` — filesystem + shell abstraction so the harness has one IO surface
//! - Error types (`FileError`, `ExecutionError`, `SessionError`, `CompactionError`, …)
//! - `Result<T, E>` — TS-shaped `{ ok, value | error }` (we use `std::result::Result` directly)
//!
//! Some shapes only matter for fully-implemented subsystems (compaction, sessions); we declare
//! them so the API surface is stable as those subsystems land.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

// ──────────────────────────────────────────────────────────────────────────────────────────
// Result helpers — TS uses `{ok, value|error}`; Rust uses `std::result::Result`. The TS aliases
// `ok()` / `err()` / `getOrThrow()` have direct Rust equivalents (`Ok`/`Err`/`unwrap`); we
// provide a single typedef so the rest of the codebase reads close to the TS.
// ──────────────────────────────────────────────────────────────────────────────────────────

pub type FsResult<T> = std::result::Result<T, FileError>;
pub type ExecResult<T> = std::result::Result<T, ExecutionError>;

// ──────────────────────────────────────────────────────────────────────────────────────────
// File / execution errors
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    NotFound,
    NotADirectory,
    IsADirectory,
    PermissionDenied,
    InvalidPath,
    Aborted,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionErrorCode {
    Timeout,
    Aborted,
    SpawnFailed,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Filesystem types
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub kind: FileKind,
    pub size: u64,
    /// Modification time as milliseconds since Unix epoch.
    pub mtime_ms: i64,
}

#[derive(Default)]
pub struct ExecOptions {
    pub cwd: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub timeout_secs: Option<u64>,
    pub abort: Option<CancellationToken>,
    pub on_stdout: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub on_stderr: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// ExecutionEnv — the harness's IO surface
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Filesystem + shell capability used by the harness. The same trait covers both because the
/// vast majority of consumers want them coupled (a native adapter wraps both; a sandbox
/// implementation can refuse `exec()` while still supporting `read_text_file`).
///
/// All methods take a cancellation token; implementations must honour it.
#[async_trait]
pub trait ExecutionEnv: Send + Sync {
    /// Current working directory for relative paths.
    fn cwd(&self) -> &str;

    /// Absolute addressed path; does **not** resolve symlinks and does **not** require the path
    /// to exist.
    async fn absolute_path(&self, path: &str, cancel: CancellationToken) -> FsResult<String>;

    /// Join path segments in the filesystem namespace.
    async fn join_path(&self, parts: &[&str], cancel: CancellationToken) -> FsResult<String>;

    /// Read a UTF-8 text file.
    async fn read_text_file(&self, path: &str, cancel: CancellationToken) -> FsResult<String>;

    /// Read up to `max_lines` UTF-8 lines from a file.
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: CancellationToken,
    ) -> FsResult<Vec<String>>;

    /// Read a binary file.
    async fn read_binary_file(&self, path: &str, cancel: CancellationToken) -> FsResult<Vec<u8>>;

    /// Create or overwrite a file, creating parent directories when supported.
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        cancel: CancellationToken,
    ) -> FsResult<()>;

    /// Create or append to a file, creating parent directories when supported.
    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        cancel: CancellationToken,
    ) -> FsResult<()>;

    /// Return metadata for the addressed path without following symlinks.
    async fn file_info(&self, path: &str, cancel: CancellationToken) -> FsResult<FileInfo>;

    /// List direct children of a directory without following symlinks.
    async fn list_dir(&self, path: &str, cancel: CancellationToken) -> FsResult<Vec<FileInfo>>;

    /// Return false for missing paths; other errors return `FileError`.
    async fn exists(&self, path: &str, cancel: CancellationToken) -> FsResult<bool>;

    /// Resolve symlinks where supported.
    async fn canonical_path(&self, path: &str, cancel: CancellationToken) -> FsResult<String>;

    /// Create a directory. `recursive` defaults to `true` at the call site.
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: CancellationToken,
    ) -> FsResult<()>;

    /// Remove a file or directory.
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: CancellationToken,
    ) -> FsResult<()>;

    /// Create a temporary directory; returns its absolute path.
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: CancellationToken,
    ) -> FsResult<String>;

    /// Create a temporary file; returns its absolute path.
    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        cancel: CancellationToken,
    ) -> FsResult<String>;

    /// Execute a shell command.
    async fn exec(&self, command: &str, options: ExecOptions) -> ExecResult<ExecOutput>;

    /// Release any pooled resources (handles, watchers, child processes).
    async fn cleanup(&self) {}
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Skill — the resource discovered by `skills.rs`
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Where a loaded skill came from. The runtime walker doesn't distinguish discovery roots,
/// so it leaves this at the default (`User`); the embedder's loader (e.g. the coding-agent
/// CLI, which loads builtin / `~/.pie/skills` / `<cwd>/.pie/skills` separately) sets the
/// correct value per skill. Used for source-aware management — e.g. "remove only
/// user-installed skills", disambiguating a builtin shadowed by a same-name user skill —
/// and for observability (showing the active source in `/skills`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Bundled with the `pie` binary; lowest precedence.
    Builtin,
    /// `~/.pie/skills/` — user-global. Default for directly-constructed skills.
    #[default]
    User,
    /// `<cwd>/.pie/skills/` — project-local; highest precedence.
    Project,
}

impl SkillSource {
    /// Stable lowercase label for display + audit (`builtin` / `user` / `project`).
    pub fn label(self) -> &'static str {
        match self {
            SkillSource::Builtin => "builtin",
            SkillSource::User => "user",
            SkillSource::Project => "project",
        }
    }
}

/// Skill loaded from a `SKILL.md` file or provided by an application.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    /// Canonical lowercase-kebab name (validated against `^[a-z0-9-]+$`, ≤64 chars).
    pub name: String,
    /// Free-form description (≤1024 chars).
    pub description: String,
    /// Absolute path to the source markdown file.
    pub file_path: String,
    /// Markdown body **without** the YAML frontmatter block.
    pub content: String,
    /// When true, the model must not auto-invoke this skill via tool calls; it can only be
    /// surfaced through the skill catalog (descriptive use).
    #[serde(default)]
    pub disable_model_invocation: bool,
    /// Origin of this skill. Defaults to [`SkillSource::User`]; the embedder's loader sets
    /// it per discovery root. Runtime-only consumers (and directly-constructed skills) see
    /// the default.
    #[serde(default)]
    pub source: SkillSource,
}

/// Frontmatter shape parsed off the `SKILL.md` head.
///
/// `disable_model_invocation` is the canonical YAML key — matches the field name on `Skill`,
/// the snake-case spelling used in issue #25 v3 documentation, and the error messages the
/// `Skill` builtin tool emits when the flag is set. `disable-model-invocation` (kebab-case)
/// is kept as a backward-compat alias for any existing SKILL.md files that used the older
/// spelling. Both forms produce the same parsed value.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SkillFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(
        default,
        rename = "disable_model_invocation",
        alias = "disable-model-invocation"
    )]
    pub disable_model_invocation: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDiagnosticCode {
    FileInfoFailed,
    ListFailed,
    ReadFailed,
    ParseFailed,
    InvalidMetadata,
}

/// Warning produced while loading skills. Skills with errors are skipped; diagnostics flow back
/// to the caller for display.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillDiagnostic {
    pub code: SkillDiagnosticCode,
    pub message: String,
    pub path: String,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// PromptTemplate
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: Option<String>,
    pub content: String,
    pub file_path: String,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Higher-level harness errors (declared up front so each subsystem can use them as it lands)
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionErrorCode {
    Aborted,
    SummarizationFailed,
    InvalidSession,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct CompactionError {
    pub code: CompactionErrorCode,
    pub message: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    NotFound,
    AlreadyExists,
    Corrupted,
    StorageFailure,
    Aborted,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}
