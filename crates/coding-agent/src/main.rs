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
mod commands;
mod config;
mod export;
mod extensions;
mod history;
mod hooks;
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
mod session;
mod skills;
mod templates;
mod tools;
mod triggers;
mod ui;

use std::io::IsTerminal as _;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
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
    /// Provider id (anthropic, openai, openrouter, …). When unset, auto-detected from env.
    #[arg(long)]
    provider: Option<String>,
    /// Model id within the provider's catalog.
    #[arg(long)]
    model: Option<String>,
    /// Thinking level (off | minimal | low | medium | high | xhigh).
    #[arg(
        long,
        default_value = "off",
        value_parser = clap::builder::PossibleValuesParser::new(commands::THINKING_LEVEL_VALUES)
    )]
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

    /// Enable a built-in skill bundled with this `pie` binary, by name. Repeatable. Unknown
    /// names hard-fail with a list of available built-ins. Built-in skills are the lowest
    /// precedence — user (`~/.pie/skills/`) and project (`<cwd>/.pie/skills/`) skills of the
    /// same name still override. Persistent enable is via `~/.pie/config.toml`
    /// `[builtin_skills] enabled = [...]`; CLI + config are unioned and de-duplicated.
    #[arg(long = "builtin-skill", value_name = "NAME")]
    builtin_skill: Vec<String>,

    /// Poll interval for local dynamic trigger checks, in seconds. Defaults to
    /// `[triggers] poll_interval_secs` from `~/.pie/config.toml`, or 60 when unset.
    #[arg(long = "trigger-poll-secs", value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    trigger_poll_secs: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    print_dynamic_help_and_exit_if_requested()?;
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

fn print_dynamic_help_and_exit_if_requested() -> Result<()> {
    if !std::env::args_os()
        .skip(1)
        .any(|arg| arg == "--help" || arg == "-h")
    {
        return Ok(());
    }
    let mut command = Cli::command().after_help(commands::cli_model_help_text());
    command.print_help()?;
    println!();
    std::process::exit(0);
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
    let local_models = local_models::load_all(&cwd).await?;
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
    let session_metadata = session.storage().get_metadata_json().await?;
    let session_id = session_metadata
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let dynamic_trigger_path = session::trigger_sidecar_path_for_session(&session, &repo).await?;

    // Install the tracing subscriber. Failure is non-fatal — we keep running without logs.
    let logging = logging::init(&session_id);

    // Build the harness.
    let stream_fn = stream_fn_with_auth_store();
    let dynamic_trigger_registry = triggers::global_registry().clone();
    let dynamic_trigger_load_error = dynamic_trigger_registry
        .load_from_path(dynamic_trigger_path)
        .err();
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
    tools.push(tools::new_trigger_tool());
    tools.push(tools::list_triggers_tool());
    tools.push(tools::remove_trigger_tool());
    tools.push(tools::set_trigger_state_tool());

    // MCP (issue #9): spawn every server configured under ~/.pie/mcp.toml or
    // <cwd>/.pie/mcp.toml, append their tools to the registry. MCP push adapters are
    // registered as trigger sources a few lines below, once we have an `Arc<AgentHarness>`.
    let mcp = mcp_loader::load_all(&cwd).await;
    let mcp_tool_count = mcp.tools.len();
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
    let combined_skills = builtin_skills::merge_with_user_project(
        resolved_builtins.skills.clone(),
        &loaded_skills.skills,
    );

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
                let merged = builtin_skills::merge_with_user_project(builtins, &loaded.skills);
                pie_agent_core::LoadSkillsOutput {
                    skills: merged,
                    diagnostics: loaded.diagnostics,
                }
            })
        })
    });
    opts.before_tool_call =
        Some(PermissionPolicy::default_for_coding_agent().as_before_tool_call());
    // Triggers from MCP servers configured with `inject_summary` / `inject_and_run` bypass
    // the sub-agent and inject their pushed summary into chat (the latter also runs one
    // model turn in the parent context); everything else falls through to the dynamic-rule
    // hook. The match is structural (server name), no model.
    opts.before_trigger_action = Some(triggers::direct_inject_action_hook(
        mcp_inject_summary_servers,
        mcp_inject_and_run_servers,
        triggers::before_trigger_action_hook(dynamic_trigger_registry.clone()),
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

    // Wire each MCP server's trigger-source adapter into the harness now that all
    // tool-initialized state (including the Skill cell above) is in place.
    // `register_notification_hook` spawns a driver task that runs `hook.run(sink)` and a
    // pump task that drains the sink into `handle_trigger`; both tear down naturally when
    // the MCP transport closes or the harness drops.
    for hook in mcp_notification_hooks {
        harness.register_notification_hook(hook);
    }
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
    let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel::<ui::FeedUpdate>();
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
        panel_status: ui::PanelStatus {
            mcp_servers: mcp.client_count,
            mcp_tools: mcp_tool_count,
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
        harness.subscribe_harness(ui::listener::harness_listener(feed_tx.clone()));
    let _unsub_dynamic_fire_once = harness.subscribe_harness(triggers::fire_once_harness_listener(
        dynamic_trigger_registry.clone(),
    ));
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
    app.run().await
}

/// Real `*Hook` trait registrations active in this binary. Only names that map to an actual
/// `AgentHarness` extension point — so users reading the panel learn what hooks they could
/// plug into. `dedup` / `cycle suppress` / `fire-once rules` / `inject-and-run` are
/// trigger-runtime *features*, not hooks, and live in [`active_trigger_features`] instead.
fn active_hook_registrations(lsp_lang_count: usize, cli_hooks_loaded: bool) -> Vec<String> {
    let mut points = vec![
        "before_tool_call".to_string(),
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
            anyhow::bail!(login_requires_tty_message(&provider));
        }
        rpassword::prompt_password(format!("api key for `{provider}`: "))
            .context("read api key without echo")
    })
    .await
    .context("login prompt task")?
}

pub(crate) fn login_requires_tty_message(provider: &str) -> String {
    format!(
        "/login requires an interactive terminal so the API key is not echoed; run pie in a TTY and use `/login {provider}`"
    )
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
Prefer running a tool over guessing. When making file changes, read the file first to confirm the exact current contents, then edit or write. Keep responses concise. \
When the user asks to create a trigger, reminder, watcher, or automation, call NewTrigger and extract a natural-language condition and action from their request. Dynamic triggers fire once by default; set fire_once=false only when the user explicitly asks for a repeating trigger. Trigger output is shown in the TUI and audit by default; set promote_to_chat=true only when the user explicitly asks for trigger results to enter the main chat context or be visible to future turns. \
When the user asks to view, list, show, inspect, or find trigger ids, call ListTriggers. \
When the user asks to pause, disable, enable, or resume a dynamic trigger, call SetTriggerState. \
When the user asks to delete, remove, or clear dynamic triggers, call RemoveTrigger."
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
}
