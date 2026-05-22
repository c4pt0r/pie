//! pie-coding-agent — minimal coding agent CLI on top of pie-agent-core.
//!
//! Modeled on `packages/coding-agent/` (the TS implementation) in spirit: same tools
//! (`read`/`write`/`edit`/`bash`/`ls`/`grep`/`find` + `memory`), same `--resume` semantics
//! scoped by cwd hash, same "interactive TUI" mode, dual-root skills loader (project ↻ user).
//! Trimmed scope: no extensions, no themes, no print/rpc/json modes.

mod agent_session;
mod auth;
mod bug_report;
mod commands;
mod config;
mod export;
mod extensions;
mod history;
mod hooks;
mod images;
mod logging;
mod lsp;
mod lsp_supervisor;
mod markdown;
mod mcp_loader;
mod mentions;
mod model;
mod oauth;
mod otlp;
mod session;
mod skills;
mod spinner;
mod templates;
mod tools;
mod tui;

use std::io::Write as _;
use std::time::{Duration, Instant};

/// Result of one rustyline readline call. Mapped from rustyline errors so the async REPL
/// can dispatch on three clean cases.
enum ReadlineOutcome {
    Line(String),
    CtrlC,
    Eof,
}

use anyhow::{Context, Result};
use clap::Parser;
use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentMessage, JsonlSessionRepo, PermissionPolicy,
    SessionContext, ThinkingLevel,
};
use pie_ai::Message as PiMessage;

#[derive(Parser, Debug)]
#[command(
    name = "pie",
    version,
    about = "Simple coding agent on top of pie-agent-core"
)]
struct Cli {
    /// Provider id (anthropic, openai, openrouter, …). When unset, auto-detected from env.
    #[arg(long)]
    provider: Option<String>,
    /// Model id within the provider's catalog.
    #[arg(long)]
    model: Option<String>,
    /// Thinking level (off | minimal | low | medium | high | xhigh).
    #[arg(long, default_value = "off")]
    thinking: String,

    /// Resume the most recent session for this cwd (or pass --resume-id for a specific one).
    #[arg(long)]
    resume: bool,
    /// Continue the most recent session for this cwd. Alias for --resume; the conventional
    /// short flag people reach for.
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().context("getting cwd")?;
    let repo = session::open_repo(&cwd).await;

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

async fn list_sessions_cmd(repo: &JsonlSessionRepo) -> Result<()> {
    let entries = session::list_entries(repo).await?;
    if entries.is_empty() {
        println!("(no sessions for this cwd)");
        return Ok(());
    }
    println!("sessions in {}:", repo.root().display());
    for e in entries {
        let preview = e.preview.as_deref().unwrap_or("");
        println!(
            "  {}  {}  {}",
            &e.id[..16.min(e.id.len())],
            e.created_at,
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
        println!("  {bucket_name}/{id_short}  {}  {preview}", e.created_at);
    }
    Ok(())
}

async fn delete_session_cmd(repo: &JsonlSessionRepo, id: &str) -> Result<()> {
    let path = session::delete_by_id(repo, id).await?;
    println!("deleted {}", path.display());
    Ok(())
}

async fn run_repl(mut cli: Cli, cwd: std::path::PathBuf, repo: JsonlSessionRepo) -> Result<()> {
    let model = model::auto_detect_model(cli.provider.as_deref(), cli.model.as_deref())?;
    let thinking = parse_thinking(&cli.thinking)?;

    // Resolve / create the session. `--continue` is just `--resume` without an id.
    let should_resume = cli.resume || cli.continue_ || cli.resume_id.is_some();
    let (session, resumed) = if should_resume {
        let s = session::resume(&repo, cli.resume_id.as_deref()).await?;
        (s, true)
    } else {
        let s = session::create(&repo, &cwd).await?;
        (s, false)
    };
    let session_id = session
        .storage()
        .get_metadata_json()
        .await?
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();

    // Install the tracing subscriber. Failure is non-fatal — we keep running without logs.
    let logging = logging::init(&session_id);

    // Build the harness.
    let memory_dir = config::memory_dir();
    let mut tools = tools::default_tools(memory_dir.clone());
    // Task delegation tool (issue #11). Shares the parent's model + stream backend so its
    // subagents go through the same provider.
    tools.push(tools::task_tool(model.clone(), None));
    // Skill tool (issue #25). Needs to reach the live `AgentHarness::skills()` snapshot, but
    // the harness does not exist yet — we are still assembling the tool list that will be
    // passed to `AgentHarness::new`. Use a `OnceCell` that we'll fill immediately after the
    // harness is constructed, before the REPL accepts any input.
    let skill_harness_cell: tools::skill::SkillHarnessCell =
        std::sync::Arc::new(once_cell::sync::OnceCell::new());
    tools.push(tools::skill_tool(skill_harness_cell.clone()));

    // MCP (issue #9): spawn every server configured under ~/.pie/mcp.toml or
    // <cwd>/.pie/mcp.toml, append their tools to the registry.
    let mcp = mcp_loader::load_all(&cwd).await;
    let mcp_tool_count = mcp.tools.len();
    tools.extend(mcp.tools);
    let tool_names = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect::<Vec<_>>();
    let memory_block = tools::memory::load_memory_block(&memory_dir).await;
    let system_prompt = compose_system_prompt(&cwd, &memory_block, &tool_names);

    let loaded_skills = skills::load_all(&cwd).await;
    let loaded_templates = templates::load_all(&cwd).await;

    let mut opts = AgentHarnessOptions::new(model.clone(), session.clone());
    opts.system_prompt = system_prompt;
    opts.thinking_level = thinking;
    opts.tools = tools;
    opts.skills = loaded_skills.skills.clone();
    opts.prompt_templates = loaded_templates.templates.clone();
    opts.before_tool_call =
        Some(PermissionPolicy::default_for_coding_agent().as_before_tool_call());
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
    assert!(
        skill_harness_cell.set(harness.clone()).is_ok(),
        "Skill tool harness cell was set twice; main.rs wiring is the only setter"
    );
    let session_runner =
        agent_session::AgentSession::new(harness.clone(), agent_session::RetrySettings::default());

    // Banner + replay (if --resume). All resume hydration lives on AgentHarness, so the CLI
    // just asks for the rebuilt SessionContext and renders it.
    let tui = tui::Tui::new();
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
    tui.banner(&display_model, &session_id, resumed, &tool_names);
    if !loaded_skills.skills.is_empty() {
        tui.system_line(&format!(
            "loaded {} skill(s): {}",
            loaded_skills.skills.len(),
            loaded_skills
                .skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !loaded_templates.templates.is_empty() {
        tui.system_line(&format!(
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
        tui.system_line(&format!(
            "mcp: connected to {} server(s), {mcp_tool_count} extra tool(s)",
            mcp.client_count,
        ));
    }
    if lsp_lang_count > 0 {
        tui.system_line(&format!(
            "lsp: {lsp_lang_count} language(s) configured; diagnostics attach to edit/write results"
        ));
    }
    for diag in &mcp.diagnostics {
        tui.error_line(&format!("mcp: {diag}"));
    }
    if !loaded_templates.diagnostics.is_empty() {
        tui.system_line(&format!(
            "templates loader: {} diagnostic(s), first: {}",
            loaded_templates.diagnostics.len(),
            loaded_templates.diagnostics[0].message
        ));
    }
    if !loaded_skills.diagnostics.is_empty() {
        tui.system_line(&format!(
            "skills loader: {} diagnostic(s), first: {}",
            loaded_skills.diagnostics.len(),
            loaded_skills.diagnostics[0].message
        ));
    }
    let (hook_model, hook_thinking) = {
        let state = harness.agent().state();
        (state.model.clone(), state.thinking_level)
    };
    let hooks = hooks::load(&cwd, session_id.clone(), hook_model.as_ref(), hook_thinking).await;
    if !hooks.runner.is_empty() {
        tui.system_line(&format!("hooks: loaded {} hook(s)", hooks.runner.len()));
    }
    for diag in &hooks.diagnostics {
        tui.system_line(&format!("hooks: {diag}"));
    }
    if let Some(ctx) = replay_context.as_ref() {
        replay_transcript(ctx, &tui);
    }

    // Wire the TUI listener so each prompt's events stream live.
    let _unsub = harness.agent().subscribe(tui.listener());
    let _unsub_hooks = harness.agent().subscribe(hooks.runner.listener());
    let _unsub_harness_hooks = harness.subscribe_harness(hooks.runner.harness_listener());

    let registry = commands::Registry::with_builtins();

    // Persistent input history (issue #2). Loaded once at startup; each successful prompt
    // submission appends + persists.
    let mut history = history::HistoryStore::load();

    // Use rustyline for the readline phase — gives us proper unicode-width-aware editing
    // (CJK chars correctly consume one logical character per backspace), ↑/↓ recall,
    // Ctrl-R reverse search, and emacs keybinds. The line read is blocking so we run it
    // on a tokio blocking-thread.
    let history_path = history::HistoryStore::default_path();
    let mut last_idle_ctrlc: Option<Instant> = None;

    loop {
        // Each iteration spawns a fresh editor. Cheap and avoids cross-iteration borrow
        // issues with the blocking task. The editor loads existing history on construction.
        let prompt_marker = "you> ".to_string();
        let history_path_clone = history_path.clone();
        let read =
            tokio::task::spawn_blocking(move || -> Result<ReadlineOutcome, anyhow::Error> {
                let mut editor = rustyline::DefaultEditor::new()?;
                if history_path_clone.exists() {
                    let _ = editor.load_history(&history_path_clone);
                }
                match editor.readline(&format!("\x1b[36m{prompt_marker}\x1b[0m")) {
                    Ok(line) => Ok(ReadlineOutcome::Line(line)),
                    Err(rustyline::error::ReadlineError::Interrupted) => Ok(ReadlineOutcome::CtrlC),
                    Err(rustyline::error::ReadlineError::Eof) => Ok(ReadlineOutcome::Eof),
                    Err(e) => Err(e.into()),
                }
            })
            .await;

        let outcome = match read {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                tui.error_line(&format!("readline: {e}"));
                break;
            }
            Err(e) => {
                tui.error_line(&format!("readline task: {e}"));
                break;
            }
        };

        let line = match outcome {
            ReadlineOutcome::Line(l) => l,
            ReadlineOutcome::Eof => {
                tui.system_line("eof — exiting");
                break;
            }
            ReadlineOutcome::CtrlC => {
                let now = Instant::now();
                if last_idle_ctrlc
                    .map(|t| now.duration_since(t) < Duration::from_millis(1500))
                    .unwrap_or(false)
                {
                    tui.system_line("bye");
                    break;
                }
                last_idle_ctrlc = Some(now);
                tui.system_line("press Ctrl-C again within 1.5s to exit, or type /quit");
                continue;
            }
        };

        // Successful input clears the "second-Ctrl-C-to-exit" arming.
        last_idle_ctrlc = None;

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // Slash commands flow through the registry; the special outcomes (Quit / ClearScreen)
        // affect REPL state, so we handle them here. Everything else falls through to a
        // prompt.
        if input.starts_with('/') {
            let ctx = commands::CommandCtx {
                harness: &harness,
                session_id: &session_id,
                log_path: logging.as_ref().map(|l| &l.log_path),
                tool_count: tool_names.len(),
                cwd: &cwd,
            };
            match commands::dispatch(input, &registry, &ctx).await {
                commands::CommandOutcome::Quit => {
                    tui.system_line("bye");
                    break;
                }
                commands::CommandOutcome::ClearScreen => {
                    print!("\x1b[2J\x1b[H");
                    let _ = std::io::stdout().flush();
                }
                commands::CommandOutcome::Error(e) => {
                    tui.error_line(&e);
                }
                commands::CommandOutcome::Handled => {}
            }
            continue;
        }

        // Expand `@file` mentions before sending. The original `@path` token stays in the
        // user's text; the file content is prepended in a small attachment block.
        let (expanded, _resolved) = mentions::expand(input, &cwd).await;

        // Attach `--image` payloads to the first prompt only (issue #16 first slice).
        // Subsequent prompts in the same session can mention files via @path or re-launch
        // the binary with --image again.
        let pending_images = if !cli.image.is_empty() {
            match images::load_all(&cli.image).await {
                Ok(imgs) => imgs,
                Err(e) => {
                    tui.error_line(&format!("--image: {e}"));
                    cli.image.clear();
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let has_images = !pending_images.is_empty();
        if has_images {
            cli.image.clear();
        }

        // Run the prompt while watching for Ctrl-C. On signal, ask the harness to abort and
        // keep awaiting the future so it cleans up; the result tells us whether it aborted.
        let aborted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let aborted_for_signal = aborted.clone();
        let harness_for_signal = harness.clone();
        // Append to persistent history before sending. We store the raw user input (without
        // @file expansion) so recall surfaces what the user actually typed.
        history.append(input);

        // Active-prompt spinner. Starts BEFORE the prompt future, gets cancelled on the
        // first event that means "the LLM is producing user-visible output" — final text
        // and tool executions. Keep it alive while reasoning/thinking deltas stream.
        let spin = spinner::start("thinking");
        let spin_for_listener = spin.clone();
        let _unsub_spin = harness.agent().subscribe(std::sync::Arc::new(move |ev, _| {
            let s = spin_for_listener.clone();
            Box::pin(async move {
                if should_stop_spinner_on(&ev) {
                    s.stop_sync();
                }
            })
        }));

        // First-time image attachment goes through harness.prompt_with_images directly; the
        // session_runner retry/rewind path doesn't need to participate for a one-shot
        // describe-this-image flow.
        let prompt_fut = async {
            if has_images {
                harness.prompt_with_images(expanded, pending_images).await
            } else {
                session_runner.prompt(expanded).await
            }
        };
        tokio::pin!(prompt_fut);
        let signal_fut = async move {
            // First Ctrl-C while a prompt is in flight: abort.
            let _ = tokio::signal::ctrl_c().await;
            harness_for_signal.abort();
            aborted_for_signal.store(true, std::sync::atomic::Ordering::SeqCst);
        };
        tokio::pin!(signal_fut);

        // `biased` keeps the prompt future polled first so a fast completion doesn't get
        // pre-empted by a stale signal future.
        let res = loop {
            tokio::select! {
                biased;
                res = &mut prompt_fut => break res,
                _ = &mut signal_fut, if !aborted.load(std::sync::atomic::Ordering::SeqCst) => {
                    // The signal future will not re-arm after firing; subsequent ctrl_c during
                    // the same turn falls through to default tokio handling. Loop back to
                    // continue awaiting the prompt future for clean unwind.
                }
            }
        };

        // Idempotent: if the listener already fired, this is a no-op. If the prompt
        // errored or aborted before any agent event, this clears the spinner here.
        spin.stop_sync();

        if aborted.load(std::sync::atomic::Ordering::SeqCst) {
            tui.system_line("[aborted]");
        } else if let Err(e) = res {
            tui.error_line(&format!("{e}"));
        }
    }
    Ok(())
}

/// Predicate for spinner cancellation. The spinner should remain visible during the gap
/// between user submission and user-visible output. Thinking deltas are still the model
/// working, so the spinner stays animated until text/tool output starts. AgentStart /
/// MessageStart fire too early to be useful here.
fn should_stop_spinner_on(ev: &pie_agent_core::AgentEvent) -> bool {
    use pie_agent_core::AgentEvent;
    use pie_ai::AssistantMessageEvent;
    match ev {
        AgentEvent::ToolExecutionStart { .. } | AgentEvent::ToolExecutionEnd { .. } => true,
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => matches!(
            assistant_message_event,
            AssistantMessageEvent::TextDelta { .. } | AssistantMessageEvent::ToolCallDelta { .. }
        ),
        _ => false,
    }
}

fn parse_thinking(s: &str) -> Result<ThinkingLevel> {
    s.parse().map_err(anyhow::Error::msg)
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
Prefer running a tool over guessing. When making file changes, read the file first to confirm the exact current contents, then edit or write. Keep responses concise."
    )
}

fn replay_transcript(ctx: &SessionContext, tui: &tui::Tui) {
    if ctx.messages.is_empty() {
        return;
    }
    tui.system_line(&format!(
        "resumed — replaying {} messages",
        ctx.messages.len()
    ));
    for m in &ctx.messages {
        tui::render_persisted(m);
    }
    // Skip custom variants (compaction_summary etc.); they aren't model-visible here. But the
    // harness uses them via convert_to_llm filtering — that's already handled by pie-agent-core.
    drop_unused(&ctx.messages);
}

fn drop_unused(_: &[AgentMessage]) {}

/// Helper for callers that want to feed a Message (raw pie-ai role variant) into the agent. Not
/// directly used by the REPL but kept here for the tests.
pub fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(text.into()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }))
}
