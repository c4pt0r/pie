//! Full-screen terminal UI for the `pie` REPL.
//!
//! Layout is a fixed bottom **input box** with a scrolling **conversation feed** above it:
//!
//! ```text
//! ┌────────────────────────── conversation feed ──────────────────────────┐
//! │ you ▸ refactor the tui                                                  │
//! │ ⚙ read(path="src/main.rs")                                              │
//! │     …file contents…                                                     │
//! │ Done. The input box is now pinned to the bottom.                        │
//! ├── pie · anthropic:claude · ⠹ working ──────────────────────────────────┤
//! │ > type here…                                                            │
//! │ Enter send · Alt+Enter newline · ↑↓ history · PgUp/PgDn scroll · /help  │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The model turn runs as a local future polled by the event loop's `select!`, so the feed
//! streams and the input box stays live while the assistant responds; Ctrl-C/Esc aborts the
//! in-flight turn (raw mode delivers Ctrl-C as a key, not a signal). Inject-and-run triggered
//! turns funnel through the same single serialized run slot as user prompts, so they never race.
//!
//! Agent/harness events never write to stdout directly — they arrive as [`FeedUpdate`]s on a
//! channel (see [`listener`]) and slash-command output arrives via the console sink
//! (`commands::console`). The ratatui terminal is the single writer.

pub mod feed;
pub(crate) mod kernel;
pub mod listener;
pub(crate) mod relay;
pub(crate) mod web;

pub use feed::FeedUpdate;

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt as _;
use once_cell::sync::Lazy;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph, Wrap};
use regex::Regex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tui_textarea::TextArea;

use crate::agent_session::RetrySettings;
use crate::commands::{self, CommandCtx, CommandOutcome, Registry};
use crate::control_plane_prompt::UiControlPlanePrompt;
use crate::history::HistoryStore;
use crate::readline::SlashCompleter;
use crate::{images, mentions};
use feed::{Feed, Level, TriggerPollStatus};
use kernel::{QueuedTurn, ReplKernel, TurnState, poll_turn};
use pie_agent_core::{AgentHarness, AgentMessage, AgentRunError, SkillSource};
use pie_ai::{ContentBlock, ImageContent, Message, UserContent, UserContentBlock};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_INPUT_ROWS: usize = 6;
const SCROLL_STEP: usize = 3;
const COMPLETION_POPUP_MAX: usize = 8;
const QUEUED_PREVIEW_CHARS: usize = 80;
const TRIGGER_PANEL_MIN_TOTAL_WIDTH: u16 = 100;
const TRIGGER_PANEL_WIDTH: u16 = 36;
const TRIGGER_PANEL_RULE_LIMIT: usize = 5;
const CONTROL_PROMPT_TEXT_WIDTH: usize = 68;

#[derive(Clone, Debug, Default)]
pub struct PanelStatus {
    pub mcp_servers: usize,
    pub mcp_tools: usize,
    pub mcp_server_names: Vec<String>,
    pub mcp_tool_names: Vec<String>,
    pub tool_names: Vec<String>,
    /// Count of `McpNotificationHook` instances (RFC 1 §4.2.3) — server-pushed notification
    /// adapters fanning MCP frames into the trigger runtime. Distinct from `hook_points`,
    /// which lists `*Hook` trait registrations (e.g. `before_tool_call`).
    pub mcp_notification_hooks: usize,
    /// Real `AgentHarness` `*Hook` trait registrations active in this binary.
    pub hook_points: Vec<String>,
    /// Trigger-runtime pipeline features wired in this binary (dedup, cycle, etc.). Not
    /// pluggable callbacks — labelled separately from `hook_points` so users can't mistake
    /// them for extension points.
    pub trigger_features: Vec<String>,
}

/// Everything the app needs to run a session, assembled by `main.rs` after the harness is built.
pub struct AppConfig {
    pub harness: Arc<AgentHarness>,
    pub retry: RetrySettings,
    pub registry: Registry,
    pub cwd: PathBuf,
    pub session_id: String,
    pub log_path: Option<PathBuf>,
    pub tool_count: usize,
    pub history: HistoryStore,
    /// `--image` payloads attached to the first prompt only.
    pub pending_images: Vec<PathBuf>,
    pub feed_rx: UnboundedReceiver<FeedUpdate>,
    pub main_run_rx: UnboundedReceiver<String>,
    pub control_plane_prompt_rx: Option<UnboundedReceiver<UiControlPlanePrompt>>,
    pub panel_status: PanelStatus,
}

pub struct App {
    kernel: ReplKernel,
    registry: Registry,
    completer: SlashCompleter,
    cwd: PathBuf,
    session_id: String,
    log_path: Option<PathBuf>,
    tool_count: usize,

    history: HistoryStore,
    history_idx: Option<usize>,
    draft: String,
    pending_skill: Option<String>,
    pending_images: Vec<PathBuf>,
    pending_pasted_images: Vec<ImageContent>,

    feed: Feed,
    latest_trigger_poll: Option<TriggerPollStatus>,
    latest_goal: Option<crate::goal::GoalState>,
    feed_rx: Option<UnboundedReceiver<FeedUpdate>>,
    main_run_rx: Option<UnboundedReceiver<String>>,
    control_plane_prompt_rx: Option<UnboundedReceiver<UiControlPlanePrompt>>,
    control_plane_prompt: Option<UiControlPlanePrompt>,
    panel_status: PanelStatus,

    input: TextArea<'static>,
    completions: Vec<String>,
    completion_idx: usize,

    scroll: usize,
    follow: bool,
    last_viewport_h: usize,
    last_feed_area: Option<Rect>,

    busy: bool,
    queued_turns: std::collections::VecDeque<QueuedTurn>,
    spinner_frame: usize,
    last_ctrlc: Option<Instant>,
    quit: bool,

    // Remote relay (issue #22). The channels exist from construction so the event loops
    // can always select on them; they only carry traffic while a relay is connected.
    relay: Option<relay::RelayHandle>,
    /// Render a QR code of the relay URL into the feed on connect. Only useful where a
    /// real terminal shows the feed (TUI); off for web/headless modes.
    relay_qr_in_feed: bool,
    relay_prompt_tx: UnboundedSender<String>,
    relay_prompt_rx: Option<UnboundedReceiver<String>>,
    relay_abort_tx: UnboundedSender<()>,
    relay_abort_rx: Option<UnboundedReceiver<()>>,
    relay_resolve_tx: UnboundedSender<bool>,
    relay_resolve_rx: Option<UnboundedReceiver<bool>>,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let completer =
            SlashCompleter::from_registry_and_skills(&config.registry, &config.harness.skills());
        let (relay_prompt_tx, relay_prompt_rx) = tokio::sync::mpsc::unbounded_channel();
        let (relay_abort_tx, relay_abort_rx) = tokio::sync::mpsc::unbounded_channel();
        let (relay_resolve_tx, relay_resolve_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            kernel: ReplKernel::new(config.harness, config.retry),
            registry: config.registry,
            completer,
            cwd: config.cwd,
            session_id: config.session_id,
            log_path: config.log_path,
            tool_count: config.tool_count,
            history: config.history,
            history_idx: None,
            draft: String::new(),
            pending_skill: None,
            pending_images: config.pending_images,
            pending_pasted_images: Vec::new(),
            feed: Feed::new(),
            latest_trigger_poll: None,
            latest_goal: None,
            feed_rx: Some(config.feed_rx),
            main_run_rx: Some(config.main_run_rx),
            control_plane_prompt_rx: config.control_plane_prompt_rx,
            control_plane_prompt: None,
            panel_status: config.panel_status,
            input: new_textarea(),
            completions: Vec::new(),
            completion_idx: 0,
            scroll: 0,
            follow: true,
            last_viewport_h: 1,
            last_feed_area: None,
            busy: false,
            queued_turns: std::collections::VecDeque::new(),
            spinner_frame: 0,
            last_ctrlc: None,
            quit: false,
            relay: None,
            relay_qr_in_feed: false,
            relay_prompt_tx,
            relay_prompt_rx: Some(relay_prompt_rx),
            relay_abort_tx,
            relay_abort_rx: Some(relay_abort_rx),
            relay_resolve_tx,
            relay_resolve_rx: Some(relay_resolve_rx),
        }
    }

    // ── startup feed seeding (called by main.rs before run) ─────────────────────────────

    pub fn banner(
        &mut self,
        model: &pie_ai::Model,
        session_id: &str,
        resumed: bool,
        tools: &[String],
    ) {
        self.feed
            .push_plain_untimed("──────── pie-coding-agent ────────", Level::Header);
        self.feed.push_plain_untimed(
            format!(
                "model:   {} ({}/{})",
                model.name, model.provider.0, model.id
            ),
            Level::Output,
        );
        self.feed.push_plain_untimed(
            format!(
                "session: {session_id}{}",
                if resumed { "  [resumed]" } else { "" }
            ),
            Level::Output,
        );
        let tools = if tools.is_empty() {
            "(none)".to_string()
        } else {
            tools.join(", ")
        };
        self.feed
            .push_plain_untimed(format!("tools:   {tools}"), Level::Output);
        self.feed.push_plain_untimed(
            "Enter send · Ctrl-V paste text/images · Ctrl-C abort/exit · /help",
            Level::System,
        );
    }

    pub fn system_line(&mut self, text: impl AsRef<str>) {
        self.feed.push_plain(text.as_ref(), Level::System);
    }

    pub fn error_line(&mut self, text: impl AsRef<str>) {
        self.feed
            .push_plain(format!("error: {}", text.as_ref()), Level::Error);
    }

    /// Push a replayed transcript (from `--resume`) into the feed as finished blocks.
    pub fn replay(&mut self, messages: &[AgentMessage]) {
        if messages.is_empty() {
            return;
        }
        self.system_line(format!("resumed — replaying {} messages", messages.len()));
        for message in messages {
            self.replay_message(message);
        }
    }

    fn replay_message(&mut self, message: &AgentMessage) {
        match message {
            AgentMessage::Llm(Message::User(u)) => {
                let text = match &u.content {
                    UserContent::Text(s) => s.clone(),
                    UserContent::Blocks(blocks) => blocks
                        .iter()
                        .map(|b| match b {
                            UserContentBlock::Text(t) => t.text.clone(),
                            UserContentBlock::Image(ImageContent { mime_type, .. }) => {
                                format!("<image {mime_type}>")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                self.feed.push_user_at(text, u.timestamp);
            }
            AgentMessage::Llm(Message::Assistant(a)) => {
                for b in &a.content {
                    match b {
                        ContentBlock::Text(t) => {
                            self.feed.push_assistant_at(t.text.clone(), a.timestamp)
                        }
                        ContentBlock::Thinking(t) => {
                            self.feed.push_thinking_at(t.thinking.clone(), a.timestamp)
                        }
                        ContentBlock::ToolCall(tc) => self.feed.push_tool_at(
                            tc.name.clone(),
                            feed::preview(&serde_json::Value::Object(tc.arguments.clone())),
                            a.timestamp,
                        ),
                        ContentBlock::Image(_) => {}
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(tr)) => {
                self.feed.push_tool_result_at(
                    tr.tool_call_id.clone(),
                    feed::compact_tool_content_blocks(&tr.content, tr.is_error),
                    tr.is_error,
                    tr.timestamp,
                );
            }
            AgentMessage::Custom(_) => {}
        }
    }

    // ── main entry ──────────────────────────────────────────────────────────────────────

    pub async fn run(mut self) -> Result<()> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return self.run_headless().await;
        }
        self.relay_qr_in_feed = true;
        enter_tui()?;
        let backend = CrosstermBackend::new(std::io::stdout());
        let mut terminal = Terminal::new(backend)?;
        let result = self.event_loop(&mut terminal).await;
        leave_tui().ok();
        terminal.show_cursor().ok();
        result
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let mut reader = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut feed_rx = self.feed_rx.take().expect("feed_rx taken once");
        let mut main_run_rx = self.main_run_rx.take().expect("main_run_rx taken once");
        let mut control_plane_prompt_rx = self.control_plane_prompt_rx.take();
        let mut relay_prompt_rx = self
            .relay_prompt_rx
            .take()
            .expect("relay_prompt_rx taken once");
        let mut relay_abort_rx = self
            .relay_abort_rx
            .take()
            .expect("relay_abort_rx taken once");
        let mut relay_resolve_rx = self
            .relay_resolve_rx
            .take()
            .expect("relay_resolve_rx taken once");
        let mut turn = TurnState::default();
        self.refresh_goal_state().await;

        loop {
            terminal.draw(|f| self.render(f))?;
            self.push_relay_snapshot();
            if self.quit {
                break;
            }
            tokio::select! {
                biased;
                result = poll_turn(&mut turn.fut), if turn.fut.is_some() => {
                    self.finish_turn(&mut turn, result).await;
                }
                maybe_event = reader.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_event(event, &mut turn, terminal).await?,
                        Some(Err(_)) => {}
                        None => self.quit = true,
                    }
                }
                Some(update) = feed_rx.recv() => {
                    self.apply_feed_update(update);
                    while let Ok(update) = feed_rx.try_recv() {
                        self.apply_feed_update(update);
                    }
                }
                Some(trace_id) = main_run_rx.recv(), if turn.fut.is_none() => {
                    self.start_triggered_turn(trace_id, &mut turn);
                }
                Some(text) = relay_prompt_rx.recv() => {
                    self.submit_remote_text(text, &mut turn);
                }
                Some(()) = relay_abort_rx.recv() => {
                    if turn.fut.is_some() {
                        self.system_line("[web] abort requested");
                        self.request_abort(&mut turn);
                    }
                }
                Some(approve) = relay_resolve_rx.recv() => {
                    self.resolve_from_relay(approve);
                }
                Some(prompt) = async {
                    match control_plane_prompt_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                }, if self.control_plane_prompt.is_none() && control_plane_prompt_rx.is_some() => {
                    self.show_control_plane_prompt(prompt);
                }
                _ = tick.tick() => {
                    if turn.fut.is_some() {
                        self.spinner_frame = self.spinner_frame.wrapping_add(1);
                    }
                }
            }
        }
        Ok(())
    }

    /// Wrap up a finished turn: clear the busy state and surface an aborted/error line.
    async fn finish_turn(
        &mut self,
        turn: &mut TurnState,
        result: Result<Option<String>, AgentRunError>,
    ) {
        turn.fut = None;
        self.busy = false;
        self.spinner_frame = 0;
        if turn.aborted {
            self.system_line("[aborted]");
        } else {
            match result {
                Ok(Some(message)) => self.system_line(message),
                Ok(None) => {}
                Err(e) => self.error_line(format!(
                    "{}{}",
                    turn.prefix,
                    user_facing_run_error(&e.to_string())
                )),
            }
        }
        turn.aborted = false;
        turn.prefix = "";
        self.refresh_goal_state().await;
        self.start_next_queued_turn(turn);
    }

    /// Handle `/web-connect` family outcomes. Shared by the TUI and web event loops.
    async fn handle_web_relay(&mut self, action: commands::WebRelayAction) {
        use commands::WebRelayAction;
        match action {
            WebRelayAction::Connect => {
                if let Some(active) = &self.relay {
                    self.system_line(format!("web relay already active: {}", active.url));
                    return;
                }
                let base = match crate::config::relay_base_url().await {
                    Ok(base) => base,
                    Err(e) => {
                        self.error_line(format!("web-connect: {e}"));
                        return;
                    }
                };
                match relay::start(
                    &base,
                    self.relay_prompt_tx.clone(),
                    self.relay_abort_tx.clone(),
                    self.relay_resolve_tx.clone(),
                ) {
                    Ok(handle) => {
                        self.system_line(format!("web relay: {}", handle.url));
                        self.system_line(
                            "warning: anyone with this URL can watch the full conversation, \
                             send prompts, AND approve permission requests until /web-disconnect",
                        );
                        if self.relay_qr_in_feed {
                            match relay::qr_lines(&handle.url) {
                                Ok(lines) => {
                                    self.feed.push_plain_untimed("", Level::Qr);
                                    for line in lines {
                                        self.feed.push_plain_untimed(line, Level::Qr);
                                    }
                                    self.feed.push_plain_untimed(
                                        "scan with your phone to open the session",
                                        Level::System,
                                    );
                                }
                                Err(e) => self.system_line(format!("qr render skipped: {e}")),
                            }
                        }
                        self.relay = Some(handle);
                        self.push_relay_snapshot();
                    }
                    Err(e) => self.error_line(format!("web-connect: {e}")),
                }
            }
            WebRelayAction::Status => match &self.relay {
                Some(active) => self.system_line(active.status_line()),
                None => self.system_line("web relay is off — start one with /web-connect"),
            },
            WebRelayAction::Disconnect => match self.relay.take() {
                Some(active) => {
                    active.shutdown();
                    self.system_line("web relay disconnected; the session URL is now invalid");
                }
                None => self.system_line("web relay is not active"),
            },
        }
    }

    /// Queue the current state to the relay, if one is connected. Cheap when off; the
    /// relay task debounces actual sends.
    fn push_relay_snapshot(&self) {
        if let Some(active) = &self.relay {
            active.push_snapshot(self.web_snapshot());
        }
    }

    /// Resolve a pending control-plane prompt from the relay — first-class, identical
    /// to a local confirmation (owner decision 2026-06-11).
    fn resolve_from_relay(&mut self, approve: bool) {
        if self.control_plane_prompt.is_none() {
            return;
        }
        let decision = if approve {
            pie_agent_core::ControlPlanePromptDecision::Allow
        } else {
            pie_agent_core::ControlPlanePromptDecision::Deny {
                reason: Some("denied via web relay".into()),
            }
        };
        self.resolve_control_plane_prompt(decision);
    }

    /// Inject a prompt that arrived over the relay through the same path as local
    /// input. Remote slash commands are refused — the capability URL grants prompting,
    /// not REPL control.
    fn submit_remote_text(&mut self, text: String, turn: &mut TurnState) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.starts_with('/') {
            self.system_line("[web] remote slash command refused");
            return;
        }
        let display = format!("[web] {trimmed}");
        self.follow = true;
        if turn.fut.is_some() {
            self.queue_user_prompt(display, trimmed, Vec::new());
        } else {
            self.feed.push_user(display);
            self.start_user_prompt_turn(trimmed, Vec::new(), turn);
        }
    }

    fn apply_feed_update(&mut self, update: FeedUpdate) {
        match update {
            FeedUpdate::TriggerPollStatus(status) => {
                self.latest_trigger_poll = Some(status);
            }
            other => self.feed.apply(other),
        }
    }

    fn show_control_plane_prompt(&mut self, prompt: UiControlPlanePrompt) {
        let label = safe_control_prompt_label(&prompt.request.label);
        self.control_plane_prompt = Some(prompt);
        self.system_line(format!("approval required: {label}"));
        self.follow = true;
    }

    fn resolve_control_plane_prompt(
        &mut self,
        decision: pie_agent_core::ControlPlanePromptDecision,
    ) {
        let Some(prompt) = self.control_plane_prompt.take() else {
            return;
        };
        let label = safe_control_prompt_label(&prompt.request.label);
        let message = match &decision {
            pie_agent_core::ControlPlanePromptDecision::Allow => {
                format!("approved control-plane action: {label}")
            }
            pie_agent_core::ControlPlanePromptDecision::Deny { reason } => {
                let reason = reason.as_deref().unwrap_or("denied by user");
                let reason = safe_control_prompt_text(reason, CONTROL_PROMPT_TEXT_WIDTH);
                format!("denied control-plane action: {label} ({reason})")
            }
            pie_agent_core::ControlPlanePromptDecision::Timeout => {
                format!("control-plane action timed out: {label}")
            }
        };
        prompt.resolve(decision);
        self.system_line(message);
    }

    // ── event handling ──────────────────────────────────────────────────────────────────

    async fn handle_event(
        &mut self,
        event: Event,
        turn: &mut TurnState,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.handle_key(key, turn, terminal).await?;
            }
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.handle_mouse_scroll(m.column, m.row, true),
                MouseEventKind::ScrollDown => self.handle_mouse_scroll(m.column, m.row, false),
                _ => {}
            },
            Event::Paste(text) => {
                self.input.insert_str(&text);
                self.refresh_completions();
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_key(
        &mut self,
        key: KeyEvent,
        turn: &mut TurnState,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        if self.handle_control_plane_prompt_key(&key) {
            return Ok(());
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if turn.fut.is_some() {
                    self.request_abort(turn);
                } else if self.on_idle_ctrlc() {
                    self.quit = true;
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.handle_ctrl_d(turn) {
                    return Ok(());
                }
                if self.input_text().is_empty() {
                    self.system_line("eof — exiting");
                    self.quit = true;
                } else {
                    self.input.input(key);
                    self.refresh_completions();
                }
            }
            KeyCode::Esc => {
                if !self.completions.is_empty() {
                    self.completions.clear();
                } else if turn.fut.is_some() {
                    self.request_abort(turn);
                } else {
                    self.clear_input();
                }
            }
            KeyCode::Enter if alt || shift => {
                self.input.insert_newline();
                self.refresh_completions();
            }
            KeyCode::Enter => {
                self.submit(turn, terminal).await?;
            }
            KeyCode::Char('v') if ctrl => {
                self.paste_clipboard().await;
            }
            KeyCode::Tab => self.cycle_completion(),
            KeyCode::PageUp => self.scroll_up(self.last_viewport_h.max(1)),
            KeyCode::PageDown => self.scroll_down(self.last_viewport_h.max(1)),
            KeyCode::Up if self.input_is_single_line() => self.history_prev(),
            KeyCode::Down if self.input_is_single_line() => self.history_next(),
            KeyCode::Char('u') if ctrl => {
                if self.input_text().is_empty() && turn.fut.is_some() {
                    self.cancel_last_queued_turn();
                } else {
                    self.clear_input();
                }
            }
            _ => {
                self.input.input(key);
                self.last_ctrlc = None;
                self.refresh_completions();
            }
        }
        Ok(())
    }

    fn handle_control_plane_prompt_key(&mut self, key: &KeyEvent) -> bool {
        if self.control_plane_prompt.is_none() {
            return false;
        }
        if key.kind == KeyEventKind::Release {
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let allow = matches!(
            key.code,
            KeyCode::Enter
                | KeyCode::Char('y')
                | KeyCode::Char('Y')
                | KeyCode::Char('a')
                | KeyCode::Char('A')
        );
        let deny = matches!(
            key.code,
            KeyCode::Esc
                | KeyCode::Char('n')
                | KeyCode::Char('N')
                | KeyCode::Char('d')
                | KeyCode::Char('D')
        ) || (ctrl && matches!(key.code, KeyCode::Char('c')));
        if allow {
            self.resolve_control_plane_prompt(pie_agent_core::ControlPlanePromptDecision::Allow);
        } else if deny {
            self.resolve_control_plane_prompt(pie_agent_core::ControlPlanePromptDecision::Deny {
                reason: Some("denied by user".into()),
            });
        }
        true
    }

    // ── submit / dispatch ───────────────────────────────────────────────────────────────

    async fn submit(
        &mut self,
        turn: &mut TurnState,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let text = self.input_text();
        let trimmed = text.trim().to_string();
        let has_pending_images =
            !self.pending_images.is_empty() || !self.pending_pasted_images.is_empty();
        if trimmed.is_empty() && !has_pending_images {
            return Ok(());
        }
        if trimmed.starts_with('/') {
            self.clear_input();
            self.history_idx = None;
            self.last_ctrlc = None;
            self.history.append(&trimmed);
            self.follow = true;
            self.feed.push_user(&trimmed);
            self.dispatch_slash(&trimmed, terminal, turn).await;
            return Ok(());
        }

        if !self.validate_pending_image_support() {
            return Ok(());
        }

        self.clear_input();
        self.history_idx = None;
        self.last_ctrlc = None;
        if !trimmed.is_empty() {
            self.history.append(&trimmed);
        }
        self.follow = true;

        let expanded = if trimmed.is_empty() {
            String::new()
        } else {
            mentions::expand(&trimmed, &self.cwd).await.0
        };
        let prompt_text =
            commands::attach_skill_prompt(expanded, self.pending_skill.take().as_deref());

        // `--image` payloads attach to the first prompt only.
        let image_paths = std::mem::take(&mut self.pending_images);
        let mut loaded_images = if image_paths.is_empty() {
            Vec::new()
        } else {
            match images::load_all(&image_paths).await {
                Ok(imgs) => imgs,
                Err(e) => {
                    self.error_line(format!("--image: {e}"));
                    Vec::new()
                }
            }
        };
        loaded_images.append(&mut self.pending_pasted_images);
        if prompt_text.trim().is_empty() && loaded_images.is_empty() {
            return Ok(());
        }

        let display = prompt_display(&trimmed, loaded_images.len());

        if turn.fut.is_some() {
            self.queue_user_prompt(display, prompt_text, loaded_images);
        } else {
            self.feed.push_user(display);
            self.start_user_prompt_turn(prompt_text, loaded_images, turn);
        }
        Ok(())
    }

    fn start_triggered_turn(&mut self, trace_id: String, turn: &mut TurnState) {
        // The kernel emits this only for an idle parent, but a user prompt may have started in
        // the gap; `continue_` would return AlreadyStreaming. Skip rather than error.
        if self.kernel.is_streaming() {
            return;
        }
        let short: String = trace_id.chars().take(8).collect();
        self.system_line(format!("running triggered turn (trace {short})"));
        self.follow = true;
        turn.fut = Some(self.kernel.continue_turn());
        turn.aborted = false;
        turn.prefix = "triggered turn: ";
        self.busy = true;
    }

    async fn paste_clipboard(&mut self) {
        match crate::clipboard_image::read_clipboard().await {
            Ok(crate::clipboard_image::ClipboardPaste::Image(image)) => {
                self.attach_clipboard_image(image);
            }
            Ok(crate::clipboard_image::ClipboardPaste::Text(text)) => {
                self.input.insert_str(&text);
                self.refresh_completions();
            }
            Ok(crate::clipboard_image::ClipboardPaste::Empty) => {
                self.system_line("clipboard is empty");
            }
            Err(e) => {
                self.error_line(format!("clipboard paste failed: {e}"));
            }
        }
    }

    fn attach_clipboard_image(&mut self, image: crate::clipboard_image::ClipboardImage) {
        if !self.current_model_accepts_images() {
            self.error_line("current model does not support image input; switch to a vision-capable model before pasting an image");
            return;
        }
        if self.pending_pasted_images.len() + self.pending_images.len()
            >= images::MAX_IMAGES_PER_MESSAGE
        {
            self.error_line(format!(
                "image attachment limit reached (max {} per message)",
                images::MAX_IMAGES_PER_MESSAGE
            ));
            return;
        }

        let size = human_bytes(image.encoded_bytes);
        let index = self.pending_pasted_images.len() + 1;
        let label = format!(
            "attached clipboard image #{index} ({}x{}, {size}); it will be sent with your next prompt",
            image.width, image.height
        );
        self.pending_pasted_images.push(image.image);
        self.system_line(label);
    }

    fn current_model_accepts_images(&self) -> bool {
        self.kernel.current_model_accepts_images()
    }

    fn validate_pending_image_support(&mut self) -> bool {
        let count = self.pending_images.len() + self.pending_pasted_images.len();
        if count == 0 || self.current_model_accepts_images() {
            return true;
        }
        self.error_line(format!(
            "current model does not support image input; switch to a vision-capable model before sending {count} image attachment(s)"
        ));
        false
    }

    fn queue_user_prompt(&mut self, display: String, prompt: String, images: Vec<ImageContent>) {
        self.enqueue_turn(QueuedTurn::UserPrompt {
            display,
            prompt,
            images,
        });
    }

    fn enqueue_turn(&mut self, job: QueuedTurn) {
        let preview = queue_preview(job.display());
        self.queued_turns.push_back(job);
        self.system_line(format!(
            "queued next message #{}: {preview}",
            self.queued_turns.len()
        ));
    }

    fn cancel_last_queued_turn(&mut self) {
        let Some(job) = self.queued_turns.pop_back() else {
            self.system_line("queue is empty");
            return;
        };
        let preview = queue_preview(job.display());
        self.system_line(format!("removed queued message: {preview}"));
    }

    fn start_next_queued_turn(&mut self, turn: &mut TurnState) -> bool {
        if turn.fut.is_some() {
            return true;
        }
        let Some(job) = self.queued_turns.pop_front() else {
            return false;
        };
        let remaining = self.queued_turns.len();
        self.system_line(if remaining == 0 {
            "running queued message".to_string()
        } else {
            format!("running queued message ({remaining} still queued)")
        });
        match job {
            QueuedTurn::UserPrompt {
                display,
                prompt,
                images,
            } => {
                self.feed.push_user(display);
                self.start_user_prompt_turn(prompt, images, turn);
            }
            QueuedTurn::AgentPrompt {
                display,
                prompt,
                error_context,
            } => {
                self.feed.push_user(display);
                self.start_prompt_turn(prompt, error_context, turn);
            }
            QueuedTurn::PromptTemplate {
                display,
                name,
                vars,
            } => {
                self.feed.push_user(display);
                self.start_template_turn(name, vars, turn);
            }
            QueuedTurn::Compaction { display, custom } => {
                self.feed.push_user(display);
                self.start_compaction_turn(custom, turn);
            }
        }
        true
    }

    async fn refresh_goal_state(&mut self) {
        self.latest_goal = crate::goal::current(self.kernel.harness()).await;
    }

    async fn dispatch_slash(
        &mut self,
        input: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        turn: &mut TurnState,
    ) {
        let outcome = {
            let ctx = CommandCtx {
                harness: self.kernel.harness(),
                session_id: &self.session_id,
                log_path: self.log_path.as_ref(),
                tool_count: self.tool_count,
                cwd: &self.cwd,
            };
            commands::dispatch(input, &self.registry, &ctx).await
        };
        match outcome {
            CommandOutcome::Quit => self.quit = true,
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
            CommandOutcome::WebRelay(action) => self.handle_web_relay(action).await,
            CommandOutcome::LoginSecret {
                provider,
                storage_key,
                recovery_command: _,
            } => {
                self.login(&provider, storage_key.as_deref(), terminal)
                    .await;
            }
            CommandOutcome::Handled => {}
        }
        if input.trim_start().starts_with("/goal") {
            self.refresh_goal_state().await;
        }
    }

    fn start_prompt_turn(
        &mut self,
        prompt: String,
        error_context: &'static str,
        turn: &mut TurnState,
    ) {
        turn.fut = Some(self.kernel.prompt_turn(prompt));
        turn.aborted = false;
        turn.prefix = error_context;
        self.busy = true;
    }

    fn start_user_prompt_turn(
        &mut self,
        prompt_text: String,
        loaded_images: Vec<ImageContent>,
        turn: &mut TurnState,
    ) {
        turn.fut = Some(self.kernel.user_prompt_turn(prompt_text, loaded_images));
        turn.aborted = false;
        turn.prefix = "";
        self.busy = true;
    }

    fn start_template_turn(
        &mut self,
        name: String,
        vars: serde_json::Map<String, serde_json::Value>,
        turn: &mut TurnState,
    ) {
        turn.fut = Some(self.kernel.template_turn(name, vars));
        turn.aborted = false;
        turn.prefix = "template run failed: ";
        self.busy = true;
    }

    fn start_compaction_turn(&mut self, custom: Option<String>, turn: &mut TurnState) {
        turn.fut = Some(self.kernel.compaction_turn(custom));
        turn.aborted = false;
        turn.prefix = "compaction failed: ";
        self.busy = true;
    }

    async fn login(
        &mut self,
        provider: &str,
        storage_key: Option<&str>,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) {
        // rpassword needs a cooked terminal with echo control, so drop out of the full-screen
        // UI for the prompt, then restore.
        leave_tui().ok();
        let result = crate::prompt_for_api_key(provider).await;
        let _ = enter_tui();
        let _ = terminal.clear();
        match result {
            Ok(token) if token.trim().is_empty() => {
                self.error_line("empty api key; login cancelled")
            }
            Ok(token) => match commands::save_api_key(storage_key.unwrap_or(provider), &token) {
                Ok(path) => self.system_line(format!(
                    "saved api key for `{provider}` to {}",
                    path.display()
                )),
                Err(e) => self.error_line(e),
            },
            Err(e) => self.error_line(e.to_string()),
        }
    }

    fn request_abort(&mut self, turn: &mut TurnState) {
        if turn.fut.is_some() {
            turn.aborted = true;
            self.kernel.abort();
            self.system_line("aborting current turn…");
        }
    }

    fn handle_ctrl_d(&mut self, turn: &mut TurnState) -> bool {
        if turn.fut.is_some() {
            self.request_abort(turn);
            true
        } else {
            false
        }
    }

    fn on_idle_ctrlc(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_ctrlc
            .map(|t| now.duration_since(t) < Duration::from_millis(1500))
            .unwrap_or(false)
        {
            return true;
        }
        self.last_ctrlc = Some(now);
        self.system_line("press Ctrl-C again within 1.5s to exit, or type /quit");
        false
    }

    // ── input helpers ───────────────────────────────────────────────────────────────────

    fn input_text(&self) -> String {
        self.input.lines().join("\n")
    }

    fn input_is_single_line(&self) -> bool {
        self.input.lines().len() <= 1
    }

    fn clear_input(&mut self) {
        self.input = new_textarea();
        self.completions.clear();
        self.completion_idx = 0;
    }

    fn set_input(&mut self, text: &str) {
        let mut input = new_textarea();
        input.insert_str(text);
        self.input = input;
        self.refresh_completions();
    }

    fn refresh_completions(&mut self) {
        self.completer = SlashCompleter::from_registry_and_skills(
            &self.registry,
            &self.kernel.harness().skills(),
        );
        self.completions = if self.input_is_single_line() {
            self.completer.matches(&self.input_text())
        } else {
            Vec::new()
        };
        self.completion_idx = 0;
    }

    fn cycle_completion(&mut self) {
        if self.completions.is_empty() {
            return;
        }
        let options = self.completions.clone();
        let pick = self.completions[self.completion_idx % self.completions.len()].clone();
        self.completion_idx = (self.completion_idx + 1) % self.completions.len();
        // Replace just the slash token (the whole single-line input here).
        let mut input = new_textarea();
        input.insert_str(&pick);
        self.input = input;
        if options.len() > 1 {
            // Keep the original candidate set so repeated Tab cycles through visible choices.
            self.completions = options;
        } else {
            self.completions.clear();
            self.completion_idx = 0;
        }
    }

    fn history_prev(&mut self) {
        let entries = self.history.entries();
        if entries.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.draft = self.input_text();
                entries.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        let text = entries[idx].clone();
        self.set_input(&text);
    }

    fn history_next(&mut self) {
        let Some(idx) = self.history_idx else {
            return;
        };
        let entries = self.history.entries();
        if idx + 1 < entries.len() {
            let text = entries[idx + 1].clone();
            self.history_idx = Some(idx + 1);
            self.set_input(&text);
        } else {
            self.history_idx = None;
            let draft = self.draft.clone();
            self.set_input(&draft);
        }
    }

    fn scroll_up(&mut self, n: usize) {
        self.follow = false;
        self.scroll = self.scroll.saturating_sub(n);
    }

    fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_add(n);
        // render() clamps and re-enables follow when we reach the bottom.
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, up: bool) {
        if !self.mouse_in_feed(column, row) {
            return;
        }
        if up {
            self.scroll_up(SCROLL_STEP);
        } else {
            self.scroll_down(SCROLL_STEP);
        }
    }

    fn mouse_in_feed(&self, column: u16, row: u16) -> bool {
        let Some(area) = self.last_feed_area else {
            return false;
        };
        column >= area.x
            && column < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height)
    }

    // ── rendering ───────────────────────────────────────────────────────────────────────

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let input_rows = self.input.lines().len().clamp(1, MAX_INPUT_ROWS) as u16;
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),              // status separator
            Constraint::Length(input_rows + 2), // input box with border
            Constraint::Length(1),              // hint line
        ])
        .split(area);
        let content_area = chunks[0];
        let status_area = chunks[1];
        let input_area = chunks[2];
        let hint_area = chunks[3];
        let (feed_area, trigger_area) = if content_area.width >= TRIGGER_PANEL_MIN_TOTAL_WIDTH
            && self.should_show_side_panel()
        {
            let cols =
                Layout::horizontal([Constraint::Min(40), Constraint::Length(TRIGGER_PANEL_WIDTH)])
                    .split(content_area);
            (cols[0], Some(cols[1]))
        } else {
            (content_area, None)
        };
        self.last_feed_area = Some(feed_area);

        // Feed (pre-wrapped to width so scroll math is exact).
        let lines = self.feed.lines(feed_area.width as usize);
        let total = lines.len();
        let viewport = feed_area.height as usize;
        self.last_viewport_h = viewport;
        let max_scroll = total.saturating_sub(viewport);
        if self.follow {
            self.scroll = max_scroll;
        } else {
            self.scroll = self.scroll.min(max_scroll);
            if self.scroll >= max_scroll {
                self.follow = true;
            }
        }
        let feed = Paragraph::new(lines).scroll((self.scroll as u16, 0));
        frame.render_widget(feed, feed_area);
        if let Some(area) = trigger_area {
            self.render_trigger_panel(frame, area);
        }

        // Status separator: rule + model + run state.
        frame.render_widget(
            self.status_line(status_area.width as usize, max_scroll),
            status_area,
        );

        // Input box. Keep the cursor away from the terminal edge and reserve a visible prompt
        // column so the typing surface feels intentional instead of cramped.
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = input_block.inner(input_area);
        frame.render_widget(input_block, input_area);
        if inner.width > 0 && inner.height > 0 {
            let prompt_width = inner.width.min(2);
            let prompt_area = Rect {
                x: inner.x,
                y: inner.y,
                width: prompt_width,
                height: inner.height,
            };
            frame.render_widget(
                Paragraph::new(Line::styled("> ", Style::default().fg(Color::Cyan))),
                prompt_area,
            );
            let text_area = Rect {
                x: inner.x + prompt_width,
                y: inner.y,
                width: inner.width.saturating_sub(prompt_width),
                height: inner.height,
            };
            if text_area.width > 0 {
                frame.render_widget(&self.input, text_area);
            }
        }

        // Hint line.
        let hint = if self.busy {
            "Enter queue next · Ctrl-V paste · Alt+Enter newline · Ctrl-C abort current · empty Ctrl-U removes queued"
        } else {
            "Enter send · Ctrl-V paste · Alt+Enter newline · ↑↓ history · Wheel/PgUp scroll · Ctrl-C abort"
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                feed::truncate_chars(hint, hint_area.width as usize),
                Style::default().fg(Color::DarkGray),
            )),
            hint_area,
        );

        // Completion popup, drawn above the input over the feed.
        self.render_completions(frame, status_area);
        self.render_control_plane_prompt(frame);
    }

    fn render_control_plane_prompt(&self, frame: &mut ratatui::Frame) {
        let Some(prompt) = self.control_plane_prompt.as_ref() else {
            return;
        };
        let area = frame.area();
        let width = area.width.clamp(40, 78);
        let height = area.height.clamp(8, 14);
        let rect = centered_rect(area, width, height);
        let request = &prompt.request;
        let text = vec![
            Line::styled(
                "Control-plane approval required",
                Style::default().fg(Color::Yellow),
            ),
            Line::raw(""),
            Line::raw(format!(
                "Action: {}",
                safe_control_prompt_label(&request.label)
            )),
            Line::raw(format!(
                "Tool: {}",
                safe_control_prompt_text(&request.tool_name, 80)
            )),
            Line::raw(format!(
                "Reason: {}",
                safe_control_prompt_text(&request.reason, CONTROL_PROMPT_TEXT_WIDTH)
            )),
            Line::raw(format!(
                "Args hash: {}",
                request.args_hash.chars().take(12).collect::<String>()
            )),
            Line::raw(format!(
                "Preview: {}",
                safe_control_prompt_payload(&request.payload, CONTROL_PROMPT_TEXT_WIDTH)
            )),
            Line::raw(""),
            Line::styled(
                "Enter/Y approve · N/D/Esc/Ctrl-C deny",
                Style::default().fg(Color::Cyan),
            ),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Confirm ")
            .border_style(Style::default().fg(Color::Yellow));
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
            rect,
        );
    }

    fn render_trigger_panel(&self, frame: &mut ratatui::Frame, area: Rect) {
        let lines =
            self.trigger_panel_lines(area.width.saturating_sub(2) as usize, area.height as usize);
        let panel = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::LEFT)
                .padding(Padding::left(1))
                .title(" Automation ")
                .border_style(Style::default().fg(Color::DarkGray))
                .title_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(panel, area);
    }

    fn should_show_side_panel(&self) -> bool {
        !self.kernel.harness().skills().is_empty()
            || !crate::triggers::global_registry().list().is_empty()
            || !crate::triggers::global_cron_registry().list().is_empty()
            || self.latest_trigger_poll.is_some()
            || self.latest_goal.is_some()
            || self.panel_status.mcp_servers > 0
            || self.panel_status.mcp_notification_hooks > 0
    }

    fn trigger_panel_lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let rules = crate::triggers::global_registry().list();
        let cron_jobs = crate::triggers::global_cron_registry().list();

        let mut lines = Vec::new();
        let skills = self.kernel.harness().skills();
        lines.push(panel_line("Skills".to_string(), Color::Cyan, width));
        if skills.is_empty() {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            let disabled = skills
                .iter()
                .filter(|skill| skill.disable_model_invocation)
                .count();
            let enabled = skills.len().saturating_sub(disabled);
            lines.push(panel_line(
                format!("enabled {enabled} · disabled {disabled}"),
                if disabled == 0 {
                    Color::Green
                } else {
                    Color::Yellow
                },
                width,
            ));
            let source_count =
                |source| skills.iter().filter(|skill| skill.source == source).count();
            lines.push(panel_line(
                format!(
                    "builtin {} · user {} · project {}",
                    source_count(SkillSource::Builtin),
                    source_count(SkillSource::User),
                    source_count(SkillSource::Project)
                ),
                Color::DarkGray,
                width,
            ));
        }

        lines.push(Line::raw(""));
        lines.push(panel_line("Triggers".to_string(), Color::Cyan, width));
        if rules.is_empty() {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            for rule in rules.iter().take(TRIGGER_PANEL_RULE_LIMIT) {
                let state_flag = if rule.enabled { "enabled" } else { "disabled" };
                let mode = if rule.fire_once { "once" } else { "repeat" };
                let id = feed::truncate_chars(&rule.id, 12);
                let color = if rule.enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                };
                lines.push(panel_line(
                    format!("{id} [{state_flag}, {mode}]"),
                    color,
                    width,
                ));
                lines.push(panel_line(
                    format!("  when {}", panel_rule_preview(&rule.condition, width)),
                    Color::DarkGray,
                    width,
                ));
                lines.push(panel_line(
                    format!("  do   {}", panel_rule_preview(&rule.action, width)),
                    Color::DarkGray,
                    width,
                ));
            }
            if rules.len() > TRIGGER_PANEL_RULE_LIMIT {
                lines.push(panel_line(
                    format!("… {} more", rules.len() - TRIGGER_PANEL_RULE_LIMIT),
                    Color::DarkGray,
                    width,
                ));
            }
        }

        if let Some(status) = &self.latest_trigger_poll {
            lines.push(Line::raw(""));
            lines.push(panel_line("Polling".to_string(), Color::Cyan, width));
            lines.push(panel_line(
                format!("{} · no match", status.checked_at),
                Color::Yellow,
                width,
            ));
            lines.push(panel_line(
                format!(
                    "{} / {}",
                    panel_rule_preview(&status.source_label, width),
                    panel_rule_preview(&status.event_label, width)
                ),
                Color::DarkGray,
                width,
            ));
            lines.push(panel_line(
                format!("trace {}", panel_rule_preview(&status.trace_id, width)),
                Color::DarkGray,
                width,
            ));
            lines.push(panel_line(
                format!("  {}", panel_rule_preview(&status.summary, width)),
                Color::DarkGray,
                width,
            ));
        }

        if let Some(goal) = &self.latest_goal {
            lines.push(Line::raw(""));
            lines.push(panel_line("Goal".to_string(), Color::Cyan, width));
            let color = match goal.status {
                crate::goal::GoalStatus::Pursuing => Color::Yellow,
                crate::goal::GoalStatus::Achieved => Color::Green,
                crate::goal::GoalStatus::Paused | crate::goal::GoalStatus::BudgetLimited => {
                    Color::DarkGray
                }
                crate::goal::GoalStatus::Cleared => Color::DarkGray,
            };
            lines.push(panel_line(goal.status.as_str().to_string(), color, width));
            lines.push(panel_line(
                panel_rule_preview(&goal.condition, width),
                Color::DarkGray,
                width,
            ));
            if goal.iterations > 0 {
                lines.push(panel_line(
                    format!("checks {}", goal.iterations),
                    Color::DarkGray,
                    width,
                ));
            }
            if let Some(reason) = goal.last_reason.as_deref() {
                lines.push(panel_line(
                    format!("  {}", panel_rule_preview(reason, width)),
                    Color::DarkGray,
                    width,
                ));
            }
        }

        lines.push(Line::raw(""));
        lines.push(panel_line("Cron (session)".to_string(), Color::Cyan, width));
        if cron_jobs.is_empty() {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            let enabled = cron_jobs.iter().filter(|job| job.enabled).count();
            let disabled = cron_jobs.len().saturating_sub(enabled);
            lines.push(panel_line(
                format!("enabled {enabled} · disabled {disabled}"),
                if disabled == 0 {
                    Color::Green
                } else {
                    Color::Yellow
                },
                width,
            ));
            for job in cron_jobs.iter().take(TRIGGER_PANEL_RULE_LIMIT) {
                let state_flag = if job.enabled { "enabled" } else { "disabled" };
                let id = feed::truncate_chars(&job.id, 12);
                let color = if job.enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                };
                lines.push(panel_line(
                    format!("{id} [{state_flag}] {}", job.schedule),
                    color,
                    width,
                ));
                lines.push(panel_line(
                    format!("  do {}", panel_rule_preview(&job.action, width)),
                    Color::DarkGray,
                    width,
                ));
                if job.skipped_overlap_count > 0 {
                    lines.push(panel_line(
                        format!("  skipped overlaps {}", job.skipped_overlap_count),
                        Color::Yellow,
                        width,
                    ));
                }
            }
            if cron_jobs.len() > TRIGGER_PANEL_RULE_LIMIT {
                lines.push(panel_line(
                    format!("… {} more", cron_jobs.len() - TRIGGER_PANEL_RULE_LIMIT),
                    Color::DarkGray,
                    width,
                ));
            }
        }

        let hook_rows = self.panel_status.hook_points.len().max(1);
        let feature_rows = self.panel_status.trigger_features.len().max(1);
        // Skills + Triggers are variable above. Reserve enough rows for the lower static status
        // sections so MCP/Hooks/Runtime don't get clipped in ordinary tall terminals.
        let status_rows = 2 + 2 + 2 + hook_rows + 2 + feature_rows;
        while lines.len() + status_rows < height {
            lines.push(Line::raw(""));
        }

        lines.push(Line::raw(""));
        lines.push(panel_line("MCP".to_string(), Color::Cyan, width));
        if self.panel_status.mcp_servers == 0 {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            lines.push(panel_line(
                format!(
                    "servers {} · tools {}",
                    self.panel_status.mcp_servers, self.panel_status.mcp_tools
                ),
                Color::Green,
                width,
            ));
            lines.push(panel_line(
                format!(
                    "notification hooks {}",
                    self.panel_status.mcp_notification_hooks
                ),
                Color::DarkGray,
                width,
            ));
        }

        lines.push(Line::raw(""));
        lines.push(panel_line("Hooks".to_string(), Color::Cyan, width));
        if self.panel_status.hook_points.is_empty() {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            for point in &self.panel_status.hook_points {
                lines.push(panel_line(format!("· {point}"), Color::DarkGray, width));
            }
        }

        lines.push(Line::raw(""));
        lines.push(panel_line("Runtime".to_string(), Color::Cyan, width));
        if self.panel_status.trigger_features.is_empty() {
            lines.push(panel_line("none".to_string(), Color::DarkGray, width));
        } else {
            for feature in &self.panel_status.trigger_features {
                lines.push(panel_line(format!("• {feature}"), Color::DarkGray, width));
            }
        }
        lines
    }

    fn status_line(&self, width: usize, max_scroll: usize) -> Paragraph<'static> {
        let model = {
            let state = self.kernel.harness().agent().state();
            state
                .model
                .as_ref()
                .map(|m| format!("{}:{}", m.provider.0, m.id))
                .unwrap_or_else(|| "no-model".into())
        };
        let queue = if self.queued_turns.is_empty() {
            String::new()
        } else {
            format!(" · {} queued", self.queued_turns.len())
        };
        let status = if self.busy {
            format!(
                "{} working (Ctrl-C aborts){queue}",
                SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()],
            )
        } else {
            format!("ready{queue}")
        };
        let scrolled = if self.follow { "" } else { " ↑scrolled" };
        let label = format!(" pie · {model} · {status}{scrolled} ");
        let mut text = label.clone();
        let used = unicode_width::UnicodeWidthStr::width(label.as_str());
        if width > used {
            text.push_str(&"─".repeat(width - used));
        }
        let _ = max_scroll;
        Paragraph::new(Line::styled(text, Style::default().fg(Color::DarkGray)))
    }

    fn render_completions(&self, frame: &mut ratatui::Frame, status_area: Rect) {
        if self.completions.is_empty() {
            return;
        }
        let shown = self.completions.len().min(COMPLETION_POPUP_MAX);
        let height = shown as u16 + 2; // borders
        let area = frame.area();
        let y = status_area.y.saturating_sub(height).max(area.y);
        let width = area.width.clamp(10, 60);
        let rect = Rect {
            x: area.x,
            y,
            width,
            height,
        };
        let items: Vec<ListItem> = self
            .completions
            .iter()
            .take(shown)
            .enumerate()
            .map(|(i, c)| {
                let selected = i == self.completion_idx % self.completions.len();
                let style = if selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                ListItem::new(Line::styled(c.clone(), style))
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("commands (Tab)")
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(list, rect);
    }

    // ── non-interactive fallback ──────────────────────────────────────────────────────────

    /// Line-based fallback for non-TTY stdin/stdout (e.g. `echo prompt | pie`). No fixed input
    /// box — just read prompts from stdin and stream feed updates to stdout.
    async fn run_headless(mut self) -> Result<()> {
        use tokio::io::{AsyncBufReadExt as _, BufReader};

        // Flush startup feed (banner/diagnostics) first.
        for line in self.feed.lines(100) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("{text}");
        }
        let _ = std::io::stdout().flush();

        // A background printer drains feed updates (agent stream + command output) to stdout.
        let mut feed_rx = self.feed_rx.take().expect("feed_rx");
        tokio::spawn(async move {
            let mut at_line_start = true;
            while let Some(update) = feed_rx.recv().await {
                print_headless_update(&update, &mut at_line_start);
            }
        });

        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let input = line.trim();
            if input.is_empty() {
                continue;
            }
            if input.starts_with('/') {
                let ctx = CommandCtx {
                    harness: self.kernel.harness(),
                    session_id: &self.session_id,
                    log_path: self.log_path.as_ref(),
                    tool_count: self.tool_count,
                    cwd: &self.cwd,
                };
                match commands::dispatch(input, &self.registry, &ctx).await {
                    CommandOutcome::Quit => break,
                    CommandOutcome::Error(e) => eprintln!("error: {e}"),
                    CommandOutcome::LoginSecret {
                        provider,
                        recovery_command,
                        ..
                    } => {
                        eprintln!(
                            "error: {}",
                            crate::login_requires_tty_message(
                                &provider,
                                recovery_command.as_deref()
                            )
                        );
                    }
                    CommandOutcome::RunAgentPrompt {
                        prompt,
                        error_context,
                    } => {
                        if let Err(e) = self.kernel.harness().prompt(prompt).await {
                            eprintln!("error: {error_context}{e}");
                        }
                    }
                    CommandOutcome::RunPromptTemplate { name, vars } => {
                        if let Err(e) = self
                            .kernel
                            .harness()
                            .prompt_from_template(&name, vars)
                            .await
                        {
                            eprintln!("error: template run failed: {e}");
                        }
                    }
                    CommandOutcome::RunCompaction { custom } => {
                        match self.kernel.harness().force_compact(custom).await {
                            Ok(true) => println!("compaction ran"),
                            Ok(false) => println!("nothing to compact"),
                            Err(e) => eprintln!("error: compaction failed: {e}"),
                        }
                    }
                    CommandOutcome::WebRelay(_) => {
                        eprintln!("error: /web-connect requires the interactive TUI or --web mode");
                    }
                    _ => {}
                }
                continue;
            }
            let (expanded, _) = mentions::expand(input, &self.cwd).await;
            let prompt = commands::attach_skill_prompt(expanded, None);
            if let Err(e) = self.kernel.user_prompt_turn(prompt, Vec::new()).await {
                eprintln!("error: {e}");
            }
        }
        Ok(())
    }
}

fn panel_line(text: String, color: Color, width: usize) -> Line<'static> {
    Line::styled(
        feed::truncate_chars(&text, width.max(1)),
        Style::default().fg(color),
    )
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn safe_control_prompt_label(text: &str) -> String {
    safe_control_prompt_text(text, 120)
}

fn safe_control_prompt_text(text: &str, cap: usize) -> String {
    let redaction_window = cap.max(1).saturating_mul(4).min(1024);
    let redacted = redact_control_prompt_secrets(&feed::truncate_chars(text, redaction_window));
    feed::truncate_chars(&redacted, cap.max(1)).replace('\n', " ")
}

fn safe_control_prompt_payload(value: &serde_json::Value, cap: usize) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    safe_control_prompt_text(&text, cap)
}
fn redact_control_prompt_secrets(text: &str) -> String {
    static TOKENISH_FIELD: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?i)(token|secret|password|api[_-]?key|authorization|cookie)(["'=:\s]+)([^"',\s&}]+)"#,
        )
        .expect("control prompt redaction regex must compile")
    });
    let redacted = crate::bug_report::redact(text);
    TOKENISH_FIELD
        .replace_all(&redacted, "$1$2[REDACTED]")
        .into_owned()
}

fn panel_rule_preview(text: &str, width: usize) -> String {
    let redacted = crate::bug_report::redact(text).replace('\n', " ");
    feed::truncate_chars(&redacted, width.max(1))
}

fn queue_preview(text: &str) -> String {
    let redacted = crate::bug_report::redact(text).replace('\n', " ");
    feed::truncate_chars(&redacted, QUEUED_PREVIEW_CHARS)
}

fn prompt_display(text: &str, image_count: usize) -> String {
    if image_count == 0 {
        return text.to_string();
    }
    let suffix = image_attachment_display(image_count);
    if text.is_empty() {
        suffix
    } else {
        format!("{text}\n{suffix}")
    }
}

fn image_attachment_display(image_count: usize) -> String {
    match image_count {
        1 => "[1 image attachment]".to_string(),
        n => format!("[{n} image attachments]"),
    }
}

fn human_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn user_facing_run_error(error: &str) -> String {
    let Some(rest) = error.strip_prefix("no API key for provider: ") else {
        return error.to_string();
    };
    let provider = rest.split(';').next().unwrap_or(rest).trim();
    if provider.is_empty() {
        return error.to_string();
    }
    let vars = pie_ai::env_api_keys::env_var_names(provider);
    let credential_hint = if vars.is_empty() {
        "configure a provider-specific credential".to_string()
    } else {
        format!("set {}", vars.join(" or "))
    };
    format!("no API key for provider: {provider}; run /login {provider} or {credential_hint}")
}

fn new_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::default());
    textarea.set_placeholder_text("type a message, or /help");
    textarea
}

fn enter_tui() -> Result<()> {
    enable_raw_mode()?;
    write_enter_tui_commands(&mut std::io::stdout())?;
    Ok(())
}

fn leave_tui() -> Result<()> {
    write_leave_tui_commands(&mut std::io::stdout())?;
    disable_raw_mode()?;
    Ok(())
}

fn write_enter_tui_commands(out: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )
}

fn write_leave_tui_commands(out: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
}

fn print_headless_update(update: &FeedUpdate, at_line_start: &mut bool) {
    let mut out = std::io::stdout();
    match update {
        FeedUpdate::TextDelta(delta) => {
            let _ = write!(out, "{delta}");
            *at_line_start = delta.ends_with('\n');
        }
        FeedUpdate::ThinkingDelta(_) => {}
        FeedUpdate::ToolStart { name, args } => {
            if !*at_line_start {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "⚙ {name}{args}");
            *at_line_start = true;
        }
        FeedUpdate::ToolProgress { .. } => {}
        FeedUpdate::ToolEnd { lines, .. } => {
            for line in lines {
                let _ = writeln!(out, "    {line}");
            }
            *at_line_start = true;
        }
        FeedUpdate::Plain { text, .. } => {
            if !*at_line_start {
                let _ = writeln!(out);
            }
            let _ = writeln!(out, "{text}");
            *at_line_start = true;
        }
        FeedUpdate::TriggerPollStatus(_) => {}
        FeedUpdate::SkillsReloaded { .. } => {}
        FeedUpdate::TurnStart => {}
        FeedUpdate::TurnEnd => {
            if !*at_line_start {
                let _ = writeln!(out);
                *at_line_start = true;
            }
        }
    }
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_agent_core::{AgentHarnessOptions, MemorySessionStorage, Session, SessionStorage};
    use pie_ai::{ToolResultMessage, ToolResultRole};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use tokio_util::sync::CancellationToken;

    fn faux_model() -> pie_ai::Model {
        pie_ai::Model {
            id: "faux".into(),
            name: "Faux".into(),
            api: pie_ai::Api::from("faux"),
            provider: pie_ai::Provider::from("faux"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pie_ai::ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
            compat: None,
        }
    }

    fn faux_vision_model() -> pie_ai::Model {
        let mut model = faux_model();
        model.input = vec![pie_ai::InputModality::Text, pie_ai::InputModality::Image];
        model
    }

    static TRIGGER_REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_app() -> App {
        test_app_with_model(faux_model())
    }

    fn test_app_with_model(model: pie_ai::Model) -> App {
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage as Arc<dyn SessionStorage>);
        let opts = AgentHarnessOptions::new(model, session);
        test_app_with_options(opts)
    }

    fn test_app_with_options(opts: AgentHarnessOptions) -> App {
        let harness = Arc::new(AgentHarness::new(opts));
        let (_ftx, feed_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_mtx, main_run_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(AppConfig {
            harness,
            retry: RetrySettings::default(),
            registry: Registry::with_builtins(),
            cwd: std::path::PathBuf::from("."),
            session_id: "test".into(),
            log_path: None,
            tool_count: 0,
            history: HistoryStore::load_from(std::path::Path::new("/nonexistent-pie-history")),
            pending_images: vec![],
            feed_rx,
            main_run_rx,
            control_plane_prompt_rx: None,
            panel_status: PanelStatus::default(),
        })
    }

    fn ui_skill(
        name: &str,
        source: pie_agent_core::SkillSource,
        disabled: bool,
    ) -> pie_agent_core::Skill {
        pie_agent_core::Skill {
            name: name.into(),
            description: format!("description for {name}"),
            file_path: format!("/tmp/{name}/SKILL.md"),
            content: "SECRET SKILL BODY".into(),
            disable_model_invocation: disabled,
            source,
        }
    }

    fn one_pixel_clipboard_image() -> crate::clipboard_image::ClipboardImage {
        crate::clipboard_image::encode_rgba_clipboard_image(1, 1, vec![255, 0, 0, 255]).unwrap()
    }

    fn buffer_text(buf: &Buffer) -> String {
        let area = *buf.area();
        let mut rows = Vec::new();
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            rows.push(row.trim_end().to_string());
        }
        rows.join("\n")
    }

    fn feed_text(app: &App) -> String {
        app.feed
            .lines(100)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn feed_lines_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point of the refactor: the input box is pinned at the bottom with the
    /// conversation feed scrolling above it. Render to an off-screen backend and assert the
    /// spatial layout — feed content near the top, the status rule + input box at the bottom.
    #[test]
    fn renders_feed_above_pinned_input_box() {
        let mut app = test_app();
        app.feed.push_user("hello world");
        app.feed.push_assistant("hi there, the box is pinned");

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        let lines: Vec<&str> = text.lines().collect();

        // Feed content rendered (somewhere in the upper region).
        assert!(
            text.contains("you ▸ hello world"),
            "feed user line missing:\n{text}"
        );
        let user_row = text.lines().find(|line| line.contains("you ▸ hello world"));
        let user_row = user_row.expect("feed user row");
        assert_full_timestamp_prefix(user_row, &text);
        assert!(
            text.contains("ai ▸ hi there, the box is pinned"),
            "assistant line missing:\n{text}"
        );
        // The status rule (separator above the input) carries the model + ready state.
        assert!(text.contains("pie ·"), "status rule missing:\n{text}");
        assert!(
            text.contains("ready"),
            "status should read ready when idle:\n{text}"
        );
        // The status rule and the hint line live in the bottom five rows — the bordered input
        // box is between them, pinned to the bottom.
        let status_row = lines.iter().position(|l| l.contains("pie ·")).unwrap();
        assert!(
            status_row >= lines.len() - 5,
            "status rule should be pinned near the bottom (row {status_row} of {}):\n{text}",
            lines.len()
        );
    }

    #[test]
    fn input_box_has_breathing_room_and_prompt() {
        let mut app = test_app();
        let backend = TestBackend::new(50, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("┌"), "input border missing:\n{text}");
        assert!(text.contains("└"), "input border missing:\n{text}");
        assert!(
            text.contains("│>  type a message, or /help"),
            "input should have a prompt and horizontal padding:\n{text}"
        );
        assert!(
            text.contains("Ctrl-V paste"),
            "idle hint should advertise paste support:\n{text}"
        );
    }

    #[test]
    fn wide_layout_renders_trigger_panel() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        app.panel_status = PanelStatus {
            mcp_servers: 1,
            mcp_tools: 2,
            mcp_server_names: Vec::new(),
            mcp_tool_names: Vec::new(),
            tool_names: Vec::new(),
            mcp_notification_hooks: 1,
            hook_points: vec!["before_tool_call".into(), "after_tool_call".into()],
            trigger_features: vec!["dedup".into(), "cycle suppress".into()],
        };
        crate::triggers::global_registry()
            .add_rule("a build finishes", "summarize the result")
            .unwrap();

        // Tall enough that all three sections (MCP / Hooks / Runtime) clear the
        // right-rail clip — see `trigger_panel_lines`'s `status_rows` budget. The Trigger
        // runtime bullets render at the bottom of the panel; a 20-row buffer cuts them off.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Automation"), "panel title missing:\n{text}");
        assert!(text.contains("Triggers"), "trigger list missing:\n{text}");
        assert!(
            text.contains("[enabled, once]"),
            "trigger flags missing:\n{text}"
        );
        assert!(
            text.contains("when a build finishes"),
            "rule condition missing:\n{text}"
        );
        assert!(text.contains("MCP"), "mcp section missing:\n{text}");
        assert!(
            text.contains("servers 1 · tools 2"),
            "mcp status missing:\n{text}"
        );
        assert!(
            text.contains("notification hooks 1"),
            "renamed mcp notification-hook label missing:\n{text}"
        );
        assert!(text.contains("Hooks"), "hooks section missing:\n{text}");
        assert!(
            text.contains("before_tool_call"),
            "hook point status missing:\n{text}"
        );
        assert!(
            !text.contains("✓ before_tool_call"),
            "hook point rows should not use success checkmarks:\n{text}"
        );
        // Runtime features render as their own section, separate from `Hooks`, so users
        // can't mistake `dedup` / `cycle suppress` etc. for pluggable callbacks.
        assert!(
            text.contains("Runtime"),
            "runtime feature section title missing:\n{text}"
        );
        assert!(
            text.contains("dedup"),
            "trigger-runtime feature label missing:\n{text}"
        );
    }

    #[test]
    fn wide_layout_hides_empty_static_trigger_panel() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        app.panel_status = PanelStatus {
            mcp_servers: 0,
            mcp_tools: 0,
            mcp_server_names: Vec::new(),
            mcp_tool_names: Vec::new(),
            tool_names: Vec::new(),
            mcp_notification_hooks: 0,
            hook_points: vec!["before_tool_call".into(), "after_tool_call".into()],
            trigger_features: vec!["dedup".into(), "cycle suppress".into()],
        };

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(
            !text.contains("Triggers"),
            "empty static trigger panel should not consume width:\n{text}"
        );
        assert!(
            !text.contains("before_tool_call"),
            "static hook rows belong in /triggers status, not the default side panel:\n{text}"
        );
        assert!(
            !text.contains("cycle suppress"),
            "static trigger-runtime rows belong in /triggers status, not the default side panel:\n{text}"
        );
    }

    #[test]
    fn wide_layout_shows_latest_poll_status_without_feed_line() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        app.latest_trigger_poll = Some(TriggerPollStatus {
            checked_at: "11:22:33".into(),
            trace_id: "trace-chrome-check".into(),
            source_label: "local:dynamic".into(),
            event_label: "dynamic periodic check".into(),
            summary: "Checked Chrome tabs; no matching rule found.".into(),
        });

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Automation"), "panel title missing:\n{text}");
        assert!(text.contains("Polling"), "polling section missing:\n{text}");
        assert!(
            text.contains("11:22:33 · no match"),
            "poll status missing:\n{text}"
        );
        assert!(
            text.contains("local:dynamic / dynamic periodic c"),
            "poll source missing:\n{text}"
        );
        assert!(
            text.contains("trace trace-chrome-check"),
            "poll trace missing:\n{text}"
        );
        assert!(
            text.contains("no matching"),
            "poll summary missing:\n{text}"
        );
        assert!(
            !text.contains("[trigger completed]"),
            "poll status should not be appended to the feed:\n{text}"
        );
    }

    #[test]
    fn wide_layout_renders_compact_skills_panel_summary() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        app.kernel.harness().replace_skills(vec![
            ui_skill(
                "builtin-review",
                pie_agent_core::SkillSource::Builtin,
                false,
            ),
            ui_skill("user-format", pie_agent_core::SkillSource::User, true),
            ui_skill("project-plan", pie_agent_core::SkillSource::Project, false),
        ]);

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Automation"), "panel title missing:\n{text}");
        assert!(text.contains("Skills"), "skills section missing:\n{text}");
        assert!(
            text.contains("enabled 2 · disabled 1"),
            "skills enabled/disabled summary missing:\n{text}"
        );
        assert!(
            text.contains("builtin 1 · user 1 · project 1"),
            "skills source summary missing:\n{text}"
        );
        assert!(
            !text.contains("SECRET SKILL BODY"),
            "skills panel must not render skill body:\n{text}"
        );
    }

    #[test]
    fn trigger_panel_redacts_rule_preview_secrets() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        let secret = "sk-panel-secret-should-not-render-1234567890";
        crate::triggers::global_registry()
            .add_rule(
                &format!("when header is Bearer {secret}"),
                &format!("call API with {secret}"),
            )
            .unwrap();

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(
            !text.contains(secret),
            "trigger panel leaked secret:\n{text}"
        );
        assert!(
            text.contains("[REDACTED:"),
            "trigger panel should show redaction marker:\n{text}"
        );
    }

    #[test]
    fn narrow_layout_hides_trigger_panel() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        crate::triggers::global_registry()
            .add_rule("a build finishes", "summarize the result")
            .unwrap();

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(
            !text.contains("Triggers"),
            "trigger panel should be hidden on narrow terminals:\n{text}"
        );
    }

    #[test]
    fn wide_layout_renders_cron_jobs_in_automation_panel() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        crate::triggers::global_cron_registry().clear_for_tests();
        let mut app = test_app();
        let secret = "sk-cron-panel-secret-12345678901234567890";
        crate::triggers::global_cron_registry()
            .add_job("*/10 * * * *", &format!("call API with {secret}"))
            .unwrap();

        let backend = TestBackend::new(120, 26);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("Automation"), "panel title missing:\n{text}");
        assert!(
            text.contains("Cron (session)"),
            "cron scope label missing:\n{text}"
        );
        assert!(
            text.contains("enabled 1 · disabled 0"),
            "cron count summary missing:\n{text}"
        );
        assert!(
            text.contains("*/10 * * *"),
            "cron schedule missing:\n{text}"
        );
        assert!(!text.contains(secret), "cron panel leaked secret:\n{text}");
        assert!(
            text.contains("[REDACTED:"),
            "cron panel should show redaction marker:\n{text}"
        );
    }

    #[test]
    fn tab_cycles_slash_command_completions() {
        let mut app = test_app();
        app.set_input("/");
        let options = app.completions.clone();
        assert!(
            options.len() > 1,
            "slash prefix should expose multiple command completions"
        );

        app.cycle_completion();
        let first = app.input_text();
        app.cycle_completion();
        let second = app.input_text();

        assert_eq!(first, options[0]);
        assert_eq!(second, options[1]);
        assert_ne!(first, second);
    }

    #[test]
    fn tab_completion_includes_hot_loaded_skill_commands() {
        let mut app = test_app();
        app.kernel.harness().replace_skills(vec![ui_skill(
            "db9",
            pie_agent_core::SkillSource::User,
            false,
        )]);

        app.set_input("/d");

        assert!(
            app.completions.contains(&"/db9".to_string()),
            "skill command should be offered after hot catalog update: {:?}",
            app.completions
        );
    }

    #[test]
    fn ctrl_d_aborts_active_turn_before_exiting() {
        let mut app = test_app();
        let mut turn = TurnState {
            fut: Some(Box::pin(std::future::pending())),
            aborted: false,
            prefix: "",
        };

        assert!(app.handle_ctrl_d(&mut turn));

        assert!(turn.aborted);
        assert!(!app.quit, "Ctrl-D during work should abort, not exit");
    }

    #[test]
    fn mouse_wheel_scrolls_only_inside_feed_area() {
        let mut app = test_app();
        app.last_feed_area = Some(Rect::new(2, 1, 20, 6));
        app.scroll = 10;
        app.follow = true;

        app.handle_mouse_scroll(5, 3, true);
        assert_eq!(app.scroll, 7);
        assert!(!app.follow);

        app.handle_mouse_scroll(5, 8, true);
        assert_eq!(
            app.scroll, 7,
            "wheel events outside the feed should not move the conversation scroll"
        );

        app.handle_mouse_scroll(5, 3, false);
        assert_eq!(app.scroll, 10);
    }

    #[test]
    fn status_rule_shows_working_spinner_when_busy() {
        let mut app = test_app();
        app.busy = true;
        app.spinner_frame = 2;
        let backend = TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("working"),
            "busy status should say working:\n{text}"
        );
    }

    #[tokio::test]
    async fn finished_turn_starts_next_queued_prompt_fifo() {
        let mut app = test_app();
        let mut turn = TurnState {
            fut: Some(Box::pin(async {
                Ok::<Option<String>, AgentRunError>(None)
            })),
            aborted: false,
            prefix: "",
        };

        app.queue_user_prompt("next question".into(), "next question".into(), Vec::new());
        assert_eq!(app.queued_turns.len(), 1);

        app.finish_turn(&mut turn, Ok(None)).await;

        assert!(turn.fut.is_some(), "queued prompt should start immediately");
        assert!(app.busy, "starting queued prompt should mark UI busy");
        assert!(app.queued_turns.is_empty());
        let text = feed_text(&app);
        assert!(text.contains("queued next message #1: next question"));
        assert!(text.contains("running queued message"));
        assert!(text.contains("you ▸ next question"));
    }

    #[tokio::test]
    async fn finished_turn_refreshes_goal_panel_state() {
        let mut app = test_app();
        crate::goal::set(
            app.kernel.harness(),
            "run verification before stopping".into(),
        )
        .await
        .unwrap();
        let mut turn = TurnState {
            fut: Some(Box::pin(async {
                Ok::<Option<String>, AgentRunError>(None)
            })),
            aborted: false,
            prefix: "",
        };

        app.finish_turn(&mut turn, Ok(None)).await;

        let goal = app.latest_goal.as_ref().expect("goal state");
        assert_eq!(goal.condition, "run verification before stopping");
        assert_eq!(goal.status, crate::goal::GoalStatus::Pursuing);
        assert!(
            turn.fut.is_none(),
            "runtime OnTurnEndHook, not the UI, owns continuation"
        );
    }

    #[test]
    fn status_rule_shows_queued_count_while_busy() {
        let mut app = test_app();
        app.busy = true;
        app.queue_user_prompt("queued one".into(), "queued one".into(), Vec::new());

        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("working"), "{text}");
        assert!(text.contains("1 queued"), "{text}");
    }

    #[test]
    fn empty_ctrl_u_removes_last_queued_prompt_while_busy() {
        let mut app = test_app();
        let turn = TurnState {
            fut: Some(Box::pin(async {
                Ok::<Option<String>, AgentRunError>(None)
            })),
            aborted: false,
            prefix: "",
        };
        app.queue_user_prompt("first".into(), "first".into(), Vec::new());
        app.queue_user_prompt("second".into(), "second".into(), Vec::new());

        app.cancel_last_queued_turn();

        assert_eq!(app.queued_turns.len(), 1);
        assert_eq!(app.queued_turns[0].display(), "first");
        assert!(
            feed_text(&app).contains("removed queued message: second"),
            "feed should explain queue cancellation"
        );
        assert!(
            turn.fut.is_some(),
            "canceling queued item must not abort current turn"
        );
    }

    #[test]
    fn attaching_clipboard_image_requires_vision_model() {
        let mut app = test_app();
        app.attach_clipboard_image(one_pixel_clipboard_image());

        assert!(app.pending_pasted_images.is_empty());
        let text = feed_text(&app);
        assert!(
            text.contains("current model does not support image input"),
            "{text}"
        );
    }

    #[test]
    fn pending_path_image_requires_vision_model_without_dropping_attachment() {
        let mut app = test_app();
        app.pending_images
            .push(std::path::PathBuf::from("/tmp/screenshot.png"));

        assert!(!app.validate_pending_image_support());

        assert_eq!(app.pending_images.len(), 1);
        let text = feed_text(&app);
        assert!(
            text.contains("current model does not support image input"),
            "{text}"
        );
        assert!(
            text.contains("1 image attachment"),
            "error should mention pending attachment count: {text}"
        );
    }

    #[test]
    fn pending_path_image_allowed_for_vision_model() {
        let mut app = test_app_with_model(faux_vision_model());
        app.pending_images
            .push(std::path::PathBuf::from("/tmp/screenshot.png"));

        assert!(app.validate_pending_image_support());
        assert!(feed_text(&app).is_empty());
    }

    #[test]
    fn missing_api_key_error_uses_cli_recovery_action() {
        let text = user_facing_run_error(
            "no API key for provider: deepseek; set DEEPSEEK_API_KEY or pass options.api_key",
        );

        assert!(text.contains("/login deepseek"), "{text}");
        assert!(text.contains("DEEPSEEK_API_KEY"), "{text}");
        assert!(!text.contains("options.api_key"), "{text}");
    }

    #[test]
    fn attaching_clipboard_image_adds_pending_image_without_blob_echo() {
        let mut app = test_app_with_model(faux_vision_model());
        app.attach_clipboard_image(one_pixel_clipboard_image());

        assert_eq!(app.pending_pasted_images.len(), 1);
        let text = feed_text(&app);
        assert!(text.contains("attached clipboard image #1"), "{text}");
        assert!(text.contains("1x1"), "{text}");
        assert!(
            !text.contains(&app.pending_pasted_images[0].data),
            "clipboard image base64 leaked into feed"
        );
    }

    #[test]
    fn queued_prompt_carries_pending_clipboard_image() {
        let mut app = test_app_with_model(faux_vision_model());
        app.attach_clipboard_image(one_pixel_clipboard_image());
        let data = app.pending_pasted_images[0].data.clone();
        let images = std::mem::take(&mut app.pending_pasted_images);

        app.queue_user_prompt(
            "describe this image".into(),
            "describe this image".into(),
            images,
        );

        assert!(app.pending_pasted_images.is_empty());
        let Some(QueuedTurn::UserPrompt { images, .. }) = app.queued_turns.front() else {
            panic!("expected queued user prompt");
        };
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, data);
    }

    #[test]
    fn prompt_display_surfaces_image_only_attachment_without_blob() {
        assert_eq!(
            prompt_display("", 1),
            "[1 image attachment]",
            "image-only prompts need a visible feed label"
        );
        assert_eq!(
            prompt_display("describe this", 2),
            "describe this\n[2 image attachments]"
        );
    }

    #[test]
    fn queued_prompt_preview_redacts_token_like_text() {
        let mut app = test_app();
        app.queue_user_prompt(
            "use sk-abcdefghijklmnopqrstuvwxyz123456".into(),
            "use sk-abcdefghijklmnopqrstuvwxyz123456".into(),
            Vec::new(),
        );

        let text = feed_text(&app);
        assert!(text.contains("[REDACTED:openai_anthropic_key]"), "{text}");
        assert!(
            !text.contains("sk-abcdefghijklmnopqrstuvwxyz123456"),
            "{text}"
        );
    }

    #[test]
    fn control_plane_prompt_card_redacts_payload_and_denies_without_clearing_queue() {
        use pie_agent_core::ControlPlanePromptRequest;
        use tokio::sync::oneshot;

        let mut app = test_app();
        app.queue_user_prompt(
            "queued secret=should-redact".into(),
            "queued".into(),
            Vec::new(),
        );
        let (tx, mut rx) = oneshot::channel();
        app.show_control_plane_prompt(UiControlPlanePrompt {
            request: ControlPlanePromptRequest {
                tool_call_id: "call_1".into(),
                tool_name: "InstallSkill".into(),
                args_hash: "abcdef1234567890".repeat(4),
                label: "Install https://example.com/?token=SECRET_TOKEN".into(),
                payload: serde_json::json!({
                    "url": "https://example.com/?token=SECRET_TOKEN",
                    "args_hash": "abcdef1234567890"
                }),
                reason: "writes skill files with password=SECRET_TOKEN".into(),
            },
            responder: tx,
        });

        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Control-plane approval required"));
        assert!(
            !rendered.contains("SECRET_TOKEN"),
            "prompt card must redact token-like payloads:\n{rendered}"
        );
        assert!(
            app.handle_control_plane_prompt_key(&KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::empty()
            ))
        );
        assert!(app.control_plane_prompt.is_none());
        let decision = rx.try_recv().expect("deny decision should be sent");
        match decision {
            pie_agent_core::ControlPlanePromptDecision::Deny { reason } => {
                assert_eq!(reason.as_deref(), Some("denied by user"));
            }
            other => panic!("expected deny decision, got {other:?}"),
        }
        assert_eq!(
            app.queued_turns.len(),
            1,
            "denying a prompt must not clear unrelated queued user input"
        );
    }

    #[test]
    fn control_plane_prompt_enter_sends_allow_decision() {
        use pie_agent_core::ControlPlanePromptRequest;
        use tokio::sync::oneshot;

        let mut app = test_app();
        let (tx, mut rx) = oneshot::channel();
        app.show_control_plane_prompt(UiControlPlanePrompt {
            request: ControlPlanePromptRequest {
                tool_call_id: "call_1".into(),
                tool_name: "InstallSkill".into(),
                args_hash: "abcdef1234567890".repeat(4),
                label: "Install skill".into(),
                payload: serde_json::json!({
                    "source": "user",
                    "args_hash": "abcdef1234567890"
                }),
                reason: "writes skill files".into(),
            },
            responder: tx,
        });

        assert!(app.handle_control_plane_prompt_key(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::empty()
        )));
        assert!(app.control_plane_prompt.is_none());
        let decision = rx.try_recv().expect("allow decision should be sent");
        assert!(matches!(
            decision,
            pie_agent_core::ControlPlanePromptDecision::Allow
        ));
    }

    #[test]
    fn headless_control_plane_prompt_hook_denies_with_recovery() {
        let hook = crate::control_plane_prompt::deny_hook(
            "control-plane prompt requires an interactive terminal; run pie in a TTY to approve this action",
        );
        let decision = futures::executor::block_on(hook(
            pie_agent_core::ControlPlanePromptRequest {
                tool_call_id: "call_1".into(),
                tool_name: "InstallSkill".into(),
                args_hash: "a".repeat(64),
                label: "Install skill".into(),
                payload: serde_json::json!({ "url": "https://example.com/?token=SECRET" }),
                reason: "writes skill files".into(),
            },
            CancellationToken::new(),
        ));
        match decision {
            pie_agent_core::ControlPlanePromptDecision::Deny { reason } => {
                let reason = reason.unwrap_or_default();
                assert!(reason.contains("interactive terminal"));
                assert!(!reason.contains("SECRET"));
            }
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn headless_yes_control_plane_prompt_hook_allows_without_rendering_payload() {
        let hook = crate::control_plane_prompt::allow_hook();
        let decision = futures::executor::block_on(hook(
            pie_agent_core::ControlPlanePromptRequest {
                tool_call_id: "call_1".into(),
                tool_name: "InstallSkill".into(),
                args_hash: "a".repeat(64),
                label: "Install skill".into(),
                payload: serde_json::json!({ "url": "https://example.com/?token=SECRET" }),
                reason: "writes skill files".into(),
            },
            CancellationToken::new(),
        ));
        assert!(matches!(
            decision,
            pie_agent_core::ControlPlanePromptDecision::Allow
        ));
    }

    #[test]
    fn login_requires_tty_message_is_bounded_and_secret_free() {
        let msg = crate::login_requires_tty_message("ds4", None);
        assert!(msg.contains("interactive terminal"));
        assert!(msg.contains("/login ds4"));
        assert!(!msg.contains("api key for"));
        assert!(!msg.contains("sk-"));
    }

    #[test]
    fn tui_enter_leave_enable_mouse_capture_for_feed_wheel_scroll() {
        let mut enter = Vec::new();
        write_enter_tui_commands(&mut enter).unwrap();
        let enter = String::from_utf8(enter).unwrap();
        assert!(enter.contains("\x1b[?1049h"));
        assert!(enter.contains("\x1b[?2004h"));
        assert!(
            enter.contains("\x1b[?1000h") && enter.contains("\x1b[?1006h"),
            "TUI must capture mouse events so wheel scroll reaches the feed: {enter:?}"
        );

        let mut leave = Vec::new();
        write_leave_tui_commands(&mut leave).unwrap();
        let leave = String::from_utf8(leave).unwrap();
        assert!(leave.contains("\x1b[?2004l"));
        assert!(leave.contains("\x1b[?1049l"));
        assert!(
            leave.contains("\x1b[?1000l") && leave.contains("\x1b[?1006l"),
            "leave path should restore terminal mouse handling: {leave:?}"
        );
    }

    #[test]
    fn replayed_tool_results_are_compacted_for_display() {
        let mut app = test_app();
        let text = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let message = AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "tool-1".into(),
            tool_name: "bash".into(),
            content: vec![UserContentBlock::text(text)],
            details: None,
            is_error: false,
            timestamp: 0,
        }));

        app.replay_message(&message);

        let rendered = feed_lines_text(&app.feed.lines(120));
        assert!(rendered.contains("line 0"));
        assert!(rendered.contains("line 49"));
        assert!(rendered.contains("truncated"));
        assert!(
            !rendered.contains("line 25"),
            "middle of long tool output should be hidden in replay display:\n{rendered}"
        );
    }

    #[test]
    fn replayed_messages_render_with_message_timestamps() {
        let mut app = test_app();
        let timestamp = chrono::Utc::now().timestamp_millis();
        app.replay_message(&AgentMessage::Llm(Message::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: UserContent::Text("historical question".into()),
            timestamp,
        })));
        app.replay_message(&AgentMessage::Llm(Message::Assistant(
            pie_ai::AssistantMessage {
                role: pie_ai::AssistantRole::Assistant,
                content: vec![ContentBlock::Text(pie_ai::TextContent {
                    text: "historical answer".into(),
                    ..Default::default()
                })],
                api: pie_ai::Api::from("faux"),
                provider: pie_ai::Provider::from("faux"),
                model: "faux".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pie_ai::Usage::default(),
                stop_reason: pie_ai::StopReason::Stop,
                error_message: None,
                timestamp,
            },
        )));

        let rendered = feed_lines_text(&app.feed.lines(120));
        let user_row = rendered
            .lines()
            .find(|line| line.contains("you ▸ historical question"))
            .expect("user row");
        let assistant_row = rendered
            .lines()
            .find(|line| line.contains("ai ▸ historical answer"))
            .expect("assistant row");

        assert_full_timestamp_prefix(user_row, &rendered);
        assert_full_timestamp_prefix(assistant_row, &rendered);
    }

    fn assert_full_timestamp_prefix(row: &str, rendered: &str) {
        assert_eq!(row.chars().nth(4), Some('-'), "{rendered}");
        assert_eq!(row.chars().nth(7), Some('-'), "{rendered}");
        assert_eq!(row.chars().nth(10), Some(' '), "{rendered}");
        assert_eq!(row.chars().nth(13), Some(':'), "{rendered}");
    }
}
