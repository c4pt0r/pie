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
pub mod listener;

pub use feed::FeedUpdate;

use std::collections::VecDeque;
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
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Padding, Paragraph};
use tokio::sync::mpsc::UnboundedReceiver;
use tui_textarea::TextArea;

use crate::agent_session::{AgentSession, RetrySettings};
use crate::commands::{self, CommandCtx, CommandOutcome, Registry};
use crate::history::HistoryStore;
use crate::readline::SlashCompleter;
use crate::{images, mentions};
use feed::{Feed, Level};
use pie_agent_core::{AgentHarness, AgentMessage, AgentRunError};
use pie_ai::{ContentBlock, ImageContent, Message, UserContent, UserContentBlock};

/// In-flight model turn, polled in the event loop's `select!`. Running it as a local future
/// (not `tokio::spawn`) sidesteps the `Send` bound — `AgentSession::prompt` briefly holds a
/// `parking_lot` guard across an `.await`, so its future is `!Send`.
type TurnFut =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<String>, AgentRunError>>>>;

#[derive(Default)]
struct TurnState {
    fut: Option<TurnFut>,
    aborted: bool,
    /// Prefix for the error line if the turn fails (e.g. `triggered turn: `).
    prefix: &'static str,
}

async fn poll_turn(fut: &mut Option<TurnFut>) -> Result<Option<String>, AgentRunError> {
    // Only created by `select!` when `fut.is_some()`, so the unwrap is sound.
    fut.as_mut().expect("turn future present").await
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_INPUT_ROWS: usize = 6;
const SCROLL_STEP: usize = 3;
const COMPLETION_POPUP_MAX: usize = 8;
const QUEUED_PREVIEW_CHARS: usize = 80;

enum QueuedTurn {
    UserPrompt {
        display: String,
        prompt: String,
        images: Vec<ImageContent>,
    },
    AgentPrompt {
        display: String,
        prompt: String,
        error_context: &'static str,
    },
    PromptTemplate {
        display: String,
        name: String,
        vars: serde_json::Map<String, serde_json::Value>,
    },
    Compaction {
        display: String,
        custom: Option<String>,
    },
}

impl QueuedTurn {
    fn display(&self) -> &str {
        match self {
            Self::UserPrompt { display, .. }
            | Self::AgentPrompt { display, .. }
            | Self::PromptTemplate { display, .. }
            | Self::Compaction { display, .. } => display,
        }
    }
}
const TRIGGER_PANEL_MIN_TOTAL_WIDTH: u16 = 100;
const TRIGGER_PANEL_WIDTH: u16 = 36;
const TRIGGER_PANEL_RULE_LIMIT: usize = 5;

#[derive(Clone, Debug, Default)]
pub struct PanelStatus {
    pub mcp_servers: usize,
    pub mcp_tools: usize,
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
    pub panel_status: PanelStatus,
}

pub struct App {
    harness: Arc<AgentHarness>,
    retry: RetrySettings,
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

    feed: Feed,
    feed_rx: Option<UnboundedReceiver<FeedUpdate>>,
    main_run_rx: Option<UnboundedReceiver<String>>,
    panel_status: PanelStatus,

    input: TextArea<'static>,
    completions: Vec<String>,
    completion_idx: usize,

    scroll: usize,
    follow: bool,
    last_viewport_h: usize,
    last_feed_area: Option<Rect>,

    busy: bool,
    queued_turns: VecDeque<QueuedTurn>,
    spinner_frame: usize,
    last_ctrlc: Option<Instant>,
    quit: bool,
}

impl App {
    pub fn new(config: AppConfig) -> Self {
        let completer = SlashCompleter::from_registry(&config.registry);
        Self {
            harness: config.harness,
            retry: config.retry,
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
            feed: Feed::new(),
            feed_rx: Some(config.feed_rx),
            main_run_rx: Some(config.main_run_rx),
            panel_status: config.panel_status,
            input: new_textarea(),
            completions: Vec::new(),
            completion_idx: 0,
            scroll: 0,
            follow: true,
            last_viewport_h: 1,
            last_feed_area: None,
            busy: false,
            queued_turns: VecDeque::new(),
            spinner_frame: 0,
            last_ctrlc: None,
            quit: false,
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
            .push_plain("──────── pie-coding-agent ────────", Level::Header);
        self.feed.push_plain(
            format!(
                "model:   {} ({}/{})",
                model.name, model.provider.0, model.id
            ),
            Level::Output,
        );
        self.feed.push_plain(
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
            .push_plain(format!("tools:   {tools}"), Level::Output);
        self.feed.push_plain(
            "Enter to send · Ctrl-C to abort/exit · /help for commands",
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
                self.feed.push_user(text);
            }
            AgentMessage::Llm(Message::Assistant(a)) => {
                for b in &a.content {
                    match b {
                        ContentBlock::Text(t) => self.feed.push_assistant(t.text.clone()),
                        ContentBlock::Thinking(t) => self.feed.push_thinking(t.thinking.clone()),
                        ContentBlock::ToolCall(tc) => self.feed.push_tool(
                            tc.name.clone(),
                            feed::preview(&serde_json::Value::Object(tc.arguments.clone())),
                        ),
                        ContentBlock::Image(_) => {}
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(tr)) => {
                self.feed.push_tool_result(
                    tr.tool_call_id.clone(),
                    feed::compact_tool_content_blocks(&tr.content, tr.is_error),
                    tr.is_error,
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
        let mut turn = TurnState::default();

        loop {
            terminal.draw(|f| self.render(f))?;
            if self.quit {
                break;
            }
            tokio::select! {
                biased;
                result = poll_turn(&mut turn.fut), if turn.fut.is_some() => {
                    self.finish_turn(&mut turn, result);
                }
                maybe_event = reader.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_event(event, &mut turn, terminal).await?,
                        Some(Err(_)) => {}
                        None => self.quit = true,
                    }
                }
                Some(update) = feed_rx.recv() => {
                    self.feed.apply(update);
                    while let Ok(update) = feed_rx.try_recv() {
                        self.feed.apply(update);
                    }
                }
                Some(trace_id) = main_run_rx.recv(), if turn.fut.is_none() => {
                    self.start_triggered_turn(trace_id, &mut turn);
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
    fn finish_turn(&mut self, turn: &mut TurnState, result: Result<Option<String>, AgentRunError>) {
        turn.fut = None;
        self.busy = false;
        self.spinner_frame = 0;
        if turn.aborted {
            self.system_line("[aborted]");
        } else {
            match result {
                Ok(Some(message)) => self.system_line(message),
                Ok(None) => {}
                Err(e) => self.error_line(format!("{}{e}", turn.prefix)),
            }
        }
        turn.aborted = false;
        turn.prefix = "";
        self.start_next_queued_turn(turn);
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

    // ── submit / dispatch ───────────────────────────────────────────────────────────────

    async fn submit(
        &mut self,
        turn: &mut TurnState,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<()> {
        let text = self.input_text();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return Ok(());
        }
        self.clear_input();
        self.history_idx = None;
        self.last_ctrlc = None;
        self.history.append(&trimmed);
        self.follow = true;

        if trimmed.starts_with('/') {
            self.feed.push_user(&trimmed);
            self.dispatch_slash(&trimmed, terminal, turn).await;
            return Ok(());
        }

        let (expanded, _resolved) = mentions::expand(&trimmed, &self.cwd).await;
        let prompt_text =
            commands::attach_skill_prompt(expanded, self.pending_skill.take().as_deref());

        // `--image` payloads attach to the first prompt only.
        let images = std::mem::take(&mut self.pending_images);
        let loaded_images = if images.is_empty() {
            Vec::new()
        } else {
            match images::load_all(&images).await {
                Ok(imgs) => imgs,
                Err(e) => {
                    self.error_line(format!("--image: {e}"));
                    Vec::new()
                }
            }
        };

        if turn.fut.is_some() {
            self.queue_user_prompt(trimmed, prompt_text, loaded_images);
        } else {
            self.feed.push_user(&trimmed);
            self.start_user_prompt_turn(prompt_text, loaded_images, turn);
        }
        Ok(())
    }

    fn start_triggered_turn(&mut self, trace_id: String, turn: &mut TurnState) {
        // The kernel emits this only for an idle parent, but a user prompt may have started in
        // the gap; `continue_` would return AlreadyStreaming. Skip rather than error.
        if self.harness.agent().is_streaming() {
            return;
        }
        let short: String = trace_id.chars().take(8).collect();
        self.system_line(format!("running triggered turn (trace {short})"));
        self.follow = true;
        let harness = self.harness.clone();
        turn.fut = Some(Box::pin(
            async move { harness.continue_().await.map(|_| None) },
        ));
        turn.aborted = false;
        turn.prefix = "triggered turn: ";
        self.busy = true;
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

    fn start_next_queued_turn(&mut self, turn: &mut TurnState) {
        if turn.fut.is_some() {
            return;
        }
        let Some(job) = self.queued_turns.pop_front() else {
            return;
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
    }

    async fn dispatch_slash(
        &mut self,
        input: &str,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
        turn: &mut TurnState,
    ) {
        let outcome = {
            let ctx = CommandCtx {
                harness: &self.harness,
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
            CommandOutcome::LoginSecret { provider } => {
                self.login(&provider, terminal).await;
            }
            CommandOutcome::Handled => {}
        }
    }

    fn start_prompt_turn(
        &mut self,
        prompt: String,
        error_context: &'static str,
        turn: &mut TurnState,
    ) {
        let harness = self.harness.clone();
        turn.fut = Some(Box::pin(async move {
            harness.prompt(prompt).await.map(|_| None)
        }));
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
        let harness = self.harness.clone();
        let retry = self.retry.clone();
        let has_images = !loaded_images.is_empty();
        turn.fut = Some(Box::pin(async move {
            if has_images {
                harness
                    .prompt_with_images(prompt_text, loaded_images)
                    .await
                    .map(|_| None)
            } else {
                AgentSession::new(harness, retry)
                    .prompt(prompt_text)
                    .await
                    .map(|_| None)
            }
        }));
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
        let harness = self.harness.clone();
        turn.fut = Some(Box::pin(async move {
            harness
                .prompt_from_template(&name, vars)
                .await
                .map(|_| None)
        }));
        turn.aborted = false;
        turn.prefix = "template run failed: ";
        self.busy = true;
    }

    fn start_compaction_turn(&mut self, custom: Option<String>, turn: &mut TurnState) {
        let harness = self.harness.clone();
        turn.fut = Some(Box::pin(async move {
            harness.force_compact(custom).await.map(|ran| {
                Some(if ran {
                    "compaction ran".to_string()
                } else {
                    "nothing to compact".to_string()
                })
            })
        }));
        turn.aborted = false;
        turn.prefix = "compaction failed: ";
        self.busy = true;
    }

    async fn login(
        &mut self,
        provider: &str,
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
            Ok(token) => match commands::save_api_key(provider, &token) {
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
            self.harness.abort();
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
        let (feed_area, trigger_area) = if content_area.width >= TRIGGER_PANEL_MIN_TOTAL_WIDTH {
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
            "Enter queue next · Alt+Enter newline · Ctrl-C abort current · empty Ctrl-U removes last queued · Wheel/PgUp scroll"
        } else {
            "Enter send · Alt+Enter newline · ↑↓ history · Wheel/PgUp scroll · Shift/Option-drag select · Ctrl-C abort"
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
    }

    fn render_trigger_panel(&self, frame: &mut ratatui::Frame, area: Rect) {
        let lines =
            self.trigger_panel_lines(area.width.saturating_sub(2) as usize, area.height as usize);
        let panel = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::LEFT)
                .padding(Padding::left(1))
                .title(" triggers ")
                .border_style(Style::default().fg(Color::DarkGray))
                .title_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(panel, area);
    }

    fn trigger_panel_lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let rules = crate::triggers::global_registry().list();

        let mut lines = Vec::new();
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

        let hook_rows = self.panel_status.hook_points.len().max(1);
        let feature_rows = self.panel_status.trigger_features.len().max(1);
        // 2 section gaps + 2 section titles + 2 mcp body rows + hook rows + feature rows + 1 trigger-runtime gap/title
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
                lines.push(panel_line(format!("✓ {point}"), Color::Green, width));
            }
        }

        lines.push(Line::raw(""));
        lines.push(panel_line(
            "Trigger runtime".to_string(),
            Color::Cyan,
            width,
        ));
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
            let state = self.harness.agent().state();
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
                    harness: &self.harness,
                    session_id: &self.session_id,
                    log_path: self.log_path.as_ref(),
                    tool_count: self.tool_count,
                    cwd: &self.cwd,
                };
                match commands::dispatch(input, &self.registry, &ctx).await {
                    CommandOutcome::Quit => break,
                    CommandOutcome::Error(e) => eprintln!("error: {e}"),
                    CommandOutcome::LoginSecret { provider } => {
                        eprintln!("error: {}", crate::login_requires_tty_message(&provider));
                    }
                    CommandOutcome::RunAgentPrompt {
                        prompt,
                        error_context,
                    } => {
                        if let Err(e) = self.harness.prompt(prompt).await {
                            eprintln!("error: {error_context}{e}");
                        }
                    }
                    CommandOutcome::RunPromptTemplate { name, vars } => {
                        if let Err(e) = self.harness.prompt_from_template(&name, vars).await {
                            eprintln!("error: template run failed: {e}");
                        }
                    }
                    CommandOutcome::RunCompaction { custom } => {
                        match self.harness.force_compact(custom).await {
                            Ok(true) => println!("compaction ran"),
                            Ok(false) => println!("nothing to compact"),
                            Err(e) => eprintln!("error: compaction failed: {e}"),
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let (expanded, _) = mentions::expand(input, &self.cwd).await;
            let prompt = commands::attach_skill_prompt(expanded, None);
            if let Err(e) = AgentSession::new(self.harness.clone(), self.retry.clone())
                .prompt(prompt)
                .await
            {
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

fn panel_rule_preview(text: &str, width: usize) -> String {
    let redacted = crate::bug_report::redact(text).replace('\n', " ");
    feed::truncate_chars(&redacted, width.max(1))
}

fn queue_preview(text: &str) -> String {
    let redacted = crate::bug_report::redact(text).replace('\n', " ");
    feed::truncate_chars(&redacted, QUEUED_PREVIEW_CHARS)
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

    static TRIGGER_REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_app() -> App {
        let storage = Arc::new(MemorySessionStorage::new());
        let session = Session::new(storage as Arc<dyn SessionStorage>);
        let opts = AgentHarnessOptions::new(faux_model(), session);
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
            panel_status: PanelStatus::default(),
        })
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
        assert!(
            text.contains("hi there, the box is pinned"),
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
    }

    #[test]
    fn wide_layout_renders_trigger_panel() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
        let mut app = test_app();
        app.panel_status = PanelStatus {
            mcp_servers: 1,
            mcp_tools: 2,
            mcp_notification_hooks: 1,
            hook_points: vec!["before_tool_call".into(), "after_tool_call".into()],
            trigger_features: vec!["dedup".into(), "cycle suppress".into()],
        };
        crate::triggers::global_registry()
            .add_rule("a build finishes", "summarize the result")
            .unwrap();

        // Tall enough that all three sections (MCP / Hooks / Trigger runtime) clear the
        // right-rail clip — see `trigger_panel_lines`'s `status_rows` budget. The Trigger
        // runtime bullets render at the bottom of the panel; a 20-row buffer cuts them off.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("triggers"), "panel title missing:\n{text}");
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
        // Trigger runtime features render as their own section, separate from `Hooks`, so users
        // can't mistake `dedup` / `cycle suppress` etc. for pluggable callbacks.
        assert!(
            text.contains("Trigger runtime"),
            "trigger-runtime feature section title missing:\n{text}"
        );
        assert!(
            text.contains("dedup"),
            "trigger-runtime feature label missing:\n{text}"
        );
    }

    #[test]
    fn trigger_panel_redacts_rule_preview_secrets() {
        let _guard = TRIGGER_REGISTRY_TEST_LOCK.lock().unwrap();
        crate::triggers::global_registry().clear_for_tests();
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

    #[test]
    fn finished_turn_starts_next_queued_prompt_fifo() {
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

        app.finish_turn(&mut turn, Ok(None));

        assert!(turn.fut.is_some(), "queued prompt should start immediately");
        assert!(app.busy, "starting queued prompt should mark UI busy");
        assert!(app.queued_turns.is_empty());
        let text = feed_text(&app);
        assert!(text.contains("queued next message #1: next question"));
        assert!(text.contains("running queued message"));
        assert!(text.contains("you ▸ next question"));
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
    fn login_requires_tty_message_is_bounded_and_secret_free() {
        let msg = crate::login_requires_tty_message("ds4");
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
}
