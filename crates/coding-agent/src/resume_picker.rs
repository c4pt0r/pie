//! Interactive `--resume` session picker.
//!
//! Replaces the print-everything-then-read-a-number prompt: with a long history the old
//! list pushed the newest sessions off screen. This renders a scrolling viewport menu in
//! raw mode — newest sessions first, a pinned "start a new session" row on top, arrow-key
//! navigation, Enter to choose, `q`/Esc to cancel.
//!
//! Pure logic (viewport math, line rendering, key mapping) is separated from the terminal
//! IO so it stays unit-testable; only [`pick_blocking`] touches crossterm.

use std::io::Write as _;

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// One selectable session row (already newest-first; the pinned "clean" row is added by
/// the renderer, not the caller).
pub struct PickerRow {
    pub id_short: String,
    pub created_at: String,
    pub badge: Option<String>,
    pub preview: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickerChoice {
    /// Start a fresh session.
    Clean,
    /// Resume the row at this index into the caller's (newest-first) slice.
    Resume(usize),
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Select,
    Cancel,
    None,
}

const SELECTED_PREFIX: &str = "→ ";
const UNSELECTED_PREFIX: &str = "  ";

/// Total selectable entries: the pinned clean row + every session row.
fn entry_count(rows: usize) -> usize {
    rows + 1
}

/// Compute the `[start, end)` slice of entries visible in a viewport of `height` rows,
/// keeping `selected` in view. `height` is clamped to at least 1.
fn visible_window(selected: usize, total: usize, height: usize) -> (usize, usize) {
    let height = height.max(1);
    if total <= height {
        return (0, total);
    }
    // Center-ish follow: keep the selection visible, pin the window to the ends.
    let start = selected
        .saturating_sub(height / 2)
        .min(total - height)
        .min(selected); // selection never scrolls above the window
    (start, start + height)
}

/// Render the full menu (header, entries in the window, scroll indicators, footer) for a
/// terminal of `width` columns with `height` entry rows visible.
fn render_lines(rows: &[PickerRow], selected: usize, width: usize, height: usize) -> Vec<String> {
    let total = entry_count(rows.len());
    let (start, end) = visible_window(selected, total, height);

    let mut lines = Vec::new();
    lines.push(truncate_line(
        &format!(
            "resume a session ({} total) — ↑/↓ move · Enter select · q cancel",
            rows.len()
        ),
        width,
    ));
    if start > 0 {
        lines.push(truncate_line(&format!("  … {start} more above"), width));
    }
    for idx in start..end {
        let marker = if idx == selected {
            SELECTED_PREFIX
        } else {
            UNSELECTED_PREFIX
        };
        let body = if idx == 0 {
            "✚ start a new session".to_string()
        } else {
            let row = &rows[idx - 1];
            let badge = row
                .badge
                .as_deref()
                .map(|b| format!("  [{b}]"))
                .unwrap_or_default();
            format!(
                "{}  {}{}  {}",
                row.id_short, row.created_at, badge, row.preview
            )
        };
        let line = format!("{marker}{body}");
        if idx == selected {
            // Reverse video on the selection; applied after truncation so the escape
            // codes never count against the width budget.
            let visible = truncate_line(&line, width);
            lines.push(format!("\x1b[7m{visible}\x1b[0m"));
        } else {
            lines.push(truncate_line(&line, width));
        }
    }
    if end < total {
        lines.push(truncate_line(
            &format!("  … {} more below", total - end),
            width,
        ));
    }
    lines
}

fn truncate_line(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn key_action(key: &KeyEvent) -> Action {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Cancel;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Enter => Action::Select,
        KeyCode::Esc | KeyCode::Char('q') => Action::Cancel,
        _ => Action::None,
    }
}

/// Run the interactive picker on the current terminal. Blocking; call from
/// `spawn_blocking`. The caller must have verified stdin/stdout are TTYs.
pub fn pick_blocking(rows: &[PickerRow]) -> Result<PickerChoice> {
    let total = entry_count(rows.len());
    let mut selected = 0usize;

    let _guard = RawModeGuard::enable()?;
    let mut out = std::io::stderr();
    let mut painted_lines = 0usize;

    loop {
        let (width, term_rows) = crossterm::terminal::size().unwrap_or((100, 30));
        // Some pseudo-terminals report a zero/absurdly small size; render for a sane
        // minimum instead of truncating every row to nothing.
        let width = if width < 40 { 100 } else { width };
        let term_rows = if term_rows < 5 { 30 } else { term_rows };
        // header + footer + possible two scroll indicators take ~4 rows.
        let height = (term_rows as usize).saturating_sub(4).clamp(1, 30);
        let lines = render_lines(rows, selected, width as usize, height);

        // Repaint in place: move back to the top of the previous frame and redraw.
        if painted_lines > 0 {
            crossterm::execute!(
                out,
                crossterm::cursor::MoveUp(painted_lines as u16),
                crossterm::cursor::MoveToColumn(0),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::FromCursorDown),
            )?;
        }
        for line in &lines {
            // Raw mode needs explicit \r\n.
            write!(out, "{line}\r\n")?;
        }
        out.flush()?;
        painted_lines = lines.len();

        match crossterm::event::read().context("read picker key")? {
            Event::Key(key) => match key_action(&key) {
                Action::Up => selected = selected.saturating_sub(1),
                Action::Down => selected = (selected + 1).min(total - 1),
                Action::PageUp => selected = selected.saturating_sub(10),
                Action::PageDown => selected = (selected + 10).min(total - 1),
                Action::Home => selected = 0,
                Action::End => selected = total - 1,
                Action::Select => {
                    return Ok(if selected == 0 {
                        PickerChoice::Clean
                    } else {
                        PickerChoice::Resume(selected - 1)
                    });
                }
                Action::Cancel => return Ok(PickerChoice::Cancelled),
                Action::None => {}
            },
            Event::Resize(..) => {}
            _ => {}
        }
    }
}

/// Re-enables cooked mode on drop so a panic or early return can't wedge the terminal.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("enable raw mode for session picker")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn row(id: &str) -> PickerRow {
        PickerRow {
            id_short: id.into(),
            created_at: "2026-06-11T03:43".into(),
            badge: None,
            preview: format!("preview for {id}"),
        }
    }

    #[test]
    fn window_shows_everything_when_it_fits() {
        assert_eq!(visible_window(0, 5, 10), (0, 5));
        assert_eq!(visible_window(4, 5, 10), (0, 5));
    }

    #[test]
    fn window_follows_selection_through_long_lists() {
        // Selection at top: window starts at 0.
        assert_eq!(visible_window(0, 100, 10), (0, 10));
        // Selection below the fold: window slides so the selection is visible.
        let (start, end) = visible_window(50, 100, 10);
        assert!((start..end).contains(&50), "{start}..{end}");
        assert_eq!(end - start, 10);
        // Selection at the very end: window pins to the tail.
        assert_eq!(visible_window(99, 100, 10), (90, 100));
    }

    #[test]
    fn render_marks_selection_and_shows_badges_and_scroll_indicators() {
        let rows: Vec<PickerRow> = (0..20).map(|i| row(&format!("session-{i:02}"))).collect();
        let mut with_badge = rows;
        with_badge[0].badge = Some("2 cron, 1 trigger".into());

        // Clean row selected: it carries the marker, first session does not.
        let lines = render_lines(&with_badge, 0, 100, 5);
        let joined = lines.join("\n");
        assert!(joined.contains("start a new session"), "{joined}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains(SELECTED_PREFIX) && l.contains("start a new session")),
            "{joined}"
        );
        assert!(joined.contains("[2 cron, 1 trigger]"), "{joined}");
        assert!(
            joined.contains("more below"),
            "long list must show a scroll indicator: {joined}"
        );

        // Selecting deep in the list keeps the selection on screen — the newest-first top
        // rows scroll away instead of the selection.
        let lines = render_lines(&with_badge, 15, 100, 5);
        let joined = lines.join("\n");
        assert!(
            joined.contains("session-14"),
            "selected row must be visible: {joined}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains(SELECTED_PREFIX) && l.contains("session-14")),
            "{joined}"
        );
        assert!(joined.contains("more above"), "{joined}");
    }

    #[test]
    fn render_truncates_to_width() {
        let mut long = row("session-00");
        long.preview = "x".repeat(500);
        let lines = render_lines(&[long], 0, 60, 5);
        assert!(
            lines.iter().all(|l| l.chars().count() <= 60),
            "every line must fit the terminal width"
        );
    }

    #[test]
    fn keys_map_to_actions() {
        assert_eq!(key_action(&key(KeyCode::Up)), Action::Up);
        assert_eq!(key_action(&key(KeyCode::Char('k'))), Action::Up);
        assert_eq!(key_action(&key(KeyCode::Down)), Action::Down);
        assert_eq!(key_action(&key(KeyCode::Char('j'))), Action::Down);
        assert_eq!(key_action(&key(KeyCode::Enter)), Action::Select);
        assert_eq!(key_action(&key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(key_action(&key(KeyCode::Char('q'))), Action::Cancel);
        assert_eq!(key_action(&key(KeyCode::Home)), Action::Home);
        assert_eq!(key_action(&key(KeyCode::End)), Action::End);
        assert_eq!(key_action(&key(KeyCode::PageUp)), Action::PageUp);
        assert_eq!(key_action(&key(KeyCode::PageDown)), Action::PageDown);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_action(&ctrl_c), Action::Cancel);
        // Key release events (kitty protocol) must not double-fire.
        let mut release = key(KeyCode::Down);
        release.kind = KeyEventKind::Release;
        assert_eq!(key_action(&release), Action::None);
    }
}
