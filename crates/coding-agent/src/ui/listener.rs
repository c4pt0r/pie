//! Adapters that turn live `AgentEvent`/`HarnessEvent` streams into [`FeedUpdate`]s and push
//! them onto the UI channel. These replace the old stdout-writing `tui::Tui` listeners: the
//! full-screen app owns the only writer (the ratatui terminal), so listeners must never touch
//! stdout — they only enqueue structured updates that the run loop drains and renders.

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use pie_agent_core::{AgentEvent, AgentListener, HarnessEvent, HarnessListener};
use pie_ai::{AssistantMessageEvent, UserContentBlock};
use tokio::sync::mpsc::UnboundedSender;

use super::feed::{FeedUpdate, Level, preview, truncate_chars};

/// Build the per-turn agent listener. Maps streaming deltas, tool calls, and turn boundaries
/// into feed updates.
pub fn agent_listener(tx: UnboundedSender<FeedUpdate>) -> AgentListener {
    Arc::new(move |event, _cancel| {
        let tx = tx.clone();
        Box::pin(async move {
            for update in map_agent_event(&event) {
                let _ = tx.send(update);
            }
        })
    })
}

fn map_agent_event(event: &AgentEvent) -> Vec<FeedUpdate> {
    match event {
        AgentEvent::AgentStart => vec![FeedUpdate::TurnStart],
        AgentEvent::AgentEnd { .. } => vec![FeedUpdate::TurnEnd],
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                vec![FeedUpdate::TextDelta(delta.clone())]
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                vec![FeedUpdate::ThinkingDelta(delta.clone())]
            }
            _ => Vec::new(),
        },
        AgentEvent::ToolExecutionStart {
            tool_name, args, ..
        } => vec![FeedUpdate::ToolStart {
            name: tool_name.clone(),
            args: preview(args),
        }],
        AgentEvent::ToolExecutionEnd {
            result, is_error, ..
        } => {
            let mut lines = Vec::new();
            for block in &result.content {
                if let UserContentBlock::Text(t) = block {
                    for line in t.text.lines() {
                        lines.push(line.to_string());
                    }
                }
            }
            vec![FeedUpdate::ToolEnd {
                lines,
                is_error: *is_error,
            }]
        }
        _ => Vec::new(),
    }
}

/// Build the harness listener for trigger lifecycle lines. Keeps the same "stay quiet unless a
/// dynamic periodic check actually matched" behavior the old renderer had.
pub fn harness_listener(tx: UnboundedSender<FeedUpdate>) -> HarnessListener {
    let quiet: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    Arc::new(move |event| {
        if let Some(update) = map_harness_event(&event, &quiet) {
            let _ = tx.send(update);
        }
    })
}

fn map_harness_event(event: &HarnessEvent, quiet: &Mutex<HashSet<String>>) -> Option<FeedUpdate> {
    match event {
        HarnessEvent::TriggerCompleted {
            trace_id, summary, ..
        } => {
            let summary = summary.as_deref().unwrap_or("completed");
            let was_quiet = quiet.lock().remove(trace_id);
            if was_quiet && summary.trim() == "no dynamic trigger rule matched" {
                return None;
            }
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger completed] trace={} {}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(summary, 180)
                ),
                level: Level::Note,
            })
        }
        HarnessEvent::TriggerFailed { trace_id, reason } => {
            quiet.lock().remove(trace_id);
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger failed] trace={} {}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(reason, 180)
                ),
                level: Level::Error,
            })
        }
        HarnessEvent::TriggerExecutionStarted {
            trace_id,
            source_label,
            event_label,
            prompt_preview,
        } => {
            if source_label == "local:dynamic" && event_label == "dynamic periodic check" {
                quiet.lock().insert(trace_id.clone());
                return None;
            }
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger running] trace={} {}",
                    truncate_chars(trace_id, 24),
                    truncate_chars(prompt_preview, 120)
                ),
                level: Level::System,
            })
        }
        _ => None,
    }
}
