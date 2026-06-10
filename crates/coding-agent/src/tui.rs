//! Terminal output helpers + AgentEvent renderer. Modeled on the TS interactive mode but kept
//! deliberately spartan: no widgets, no scrollback, just colored line-stream output.
//!
//! All formatting goes through crossterm so it's cross-platform and degrades gracefully on
//! non-TTY targets. We never enable raw mode; the REPL uses plain `stdin().lock().read_line`,
//! which means Ctrl-C interrupts the whole process — fine for a "simple" agent.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use parking_lot::Mutex;
use pie_agent_core::{AgentEvent, AgentListener, AgentMessage, HarnessEvent, HarnessListener};
use pie_ai::{
    AssistantMessageEvent, ContentBlock, ImageContent, Message, UserContent, UserContentBlock,
};

#[derive(Default)]
struct RenderState {
    /// Streamed text emitted so far this turn (so we can decide when to insert a newline).
    text_open: bool,
    /// True until the first non-whitespace text character is emitted for the current block.
    trim_text_prefix: bool,
    /// True while a thinking block is being streamed.
    thinking_open: bool,
    /// Trace ids for background dynamic checks that should stay quiet unless they do work.
    quiet_dynamic_trigger_traces: HashSet<String>,
}

#[derive(Clone)]
pub struct Tui {
    state: Arc<Mutex<RenderState>>,
}

impl Tui {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RenderState::default())),
        }
    }

    /// Render the startup banner. `tool_names` comes from the registered tool definitions so
    /// adding/removing a tool in `tools::default_tools()` flows through here automatically — no
    /// hand-edited literal list to drift out of sync.
    pub fn banner(
        &self,
        model: &pie_ai::Model,
        session_id: &str,
        resumed: bool,
        tool_names: &[String],
    ) {
        let mut out = std::io::stdout();
        let _ = out.execute(SetForegroundColor(Color::Magenta));
        let _ = out.execute(Print("──────── pie-coding-agent ────────\n"));
        let _ = out.execute(ResetColor);
        println!(
            "model:   {} ({}/{})",
            model.name, model.provider.0, model.id
        );
        println!(
            "session: {session_id}{}",
            if resumed { "  [resumed]" } else { "" }
        );
        let tools = if tool_names.is_empty() {
            "(none)".to_string()
        } else {
            tool_names.join(", ")
        };
        println!("tools:   {tools}");
        println!("type a message and press Enter. Ctrl-C to quit.\n");
    }

    /// Legacy prompt marker. rustyline now renders the prompt directly, but the method is
    /// kept available for tests + non-rustyline embedders.
    #[allow(dead_code)]
    pub fn user_prompt_marker(&self) {
        let mut out = std::io::stdout();
        let _ = out.execute(SetForegroundColor(Color::Cyan));
        let _ = out.execute(Print("\nyou> "));
        let _ = out.execute(ResetColor);
        let _ = out.flush();
    }

    pub fn system_line(&self, text: &str) {
        let mut out = std::io::stdout();
        let _ = out.execute(SetForegroundColor(Color::DarkGrey));
        let _ = out.execute(Print(format!("[{text}]\n")));
        let _ = out.execute(ResetColor);
    }

    pub fn error_line(&self, text: &str) {
        let mut out = std::io::stdout();
        let _ = out.execute(SetForegroundColor(Color::Red));
        let _ = out.execute(Print(format!("[error] {text}\n")));
        let _ = out.execute(ResetColor);
    }

    /// Build an `AgentListener` that prints lifecycle events. Holds onto `self` (cheaply
    /// clonable Arc state) so deltas accumulate across events.
    pub fn listener(&self) -> AgentListener {
        let me = self.clone();
        Arc::new(move |event, _cancel| {
            let me = me.clone();
            Box::pin(async move {
                me.handle_event(&event);
            })
        })
    }

    pub fn harness_listener(&self) -> HarnessListener {
        let me = self.clone();
        Arc::new(move |event| {
            me.handle_harness_event(&event);
        })
    }

    fn handle_event(&self, event: &AgentEvent) {
        let mut out = std::io::stdout();
        self.render_event(event, &mut out);
    }

    fn handle_harness_event(&self, event: &HarnessEvent) {
        let mut out = std::io::stdout();
        self.render_harness_event(event, &mut out);
    }

    /// Render one event to any `Write`. Stdout in production, a `Vec<u8>` in tests so we
    /// can inspect the exact ANSI-bearing byte stream the agent emits during a turn.
    ///
    /// Reorg notes vs the previous version (which was racy + duplicated):
    /// - Tool calls now print exactly once, on `ToolExecutionStart` (the half-formed
    ///   `MessageUpdate::ToolCallStart` no longer emits — it just tracked thinking-close
    ///   state previously).
    /// - Thinking content is emitted to stderr instead of stdout so a future
    ///   `--no-thinking` flag / pipe consumer can suppress it without losing the actual
    ///   reply.
    /// - Every state transition (thinking→text, thinking→tool, tool-result→text) emits a
    ///   single explicit `\n` then resets color/attrs so subsequent text never carries
    ///   stale formatting.
    pub fn render_event(&self, event: &AgentEvent, out: &mut dyn std::io::Write) {
        match event {
            AgentEvent::AgentStart => {
                let mut s = self.state.lock();
                s.text_open = false;
                s.trim_text_prefix = true;
                s.thinking_open = false;
            }
            AgentEvent::AgentEnd { .. } => {
                // Close the open content line so the next REPL prompt isn't glued onto it.
                self.close_open_block(out);
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    self.close_thinking(out);
                    let should_trim = self.state.lock().trim_text_prefix;
                    let delta = if should_trim {
                        delta.trim_start_matches(|c: char| c.is_ascii_whitespace())
                    } else {
                        delta.as_str()
                    };
                    if !delta.is_empty() {
                        let mut s = self.state.lock();
                        s.text_open = true;
                        s.trim_text_prefix = false;
                    }
                    let _ = write!(out, "{delta}");
                    let _ = out.flush();
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    if !self.state.lock().thinking_open {
                        let _ = write!(out, "\n{DARK_GREY}{ITALIC}[thinking] ");
                        self.state.lock().thinking_open = true;
                    }
                    let _ = write!(out, "{delta}");
                    let _ = out.flush();
                }
                _ => {}
            },
            AgentEvent::ToolExecutionStart {
                tool_name, args, ..
            } => {
                self.close_thinking(out);
                self.close_text(out);
                let arg_preview = preview(args);
                let _ = writeln!(out, "{YELLOW}⚙ {tool_name}{arg_preview}{RESET}");
                let _ = out.flush();
            }
            AgentEvent::ToolExecutionEnd {
                tool_name: _,
                result,
                is_error,
                ..
            } => {
                let color = if *is_error { RED } else { DARK_GREEN };
                for block in &result.content {
                    if let UserContentBlock::Text(t) = block {
                        for line in t.text.lines() {
                            let _ = writeln!(out, "{color}    {line}{RESET}");
                        }
                    }
                }
                let _ = out.flush();
            }
            _ => {}
        }
    }

    pub fn render_harness_event(&self, event: &HarnessEvent, out: &mut dyn std::io::Write) {
        match event {
            HarnessEvent::TriggerHandlingStart {
                trace_id,
                source_kind,
                source_label,
                event_label,
                ..
            } => {
                if source_label == "local:dynamic" && event_label == "dynamic periodic check" {
                    self.state
                        .lock()
                        .quiet_dynamic_trigger_traces
                        .insert(trace_id.clone());
                    return;
                }
                self.begin_async_status_line(out);
                let _ = writeln!(
                    out,
                    "{DARK_GREY}[trigger fired] trace={} source={} kind={} event={}{}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(source_label, 48),
                    source_kind_label(*source_kind),
                    truncate_chars(event_label, 64),
                    RESET
                );
                let _ = out.flush();
            }
            HarnessEvent::TriggerHandled {
                trace_id, state, ..
            } => match state {
                pie_agent_core::TriggerState::Accepted => {}
                pie_agent_core::TriggerState::Deduped
                | pie_agent_core::TriggerState::CycleSuppressed
                | pie_agent_core::TriggerState::PermissionDenied
                | pie_agent_core::TriggerState::NeedsApproval => {
                    self.state
                        .lock()
                        .quiet_dynamic_trigger_traces
                        .remove(trace_id);
                    self.begin_async_status_line(out);
                    let color = trigger_state_color(*state);
                    let _ = writeln!(
                        out,
                        "{color}[trigger {}] trace={}{}",
                        trigger_state_label(*state),
                        truncate_chars(trace_id, 24),
                        RESET
                    );
                    let _ = out.flush();
                }
                _ => {}
            },
            HarnessEvent::TriggerCompleted {
                trace_id, summary, ..
            } => {
                let summary = summary.as_deref().unwrap_or("completed");
                if self
                    .state
                    .lock()
                    .quiet_dynamic_trigger_traces
                    .remove(trace_id)
                    && is_no_match_dynamic_summary(summary)
                {
                    return;
                }
                self.begin_async_status_line(out);
                let _ = writeln!(
                    out,
                    "{DARK_GREEN}[trigger completed] trace={} {}{RESET}",
                    truncate_chars(trace_id, 24),
                    summary
                );
                let _ = out.flush();
            }
            HarnessEvent::TriggerFailed { trace_id, reason } => {
                self.state
                    .lock()
                    .quiet_dynamic_trigger_traces
                    .remove(trace_id);
                self.begin_async_status_line(out);
                let _ = writeln!(
                    out,
                    "{RED}[trigger failed] trace={} {}{RESET}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(reason, 180)
                );
                let _ = out.flush();
            }
            HarnessEvent::TriggerExecutionStarted {
                trace_id,
                source_label,
                event_label,
                prompt_preview,
            } => {
                if source_label == "local:dynamic" && event_label == "dynamic periodic check" {
                    self.state
                        .lock()
                        .quiet_dynamic_trigger_traces
                        .insert(trace_id.clone());
                    return;
                }
                self.begin_async_status_line(out);
                let _ = writeln!(
                    out,
                    "{DARK_GREY}[trigger running] trace={} {}{RESET}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(prompt_preview, 120)
                );
                let _ = out.flush();
            }
            _ => {}
        }
    }

    fn close_thinking(&self, out: &mut dyn std::io::Write) -> bool {
        if self.state.lock().thinking_open {
            let _ = writeln!(out, "{RESET}");
            self.state.lock().thinking_open = false;
            return true;
        }
        false
    }

    fn close_text(&self, out: &mut dyn std::io::Write) -> bool {
        if self.state.lock().text_open {
            let _ = writeln!(out);
            let mut s = self.state.lock();
            s.text_open = false;
            s.trim_text_prefix = true;
            return true;
        }
        false
    }

    fn close_open_block(&self, out: &mut dyn std::io::Write) -> bool {
        let closed_thinking = self.close_thinking(out);
        let closed_text = self.close_text(out);
        closed_thinking || closed_text
    }

    fn begin_async_status_line(&self, out: &mut dyn std::io::Write) {
        if !self.close_open_block(out) {
            let _ = writeln!(out);
        }
    }
}

fn trigger_state_label(state: pie_agent_core::TriggerState) -> &'static str {
    match state {
        pie_agent_core::TriggerState::Deduped => "deduped",
        pie_agent_core::TriggerState::CycleSuppressed => "cycle-suppressed",
        pie_agent_core::TriggerState::PermissionDenied => "permission-denied",
        pie_agent_core::TriggerState::NeedsApproval => "needs-approval",
        pie_agent_core::TriggerState::Received => "received",
        pie_agent_core::TriggerState::Accepted => "accepted",
        pie_agent_core::TriggerState::Running => "running",
        pie_agent_core::TriggerState::Failed => "failed",
        pie_agent_core::TriggerState::Completed => "completed",
    }
}

fn is_no_match_dynamic_summary(summary: &str) -> bool {
    let normalized = summary.trim().to_ascii_lowercase();
    normalized == "no dynamic trigger rule matched"
        || normalized.contains("no dynamic trigger rule matched")
        || normalized.contains("no trigger rule matched")
        || normalized.contains("no dynamic rule matched")
        || normalized.contains("no matching trigger")
        || normalized.contains("no matching rule")
        || normalized.contains("no match found")
        || normalized.contains("nothing matched")
        || normalized.contains("not matched")
}

fn trigger_state_color(state: pie_agent_core::TriggerState) -> &'static str {
    match state {
        pie_agent_core::TriggerState::PermissionDenied
        | pie_agent_core::TriggerState::NeedsApproval => RED,
        _ => DARK_GREY,
    }
}

fn source_kind_label(kind: pie_agent_core::SourceKind) -> &'static str {
    match kind {
        pie_agent_core::SourceKind::Local => "local",
        pie_agent_core::SourceKind::Mcp => "mcp",
    }
}

// Hand-rolled ANSI SGR escapes. Cheaper than the crossterm command queue + works on any
// `Write` impl (so the test capture can use a `Vec<u8>`).
const RESET: &str = "\x1b[0m";
const ITALIC: &str = "\x1b[3m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const DARK_GREY: &str = "\x1b[90m";
const DARK_GREEN: &str = "\x1b[32;2m";

fn preview(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        let mut parts = Vec::new();
        for (k, v) in obj.iter().take(3) {
            let val = match v {
                serde_json::Value::String(s) => {
                    let s = s.replace('\n', "\\n");
                    let s = truncate_chars(&s, 60);
                    format!("\"{s}\"")
                }
                _ => {
                    let s = v.to_string();
                    truncate_chars(&s, 60)
                }
            };
            parts.push(format!("{k}={val}"));
        }
        if obj.len() > 3 {
            parts.push("…".into());
        }
        format!("({})", parts.join(", "))
    } else {
        String::new()
    }
}

/// Truncate `s` to at most `max_chars` chars (NOT bytes — `String::truncate` panics if the
/// byte offset falls inside a multi-byte UTF-8 character). Returns the original on no
/// truncation; otherwise appends an ellipsis.
pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Render a persisted user/assistant message from a session replay. Used when --resume hands
/// us a transcript to redisplay before opening the REPL.
pub fn render_persisted(message: &AgentMessage) {
    let mut out = std::io::stdout();
    match message {
        AgentMessage::Llm(Message::User(u)) => {
            let _ = out.execute(SetForegroundColor(Color::Cyan));
            let _ = out.execute(Print("\nyou> "));
            let _ = out.execute(ResetColor);
            match &u.content {
                UserContent::Text(s) => {
                    let _ = out.execute(Print(format!("{s}\n")));
                }
                UserContent::Blocks(blocks) => {
                    for b in blocks {
                        match b {
                            UserContentBlock::Text(t) => {
                                let _ = out.execute(Print(format!("{}\n", t.text)));
                            }
                            UserContentBlock::Image(ImageContent { mime_type, .. }) => {
                                let _ = out.execute(Print(format!("<image {mime_type}>\n")));
                            }
                        }
                    }
                }
            }
        }
        AgentMessage::Llm(Message::Assistant(a)) => {
            let _ = out.execute(Print("\n"));
            for b in &a.content {
                match b {
                    ContentBlock::Text(t) => {
                        let _ = out.execute(Print(format!("{}\n", t.text)));
                    }
                    ContentBlock::Thinking(t) => {
                        let _ = out.execute(SetForegroundColor(Color::DarkGrey));
                        let _ = out.execute(SetAttribute(Attribute::Italic));
                        let _ = out.execute(Print(format!("[thinking] {}\n", t.thinking)));
                        let _ = out.execute(SetAttribute(Attribute::Reset));
                        let _ = out.execute(ResetColor);
                    }
                    ContentBlock::ToolCall(tc) => {
                        let _ = out.execute(SetForegroundColor(Color::Yellow));
                        let _ = out.execute(Print(format!(
                            "⚙ {}({})\n",
                            tc.name,
                            preview(&serde_json::Value::Object(tc.arguments.clone()))
                        )));
                        let _ = out.execute(ResetColor);
                    }
                    ContentBlock::Image(_) => {}
                }
            }
        }
        AgentMessage::Llm(Message::ToolResult(tr)) => {
            let color = if tr.is_error {
                Color::Red
            } else {
                Color::DarkGreen
            };
            let _ = out.execute(SetForegroundColor(color));
            let _ = out.execute(Print(format!("  ⤷ {} →\n", tr.tool_name)));
            for b in &tr.content {
                if let UserContentBlock::Text(t) = b {
                    for line in t.text.lines() {
                        let _ = out.execute(Print(format!("    {line}\n")));
                    }
                }
            }
            let _ = out.execute(ResetColor);
        }
        AgentMessage::Custom(_) => {}
    }
}
