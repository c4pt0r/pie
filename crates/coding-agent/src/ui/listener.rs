//! Adapters that turn live `AgentEvent`/`HarnessEvent` streams into [`FeedUpdate`]s and push
//! them onto the UI channel. These replace the old stdout-writing `tui::Tui` listeners: the
//! full-screen app owns the only writer (the ratatui terminal), so listeners must never touch
//! stdout — they only enqueue structured updates that the run loop drains and renders.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Local;
use parking_lot::Mutex;
use pie_agent_core::{AgentEvent, AgentListener, HarnessEvent, HarnessListener, TriggerState};
use pie_ai::AssistantMessageEvent;
use tokio::sync::mpsc::UnboundedSender;

use super::feed::{
    FeedUpdate, Level, TriggerPollStatus, compact_tool_content_blocks, preview, truncate_chars,
};

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
        } => {
            let (name, args) = tool_start_display(tool_name, args);
            vec![FeedUpdate::ToolStart { name, args }]
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial_result,
            ..
        } => {
            vec![FeedUpdate::ToolProgress {
                tool_call_id: tool_call_id.clone(),
                lines: compact_tool_content_blocks(&partial_result.content, false),
                is_error: false,
            }]
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => {
            vec![FeedUpdate::ToolEnd {
                tool_call_id: tool_call_id.clone(),
                lines: compact_tool_content_blocks(&result.content, *is_error),
                is_error: *is_error,
            }]
        }
        _ => Vec::new(),
    }
}

fn tool_start_display(tool_name: &str, args: &serde_json::Value) -> (String, String) {
    if tool_name == "Skill" {
        if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
            return (
                format!("Skill({})", truncate_chars(name, 48)),
                String::new(),
            );
        }
    }
    (tool_name.to_string(), preview(args))
}

/// Build the harness listener for trigger lifecycle lines. Keeps the same "stay quiet unless a
/// dynamic periodic check actually matched" behavior the old renderer had.
pub fn harness_listener(tx: UnboundedSender<FeedUpdate>, debug: bool) -> HarnessListener {
    let quiet: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    Arc::new(move |event| {
        if let Some(update) = map_harness_event(&event, &quiet, debug) {
            let _ = tx.send(update);
        }
    })
}

fn map_harness_event(
    event: &HarnessEvent,
    quiet: &Mutex<HashSet<String>>,
    debug: bool,
) -> Option<FeedUpdate> {
    match event {
        HarnessEvent::TriggerHandlingStart {
            trace_id,
            source_kind,
            source_label,
            event_label,
            ..
        } => {
            if !debug && source_label == "local:dynamic" && event_label == "dynamic periodic check"
            {
                quiet.lock().insert(trace_id.clone());
                return None;
            }
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger fired] trace={} source={} kind={} event={}",
                    debug_text(debug, trace_id, 24),
                    debug_text(debug, source_label, 48),
                    source_kind_label(*source_kind),
                    debug_text(debug, event_label, 64)
                ),
                level: Level::System,
            })
        }
        HarnessEvent::TriggerHandled {
            trace_id, state, ..
        } => match state {
            TriggerState::Accepted => None,
            TriggerState::Deduped
            | TriggerState::CycleSuppressed
            | TriggerState::PermissionDenied
            | TriggerState::NeedsApproval => {
                quiet.lock().remove(trace_id);
                Some(FeedUpdate::Plain {
                    text: format!(
                        "[trigger {}] trace={}",
                        trigger_state_label(*state),
                        debug_text(debug, trace_id, 24)
                    ),
                    level: trigger_state_level(*state),
                })
            }
            _ => None,
        },
        HarnessEvent::TriggerCompleted {
            trace_id, summary, ..
        } => {
            let summary = summary.as_deref().unwrap_or("completed");
            let was_quiet = quiet.lock().remove(trace_id);
            if !debug && was_quiet && is_no_match_dynamic_summary(summary) {
                return Some(dynamic_poll_status_update(
                    trace_id,
                    "local:dynamic",
                    "dynamic periodic check",
                    summary,
                ));
            }
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger completed] trace={} {}",
                    debug_text(debug, trace_id, 24),
                    summary
                ),
                level: Level::Note,
            })
        }
        HarnessEvent::TriggerFailed { trace_id, reason } => {
            quiet.lock().remove(trace_id);
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger failed] trace={} {}",
                    debug_text(debug, trace_id, 24),
                    debug_text(debug, reason, 180)
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
            if !debug && source_label == "local:dynamic" && event_label == "dynamic periodic check"
            {
                quiet.lock().insert(trace_id.clone());
                return None;
            }
            Some(FeedUpdate::Plain {
                text: format!(
                    "[trigger running] trace={} {}",
                    debug_text(debug, trace_id, 24),
                    debug_text(debug, prompt_preview, 120)
                ),
                level: Level::System,
            })
        }
        HarnessEvent::TurnEnded {
            decision,
            reason,
            next_prompt_preview,
            ..
        } => match *decision {
            "continue" => Some(FeedUpdate::Plain {
                text: format!(
                    "[goal continuing] {}",
                    debug_text(
                        debug,
                        next_prompt_preview
                            .as_deref()
                            .unwrap_or("continuing toward the active goal"),
                        160
                    )
                ),
                level: Level::System,
            }),
            "pause" | "budget_limited" => Some(FeedUpdate::Plain {
                text: format!(
                    "[goal paused] {}",
                    debug_text(debug, reason.as_deref().unwrap_or(*decision), 160)
                ),
                level: Level::Error,
            }),
            _ => None,
        },
        // Display-only sidebar refresh: the catalog can change with no other feed
        // activity (sub-agent installs a skill while the parent is idle), so the reload
        // must drive a repaint itself.
        HarnessEvent::SkillsReloaded { total } => {
            Some(FeedUpdate::SkillsReloaded { total: *total })
        }
        _ => None,
    }
}

fn debug_text(debug: bool, s: &str, max_chars: usize) -> String {
    if debug {
        s.to_string()
    } else {
        truncate_chars(s, max_chars)
    }
}

fn dynamic_poll_status_update(
    trace_id: &str,
    source_label: &str,
    event_label: &str,
    summary: &str,
) -> FeedUpdate {
    FeedUpdate::TriggerPollStatus(TriggerPollStatus {
        checked_at: Local::now().format("%H:%M:%S").to_string(),
        trace_id: truncate_chars(trace_id, 24),
        source_label: truncate_chars(source_label, 48),
        event_label: truncate_chars(event_label, 64),
        summary: truncate_chars(&crate::bug_report::redact(summary).replace('\n', " "), 120),
    })
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

#[cfg(test)]
fn map_harness_event_for_test(event: &HarnessEvent) -> Option<FeedUpdate> {
    let quiet = Mutex::new(HashSet::new());
    map_harness_event(event, &quiet, false)
}

fn trigger_state_label(state: TriggerState) -> &'static str {
    match state {
        TriggerState::Deduped => "deduped",
        TriggerState::CycleSuppressed => "cycle-suppressed",
        TriggerState::PermissionDenied => "permission-denied",
        TriggerState::NeedsApproval => "needs-approval",
        TriggerState::Received => "received",
        TriggerState::Accepted => "accepted",
        TriggerState::Running => "running",
        TriggerState::Failed => "failed",
        TriggerState::Completed => "completed",
    }
}

fn trigger_state_level(state: TriggerState) -> Level {
    match state {
        TriggerState::PermissionDenied | TriggerState::NeedsApproval => Level::Error,
        _ => Level::System,
    }
}

fn source_kind_label(kind: pie_agent_core::SourceKind) -> &'static str {
    match kind {
        pie_agent_core::SourceKind::Local => "local",
        pie_agent_core::SourceKind::Mcp => "mcp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_agent_core::AgentToolResult;
    use pie_ai::UserContentBlock;

    fn text_result(text: impl Into<String>) -> AgentToolResult {
        AgentToolResult {
            content: vec![UserContentBlock::text(text.into())],
            details: serde_json::Value::Null,
            terminate: None,
        }
    }

    #[test]
    fn tool_update_output_is_compacted_for_display() {
        let text = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let event = AgentEvent::ToolExecutionUpdate {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            args: serde_json::Value::Null,
            partial_result: text_result(text),
        };

        let updates = map_agent_event(&event);
        let [
            FeedUpdate::ToolProgress {
                tool_call_id,
                lines,
                ..
            },
        ] = updates.as_slice()
        else {
            panic!("expected one tool progress update");
        };
        assert_eq!(tool_call_id, "call-1");
        assert!(lines.iter().any(|line| line.contains("truncated")));
        assert!(lines.len() <= 25);
    }

    #[test]
    fn tool_result_output_is_compacted_without_mutating_result() {
        let original = "x".repeat(400);
        let result = text_result(original.clone());
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "bash".into(),
            result: result.clone(),
            is_error: false,
        };

        let updates = map_agent_event(&event);
        let [FeedUpdate::ToolEnd { lines, .. }] = updates.as_slice() else {
            panic!("expected one tool end update");
        };
        assert!(lines[0].ends_with('…'));
        if let UserContentBlock::Text(text) = &result.content[0] {
            assert_eq!(text.text, original);
        }
    }

    #[test]
    fn short_tool_output_display_stays_unchanged() {
        let event = AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: text_result("short\noutput"),
            is_error: false,
        };

        let updates = map_agent_event(&event);
        let [FeedUpdate::ToolEnd { lines, .. }] = updates.as_slice() else {
            panic!("expected one tool end update");
        };
        assert_eq!(lines, &vec!["short".to_string(), "output".to_string()]);
    }

    #[test]
    fn skill_tool_start_uses_bounded_label_without_body() {
        let event = AgentEvent::ToolExecutionStart {
            tool_call_id: "call-skill".into(),
            tool_name: "Skill".into(),
            args: serde_json::json!({
                "name": "review-pr",
                "content": "SECRET SKILL BODY"
            }),
        };

        let updates = map_agent_event(&event);
        let [FeedUpdate::ToolStart { name, args }] = updates.as_slice() else {
            panic!("expected one tool start update");
        };
        assert_eq!(name, "Skill(review-pr)");
        assert!(
            args.is_empty(),
            "Skill tool args should not be rendered: {args}"
        );
    }

    /// A catalog hot-reload must reach the UI as an update (so the skills sidebar
    /// repaints and the web snapshot republishes) without appending a conversation line.
    #[test]
    fn skills_reloaded_maps_to_sidebar_refresh_update() {
        let update = map_harness_event_for_test(&HarnessEvent::SkillsReloaded { total: 3 })
            .expect("skills reload must produce a feed update");
        assert!(
            matches!(update, FeedUpdate::SkillsReloaded { total: 3 }),
            "got {update:?}"
        );
    }

    #[test]
    fn trigger_handling_start_renders_preview_safe_live_line() {
        let update = map_harness_event_for_test(&HarnessEvent::TriggerHandlingStart {
            idempotency_key: "idem-key".into(),
            source_kind: pie_agent_core::SourceKind::Mcp,
            source_label: "mcp:github".into(),
            event_label: "pr.merged".into(),
            trace_id: "trace-start".into(),
        })
        .expect("start event should render");

        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::System);
        assert!(text.contains("[trigger fired] trace=trace-start"));
        assert!(text.contains("source=mcp:github"));
        assert!(text.contains("event=pr.merged"));
    }

    #[test]
    fn debug_mode_renders_dynamic_periodic_trigger_lines() {
        let quiet = Mutex::new(HashSet::new());
        let update = map_harness_event(
            &HarnessEvent::TriggerHandlingStart {
                idempotency_key: "idem-key".into(),
                source_kind: pie_agent_core::SourceKind::Local,
                source_label: "local:dynamic".into(),
                event_label: "dynamic periodic check".into(),
                trace_id: "trace-debug".into(),
            },
            &quiet,
            true,
        )
        .expect("debug mode should render dynamic periodic checks");

        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::System);
        assert!(text.contains("[trigger fired] trace=trace-debug"));
        assert!(text.contains("source=local:dynamic"));
    }

    #[test]
    fn trigger_completed_summary_is_not_display_truncated() {
        let summary = (0..30)
            .map(|i| format!("trigger result line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let update = map_harness_event_for_test(&HarnessEvent::TriggerCompleted {
            trace_id: "trace-full-trigger-result".into(),
            summary: Some(summary.clone()),
            cost_usd: None,
            details: serde_json::Value::Null,
        })
        .expect("completion should render");

        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::Note);
        assert!(text.contains("[trigger completed] trace=trace-full-trigger-resu"));
        assert!(text.contains("trigger result line 0"));
        assert!(text.contains("trigger result line 29"));
        assert!(text.ends_with(&summary));
        assert!(!text.contains("truncated"));
    }

    #[test]
    fn turn_end_continue_surfaces_goal_status_line() {
        let update = map_harness_event_for_test(&HarnessEvent::TurnEnded {
            decision: "continue",
            continuation_count: 1,
            reason: None,
            next_prompt_preview: Some("缺口: missing verification output. 继续。".into()),
        })
        .expect("continue should render");

        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::System);
        assert!(text.contains("[goal continuing]"));
        assert!(text.contains("missing verification output"));
    }

    #[test]
    fn turn_end_stop_stays_quiet() {
        let update = map_harness_event_for_test(&HarnessEvent::TurnEnded {
            decision: "stop",
            continuation_count: 0,
            reason: None,
            next_prompt_preview: None,
        });
        assert!(update.is_none(), "normal stop should not add feed noise");
    }

    #[test]
    fn dynamic_periodic_no_match_variants_stay_quiet() {
        let quiet = Mutex::new(HashSet::new());
        assert!(
            map_harness_event(
                &HarnessEvent::TriggerExecutionStarted {
                    trace_id: "trace-chrome-check".into(),
                    source_label: "local:dynamic".into(),
                    event_label: "dynamic periodic check".into(),
                    prompt_preview: "Check Chrome Tab Job".into(),
                },
                &quiet,
                false,
            )
            .is_none()
        );

        let update = map_harness_event(
            &HarnessEvent::TriggerCompleted {
                trace_id: "trace-chrome-check".into(),
                summary: Some("Checked Chrome tabs; no matching rule found.".into()),
                cost_usd: None,
                details: serde_json::Value::Null,
            },
            &quiet,
            false,
        );
        let Some(FeedUpdate::TriggerPollStatus(status)) = update else {
            panic!("dynamic no-match poll completion should update poll status");
        };
        assert_eq!(status.trace_id, "trace-chrome-check");
        assert_eq!(status.source_label, "local:dynamic");
        assert_eq!(status.event_label, "dynamic periodic check");
        assert!(status.summary.contains("no matching rule found"));
    }

    #[test]
    fn dynamic_periodic_poll_status_redacts_and_bounds_summary() {
        let marker = "sk-test-secret-1234567890";
        let update = dynamic_poll_status_update(
            "trace-secret",
            "local:dynamic",
            "dynamic periodic check",
            &format!("Checked Chrome tabs with token {marker}; no matching rule found."),
        );
        let FeedUpdate::TriggerPollStatus(status) = update else {
            panic!("expected poll status");
        };
        assert!(!status.summary.contains(marker));
        assert!(status.summary.contains("[REDACTED:"));
        assert!(status.summary.chars().count() <= 120);
    }

    #[test]
    fn dynamic_periodic_matched_completion_renders_result() {
        let quiet = Mutex::new(HashSet::new());
        assert!(
            map_harness_event(
                &HarnessEvent::TriggerExecutionStarted {
                    trace_id: "trace-chrome-match".into(),
                    source_label: "local:dynamic".into(),
                    event_label: "dynamic periodic check".into(),
                    prompt_preview: "Check Chrome Tab Job".into(),
                },
                &quiet,
                false,
            )
            .is_none()
        );

        let update = map_harness_event(
            &HarnessEvent::TriggerCompleted {
                trace_id: "trace-chrome-match".into(),
                summary: Some("matched dyn-123 and archived the Chrome tab".into()),
                cost_usd: None,
                details: serde_json::Value::Null,
            },
            &quiet,
            false,
        )
        .expect("matched trigger result should render");
        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::Note);
        assert!(text.contains("archived the Chrome tab"));
    }

    #[test]
    fn trigger_deduped_renders_terminal_status_line() {
        let update = map_harness_event_for_test(&HarnessEvent::TriggerHandled {
            idempotency_key: "idem-key".into(),
            trace_id: "trace-deduped".into(),
            state: TriggerState::Deduped,
            audit_entry_id: None,
            evaluator_decision: Some(serde_json::json!({ "outcome": "deduped" })),
        })
        .expect("deduped state should render");

        let FeedUpdate::Plain { text, level } = update else {
            panic!("expected plain update");
        };
        assert_eq!(level, Level::System);
        assert_eq!(text, "[trigger deduped] trace=trace-deduped");
    }
}
