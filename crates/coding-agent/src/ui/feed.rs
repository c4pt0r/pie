//! Conversation-feed model for the full-screen TUI.
//!
//! The feed is the scrolling region above the pinned input box. It is an ordered list of
//! [`Block`]s — user prompts, assistant text, thinking, tool calls/results, and assorted
//! status lines. Streaming [`FeedUpdate`]s mutate it in place (text/thinking deltas append to
//! the currently-open block; tool/turn boundaries close it), mirroring the transition state
//! machine the old line-stream renderer in `tui.rs` used, but producing a structured model we
//! can re-wrap and scroll instead of raw stdout bytes.
//!
//! Rendering is width-aware: [`Feed::lines`] word-wraps every block to the available width and
//! returns ready-to-draw `ratatui` lines, so scroll math operates on real display rows.

use chrono::{DateTime, Local, TimeZone, Utc};
use pie_ai::UserContentBlock;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Visual class for a plain status/output line. Maps to a concrete [`Style`] at render time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Slash-command stdout and other neutral output.
    Output,
    /// Dim diagnostic line (the old `[system]` style).
    System,
    /// Error line.
    Error,
    /// Positive status (e.g. a trigger completed).
    Note,
    /// Banner heading.
    Header,
    /// Terminal-only block art (the /web-connect QR code). The TUI renders it; web
    /// surfaces skip it — a browser viewer has already opened the page, and browser
    /// line-height breaks the half-block grid anyway.
    Qr,
}

/// A message sent from the agent/harness listeners (or the console sink) into the UI loop,
/// where it is applied to the [`Feed`]. Crosses thread boundaries, so every field is owned.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FeedUpdate {
    TurnStart,
    TurnEnd,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart {
        name: String,
        args: String,
    },
    ToolProgress {
        tool_call_id: String,
        lines: Vec<String>,
        is_error: bool,
    },
    ToolEnd {
        tool_call_id: String,
        lines: Vec<String>,
        is_error: bool,
    },
    Plain {
        text: String,
        level: Level,
    },
    TriggerPollStatus(TriggerPollStatus),
    /// The skill catalog was hot-reloaded. Display-only: appends no feed block, but the
    /// update itself drives a TUI repaint / web snapshot republish so sidebars showing the
    /// catalog never go stale.
    SkillsReloaded {
        total: usize,
    },
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebFeedBlock {
    User {
        text: String,
        timestamp: Option<String>,
    },
    Assistant {
        text: String,
        timestamp: Option<String>,
    },
    Thinking {
        text: String,
        timestamp: Option<String>,
    },
    Tool {
        name: String,
        args: String,
        timestamp: Option<String>,
    },
    ToolResult {
        lines: Vec<String>,
        is_error: bool,
        timestamp: Option<String>,
    },
    Plain {
        text: String,
        level: Level,
        timestamp: Option<String>,
    },
}

/// Bounded, display-only status for periodic trigger checks that should stay visible in the
/// main UI without appending a line to the conversation feed.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TriggerPollStatus {
    pub checked_at: String,
    pub trace_id: String,
    pub source_label: String,
    pub event_label: String,
    pub summary: String,
}

/// One renderable unit in the feed.
#[derive(Clone, Debug)]
enum Block {
    User {
        text: String,
        timestamp: Option<String>,
    },
    Assistant {
        text: String,
        timestamp: Option<String>,
    },
    Thinking {
        text: String,
        timestamp: Option<String>,
    },
    Tool {
        name: String,
        args: String,
        timestamp: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        lines: Vec<String>,
        is_error: bool,
        timestamp: Option<String>,
    },
    Plain {
        text: String,
        level: Level,
        timestamp: Option<String>,
    },
}

/// Which streaming block (if any) is currently open for appends.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    None,
    Text,
    Thinking,
}

pub struct Feed {
    blocks: Vec<Block>,
    open: Open,
    /// True until the first non-whitespace character of the current assistant text block is
    /// seen, so we drop the leading whitespace the model often emits after tool calls.
    trim_text: bool,
}

impl Feed {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            open: Open::None,
            trim_text: true,
        }
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.open = Open::None;
        self.trim_text = true;
    }

    /// Push a user prompt block. Called directly by the loop on submit / on resume replay.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push_user_with_timestamp(text, current_time_label());
    }

    pub fn push_user_at(&mut self, text: impl Into<String>, timestamp_ms: i64) {
        self.push_user_with_timestamp(text, message_timestamp_label(timestamp_ms));
    }

    fn push_user_with_timestamp(&mut self, text: impl Into<String>, timestamp: Option<String>) {
        self.open = Open::None;
        self.blocks.push(Block::User {
            text: text.into(),
            timestamp,
        });
    }

    /// Push a finished assistant text block (used by resume replay where we have whole turns).
    #[cfg(test)]
    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.push_assistant_with_timestamp(text, current_time_label());
    }

    pub fn push_assistant_at(&mut self, text: impl Into<String>, timestamp_ms: i64) {
        self.push_assistant_with_timestamp(text, message_timestamp_label(timestamp_ms));
    }

    fn push_assistant_with_timestamp(
        &mut self,
        text: impl Into<String>,
        timestamp: Option<String>,
    ) {
        self.open = Open::None;
        self.blocks.push(Block::Assistant {
            text: text.into(),
            timestamp,
        });
    }

    pub fn push_thinking_at(&mut self, text: impl Into<String>, timestamp_ms: i64) {
        self.push_thinking_with_timestamp(text, message_timestamp_label(timestamp_ms));
    }

    fn push_thinking_with_timestamp(&mut self, text: impl Into<String>, timestamp: Option<String>) {
        self.open = Open::None;
        self.blocks.push(Block::Thinking {
            text: text.into(),
            timestamp,
        });
    }

    pub fn push_plain(&mut self, text: impl Into<String>, level: Level) {
        self.push_plain_with_timestamp(text, level, current_time_label());
    }

    pub fn push_plain_untimed(&mut self, text: impl Into<String>, level: Level) {
        self.push_plain_with_timestamp(text, level, None);
    }

    fn push_plain_with_timestamp(
        &mut self,
        text: impl Into<String>,
        level: Level,
        timestamp: Option<String>,
    ) {
        self.open = Open::None;
        self.blocks.push(Block::Plain {
            text: text.into(),
            level,
            timestamp,
        });
    }

    pub fn push_tool(&mut self, name: impl Into<String>, args: impl Into<String>) {
        self.push_tool_with_timestamp(name, args, current_time_label());
    }

    pub fn push_tool_at(
        &mut self,
        name: impl Into<String>,
        args: impl Into<String>,
        timestamp_ms: i64,
    ) {
        self.push_tool_with_timestamp(name, args, message_timestamp_label(timestamp_ms));
    }

    fn push_tool_with_timestamp(
        &mut self,
        name: impl Into<String>,
        args: impl Into<String>,
        timestamp: Option<String>,
    ) {
        self.open = Open::None;
        self.blocks.push(Block::Tool {
            name: name.into(),
            args: args.into(),
            timestamp,
        });
    }

    pub fn push_tool_result(
        &mut self,
        tool_call_id: impl Into<String>,
        lines: Vec<String>,
        is_error: bool,
    ) {
        self.push_tool_result_with_timestamp(tool_call_id, lines, is_error, current_time_label());
    }

    pub fn push_tool_result_at(
        &mut self,
        tool_call_id: impl Into<String>,
        lines: Vec<String>,
        is_error: bool,
        timestamp_ms: i64,
    ) {
        self.push_tool_result_with_timestamp(
            tool_call_id,
            lines,
            is_error,
            message_timestamp_label(timestamp_ms),
        );
    }

    fn push_tool_result_with_timestamp(
        &mut self,
        tool_call_id: impl Into<String>,
        lines: Vec<String>,
        is_error: bool,
        timestamp: Option<String>,
    ) {
        self.open = Open::None;
        self.blocks.push(Block::ToolResult {
            tool_call_id: tool_call_id.into(),
            lines,
            is_error,
            timestamp,
        });
    }

    fn upsert_tool_result(&mut self, tool_call_id: String, lines: Vec<String>, is_error: bool) {
        self.open = Open::None;
        if let Some(Block::ToolResult {
            lines: existing,
            is_error: existing_is_error,
            timestamp,
            ..
        }) = self.blocks.iter_mut().rev().find(|block| {
            matches!(
                block,
                Block::ToolResult {
                    tool_call_id: id,
                    ..
                } if id == &tool_call_id
            )
        }) {
            *existing = lines;
            *existing_is_error = is_error;
            *timestamp = current_time_label();
            return;
        }
        self.push_tool_result(tool_call_id, lines, is_error);
    }

    pub fn apply(&mut self, update: FeedUpdate) {
        match update {
            FeedUpdate::TurnStart | FeedUpdate::TurnEnd => {
                self.open = Open::None;
                self.trim_text = true;
            }
            FeedUpdate::TextDelta(delta) => self.text_delta(&delta),
            FeedUpdate::ThinkingDelta(delta) => self.thinking_delta(&delta),
            FeedUpdate::ToolStart { name, args } => self.push_tool(name, args),
            FeedUpdate::ToolProgress {
                tool_call_id,
                lines,
                is_error,
            }
            | FeedUpdate::ToolEnd {
                tool_call_id,
                lines,
                is_error,
            } => self.upsert_tool_result(tool_call_id, lines, is_error),
            FeedUpdate::Plain { text, level } => self.push_plain(text, level),
            FeedUpdate::TriggerPollStatus(_) => {}
            FeedUpdate::SkillsReloaded { .. } => {}
        }
    }

    fn text_delta(&mut self, delta: &str) {
        let delta = if self.trim_text {
            let trimmed = delta.trim_start_matches(|c: char| c.is_ascii_whitespace());
            if !trimmed.is_empty() {
                self.trim_text = false;
            }
            trimmed
        } else {
            delta
        };
        if delta.is_empty() {
            return;
        }
        if self.open != Open::Text {
            self.blocks.push(Block::Assistant {
                text: String::new(),
                timestamp: current_time_label(),
            });
            self.open = Open::Text;
        }
        if let Some(Block::Assistant { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
    }

    fn thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() && self.open != Open::Thinking {
            return;
        }
        if self.open != Open::Thinking {
            self.blocks.push(Block::Thinking {
                text: String::new(),
                timestamp: current_time_label(),
            });
            self.open = Open::Thinking;
        }
        if let Some(Block::Thinking { text, .. }) = self.blocks.last_mut() {
            text.push_str(delta);
        }
    }

    /// Render the whole feed to width-wrapped `ratatui` lines, ready to scroll/draw.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut previous: Option<&Block> = None;
        for block in &self.blocks {
            if should_separate(previous, block, !out.is_empty()) {
                out.push(Line::raw(""));
            }
            match block {
                Block::User { text, timestamp } => {
                    let prefix = display_prefix(timestamp.as_deref(), "you ▸ ");
                    push_paragraphs(&mut out, text, USER_STYLE, Some(&prefix), width);
                }
                Block::Assistant { text, timestamp } => {
                    let prefix = display_prefix(timestamp.as_deref(), "ai ▸ ");
                    push_paragraphs(&mut out, text, Style::default(), Some(&prefix), width);
                }
                Block::Thinking { text, timestamp } => {
                    let prefix = display_prefix(timestamp.as_deref(), "[thinking] ");
                    push_paragraphs(&mut out, text, THINKING_STYLE, Some(&prefix), width);
                }
                Block::Tool {
                    name,
                    args,
                    timestamp,
                } => {
                    let text = format!("⚙ {name}{args}");
                    let prefix = display_prefix(timestamp.as_deref(), "");
                    push_paragraphs(&mut out, &text, TOOL_STYLE, Some(&prefix), width);
                }
                Block::ToolResult {
                    lines,
                    is_error,
                    timestamp,
                    ..
                } => {
                    let style = if *is_error {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Green)
                    };
                    let mut first = true;
                    for line in lines {
                        let indented = if first {
                            first = false;
                            format!("{}    {line}", display_prefix(timestamp.as_deref(), ""))
                        } else {
                            format!("    {line}")
                        };
                        for row in wrap_str(&indented, width) {
                            out.push(Line::styled(row, style));
                        }
                    }
                }
                Block::Plain {
                    text,
                    level,
                    timestamp,
                } => {
                    let prefix = timestamp.as_deref().map(|ts| display_prefix(Some(ts), ""));
                    push_paragraphs(
                        &mut out,
                        text,
                        style_for_level(*level),
                        prefix.as_deref(),
                        width,
                    );
                }
            }
            previous = Some(block);
        }
        out
    }

    pub fn web_blocks(&self) -> Vec<WebFeedBlock> {
        self.blocks
            .iter()
            .map(|block| match block {
                Block::User { text, timestamp } => WebFeedBlock::User {
                    text: text.clone(),
                    timestamp: timestamp.clone(),
                },
                Block::Assistant { text, timestamp } => WebFeedBlock::Assistant {
                    text: text.clone(),
                    timestamp: timestamp.clone(),
                },
                Block::Thinking { text, timestamp } => WebFeedBlock::Thinking {
                    text: text.clone(),
                    timestamp: timestamp.clone(),
                },
                Block::Tool {
                    name,
                    args,
                    timestamp,
                } => WebFeedBlock::Tool {
                    name: name.clone(),
                    args: args.clone(),
                    timestamp: timestamp.clone(),
                },
                Block::ToolResult {
                    lines,
                    is_error,
                    timestamp,
                    ..
                } => WebFeedBlock::ToolResult {
                    lines: lines.clone(),
                    is_error: *is_error,
                    timestamp: timestamp.clone(),
                },
                Block::Plain {
                    text,
                    level,
                    timestamp,
                } => WebFeedBlock::Plain {
                    text: text.clone(),
                    level: *level,
                    timestamp: timestamp.clone(),
                },
            })
            .collect()
    }
}

impl Default for Feed {
    fn default() -> Self {
        Self::new()
    }
}

const USER_STYLE: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
const THINKING_STYLE: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::ITALIC);
const TOOL_STYLE: Style = Style::new().fg(Color::Yellow);
pub const TOOL_OUTPUT_HEAD_LINES: usize = 20;
pub const TOOL_OUTPUT_TAIL_LINES: usize = 4;
pub const TOOL_OUTPUT_ERROR_HEAD_LINES: usize = 40;
pub const TOOL_OUTPUT_ERROR_TAIL_LINES: usize = 8;
pub const TOOL_OUTPUT_MAX_LINE_CHARS: usize = 200;
pub const TOOL_OUTPUT_ERROR_MAX_LINE_CHARS: usize = 240;

fn should_separate(previous: Option<&Block>, current: &Block, has_output: bool) -> bool {
    if !has_output {
        return false;
    }
    matches!(
        (previous, current),
        (_, Block::User { .. })
            | (
                Some(Block::User { .. }),
                Block::Assistant { .. } | Block::Thinking { .. } | Block::Tool { .. }
            )
    )
}

fn style_for_level(level: Level) -> Style {
    match level {
        Level::Output => Style::default(),
        Level::System => Style::default().fg(Color::DarkGray),
        Level::Error => Style::default().fg(Color::Red),
        Level::Note => Style::default().fg(Color::Green),
        Level::Header => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        Level::Qr => Style::default(),
    }
}

fn display_prefix(timestamp: Option<&str>, label: &str) -> String {
    match timestamp {
        Some(ts) if label.is_empty() => format!("{ts} "),
        Some(ts) => format!("{ts} {label}"),
        None => label.to_string(),
    }
}

fn current_time_label() -> Option<String> {
    Some(Local::now().format("%Y-%m-%d %H:%M").to_string())
}

fn message_timestamp_label(timestamp_ms: i64) -> Option<String> {
    if timestamp_ms <= 0 {
        return None;
    }
    let dt = Utc.timestamp_millis_opt(timestamp_ms).single()?;
    Some(format_timestamp_label(dt, Local::now()))
}

fn format_timestamp_label(timestamp: DateTime<Utc>, _now: DateTime<Local>) -> String {
    let local = timestamp.with_timezone(&Local);
    local.format("%Y-%m-%d %H:%M").to_string()
}

/// Split `text` on newlines, word-wrap each paragraph to `width`, and push styled lines. An
/// optional `prefix` is prepended to the very first paragraph (e.g. `you ▸ `).
fn push_paragraphs(
    out: &mut Vec<Line<'static>>,
    text: &str,
    style: Style,
    prefix: Option<&str>,
    width: usize,
) {
    for (i, para) in text.split('\n').enumerate() {
        let owned;
        let para = if i == 0 {
            if let Some(p) = prefix {
                owned = format!("{p}{para}");
                owned.as_str()
            } else {
                para
            }
        } else {
            para
        };
        for row in wrap_str(para, width) {
            out.push(Line::styled(row, style));
        }
    }
}

/// Display-width-aware word wrap. Breaks at the last space that fits; hard-breaks a single
/// word longer than `width`. Preserves leading whitespace (so indented tool output keeps its
/// shape). Returns at least one row (possibly empty) so blank lines survive.
fn wrap_str(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut last_space: Option<usize> = None;
    for ch in text.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + cw > width && !cur.is_empty() {
            if let Some(bp) = last_space.take() {
                let rest = cur.split_off(bp);
                let rest = rest.trim_start_matches(' ').to_string();
                let done = std::mem::replace(&mut cur, rest);
                rows.push(done.trim_end().to_string());
                cur_w = UnicodeWidthStr::width(cur.as_str());
            } else {
                rows.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
        }
        cur.push(ch);
        cur_w += cw;
        if ch == ' ' {
            last_space = Some(cur.len());
        }
    }
    rows.push(cur);
    rows
}

/// Render a short, single-line preview of tool-call arguments — the first few keys with
/// truncated values. Mirrors the old `tui::preview` shape (`(k="v", k2=…)`).
pub fn preview(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (k, v) in obj.iter().take(3) {
        let val = match v {
            serde_json::Value::String(s) => {
                let s = s.replace('\n', "\\n");
                format!("\"{}\"", truncate_chars(&s, 60))
            }
            _ => truncate_chars(&v.to_string(), 60),
        };
        parts.push(format!("{k}={val}"));
    }
    if obj.len() > 3 {
        parts.push("…".into());
    }
    format!("({})", parts.join(", "))
}

/// Build a compact, display-only preview of tool output. The full tool result still flows to
/// the model/session; this only limits what the TUI/feed shows while tools are running.
pub fn compact_tool_output_lines(lines: Vec<String>, is_error: bool) -> Vec<String> {
    let (head_lines, tail_lines, max_line_chars) = if is_error {
        (
            TOOL_OUTPUT_ERROR_HEAD_LINES,
            TOOL_OUTPUT_ERROR_TAIL_LINES,
            TOOL_OUTPUT_ERROR_MAX_LINE_CHARS,
        )
    } else {
        (
            TOOL_OUTPUT_HEAD_LINES,
            TOOL_OUTPUT_TAIL_LINES,
            TOOL_OUTPUT_MAX_LINE_CHARS,
        )
    };
    let original_line_count = lines.len();
    let mut hidden_bytes = 0usize;
    let mut compacted: Vec<String> = lines
        .into_iter()
        .map(|line| {
            let kept_bytes: usize = line.chars().take(max_line_chars).map(char::len_utf8).sum();
            if kept_bytes < line.len() {
                hidden_bytes += line.len() - kept_bytes;
                truncate_chars(&line, max_line_chars)
            } else {
                line
            }
        })
        .collect();

    let max_lines = head_lines + tail_lines;
    let mut hidden_lines = 0usize;
    if compacted.len() > max_lines {
        hidden_lines = compacted.len() - max_lines;
        let tail = compacted.split_off(compacted.len() - tail_lines);
        let omitted = compacted.split_off(head_lines);
        hidden_bytes += omitted.iter().map(|line| line.len() + 1).sum::<usize>();
        compacted.push(truncation_marker(hidden_bytes, hidden_lines));
        compacted.extend(tail);
    } else if hidden_bytes > 0 {
        compacted.push(truncation_marker(hidden_bytes, hidden_lines));
    }

    if original_line_count == 0 {
        Vec::new()
    } else {
        compacted
    }
}

/// Extract text blocks from a tool result and build the same display-only compact preview used
/// for live tool events. This keeps resume replay, headless output, and legacy renderers from
/// accidentally bypassing the display cap.
pub fn compact_tool_content_blocks(blocks: &[UserContentBlock], is_error: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for block in blocks {
        if let UserContentBlock::Text(t) = block {
            lines.extend(t.text.lines().map(ToString::to_string));
        }
    }
    compact_tool_output_lines(lines, is_error)
}

fn truncation_marker(hidden_bytes: usize, hidden_lines: usize) -> String {
    match (hidden_bytes, hidden_lines) {
        (0, 0) => {
            "… truncated for display; full output remains available to the agent …".to_string()
        }
        (bytes, 0) => format!(
            "… truncated {bytes} bytes for display; full output remains available to the agent …"
        ),
        (0, lines) => format!(
            "… truncated {lines} lines for display; full output remains available to the agent …"
        ),
        (bytes, lines) => format!(
            "… truncated {bytes} bytes / {lines} lines for display; full output remains available to the agent …"
        ),
    }
}

/// Truncate to at most `max_chars` characters (not bytes — never splits a multi-byte glyph),
/// appending an ellipsis when shortened.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn assert_full_timestamp_prefix(row: &str, rendered: &str) {
        assert_eq!(row.chars().nth(4), Some('-'), "{rendered}");
        assert_eq!(row.chars().nth(7), Some('-'), "{rendered}");
        assert_eq!(row.chars().nth(10), Some(' '), "{rendered}");
        assert_eq!(row.chars().nth(13), Some(':'), "{rendered}");
    }

    #[test]
    fn text_deltas_accumulate_into_one_assistant_block() {
        let mut feed = Feed::new();
        feed.apply(FeedUpdate::TurnStart);
        feed.apply(FeedUpdate::TextDelta(" hello".into()));
        feed.apply(FeedUpdate::TextDelta(" world".into()));
        feed.apply(FeedUpdate::TurnEnd);
        let rendered = plain_text(&feed.lines(80));
        // Leading whitespace before the first visible char is trimmed.
        assert!(rendered.contains("ai ▸ hello world"), "{rendered}");
    }

    #[test]
    fn thinking_then_text_then_tool_keep_separate_blocks() {
        let mut feed = Feed::new();
        feed.apply(FeedUpdate::TurnStart);
        feed.apply(FeedUpdate::ThinkingDelta("pondering".into()));
        feed.apply(FeedUpdate::TextDelta("answer".into()));
        feed.apply(FeedUpdate::ToolStart {
            name: "read".into(),
            args: "(path=\"x\")".into(),
        });
        feed.apply(FeedUpdate::ToolEnd {
            tool_call_id: "tool-1".into(),
            lines: vec!["line a".into(), "line b".into()],
            is_error: false,
        });
        feed.apply(FeedUpdate::TextDelta("after tool".into()));
        let rendered = plain_text(&feed.lines(80));
        assert!(rendered.contains("[thinking] pondering"));
        assert!(rendered.contains("answer"));
        assert!(rendered.contains("⚙ read(path=\"x\")"));
        assert!(rendered.contains("    line a"));
        assert!(rendered.contains("after tool"));
        // text-after-tool starts a fresh assistant block, not glued to "answer".
        let idx_answer = rendered.find("answer").unwrap();
        let idx_after = rendered.find("after tool").unwrap();
        assert!(idx_after > idx_answer);
    }

    #[test]
    fn wrap_breaks_on_word_boundaries_and_preserves_indent() {
        let rows = wrap_str("    aaaa bbbb cccc", 10);
        assert_eq!(rows[0], "    aaaa");
        assert!(rows.len() >= 2);
    }

    #[test]
    fn wrap_hard_breaks_overlong_word() {
        let rows = wrap_str("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn cjk_text_survives_wrapping() {
        let rows = wrap_str("你好世界一二三四", 6);
        // Each CJK glyph is width 2 → 3 per row of width 6.
        assert!(rows.iter().all(|r| UnicodeWidthStr::width(r.as_str()) <= 6));
        assert_eq!(rows.concat(), "你好世界一二三四");
    }

    #[test]
    fn user_block_gets_prefix_and_blank_separator() {
        let mut feed = Feed::new();
        feed.push_plain("banner", Level::Header);
        feed.push_user("do the thing");
        let rendered = plain_text(&feed.lines(80));
        assert!(rendered.contains("you ▸ do the thing"));
        // a blank line separates the banner from the user turn
        assert!(rendered.contains("\n\n"));
    }

    #[test]
    fn user_and_assistant_blocks_have_breathing_room() {
        let mut feed = Feed::new();
        feed.push_user("tight?");
        feed.push_assistant("not anymore");
        let rendered = plain_text(&feed.lines(80));

        assert!(
            rendered.contains("you ▸ tight?\n\n") && rendered.contains("ai ▸ not anymore"),
            "assistant reply should not be glued to the user prompt:\n{rendered}"
        );
    }

    #[test]
    fn user_and_tool_first_reply_have_breathing_room() {
        let mut feed = Feed::new();
        feed.push_user("inspect");
        feed.push_tool("read", "(path=\"x\")");
        feed.push_tool_result("tool-1", vec!["contents".into()], false);
        let rendered = plain_text(&feed.lines(80));

        assert!(
            rendered.contains("you ▸ inspect\n\n")
                && rendered.contains("⚙ read(path=\"x\")")
                && rendered.contains("    contents"),
            "tool-first assistant activity should not be glued to the user prompt, but tool result should stay with the tool call:\n{rendered}"
        );
    }

    #[test]
    fn rendered_message_blocks_include_short_time_prefix() {
        let mut feed = Feed::new();
        feed.push_user("hello");
        feed.push_assistant("hi");
        feed.push_tool("read", "(path=\"x\")");
        feed.push_tool_result("tool-1", vec!["ok".into()], false);
        let rendered = plain_text(&feed.lines(120));
        let rows: Vec<&str> = rendered.lines().collect();

        assert!(rows[0].contains("you ▸ hello"), "{rendered}");
        assert_full_timestamp_prefix(rows[0], &rendered);
        assert!(rows[2].contains("ai ▸ hi"), "{rendered}");
        assert_full_timestamp_prefix(rows[2], &rendered);
        assert!(rows[3].contains("⚙ read(path=\"x\")"), "{rendered}");
        assert_full_timestamp_prefix(rows[3], &rendered);
        assert!(rows[4].contains("    ok"), "{rendered}");
        assert_full_timestamp_prefix(rows[4], &rendered);
    }

    #[test]
    fn timestamp_label_includes_full_date_and_time() {
        let today = Local
            .with_ymd_and_hms(2026, 5, 27, 14, 37, 0)
            .single()
            .unwrap();
        let same_day = today.with_timezone(&Utc);
        let previous_day = Local
            .with_ymd_and_hms(2026, 5, 26, 23, 59, 0)
            .single()
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(format_timestamp_label(same_day, today), "2026-05-27 14:37");
        assert_eq!(
            format_timestamp_label(previous_day, today),
            "2026-05-26 23:59"
        );
    }

    #[test]
    fn narrow_width_keeps_timestamped_blocks_renderable() {
        let mut feed = Feed::new();
        feed.push_user("a very long message that wraps");
        let rendered = plain_text(&feed.lines(16));

        assert!(rendered.contains("you ▸"), "{rendered}");
        assert!(
            rendered
                .lines()
                .all(|line| UnicodeWidthStr::width(line) <= 16),
            "{rendered}"
        );
    }

    #[test]
    fn compact_tool_output_keeps_short_output_unchanged() {
        let lines = vec!["ok".to_string(), "done".to_string()];
        assert_eq!(compact_tool_output_lines(lines.clone(), false), lines);
    }

    #[test]
    fn compact_tool_output_keeps_head_and_tail_with_summary() {
        let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
        let compacted = compact_tool_output_lines(lines, false);

        assert!(compacted.len() <= TOOL_OUTPUT_HEAD_LINES + TOOL_OUTPUT_TAIL_LINES + 1);
        assert_eq!(compacted.first().map(String::as_str), Some("line 0"));
        assert!(compacted.iter().any(|line| line.contains("truncated")));
        assert!(
            compacted
                .iter()
                .any(|line| line.contains("full output remains available to the agent"))
        );
        assert_eq!(compacted.last().map(String::as_str), Some("line 39"));
    }

    #[test]
    fn compact_tool_output_allows_more_error_context() {
        let lines: Vec<String> = (0..36).map(|i| format!("line {i}")).collect();

        assert!(
            compact_tool_output_lines(lines.clone(), false)
                .iter()
                .any(|line| line.contains("truncated"))
        );
        assert_eq!(compact_tool_output_lines(lines, true).len(), 36);
    }

    #[test]
    fn compact_tool_output_truncates_utf8_safely() {
        let long = "你好".repeat(TOOL_OUTPUT_MAX_LINE_CHARS + 10);
        let compacted = compact_tool_output_lines(vec![long], false);

        assert!(compacted[0].ends_with('…'));
        assert!(compacted.iter().any(|line| line.contains("truncated")));
    }

    #[test]
    fn tool_progress_for_same_call_is_replaced_not_appended() {
        let mut feed = Feed::new();
        feed.apply(FeedUpdate::ToolProgress {
            tool_call_id: "tool-1".into(),
            lines: vec!["old progress".into()],
            is_error: false,
        });
        feed.apply(FeedUpdate::ToolProgress {
            tool_call_id: "tool-1".into(),
            lines: vec!["new progress".into()],
            is_error: false,
        });

        let rendered = plain_text(&feed.lines(80));
        assert!(!rendered.contains("old progress"));
        assert!(rendered.contains("new progress"));
    }

    #[test]
    fn final_tool_output_replaces_progress_for_same_call() {
        let mut feed = Feed::new();
        feed.apply(FeedUpdate::ToolProgress {
            tool_call_id: "tool-1".into(),
            lines: vec!["progress".into()],
            is_error: false,
        });
        feed.apply(FeedUpdate::ToolEnd {
            tool_call_id: "tool-1".into(),
            lines: vec!["final result".into()],
            is_error: false,
        });

        let rendered = plain_text(&feed.lines(80));
        assert!(!rendered.contains("progress"));
        assert!(rendered.contains("final result"));
    }
}
