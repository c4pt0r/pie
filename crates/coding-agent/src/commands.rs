//! Slash-command registry. Tracks a small set of REPL builtins and dispatches by name.
//!
//! Built-in commands today: `/help`, `/clear`, `/skills`, `/skill`, `/quit` (and aliases),
//! `/model`, `/thinking`. The trait is shaped so future extensions (issue #10 Part B) can
//! register additional commands without touching this file.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pie_agent_core::{
    AgentHarness, HookState, NotificationHookStatus, NotificationStatusSnapshot,
    RunningTriggerState, SessionTreeEntry, Skill, ThinkingLevel,
};
use pie_ai::{Provider, get_model};

#[cfg_attr(test, allow(dead_code))]
pub const THINKING_LEVEL_VALUES: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];
pub const THINKING_LEVEL_USAGE: &str = "[off|minimal|low|medium|high|xhigh]";

/// Outcome of running a command. Drives the REPL's next action.
#[cfg_attr(test, allow(dead_code))]
#[derive(Debug)]
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
    /// Commands return this instead of awaiting the harness directly so Ctrl-C can abort
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
    /// Prompt for a provider credential without echoing the secret in the terminal input line.
    LoginSecret { provider: String },
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
        r.register(Arc::new(SessionsCommand));
        r.register(Arc::new(ShareCommand));
        r.register(Arc::new(LoginCommand));
        r.register(Arc::new(LogoutCommand));
        r.register(Arc::new(FindCommand));
        r.register(Arc::new(HistoryCommand));
        r.register(Arc::new(TriggersCommand));
        r.register(Arc::new(NewTriggerCommand));
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
        "show available commands"
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
        "list loaded skills"
    }
    async fn run(&self, _argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let skills = ctx.harness.skills();
        if skills.is_empty() {
            println!(
                "(no skills loaded — drop SKILL.md files under ~/.pie/skills/<name>/ or <cwd>/.pie/skills/<name>/)"
            );
        } else {
            println!("Loaded skills ({}):", skills.len());
            for s in &skills {
                let disabled = if s.disable_model_invocation {
                    "  [disabled: disable_model_invocation=true]"
                } else {
                    ""
                };
                println!(
                    "  - {}  ({}){}",
                    s.name,
                    skill_source_label(s, ctx.cwd),
                    disabled
                );
                if !s.description.is_empty() {
                    println!("      {}", s.description);
                }
                println!("      path: {}", s.file_path);
            }
        }
        CommandOutcome::Handled
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
        println!("attached skill: {name} for next turn");
        CommandOutcome::AttachSkill { name: name.clone() }
    }
}

fn skill_source_label(skill: &Skill, cwd: &std::path::Path) -> String {
    // Built-in skills are bundled into the `pie` binary and surface a synthetic
    // `<builtin>/<name>/SKILL.md` path. Detect that before falling through to the disk-path
    // checks below.
    if skill.file_path.starts_with("<builtin>/") {
        return "builtin".into();
    }
    let path = std::path::Path::new(&skill.file_path);
    if path.starts_with(cwd.join(".pie").join("skills")) {
        "project".into()
    } else if skill.file_path.contains("/.pie/skills/")
        || skill.file_path.contains("/.pie/skills\\")
    {
        "user".into()
    } else {
        "source path".into()
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
        "[provider:model-id]"
    }
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        if argv.is_empty() {
            let current = ctx.harness.agent().state().model.clone();
            match current {
                Some(m) => println!("active model: {}:{}", m.provider.0, m.id),
                None => println!("(no model active)"),
            }
            return CommandOutcome::Handled;
        }
        // Accept either `provider:id` or two separate args `provider id`.
        let spec = argv.join(" ");
        let (provider, id) = match spec.split_once(':') {
            Some((p, i)) => (p.to_string(), i.to_string()),
            None => {
                return CommandOutcome::Error(
                    "expected provider:model-id, e.g. /model anthropic:claude-haiku-4-5".into(),
                );
            }
        };
        let provider_obj = Provider::from(provider.as_str());
        let Some(model) = get_model(&provider_obj, &id) else {
            return CommandOutcome::Error(format!("unknown model in catalog: {provider}:{id}"));
        };
        match ctx.harness.set_model(model.clone()).await {
            Ok(_) => {
                println!("switched to {provider}:{id}");
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set_model failed: {e}")),
        }
    }
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
            println!("thinking level: {}", lvl.map(|l| l.as_str()).unwrap_or("?"));
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
                println!("thinking level: {}", level.as_str());
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set_thinking_level failed: {e}")),
        }
    }
}

// Re-export for `print_help` in main.rs.
pub fn print_help(registry: &Registry) {
    println!();
    println!("Commands:");
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
        println!(
            "  /{}{}    {}{}",
            cmd.name(),
            usage,
            cmd.description(),
            aliases
        );
    }
    println!();
    println!("Anything else is sent as a prompt to the agent.");
    println!();
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
            println!("cost counters reset");
            return CommandOutcome::Handled;
        }
        let snap = ctx.harness.cost();
        println!("{}", pie_agent_core::cost_full_breakdown(&snap));
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
        println!();
        println!("Diagnostic snapshot:");
        println!("  session       {}", ctx.session_id);
        println!("  model         {model}");
        println!("  thinking      {thinking}");
        println!("  tools         {}", ctx.tool_count);
        println!("  skills        {skill_count}");
        println!(
            "  cost          {}",
            pie_agent_core::cost_one_line_summary(&cost)
        );
        println!("  log file      {log}");
        println!();
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
                println!(
                    "(no templates loaded — drop `.md` files under ~/.pie/templates/ or <cwd>/.pie/templates/)"
                );
            } else {
                println!("Loaded templates ({}):", templates.len());
                for t in &templates {
                    let desc = t.description.clone().unwrap_or_default();
                    println!("  /template {}  {}", t.name, desc);
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
                println!("saved transcript: {}", p.display());
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
    async fn run(&self, argv: &[String], ctx: &CommandCtx<'_>) -> CommandOutcome {
        let custom = if argv.is_empty() {
            None
        } else {
            Some(argv.join(" "))
        };
        match ctx.harness.force_compact(custom).await {
            Ok(true) => {
                println!("compaction ran");
                CommandOutcome::Handled
            }
            Ok(false) => {
                println!("nothing to compact");
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("compaction failed: {e}")),
        }
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
                println!("undid last turn");
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
                println!("wrote bug report: {}", path.display());
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
                Ok(Some(n)) => println!("session name: {n}"),
                Ok(None) => println!("(unnamed session)"),
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
                println!("session name set to: {trimmed}");
                CommandOutcome::Handled
            }
            Err(e) => CommandOutcome::Error(format!("set name failed: {e}")),
        }
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
            println!("(no sessions for this cwd)");
            return CommandOutcome::Handled;
        }
        println!("Sessions:");
        for e in entries {
            let preview = e.preview.as_deref().unwrap_or("");
            let id_short: String = e.id.chars().take(16).collect();
            println!("  {}  {}  {}", id_short, e.created_at, preview);
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
        println!("shared: {url}");
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
                    println!("removed credential for `{provider}`");
                    CommandOutcome::Handled
                }
                Err(e) => CommandOutcome::Error(format!("save auth store: {e}")),
            },
            None => {
                println!("no credential stored for `{provider}`");
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
                        println!("  {path_short}  {snip}");
                    }
                }
            }
        }
        if hits == 0 {
            println!("(no matches)");
        } else {
            println!("({hits} match(es))");
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
            println!("(no history yet)");
            return CommandOutcome::Handled;
        }
        let start = entries.len().saturating_sub(limit);
        for (i, e) in entries[start..].iter().enumerate() {
            let n = start + i + 1;
            // Truncate long entries to 200 chars to keep the listing skimmable.
            let preview: String = e.chars().take(200).collect();
            let suffix = if preview.len() < e.len() { "…" } else { "" };
            println!("  {n}: {preview}{suffix}");
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
                    println!("{line}");
                }
                CommandOutcome::Handled
            }
            "rules" => {
                let rules = crate::triggers::global_registry().list();
                for line in render_dynamic_trigger_rules(&rules, usize::MAX) {
                    println!("{line}");
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
                            println!("removed {count} dynamic trigger rule(s)");
                            CommandOutcome::Handled
                        }
                        Err(e) => CommandOutcome::Error(e.to_string()),
                    }
                } else {
                    match crate::triggers::global_registry().remove_rule(target) {
                        Ok(Some(rule)) => {
                            println!("removed trigger {}", rule.id);
                            println!("  condition: {}", rule.condition);
                            println!("  action: {}", rule.action);
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
                    println!("{line}");
                }
                CommandOutcome::Handled
            }
            "running" => {
                let snapshot = ctx.harness.notification_status_snapshot();
                for line in render_running_triggers(&snapshot.running) {
                    println!("{line}");
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
                    println!("{line}");
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
                    println!("requested abort for {count} running trigger(s)");
                } else {
                    if !snapshot.running.iter().any(|t| t.trace_id == *target) {
                        return CommandOutcome::Error(format!(
                            "no running trigger with trace_id '{target}'"
                        ));
                    }
                    ctx.harness.abort_trigger(target);
                    println!("requested abort for trigger {target}");
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
            error_context: "create trigger",
        }
    }
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
            println!("{state} trigger {}", rule.id);
            println!("  condition: {}", rule.condition);
            println!("  action: {}", rule.action);
            if rule.enabled && rule.fire_once {
                println!("  fire_once: true (will disable again after the next successful match)");
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

pub async fn dispatch(input: &str, registry: &Registry, ctx: &CommandCtx<'_>) -> CommandOutcome {
    let (name, argv) = match parse(input) {
        Some(parts) => parts,
        None => return CommandOutcome::Error("not a slash command".into()),
    };
    // Special-case `/help`: the handler can't see the registry, so we render here.
    if name == "help" {
        print_help(registry);
        return CommandOutcome::Handled;
    }
    let Some(cmd) = registry.find(&name) else {
        return CommandOutcome::Error(format!("unknown command: /{name} (try /help)"));
    };
    cmd.run(&argv, ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn registry_lookup_by_name_and_alias() {
        let r = Registry::with_builtins();
        assert!(r.find("quit").is_some());
        assert!(r.find("q").is_some());
        assert!(r.find("exit").is_some());
        assert!(r.find("triggers").is_some());
        assert!(r.find("nope").is_none());
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

    /// Helper: build a Skill record only filling the fields `skill_source_label` reads.
    fn skill_with_path(name: &str, file_path: &str) -> Skill {
        Skill {
            name: name.into(),
            description: String::new(),
            file_path: file_path.into(),
            content: String::new(),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn skill_source_label_recognizes_builtin_synthetic_path() {
        // Built-in skills (#32) carry a synthetic `<builtin>/<name>/SKILL.md` path. `/skills`
        // must classify these as `builtin`, not fall through to the `source path` catch-all.
        let s = skill_with_path(
            "karpathy-guidelines",
            "<builtin>/karpathy-guidelines/SKILL.md",
        );
        let cwd = std::path::PathBuf::from("/tmp/some-project");
        assert_eq!(skill_source_label(&s, &cwd), "builtin");
    }

    #[test]
    fn skill_source_label_distinguishes_builtin_from_project_and_user() {
        // Round out the test by confirming the new builtin branch doesn't shadow the existing
        // project / user paths. This locks `/skills` source classification across all three
        // tiers in one place.
        let cwd = std::path::PathBuf::from("/repo");
        let project = skill_with_path("p", "/repo/.pie/skills/p/SKILL.md");
        assert_eq!(skill_source_label(&project, &cwd), "project");
        let user = skill_with_path("u", "/home/me/.pie/skills/u/SKILL.md");
        assert_eq!(skill_source_label(&user, &cwd), "user");
        let builtin = skill_with_path("b", "<builtin>/b/SKILL.md");
        assert_eq!(skill_source_label(&builtin, &cwd), "builtin");
        let unknown = skill_with_path("x", "/some/weird/place/x/SKILL.md");
        assert_eq!(skill_source_label(&unknown, &cwd), "source path");
    }
}
