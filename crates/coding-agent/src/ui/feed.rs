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

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Visual class for a plain status/output line. Maps to a concrete [`Style`] at render time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

/// A message sent from the agent/harness listeners (or the console sink) into the UI loop,
/// where it is applied to the [`Feed`]. Crosses thread boundaries, so every field is owned.
#[derive(Clone, Debug)]
pub enum FeedUpdate {
    TurnStart,
    TurnEnd,
    TextDelta(String),
    ThinkingDelta(String),
    ToolStart { name: String, args: String },
    ToolEnd { lines: Vec<String>, is_error: bool },
    Plain { text: String, level: Level },
}

/// One renderable unit in the feed.
#[derive(Clone, Debug)]
enum Block {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool { name: String, args: String },
    ToolResult { lines: Vec<String>, is_error: bool },
    Plain { text: String, level: Level },
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
        self.open = Open::None;
        self.blocks.push(Block::User(text.into()));
    }

    /// Push a finished assistant text block (used by resume replay where we have whole turns).
    pub fn push_assistant(&mut self, text: impl Into<String>) {
        self.open = Open::None;
        self.blocks.push(Block::Assistant(text.into()));
    }

    /// Push a finished thinking block (used by resume replay).
    pub fn push_thinking(&mut self, text: impl Into<String>) {
        self.open = Open::None;
        self.blocks.push(Block::Thinking(text.into()));
    }

    pub fn push_plain(&mut self, text: impl Into<String>, level: Level) {
        self.open = Open::None;
        self.blocks.push(Block::Plain {
            text: text.into(),
            level,
        });
    }

    pub fn push_tool(&mut self, name: impl Into<String>, args: impl Into<String>) {
        self.open = Open::None;
        self.blocks.push(Block::Tool {
            name: name.into(),
            args: args.into(),
        });
    }

    pub fn push_tool_result(&mut self, lines: Vec<String>, is_error: bool) {
        self.open = Open::None;
        self.blocks.push(Block::ToolResult { lines, is_error });
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
            FeedUpdate::ToolEnd { lines, is_error } => self.push_tool_result(lines, is_error),
            FeedUpdate::Plain { text, level } => self.push_plain(text, level),
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
            self.blocks.push(Block::Assistant(String::new()));
            self.open = Open::Text;
        }
        if let Some(Block::Assistant(s)) = self.blocks.last_mut() {
            s.push_str(delta);
        }
    }

    fn thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() && self.open != Open::Thinking {
            return;
        }
        if self.open != Open::Thinking {
            self.blocks.push(Block::Thinking(String::new()));
            self.open = Open::Thinking;
        }
        if let Some(Block::Thinking(s)) = self.blocks.last_mut() {
            s.push_str(delta);
        }
    }

    /// Render the whole feed to width-wrapped `ratatui` lines, ready to scroll/draw.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = width.max(1);
        let mut out: Vec<Line<'static>> = Vec::new();
        for block in &self.blocks {
            match block {
                Block::User(text) => {
                    if !out.is_empty() {
                        out.push(Line::raw(""));
                    }
                    push_paragraphs(&mut out, text, USER_STYLE, Some("you ▸ "), width);
                }
                Block::Assistant(text) => {
                    push_paragraphs(&mut out, text, Style::default(), None, width);
                }
                Block::Thinking(text) => {
                    push_paragraphs(&mut out, text, THINKING_STYLE, Some("[thinking] "), width);
                }
                Block::Tool { name, args } => {
                    let text = format!("⚙ {name}{args}");
                    push_paragraphs(&mut out, &text, TOOL_STYLE, None, width);
                }
                Block::ToolResult { lines, is_error } => {
                    let style = if *is_error {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::Green)
                    };
                    for line in lines {
                        let indented = format!("    {line}");
                        for row in wrap_str(&indented, width) {
                            out.push(Line::styled(row, style));
                        }
                    }
                }
                Block::Plain { text, level } => {
                    push_paragraphs(&mut out, text, style_for_level(*level), None, width);
                }
            }
        }
        out
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

fn style_for_level(level: Level) -> Style {
    match level {
        Level::Output => Style::default(),
        Level::System => Style::default().fg(Color::DarkGray),
        Level::Error => Style::default().fg(Color::Red),
        Level::Note => Style::default().fg(Color::Green),
        Level::Header => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    }
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

    #[test]
    fn text_deltas_accumulate_into_one_assistant_block() {
        let mut feed = Feed::new();
        feed.apply(FeedUpdate::TurnStart);
        feed.apply(FeedUpdate::TextDelta(" hello".into()));
        feed.apply(FeedUpdate::TextDelta(" world".into()));
        feed.apply(FeedUpdate::TurnEnd);
        let rendered = plain_text(&feed.lines(80));
        // Leading whitespace before the first visible char is trimmed.
        assert_eq!(rendered, "hello world");
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
        assert!(rendered.contains("\n\nyou ▸"));
    }
}
