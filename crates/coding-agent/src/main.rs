//! pie-coding-agent — minimal coding agent CLI on top of pie-agent-core.
//!
//! Modeled on `packages/coding-agent/` (the TS implementation) in spirit: same tools
//! (`read`/`write`/`edit`/`bash`/`ls`/`grep`/`find` + `memory`), same `--resume` semantics
//! scoped by cwd hash, same "interactive TUI" mode, dual-root skills loader (project ↻ user).
//! Trimmed scope: no extensions, no themes, no print/rpc/json modes.

mod agent_session;
mod auth;
mod bug_report;
mod builtin_skills;
mod clipboard_image;
mod commands;
mod config;
mod control_plane_prompt;
mod debug;
mod export;
mod extensions;
mod goal;
mod history;
mod hooks;
#[allow(dead_code)]
mod images;
mod local_models;
mod logging;
mod lsp;
mod lsp_supervisor;
mod markdown;
mod mcp_loader;
mod mentions;
mod model;
mod oauth;
mod otlp;
mod readline;
mod resume_picker;
mod session;
mod session_archive;
mod skills;
mod skills_state;
mod templates;
mod tools;
mod triggers;
mod ui;

use std::io::IsTerminal as _;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentMessage, JsonlSessionRepo, PermissionPolicy,
    ThinkingLevel,
};
use pie_ai::Message as PiMessage;

#[derive(Parser, Debug)]
#[command(
    name = "pie",
    version,
    about = "Simple coding agent on top of pie-agent-core"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    /// Provider id (anthropic, openai, openrouter, …). When unset, auto-detected from env.
    #[arg(long)]
    provider: Option<String>,
    /// Model id within the provider's catalog.
    #[arg(long)]
    model: Option<String>,
    /// Override the selected model's base URL for this run. Useful for local OpenAI-compatible
    /// servers such as DS4.
    #[arg(long = "base-url", value_name = "URL")]
    base_url: Option<String>,
    /// Thinking level (off | minimal | low | medium | high | xhigh).
    #[arg(
        long,
        default_value = "off",
        value_parser = clap::builder::PossibleValuesParser::new(commands::THINKING_LEVEL_VALUES)
    )]
    thinking: String,

    /// Select a session for this cwd to resume. Pass an id to resume a specific one
    /// directly (same as --resume-id); bare --resume opens the picker.
    #[arg(long, value_name = "ID", num_args = 0..=1)]
    resume: Option<Option<String>>,
    /// Continue the most recent session for this cwd.
    #[arg(long = "continue", short = 'c')]
    continue_: bool,
    /// Resume a specific session by id (full UUIDv7 or a unique prefix).
    #[arg(long, value_name = "ID")]
    resume_id: Option<String>,

    /// List sessions for this cwd and exit.
    #[arg(long)]
    list_sessions: bool,
    /// List sessions across every cwd we know about (~/.pie/sessions/*) and exit.
    #[arg(long)]
    list_all_sessions: bool,
    /// Delete a session by id and exit.
    #[arg(long, value_name = "ID")]
    delete_session: Option<String>,
    /// Attach an image to the first prompt of this session. Repeatable. Supported formats:
    /// PNG, JPEG, WebP, GIF. Each image is capped at 10 MiB; max 10 per message.
    #[arg(long = "image", value_name = "PATH")]
    image: Vec<std::path::PathBuf>,

    /// Enable a built-in skill bundled with this `pie` binary, by name. Repeatable. Unknown
    /// names hard-fail with a list of available built-ins. Built-in skills are the lowest
    /// precedence — user (`~/.pie/skills/`) and project (`<cwd>/.pie/skills/`) skills of the
    /// same name still override. Persistent enable is via `~/.pie/config.toml`
    /// `[builtin_skills] enabled = [...]`; CLI + config are unioned and de-duplicated.
    #[arg(long = "builtin-skill", value_name = "NAME")]
    builtin_skill: Vec<String>,

    /// Poll interval for local dynamic trigger checks, in seconds. Defaults to
    /// `[triggers] poll_interval_secs` from `~/.pie/config.toml`, or 600 when unset.
    #[arg(long = "trigger-poll-secs", value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    trigger_poll_secs: Option<u64>,

    /// Show LLM call debug logs in the conversation feed, including trigger/sub-agent calls.
    #[arg(long)]
    debug: bool,

    /// Auto-approve control-plane prompts.
    #[arg(long)]
    yes: bool,

    /// Auto-approve every approval prompt, including control-plane writes.
    #[arg(long = "always-allow")]
    always_allow: bool,

    /// Run the local browser UI instead of the terminal UI. Defaults to loopback-only.
    #[arg(long, conflicts_with = "tui")]
    web: bool,
    /// Run the terminal UI even when local defaults would open the Web UI.
    #[arg(long, conflicts_with = "web")]
    tui: bool,
    /// Host for `--web`. Must be a loopback address.
    #[arg(long = "web-host", default_value = "127.0.0.1", value_name = "HOST")]
    web_host: String,
    /// Port for `--web`; use 0 to bind a random free port.
    #[arg(long = "web-port", default_value_t = 0, value_name = "PORT")]
    web_port: u16,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Export or import replayable `.piesession` backups.
    Session {
        #[command(subcommand)]
        command: SessionCliCommand,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCliCommand {
    /// Export a session transcript and automation sidecars to a `.piesession` archive.
    Export {
        /// Session id to export (full UUIDv7 or unique prefix). Defaults to newest for this cwd.
        #[arg(long, conflicts_with = "current")]
        session: Option<String>,
        /// Export the newest session for this cwd.
        #[arg(long)]
        current: bool,
        /// Destination `.piesession` file. Defaults to `pie-session-<id>.piesession` in cwd.
        #[arg(long, value_name = "FILE")]
        output: Option<std::path::PathBuf>,
        /// Do not include dynamic trigger or cron sidecars.
        #[arg(long = "exclude-triggers")]
        exclude_triggers: bool,
    },
    /// Import a `.piesession` archive as a new local session.
    Import {
        /// `.piesession` archive to import.
        file: std::path::PathBuf,
        /// Cwd to write into the imported session metadata. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        cwd: Option<std::path::PathBuf>,
        /// Activation mode for imported triggers/crons. Defaults to disabled; ask is reserved.
        #[arg(long = "activate-triggers", default_value = "off")]
        activate_triggers: ActivateTriggersArg,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ActivateTriggersArg {
    Off,
    Ask,
    On,
}

impl From<ActivateTriggersArg> for session_archive::ActivateTriggers {
    fn from(value: ActivateTriggersArg) -> Self {
        match value {
            ActivateTriggersArg::Off => Self::Off,
            ActivateTriggersArg::Ask => Self::Ask,
            ActivateTriggersArg::On => Self::On,
        }
    }
}

impl Cli {
    /// Session id to resume, merging both spellings: `--resume-id <id>` wins, then
    /// `--resume <id>`. Bare `--resume` (the picker) yields `None`.
    fn effective_resume_id(&self) -> Option<&str> {
        self.resume_id
            .as_deref()
            .or_else(|| self.resume.as_ref().and_then(|inner| inner.as_deref()))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    print_dynamic_help_and_exit_if_requested()?;
    let cli = Cli::parse();
    let cwd = std::env::current_dir().context("getting cwd")?;
    let repo = session::open_repo(&cwd).await;

    if let Some(command) = &cli.command {
        return run_cli_command(command, &repo, &cwd).await;
    }

    if cli.list_sessions {
        return list_sessions_cmd(&repo).await;
    }
    if cli.list_all_sessions {
        return list_all_sessions_cmd().await;
    }
    if let Some(id) = &cli.delete_session {
        return delete_session_cmd(&repo, id).await;
    }

    run_repl(cli, cwd, repo).await
}

async fn run_cli_command(
    command: &CliCommand,
    repo: &JsonlSessionRepo,
    cwd: &std::path::Path,
) -> Result<()> {
    match command {
        CliCommand::Session { command } => run_session_cli_command(command, repo, cwd).await,
    }
}

async fn run_session_cli_command(
    command: &SessionCliCommand,
    repo: &JsonlSessionRepo,
    cwd: &std::path::Path,
) -> Result<()> {
    match command {
        SessionCliCommand::Export {
            session,
            current: _,
            output,
            exclude_triggers,
        } => {
            let session_path = if let Some(id) = session {
                session::find_path_by_id(repo, id)
                    .await?
                    .with_context(|| format!("no session matches id {id}"))?
            } else {
                session::newest_path(repo).await?.with_context(|| {
                    format!("no sessions to export in {}", repo.root().display())
                })?
            };
            let session = repo.open(&session_path).await?;
            let metadata = session.storage().get_metadata_json().await?;
            let session_id = metadata
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    session_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("session")
                });
            let output_path = output
                .clone()
                .unwrap_or_else(|| session_archive::default_export_path(cwd, session_id));
            let output_path = if output_path.is_absolute() {
                output_path
            } else {
                cwd.join(output_path)
            };
            print_session_archive_warning();
            let summary =
                session_archive::export_session(&session_path, &output_path, *exclude_triggers)
                    .await?;
            println!(
                "exported session archive: {}",
                summary.output_path.display()
            );
            println!(
                "session {} entries={} triggers={} cron={}",
                short_id(&summary.session_id),
                summary.entry_count,
                yes_no(summary.has_triggers),
                yes_no(summary.has_cron)
            );
            Ok(())
        }
        SessionCliCommand::Import {
            file,
            cwd: import_cwd,
            activate_triggers,
        } => {
            let archive_path = if file.is_absolute() {
                file.clone()
            } else {
                cwd.join(file)
            };
            let target_cwd = import_cwd.clone().unwrap_or_else(|| cwd.to_path_buf());
            print_session_archive_warning();
            let summary = session_archive::import_session(
                repo,
                &archive_path,
                &target_cwd,
                (*activate_triggers).into(),
            )
            .await?;
            println!("imported session: {}", short_id(&summary.session_id));
            println!("path: {}", summary.session_path.display());
            println!(
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
            println!("resume with: pie --resume-id {}", summary.session_id);
            Ok(())
        }
    }
}

fn print_session_archive_warning() {
    println!(
        "warning: .piesession archives include transcript and tool history. They do not include separate auth stores, provider credentials, OAuth tokens, or MCP config."
    );
}

fn short_id(id: &str) -> String {
    id.chars().take(16).collect()
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn print_dynamic_help_and_exit_if_requested() -> Result<()> {
    if !should_print_dynamic_top_level_help(std::env::args_os().skip(1)) {
        return Ok(());
    }
    let mut command = Cli::command().after_help(commands::cli_model_help_text());
    command.print_help()?;
    println!();
    std::process::exit(0);
}

fn should_print_dynamic_top_level_help<I>(args: I) -> bool
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let subcommands: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_string())
        .collect();
    let mut has_help = false;
    for arg in args {
        if arg == "--help" || arg == "-h" {
            has_help = true;
            continue;
        }
        if arg
            .to_str()
            .is_some_and(|arg| subcommands.iter().any(|subcommand| subcommand == arg))
        {
            return false;
        }
    }
    has_help
}

async fn list_sessions_cmd(repo: &JsonlSessionRepo) -> Result<()> {
    let entries = session::list_entries(repo).await?;
    if entries.is_empty() {
        println!("(no sessions for this cwd)");
        return Ok(());
    }
    println!("sessions in {}:", repo.root().display());
    for e in entries {
        let preview = e.preview.as_deref().unwrap_or("");
        let badge = e
            .automation
            .badge()
            .map(|b| format!("  [{b}]"))
            .unwrap_or_default();
        println!(
            "  {}  {}{}  {}",
            &e.id[..16.min(e.id.len())],
            e.created_at,
            badge,
            preview
        );
    }
    Ok(())
}

/// List sessions across every cwd-hash bucket under `<base>/sessions/`. For each session we
/// show: short id, the cwd it was created from, created-at timestamp, first user-message
/// preview.
async fn list_all_sessions_cmd() -> Result<()> {
    let root = config::base_dir().join("sessions");
    if !root.exists() {
        println!("(no sessions root: {})", root.display());
        return Ok(());
    }
    let mut buckets = Vec::new();
    let mut rd = tokio::fs::read_dir(&root)
        .await
        .with_context(|| format!("read {}", root.display()))?;
    while let Some(entry) = rd.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            buckets.push(entry.path());
        }
    }
    buckets.sort();

    let mut all = Vec::new();
    for b in &buckets {
        let repo = pie_agent_core::JsonlSessionRepo::new(b);
        // list_entries may return Err if the bucket is empty/malformed; skip those gracefully.
        let entries = session::list_entries(&repo).await.unwrap_or_default();
        for e in entries {
            all.push((b.clone(), e));
        }
    }
    if all.is_empty() {
        println!("(no sessions found under {})", root.display());
        return Ok(());
    }
    // Sort by session id (UUIDv7, time-ordered) so newest is last in output.
    all.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    println!("All sessions ({}):", all.len());
    for (bucket, e) in all {
        let bucket_name = bucket.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let preview = e.preview.as_deref().unwrap_or("");
        let id_short: String = e.id.chars().take(16).collect();
        let badge = e
            .automation
            .badge()
            .map(|b| format!("  [{b}]"))
            .unwrap_or_default();
        println!(
            "  {bucket_name}/{id_short}  {}{badge}  {preview}",
            e.created_at
        );
    }
    Ok(())
}

async fn delete_session_cmd(repo: &JsonlSessionRepo, id: &str) -> Result<()> {
    let path = session::delete_by_id(repo, id).await?;
    println!("deleted {}", path.display());
    Ok(())
}

async fn select_resume_session(
    repo: &JsonlSessionRepo,
    cwd: &std::path::Path,
) -> Result<(pie_agent_core::Session, bool)> {
    let mut entries = session::list_entries(repo).await?;
    if entries.is_empty() {
        anyhow::bail!("no sessions to resume in {}", repo.root().display());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "multiple sessions found in {}; run `pie --list-sessions` and resume one with `pie --resume-id <id>`",
            repo.root().display()
        );
    }

    entries.reverse(); // newest first — index 0 in the picker is the latest session
    let rows: Vec<resume_picker::PickerRow> = entries
        .iter()
        .map(|entry| resume_picker::PickerRow {
            id_short: entry.id.chars().take(16).collect(),
            // RFC3339 with sub-second precision is noise in a menu; minutes are enough.
            created_at: entry.created_at.chars().take(16).collect(),
            badge: entry.automation.badge(),
            preview: entry.preview.clone().unwrap_or_default(),
        })
        .collect();
    let choice = tokio::task::spawn_blocking(move || resume_picker::pick_blocking(&rows))
        .await
        .context("resume picker task")??;
    match choice {
        resume_picker::PickerChoice::Clean => Ok((session::create(repo, cwd).await?, false)),
        resume_picker::PickerChoice::Resume(selected) => {
            Ok((repo.open(&entries[selected].path).await?, true))
        }
        resume_picker::PickerChoice::Cancelled => anyhow::bail!("resume selection cancelled"),
    }
}

async fn run_repl(mut cli: Cli, cwd: std::path::PathBuf, repo: JsonlSessionRepo) -> Result<()> {
    let run_web = should_run_web(&cli);
    let cli_base_url = cli.base_url.clone();
    validate_base_url_override(&cli)?;
    let local_models = local_models::load_all(&cwd, cli_base_url.as_deref()).await?;
    let mut model_credential_warning: Option<String> = None;
    let mut model = match model::auto_detect_model(cli.provider.as_deref(), cli.model.as_deref()) {
        Ok(model) => model,
        // No credential anywhere and no explicit override: start anyway so
        // notification-only sessions (e.g. summary-mode webhook endpoints) still work.
        // The first model turn surfaces the auth error; `/login` fixes it live.
        Err(e)
            if cli.provider.is_none()
                && cli.model.is_none()
                && e.to_string().starts_with("no API key found") =>
        {
            model_credential_warning = Some(e.to_string());
            model::credential_less_default()
        }
        Err(e) => return Err(e),
    };
    if let Some(base_url) = cli_base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        model.base_url = base_url.to_string();
    }
    let thinking = parse_thinking(&cli.thinking)?;

    // Resolve / create the session. `--resume` asks the user which cwd-scoped transcript to
    // reopen, while `--continue` keeps the old "newest session" fast path.
    let should_resume = cli.resume.is_some() || cli.continue_ || cli.resume_id.is_some();
    let (session, resumed) = if should_resume {
        if let Some(id) = cli.effective_resume_id() {
            (session::resume(&repo, Some(id)).await?, true)
        } else if cli.resume.is_some() {
            select_resume_session(&repo, &cwd).await?
        } else {
            (session::resume(&repo, None).await?, true)
        }
    } else {
        let s = session::create(&repo, &cwd).await?;
        (s, false)
    };
    let session_metadata = session.storage().get_metadata_json().await?;
    let session_id = session_metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let dynamic_trigger_path = session::trigger_sidecar_path_for_session(&session, &repo).await?;

    // Install the tracing subscriber. Failure is non-fatal — we keep running without logs.
    let logging = logging::init(&session_id);

    // Feed channel is created before the harness so debug stream wrappers and trigger hooks can
    // buffer UI-visible diagnostics even if they fire during startup.
    let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel::<ui::FeedUpdate>();

    // Build the harness.
    let stream_fn = if cli.debug {
        debug::wrap_stream_fn(stream_fn_with_auth_store(), feed_tx.clone())
    } else {
        stream_fn_with_auth_store()
    };
    let dynamic_trigger_registry = triggers::global_registry().clone();
    let dynamic_trigger_load_error = dynamic_trigger_registry
        .load_from_path(dynamic_trigger_path)
        .err();
    let cron_registry = triggers::global_cron_registry().clone();
    let cron_path = session::cron_sidecar_path_for_session(&session, &repo).await?;
    let cron_load_error = cron_registry.load_from_path(cron_path).err();
    let memory_dir = config::memory_dir();
    let mut tools = tools::default_tools(memory_dir.clone());
    // Task delegation tool (issue #11). Shares the parent's model + stream backend so its
    // subagents go through the same provider.
    tools.push(tools::task_tool(model.clone(), Some(stream_fn.clone())));
    // Skill tool (issue #25). Needs to reach the live `AgentHarness::skills()` snapshot, but
    // the harness does not exist yet — we are still assembling the tool list that will be
    // passed to `AgentHarness::new`. Use a `OnceCell` that we'll fill immediately after the
    // harness is constructed, before the REPL accepts any input.
    let skill_harness_cell: tools::skill::SkillHarnessCell =
        std::sync::Arc::new(once_cell::sync::OnceCell::new());
    tools.push(tools::skill_tool(skill_harness_cell.clone()));
    // InstallSkill tool (issue #87). Shares the same `skill_harness_cell` as `skill_tool`
    // so post-install it can call `harness.reload_skills_from_disk()` to hot-reload the
    // catalog. Two-phase preview→confirm safety inside the tool; see
    // `crates/coding-agent/src/tools/install_skill.rs` for the security model.
    tools.push(tools::install_skill_tool(skill_harness_cell.clone()));
    // SkillBuilder tool (issue #21). Authors a NEW user skill from structured fields —
    // renders the canonical SKILL.md itself, then shares InstallSkill's validation,
    // atomic-write, and hot-reload path via the same harness cell.
    tools.push(tools::skill_builder_tool(skill_harness_cell.clone()));
    // SetSkillState tool (task #23, S-A2). Enable/disable a loaded skill at runtime via the
    // `~/.pie/skills-state.json` overlay; shares the same harness cell so it can reload the
    // catalog after writing.
    tools.push(tools::set_skill_state_tool(skill_harness_cell.clone()));
    // RemoveSkill tool (task #23, S-A2b). Deletes a user-installed skill dir + clears its
    // overlay entry + reloads; builtin/project skills are refused (disable instead).
    tools.push(tools::remove_skill_tool(skill_harness_cell.clone()));
    tools.push(tools::new_cron_job_tool(skill_harness_cell.clone()));
    tools.push(tools::list_cron_jobs_tool());
    tools.push(tools::remove_cron_job_tool(skill_harness_cell.clone()));
    tools.push(tools::set_cron_job_state_tool(skill_harness_cell.clone()));
    tools.push(tools::new_trigger_tool());
    tools.push(tools::list_triggers_tool());
    tools.push(tools::remove_trigger_tool());
    tools.push(tools::set_trigger_state_tool());

    // MCP (issue #9): spawn every server configured under ~/.pie/mcp.toml or
    // <cwd>/.pie/mcp.toml, append their tools to the registry. MCP push adapters are
    // registered as trigger sources a few lines below, once we have an `Arc<AgentHarness>`.
    let mcp = mcp_loader::load_all(&cwd).await;
    let mcp_tool_count = mcp.tools.len();
    let mcp_tool_names = mcp
        .tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect::<Vec<_>>();
    let mcp_server_names = mcp.server_names.clone();
    let mcp_notification_hooks = mcp.notification_hooks;
    let mcp_notification_hook_count = mcp_notification_hooks.len();
    let mcp_inject_summary_servers = mcp.inject_summary_servers;
    let mcp_inject_and_run_servers = mcp.inject_and_run_servers;
    tools.extend(mcp.tools);
    let tool_names = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect::<Vec<_>>();
    let memory_block = tools::memory::load_memory_block(&memory_dir).await;
    let system_prompt = compose_system_prompt(&cwd, &memory_block, &tool_names);

    let loaded_skills = skills::load_all(&cwd).await;
    let loaded_templates = templates::load_all(&cwd).await;

    // Built-in skill resolution (issue #32). The CLI flag `--builtin-skill <name>` is the
    // one-time enable path; `~/.pie/config.toml [builtin_skills] enabled = [...]` is the
    // persistent path. Unknown names from the CLI hard-fail with a non-zero exit; unknown
    // names in the config produce a startup diagnostic but do not block. Both inputs are
    // unioned and de-duplicated. Built-in skills are appended *first* so the later user /
    // project layers (already in `loaded_skills.skills`) can shadow on name collision via
    // the same precedence rule the harness already uses.
    let config_enabled_builtins = read_builtin_skills_config(&config::base_dir()).await;
    let (trigger_poll_secs, trigger_config_diagnostic) =
        read_trigger_poll_interval_secs(&config::base_dir(), cli.trigger_poll_secs).await;
    triggers::dynamic::set_dynamic_trigger_poll_interval_secs(trigger_poll_secs);
    let resolved_builtins =
        match builtin_skills::resolve_builtins(&cli.builtin_skill, &config_enabled_builtins) {
            Ok(r) => r,
            Err(e) => {
                // Hard fail on unknown CLI name — non-zero exit with the available list.
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        };
    let mut combined_skills = builtin_skills::merge_with_user_project(
        resolved_builtins.skills.clone(),
        &loaded_skills.skills,
    );
    // Apply the runtime enable/disable overlay (`~/.pie/skills-state.json`). A user who ran
    // `/skills disable <name>` (or the SetSkillState tool) sees that choice survive across
    // restarts without their SKILL.md being edited. Keyed by {source, name}.
    {
        let state = skills_state::load(&config::base_dir()).await;
        skills_state::apply(&state, &mut combined_skills);
    }

    let goal_harness_cell: Arc<OnceLock<Arc<AgentHarness>>> = Arc::new(OnceLock::new());
    let mut opts = AgentHarnessOptions::new(model.clone(), session.clone());
    opts.system_prompt = system_prompt;
    opts.thinking_level = thinking;
    opts.tools = tools;
    opts.skills = combined_skills.clone();
    opts.prompt_templates = loaded_templates.templates.clone();
    opts.stream_fn = Some(stream_fn.clone());
    // Skill catalog hot-reload. `AgentHarness::reload_skills_from_disk()` invokes this
    // closure, so every reload entry point (the future `InstallSkillTool`, `/skills
    // reload`, any control-plane API) shares the same source directories and dedup policy
    // we used at startup — no path drift between "where skills get loaded from" and
    // "where reload looks." Built-in skills are re-merged so a user-installed skill of
    // the same name shadows the built-in just like at startup.
    opts.reload_skills_fn = Some({
        let cwd = cwd.clone();
        let builtins = resolved_builtins.skills.clone();
        std::sync::Arc::new(move || {
            let cwd = cwd.clone();
            let builtins = builtins.clone();
            Box::pin(async move {
                let loaded = skills::load_all(&cwd).await;
                let mut merged = builtin_skills::merge_with_user_project(builtins, &loaded.skills);
                // Re-apply the enable/disable overlay on every reload so a disabled skill
                // stays disabled after an install/remove/reload. Same source-of-truth as the
                // startup path above.
                let state = skills_state::load(&config::base_dir()).await;
                skills_state::apply(&state, &mut merged);
                pie_agent_core::LoadSkillsOutput {
                    skills: merged,
                    diagnostics: loaded.diagnostics,
                }
            })
        })
    });
    opts.on_turn_end = Some(goal::stop_hook(goal_harness_cell.clone()));
    opts.turn_continuation_cap = Some(goal::MAX_CONTINUATIONS);
    opts.before_tool_call =
        Some(PermissionPolicy::default_for_coding_agent().as_before_tool_call());
    let interactive_tui =
        !run_web && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let control_plane_prompt_rx = if cli.always_allow || cli.yes {
        opts.on_control_plane_prompt = Some(control_plane_prompt::allow_hook());
        None
    } else if interactive_tui || run_web {
        let (hook, rx) = control_plane_prompt::interactive_hook();
        opts.on_control_plane_prompt = Some(hook);
        Some(rx)
    } else {
        opts.on_control_plane_prompt = Some(control_plane_prompt::deny_hook(
            "control-plane prompt requires an interactive terminal; run pie in a TTY to approve this action",
        ));
        None
    };
    // Triggers from MCP servers configured with `inject_summary` / `inject_and_run` bypass
    // the sub-agent and inject their pushed summary into chat (the latter also runs one
    // model turn in the parent context); everything else falls through to the dynamic-rule
    // hook. The match is structural (server name), no model.
    opts.before_trigger_action = Some(triggers::cron_action_hook(
        cron_registry.clone(),
        triggers::direct_inject_action_hook(
            mcp_inject_summary_servers,
            mcp_inject_and_run_servers,
            triggers::before_trigger_action_hook(dynamic_trigger_registry.clone()),
        ),
    ));
    // LSP feedback loop (issue #12): attach diagnostics to write/edit tool results when
    // ~/.pie/lsp.toml or <cwd>/.pie/lsp.toml is configured.
    let lsp_supervisor = std::sync::Arc::new(lsp_supervisor::LspSupervisor::load(&cwd).await);
    let lsp_lang_count = lsp_supervisor.language_count();
    if !lsp_supervisor.is_empty() {
        opts.after_tool_call = Some(lsp_supervisor::as_after_tool_call(lsp_supervisor.clone()));
    }
    let harness = std::sync::Arc::new(AgentHarness::new(opts));

    // Resolve the Skill tool's chicken-and-egg harness reference (issue #25). The cell was
    // handed to the tool at construction time; we set it now, before the REPL accepts any
    // input. The `is_ok()` assert is a double-init guard: any future refactor that
    // accidentally reaches this line twice will surface as a test/CI failure rather than as a
    // runtime panic on the second set.
    //
    // This must happen BEFORE `register_notification_hook` below — RFC 1 sub-PR 5 will
    // make accepted triggers spawn agent-loop tasks, and one of those could land on the
    // Skill tool before the REPL ever runs. If we registered hooks first, a fast MCP push
    // (server emits `tools/listChanged` mid-handshake) could race the Skill cell set and
    // hit an unset `OnceCell`. Today the trigger pipeline only persists audit + emits
    // `TriggerHandled` so the race is benign, but keeping the order locked here means the
    // tool surface is fully initialized the moment the trigger surface goes live.
    assert!(
        skill_harness_cell.set(harness.clone()).is_ok(),
        "Skill tool harness cell was set twice; main.rs wiring is the only setter"
    );
    assert!(
        goal_harness_cell.set(harness.clone()).is_ok(),
        "Goal hook harness cell was set twice; main.rs wiring is the only setter"
    );

    // Wire each MCP server's trigger-source adapter into the harness now that all
    // tool-initialized state (including the Skill cell above) is in place.
    // `register_notification_hook` spawns a driver task that runs `hook.run(sink)` and a
    // pump task that drains the sink into `handle_trigger`; both tear down naturally when
    // the MCP transport closes or the harness drops.
    for hook in mcp_notification_hooks {
        harness.register_notification_hook(hook);
    }
    harness.register_notification_hook(std::sync::Arc::new(triggers::CronNotificationHook::new(
        cron_registry.clone(),
    )));
    harness.register_notification_hook(std::sync::Arc::new(
        triggers::DynamicTriggerCheckHook::new(dynamic_trigger_registry.clone()),
    ));
    // Resume hydration (if --resume) — the rebuilt transcript is replayed into the feed below.
    let replay_context = if resumed {
        Some(harness.rehydrate_from_session().await?)
    } else {
        None
    };
    let display_model = harness
        .agent()
        .state()
        .model
        .clone()
        .unwrap_or_else(|| model.clone());
    let (hook_model, hook_thinking) = {
        let state = harness.agent().state();
        (state.model.clone(), state.thinking_level)
    };
    let hooks = hooks::load(&cwd, session_id.clone(), hook_model.as_ref(), hook_thinking).await;

    // Feed + trigger channels. Agent/harness listeners and the slash-command console sink push
    // structured updates onto `feed_tx`; the UI loop drains `feed_rx` and renders. Inject-and-run
    // triggered turns arrive on `main_run_*`. The full-screen TUI is the only terminal writer, so
    // nothing here writes to stdout directly.
    {
        let tx = feed_tx.clone();
        commands::console::set_sink(Box::new(move |line| {
            let _ = tx.send(ui::FeedUpdate::Plain {
                text: line,
                level: ui::feed::Level::Output,
            });
        }));
    }
    let (main_run_tx, main_run_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    let mut app = ui::App::new(ui::AppConfig {
        harness: harness.clone(),
        retry: agent_session::RetrySettings::default(),
        registry: commands::Registry::with_builtins(),
        cwd: cwd.clone(),
        session_id: session_id.clone(),
        log_path: logging.as_ref().map(|l| l.log_path.clone()),
        tool_count: tool_names.len(),
        history: history::HistoryStore::load(),
        pending_images: std::mem::take(&mut cli.image),
        feed_rx,
        main_run_rx,
        control_plane_prompt_rx,
        panel_status: ui::PanelStatus {
            mcp_servers: mcp.client_count,
            mcp_tools: mcp_tool_count,
            mcp_server_names,
            mcp_tool_names,
            tool_names: tool_names.clone(),
            mcp_notification_hooks: mcp_notification_hook_count,
            hook_points: active_hook_registrations(lsp_lang_count, !hooks.runner.is_empty()),
            trigger_features: active_trigger_features(),
        },
    });
    app.banner(&display_model, &session_id, resumed, &tool_names);
    if !local_models.models.is_empty() {
        app.system_line(format!(
            "loaded {} local model(s): {}",
            local_models.models.len(),
            local_models
                .models
                .iter()
                .map(|m| format!("{}:{}", m.provider.0, m.id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Surface built-in skill resolution diagnostics (e.g. unknown names in config). The CLI
    // hard-fail path returns early before reaching here, so anything we have at this point is
    // a soft warning. Print one line per diagnostic so the user can see what the config
    // ignored.
    for diag in &resolved_builtins.diagnostics {
        app.system_line(diag);
    }
    if let Some(diag) = trigger_config_diagnostic {
        app.error_line(diag);
    }
    if !combined_skills.is_empty() {
        app.system_line(format!(
            "loaded {} skill(s): {}",
            combined_skills.len(),
            combined_skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(warning) = &model_credential_warning {
        app.error_line(format!(
            "warning: {warning} Started without a model — chat turns will fail until a key is \
             provided; notification-only features (e.g. webhook endpoints) still work."
        ));
    }
    if let Some(err) = &dynamic_trigger_load_error {
        app.error_line(format!("dynamic triggers: {err}"));
    } else if !dynamic_trigger_registry.list().is_empty() {
        let location = dynamic_trigger_registry
            .storage_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "memory".into());
        app.system_line(format!(
            "loaded {} dynamic trigger rule(s) from {}",
            dynamic_trigger_registry.list().len(),
            location
        ));
    }
    if let Some(err) = &cron_load_error {
        app.error_line(format!("cron: {err}"));
    } else if !cron_registry.list().is_empty() {
        let location = cron_registry
            .storage_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "memory".into());
        app.system_line(format!(
            "loaded {} cron job(s) from {}",
            cron_registry.list().len(),
            location
        ));
    }
    // Cron jobs and trigger rules only run while their own session is open. If this session
    // has none but a sibling session does, say so once — otherwise exiting that session
    // silently stops the user's automation with no trace anywhere in the UI.
    if dynamic_trigger_registry.list().is_empty() && cron_registry.list().is_empty() {
        let current_session_path = session_metadata
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);
        if let Some(hint) =
            session::automation_elsewhere_hint(&repo, current_session_path.as_deref()).await
        {
            app.system_line(hint);
        }
    }
    if !loaded_templates.templates.is_empty() {
        app.system_line(format!(
            "loaded {} template(s): {}",
            loaded_templates.templates.len(),
            loaded_templates
                .templates
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if mcp.client_count > 0 {
        app.system_line(format!(
            "mcp: connected to {} server(s), {mcp_tool_count} extra tool(s)",
            mcp.client_count,
        ));
    }
    if mcp_notification_hook_count > 0 {
        app.system_line(format!(
            "trigger sources: watching {} configured MCP push source(s)",
            mcp_notification_hook_count
        ));
    }
    if cli.debug {
        app.system_line("debug: LLM call logging is enabled");
    }
    app.system_line(format!(
        "triggers: local dynamic checker polls every {trigger_poll_secs}s while enabled rules exist"
    ));
    if lsp_lang_count > 0 {
        app.system_line(format!(
            "lsp: {lsp_lang_count} language(s) configured; diagnostics attach to edit/write results"
        ));
    }
    for diag in &mcp.diagnostics {
        app.error_line(format!("mcp: {diag}"));
    }
    if !loaded_templates.diagnostics.is_empty() {
        app.system_line(format!(
            "templates loader: {} diagnostic(s), first: {}",
            loaded_templates.diagnostics.len(),
            loaded_templates.diagnostics[0].message
        ));
    }
    if !loaded_skills.diagnostics.is_empty() {
        app.system_line(format!(
            "skills loader: {} diagnostic(s), first: {}",
            loaded_skills.diagnostics.len(),
            loaded_skills.diagnostics[0].message
        ));
    }
    if !hooks.runner.is_empty() {
        app.system_line(format!("hooks: loaded {} hook(s)", hooks.runner.len()));
    }
    for diag in &hooks.diagnostics {
        app.system_line(format!("hooks: {diag}"));
    }
    if let Some(ctx) = replay_context.as_ref() {
        app.replay(&ctx.messages);
    }

    // Stream agent + harness events into the feed. These listeners never touch stdout — they
    // only enqueue structured updates that the UI loop renders.
    let _unsub = harness
        .agent()
        .subscribe(ui::listener::agent_listener(feed_tx.clone()));
    let _unsub_harness_tui =
        harness.subscribe_harness(ui::listener::harness_listener(feed_tx.clone(), cli.debug));
    let _unsub_dynamic_fire_once = harness.subscribe_harness(triggers::fire_once_harness_listener(
        dynamic_trigger_registry.clone(),
    ));
    let _unsub_cron =
        harness.subscribe_harness(triggers::cron_harness_listener(cron_registry.clone()));
    let _unsub_hooks = harness.agent().subscribe(hooks.runner.listener());
    let _unsub_harness_hooks = harness.subscribe_harness(hooks.runner.harness_listener());

    // Inject-and-run delivery (`TriggerDelivery::InjectAndRun`): when a trigger injects a
    // prompt into the IDLE parent and asks for a model turn, the kernel cannot run the
    // single-tenant agent itself, so it emits `TriggerRequestsMainRun`. We funnel those into
    // one channel that the REPL loop drains on the SAME serialized path as user input — so a
    // triggered turn and a user prompt never race for the agent. The only sender lives in
    // this listener, so the channel stays open exactly as long as the subscription does.
    let _unsub_main_run = harness.subscribe_harness(std::sync::Arc::new(
        move |ev: pie_agent_core::HarnessEvent| {
            if let pie_agent_core::HarnessEvent::TriggerRequestsMainRun { trace_id } = ev {
                // Non-blocking on an unbounded channel; the UI loop drains it on the same
                // serialized run slot as user input. The message itself was already injected
                // by the kernel.
                let _ = main_run_tx.send(trace_id);
            }
        },
    ));

    // Hand off to the full-screen UI. It owns the terminal, the input box, the scrolling feed,
    // and the serialized run slot (user prompts + inject-and-run triggered turns) until quit.
    if run_web {
        app.run_web(ui::web::WebOptions {
            host: cli.web_host.clone(),
            port: cli.web_port,
        })
        .await
    } else {
        app.run().await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiMode {
    Web,
    Tui,
    Headless,
}

fn should_run_web(cli: &Cli) -> bool {
    resolve_ui_mode(
        cli.web,
        cli.tui,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        is_remote_tty_env(|name| std::env::var_os(name).is_some()),
    ) == UiMode::Web
}

fn resolve_ui_mode(web: bool, tui: bool, interactive_tty: bool, remote_tty: bool) -> UiMode {
    if web {
        return UiMode::Web;
    }
    if tui {
        return UiMode::Tui;
    }
    if !interactive_tty {
        return UiMode::Headless;
    }
    if remote_tty { UiMode::Tui } else { UiMode::Web }
}

fn is_remote_tty_env(mut has_env: impl FnMut(&str) -> bool) -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"]
        .into_iter()
        .any(&mut has_env)
}

/// Real `*Hook` trait registrations active in this binary. Only names that map to an actual
/// `AgentHarness` extension point — so users reading the panel learn what hooks they could
/// plug into. `dedup` / `cycle suppress` / `fire-once rules` / `inject-and-run` are
/// trigger-runtime *features*, not hooks, and live in [`active_trigger_features`] instead.
fn active_hook_registrations(lsp_lang_count: usize, cli_hooks_loaded: bool) -> Vec<String> {
    let mut points = vec![
        "before_tool_call".to_string(),
        "on_control_plane_prompt".to_string(),
        "before_trigger_action".to_string(),
    ];
    if lsp_lang_count > 0 {
        points.push("after_tool_call".to_string());
    }
    if cli_hooks_loaded {
        points.push("cli_hooks".to_string());
    }
    points
}

/// Trigger-runtime features always wired in the current binary. Distinct from hook
/// registrations — these are pipeline behaviors (dedup, cycle suppression, fire-once rules,
/// inject-and-run delivery), not pluggable callbacks.
fn active_trigger_features() -> Vec<String> {
    vec![
        "dedup".to_string(),
        "cycle suppress".to_string(),
        "fire-once rules".to_string(),
        "inject-and-run".to_string(),
    ]
}

pub(crate) async fn prompt_for_api_key(provider: &str) -> Result<String> {
    let provider = provider.to_string();
    tokio::task::spawn_blocking(move || {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(login_requires_tty_message(&provider, None));
        }
        rpassword::prompt_password(format!("api key for `{provider}`: "))
            .context("read api key without echo")
    })
    .await
    .context("login prompt task")?
}

pub(crate) fn login_requires_tty_message(provider: &str, recovery_command: Option<&str>) -> String {
    let command = recovery_command
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("/login {provider}"));
    format!(
        "/login requires an interactive terminal so the API key is not echoed; run pie in a TTY and use `{command}`"
    )
}

fn parse_thinking(s: &str) -> Result<ThinkingLevel> {
    s.parse().map_err(anyhow::Error::msg)
}

fn validate_base_url_override(cli: &Cli) -> Result<()> {
    let Some(_base_url) = cli
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(());
    };
    if cli
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .is_none()
    {
        anyhow::bail!(
            "--base-url requires an explicit --provider so credentials cannot be auto-detected for the wrong endpoint"
        );
    }
    Ok(())
}

fn compose_system_prompt(cwd: &std::path::Path, memory: &str, tool_names: &[String]) -> String {
    let mut s = String::new();
    s.push_str(&render_base_prompt(tool_names));
    s.push_str("\n\n");
    s.push_str(&format!("Current working directory: {}\n", cwd.display()));
    if !memory.is_empty() {
        s.push('\n');
        s.push_str(memory);
        s.push('\n');
    }
    s
}

/// Build the prompt header. The tool inventory is rendered from the actual registered tool
/// definitions so adding/removing a tool in `tools::default_tools()` flows through here without
/// a hand-edited literal list.
fn render_base_prompt(tool_names: &[String]) -> String {
    let inventory = if tool_names.is_empty() {
        "no tools registered".to_string()
    } else {
        tool_names.join(", ")
    };
    format!(
        "You are pie-coding-agent, a minimal coding assistant running in a terminal. \
You have access to the following tools: {inventory}. \
Prefer running a tool over guessing. When making file changes, read the file first to confirm the exact current contents, then edit or write. Keep responses concise. \
When the user asks for a fixed time, recurring, scheduled, hourly, daily, weekly, crontab, 定时任务, 每小时, or similar time-based job, call NewCronJob instead of NewTrigger. \
When the user asks to view, list, show, inspect, or find scheduled jobs or cron job ids, call ListCronJobs. \
When the user asks to pause or disable a scheduled job or cron job, call SetCronJobState with enabled=false; enabling/resuming should point the user to /cron enable <id> until confirmation support is wired. \
When the user asks to delete, remove, or clear scheduled jobs or cron jobs, call RemoveCronJob first with confirm=false to preview, then only call confirm=true after explicit user confirmation. \
When the user asks to create a trigger, reminder, watcher, or automation, call NewTrigger and extract a natural-language condition and action from their request. Dynamic triggers fire once by default; set fire_once=false only when the user explicitly asks for a repeating trigger. Trigger output is shown in the TUI and audit by default; set promote_to_chat=true only when the user explicitly asks for trigger results to enter the main chat context or be visible to future turns. \
When the user asks to view, list, show, inspect, or find trigger ids, call ListTriggers. \
When the user asks to pause, disable, enable, or resume a dynamic trigger, call SetTriggerState. \
When the user asks to delete, remove, or clear dynamic triggers, call RemoveTrigger. \
When the user asks to create, save, or codify a reusable skill, workflow, checklist, or convention, or to summarize recent work or this conversation into a skill (技能, 保存为技能, 把刚才的工作总结成 skill), call SkillBuilder with structured name/description/instructions. For summarize-into-skill requests, distill the generalizable steps from the conversation — what was actually done, the commands used, the pitfalls — not a transcript. Call once without confirm to preview and show the user the planned name and description, then call with confirm=true after they agree. Use InstallSkill only for installing an existing SKILL.md from a URL, file, or pasted content."
    )
}

fn stream_fn_with_auth_store() -> pie_agent_core::StreamFn {
    std::sync::Arc::new(|model, context, options| {
        let merged = apply_auth_to_simple_options(model, options, |provider| {
            crate::auth::AuthStore::load()
                .ok()
                .and_then(|store| store.resolve_for_provider(provider))
        });
        pie_ai::stream_simple(model, context, Some(&merged))
    })
}

fn apply_auth_to_simple_options<F>(
    model: &pie_ai::Model,
    options: Option<&pie_ai::SimpleStreamOptions>,
    resolve_api_key: F,
) -> pie_ai::SimpleStreamOptions
where
    F: FnOnce(&str) -> Option<String>,
{
    let mut merged = options.cloned().unwrap_or_default();
    let needs_api_key = merged
        .base
        .api_key
        .as_deref()
        .map(str::trim)
        .map(str::is_empty)
        .unwrap_or(true);
    if needs_api_key {
        if let Some(api_key) = resolve_api_key(&model.provider.0).filter(|k| !k.trim().is_empty()) {
            merged.base.api_key = Some(api_key);
        }
    }
    merged
}

/// Helper for callers that want to feed a Message (raw pie-ai role variant) into the agent. Not
/// directly used by the REPL but kept here for the tests.
pub fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(text.into()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }))
}

/// Read `<base_dir>/config.toml` and extract the `[builtin_skills] enabled = [...]` list.
/// Missing file → empty list. Parse error / missing section → empty list (the parser itself
/// returns empty per #32's soft fail-closed posture; see
/// [`builtin_skills::parse_builtin_skills_config`]).
async fn read_builtin_skills_config(base_dir: &std::path::Path) -> Vec<String> {
    let path = base_dir.join("config.toml");
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return Vec::new();
    };
    builtin_skills::parse_builtin_skills_config(&text)
}

/// Resolve the local dynamic trigger poll interval. CLI overrides config; config overrides
/// the built-in default. A malformed config reports a diagnostic but does not block startup.
async fn read_trigger_poll_interval_secs(
    base_dir: &std::path::Path,
    cli_override: Option<u64>,
) -> (u64, Option<String>) {
    if let Some(secs) = cli_override {
        return (secs, None);
    }

    let default = triggers::dynamic::DEFAULT_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS;
    let path = base_dir.join("config.toml");
    let Ok(text) = tokio::fs::read_to_string(&path).await else {
        return (default, None);
    };
    match config::parse_trigger_poll_interval_secs(&text) {
        Ok(Some(secs)) => (secs, None),
        Ok(None) => (default, None),
        Err(err) => (
            default,
            Some(format!(
                "triggers: ignoring invalid poll interval in {}: {err}",
                path.display()
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_flag_accepts_optional_session_id() {
        use clap::Parser;
        // Bare --resume keeps the picker behavior.
        let bare = Cli::try_parse_from(["pie", "--resume"]).expect("bare --resume parses");
        assert!(bare.resume.is_some());
        assert_eq!(bare.effective_resume_id(), None);

        // --resume <id> behaves like --resume-id <id>.
        let with_id =
            Cli::try_parse_from(["pie", "--resume", "019ea2fd"]).expect("--resume with id");
        assert_eq!(with_id.effective_resume_id(), Some("019ea2fd"));

        // --resume-id still works and wins when both are given.
        let both =
            Cli::try_parse_from(["pie", "--resume", "aaa", "--resume-id", "bbb"]).expect("both");
        assert_eq!(both.effective_resume_id(), Some("bbb"));

        // --resume followed by another flag must not swallow the flag as its value.
        let with_flag =
            Cli::try_parse_from(["pie", "--resume", "--web"]).expect("flag not swallowed");
        assert_eq!(with_flag.effective_resume_id(), None);
        assert!(with_flag.resume.is_some());
        assert!(with_flag.web);

        // Absent entirely.
        let none = Cli::try_parse_from(["pie"]).expect("no flags");
        assert!(none.resume.is_none());
        assert_eq!(none.effective_resume_id(), None);
    }

    fn model(provider: &str) -> pie_ai::Model {
        pie_ai::Model {
            id: "deepseek-v4-flash".into(),
            name: "DeepSeek V4 Flash".into(),
            api: pie_ai::Api::from("openai-responses"),
            provider: pie_ai::Provider::from(provider),
            base_url: "http://127.0.0.1:8000/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![pie_ai::InputModality::Text],
            cost: pie_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 384_000,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn auth_wrapper_injects_provider_scoped_stored_key() {
        let opts = apply_auth_to_simple_options(&model("ds4"), None, |provider| {
            assert_eq!(provider, "ds4");
            Some("stored-ds4-key".into())
        });
        assert_eq!(opts.base.api_key.as_deref(), Some("stored-ds4-key"));
    }

    #[test]
    fn auth_wrapper_keeps_explicit_api_key() {
        let mut existing = pie_ai::SimpleStreamOptions::default();
        existing.base.api_key = Some("explicit-key".into());
        let opts = apply_auth_to_simple_options(&model("ds4"), Some(&existing), |_| {
            Some("stored-ds4-key".into())
        });
        assert_eq!(opts.base.api_key.as_deref(), Some("explicit-key"));
    }

    #[test]
    fn auth_wrapper_fails_closed_without_provider_scoped_key() {
        let opts = apply_auth_to_simple_options(&model("ds4"), None, |_| None);
        assert_eq!(opts.base.api_key, None);
    }

    #[test]
    fn base_url_override_requires_explicit_provider() {
        let mut cli = Cli {
            command: None,
            provider: None,
            model: Some("deepseek-v4-flash".into()),
            base_url: Some("http://user:secret-token@127.0.0.1:8000/v1?token=secret".into()),
            thinking: "off".into(),
            resume: None,
            continue_: false,
            resume_id: None,
            list_sessions: false,
            list_all_sessions: false,
            delete_session: None,
            image: Vec::new(),
            builtin_skill: Vec::new(),
            trigger_poll_secs: None,
            debug: false,
            yes: false,
            always_allow: false,
            web: false,
            tui: false,
            web_host: "127.0.0.1".into(),
            web_port: 0,
        };
        let err = validate_base_url_override(&cli).unwrap_err().to_string();
        assert!(
            err.contains("--base-url requires an explicit --provider"),
            "{err}"
        );
        assert!(!err.contains("secret-token"), "{err}");
        assert!(!err.contains("127.0.0.1"), "{err}");
        assert!(!err.contains("token=secret"), "{err}");
        assert!(!err.contains("OPENAI_API_KEY"), "{err}");
        assert!(!err.contains("DS4_API_KEY"), "{err}");

        cli.provider = Some("ds4".into());
        validate_base_url_override(&cli).unwrap();
    }

    #[test]
    fn cli_parses_session_export_import_commands() {
        let cli = Cli::parse_from([
            "pie",
            "session",
            "export",
            "--session",
            "018f",
            "--output",
            "backup.piesession",
            "--exclude-triggers",
        ]);
        match cli.command {
            Some(CliCommand::Session {
                command:
                    SessionCliCommand::Export {
                        session,
                        output,
                        exclude_triggers,
                        ..
                    },
            }) => {
                assert_eq!(session.as_deref(), Some("018f"));
                assert_eq!(
                    output.unwrap(),
                    std::path::PathBuf::from("backup.piesession")
                );
                assert!(exclude_triggers);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "pie",
            "session",
            "import",
            "backup.piesession",
            "--activate-triggers",
            "off",
        ]);
        assert!(matches!(
            cli.command,
            Some(CliCommand::Session {
                command: SessionCliCommand::Import {
                    activate_triggers: ActivateTriggersArg::Off,
                    ..
                }
            })
        ));
    }

    #[tokio::test]
    async fn cli_session_import_ask_activation_is_explicitly_unsupported() {
        let temp = tempfile::tempdir().unwrap();
        let repo = JsonlSessionRepo::new(temp.path().join("sessions"));
        let command = SessionCliCommand::Import {
            file: std::path::PathBuf::from("missing.piesession"),
            cwd: None,
            activate_triggers: ActivateTriggersArg::Ask,
        };
        let err = run_session_cli_command(&command, &repo, temp.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("activate-triggers=ask"), "{err}");
        assert!(err.contains("not implemented"), "{err}");
    }

    #[test]
    fn ui_mode_defaults_to_web_for_local_tty() {
        assert_eq!(resolve_ui_mode(false, false, true, false), UiMode::Web);
    }

    #[test]
    fn ui_mode_defaults_to_tui_for_remote_tty() {
        assert_eq!(resolve_ui_mode(false, false, true, true), UiMode::Tui);
    }

    #[test]
    fn ui_mode_keeps_headless_for_non_tty() {
        assert_eq!(
            resolve_ui_mode(false, false, false, false),
            UiMode::Headless
        );
    }

    #[test]
    fn explicit_ui_flags_override_default() {
        assert_eq!(resolve_ui_mode(true, false, true, true), UiMode::Web);
        assert_eq!(resolve_ui_mode(false, true, true, false), UiMode::Tui);
    }

    #[test]
    fn remote_tty_env_detects_ssh_and_mosh() {
        assert!(is_remote_tty_env(|name| name == "SSH_CONNECTION"));
        assert!(is_remote_tty_env(|name| name == "MOSH_CONNECTION"));
        assert!(!is_remote_tty_env(|_| false));
    }

    #[tokio::test]
    async fn trigger_poll_interval_defaults_to_ten_minutes_and_allows_overrides() {
        let temp = tempfile::tempdir().unwrap();
        let base_dir = temp.path();

        let (default_secs, diagnostic) = read_trigger_poll_interval_secs(base_dir, None).await;
        assert_eq!(
            default_secs,
            crate::triggers::dynamic::DEFAULT_DYNAMIC_TRIGGER_POLL_INTERVAL_SECS
        );
        assert_eq!(default_secs, 600);
        assert!(diagnostic.is_none());

        tokio::fs::write(
            base_dir.join("config.toml"),
            "[triggers]\npoll_interval_secs = 60\n",
        )
        .await
        .unwrap();
        let (config_secs, diagnostic) = read_trigger_poll_interval_secs(base_dir, None).await;
        assert_eq!(config_secs, 60);
        assert!(diagnostic.is_none());

        let (cli_secs, diagnostic) = read_trigger_poll_interval_secs(base_dir, Some(15)).await;
        assert_eq!(cli_secs, 15);
        assert!(diagnostic.is_none());
    }
}
