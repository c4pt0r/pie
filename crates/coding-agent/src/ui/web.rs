//! Local browser UI for the coding-agent REPL.
//!
//! This is intentionally a small loopback-only surface. The browser layer sends commands into the
//! same single-turn event loop used by the TUI and receives full feed snapshots over SSE.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};

use super::kernel::{QueuedTurn, TurnState, poll_turn};
use super::{App, CommandCtx, CommandOutcome, feed, mentions};
use crate::readline::SlashCompleter;
use pie_agent_core::SkillSource;

#[derive(Clone, Debug)]
pub struct WebOptions {
    pub host: String,
    pub port: u16,
}

#[derive(Clone)]
struct HttpState {
    commands: mpsc::UnboundedSender<WebCommand>,
    snapshots: broadcast::Sender<WebSnapshot>,
    latest: Arc<Mutex<WebSnapshot>>,
    completer: SlashCompleter,
}

#[derive(Debug)]
enum WebCommand {
    Submit { text: String },
    Abort,
    ResolveControlPlane { approve: bool },
}

#[derive(Clone, Debug, Serialize)]
struct WebSnapshot {
    session_id: String,
    model: String,
    cwd: String,
    busy: bool,
    queued_count: usize,
    latest_trigger_poll: Option<super::feed::TriggerPollStatus>,
    goal: Option<WebGoalSnapshot>,
    control_plane_prompt: Option<WebControlPlanePromptSnapshot>,
    sidebar: WebSidebarSnapshot,
    feed_blocks: Vec<feed::WebFeedBlock>,
    feed_lines: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebGoalSnapshot {
    condition: String,
    status: String,
    iterations: u32,
    last_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebControlPlanePromptSnapshot {
    tool_name: String,
    label: String,
    reason: String,
    args_hash: String,
    payload: String,
}

#[derive(Clone, Debug, Serialize)]
struct WebSidebarSnapshot {
    skills: WebSkillsSnapshot,
    triggers: WebTriggersSnapshot,
    cron: WebCronSnapshot,
    mcp: WebMcpSnapshot,
    tools: WebToolsSnapshot,
    hooks: Vec<String>,
    runtime: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebSkillsSnapshot {
    total: usize,
    enabled: usize,
    disabled: usize,
    builtin: usize,
    user: usize,
    project: usize,
    items: Vec<WebSkillSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct WebSkillSnapshot {
    name: String,
    source: String,
    file_path: String,
    enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
struct WebTriggersSnapshot {
    total: usize,
    enabled: usize,
    disabled: usize,
    rules: Vec<WebTriggerRuleSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct WebTriggerRuleSnapshot {
    id: String,
    enabled: bool,
    mode: String,
    condition: String,
    action: String,
}

#[derive(Clone, Debug, Serialize)]
struct WebCronSnapshot {
    total: usize,
    enabled: usize,
    disabled: usize,
    jobs: Vec<WebCronJobSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct WebCronJobSnapshot {
    id: String,
    enabled: bool,
    schedule: String,
    action: String,
    skipped_overlap_count: u64,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebMcpSnapshot {
    servers: usize,
    tools: usize,
    notification_hooks: usize,
    server_names: Vec<String>,
    tool_names: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebToolsSnapshot {
    total: usize,
    names: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PromptRequest {
    text: String,
}

#[derive(Debug, Deserialize)]
struct CompleteRequest {
    text: String,
}

#[derive(Debug, Serialize)]
struct CompleteResponse {
    completions: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CommandAccepted {
    accepted: bool,
}

#[derive(Debug, Deserialize)]
struct ControlPlaneDecisionRequest {
    approve: bool,
}

impl App {
    pub async fn run_web(mut self, options: WebOptions) -> Result<()> {
        let addr = bind_addr(&options)?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind web ui on {addr}"))?;
        let actual = listener.local_addr()?;

        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<WebCommand>();
        let (snapshot_tx, _) = broadcast::channel::<WebSnapshot>(128);
        let latest = Arc::new(Mutex::new(self.web_snapshot()));
        let router = web_router(HttpState {
            commands: command_tx,
            snapshots: snapshot_tx.clone(),
            latest: latest.clone(),
            completer: self.completer.clone(),
        });

        let server = axum::serve(listener, router.into_make_service());
        let mut server_task = tokio::spawn(async move { server.await });
        let url = format!("http://{actual}");
        println!("pie web listening on {url}");
        if let Err(e) = open_web_browser(&url) {
            eprintln!("web browser auto-open skipped: {e}");
        }

        let mut feed_rx = self.feed_rx.take().expect("feed_rx taken once");
        let mut main_run_rx = self.main_run_rx.take().expect("main_run_rx taken once");
        let mut control_plane_prompt_rx = self.control_plane_prompt_rx.take();
        let mut turn = TurnState::default();
        self.refresh_goal_state().await;
        self.publish_snapshot(&latest, &snapshot_tx).await;

        loop {
            tokio::select! {
                biased;
                result = poll_turn(&mut turn.fut), if turn.fut.is_some() => {
                    self.finish_turn(&mut turn, result).await;
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(command) = command_rx.recv() => {
                    self.handle_web_command(command, &mut turn).await;
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(update) = feed_rx.recv() => {
                    self.apply_feed_update(update);
                    while let Ok(update) = feed_rx.try_recv() {
                        self.apply_feed_update(update);
                    }
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(trace_id) = main_run_rx.recv(), if turn.fut.is_none() => {
                    self.start_triggered_turn(trace_id, &mut turn);
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(prompt) = async {
                    match control_plane_prompt_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if self.control_plane_prompt.is_none() && control_plane_prompt_rx.is_some() => {
                    self.show_control_plane_prompt(prompt);
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    if turn.fut.is_some() {
                        self.request_abort(&mut turn);
                        self.publish_snapshot(&latest, &snapshot_tx).await;
                    }
                    break;
                }
                server_result = &mut server_task => {
                    match server_result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => self.error_line(format!("web server: {e}")),
                        Err(e) => self.error_line(format!("web server task: {e}")),
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_web_command(&mut self, command: WebCommand, turn: &mut TurnState) {
        match command {
            WebCommand::Submit { text } => self.submit_web_text(text, turn).await,
            WebCommand::Abort => self.request_abort(turn),
            WebCommand::ResolveControlPlane { approve } => {
                let decision = if approve {
                    pie_agent_core::ControlPlanePromptDecision::Allow
                } else {
                    pie_agent_core::ControlPlanePromptDecision::Deny {
                        reason: Some("denied by user".into()),
                    }
                };
                self.resolve_control_plane_prompt(decision);
            }
        }
    }

    async fn submit_web_text(&mut self, text: String, turn: &mut TurnState) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.history.append(&trimmed);
        self.follow = true;

        if trimmed.starts_with('/') {
            self.feed.push_user(&trimmed);
            self.dispatch_web_slash(&trimmed, turn).await;
            return;
        }

        let expanded = mentions::expand(&trimmed, &self.cwd).await.0;
        let prompt_text =
            crate::commands::attach_skill_prompt(expanded, self.pending_skill.take().as_deref());
        let display = trimmed;
        if turn.fut.is_some() {
            self.queue_user_prompt(display, prompt_text, Vec::new());
        } else {
            self.feed.push_user(display);
            self.start_user_prompt_turn(prompt_text, Vec::new(), turn);
        }
    }

    async fn dispatch_web_slash(&mut self, input: &str, turn: &mut TurnState) {
        let outcome = {
            let ctx = CommandCtx {
                harness: self.kernel.harness(),
                session_id: &self.session_id,
                log_path: self.log_path.as_ref(),
                tool_count: self.tool_count,
                cwd: &self.cwd,
            };
            crate::commands::dispatch(input, &self.registry, &ctx).await
        };
        match outcome {
            CommandOutcome::Quit => {
                self.system_line("web ui stays running; close the browser tab or press Ctrl-C in the terminal to stop the server");
            }
            CommandOutcome::ClearScreen => {
                self.feed.clear();
                self.follow = true;
            }
            CommandOutcome::Error(e) => self.error_line(e),
            CommandOutcome::AttachSkill { name } => {
                self.pending_skill = Some(name);
            }
            CommandOutcome::RunAgentPrompt {
                prompt,
                error_context,
            } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::AgentPrompt {
                        display: input.to_string(),
                        prompt,
                        error_context,
                    });
                } else {
                    self.start_prompt_turn(prompt, error_context, turn);
                }
            }
            CommandOutcome::RunPromptTemplate { name, vars } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::PromptTemplate {
                        display: input.to_string(),
                        name,
                        vars,
                    });
                } else {
                    self.start_template_turn(name, vars, turn);
                }
            }
            CommandOutcome::RunCompaction { custom } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::Compaction {
                        display: input.to_string(),
                        custom,
                    });
                } else {
                    self.start_compaction_turn(custom, turn);
                }
            }
            CommandOutcome::LoginSecret {
                provider,
                recovery_command,
                ..
            } => {
                let command = recovery_command.unwrap_or_else(|| format!("/login {provider}"));
                self.error_line(format!(
                    "web login is not implemented yet; run `{command}` from the terminal UI"
                ));
            }
            CommandOutcome::BackgroundTask { task, .. } => {
                tokio::spawn(task);
            }
            CommandOutcome::HubJoinManual { login_url, .. } => {
                self.error_line(format!(
                    "this session can't auto-open a browser; open {login_url} to sign in, then run /hub join from a terminal UI to paste the code"
                ));
            }
            CommandOutcome::Handled => {}
        }
        if input.trim_start().starts_with("/goal") {
            self.refresh_goal_state().await;
        }
    }

    fn web_snapshot(&self) -> WebSnapshot {
        let model = {
            let state = self.kernel.harness().agent().state();
            state
                .model
                .as_ref()
                .map(|m| format!("{}:{}", m.provider.0, m.id))
                .unwrap_or_else(|| "no-model".to_string())
        };
        WebSnapshot {
            session_id: self.session_id.clone(),
            model,
            cwd: self.cwd.display().to_string(),
            busy: self.busy,
            queued_count: self.queued_turns.len(),
            latest_trigger_poll: self.latest_trigger_poll.clone(),
            goal: self.latest_goal.as_ref().map(|goal| WebGoalSnapshot {
                condition: crate::bug_report::redact(&goal.condition),
                status: goal.status.as_str().to_string(),
                iterations: goal.iterations,
                last_reason: goal.last_reason.as_deref().map(crate::bug_report::redact),
            }),
            control_plane_prompt: self
                .control_plane_prompt
                .as_ref()
                .map(|prompt| web_control_plane_prompt_snapshot(&prompt.request)),
            sidebar: self.web_sidebar_snapshot(),
            feed_blocks: self.feed.web_blocks(),
            feed_lines: web_feed_lines(&self.feed),
        }
    }

    fn web_sidebar_snapshot(&self) -> WebSidebarSnapshot {
        const ITEM_LIMIT: usize = 8;

        let skills = self.kernel.harness().skills();
        let disabled = skills
            .iter()
            .filter(|skill| skill.disable_model_invocation)
            .count();
        let enabled = skills.len().saturating_sub(disabled);
        let source_count = |source| skills.iter().filter(|skill| skill.source == source).count();

        let rules = crate::triggers::global_registry().list();
        let trigger_enabled = rules.iter().filter(|rule| rule.enabled).count();
        let trigger_rules = rules
            .iter()
            .take(ITEM_LIMIT)
            .map(|rule| WebTriggerRuleSnapshot {
                id: feed::truncate_chars(&rule.id, 18),
                enabled: rule.enabled,
                mode: if rule.fire_once { "once" } else { "repeat" }.to_string(),
                condition: web_preview(&rule.condition),
                action: web_preview(&rule.action),
            })
            .collect::<Vec<_>>();

        let cron_jobs = crate::triggers::global_cron_registry().list();
        let cron_enabled = cron_jobs.iter().filter(|job| job.enabled).count();
        let cron_job_rows = cron_jobs
            .iter()
            .take(ITEM_LIMIT)
            .map(|job| WebCronJobSnapshot {
                id: feed::truncate_chars(&job.id, 18),
                enabled: job.enabled,
                schedule: job.schedule.clone(),
                action: web_preview(&job.action),
                skipped_overlap_count: job.skipped_overlap_count,
                last_error: job.last_error.as_deref().map(web_preview),
            })
            .collect::<Vec<_>>();

        WebSidebarSnapshot {
            skills: WebSkillsSnapshot {
                total: skills.len(),
                enabled,
                disabled,
                builtin: source_count(SkillSource::Builtin),
                user: source_count(SkillSource::User),
                project: source_count(SkillSource::Project),
                items: skills
                    .iter()
                    .map(|skill| WebSkillSnapshot {
                        name: skill.name.clone(),
                        source: skill.source.label().to_string(),
                        file_path: skill.file_path.clone(),
                        enabled: !skill.disable_model_invocation,
                    })
                    .collect(),
            },
            triggers: WebTriggersSnapshot {
                total: rules.len(),
                enabled: trigger_enabled,
                disabled: rules.len().saturating_sub(trigger_enabled),
                rules: trigger_rules,
            },
            cron: WebCronSnapshot {
                total: cron_jobs.len(),
                enabled: cron_enabled,
                disabled: cron_jobs.len().saturating_sub(cron_enabled),
                jobs: cron_job_rows,
            },
            mcp: WebMcpSnapshot {
                servers: self.panel_status.mcp_servers,
                tools: self.panel_status.mcp_tools,
                notification_hooks: self.panel_status.mcp_notification_hooks,
                server_names: self.panel_status.mcp_server_names.clone(),
                tool_names: self.panel_status.mcp_tool_names.clone(),
            },
            tools: WebToolsSnapshot {
                total: self.panel_status.tool_names.len(),
                names: self.panel_status.tool_names.clone(),
            },
            hooks: self.panel_status.hook_points.clone(),
            runtime: self.panel_status.trigger_features.clone(),
        }
    }

    async fn publish_snapshot(
        &self,
        latest: &Arc<Mutex<WebSnapshot>>,
        snapshots: &broadcast::Sender<WebSnapshot>,
    ) {
        let snapshot = self.web_snapshot();
        *latest.lock().await = snapshot.clone();
        let _ = snapshots.send(snapshot);
    }
}

fn web_router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/state", get(state_snapshot))
        .route("/events", get(events))
        .route("/prompt", post(prompt))
        .route("/complete", post(complete))
        .route("/abort", post(abort))
        .route("/control-plane/resolve", post(resolve_control_plane))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn state_snapshot(State(state): State<HttpState>) -> Json<WebSnapshot> {
    Json(state.latest.lock().await.clone())
}

async fn events(
    State(state): State<HttpState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.snapshots.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(snapshot) => {
                    let data = serde_json::to_string(&snapshot)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_string());
                    return Some((Ok(Event::default().event("snapshot").data(data)), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn prompt(
    State(state): State<HttpState>,
    Json(req): Json<PromptRequest>,
) -> impl IntoResponse {
    let accepted = state
        .commands
        .send(WebCommand::Submit { text: req.text })
        .is_ok();
    Json(CommandAccepted { accepted })
}

async fn complete(
    State(state): State<HttpState>,
    Json(req): Json<CompleteRequest>,
) -> impl IntoResponse {
    Json(CompleteResponse {
        completions: state.completer.matches(&req.text),
    })
}

async fn abort(State(state): State<HttpState>) -> impl IntoResponse {
    let accepted = state.commands.send(WebCommand::Abort).is_ok();
    Json(CommandAccepted { accepted })
}

async fn resolve_control_plane(
    State(state): State<HttpState>,
    Json(req): Json<ControlPlaneDecisionRequest>,
) -> impl IntoResponse {
    let accepted = state
        .commands
        .send(WebCommand::ResolveControlPlane {
            approve: req.approve,
        })
        .is_ok();
    Json(CommandAccepted { accepted })
}

fn web_feed_lines(feed: &feed::Feed) -> Vec<String> {
    feed.lines(100)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect()
}

fn web_preview(text: &str) -> String {
    feed::truncate_chars(&crate::bug_report::redact(text), 120)
}

fn web_control_plane_prompt_snapshot(
    request: &pie_agent_core::ControlPlanePromptRequest,
) -> WebControlPlanePromptSnapshot {
    let payload = serde_json::to_string_pretty(&request.payload)
        .unwrap_or_else(|_| request.payload.to_string());
    WebControlPlanePromptSnapshot {
        tool_name: web_prompt_text(&request.tool_name, 80),
        label: web_prompt_text(&request.label, 160),
        reason: web_prompt_text(&request.reason, 180),
        args_hash: request.args_hash.chars().take(12).collect(),
        payload: web_prompt_text(&payload, 800),
    }
}

fn web_prompt_text(text: &str, cap: usize) -> String {
    feed::truncate_chars(&crate::bug_report::redact(text), cap)
}

fn bind_addr(options: &WebOptions) -> Result<SocketAddr> {
    let ip = match options.host.as_str() {
        "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        host => host
            .parse::<IpAddr>()
            .with_context(|| format!("parse --web-host `{host}` as an IP address"))?,
    };
    if !ip.is_loopback() {
        bail!("refusing non-loopback web bind {ip}; Web UI is loopback-only");
    }
    Ok(SocketAddr::new(ip, options.port))
}

fn open_web_browser(url: &str) -> Result<()> {
    if !crate::hub_join::browser_auto_open_available() {
        bail!("browser auto-open unavailable in this session");
    }
    let mut command = open_browser_command(url);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().context("spawn system browser")?;
    Ok(())
}

fn open_browser_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        cmd
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    }
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>pie web</title>
  <script>
    try {
      const theme = localStorage.getItem('pie-web-theme');
      if (theme === 'dark' || theme === 'light') {
        document.documentElement.dataset.theme = theme;
      }
    } catch (_) {}
  </script>
  <style>
    :root {
      color-scheme: light;
      --bg: #f7f7f7;
      --panel: #ffffff;
      --side: #fafafa;
      --field: #ffffff;
      --button-bg: #111111;
      --button-fg: #ffffff;
      --ink: #111111;
      --muted: #6b6b6b;
      --faint: #9a9a9a;
      --line: #d8d8d8;
      --line-strong: #b9b9b9;
      --soft: #eeeeee;
      --poll-bg: #fbfbfb;
      --overlay: rgba(255, 255, 255, 0.72);
      --shadow: rgba(0, 0, 0, 0.14);
      --tool: #5f5f5f;
      --tool-output: #8a8a8a;
    }
    :root[data-theme="dark"] {
      color-scheme: dark;
      --bg: #050505;
      --panel: #0b0b0b;
      --side: #080808;
      --field: #111111;
      --button-bg: #f4f4f4;
      --button-fg: #050505;
      --ink: #f4f4f4;
      --muted: #a1a1a1;
      --faint: #686868;
      --line: #262626;
      --line-strong: #4a4a4a;
      --soft: #1b1b1b;
      --poll-bg: #101010;
      --overlay: rgba(0, 0, 0, 0.72);
      --shadow: rgba(0, 0, 0, 0.5);
      --tool: #bcbcbc;
      --tool-output: #777777;
    }
    * { box-sizing: border-box; }
    html, body { height: 100%; overflow: hidden; }
    body {
      margin: 0;
      background: var(--bg);
      color: var(--ink);
      font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      letter-spacing: 0;
    }
    button, textarea { font: inherit; color: inherit; }
    .app {
      height: 100vh;
      height: 100dvh;
      display: grid;
      grid-template-columns: minmax(0, 1fr) 340px;
      background: var(--panel);
      overflow: hidden;
    }
    .workspace {
      min-width: 0;
      min-height: 0;
      display: grid;
      grid-template-rows: auto minmax(0, 1fr) auto;
      border-right: 1px solid var(--line);
    }
    header {
      min-height: 56px;
      padding: 12px 18px;
      border-bottom: 1px solid var(--line);
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 18px;
    }
    .brand { display: flex; align-items: baseline; gap: 10px; min-width: 0; }
    .brand strong { font-size: 15px; font-weight: 650; letter-spacing: 0; }
    .cwd-chip {
      min-width: 0;
      max-width: min(320px, 34vw);
      border: 1px solid var(--line);
      border-radius: 999px;
      padding: 1px 8px;
      color: var(--muted);
      background: var(--field);
      font-size: 12px;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .meta {
      min-width: 0;
      color: var(--muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .run-state {
      flex: 0 0 auto;
      display: inline-flex;
      align-items: center;
      gap: 8px;
      color: var(--muted);
    }
    .header-actions {
      flex: 0 0 auto;
      display: inline-flex;
      align-items: center;
      gap: 8px;
    }
    .dot {
      width: 7px;
      height: 7px;
      border-radius: 999px;
      background: var(--faint);
    }
    .run-state.busy .dot { background: var(--ink); }
    .content {
      min-height: 0;
      display: grid;
      grid-template-rows: auto minmax(0, 1fr);
    }
    #poll {
      display: none;
      padding: 9px 18px;
      border-bottom: 1px solid var(--line);
      color: var(--muted);
      background: var(--poll-bg);
      font-size: 12px;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    #poll.visible { display: block; }
    #feed {
      min-height: 0;
      overflow: auto;
      padding: 18px;
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      scroll-behavior: smooth;
    }
    .line {
      min-height: 1.5em;
      padding: 1px 0;
    }
    .line:empty::before { content: " "; }
    .feed-block {
      margin: 0 0 14px;
      overflow-wrap: anywhere;
    }
    .feed-meta {
      color: var(--muted);
      font-size: 12px;
      margin-bottom: 3px;
    }
    .feed-user .feed-body {
      color: var(--ink);
      white-space: pre-wrap;
    }
    .feed-thinking .feed-body {
      color: var(--muted);
      font-style: italic;
      white-space: pre-wrap;
    }
    .feed-plain.level_error .feed-body { color: var(--ink); }
    .markdown {
      color: var(--ink);
    }
    .markdown > :first-child { margin-top: 0; }
    .markdown > :last-child { margin-bottom: 0; }
    .markdown p { margin: 0 0 10px; }
    .markdown h1, .markdown h2, .markdown h3 {
      margin: 14px 0 8px;
      line-height: 1.25;
      letter-spacing: 0;
    }
    .markdown h1 { font-size: 18px; }
    .markdown h2 { font-size: 16px; }
    .markdown h3 { font-size: 14px; }
    .markdown ul, .markdown ol {
      margin: 0 0 10px 22px;
      padding: 0;
    }
    .markdown li { margin: 3px 0; }
    .markdown blockquote {
      margin: 0 0 10px;
      padding: 0 0 0 12px;
      border-left: 2px solid var(--line-strong);
      color: var(--muted);
    }
    .markdown pre {
      margin: 0 0 10px;
      padding: 10px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--side);
      overflow: auto;
      white-space: pre;
    }
    .markdown code {
      border: 1px solid var(--line);
      border-radius: 4px;
      background: var(--side);
      padding: 1px 4px;
    }
    .markdown pre code {
      border: 0;
      border-radius: 0;
      background: transparent;
      padding: 0;
    }
    .markdown a {
      color: var(--ink);
      text-decoration: underline;
      text-underline-offset: 2px;
    }
    .tool-details {
      margin: 2px 0;
      color: var(--tool);
    }
    .tool-details summary {
      cursor: pointer;
      min-height: 1.5em;
      outline: none;
    }
    .tool-details summary:hover { color: var(--ink); }
    .tool-output {
      margin: 4px 0 8px 18px;
      color: var(--tool-output);
      white-space: pre-wrap;
      overflow-wrap: anywhere;
    }
    footer {
      padding: 14px 18px 16px;
      border-top: 1px solid var(--line);
      background: var(--panel);
    }
    .completion-popup {
      display: none;
      max-height: min(220px, 32vh);
      overflow: auto;
      margin-bottom: 8px;
      border: 1px solid var(--line-strong);
      border-radius: 6px;
      background: var(--field);
      box-shadow: 0 10px 30px var(--shadow);
    }
    .completion-popup.visible { display: block; }
    .completion-item {
      width: 100%;
      height: 34px;
      display: flex;
      align-items: center;
      border: 0;
      border-bottom: 1px solid var(--line);
      border-radius: 0;
      background: transparent;
      color: var(--ink);
      text-align: left;
      padding: 0 11px;
    }
    .completion-item:last-child { border-bottom: 0; }
    .completion-item.selected,
    .completion-item:hover {
      background: var(--soft);
      filter: none;
    }
    form {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto auto;
      gap: 8px;
      align-items: start;
    }
    textarea {
      width: 100%;
      height: 40px;
      min-height: 40px;
      max-height: 40px;
      resize: none;
      border: 1px solid var(--line-strong);
      border-radius: 6px;
      background: var(--field);
      padding: 9px 11px;
      outline: none;
    }
    textarea:focus { border-color: var(--ink); }
    button {
      height: 40px;
      border: 1px solid var(--ink);
      border-radius: 6px;
      background: var(--button-bg);
      color: var(--button-fg);
      padding: 0 14px;
      cursor: pointer;
    }
    button.secondary {
      border-color: var(--line-strong);
      background: var(--field);
      color: var(--ink);
    }
    .theme-toggle {
      min-width: 72px;
      border-color: var(--line-strong);
      background: var(--field);
      color: var(--ink);
    }
    button:hover { filter: contrast(0.92); }
    .form-hint {
      margin-top: 8px;
      color: var(--muted);
      font-size: 12px;
    }
    aside {
      min-width: 0;
      min-height: 0;
      overflow: auto;
      background: var(--side);
    }
    .side-head {
      min-height: 56px;
      padding: 14px 16px;
      border-bottom: 1px solid var(--line);
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
    }
    .side-title { font-weight: 650; }
    .side-subtle { color: var(--muted); font-size: 12px; }
    .side-section {
      padding: 15px 16px;
      border-bottom: 1px solid var(--line);
    }
    .section-head {
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 12px;
      margin-bottom: 10px;
    }
    .section-head h2 {
      margin: 0;
      font-size: 12px;
      text-transform: uppercase;
      letter-spacing: 0;
      font-weight: 700;
    }
    .count { color: var(--muted); font-size: 12px; white-space: nowrap; }
    .metrics {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      border: 1px solid var(--line);
      border-radius: 6px;
      overflow: hidden;
      background: var(--field);
    }
    .metric {
      min-width: 0;
      padding: 8px;
      border-right: 1px solid var(--line);
    }
    .metric:last-child { border-right: 0; }
    .metric b { display: block; font-size: 16px; line-height: 1.2; }
    .metric span { display: block; color: var(--muted); font-size: 11px; }
    button.metric {
      width: 100%;
      height: auto;
      min-height: 48px;
      border: 0;
      border-right: 1px solid var(--line);
      border-radius: 0;
      background: transparent;
      color: var(--ink);
      text-align: left;
      cursor: pointer;
    }
    button.metric:hover { background: var(--soft); filter: none; }
    .detail-modal {
      position: fixed;
      inset: 0;
      display: none;
      align-items: center;
      justify-content: center;
      padding: 20px;
      background: var(--overlay);
      z-index: 18;
    }
    .detail-modal.visible { display: flex; }
    .detail-panel {
      width: min(560px, 100%);
      max-height: min(640px, calc(100vh - 40px));
      overflow: auto;
      border: 1px solid var(--ink);
      border-radius: 8px;
      background: var(--panel);
      box-shadow: 0 16px 50px var(--shadow);
    }
    .detail-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      padding: 16px 18px;
      border-bottom: 1px solid var(--line);
    }
    .detail-head h2 {
      margin: 0;
      font-size: 15px;
      letter-spacing: 0;
    }
    .detail-body {
      padding: 16px 18px;
    }
    .detail-close {
      width: 28px;
      height: 28px;
      padding: 0;
      border-color: var(--line-strong);
      background: var(--field);
      color: var(--ink);
    }
    .rows { display: grid; gap: 9px; margin-top: 10px; }
    .row {
      min-width: 0;
      display: grid;
      gap: 3px;
      padding-bottom: 9px;
      border-bottom: 1px solid var(--soft);
    }
    .row:last-child { padding-bottom: 0; border-bottom: 0; }
    .row-top {
      min-width: 0;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
    }
    .row-id {
      min-width: 0;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
      font-weight: 600;
    }
    .row-text {
      color: var(--muted);
      font-size: 12px;
      overflow-wrap: anywhere;
    }
    .pill {
      flex: 0 0 auto;
      border: 1px solid var(--line-strong);
      border-radius: 999px;
      padding: 1px 7px;
      color: var(--muted);
      font-size: 11px;
      background: var(--field);
    }
    .pill.on { border-color: var(--ink); color: var(--ink); }
    .empty { color: var(--muted); font-size: 12px; }
    .muted { color: var(--muted); }
    .approval {
      position: fixed;
      inset: 0;
      display: none;
      align-items: center;
      justify-content: center;
      padding: 20px;
      background: var(--overlay);
      z-index: 20;
    }
    .approval.visible { display: flex; }
    .approval-panel {
      width: min(620px, 100%);
      max-height: min(680px, calc(100vh - 40px));
      overflow: auto;
      border: 1px solid var(--ink);
      border-radius: 8px;
      background: var(--panel);
      box-shadow: 0 16px 50px var(--shadow);
    }
    .approval-head {
      padding: 16px 18px;
      border-bottom: 1px solid var(--line);
    }
    .approval-head h2 {
      margin: 0 0 4px;
      font-size: 15px;
      letter-spacing: 0;
    }
    .approval-body {
      padding: 16px 18px;
      display: grid;
      gap: 12px;
    }
    .approval-row { display: grid; gap: 4px; }
    .approval-label {
      color: var(--muted);
      font-size: 11px;
      text-transform: uppercase;
      letter-spacing: 0;
    }
    .approval-value {
      overflow-wrap: anywhere;
      white-space: pre-wrap;
    }
    .approval-payload {
      margin: 0;
      max-height: 220px;
      overflow: auto;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--side);
      padding: 10px;
      white-space: pre-wrap;
      overflow-wrap: anywhere;
    }
    .approval-actions {
      padding: 14px 18px 16px;
      border-top: 1px solid var(--line);
      display: flex;
      justify-content: flex-end;
      gap: 8px;
    }
    @media (max-width: 860px) {
      .app { grid-template-columns: 1fr; grid-template-rows: minmax(0, 1fr) minmax(260px, 38vh); }
      .workspace { border-right: 0; }
      aside { border-top: 1px solid var(--line); }
      form { grid-template-columns: 1fr; }
      form button { width: 100%; }
      header { align-items: flex-start; flex-direction: column; }
    }
  </style>
</head>
<body>
<main class="app">
  <section class="workspace">
    <header>
      <div class="brand">
        <strong>pie web</strong>
        <span id="cwd" class="cwd-chip"></span>
        <span id="model" class="meta"></span>
      </div>
      <div class="header-actions">
        <span id="status" class="run-state"><span class="dot"></span><span id="statusText">ready</span></span>
        <button type="button" class="theme-toggle" id="themeToggle">Dark</button>
      </div>
    </header>
    <div class="content">
      <div id="poll"></div>
      <section id="feed" aria-live="polite"></section>
    </div>
    <footer>
      <div id="completionPopup" class="completion-popup"></div>
      <form id="form">
        <textarea id="input" placeholder="Message pie, or type /help"></textarea>
        <button type="submit">Send</button>
        <button type="button" class="secondary" id="abort">Abort</button>
      </form>
      <div class="form-hint">Cmd+Enter send · Enter newline</div>
    </footer>
  </section>
  <aside aria-label="Automation status">
    <div class="side-head">
      <span class="side-title">Automation</span>
      <span id="session" class="side-subtle"></span>
    </div>
    <div id="sidebar"></div>
  </aside>
</main>
<section id="detailModal" class="detail-modal" aria-live="polite">
  <div class="detail-panel" role="dialog" aria-modal="true" aria-labelledby="detailTitle">
    <div class="detail-head">
      <h2 id="detailTitle">Details</h2>
      <button type="button" class="detail-close" id="closeDetail" aria-label="Close details">x</button>
    </div>
    <div id="detailBody" class="detail-body"></div>
  </div>
</section>
<section id="approval" class="approval" aria-live="polite">
  <div class="approval-panel" role="dialog" aria-modal="true" aria-labelledby="approvalTitle">
    <div class="approval-head">
      <h2 id="approvalTitle">Approval required</h2>
      <div id="approvalSubtitle" class="muted"></div>
    </div>
    <div class="approval-body">
      <div class="approval-row">
        <div class="approval-label">Action</div>
        <div id="approvalLabel" class="approval-value"></div>
      </div>
      <div class="approval-row">
        <div class="approval-label">Reason</div>
        <div id="approvalReason" class="approval-value"></div>
      </div>
      <div class="approval-row">
        <div class="approval-label">Args hash</div>
        <div id="approvalHash" class="approval-value"></div>
      </div>
      <div class="approval-row">
        <div class="approval-label">Preview</div>
        <pre id="approvalPayload" class="approval-payload"></pre>
      </div>
    </div>
    <div class="approval-actions">
      <button type="button" class="secondary" id="denyApproval">Deny</button>
      <button type="button" id="approveApproval">Approve</button>
    </div>
  </div>
</section>
<script>
const feed = document.getElementById('feed');
const model = document.getElementById('model');
const cwd = document.getElementById('cwd');
const status = document.getElementById('status');
const statusText = document.getElementById('statusText');
const poll = document.getElementById('poll');
const input = document.getElementById('input');
const form = document.getElementById('form');
const abortButton = document.getElementById('abort');
const themeToggle = document.getElementById('themeToggle');
const sidebar = document.getElementById('sidebar');
const session = document.getElementById('session');
const completionPopup = document.getElementById('completionPopup');
const detailModal = document.getElementById('detailModal');
const detailTitle = document.getElementById('detailTitle');
const detailBody = document.getElementById('detailBody');
const closeDetail = document.getElementById('closeDetail');
const approval = document.getElementById('approval');
const approvalSubtitle = document.getElementById('approvalSubtitle');
const approvalLabel = document.getElementById('approvalLabel');
const approvalReason = document.getElementById('approvalReason');
const approvalHash = document.getElementById('approvalHash');
const approvalPayload = document.getElementById('approvalPayload');
const approveApproval = document.getElementById('approveApproval');
const denyApproval = document.getElementById('denyApproval');
const THEME_KEY = 'pie-web-theme';
let completionItems = [];
let completionIndex = 0;
let completionRequestSeq = 0;

function currentTheme() {
  return document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light';
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  themeToggle.textContent = theme === 'dark' ? 'Light' : 'Dark';
  try { localStorage.setItem(THEME_KEY, theme); } catch (_) {}
}

applyTheme(currentTheme());
themeToggle.addEventListener('click', () => {
  applyTheme(currentTheme() === 'dark' ? 'light' : 'dark');
});

function node(tag, attrs = {}, children = []) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === 'class') el.className = value;
    else if (key === 'text') el.textContent = value;
    else el.setAttribute(key, value);
  }
  for (const child of children) el.append(child);
  return el;
}

function detailRows(items) {
  if (!items.length) return empty();
  return node('div', { class: 'rows' }, items.map((item) => node('div', { class: 'row' }, [
    node('div', { class: 'row-text', text: item })
  ])));
}

function showDetail(title, items) {
  detailTitle.textContent = title;
  detailBody.replaceChildren(detailRows(items));
  detailModal.className = 'detail-modal visible';
}

function hideDetail() {
  detailModal.className = 'detail-modal';
  detailBody.replaceChildren();
}

function metric(label, value, detailTitle, detailItems = []) {
  const tag = detailTitle ? 'button' : 'div';
  const attrs = detailTitle ? { class: 'metric', type: 'button' } : { class: 'metric' };
  const el = node(tag, attrs, [
    node('b', { text: String(value) }),
    node('span', { text: label })
  ]);
  if (detailTitle) {
    el.addEventListener('click', () => showDetail(detailTitle, detailItems));
  }
  return el;
}

function section(title, count, body) {
  return node('section', { class: 'side-section' }, [
    node('div', { class: 'section-head' }, [
      node('h2', { text: title }),
      node('span', { class: 'count', text: count })
    ]),
    body
  ]);
}

function metrics(items) {
  return node('div', { class: 'metrics' }, items.map(([label, value, title, detailItems]) => metric(label, value, title, detailItems || [])));
}

function empty(label = 'none') {
  return node('div', { class: 'empty', text: label });
}

function hideCompletions() {
  completionItems = [];
  completionIndex = 0;
  completionPopup.className = 'completion-popup';
  completionPopup.replaceChildren();
}

function renderCompletions() {
  if (!completionItems.length) {
    completionPopup.className = 'completion-popup';
    completionPopup.replaceChildren();
    return;
  }
  completionPopup.className = 'completion-popup visible';
  completionPopup.replaceChildren(...completionItems.map((item, index) => {
    const button = node('button', {
      class: index === completionIndex ? 'completion-item selected' : 'completion-item',
      type: 'button',
      text: item
    });
    button.addEventListener('mousedown', (event) => event.preventDefault());
    button.addEventListener('click', () => applyCompletion(index, false));
    return button;
  }));
}

function canComplete(text) {
  const trimmed = String(text ?? '').trimStart();
  return trimmed.startsWith('/') && !/\s/.test(trimmed.slice(1));
}

async function refreshCompletions() {
  const text = input.value;
  if (!canComplete(text)) {
    hideCompletions();
    return;
  }
  const requestSeq = ++completionRequestSeq;
  const response = await fetch('/complete', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text })
  });
  if (requestSeq !== completionRequestSeq) return;
  if (!response.ok) {
    hideCompletions();
    return;
  }
  const data = await response.json();
  completionItems = Array.isArray(data.completions) ? data.completions : [];
  completionIndex = 0;
  renderCompletions();
}

function applyCompletion(index = completionIndex, keepPopup = true) {
  if (!completionItems.length) return;
  const bounded = ((index % completionItems.length) + completionItems.length) % completionItems.length;
  completionIndex = bounded;
  input.value = completionItems[bounded];
  input.focus();
  if (keepPopup) {
    renderCompletions();
  } else {
    hideCompletions();
  }
}

function moveCompletion(delta) {
  if (!completionItems.length) return;
  completionIndex = (completionIndex + delta + completionItems.length) % completionItems.length;
  renderCompletions();
}

function pill(label, on) {
  return node('span', { class: on ? 'pill on' : 'pill', text: label });
}

function renderRules(rules) {
  if (!rules.length) return empty();
  return node('div', { class: 'rows' }, rules.map((rule) => node('div', { class: 'row' }, [
    node('div', { class: 'row-top' }, [
      node('span', { class: 'row-id', text: rule.id }),
      pill((rule.enabled ? 'enabled' : 'disabled') + ' / ' + rule.mode, rule.enabled)
    ]),
    node('div', { class: 'row-text', text: 'when ' + rule.condition }),
    node('div', { class: 'row-text', text: 'do ' + rule.action })
  ])));
}

function renderCronJobs(jobs) {
  if (!jobs.length) return empty();
  return node('div', { class: 'rows' }, jobs.map((job) => {
    const children = [
      node('div', { class: 'row-top' }, [
        node('span', { class: 'row-id', text: job.id }),
        pill(job.enabled ? 'enabled' : 'disabled', job.enabled)
      ]),
      node('div', { class: 'row-text', text: job.schedule }),
      node('div', { class: 'row-text', text: 'do ' + job.action })
    ];
    if (job.skipped_overlap_count) {
      children.push(node('div', { class: 'row-text', text: 'skipped overlaps ' + job.skipped_overlap_count }));
    }
    if (job.last_error) {
      children.push(node('div', { class: 'row-text', text: 'error ' + job.last_error }));
    }
    return node('div', { class: 'row' }, children);
  }));
}

function renderList(items) {
  if (!items.length) return empty();
  return node('div', { class: 'rows' }, items.map((item) => node('div', { class: 'row' }, [
    node('div', { class: 'row-text', text: item })
  ])));
}

function renderSkills(skills) {
  const skillDetail = (skill) =>
    skill.name + ' / ' + skill.source + ' / ' + (skill.enabled ? 'enabled' : 'disabled') + ' / path ' + skill.file_path;
  const allSkills = skills.items.map((skill) =>
    skillDetail(skill)
  );
  const enabledSkills = skills.items
    .filter((skill) => skill.enabled)
    .map(skillDetail);
  const disabledSkills = skills.items
    .filter((skill) => !skill.enabled)
    .map(skillDetail);
  return node('div', {}, [
    metrics([
      ['enabled', skills.enabled, 'Enabled skills', enabledSkills],
      ['disabled', skills.disabled, 'Disabled skills', disabledSkills],
      ['total', skills.total, 'All skills', allSkills]
    ]),
    node('div', { class: 'rows' }, [
      node('div', { class: 'row' }, [
        node('div', { class: 'row-text', text: 'builtin ' + skills.builtin + ' / user ' + skills.user + ' / project ' + skills.project })
      ])
    ])
  ]);
}

function renderSidebar(snapshot) {
  const side = snapshot.sidebar;
  const blocks = [];
  blocks.push(section('Skills', side.skills.total + ' total', renderSkills(side.skills)));
  blocks.push(section('Triggers', side.triggers.enabled + ' enabled / ' + side.triggers.disabled + ' disabled', renderRules(side.triggers.rules)));

  const pollBody = snapshot.latest_trigger_poll
    ? node('div', { class: 'rows' }, [
        node('div', { class: 'row' }, [
          node('div', { class: 'row-top' }, [
            node('span', { class: 'row-id', text: snapshot.latest_trigger_poll.checked_at }),
            pill('no match', false)
          ]),
          node('div', { class: 'row-text', text: snapshot.latest_trigger_poll.source_label + ' / ' + snapshot.latest_trigger_poll.event_label }),
          node('div', { class: 'row-text', text: snapshot.latest_trigger_poll.summary })
        ])
      ])
    : empty();
  blocks.push(section('Polling', snapshot.latest_trigger_poll ? 'latest check' : 'idle', pollBody));

  const goalBody = snapshot.goal
    ? node('div', { class: 'rows' }, [
        node('div', { class: 'row' }, [
          node('div', { class: 'row-top' }, [
            node('span', { class: 'row-id', text: snapshot.goal.status }),
            pill(snapshot.goal.iterations + ' checks', snapshot.goal.status === 'pursuing')
          ]),
          node('div', { class: 'row-text', text: snapshot.goal.condition }),
          snapshot.goal.last_reason ? node('div', { class: 'row-text', text: snapshot.goal.last_reason }) : document.createTextNode('')
        ])
      ])
    : empty();
  blocks.push(section('Goal', snapshot.goal ? snapshot.goal.status : 'none', goalBody));

  blocks.push(section('Cron', side.cron.enabled + ' enabled / ' + side.cron.disabled + ' disabled', renderCronJobs(side.cron.jobs)));
  blocks.push(section('MCP', side.mcp.servers + ' servers', metrics([
    ['servers', side.mcp.servers, 'MCP servers', side.mcp.server_names],
    ['tools', side.mcp.tools, 'MCP tools', side.mcp.tool_names],
    ['notify', side.mcp.notification_hooks, 'MCP notification hooks', side.mcp.server_names]
  ])));
  blocks.push(section('Tools', side.tools.total + ' total', metrics([
    ['registered', side.tools.total, 'Registered tools', side.tools.names],
    ['mcp', side.mcp.tools, 'MCP tools', side.mcp.tool_names],
    ['builtin', Math.max(side.tools.total - side.mcp.tools, 0), 'Built-in tools', side.tools.names.filter((name) => !side.mcp.tool_names.includes(name))]
  ])));
  blocks.push(section('Hooks', side.hooks.length + ' active', renderList(side.hooks)));
  blocks.push(section('Runtime', side.runtime.length + ' features', renderList(side.runtime)));
  sidebar.replaceChildren(...blocks);
}

function renderApproval(snapshot) {
  const prompt = snapshot.control_plane_prompt;
  if (!prompt) {
    approval.className = 'approval';
    return;
  }
  approvalSubtitle.textContent = prompt.tool_name;
  approvalLabel.textContent = prompt.label;
  approvalReason.textContent = prompt.reason;
  approvalHash.textContent = prompt.args_hash;
  approvalPayload.textContent = prompt.payload;
  approval.className = 'approval visible';
}

function escapeHtml(text) {
  return String(text ?? '').replace(/[&<>"']/g, (ch) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;'
  })[ch]);
}

function escapeAttr(text) {
  return escapeHtml(text).replace(/`/g, '&#96;');
}

function safeHref(url) {
  const trimmed = String(url ?? '').trim();
  if (/^(https?:|mailto:|#|\/)/i.test(trimmed)) return trimmed;
  return '';
}

function renderInlineMarkdown(text) {
  let html = escapeHtml(text);
  html = html.replace(/`([^`]+)`/g, '<code>$1</code>');
  html = html.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  html = html.replace(/\*([^*]+)\*/g, '<em>$1</em>');
  html = html.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_m, label, url) => {
    const href = safeHref(url.replace(/&amp;/g, '&'));
    if (!href) return label;
    return '<a href="' + escapeAttr(href) + '" target="_blank" rel="noreferrer">' + label + '</a>';
  });
  return html;
}

function paragraphHtml(lines) {
  return '<p>' + lines.map(renderInlineMarkdown).join('<br>') + '</p>';
}

function markdownToHtml(markdown) {
  const lines = String(markdown ?? '').replace(/\r\n/g, '\n').split('\n');
  const out = [];
  let paragraph = [];
  let list = null;
  let quote = [];
  let inCode = false;
  let code = [];

  function flushParagraph() {
    if (!paragraph.length) return;
    out.push(paragraphHtml(paragraph));
    paragraph = [];
  }
  function flushList() {
    if (!list) return;
    out.push('<' + list.type + '>' + list.items.map((item) => '<li>' + renderInlineMarkdown(item) + '</li>').join('') + '</' + list.type + '>');
    list = null;
  }
  function flushQuote() {
    if (!quote.length) return;
    out.push('<blockquote>' + quote.map(renderInlineMarkdown).join('<br>') + '</blockquote>');
    quote = [];
  }

  for (const line of lines) {
    const fence = line.match(/^```/);
    if (fence) {
      if (inCode) {
        out.push('<pre><code>' + escapeHtml(code.join('\n')) + '</code></pre>');
        code = [];
        inCode = false;
      } else {
        flushParagraph();
        flushList();
        flushQuote();
        inCode = true;
      }
      continue;
    }
    if (inCode) {
      code.push(line);
      continue;
    }
    if (!line.trim()) {
      flushParagraph();
      flushList();
      flushQuote();
      continue;
    }
    const heading = line.match(/^(#{1,3})\s+(.+)$/);
    if (heading) {
      flushParagraph();
      flushList();
      flushQuote();
      const level = heading[1].length;
      out.push('<h' + level + '>' + renderInlineMarkdown(heading[2]) + '</h' + level + '>');
      continue;
    }
    const unordered = line.match(/^\s*[-*]\s+(.+)$/);
    const ordered = line.match(/^\s*\d+\.\s+(.+)$/);
    if (unordered || ordered) {
      flushParagraph();
      flushQuote();
      const type = unordered ? 'ul' : 'ol';
      if (!list || list.type !== type) flushList();
      if (!list) list = { type, items: [] };
      list.items.push((unordered || ordered)[1]);
      continue;
    }
    const quoted = line.match(/^>\s?(.+)$/);
    if (quoted) {
      flushParagraph();
      flushList();
      quote.push(quoted[1]);
      continue;
    }
    flushList();
    flushQuote();
    paragraph.push(line);
  }
  if (inCode) out.push('<pre><code>' + escapeHtml(code.join('\n')) + '</code></pre>');
  flushParagraph();
  flushList();
  flushQuote();
  return out.join('');
}

function isToolStartLine(line) {
  return /(^|\s)⚙ /.test(line);
}

function isToolOutputLine(line) {
  return /^((\d\d:\d\d|\d\d-\d\d \d\d:\d\d) )? {4}/.test(line);
}

function renderFeedLines(lines) {
  const nodes = [];
  let tool = null;
  function flushTool() {
    if (!tool) return;
    const details = node('details', { class: 'tool-details' }, [
      node('summary', { text: tool.summary }),
      node('pre', { class: 'tool-output', text: tool.output.join('\n') || 'no output' })
    ]);
    nodes.push(details);
    tool = null;
  }

  for (const line of lines) {
    if (isToolStartLine(line)) {
      flushTool();
      tool = { summary: line, output: [] };
      continue;
    }
    if (tool && (isToolOutputLine(line) || line.trim() === '')) {
      tool.output.push(line.replace(/^((\d\d:\d\d|\d\d-\d\d \d\d:\d\d) )? {4}/, ''));
      continue;
    }
    flushTool();
    nodes.push(node('div', { class: 'line', text: line }));
  }
  flushTool();
  return nodes;
}

function metaLabel(timestamp, label) {
  return (timestamp ? timestamp + ' ' : '') + label;
}

function renderMarkdownBlock(block, className, label) {
  const outer = node('article', { class: 'feed-block ' + className }, [
    node('div', { class: 'feed-meta', text: metaLabel(block.timestamp, label) })
  ]);
  const body = node('div', { class: 'feed-body markdown' });
  body.innerHTML = markdownToHtml(block.text);
  outer.append(body);
  return outer;
}

function renderFeedBlocks(blocks) {
  if (!Array.isArray(blocks)) return null;
  const nodes = [];
  let pendingTool = null;
  function flushTool() {
    if (!pendingTool) return;
    const details = node('details', { class: 'tool-details feed-block' }, [
      node('summary', { text: metaLabel(pendingTool.timestamp, '⚙ ' + pendingTool.name + pendingTool.args) }),
      node('pre', { class: 'tool-output', text: pendingTool.output.join('\n') || 'no output' })
    ]);
    nodes.push(details);
    pendingTool = null;
  }

  for (const block of blocks) {
    if (block.kind === 'tool') {
      flushTool();
      pendingTool = {
        name: block.name || '',
        args: block.args || '',
        timestamp: block.timestamp,
        output: []
      };
      continue;
    }
    if (block.kind === 'tool_result') {
      if (!pendingTool) {
        pendingTool = { name: 'tool', args: '', timestamp: block.timestamp, output: [] };
      }
      pendingTool.output.push(...(block.lines || []));
      continue;
    }
    flushTool();
    if (block.kind === 'assistant') {
      nodes.push(renderMarkdownBlock(block, 'feed-assistant', 'ai ▸'));
    } else if (block.kind === 'plain') {
      nodes.push(renderMarkdownBlock(block, 'feed-plain level_' + block.level, ''));
    } else if (block.kind === 'user') {
      nodes.push(node('article', { class: 'feed-block feed-user' }, [
        node('div', { class: 'feed-meta', text: metaLabel(block.timestamp, 'you ▸') }),
        node('div', { class: 'feed-body', text: block.text || '' })
      ]));
    } else if (block.kind === 'thinking') {
      nodes.push(node('article', { class: 'feed-block feed-thinking' }, [
        node('div', { class: 'feed-meta', text: metaLabel(block.timestamp, '[thinking]') }),
        node('div', { class: 'feed-body', text: block.text || '' })
      ]));
    }
  }
  flushTool();
  return nodes;
}

function render(snapshot) {
  model.textContent = snapshot.model;
  const cwdText = String(snapshot.cwd || '');
  cwd.textContent = cwdText || '.';
  cwd.title = cwdText;
  session.textContent = snapshot.session_id;
  const stateText = snapshot.busy
    ? ('working' + (snapshot.queued_count ? ' / ' + snapshot.queued_count + ' queued' : ''))
    : ('ready' + (snapshot.queued_count ? ' / ' + snapshot.queued_count + ' queued' : ''));
  statusText.textContent = stateText;
  status.className = snapshot.busy ? 'run-state busy' : 'run-state';
  if (snapshot.latest_trigger_poll) {
    poll.textContent = 'Polling / ' + snapshot.latest_trigger_poll.checked_at + ' / '
      + snapshot.latest_trigger_poll.source_label + ' / '
      + snapshot.latest_trigger_poll.event_label + ' / '
      + snapshot.latest_trigger_poll.summary;
    poll.className = 'visible';
  } else {
    poll.textContent = '';
    poll.className = '';
  }
  feed.replaceChildren(...(renderFeedBlocks(snapshot.feed_blocks) || renderFeedLines(snapshot.feed_lines)));
  feed.scrollTop = feed.scrollHeight;
  renderSidebar(snapshot);
  renderApproval(snapshot);
}

fetch('/state').then((r) => r.json()).then(render);
const events = new EventSource('/events');
events.addEventListener('snapshot', (event) => render(JSON.parse(event.data)));

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const text = input.value;
  if (!text.trim()) return;
  input.value = '';
  hideCompletions();
  await fetch('/prompt', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text })
  });
});

abortButton.addEventListener('click', () => fetch('/abort', { method: 'POST' }));
closeDetail.addEventListener('click', hideDetail);
detailModal.addEventListener('click', (event) => {
  if (event.target === detailModal) hideDetail();
});
async function resolveApproval(approve) {
  await fetch('/control-plane/resolve', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ approve })
  });
}
approveApproval.addEventListener('click', () => resolveApproval(true));
denyApproval.addEventListener('click', () => resolveApproval(false));
input.addEventListener('input', refreshCompletions);
input.addEventListener('keydown', (event) => {
  if (approval.classList.contains('visible') && event.key === 'Enter' && event.metaKey) {
    event.preventDefault();
    resolveApproval(true);
  } else if (event.key === 'Tab' && completionItems.length) {
    event.preventDefault();
    applyCompletion(completionIndex, true);
    moveCompletion(1);
  } else if (event.key === 'ArrowDown' && completionItems.length) {
    event.preventDefault();
    moveCompletion(1);
  } else if (event.key === 'ArrowUp' && completionItems.length) {
    event.preventDefault();
    moveCompletion(-1);
  } else if (event.key === 'Enter' && event.metaKey) {
    event.preventDefault();
    form.requestSubmit();
  } else if (event.key === 'Escape') {
    if (approval.classList.contains('visible')) {
      resolveApproval(false);
    } else if (detailModal.classList.contains('visible')) {
      hideDetail();
    } else if (completionItems.length) {
      hideCompletions();
    } else {
      fetch('/abort', { method: 'POST' });
    }
  }
});
document.addEventListener('keydown', (event) => {
  if (approval.classList.contains('visible') && event.key === 'Enter' && event.metaKey) {
    event.preventDefault();
    resolveApproval(true);
  } else if (approval.classList.contains('visible') && event.key === 'Escape') {
    event.preventDefault();
    resolveApproval(false);
  } else if (detailModal.classList.contains('visible') && event.key === 'Escape') {
    event.preventDefault();
    hideDetail();
  }
});
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn bind_addr_rejects_remote_by_default() {
        let err = bind_addr(&WebOptions {
            host: "0.0.0.0".into(),
            port: 0,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing non-loopback"));
    }

    #[test]
    fn bind_addr_accepts_loopback_and_localhost() {
        let local = bind_addr(&WebOptions {
            host: "127.0.0.1".into(),
            port: 0,
        })
        .unwrap();
        assert!(local.ip().is_loopback());

        let named = bind_addr(&WebOptions {
            host: "localhost".into(),
            port: 0,
        })
        .unwrap();
        assert!(named.ip().is_loopback());
    }

    #[test]
    fn web_feed_lines_keeps_all_rows() {
        let mut feed = feed::Feed::new();
        for i in 0..250 {
            feed.apply(feed::FeedUpdate::Plain {
                text: format!("line {i}"),
                level: feed::Level::Output,
            });
        }

        let lines = web_feed_lines(&feed);
        assert_eq!(lines.len(), 250);
        assert!(
            lines.first().is_some_and(|line| line.contains("line 0")),
            "{lines:?}"
        );
        assert!(
            lines.last().is_some_and(|line| line.contains("line 249")),
            "{lines:?}"
        );
        assert_eq!(
            lines.first().and_then(|line| line.chars().nth(2)),
            Some(':'),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn endpoints_return_state_accept_commands_and_stream_snapshots() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<WebCommand>();
        let (snapshot_tx, _) = broadcast::channel::<WebSnapshot>(16);
        let latest = Arc::new(Mutex::new(WebSnapshot {
            session_id: "sess-1".into(),
            model: "provider:model".into(),
            cwd: "/tmp/pie".into(),
            busy: false,
            queued_count: 0,
            latest_trigger_poll: None,
            goal: None,
            control_plane_prompt: None,
            sidebar: empty_sidebar_snapshot(),
            feed_blocks: Vec::new(),
            feed_lines: vec!["ready".into()],
        }));
        let router = web_router(HttpState {
            commands: command_tx,
            snapshots: snapshot_tx.clone(),
            latest,
            completer: SlashCompleter::from_registry(&crate::commands::Registry::with_builtins()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service())
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let state: serde_json::Value = client
            .get(format!("{base}/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["session_id"], "sess-1");
        assert_eq!(state["cwd"], "/tmp/pie");
        assert_eq!(state["feed_lines"][0], "ready");

        let accepted: serde_json::Value = client
            .post(format!("{base}/prompt"))
            .json(&json!({ "text": "hello" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        match command_rx.recv().await.unwrap() {
            WebCommand::Submit { text } => assert_eq!(text, "hello"),
            other => panic!("unexpected command: {other:?}"),
        }

        let accepted: serde_json::Value = client
            .post(format!("{base}/abort"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        match command_rx.recv().await.unwrap() {
            WebCommand::Abort => {}
            other => panic!("unexpected command: {other:?}"),
        }

        let accepted: serde_json::Value = client
            .post(format!("{base}/control-plane/resolve"))
            .json(&json!({ "approve": true }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        match command_rx.recv().await.unwrap() {
            WebCommand::ResolveControlPlane { approve } => assert!(approve),
            other => panic!("unexpected command: {other:?}"),
        }

        let completions: serde_json::Value = client
            .post(format!("{base}/complete"))
            .json(&json!({ "text": "/he" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            completions["completions"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == "/help")),
            "{completions}"
        );

        let response = client.get(format!("{base}/events")).send().await.unwrap();
        assert!(response.status().is_success());
        let mut stream = response.bytes_stream();
        snapshot_tx
            .send(WebSnapshot {
                session_id: "sess-1".into(),
                model: "provider:model".into(),
                cwd: "/tmp/pie".into(),
                busy: true,
                queued_count: 1,
                latest_trigger_poll: None,
                goal: None,
                control_plane_prompt: None,
                sidebar: empty_sidebar_snapshot(),
                feed_blocks: Vec::new(),
                feed_lines: vec!["streamed".into()],
            })
            .unwrap();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&chunk);
        assert!(text.contains("event: snapshot"), "{text}");
        assert!(text.contains("streamed"), "{text}");

        server.abort();
    }

    fn empty_sidebar_snapshot() -> WebSidebarSnapshot {
        WebSidebarSnapshot {
            skills: WebSkillsSnapshot {
                total: 0,
                enabled: 0,
                disabled: 0,
                builtin: 0,
                user: 0,
                project: 0,
                items: Vec::new(),
            },
            triggers: WebTriggersSnapshot {
                total: 0,
                enabled: 0,
                disabled: 0,
                rules: Vec::new(),
            },
            cron: WebCronSnapshot {
                total: 0,
                enabled: 0,
                disabled: 0,
                jobs: Vec::new(),
            },
            mcp: WebMcpSnapshot {
                servers: 0,
                tools: 0,
                notification_hooks: 0,
                server_names: Vec::new(),
                tool_names: Vec::new(),
            },
            tools: WebToolsSnapshot {
                total: 0,
                names: Vec::new(),
            },
            hooks: Vec::new(),
            runtime: Vec::new(),
        }
    }
}
