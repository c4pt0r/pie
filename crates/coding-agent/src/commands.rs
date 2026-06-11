//! Slash-command registry. Tracks a small set of REPL builtins and dispatches by name.
//!
//! Built-in commands today: `/help`, `/clear`, `/skills`, `/skill`, `/quit` (and aliases),
//! `/model`, `/thinking`. The trait is shaped so future extensions (issue #10 Part B) can
//! register additional commands without touching this file.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pie_agent_core::{
    AgentHarness, AgentTool, HookState, NotificationHookStatus, NotificationStatusSnapshot,
    RunningTriggerState, SessionTreeEntry, Skill, SkillSource, ThinkingLevel,
};
use pie_ai::{Model, Provider, UserContentBlock, get_model, list_models};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Sink for slash-command output. The full-screen TUI owns the only terminal writer, so
/// commands must not `println!` straight to stdout — they route through here. The app installs
/// a sink that forwards each line into the conversation feed; when none is installed (unit
/// tests, non-interactive shells) output falls back to stdout.
pub mod console {
    use parking_lot::Mutex;

    type Sink = Box<dyn Fn(String) + Send + Sync>;
    static SINK: Mutex<Option<Sink>> = Mutex::new(None);

    /// Install the line sink. Called once by the UI at startup. Unused when `commands.rs` is
    /// path-included by integration tests (which never install a sink).
    #[cfg_attr(test, allow(dead_code))]
    pub fn set_sink(sink: Sink) {
        *SINK.lock() = Some(sink);
    }

    /// Clear the active line sink. Used by tests to avoid leaking capture sinks across cases.
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn clear_sink() {
        *SINK.lock() = None;
    }

    /// Emit one line of command output through the active sink (or stdout when unset).
    pub fn emit_line(line: String) {
        match SINK.lock().as_ref() {
            Some(sink) => sink(line),
            None => println!("{line}"),
        }
    }
}

/// Drop-in replacement for `println!` inside this module: same call syntax, but the formatted
/// line is routed through [`console::emit_line`] instead of straight to stdout.
macro_rules! cprintln {
    () => { $crate::commands::console::emit_line(String::new()) };
    ($($arg:tt)*) => { $crate::commands::console::emit_line(std::format!($($arg)*)) };
}

#[cfg_attr(test, allow(dead_code))]
pub const THINKING_LEVEL_VALUES: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];
pub const THINKING_LEVEL_USAGE: &str = "[off|minimal|low|medium|high|xhigh]";

/// Outcome of running a command. Drives the REPL's next action.
#[cfg_attr(test, allow(dead_code))]
pub enum CommandOutcome {
    /// Continue the REPL loop normally.
    Handled,
    /// Quit the REPL cleanly.
    Quit,
    /// Clear the screen — REPL handles the ANSI escape so we don't bake it into commands.
    ClearScreen,
    /// Command surfaced an error message; REPL renders it via `tui.error_line`.
    Error(String),
    /// Attach the named skill to the next user prompt. The REPL owns prompt assembly, so this
    /// stays explicit instead of going through the agent steering queue.
    AttachSkill { name: String },
    /// Ask the REPL to run a prompt through the same active-turn path as normal user input.
    /// Commands return this instead of awaiting the harness directly so Ctrl-C/Esc can abort
    /// thinking, streaming, and tool execution consistently.
    RunAgentPrompt {
        prompt: String,
        error_context: &'static str,
    },
    /// Ask the REPL to render and run a prompt template through the active-turn path.
    RunPromptTemplate {
        name: String,
        vars: serde_json::Map<String, serde_json::Value>,
    },
    /// Ask the REPL to run compaction through the active-turn path so Ctrl-C/Esc can abort
    /// the model summarization request.
    RunCompaction { custom: Option<String> },
    /// Prompt for a credential without echoing the secret in the terminal input line.
    ///
    /// `provider` is the user-facing label used in prompts. `storage_key` is the optional auth
    /// store key when the internal lookup key must not be echoed back to the user.
    LoginSecret {
        provider: String,
        storage_key: Option<String>,
        recovery_command: Option<String>,
    },
    /// `/web-connect` family — the relay lives on the UI `App`, so the REPL layer
    /// performs the action (issue #22).
    WebRelay(WebRelayAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebRelayAction {
    Connect,
    Status,
    Disconnect,
}

impl std::fmt::Debug for CommandOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handled => f.write_str("Handled"),
            Self::Quit => f.write_str("Quit"),
            Self::ClearScreen => f.write_str("ClearScreen"),
            Self::Error(message) => f.debug_tuple("Error").field(message).finish(),
            Self::AttachSkill { name } => {
                f.debug_struct("AttachSkill").field("name", name).finish()
            }
            Self::RunAgentPrompt {
                prompt,
                error_context,
            } => f
                .debug_struct("RunAgentPrompt")
                .field("prompt", prompt)
                .field("error_context", error_context)
                .finish(),
            Self::RunPromptTemplate { name, vars } => f
                .debug_struct("RunPromptTemplate")
                .field("name", name)
                .field("vars", vars)
                .finish(),
            Self::RunCompaction { custom } => f
                .debug_struct("RunCompaction")
                .field("custom", custom)
                .finish(),
            Self::LoginSecret {
                provider,
                storage_key,
                recovery_command,
            } => f
                .debug_struct("LoginSecret")
                .field("provider", provider)
                .field("storage_key", storage_key)
                .field("recovery_command", recovery_command)
                .finish(),
            Self::WebRelay(action) => f.debug_tuple("WebRelay").field(action).finish(),
        }
    }
}

/// Context handed to a command at runtime. Kept narrow so each command's dependencies are
/// explicit.
pub struct CommandCtx<'a> {
    pub harness: &'a Arc<AgentHarness>,
    pub session_id: &'a str,
    pub log_path: Option<&'a PathBuf>,
    pub tool_count: usize,
    pub cwd: &'a std::path::Path,
}

#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// Canonical name without the leading `/`.
    fn name(&self) -> &'static str;
    /// Optional aliases (also without leading `/`).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }
    fn description(&self) -> &'static str;
    /// Optional argument hint shown in `/help`. Empty when the command takes no arguments.
    fn usage(&self) -> &'static str {
        ""
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome;
}

/// In-memory registry. Lookups are linear scans over a small set — `O(n)` is fine.
pub struct Registry {
    commands: Vec<Arc<dyn SlashCommand>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(HelpCommand));
        r.register(Arc::new(ClearCommand));
        r.register(Arc::new(SkillsCommand));
        r.register(Arc::new(SkillCommand));
        r.register(Arc::new(QuitCommand));
        r.register(Arc::new(ModelCommand));
        r.register(Arc::new(ThinkingCommand));
        r.register(Arc::new(CostCommand));
        r.register(Arc::new(DiagCommand));
        r.register(Arc::new(TemplateCommand));
        r.register(Arc::new(SaveCommand));
        r.register(Arc::new(CompactCommand));
        r.register(Arc::new(UndoCommand));
        r.register(Arc::new(BugReportCommand));
        r.register(Arc::new(NameCommand));
        r.register(Arc::new(SessionCommand));
        r.register(Arc::new(WebConnectCommand));
        r.register(Arc::new(WebDisconnectCommand));
        r.register(Arc::new(SessionsCommand));
        r.register(Arc::new(ShareCommand));
        r.register(Arc::new(LoginCommand));
        r.register(Arc::new(LogoutCommand));
        r.register(Arc::new(FindCommand));
        r.register(Arc::new(HistoryCommand));
        r.register(Arc::new(GoalCommand));
        r.register(Arc::new(GoalStartCommand));
        r.register(Arc::new(TriggersCommand));
        r.register(Arc::new(NewTriggerCommand));
        r.register(Arc::new(CronCommand));
        r
    }

    pub fn register(&mut self, command: Arc<dyn SlashCommand>) {
        self.commands.push(command);
    }

    pub fn commands(&self) -> &[Arc<dyn SlashCommand>] {
        &self.commands
    }

    /// Lookup by name or alias. `name` is the bare command without `/`.
    pub fn find(&self, name: &str) -> Option<Arc<dyn SlashCommand>> {
        self.commands
            .iter()
            .find(|c| c.name() == name || c.aliases().contains(&name))
            .cloned()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// Split `/cmd arg1 "arg with spaces"` into `(cmd, [arg1, arg with spaces])`. Returns `None`
/// if `input` doesn't start with `/`. Quoting is minimal: balanced double quotes only.
pub fn parse(input: &str) -> Option<(String, Vec<String>)> {
    let trimmed = input.trim_start();
    let body = trimmed.strip_prefix('/')?;
    let mut argv: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in body.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        argv.push(current);
    }
    if argv.is_empty() {
        // Bare `/` — no command name.
        return None;
    }
    let name = argv.remove(0);
    Some((name, argv))
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Builtins
// ──────────────────────────────────────────────────────────────────────────────────────────

struct HelpCommand;

#[async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &'static str {
        "help"
    }
    fn description(&self) -> &'static str {
        "show available commands and model catalog help"
    }
    fn usage(&self) -> &'static str {
        "[models|<command>]"
    }
    async fn run(&self, _argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        // The REPL's `print_help` walks the registry — see main.rs. This handler is a stub
        // because Help needs the Registry itself, which we don't pass into commands. The
        // REPL detects `/help` before dispatch.
        CommandOutcome::Handled
    }
}

struct ClearCommand;

#[async_trait]
impl SlashCommand for ClearCommand {
    fn name(&self) -> &'static str {
        "clear"
    }
    fn description(&self) -> &'static str {
        "clear screen (keeps conversation history)"
    }
    async fn run(&self, _argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        CommandOutcome::ClearScreen
    }
}

struct SkillsCommand;

#[async_trait]
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &'static str {
        "skills"
    }
    fn description(&self) -> &'static str {
        "list, install, inspect, reload, enable, disable, or remove skills"
    }
    fn usage(&self) -> &'static str {
        "[install [--confirm] [--overwrite] <url|path>|show <name>|reload|enable <name> [source]|disable <name> [source]|remove [--confirm] <name> [source]]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        match argv.first().map(String::as_str) {
            None | Some("list" | "ls") => {
                print_skills_list(&ctx.harness.skills());
                CommandOutcome::Handled
            }
            Some("install") => install_skill(&argv[1..], ctx).await,
            Some("show") => show_skill(&argv[1..], ctx),
            Some("reload") => reload_skills(ctx).await,
            Some("enable") => set_skill_enabled(&argv[1..], ctx, true).await,
            Some("disable") => set_skill_enabled(&argv[1..], ctx, false).await,
            Some("remove") => remove_skill(&argv[1..], ctx).await,
            Some(_) => CommandOutcome::Error(
                "usage: /skills [install [--confirm] [--overwrite] <url|path>|show <name>|reload|enable <name> [source]|disable <name> [source]|remove [--confirm] <name> [source]]"
                    .into(),
            ),
        }
    }
}

fn print_skills_list(skills: &[Skill]) {
    if skills.is_empty() {
        cprintln!(
            "(no skills loaded — drop SKILL.md files under ~/.pie/skills/<name>/ or <cwd>/.pie/skills/<name>/)"
        );
    } else {
        cprintln!("Loaded skills ({}):", skills.len());
        for s in skills {
            let disabled = if s.disable_model_invocation {
                "  [disabled: disable_model_invocation=true]"
            } else {
                ""
            };
            cprintln!("  - {}  ({}){}", s.name, s.source.label(), disabled);
            if !s.description.is_empty() {
                cprintln!("      {}", s.description);
            }
            cprintln!("      path: {}", s.file_path);
        }
    }
}

fn show_skill(argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
    let Some(name) = argv.first() else {
        return CommandOutcome::Error("usage: /skills show <name> [source]".into());
    };
    let source = match optional_skill_source(argv.get(1)) {
        Ok(source) => source,
        Err(e) => return CommandOutcome::Error(e),
    };
    let skills = ctx.harness.skills();
    let skill = match resolve_active_skill(&skills, name, source) {
        Ok(skill) => skill,
        Err(e) => return CommandOutcome::Error(e),
    };
    cprintln!("Skill: {} ({})", skill.name, skill.source.label());
    cprintln!(
        "Status: {}",
        if skill.disable_model_invocation {
            "disabled"
        } else {
            "enabled"
        }
    );
    if !skill.description.is_empty() {
        cprintln!("Description: {}", skill.description);
    }
    cprintln!("Path: {}", skill.file_path);
    cprintln!("Body: not shown; use the file path if you need to inspect the full skill.");
    CommandOutcome::Handled
}

async fn reload_skills(ctx: &CommandCtx<'_>) -> CommandOutcome {
    match ctx.harness.reload_skills_from_disk().await {
        Ok(out) => {
            cprintln!(
                "reloaded skills: {} loaded, {} diagnostics",
                out.skills.len(),
                out.diagnostics.len()
            );
            CommandOutcome::Handled
        }
        Err(e) => CommandOutcome::Error(format!("reload skills failed: {e}")),
    }
}

async fn install_skill(argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
    let parsed = match parse_skill_install_args(argv) {
        Ok(parsed) => parsed,
        Err(e) => return CommandOutcome::Error(e),
    };
    let source = skill_install_source(parsed.target, ctx.cwd);
    let params = json!({
        "source": source,
        "confirm": parsed.confirm,
        "overwrite": parsed.overwrite,
    });
    let cell = skill_harness_cell(ctx);
    let tool = crate::tools::install_skill::InstallSkillTool::new(cell);
    match tool
        .execute(
            "slash-skills-install",
            params,
            CancellationToken::new(),
            None,
        )
        .await
    {
        Ok(result) => {
            print_install_skill_result(&result, &parsed);
            CommandOutcome::Handled
        }
        Err(e) => CommandOutcome::Error(format!("install skill failed: {e}")),
    }
}

struct InstallSkillArgs<'a> {
    target: &'a str,
    confirm: bool,
    overwrite: bool,
}

fn parse_skill_install_args(argv: &[String]) -> Result<InstallSkillArgs<'_>, String> {
    let mut confirm = false;
    let mut overwrite = false;
    let mut positional = Vec::new();
    for arg in argv {
        match arg.as_str() {
            "--confirm" | "--yes" => confirm = true,
            "--overwrite" => overwrite = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option for /skills install: {other}"));
            }
            _ => positional.push(arg.as_str()),
        }
    }
    match positional.as_slice() {
        [target] => Ok(InstallSkillArgs {
            target,
            confirm,
            overwrite,
        }),
        _ => Err("usage: /skills install [--confirm] [--overwrite] <https-url|path>".into()),
    }
}

fn skill_install_source(target: &str, cwd: &std::path::Path) -> serde_json::Value {
    if target.starts_with("http://") || target.starts_with("https://") {
        json!({ "type": "url", "url": target })
    } else {
        let path = std::path::PathBuf::from(target);
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        json!({ "type": "path", "path": path.to_string_lossy().to_string() })
    }
}

fn print_install_skill_result(result: &pie_agent_core::AgentToolResult, args: &InstallSkillArgs) {
    let phase = result.details.get("phase").and_then(|v| v.as_str());
    if phase == Some("preview") {
        let name = result
            .details
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let target = result
            .details
            .get("target_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let size = result
            .details
            .get("size")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let existing = result
            .details
            .get("existing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let overwrite_required = result
            .details
            .get("overwrite_required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        cprintln!(
            "skill install preview: {name} -> {target} ({size}B, existing={existing}, overwrite_required={overwrite_required})"
        );
        let overwrite = if overwrite_required && !args.overwrite {
            " --overwrite"
        } else {
            ""
        };
        cprintln!("run `/skills install --confirm{overwrite} <same-url-or-path>` to install");
        return;
    }
    for line in tool_result_text(result).lines() {
        cprintln!("{line}");
    }
}

async fn set_skill_enabled(argv: &[String], ctx: &CommandCtx<'_>, enabled: bool) -> CommandOutcome {
    let Some(name) = argv.first() else {
        let verb = if enabled { "enable" } else { "disable" };
        return CommandOutcome::Error(format!("usage: /skills {verb} <name> [source]"));
    };
    let source = match optional_skill_source(argv.get(1)) {
        Ok(source) => source,
        Err(e) => return CommandOutcome::Error(e),
    };
    let skills = ctx.harness.skills();
    let skill = match resolve_active_skill(&skills, name, source) {
        Ok(skill) => skill,
        Err(e) => return CommandOutcome::Error(e),
    };
    let source = skill.source;
    let name = skill.name.clone();
    let was_enabled = !skill.disable_model_invocation;

    if was_enabled == enabled {
        cprintln!(
            "skill already {}: {} ({})",
            if enabled { "enabled" } else { "disabled" },
            name,
            source.label()
        );
        return CommandOutcome::Handled;
    }

    if let Err(e) =
        crate::skills_state::set_and_save(&crate::config::base_dir(), &name, source, enabled).await
    {
        return CommandOutcome::Error(format!("persist skill state failed: {e}"));
    }
    match ctx.harness.reload_skills_from_disk().await {
        Ok(out) => {
            write_skill_state_audit(ctx, &name, source, was_enabled, enabled).await;
            let diagnostics = if out.diagnostics.is_empty() {
                String::new()
            } else {
                format!(" ({} diagnostics)", out.diagnostics.len())
            };
            cprintln!(
                "{} skill: {} ({}){}",
                if enabled { "enabled" } else { "disabled" },
                name,
                source.label(),
                diagnostics
            );
            CommandOutcome::Handled
        }
        Err(e) => CommandOutcome::Error(format!("reload after skill state change failed: {e}")),
    }
}

async fn remove_skill(argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
    let parsed = match parse_skill_remove_args(argv) {
        Ok(parsed) => parsed,
        Err(e) => return CommandOutcome::Error(e),
    };
    let mut params = json!({
        "name": parsed.name,
        "confirm": parsed.confirm,
    });
    if let Some(source) = parsed.source {
        params["source"] = json!(source.label());
    }
    let cell = skill_harness_cell(ctx);
    let tool = crate::tools::remove_skill::RemoveSkillTool::new(cell);
    match tool
        .execute(
            "slash-skills-remove",
            params,
            CancellationToken::new(),
            None,
        )
        .await
    {
        Ok(result) => {
            print_remove_skill_result(&result);
            CommandOutcome::Handled
        }
        Err(e) => CommandOutcome::Error(format!("remove skill failed: {e}")),
    }
}

struct RemoveSkillArgs<'a> {
    name: &'a str,
    source: Option<SkillSource>,
    confirm: bool,
}

fn parse_skill_remove_args(argv: &[String]) -> Result<RemoveSkillArgs<'_>, String> {
    let mut confirm = false;
    let mut positional = Vec::new();
    for arg in argv {
        match arg.as_str() {
            "--confirm" | "--yes" => confirm = true,
            other if other.starts_with("--") => {
                return Err(format!("unknown option for /skills remove: {other}"));
            }
            _ => positional.push(arg.as_str()),
        }
    }
    match positional.as_slice() {
        [name] => Ok(RemoveSkillArgs {
            name,
            source: None,
            confirm,
        }),
        [name, source] => Ok(RemoveSkillArgs {
            name,
            source: Some(parse_skill_source(source)?),
            confirm,
        }),
        _ => Err("usage: /skills remove [--confirm] <name> [source]".into()),
    }
}

fn print_remove_skill_result(result: &pie_agent_core::AgentToolResult) {
    let phase = result.details.get("phase").and_then(|v| v.as_str());
    if phase == Some("preview") {
        let name = result
            .details
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        let target = result
            .details
            .get("target_path")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        cprintln!("skill remove preview: {name} (user) -> {target}");
        cprintln!("run `/skills remove --confirm {name}` to remove it");
        return;
    }
    for line in tool_result_text(result).lines() {
        cprintln!("{line}");
    }
}

fn skill_harness_cell(ctx: &CommandCtx<'_>) -> crate::tools::skill::SkillHarnessCell {
    let cell = std::sync::Arc::new(once_cell::sync::OnceCell::new());
    // This is a fresh cell scoped to a single slash command invocation, so set() can only fail
    // if this helper is called incorrectly inside the same invocation.
    let _ = cell.set(ctx.harness.clone());
    cell
}

fn tool_result_text(result: &pie_agent_core::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text(text) => Some(text.text.as_str()),
            UserContentBlock::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn write_skill_state_audit(
    ctx: &CommandCtx<'_>,
    name: &str,
    source: SkillSource,
    before_enabled: bool,
    after_enabled: bool,
) {
    let audit = json!({
        "op": "set_state",
        "actor": "slash",
        "name": name,
        "source": source.label(),
        "before_enabled": before_enabled,
        "after_enabled": after_enabled,
    });
    if let Err(e) = ctx
        .harness
        .session()
        .append_custom("skill_control_plane", Some(audit))
        .await
    {
        tracing::warn!(
            skill = %name,
            error = %e,
            "skill_control_plane audit write failed; slash state change itself succeeded"
        );
    }
}

fn optional_skill_source(raw: Option<&String>) -> Result<Option<SkillSource>, String> {
    raw.map(|s| parse_skill_source(s).map(Some))
        .unwrap_or(Ok(None))
}

fn parse_skill_source(raw: &str) -> Result<SkillSource, String> {
    match raw {
        "builtin" => Ok(SkillSource::Builtin),
        "user" => Ok(SkillSource::User),
        "project" => Ok(SkillSource::Project),
        _ => Err("invalid skill source; expected one of: builtin, user, project".into()),
    }
}

fn resolve_active_skill<'a>(
    skills: &'a [Skill],
    name: &str,
    source: Option<SkillSource>,
) -> Result<&'a Skill, String> {
    let matches = skills
        .iter()
        .filter(|skill| skill.name == name && source.map(|s| skill.source == s).unwrap_or(true))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [skill] => Ok(*skill),
        [] => {
            let source_hint = source
                .map(|source| format!(" {} ", source.label()))
                .unwrap_or_else(|| " ".into());
            Err(format!(
                "no active{source_hint}skill named '{name}'. Run /skills to list loaded skills."
            ))
        }
        _ => Err(format!(
            "multiple active skills named '{name}'; pass source: builtin, user, or project"
        )),
    }
}

struct SkillCommand;

#[async_trait]
impl SlashCommand for SkillCommand {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn description(&self) -> &'static str {
        "attach a loaded skill to the next prompt"
    }
    fn usage(&self) -> &'static str {
        "<name>"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.len() != 1 {
            return CommandOutcome::Error("usage: /skill <name>".into());
        }
        let name = &argv[0];
        let skills = ctx.harness.skills();
        let Some(skill) = skills.iter().find(|s| s.name == *name) else {
            let mut matches = skills
                .iter()
                .filter(|s| s.name.starts_with(name))
                .map(|s| s.name.as_str())
                .take(5)
                .collect::<Vec<_>>();
            if matches.is_empty() {
                matches = skills
                    .iter()
                    .filter(|s| s.name.contains(name))
                    .map(|s| s.name.as_str())
                    .take(5)
                    .collect::<Vec<_>>();
            }
            let hint = if matches.is_empty() {
                "".to_string()
            } else {
                format!(" Did you mean: {}?", matches.join(", "))
            };
            return CommandOutcome::Error(format!(
                "no skill named '{name}'. Run /skills to list loaded skills.{hint}"
            ));
        };
        if skill.disable_model_invocation {
            return CommandOutcome::Error(format!(
                "skill '{name}' is disabled (disable_model_invocation=true); edit the skill frontmatter to enable it"
            ));
        }
        cprintln!(
            "using skill: {} ({}) for next turn",
            skill.name,
            skill.source.label()
        );
        CommandOutcome::AttachSkill { name: name.clone() }
    }
}

pub fn attach_skill_prompt(text: impl Into<String>, skill_name: Option<&str>) -> String {
    let text = text.into();
    let Some(skill_name) = skill_name else {
        return text;
    };
    format!(
        "Before answering, invoke the Skill tool with name \"{skill_name}\" and use that skill's instructions for this turn.\n\nUser request:\n{text}"
    )
}

struct QuitCommand;

#[async_trait]
impl SlashCommand for QuitCommand {
    fn name(&self) -> &'static str {
        "quit"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["exit", "q"]
    }
    fn description(&self) -> &'static str {
        "exit the REPL"
    }
    async fn run(&self, _argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        CommandOutcome::Quit
    }
}

struct ModelCommand;

#[async_trait]
impl SlashCommand for ModelCommand {
    fn name(&self) -> &'static str {
        "model"
    }
    fn description(&self) -> &'static str {
        "show or switch the active model"
    }
    fn usage(&self) -> &'static str {
        "[provider:model-id|list [provider]]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            let current = ctx.harness.agent().state().model.clone();
            match current {
                Some(m) => cprintln!("active model: {}:{}", m.provider.0, m.id),
                None => cprintln!("(no model active)"),
            }
            cprintln!("Use /model list [provider] to show supported providers and models.");
            return CommandOutcome::Handled;
        }
        if matches!(argv.first().map(|s| s.as_str()), Some("list" | "ls")) {
            let provider = argv.get(1).map(String::as_str);
            match model_catalog_text(provider) {
                Ok(text) => emit_multiline(&text),
                Err(e) => return CommandOutcome::Error(e),
            }
            return CommandOutcome::Handled;
        }
        // Accept `provider:id`, the user's natural `provider/model-id`, or two separate
        // args `provider id`.
        let spec = argv.join(" ");
        let (provider, id) = match parse_model_spec(&spec) {
            Some((p, i)) => (p.to_string(), i.to_string()),
            None => {
                return CommandOutcome::Error(
                    "expected provider:model-id (provider/model-id also works), e.g. /model anthropic:claude-haiku-4-5".into(),
                );
            }
        };
        let provider_obj = Provider::from(provider.as_str());
        let Some(model) = get_model(&provider_obj, &id) else {
            return CommandOutcome::Error(unknown_model_error(&provider, &id));
        };
        match ctx.harness.set_model(model.clone()).await {
            Ok(_) => {
                if let Some(hint) = model_credential_hint(&provider) {
                    cprintln!("selected {provider}:{id}, but login is required: {hint}");
                } else {
                    cprintln!("switched to {provider}:{id}");
                }
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set_model failed: {e}")),
        }
    }
}

fn parse_model_spec(spec: &str) -> Option<(&str, &str)> {
    let spec = spec.trim();
    let (provider, id) = spec
        .split_once(':')
        .or_else(|| spec.split_once('/'))
        .or_else(|| spec.split_once(char::is_whitespace))?;
    let provider = provider.trim();
    let id = id.trim();
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some((provider, id))
}

fn model_credential_hint(provider: &str) -> Option<String> {
    let vars = pie_ai::env_api_keys::env_var_names(provider);
    let has_env = vars.iter().any(|var| {
        std::env::var(var)
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    });
    if has_env {
        return None;
    }
    let has_stored = crate::auth::AuthStore::load()
        .ok()
        .and_then(|store| store.get(provider).cloned())
        .is_some();
    if has_stored {
        return None;
    }

    let env_hint = if vars.is_empty() {
        "set the provider API key env var".to_string()
    } else {
        format!("set {}", vars.join(" or "))
    };
    Some(format!("{env_hint} or run /login {provider}"))
}

struct ThinkingCommand;

#[async_trait]
impl SlashCommand for ThinkingCommand {
    fn name(&self) -> &'static str {
        "thinking"
    }
    fn description(&self) -> &'static str {
        "show or set the thinking level"
    }
    fn usage(&self) -> &'static str {
        THINKING_LEVEL_USAGE
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            let lvl = ctx.harness.agent().state().thinking_level;
            cprintln!("thinking level: {}", lvl.map(|l| l.as_str()).unwrap_or("?"));
            return CommandOutcome::Handled;
        }
        let raw = argv[0].to_lowercase();
        let level: ThinkingLevel = match raw.parse() {
            Ok(l) => l,
            Err(e) => {
                return CommandOutcome::Error(format!("invalid level: {e}"));
            }
        };
        match ctx.harness.set_thinking_level(level).await {
            Ok(_) => {
                cprintln!("thinking level: {}", level.as_str());
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set_thinking_level failed: {e}")),
        }
    }
}

struct GoalCommand;

fn goal_start_prompt(argv: &[String]) -> String {
    argv.join(" ").trim().to_string()
}

async fn run_goal_start(prompt: String, ctx: &CommandCtx<'_>) -> CommandOutcome {
    if prompt.is_empty() {
        return CommandOutcome::Error("usage: /goal-start <prompt>".into());
    }
    if let Err(e) = crate::goal::current(ctx.harness)
        .await
        .filter(|state| state.active())
        .ok_or_else(|| "no active goal; set one with /goal <condition>".to_string())
    {
        return CommandOutcome::Error(e);
    }
    CommandOutcome::RunAgentPrompt {
        prompt,
        error_context: "goal start: ",
    }
}

#[async_trait]
impl SlashCommand for GoalCommand {
    fn name(&self) -> &'static str {
        "goal"
    }

    fn description(&self) -> &'static str {
        "set, view, pause, resume, or clear the session goal stop hook"
    }

    fn usage(&self) -> &'static str {
        "[<condition>|start <prompt>|pause|resume|clear]"
    }

    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        match argv.first().map(String::as_str) {
            None => {
                print_goal_status(ctx).await;
                CommandOutcome::Handled
            }
            Some("pause") if argv.len() == 1 => match crate::goal::pause(ctx.harness).await {
                Ok(state) => {
                    cprintln!("goal paused: {}", state.condition);
                    CommandOutcome::Handled
                }
                Err(e) => CommandOutcome::Error(e),
            },
            Some("resume") if argv.len() == 1 => match crate::goal::resume(ctx.harness).await {
                Ok(state) => {
                    cprintln!("goal resumed: {}", state.condition);
                    CommandOutcome::Handled
                }
                Err(e) => CommandOutcome::Error(e),
            },
            Some("clear") if argv.len() == 1 => match crate::goal::clear(ctx.harness).await {
                Ok(_) => {
                    cprintln!("goal cleared");
                    CommandOutcome::Handled
                }
                Err(e) => CommandOutcome::Error(e),
            },
            Some("start") => run_goal_start(goal_start_prompt(&argv[1..]), ctx).await,
            Some(_) => {
                let condition = argv.join(" ").trim().to_string();
                if condition.is_empty() {
                    return CommandOutcome::Error("usage: /goal <condition>".into());
                }
                match crate::goal::set(ctx.harness, condition).await {
                    Ok(state) => {
                        cprintln!("goal set: {}", state.condition);
                        cprintln!(
                            "goal will continue after each successful turn until transcript evidence satisfies the condition"
                        );
                        cprintln!("start by sending a normal prompt, or run /goal-start <prompt>");
                        CommandOutcome::Handled
                    }
                    Err(e) => CommandOutcome::Error(e),
                }
            }
        }
    }
}

struct GoalStartCommand;

#[async_trait]
impl SlashCommand for GoalStartCommand {
    fn name(&self) -> &'static str {
        "goal-start"
    }

    fn description(&self) -> &'static str {
        "start working on the active session goal with a prompt"
    }

    fn usage(&self) -> &'static str {
        "<prompt>"
    }

    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        run_goal_start(goal_start_prompt(argv), ctx).await
    }
}

async fn print_goal_status(ctx: &CommandCtx<'_>) {
    match crate::goal::current(ctx.harness).await {
        Some(state) if state.active() || state.status == crate::goal::GoalStatus::Achieved => {
            cprintln!("goal: {}", state.condition);
            cprintln!("status: {}", state.status.as_str());
            cprintln!("iterations: {}", state.iterations);
            if let Some(reason) = state.last_reason.as_deref() {
                cprintln!("last evaluator reason: {}", preview_text(reason, 240));
            }
        }
        _ => {
            cprintln!("no active goal; set one with /goal <condition>");
        }
    }
}

#[allow(dead_code)]
pub fn print_help(registry: &Registry, topic: Option<&str>) {
    emit_multiline(&help_text_with_skills(registry, topic, &[]));
}

pub fn print_help_with_skills(registry: &Registry, topic: Option<&str>, skills: &[Skill]) {
    emit_multiline(&help_text_with_skills(registry, topic, skills));
}

#[allow(dead_code)]
fn help_text(registry: &Registry, topic: Option<&str>) -> String {
    help_text_with_skills(registry, topic, &[])
}

fn help_text_with_skills(registry: &Registry, topic: Option<&str>, skills: &[Skill]) -> String {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return general_help_text(registry, skills);
    };
    let topic = topic.trim_start_matches('/');
    if topic == "models" {
        return model_catalog_text(None).unwrap_or_else(|e| e);
    }

    command_help_text(registry, topic, skills)
}

fn general_help_text(registry: &Registry, skills: &[Skill]) -> String {
    let mut lines = vec![String::new(), "Commands:".into()];
    for cmd in registry.commands() {
        let aliases = if cmd.aliases().is_empty() {
            String::new()
        } else {
            format!(" (aliases: {})", cmd.aliases().join(", "))
        };
        let usage = if cmd.usage().is_empty() {
            String::new()
        } else {
            format!(" {}", cmd.usage())
        };
        lines.push(format!(
            "  /{}{}    {}{}",
            cmd.name(),
            usage,
            cmd.description(),
            aliases
        ));
    }
    let shortcuts = skill_shortcuts(skills, registry);
    if !shortcuts.is_empty() {
        lines.push(String::new());
        lines.push("Skill commands:".into());
        for shortcut in shortcuts {
            let description = if shortcut.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", shortcut.description)
            };
            lines.push(format!(
                "  {} [prompt]    use loaded skill ({}){}",
                shortcut.command,
                shortcut.source.label(),
                description
            ));
        }
    }
    lines.push(String::new());
    lines.push("Models:".into());
    for line in model_help_summary_lines() {
        lines.push(line);
    }
    lines.push(String::new());
    lines.push("Anything else is sent as a prompt to the agent.".into());
    lines.push(String::new());
    lines.join("\n")
}

fn command_help_text(registry: &Registry, topic: &str, skills: &[Skill]) -> String {
    let Some(cmd) = registry.find(topic) else {
        if let Ok(Some(skill)) = resolve_skill_shortcut(skills, registry, topic) {
            let mut lines = vec![
                format!("/{topic} [prompt]"),
                format!(
                    "  use loaded skill '{}' ({})",
                    skill.name,
                    skill.source.label()
                ),
            ];
            if !skill.description.is_empty() {
                lines.push(format!("  {}", preview_text(&skill.description, 120)));
            }
            lines.push(format!("  equivalent: /skill {}", skill.name));
            return lines.join("\n");
        }
        let suggestions = registry
            .commands()
            .iter()
            .filter(|cmd| cmd.name().starts_with(topic) || cmd.aliases().contains(&topic))
            .map(|cmd| format!("/{}", cmd.name()))
            .chain(
                skill_shortcuts(skills, registry)
                    .into_iter()
                    .filter(|shortcut| shortcut.command[1..].starts_with(topic))
                    .map(|shortcut| shortcut.command),
            )
            .take(5)
            .collect::<Vec<_>>();
        let suggestion = if suggestions.is_empty() {
            "Run /help to list commands or /help models for the model catalog.".to_string()
        } else {
            format!("Did you mean {}?", suggestions.join(", "))
        };
        return format!("unknown help topic: {topic}\n{suggestion}");
    };

    let usage = if cmd.usage().is_empty() {
        format!("/{}", cmd.name())
    } else {
        format!("/{} {}", cmd.name(), cmd.usage())
    };
    let mut lines = vec![usage, format!("  {}", cmd.description())];
    if !cmd.aliases().is_empty() {
        let aliases = cmd
            .aliases()
            .iter()
            .map(|alias| format!("/{alias}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("  aliases: {aliases}"));
    }
    if cmd.name() == "help" {
        lines.push("  examples: /help model, /help /quit, /help models".into());
    } else {
        lines.push(format!("  more: /help {}", cmd.name()));
    }
    lines.join("\n")
}

pub fn cli_model_help_text() -> String {
    let mut out = String::new();
    out.push_str("Model catalog:\n");
    for line in model_help_summary_lines() {
        out.push_str("  ");
        out.push_str(line.trim_start());
        out.push('\n');
    }
    out
}

fn emit_multiline(text: &str) {
    for line in text.lines() {
        cprintln!("{line}");
    }
}

fn model_help_summary_lines() -> Vec<String> {
    let groups = model_groups();
    let total = groups.values().map(Vec::len).sum::<usize>();
    vec![
        format!(
            "  Supported providers ({}), models ({}): {}",
            groups.len(),
            total,
            provider_summary(&groups)
        ),
        "  Full list: /help models or /model list [provider]".into(),
        "  Custom models: ~/.pie/models.json and <cwd>/.pie/models.json".into(),
        "  Credentials: set provider env vars or run /login <provider>.".into(),
    ]
}

fn model_catalog_text(provider_filter: Option<&str>) -> Result<String, String> {
    let groups = model_groups();
    let total = groups.values().map(Vec::len).sum::<usize>();
    let mut out = Vec::new();
    match provider_filter {
        Some(provider) => {
            let Some(models) = groups.get(provider) else {
                return Err(unknown_provider_error(provider, &groups));
            };
            out.push(format!(
                "Supported models for provider '{provider}' ({}):",
                models.len()
            ));
            append_model_lines(&mut out, models);
        }
        None => {
            out.push(format!(
                "Supported providers/models: {} providers, {} models",
                groups.len(),
                total
            ));
            out.push(
                "Custom models are loaded from ~/.pie/models.json and <cwd>/.pie/models.json."
                    .into(),
            );
            for (provider, models) in &groups {
                out.push(format!("  {provider} ({})", models.len()));
                append_model_lines(&mut out, models);
            }
        }
    }
    Ok(out.join("\n"))
}

fn model_groups() -> BTreeMap<String, Vec<Model>> {
    let mut groups: BTreeMap<String, Vec<Model>> = BTreeMap::new();
    for model in list_models() {
        groups
            .entry(model.provider.0.clone())
            .or_default()
            .push(model);
    }
    for models in groups.values_mut() {
        models.sort_by(|a, b| a.id.cmp(&b.id));
    }
    groups
}

fn provider_summary(groups: &BTreeMap<String, Vec<Model>>) -> String {
    groups
        .iter()
        .map(|(provider, models)| format!("{provider}({})", models.len()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn append_model_lines(out: &mut Vec<String>, models: &[Model]) {
    for model in models {
        if model.name.trim().is_empty() || model.name == model.id {
            out.push(format!("    - {}", model.id));
        } else {
            out.push(format!("    - {} — {}", model.id, model.name));
        }
    }
}

fn unknown_provider_error(provider: &str, groups: &BTreeMap<String, Vec<Model>>) -> String {
    format!(
        "unknown provider '{provider}'. Known providers: {}",
        provider_summary(groups)
    )
}

fn unknown_model_error(provider: &str, id: &str) -> String {
    let groups = model_groups();
    let Some(models) = groups.get(provider) else {
        return unknown_provider_error(provider, &groups);
    };
    let candidates = models
        .iter()
        .take(12)
        .map(|m| m.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let more = if models.len() > 12 {
        format!(
            "; run /model list {provider} for all {} models",
            models.len()
        )
    } else {
        String::new()
    };
    format!("unknown model in catalog: {provider}:{id}. Candidates: {candidates}{more}")
}

struct CostCommand;

#[async_trait]
impl SlashCommand for CostCommand {
    fn name(&self) -> &'static str {
        "cost"
    }
    fn description(&self) -> &'static str {
        "show running token / USD totals for this session"
    }
    fn usage(&self) -> &'static str {
        "[reset]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.first().map(|s| s.as_str()) == Some("reset") {
            ctx.harness.reset_cost();
            cprintln!("cost counters reset");
            return CommandOutcome::Handled;
        }
        let snap = ctx.harness.cost();
        cprintln!("{}", pie_agent_core::cost_full_breakdown(&snap));
        CommandOutcome::Handled
    }
}

struct DiagCommand;

#[async_trait]
impl SlashCommand for DiagCommand {
    fn name(&self) -> &'static str {
        "diag"
    }
    fn description(&self) -> &'static str {
        "show diagnostic info (model, thinking, cost, log path)"
    }
    async fn run(&self, _argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let state = ctx.harness.agent().state();
        let model = state
            .model
            .as_ref()
            .map(|m| format!("{}:{}", m.provider.0, m.id))
            .unwrap_or_else(|| "(none)".into());
        let thinking = state
            .thinking_level
            .map(|l| l.as_str())
            .unwrap_or("?")
            .to_string();
        let skill_count = ctx.harness.skills().len();
        let cost = ctx.harness.cost();
        let log = ctx
            .log_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(logging disabled)".into());
        cprintln!();
        cprintln!("Diagnostic snapshot:");
        cprintln!("  session       {}", ctx.session_id);
        cprintln!("  model         {model}");
        cprintln!("  thinking      {thinking}");
        cprintln!("  tools         {}", ctx.tool_count);
        cprintln!("  skills        {skill_count}");
        cprintln!(
            "  cost          {}",
            pie_agent_core::cost_one_line_summary(&cost)
        );
        cprintln!("  log file      {log}");
        cprintln!();
        CommandOutcome::Handled
    }
}

struct TemplateCommand;

#[async_trait]
impl SlashCommand for TemplateCommand {
    fn name(&self) -> &'static str {
        "template"
    }
    fn description(&self) -> &'static str {
        "list templates, or run one with /template <name> [k=v ...]"
    }
    fn usage(&self) -> &'static str {
        "[name] [k=v ...]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            let templates = ctx.harness.templates();
            if templates.is_empty() {
                cprintln!(
                    "(no templates loaded — drop `.md` files under ~/.pie/templates/ or <cwd>/.pie/templates/)"
                );
            } else {
                cprintln!("Loaded templates ({}):", templates.len());
                for t in &templates {
                    let desc = t.description.clone().unwrap_or_default();
                    cprintln!("  /template {}  {}", t.name, desc);
                }
            }
            return CommandOutcome::Handled;
        }
        let name = argv[0].clone();
        // Remaining args are `k=v` pairs.
        let mut vars = serde_json::Map::new();
        for arg in &argv[1..] {
            if let Some((k, v)) = arg.split_once('=') {
                vars.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            } else {
                return CommandOutcome::Error(format!("expected k=v argument; got: {arg}"));
            }
        }
        CommandOutcome::RunPromptTemplate { name, vars }
    }
}

struct SaveCommand;

#[async_trait]
impl SlashCommand for SaveCommand {
    fn name(&self) -> &'static str {
        "save"
    }
    fn description(&self) -> &'static str {
        "export session transcript to Markdown"
    }
    fn usage(&self) -> &'static str {
        "[path]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let dest = if let Some(path) = argv.first() {
            std::path::PathBuf::from(path)
        } else {
            crate::export::default_export_path(ctx.session_id)
        };
        // If the path is relative, resolve against cwd so /save foo.md lands where the user
        // expects (and not in some random working dir).
        let dest = if dest.is_absolute() {
            dest
        } else {
            ctx.cwd.join(dest)
        };
        match crate::export::save(ctx.harness.session(), &dest).await {
            Ok(p) => {
                cprintln!("saved transcript: {}", p.display());
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("save failed: {e}")),
        }
    }
}

struct CompactCommand;

#[async_trait]
impl SlashCommand for CompactCommand {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn description(&self) -> &'static str {
        "force a context compaction now (no-op when nothing to summarize)"
    }
    fn usage(&self) -> &'static str {
        "[\"custom instructions\"]"
    }
    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        let custom = if argv.is_empty() {
            None
        } else {
            Some(argv.join(" "))
        };
        CommandOutcome::RunCompaction { custom }
    }
}

struct UndoCommand;

#[async_trait]
impl SlashCommand for UndoCommand {
    fn name(&self) -> &'static str {
        "undo"
    }
    fn description(&self) -> &'static str {
        "remove the most recent user+assistant turn from the active branch"
    }
    async fn run(&self, _argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let session = ctx.harness.session();
        let path = match session.branch(None).await {
            Ok(p) => p,
            Err(e) => return CommandOutcome::Error(format!("read branch: {e}")),
        };
        // Walk backwards for the most recent Message that's a User. That message is the
        // start of the turn we want to drop.
        let mut target_parent: Option<String> = None;
        let mut found = false;
        for entry in path.iter().rev() {
            if let pie_agent_core::SessionTreeEntry::Message {
                message: pie_agent_core::AgentMessage::Llm(pie_ai::Message::User(_)),
                parent_id,
                ..
            } = entry
            {
                target_parent = parent_id.clone();
                found = true;
                break;
            }
        }
        if !found {
            return CommandOutcome::Error("no user message to undo".into());
        }
        match ctx.harness.move_to(target_parent.as_deref(), None).await {
            Ok(_) => {
                cprintln!("undid last turn");
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("undo failed: {e}")),
        }
    }
}

struct BugReportCommand;

#[async_trait]
impl SlashCommand for BugReportCommand {
    fn name(&self) -> &'static str {
        "bug-report"
    }
    fn description(&self) -> &'static str {
        "write a redacted diagnostic dump for issue attachment"
    }
    async fn run(&self, _argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        // Snapshot the model + thinking with the lock held briefly; the MutexGuard cannot
        // cross an .await so we copy what we need and drop it.
        let (model, thinking) = {
            let state = ctx.harness.agent().state();
            let m = state
                .model
                .as_ref()
                .map(|m| format!("{}:{}", m.provider.0, m.id));
            let t = state
                .thinking_level
                .map(|l| l.as_str())
                .unwrap_or("?")
                .to_string();
            (m, t)
        };
        let cost = ctx.harness.cost();
        let diag = crate::bug_report::DiagInputs {
            session_id: ctx.session_id.to_string(),
            model,
            thinking,
            tool_count: ctx.tool_count,
            skill_count: ctx.harness.skills().len(),
            cost_summary: pie_agent_core::cost_one_line_summary(&cost),
            log_path: ctx.log_path.cloned(),
        };
        let dest = crate::bug_report::default_dest();
        match crate::bug_report::build(diag, ctx.harness.session(), &dest).await {
            Ok(path) => {
                cprintln!("wrote bug report: {}", path.display());
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("bug-report failed: {e}")),
        }
    }
}

struct NameCommand;

#[async_trait]
impl SlashCommand for NameCommand {
    fn name(&self) -> &'static str {
        "name"
    }
    fn description(&self) -> &'static str {
        "show or set the current session's name"
    }
    fn usage(&self) -> &'static str {
        "[slug]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let session = ctx.harness.session();
        if argv.is_empty() {
            match session.session_name().await {
                Ok(Some(n)) => cprintln!("session name: {n}"),
                Ok(None) => cprintln!("(unnamed session)"),
                Err(e) => return CommandOutcome::Error(format!("read name: {e}")),
            }
            return CommandOutcome::Handled;
        }
        let name = argv.join(" ");
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return CommandOutcome::Error("empty name".into());
        }
        match session.append_session_name(trimmed.to_string()).await {
            Ok(_) => {
                cprintln!("session name set to: {trimmed}");
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set name failed: {e}")),
        }
    }
}

struct SessionCommand;

#[async_trait]
impl SlashCommand for SessionCommand {
    fn name(&self) -> &'static str {
        "session"
    }
    fn description(&self) -> &'static str {
        "export/import replayable .piesession backups"
    }
    fn usage(&self) -> &'static str {
        "export [path] [--exclude-triggers] | import <path>"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        match argv.first().map(String::as_str) {
            Some("export") => session_export_command(&argv[1..], ctx).await,
            Some("import") => session_import_command(&argv[1..], ctx).await,
            Some(other) => CommandOutcome::Error(format!(
                "unknown /session subcommand: {other}; use /session export [path] or /session import <path>"
            )),
            None => CommandOutcome::Error(
                "usage: /session export [path] [--exclude-triggers] | /session import <path>"
                    .into(),
            ),
        }
    }
}

async fn session_export_command(argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
    let mut exclude_triggers = false;
    let mut path_arg: Option<&str> = None;
    for arg in argv {
        if arg == "--exclude-triggers" {
            exclude_triggers = true;
        } else if path_arg.is_none() {
            path_arg = Some(arg);
        } else {
            return CommandOutcome::Error(
                "usage: /session export [path] [--exclude-triggers]".into(),
            );
        }
    }

    let metadata = match ctx.harness.session().storage().get_metadata_json().await {
        Ok(metadata) => metadata,
        Err(err) => return CommandOutcome::Error(format!("read session metadata: {err}")),
    };
    let Some(session_path) = metadata
        .get("path")
        .and_then(|value| value.as_str())
        .map(std::path::PathBuf::from)
    else {
        return CommandOutcome::Error("session metadata is missing transcript path".into());
    };
    let output_path = match path_arg {
        Some(path) => std::path::PathBuf::from(path),
        None => crate::session_archive::default_export_path(ctx.cwd, ctx.session_id),
    };
    let output_path = if output_path.is_absolute() {
        output_path
    } else {
        ctx.cwd.join(output_path)
    };

    emit_session_archive_warning();
    match crate::session_archive::export_session(&session_path, &output_path, exclude_triggers)
        .await
    {
        Ok(summary) => {
            cprintln!(
                "exported session archive: {}",
                summary.output_path.display()
            );
            cprintln!(
                "session {} entries={} triggers={} cron={}",
                short_id(&summary.session_id),
                summary.entry_count,
                yes_no(summary.has_triggers),
                yes_no(summary.has_cron)
            );
            CommandOutcome::Handled
        }
        Err(err) => CommandOutcome::Error(format!("session export failed: {err}")),
    }
}

async fn session_import_command(argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
    if argv.len() != 1 {
        return CommandOutcome::Error("usage: /session import <path>".into());
    }
    let archive_path = std::path::PathBuf::from(&argv[0]);
    let archive_path = if archive_path.is_absolute() {
        archive_path
    } else {
        ctx.cwd.join(archive_path)
    };
    let repo = crate::session::open_repo(ctx.cwd).await;

    emit_session_archive_warning();
    match crate::session_archive::import_session(
        &repo,
        &archive_path,
        ctx.cwd,
        crate::session_archive::ActivateTriggers::Off,
    )
    .await
    {
        Ok(summary) => {
            cprintln!("imported session: {}", short_id(&summary.session_id));
            cprintln!("path: {}", summary.session_path.display());
            cprintln!(
                "entries={} triggers={} cron={} automation={}",
                summary.entry_count,
                summary.triggers_imported,
                summary.cron_imported,
                if summary.automation_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            cprintln!("resume with: pie --resume-id {}", summary.session_id);
            CommandOutcome::Handled
        }
        Err(err) => CommandOutcome::Error(format!("session import failed: {err}")),
    }
}

fn emit_session_archive_warning() {
    cprintln!(
        "warning: .piesession archives include transcript and tool history. They do not include separate auth stores, provider credentials, OAuth tokens, or MCP config."
    );
}

fn short_id(id: &str) -> String {
    id.chars().take(16).collect()
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

struct WebConnectCommand;

#[async_trait]
impl SlashCommand for WebConnectCommand {
    fn name(&self) -> &'static str {
        "web-connect"
    }
    fn description(&self) -> &'static str {
        "mount this session at the public relay (watch + prompt via secret URL)"
    }
    fn usage(&self) -> &'static str {
        "[status]"
    }
    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        match argv.first().map(String::as_str) {
            None => CommandOutcome::WebRelay(WebRelayAction::Connect),
            Some("status") => CommandOutcome::WebRelay(WebRelayAction::Status),
            Some(other) => CommandOutcome::Error(format!("unknown /web-connect argument: {other}")),
        }
    }
}

struct WebDisconnectCommand;

#[async_trait]
impl SlashCommand for WebDisconnectCommand {
    fn name(&self) -> &'static str {
        "web-disconnect"
    }
    fn description(&self) -> &'static str {
        "disconnect the public relay and invalidate the session URL"
    }
    async fn run(&self, _argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        CommandOutcome::WebRelay(WebRelayAction::Disconnect)
    }
}

struct SessionsCommand;

#[async_trait]
impl SlashCommand for SessionsCommand {
    fn name(&self) -> &'static str {
        "sessions"
    }
    fn description(&self) -> &'static str {
        "list sessions for this cwd"
    }
    async fn run(&self, _argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let repo = crate::session::open_repo(ctx.cwd).await;
        let entries = match crate::session::list_entries(&repo).await {
            Ok(e) => e,
            Err(e) => return CommandOutcome::Error(format!("list sessions: {e}")),
        };
        if entries.is_empty() {
            cprintln!("(no sessions for this cwd)");
            return CommandOutcome::Handled;
        }
        cprintln!("Sessions:");
        for e in entries {
            let preview = e.preview.as_deref().unwrap_or("");
            let id_short: String = e.id.chars().take(16).collect();
            cprintln!("  {}  {}  {}", id_short, e.created_at, preview);
        }
        CommandOutcome::Handled
    }
}

struct ShareCommand;

#[async_trait]
impl SlashCommand for ShareCommand {
    fn name(&self) -> &'static str {
        "share"
    }
    fn description(&self) -> &'static str {
        "upload transcript as a private Gist via gh (requires `gh` on PATH)"
    }
    fn usage(&self) -> &'static str {
        "[--public]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let public = argv.iter().any(|a| a == "--public");

        // Render and write to a temp file so gh gist create can ingest it.
        let dir = std::env::temp_dir().join(format!("pie-share-{}", ctx.session_id));
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            return CommandOutcome::Error(format!("share tmp dir: {e}"));
        }
        let file = dir.join("transcript.md");
        if let Err(e) = crate::export::save(ctx.harness.session(), &file).await {
            return CommandOutcome::Error(format!("save transcript: {e}"));
        }

        let mut cmd = tokio::process::Command::new("gh");
        cmd.arg("gist").arg("create");
        if public {
            cmd.arg("--public");
        }
        cmd.arg("--desc")
            .arg(format!("pie session {}", ctx.session_id))
            .arg(file.as_os_str());

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => {
                return CommandOutcome::Error(format!(
                    "gh gist create failed to spawn: {e}. Is gh on PATH?"
                ));
            }
        };
        if !output.status.success() {
            return CommandOutcome::Error(format!(
                "gh gist create exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        cprintln!("shared: {url}");
        CommandOutcome::Handled
    }
}

struct LoginCommand;

#[async_trait]
impl SlashCommand for LoginCommand {
    fn name(&self) -> &'static str {
        "login"
    }
    fn description(&self) -> &'static str {
        "store an API key for a provider in ~/.pie/auth.json"
    }
    fn usage(&self) -> &'static str {
        "<provider>"
    }
    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.len() != 1 {
            return CommandOutcome::Error(
                "usage: /login <provider>  (pie will prompt for the API key without echoing it)"
                    .into(),
            );
        }
        CommandOutcome::LoginSecret {
            provider: argv[0].clone(),
            storage_key: None,
            recovery_command: None,
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
pub fn save_api_key(provider: &str, token: &str) -> Result<PathBuf, String> {
    let mut store = match crate::auth::AuthStore::load() {
        Ok(s) => s,
        Err(e) => return Err(format!("load auth store: {e}")),
    };
    store.set(
        provider.to_string(),
        crate::auth::ProviderCredential::ApiKey {
            value: token.to_string(),
        },
    );
    if let Err(e) = store.save() {
        return Err(format!("save auth store: {e}"));
    }
    Ok(crate::auth::auth_path())
}

struct LogoutCommand;

#[async_trait]
impl SlashCommand for LogoutCommand {
    fn name(&self) -> &'static str {
        "logout"
    }
    fn description(&self) -> &'static str {
        "remove a stored credential from ~/.pie/auth.json"
    }
    fn usage(&self) -> &'static str {
        "<provider>"
    }
    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            return CommandOutcome::Error("usage: /logout <provider>".into());
        }
        let provider = &argv[0];
        let mut store = match crate::auth::AuthStore::load() {
            Ok(s) => s,
            Err(e) => return CommandOutcome::Error(format!("load auth store: {e}")),
        };
        match store.remove(provider) {
            Some(_) => match store.save() {
                Ok(()) => {
                    cprintln!("removed credential for `{provider}`");
                    CommandOutcome::Handled
                }
                Err(e) => CommandOutcome::Error(format!("save auth store: {e}")),
            },
            None => {
                cprintln!("no credential stored for `{provider}`");
                CommandOutcome::Handled
            }
        }
    }
}

struct FindCommand;

#[async_trait]
impl SlashCommand for FindCommand {
    fn name(&self) -> &'static str {
        "find"
    }
    fn description(&self) -> &'static str {
        "search every session in this cwd for prompts/replies containing <query>"
    }
    fn usage(&self) -> &'static str {
        "<query>"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            return CommandOutcome::Error("usage: /find <query>".into());
        }
        let query = argv.join(" ").to_lowercase();
        let repo = crate::session::open_repo(ctx.cwd).await;
        let files = match repo.list().await {
            Ok(f) => f,
            Err(e) => return CommandOutcome::Error(format!("list sessions: {e}")),
        };
        let mut hits = 0usize;
        for path in files {
            let session = match repo.open(&path).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let entries = session.entries().await.unwrap_or_default();
            for e in entries {
                if let pie_agent_core::SessionTreeEntry::Message { message, .. } = e {
                    let text = match &message {
                        pie_agent_core::AgentMessage::Llm(pie_ai::Message::User(u)) => {
                            match &u.content {
                                pie_ai::UserContent::Text(s) => s.clone(),
                                pie_ai::UserContent::Blocks(blocks) => blocks
                                    .iter()
                                    .filter_map(|b| match b {
                                        pie_ai::UserContentBlock::Text(t) => Some(t.text.clone()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            }
                        }
                        pie_agent_core::AgentMessage::Llm(pie_ai::Message::Assistant(a)) => a
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                pie_ai::ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => continue,
                    };
                    if text.to_lowercase().contains(&query) {
                        hits += 1;
                        let snip = text
                            .chars()
                            .take(120)
                            .collect::<String>()
                            .replace('\n', " ");
                        let path_short = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                        cprintln!("  {path_short}  {snip}");
                    }
                }
            }
        }
        if hits == 0 {
            cprintln!("(no matches)");
        } else {
            cprintln!("({hits} match(es))");
        }
        CommandOutcome::Handled
    }
}

struct HistoryCommand;

#[async_trait]
impl SlashCommand for HistoryCommand {
    fn name(&self) -> &'static str {
        "history"
    }
    fn description(&self) -> &'static str {
        "show recent submitted prompts from ~/.pie/history"
    }
    fn usage(&self) -> &'static str {
        "[N]"
    }
    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        let limit: usize = argv.first().and_then(|s| s.parse().ok()).unwrap_or(20);
        let store = crate::history::HistoryStore::load();
        let entries = store.entries();
        if entries.is_empty() {
            cprintln!("(no history yet)");
            return CommandOutcome::Handled;
        }
        let start = entries.len().saturating_sub(limit);
        for (i, e) in entries[start..].iter().enumerate() {
            let n = start + i + 1;
            // Truncate long entries to 200 chars to keep the listing skimmable.
            let preview: String = e.chars().take(200).collect();
            let suffix = if preview.len() < e.len() { "…" } else { "" };
            cprintln!("  {n}: {preview}{suffix}");
        }
        CommandOutcome::Handled
    }
}

struct TriggersCommand;

#[async_trait]
impl SlashCommand for TriggersCommand {
    fn name(&self) -> &'static str {
        "triggers"
    }
    fn description(&self) -> &'static str {
        "show trigger sources, rules, running actions, and recent audit"
    }
    fn usage(&self) -> &'static str {
        "[status|rules|sources|enable <id>|disable <id>|remove <id>|remove --all|running|audit [N]|abort <trace_id>|abort --all]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let subcommand = argv.first().map(String::as_str).unwrap_or("status");
        match subcommand {
            "status" => {
                let snapshot = ctx.harness.notification_status_snapshot();
                for line in render_triggers_status(&snapshot) {
                    cprintln!("{line}");
                }
                CommandOutcome::Handled
            }
            "rules" => {
                let rules = crate::triggers::global_registry().list();
                for line in render_dynamic_trigger_rules(&rules, usize::MAX) {
                    cprintln!("{line}");
                }
                if rules.is_empty()
                    && let Some(hint) = automation_elsewhere_hint_for_ctx(ctx).await
                {
                    cprintln!("{hint}");
                }
                CommandOutcome::Handled
            }
            "remove" | "rm" | "delete" => {
                let Some(target) = argv.get(1) else {
                    return CommandOutcome::Error("usage: /triggers remove <id>|--all".into());
                };
                if target == "--all" {
                    match crate::triggers::global_registry().clear_rules() {
                        Ok(count) => {
                            cprintln!("removed {count} dynamic trigger rule(s)");
                            CommandOutcome::Handled
                        }
                        Err(e) => CommandOutcome::Error(e.to_string()),
                    }
                } else {
                    match crate::triggers::global_registry().remove_rule(target) {
                        Ok(Some(rule)) => {
                            cprintln!("removed trigger {}", rule.id);
                            cprintln!("  condition: {}", rule.condition);
                            cprintln!("  action: {}", rule.action);
                            CommandOutcome::Handled
                        }
                        Ok(None) => CommandOutcome::Error(format!(
                            "no dynamic trigger rule with id '{target}'"
                        )),
                        Err(e) => CommandOutcome::Error(e.to_string()),
                    }
                }
            }
            "enable" | "resume" => set_dynamic_trigger_enabled(argv.get(1), true),
            "disable" | "pause" => set_dynamic_trigger_enabled(argv.get(1), false),
            "sources" | "hooks" => {
                let snapshot = ctx.harness.notification_status_snapshot();
                for line in render_trigger_sources(&snapshot.hooks) {
                    cprintln!("{line}");
                }
                CommandOutcome::Handled
            }
            "running" => {
                let snapshot = ctx.harness.notification_status_snapshot();
                for line in render_running_triggers(&snapshot.running) {
                    cprintln!("{line}");
                }
                CommandOutcome::Handled
            }
            "audit" => {
                let limit = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
                let entries = match ctx.harness.session().entries().await {
                    Ok(entries) => entries,
                    Err(e) => return CommandOutcome::Error(format!("read trigger audit: {e}")),
                };
                let rows = collect_trigger_audit_rows(&entries, limit);
                for line in render_trigger_audit(&rows) {
                    cprintln!("{line}");
                }
                CommandOutcome::Handled
            }
            "abort" => {
                let Some(target) = argv.get(1) else {
                    return CommandOutcome::Error("usage: /triggers abort <trace_id>|--all".into());
                };
                let snapshot = ctx.harness.notification_status_snapshot();
                if target == "--all" {
                    let count = snapshot.running.len();
                    ctx.harness.abort_all_triggers();
                    cprintln!("requested abort for {count} running trigger(s)");
                } else {
                    if !snapshot.running.iter().any(|t| t.trace_id == *target) {
                        return CommandOutcome::Error(format!(
                            "no running trigger with trace_id '{target}'"
                        ));
                    }
                    ctx.harness.abort_trigger(target);
                    cprintln!("requested abort for trigger {target}");
                }
                CommandOutcome::Handled
            }
            other => CommandOutcome::Error(format!(
                "unknown /triggers command: {other}. usage: /triggers {}",
                self.usage()
            )),
        }
    }
}

struct NewTriggerCommand;

#[async_trait]
impl SlashCommand for NewTriggerCommand {
    fn name(&self) -> &'static str {
        "new-trigger"
    }

    fn description(&self) -> &'static str {
        "create a dynamic natural-language trigger rule"
    }

    fn usage(&self) -> &'static str {
        "<natural-language trigger request>"
    }

    async fn run(&self, argv: &[String], _ctx: &CommandCtx<'_>) -> CommandOutcome {
        let spec = argv.join(" ");
        if spec.trim().is_empty() {
            return CommandOutcome::Error(
                "usage: /new-trigger <natural-language trigger request>".into(),
            );
        }

        let prompt = format!(
            "The user asked pie to create a dynamic trigger. Extract the trigger condition and action from the request, then call NewTrigger with structured condition and action fields. Dynamic triggers fire once by default; set fire_once=false only when the user explicitly asks for a repeating trigger. Trigger output is shown in the TUI and audit by default; set promote_to_chat=true only when the user explicitly asks for trigger results to enter the main chat context or be visible to future turns. Do not require a fixed syntax. If either the condition or action is missing, ask one concise clarification question instead of calling tools.\n\nUser request:\n{spec}"
        );
        CommandOutcome::RunAgentPrompt {
            prompt,
            error_context: "create trigger: ",
        }
    }
}

struct CronCommand;

#[async_trait]
impl SlashCommand for CronCommand {
    fn name(&self) -> &'static str {
        "cron"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["crontab"]
    }

    fn description(&self) -> &'static str {
        "manage local scheduled agent jobs"
    }

    fn usage(&self) -> &'static str {
        "[list|add \"<5-field-cron>\" <prompt>|enable <id>|disable <id>|remove <id>]"
    }

    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let subcommand = argv.first().map(String::as_str).unwrap_or("list");
        match subcommand {
            "list" | "ls" | "status" => {
                let jobs = crate::triggers::global_cron_registry().list();
                for line in render_cron_jobs(&jobs) {
                    cprintln!("{line}");
                }
                if jobs.is_empty()
                    && let Some(hint) = automation_elsewhere_hint_for_ctx(ctx).await
                {
                    cprintln!("{hint}");
                }
                CommandOutcome::Handled
            }
            "add" => {
                if argv.len() < 3 {
                    return CommandOutcome::Error(
                        "usage: /cron add \"<minute hour dom month dow>\" <prompt>".into(),
                    );
                }
                let schedule = &argv[1];
                let action = argv[2..].join(" ");
                match crate::triggers::global_cron_registry().add_job(schedule, &action) {
                    Ok(job) => {
                        write_cron_control_plane_audit(ctx, "add", None, Some(&job)).await;
                        cprintln!("added cron job {}", job.id);
                        cprintln!("  schedule: {}", job.schedule);
                        cprintln!("  action: {}", preview_cron_action(&job.action));
                        CommandOutcome::Handled
                    }
                    Err(e) => CommandOutcome::Error(e.to_string()),
                }
            }
            "enable" | "resume" => set_cron_enabled(ctx, argv.get(1), true).await,
            "disable" | "pause" => set_cron_enabled(ctx, argv.get(1), false).await,
            "remove" | "rm" | "delete" => {
                let Some(id) = argv.get(1) else {
                    return CommandOutcome::Error("usage: /cron remove <id>".into());
                };
                match crate::triggers::global_cron_registry().remove_job(id) {
                    Ok(Some(job)) => {
                        write_cron_control_plane_audit(ctx, "remove", Some(&job), None).await;
                        cprintln!("removed cron job {}", job.id);
                        CommandOutcome::Handled
                    }
                    Ok(None) => CommandOutcome::Error(format!("no cron job with id '{id}'")),
                    Err(e) => CommandOutcome::Error(e.to_string()),
                }
            }
            other => CommandOutcome::Error(format!(
                "unknown /cron command: {other}. usage: /cron {}",
                self.usage()
            )),
        }
    }
}

async fn set_cron_enabled(
    ctx: &CommandCtx<'_>,
    id: Option<&String>,
    enabled: bool,
) -> CommandOutcome {
    let Some(id) = id else {
        return CommandOutcome::Error(format!(
            "usage: /cron {} <id>",
            if enabled { "enable" } else { "disable" }
        ));
    };
    let before = crate::triggers::global_cron_registry()
        .list()
        .into_iter()
        .find(|job| job.id == *id);
    match crate::triggers::global_cron_registry().set_job_enabled(id, enabled) {
        Ok(Some(job)) => {
            write_cron_control_plane_audit(
                ctx,
                if enabled { "enable" } else { "disable" },
                before.as_ref(),
                Some(&job),
            )
            .await;
            cprintln!(
                "{} cron job {}",
                if enabled { "enabled" } else { "disabled" },
                job.id
            );
            CommandOutcome::Handled
        }
        Ok(None) => CommandOutcome::Error(format!("no cron job with id '{id}'")),
        Err(e) => CommandOutcome::Error(e.to_string()),
    }
}

async fn write_cron_control_plane_audit(
    ctx: &CommandCtx<'_>,
    op: &str,
    before: Option<&crate::triggers::cron::CronJob>,
    after: Option<&crate::triggers::cron::CronJob>,
) {
    let job = after.or(before);
    let audit = crate::triggers::cron::cron_control_plane_audit(op, "slash", before, after);
    if let Err(e) = ctx
        .harness
        .session()
        .append_custom("cron_control_plane", Some(audit))
        .await
    {
        tracing::warn!(
            op = %op,
            job_id = job.map(|job| job.id.as_str()).unwrap_or("<unknown>"),
            error = %e,
            "cron_control_plane audit write failed; slash cron change itself succeeded"
        );
    }
}

/// Hint at enabled automation living in sibling sessions of this cwd. Used by the empty
/// states of `/cron list` and `/triggers rules`, where "none" otherwise reads as data loss
/// when the user's jobs simply live in another session.
async fn automation_elsewhere_hint_for_ctx(ctx: &CommandCtx<'_>) -> Option<String> {
    let metadata = ctx
        .harness
        .session()
        .storage()
        .get_metadata_json()
        .await
        .ok()?;
    let current = metadata
        .get("path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from);
    let repo = crate::session::open_repo(ctx.cwd).await;
    crate::session::automation_elsewhere_hint(&repo, current.as_deref()).await
}

pub(crate) fn render_cron_jobs(jobs: &[crate::triggers::cron::CronJob]) -> Vec<String> {
    if jobs.is_empty() {
        return vec!["Cron jobs (session): none".into()];
    }
    let mut lines = vec![format!("Cron jobs (session, {}):", jobs.len())];
    for job in jobs {
        let state = if job.enabled { "enabled" } else { "disabled" };
        let running = job
            .running_trace_id
            .as_ref()
            .map(|trace| format!(", running {trace}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}  {}  {}{}",
            job.id, state, job.schedule, running
        ));
        lines.push(format!("    action: {}", preview_cron_action(&job.action)));
        if job.skipped_overlap_count > 0 {
            lines.push(format!("    overlap skips: {}", job.skipped_overlap_count));
        }
        if let Some(err) = &job.last_error {
            lines.push(format!("    last: {err}"));
        } else if let Some(last) = job.last_fired_at {
            lines.push(format!("    last fired: {}", last.to_rfc3339()));
        }
    }
    lines
}

fn preview_cron_action(action: &str) -> String {
    preview_cron_text(&crate::bug_report::redact(action), 120)
}

fn preview_cron_text(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in input.chars().enumerate() {
        if idx == max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub(crate) fn render_triggers_status(snapshot: &NotificationStatusSnapshot) -> Vec<String> {
    let mut lines = Vec::new();
    let runtime = snapshot.runtime;
    let dynamic_rules = crate::triggers::global_registry().list();
    let enabled_count = dynamic_rules.iter().filter(|rule| rule.enabled).count();
    let disabled_count = dynamic_rules.len().saturating_sub(enabled_count);
    let fire_once_count = dynamic_rules.iter().filter(|rule| rule.fire_once).count();
    let repeat_count = dynamic_rules.len().saturating_sub(fire_once_count);
    let promote_count = dynamic_rules
        .iter()
        .filter(|rule| rule.promote_to_chat)
        .count();
    lines.push("Trigger status:".into());
    lines.push(format!(
        "  dynamic rules: {} total, {} enabled, {} disabled ({} fire_once, {} repeat, {} promote_to_chat)",
        dynamic_rules.len(),
        enabled_count,
        disabled_count,
        fire_once_count,
        repeat_count,
        promote_count
    ));
    let dynamic_checker_count = snapshot
        .hooks
        .iter()
        .filter(|hook| {
            hook.subscription_labels
                .iter()
                .any(|label| label.contains("dynamic trigger periodic check"))
        })
        .count();
    let notification_hook_count = snapshot.hooks.len().saturating_sub(dynamic_checker_count);
    lines.push(format!(
        "  local dynamic checker: {} registered, polls every {}s while enabled rules exist",
        dynamic_checker_count,
        crate::triggers::dynamic::dynamic_trigger_poll_interval_secs()
    ));
    lines.push(format!(
        "  push trigger sources: {} configured source(s) feed server-pushed events into the same trigger runtime",
        notification_hook_count
    ));
    let storage = crate::triggers::global_registry()
        .storage_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "memory".into());
    lines.push(format!("  storage: {storage}"));
    lines.push("  output: default is TUI + audit only; rules marked promote_to_chat also enter the main chat context".into());
    lines.push(format!(
        "  engine: accepted={} deduped={} cycle_suppressed={} recent_traces={} dedup_entries={} running={}",
        runtime.accepted_total,
        runtime.deduped_total,
        runtime.cycle_suppressed_total,
        runtime.active_traces,
        runtime.dedup_entries,
        snapshot.running.len()
    ));
    let attention_count = snapshot
        .hooks
        .iter()
        .filter(|h| h.requires_attention.is_some())
        .count();
    let connected_count = snapshot
        .hooks
        .iter()
        .filter(|h| matches!(h.state, HookState::Connected))
        .count();
    lines.push(format!(
        "  sources: {} total, {} connected, {} require attention",
        snapshot.hooks.len(),
        connected_count,
        attention_count
    ));
    lines.extend(
        render_dynamic_trigger_rules(&dynamic_rules, 3)
            .into_iter()
            .skip(1),
    );
    lines.push(
        "  commands: /triggers rules | /triggers sources | /triggers disable <id> | /triggers enable <id> | /triggers remove <id> | /triggers audit".into(),
    );
    lines
}

fn set_dynamic_trigger_enabled(target: Option<&String>, enabled: bool) -> CommandOutcome {
    let Some(id) = target else {
        let action = if enabled { "enable" } else { "disable" };
        return CommandOutcome::Error(format!("usage: /triggers {action} <id>"));
    };
    match crate::triggers::global_registry().set_rule_enabled(id, enabled) {
        Ok(Some(rule)) => {
            let state = if rule.enabled { "enabled" } else { "disabled" };
            cprintln!("{state} trigger {}", rule.id);
            cprintln!("  condition: {}", rule.condition);
            cprintln!("  action: {}", rule.action);
            if rule.enabled && rule.fire_once {
                cprintln!("  fire_once: true (will disable again after the next successful match)");
            }
            CommandOutcome::Handled
        }
        Ok(None) => CommandOutcome::Error(format!("no dynamic trigger rule with id '{id}'")),
        Err(e) => CommandOutcome::Error(e.to_string()),
    }
}

pub(crate) fn render_dynamic_trigger_rules(
    rules: &[crate::triggers::dynamic::DynamicTriggerRule],
    limit: usize,
) -> Vec<String> {
    if rules.is_empty() {
        return vec!["Dynamic trigger rules: none".into()];
    }
    let shown = rules.len().min(limit);
    let mut lines = vec![format!("Dynamic trigger rules ({}):", rules.len())];
    for rule in rules.iter().take(shown) {
        let state = if rule.enabled { "enabled" } else { "disabled" };
        let fire_mode = if rule.fire_once {
            "fire_once"
        } else {
            "repeat"
        };
        let output_mode = if rule.promote_to_chat {
            "promote_to_chat"
        } else {
            "audit_only"
        };
        lines.push(format!(
            "  - {} [{state}, {fire_mode}, {output_mode}{}] when {} -> {}",
            rule.id,
            rule.fired_at
                .map(|at| format!(", fired_at={}", at.to_rfc3339()))
                .unwrap_or_default(),
            preview_text(&rule.condition, 80),
            preview_text(&rule.action, 80)
        ));
    }
    if shown < rules.len() {
        lines.push(format!(
            "  ... {} more; run /triggers rules",
            rules.len() - shown
        ));
    }
    lines
}

fn render_trigger_sources(hooks: &[NotificationHookStatus]) -> Vec<String> {
    if hooks.is_empty() {
        return vec!["(no trigger sources registered)".into()];
    }
    let mut lines = vec![format!("Trigger sources ({}):", hooks.len())];
    for (idx, hook) in hooks.iter().enumerate() {
        let labels = if hook.subscription_labels.is_empty() {
            "subscriptions: none".into()
        } else {
            format!("subscriptions: {}", hook.subscription_labels.join(", "))
        };
        lines.push(format!(
            "  - source #{}: {} queued={} dropped={} deduped={} last_event={}{}",
            idx + 1,
            render_hook_state(&hook.state),
            hook.queued_count,
            hook.dropped_count,
            hook.deduped_count,
            hook.last_event_at
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "never".into()),
            render_requires_attention(hook)
        ));
        lines.push(format!("      {labels}"));
        if let Some(err) = &hook.last_error {
            lines.push(format!("      last error: {}", preview_text(err, 160)));
        }
    }
    lines
}

fn render_hook_state(state: &HookState) -> String {
    match state {
        HookState::Connected => "connected".into(),
        HookState::Reconnecting => "reconnecting".into(),
        HookState::Disconnected { reason } => {
            format!("disconnected ({})", preview_text(reason, 80))
        }
        HookState::Disabled => "disabled".into(),
        HookState::AuthFailed { reason } => format!("auth_failed ({})", preview_text(reason, 80)),
    }
}

fn render_requires_attention(hook: &NotificationHookStatus) -> String {
    hook.requires_attention
        .as_ref()
        .map(|message| format!("  attention: {}", preview_text(message, 120)))
        .unwrap_or_default()
}

fn render_running_triggers(running: &[RunningTriggerState]) -> Vec<String> {
    if running.is_empty() {
        return vec!["(no running triggers)".into()];
    }
    let mut lines = vec![format!("Running triggers ({}):", running.len())];
    for trigger in running {
        lines.push(format!(
            "  - {}  {} / {}  since {}",
            trigger.trace_id,
            trigger.source_label,
            trigger.event_label,
            trigger.started_at.to_rfc3339()
        ));
        lines.push(format!(
            "      prompt: {}",
            preview_text(&trigger.prompt_preview, 120)
        ));
    }
    lines
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TriggerAuditRow {
    custom_type: String,
    timestamp: String,
    trace_id: Option<String>,
    state: String,
    source_label: Option<String>,
    event_label: Option<String>,
    summary: Option<String>,
    details: Vec<String>,
}

fn collect_trigger_audit_rows(entries: &[SessionTreeEntry], limit: usize) -> Vec<TriggerAuditRow> {
    entries
        .iter()
        .rev()
        .filter_map(trigger_audit_row)
        .take(limit)
        .collect()
}

fn trigger_audit_row(entry: &SessionTreeEntry) -> Option<TriggerAuditRow> {
    let SessionTreeEntry::Custom {
        timestamp,
        custom_type,
        data,
        ..
    } = entry
    else {
        return None;
    };
    if !matches!(
        custom_type.as_str(),
        "trigger" | "trigger_result" | "trigger_promotion"
    ) {
        return None;
    }
    let data = data.as_ref()?;
    let trace_id = string_field(data, "trace_id");
    let state = match custom_type.as_str() {
        "trigger" => string_field(data, "state").unwrap_or_else(|| "unknown".into()),
        "trigger_result" => match data.get("success").and_then(|v| v.as_bool()) {
            Some(true) => "completed".into(),
            Some(false) => "failed".into(),
            None => "unknown".into(),
        },
        "trigger_promotion" => string_field(data, "state").unwrap_or_else(|| "unknown".into()),
        _ => "unknown".into(),
    };
    let summary = match custom_type.as_str() {
        "trigger" => string_field(data, "payload_summary"),
        "trigger_result" => string_field(data, "summary").or_else(|| string_field(data, "reason")),
        "trigger_promotion" => {
            string_field(data, "redaction_status").map(|s| format!("redaction_status={s}"))
        }
        _ => None,
    };
    let details = match custom_type.as_str() {
        "trigger" => trigger_decision_details(data),
        "trigger_result" => trigger_result_details(data),
        "trigger_promotion" => trigger_promotion_details(data),
        _ => Vec::new(),
    };
    Some(TriggerAuditRow {
        custom_type: custom_type.clone(),
        timestamp: timestamp.clone(),
        trace_id,
        state,
        source_label: string_field(data, "source_label"),
        event_label: string_field(data, "event_label"),
        summary,
        details,
    })
}

fn render_trigger_audit(rows: &[TriggerAuditRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["(no trigger audit entries in this session)".into()];
    }
    let mut lines = vec![format!("Recent trigger audit ({}):", rows.len())];
    for row in rows {
        let trace = row.trace_id.as_deref().unwrap_or("unknown-trace");
        let source = row.source_label.as_deref().unwrap_or("-");
        let event = row.event_label.as_deref().unwrap_or("-");
        lines.push(format!(
            "  - {}  {}/{}  trace={}  {} / {}",
            row.timestamp, row.custom_type, row.state, trace, source, event
        ));
        if let Some(summary) = &row.summary {
            lines.push(format!("      {}", preview_text(summary, 160)));
        }
        for detail in &row.details {
            lines.push(format!("      {detail}"));
        }
    }
    lines
}

fn trigger_decision_details(data: &serde_json::Value) -> Vec<String> {
    let Some(decision) = data.get("evaluator_decision") else {
        return Vec::new();
    };
    let Some(outcome) = string_field(decision, "outcome") else {
        return vec!["decision: present".into()];
    };
    let mut fields = vec![format!("decision: {outcome}")];
    match outcome.as_str() {
        "accept" => {
            if let Some(permission) = string_field(decision, "permission") {
                fields.push(format!("permission: {}", preview_text(&permission, 80)));
            }
            if let Some(reason) = string_field(decision, "reason") {
                fields.push(format!("reason: {}", preview_text(&reason, 160)));
            }
        }
        "deduped" => {
            if let Some(previous) = string_field(decision, "previous_trace_id") {
                fields.push(format!(
                    "previous_trace_id: {}",
                    preview_text(&previous, 80)
                ));
            }
            if let Some(policy) = string_field(decision, "replacement_policy") {
                fields.push(format!("replacement_policy: {}", preview_text(&policy, 80)));
            }
        }
        "cycle_suppressed" => {
            if let Some(hops) = number_field(decision, "hop_count") {
                fields.push(format!("hop_count: {hops}"));
            }
        }
        _ => {}
    }
    fields
}

fn trigger_result_details(data: &serde_json::Value) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(branch_id) = string_field(data, "branch_id") {
        fields.push(format!("branch_id: {}", preview_text(&branch_id, 80)));
    }
    if let Some(count) = number_field(data, "message_count") {
        fields.push(format!("message_count: {count}"));
    }
    fields
}

fn trigger_promotion_details(data: &serde_json::Value) -> Vec<String> {
    let mut fields = Vec::new();
    if let Some(kind) = string_field(data, "promote_kind") {
        fields.push(format!("promote_kind: {}", preview_text(&kind, 80)));
    }
    if let Some(inserted) = string_field(data, "inserted_entry_id") {
        fields.push(format!(
            "inserted_entry_id: {}",
            preview_text(&inserted, 80)
        ));
    }
    fields
}

fn string_field(data: &serde_json::Value, name: &str) -> Option<String> {
    data.get(name)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn number_field(data: &serde_json::Value, name: &str) -> Option<u64> {
    data.get(name).and_then(|v| v.as_u64())
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let mut preview = text.chars().take(max_chars).collect::<String>();
    if preview.chars().count() < text.chars().count() {
        preview.push('…');
    }
    preview.replace('\n', " ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillShortcut {
    pub command: String,
    pub source: SkillSource,
    pub description: String,
}

pub fn skill_shortcuts(skills: &[Skill], registry: &Registry) -> Vec<SkillShortcut> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for skill in skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
    {
        *counts.entry(skill.name.as_str()).or_default() += 1;
    }
    let mut shortcuts = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .filter(|skill| counts.get(skill.name.as_str()) == Some(&1))
        .filter(|skill| registry.find(&skill.name).is_none())
        .map(|skill| SkillShortcut {
            command: format!("/{}", skill.name),
            source: skill.source,
            description: preview_text(&skill.description, 72),
        })
        .collect::<Vec<_>>();
    shortcuts.sort_by(|a, b| a.command.cmp(&b.command));
    shortcuts
}

fn resolve_skill_shortcut<'a>(
    skills: &'a [Skill],
    registry: &Registry,
    name: &str,
) -> Result<Option<&'a Skill>, String> {
    if registry.find(name).is_some() {
        return Ok(None);
    }
    let matching = skills
        .iter()
        .filter(|skill| skill.name == name)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Ok(None);
    }
    let enabled = matching
        .iter()
        .copied()
        .filter(|skill| !skill.disable_model_invocation)
        .collect::<Vec<_>>();
    match enabled.as_slice() {
        [skill] => Ok(Some(*skill)),
        [] => Err(format!(
            "skill '{name}' is disabled; run /skills enable {name} [source] or /skills to list loaded skills"
        )),
        _ => Err(format!(
            "multiple enabled skills named '{name}'; use /skill {name} after resolving the source with /skills show {name} [source]"
        )),
    }
}

fn run_skill_shortcut(
    name: &str,
    argv: &[String],
    registry: &Registry,
    ctx: &CommandCtx<'_>,
) -> Option<CommandOutcome> {
    match resolve_skill_shortcut(&ctx.harness.skills(), registry, name) {
        Ok(Some(skill)) => {
            cprintln!("using skill: {} ({})", skill.name, skill.source.label());
            if argv.is_empty() {
                Some(CommandOutcome::AttachSkill {
                    name: skill.name.clone(),
                })
            } else {
                Some(CommandOutcome::RunAgentPrompt {
                    prompt: attach_skill_prompt(argv.join(" "), Some(&skill.name)),
                    error_context: "skill command failed: ",
                })
            }
        }
        Ok(None) => None,
        Err(e) => Some(CommandOutcome::Error(e)),
    }
}

pub async fn dispatch(input: &str, registry: &Registry, ctx: &CommandCtx<'_>) -> CommandOutcome {
    let (name, argv) = match parse(input) {
        Some(parts) => parts,
        None => return CommandOutcome::Error("not a slash command".into()),
    };
    // Special-case `/help`: the handler can't see the registry, so we render here.
    if name == "help" {
        print_help_with_skills(
            registry,
            argv.first().map(String::as_str),
            &ctx.harness.skills(),
        );
        return CommandOutcome::Handled;
    }
    let Some(cmd) = registry.find(&name) else {
        return run_skill_shortcut(&name, &argv, registry, ctx).unwrap_or_else(|| {
            CommandOutcome::Error(format!("unknown command: /{name} (try /help)"))
        });
    };
    cmd.run(&argv, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_agent_core::SkillSource;

    struct EnvGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn custom_test_model(provider: &str, id: &str) -> Model {
        Model {
            id: id.into(),
            name: "Secret Free Model".into(),
            api: pie_ai::Api::from("openai-responses"),
            provider: Provider::from(provider),
            base_url: "https://secret-base.example/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![pie_ai::InputModality::Text],
            cost: pie_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: Some(std::collections::HashMap::from([(
                "Authorization".into(),
                "Bearer sk-secret-should-not-leak".into(),
            )])),
            compat: None,
        }
    }

    #[test]
    fn parse_splits_on_whitespace() {
        let (name, args) = parse("/model anthropic:claude").unwrap();
        assert_eq!(name, "model");
        assert_eq!(args, vec!["anthropic:claude".to_string()]);
    }

    #[test]
    fn parse_keeps_quoted_args_together() {
        let (name, args) = parse("/say \"hello world\" again").unwrap();
        assert_eq!(name, "say");
        assert_eq!(args, vec!["hello world".to_string(), "again".to_string()]);
    }

    #[test]
    fn parse_returns_none_for_non_slash() {
        assert!(parse("hello world").is_none());
        assert!(parse("/").is_none());
    }

    #[test]
    fn model_spec_accepts_colon_slash_and_two_args() {
        assert_eq!(
            parse_model_spec("deepseek:deepseek-v4-pro"),
            Some(("deepseek", "deepseek-v4-pro"))
        );
        assert_eq!(
            parse_model_spec("deepseek/deepseek-v4-pro"),
            Some(("deepseek", "deepseek-v4-pro"))
        );
        assert_eq!(
            parse_model_spec("deepseek deepseek-v4-pro"),
            Some(("deepseek", "deepseek-v4-pro"))
        );
        assert_eq!(parse_model_spec("deepseek"), None);
    }

    #[test]
    fn model_credential_hint_uses_only_selected_provider_credentials() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
        let _deepseek = EnvGuard::remove("DEEPSEEK_API_KEY");
        let _openai = EnvGuard::set("OPENAI_API_KEY", "sk-openai-should-not-count");

        let hint = model_credential_hint("deepseek").expect("deepseek key is missing");
        assert!(hint.contains("DEEPSEEK_API_KEY"), "{hint}");
        assert!(hint.contains("/login deepseek"), "{hint}");
        assert!(!hint.contains("OPENAI_API_KEY"), "{hint}");
        assert!(!hint.contains("sk-openai-should-not-count"), "{hint}");
    }

    #[test]
    fn model_credential_hint_accepts_env_or_auth_store_for_selected_provider() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
        let _deepseek = EnvGuard::set("DEEPSEEK_API_KEY", "sk-deepseek-present");
        assert!(model_credential_hint("deepseek").is_none());
        drop(_deepseek);

        let mut store = crate::auth::AuthStore::default();
        store.set(
            "deepseek",
            crate::auth::ProviderCredential::ApiKey {
                value: "stored-deepseek".into(),
            },
        );
        store.save().unwrap();
        assert!(model_credential_hint("deepseek").is_none());
    }

    #[test]
    fn registry_lookup_by_name_and_alias() {
        let r = Registry::with_builtins();
        assert!(r.find("quit").is_some());
        assert!(r.find("q").is_some());
        assert!(r.find("exit").is_some());
        assert!(r.find("session").is_some());
        assert!(r.find("triggers").is_some());
        assert!(r.find("nope").is_none());
    }

    #[test]
    fn registry_and_help_do_not_expose_removed_hub_surface() {
        let r = Registry::with_builtins();
        for removed in ["hub", "endpoint", "config"] {
            assert!(
                r.find(removed).is_none(),
                "/{removed} should not be registered"
            );
        }

        let help = help_text(&r, None);
        for removed in ["/hub", "/endpoint", "hub.inject", "pie.0xfefe.me"] {
            assert!(
                !help.contains(removed),
                "help should not expose removed hub surface `{removed}`:\n{help}"
            );
        }
    }

    #[test]
    fn model_help_summary_lists_builtin_providers_without_secrets() {
        let text = cli_model_help_text();
        assert!(text.contains("Supported providers"), "{text}");
        assert!(text.contains("anthropic("), "{text}");
        assert!(text.contains("openai("), "{text}");
        assert!(text.contains("~/.pie/models.json"), "{text}");
        assert!(text.contains("<cwd>/.pie/models.json"), "{text}");
        assert!(!text.contains("API_KEY"), "{text}");
        assert!(!text.contains("auth.json"), "{text}");
    }

    #[test]
    fn help_topic_renders_command_usage_and_aliases() {
        let registry = Registry::with_builtins();
        let model = help_text(&registry, Some("model"));
        assert!(
            model.contains("/model [provider:model-id|list [provider]]"),
            "{model}"
        );
        assert!(model.contains("show or switch the active model"), "{model}");
        assert!(model.contains("more: /help model"), "{model}");

        let quit = help_text(&registry, Some("/quit"));
        assert!(quit.contains("/quit"), "{quit}");
        assert!(quit.contains("aliases: /exit, /q"), "{quit}");

        let goal_start = help_text(&registry, Some("goal-start"));
        assert!(goal_start.contains("/goal-start <prompt>"), "{goal_start}");
        assert!(
            goal_start.contains("start working on the active session goal"),
            "{goal_start}"
        );
    }

    #[test]
    fn help_unknown_topic_gives_recovery_hint() {
        let registry = Registry::with_builtins();
        let text = help_text(&registry, Some("mod"));
        assert!(text.contains("unknown help topic: mod"), "{text}");
        assert!(text.contains("Did you mean /model?"), "{text}");
    }

    #[test]
    fn model_catalog_includes_custom_models_without_secret_fields() {
        let provider = Provider::from("help-test-provider");
        let id = "secret-free";
        pie_ai::register_custom_model(custom_test_model(&provider.0, id));

        let text = model_catalog_text(Some(&provider.0)).unwrap();
        assert!(text.contains("help-test-provider"), "{text}");
        assert!(text.contains(id), "{text}");
        assert!(text.contains("Secret Free Model"), "{text}");
        assert!(!text.contains("secret-base"), "{text}");
        assert!(!text.contains("sk-secret"), "{text}");
        assert!(!text.contains("Authorization"), "{text}");

        pie_ai::unregister_custom_model(&provider, id);
    }

    #[test]
    fn unknown_model_error_lists_candidates() {
        let message = unknown_model_error("anthropic", "definitely-not-a-model");
        assert!(message.contains("unknown model in catalog"), "{message}");
        assert!(message.contains("Candidates:"), "{message}");
        assert!(message.contains("claude"), "{message}");
    }

    #[test]
    fn unknown_provider_error_lists_provider_candidates() {
        let groups = model_groups();
        let message = unknown_provider_error("definitely-not-a-provider", &groups);
        assert!(message.contains("unknown provider"), "{message}");
        assert!(message.contains("anthropic("), "{message}");
        assert!(message.contains("openai("), "{message}");
    }

    #[test]
    fn render_triggers_status_summarizes_runtime_hooks_and_running() {
        let snapshot = NotificationStatusSnapshot {
            hooks: vec![NotificationHookStatus {
                state: HookState::Disconnected {
                    reason: "protocol_mismatch".into(),
                },
                last_event_at: None,
                last_ack_at: None,
                last_error: Some("bad frame".into()),
                queued_count: 2,
                dropped_count: 3,
                deduped_count: 4,
                subscription_labels: vec!["repo c4pt0r/pie".into()],
                requires_attention: Some("upgrade hub".into()),
            }],
            runtime: pie_agent_core::TriggerRuntimeSnapshot {
                dedup_entries: 5,
                active_traces: 6,
                accepted_total: 7,
                deduped_total: 8,
                cycle_suppressed_total: 9,
            },
            running: vec![RunningTriggerState {
                trace_id: "trace-1".into(),
                source_label: "mcp:github".into(),
                event_label: "pr_merged".into(),
                started_at: chrono::DateTime::parse_from_rfc3339("2026-05-22T19:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                prompt_preview: "summarize release".into(),
            }],
        };

        let status = render_triggers_status(&snapshot).join("\n");
        assert!(status.contains("accepted=7"));
        assert!(status.contains("recent_traces=6"));
        assert!(status.contains("1 total"));
        assert!(status.contains("1 require attention"));
        assert!(status.contains("running=1"));
        assert!(status.contains("push trigger sources: 1 configured source"));

        let sources = render_trigger_sources(&snapshot.hooks).join("\n");
        assert!(sources.contains("disconnected (protocol_mismatch)"));
        assert!(sources.contains("queued=2"));
        assert!(sources.contains("subscriptions: repo c4pt0r/pie"));
        assert!(sources.contains("attention: upgrade hub"));

        let running = render_running_triggers(&snapshot.running).join("\n");
        assert!(running.contains("trace-1"));
        assert!(running.contains("mcp:github / pr_merged"));
        assert!(running.contains("summarize release"));
    }

    #[test]
    fn collect_trigger_audit_rows_uses_preview_safe_fields_only() {
        let entries = vec![
            SessionTreeEntry::Custom {
                id: "ignored".into(),
                parent_id: None,
                timestamp: "2026-05-22T19:00:00Z".into(),
                custom_type: "not_trigger".into(),
                data: Some(serde_json::json!({"trace_id": "ignored"})),
            },
            SessionTreeEntry::Custom {
                id: "t1".into(),
                parent_id: None,
                timestamp: "2026-05-22T19:01:00Z".into(),
                custom_type: "trigger".into(),
                data: Some(serde_json::json!({
                    "trace_id": "trace-a",
                    "state": "permission_denied",
                    "source_label": "mcp:github",
                    "event_label": "pr_merged",
                    "payload_summary": "safe summary",
                    "evaluator_decision": {
                        "outcome": "accept",
                        "permission": "deny",
                        "reason": "policy says no",
                        "raw_payload": "must-not-render"
                    },
                    "payload": {"secret": "must-not-render"}
                })),
            },
            SessionTreeEntry::Custom {
                id: "r1".into(),
                parent_id: None,
                timestamp: "2026-05-22T19:02:00Z".into(),
                custom_type: "trigger_result".into(),
                data: Some(serde_json::json!({
                    "trace_id": "trace-a",
                    "success": false,
                    "reason": "aborted"
                })),
            },
            SessionTreeEntry::Custom {
                id: "p1".into(),
                parent_id: None,
                timestamp: "2026-05-22T19:03:00Z".into(),
                custom_type: "trigger_promotion".into(),
                data: Some(serde_json::json!({
                    "trace_id": "trace-a",
                    "state": "pending",
                    "redaction_status": "clean"
                })),
            },
        ];

        let rows = collect_trigger_audit_rows(&entries, 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].custom_type, "trigger_promotion");
        assert_eq!(rows[0].state, "pending");
        assert_eq!(rows[1].state, "failed");
        assert_eq!(rows[2].source_label.as_deref(), Some("mcp:github"));
        let rendered = render_trigger_audit(&rows).join("\n");
        assert!(rendered.contains("trace-a"));
        assert!(rendered.contains("safe summary"));
        assert!(rendered.contains("decision: accept"));
        assert!(rendered.contains("permission: deny"));
        assert!(rendered.contains("reason: policy says no"));
        assert!(rendered.contains("redaction_status=clean"));
        assert!(!rendered.contains("must-not-render"));
        assert!(!rendered.contains("payload"));
    }

    #[test]
    fn trigger_decision_details_explain_dedup_and_cycle_states() {
        let dedup = trigger_decision_details(&serde_json::json!({
            "evaluator_decision": {
                "outcome": "deduped",
                "replacement_policy": "latest_replaces",
                "previous_trace_id": "trace-old",
                "raw_payload": "must-not-render",
            }
        }))
        .join("\n");
        assert!(dedup.contains("decision: deduped"));
        assert!(dedup.contains("previous_trace_id: trace-old"));
        assert!(dedup.contains("replacement_policy: latest_replaces"));
        assert!(!dedup.contains("must-not-render"));

        let cycle = trigger_decision_details(&serde_json::json!({
            "evaluator_decision": {
                "outcome": "cycle_suppressed",
                "hop_count": 6,
            }
        }))
        .join("\n");
        assert!(cycle.contains("decision: cycle_suppressed"));
        assert!(cycle.contains("hop_count: 6"));
    }

    #[test]
    fn attach_skill_prompt_wraps_prompt_without_skill_body() {
        let wrapped = attach_skill_prompt("review this change", Some("review-pr"));

        assert!(wrapped.contains("Skill tool"));
        assert!(wrapped.contains("review-pr"));
        assert!(wrapped.contains("review this change"));
        assert!(!wrapped.contains("SECRET SKILL BODY"));

        assert_eq!(attach_skill_prompt("plain", None), "plain");
    }

    #[test]
    fn skill_source_label_maps_enum_variants() {
        // `/skills` now renders the structured `Skill.source` field (set by the loader per
        // discovery root) instead of inferring source from the file_path string. Lock the
        // label mapping the listing depends on.
        assert_eq!(SkillSource::Builtin.label(), "builtin");
        assert_eq!(SkillSource::User.label(), "user");
        assert_eq!(SkillSource::Project.label(), "project");
    }

    #[test]
    fn skill_source_parse_error_is_fixed_and_bounded() {
        let err = parse_skill_source("user-secret-token").unwrap_err();
        assert!(err.contains("expected one of"), "{err}");
        assert!(!err.contains("user-secret-token"), "{err}");
    }
}
