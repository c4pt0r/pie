//! Capture-test for the TUI renderer. Drives a realistic event sequence (thinking → text →
//! tool call → tool result → final text → agent end) and asserts on the captured byte
//! stream. Stand-in for a real terminal e2e: without a TTY we can't observe cursor moves,
//! but we can pin the textual content + ANSI escapes that get emitted in order, which
//! catches the bugs we hit live (spinner remnants, double-printed tool names, stale color
//! formatting bleeding into post-thinking text).

use std::sync::Arc;

use pie_agent_core::{
    AgentEvent, AgentMessage, AgentTool, AgentToolResult, HarnessEvent, SourceKind, TriggerState,
};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantRole, ContentBlock, ImageContent, Message,
    StopReason, ToolCall, ToolResultMessage, ToolResultRole, Usage, UserContentBlock,
};

#[allow(dead_code)]
#[path = "../src/tui.rs"]
mod tui;

fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: pie_ai::Api::from("faux"),
        provider: pie_ai::Provider::from("faux"),
        model: "faux".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    }
}

fn message_update(ev: AssistantMessageEvent, partial: AssistantMessage) -> AgentEvent {
    AgentEvent::MessageUpdate {
        message: AgentMessage::Llm(Message::Assistant(partial)),
        assistant_message_event: ev,
    }
}

/// Strip ANSI SGR escapes so assertions can read the textual content directly. Operates on
/// chars to preserve multi-byte UTF-8 glyphs (like the `⚙` gear).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut iter = s.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\x1b' && iter.peek() == Some(&'[') {
            iter.next(); // consume '['
            // Skip CSI bytes until a final byte in 0x40..=0x7e (i.e. an ASCII letter or '@'..'~').
            while let Some(&peek) = iter.peek() {
                iter.next();
                if ('\x40'..='\x7e').contains(&peek) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[test]
fn renders_thinking_then_text_with_clean_transition() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    let partial = assistant(vec![ContentBlock::Thinking(pie_ai::ThinkingContent {
        thinking: String::new(),
        thinking_signature: None,
        redacted: false,
    })]);

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "considering options".into(),
                partial: partial.clone(),
            },
            partial.clone(),
        ),
        &mut buf,
    );
    let partial = assistant(vec![
        ContentBlock::Thinking(pie_ai::ThinkingContent {
            thinking: "considering options".into(),
            thinking_signature: None,
            redacted: false,
        }),
        ContentBlock::text(""),
    ]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "the answer is 42".into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let captured = String::from_utf8(buf).unwrap();
    let plain = strip_ansi(&captured);
    eprintln!("---captured---\n{captured}\n---plain---\n{plain}\n---end---");

    // Thinking block is labeled and contains its content.
    assert!(plain.contains("[thinking] considering options"), "{plain}");
    assert!(
        !plain.contains("pi>"),
        "assistant prompt marker should not render: {plain}"
    );
    // Final text appears AFTER the thinking line, on its own line.
    let idx_thinking = plain.find("[thinking]").unwrap();
    let idx_answer = plain.find("the answer is 42").unwrap();
    assert!(
        idx_answer > idx_thinking,
        "answer should follow thinking: thinking@{idx_thinking} answer@{idx_answer}\n{plain}"
    );
    // Between thinking and answer there must be a newline (no on-the-same-line glue).
    let between = &plain[idx_thinking..idx_answer];
    assert!(
        between.contains('\n'),
        "missing line break between thinking and answer: {between:?}"
    );
    // No stale `[thinking]` label after the answer.
    assert_eq!(
        plain.matches("[thinking]").count(),
        1,
        "exactly one [thinking] label expected, got:\n{plain}"
    );
}

#[test]
fn tool_call_prints_exactly_once_not_duplicated() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    let tool_call = ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: {
            let mut m = serde_json::Map::new();
            m.insert("path".into(), serde_json::Value::String("/tmp/x.rs".into()));
            m
        },
        thought_signature: None,
    };
    let partial = assistant(vec![ContentBlock::ToolCall(tool_call.clone())]);

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    // MessageUpdate::ToolCallStart used to print the tool name. Verify we DON'T duplicate
    // it — only ToolExecutionStart should print.
    tui.render_event(
        &message_update(
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial.clone(),
            },
            partial.clone(),
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({ "path": "/tmp/x.rs" }),
        },
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            result: AgentToolResult {
                content: vec![UserContentBlock::text("file contents here")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        },
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let captured = String::from_utf8(buf).unwrap();
    let plain = strip_ansi(&captured);
    eprintln!("---captured---\n{captured}\n---plain---\n{plain}\n---end---");

    // The tool name appears exactly once in the captured stream.
    let count = plain.matches("⚙ read").count();
    assert_eq!(
        count, 1,
        "tool name should print exactly once; got {count} occurrences:\n{plain}"
    );
    // The args preview is included.
    assert!(plain.contains("path="), "args preview missing: {plain}");
    // The result body appears indented.
    assert!(
        plain.contains("    file contents here"),
        "result not rendered: {plain}"
    );
}

#[test]
fn text_to_tool_to_text_transitions_have_clean_line_breaks() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    // Round 1: text "first reply" → tool call → text "second reply" → end.
    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let mut partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "first reply".into(),
                partial: partial.clone(),
            },
            partial.clone(),
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "t1".into(),
            tool_name: "ls".into(),
            args: serde_json::json!({}),
        },
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "t1".into(),
            tool_name: "ls".into(),
            result: AgentToolResult {
                content: vec![UserContentBlock::text("a.rs\nb.rs")],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        },
        &mut buf,
    );
    partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "second reply".into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    eprintln!("---plain---\n{plain}\n---end---");

    // Each segment appears in order.
    let p1 = plain.find("first reply").expect("first reply present");
    let p_tool = plain.find("⚙ ls").expect("tool present");
    let p_result = plain.find("    a.rs").expect("result line present");
    let p2 = plain.find("second reply").expect("second reply present");
    assert!(
        p1 < p_tool && p_tool < p_result && p_result < p2,
        "order broken: {plain}"
    );
    // No "first reply⚙" or "a.rssecond" — every transition has a line break.
    let between_first_tool = &plain[p1..p_tool];
    assert!(
        between_first_tool.contains('\n'),
        "first→tool needs newline: {between_first_tool:?}"
    );
    let between_result_second = &plain[p_result..p2];
    assert!(
        between_result_second.contains('\n'),
        "result→second needs newline: {between_result_second:?}"
    );
}

#[test]
fn pure_text_output_drops_single_prefix_space() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: " hello".into(),
                partial: partial.clone(),
            },
            partial.clone(),
        ),
        &mut buf,
    );
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: " world".into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(plain.starts_with("hello world"), "{plain:?}");
}

#[test]
fn pure_text_output_drops_prefix_space_after_empty_delta() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: String::new(),
                partial: partial.clone(),
            },
            partial.clone(),
        ),
        &mut buf,
    );
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: " dongxu!".into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(plain.starts_with("dongxu!"), "{plain:?}");
}

#[test]
fn pure_text_output_drops_prefix_whitespace_split_across_deltas() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let partial = assistant(vec![ContentBlock::text("")]);
    for delta in [" ", "\n", "\t", " dongxu!"] {
        tui.render_event(
            &message_update(
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial: partial.clone(),
                },
                partial.clone(),
            ),
            &mut buf,
        );
    }
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert_eq!(plain, "dongxu!\n", "{plain:?}");
}

#[allow(dead_code)]
fn ensure_imports_used(
    _t: ToolResultMessage,
    _i: ImageContent,
    _r: ToolResultRole,
    _a: Arc<dyn AgentTool>,
) {
}

#[test]
fn chinese_content_renders_unchanged() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    // A full turn: Chinese thinking + Chinese reply + Chinese tool args + Chinese tool
    // result. Assert every byte sequence survives the renderer intact (no mojibake, no
    // truncation, no panic from byte-level indexing).
    let thinking_text = "让我思考一下这个问题…";
    let reply_text = "答案是：你好，世界！这是一个混合的回复，包含 ASCII and 中文。";
    let tool_result_lines = "第一行\n第二行 with mixed ASCII\n第三行";

    tui.render_event(&AgentEvent::AgentStart, &mut buf);

    let partial = assistant(vec![ContentBlock::Thinking(pie_ai::ThinkingContent {
        thinking: String::new(),
        thinking_signature: None,
        redacted: false,
    })]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: thinking_text.into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );

    tui.render_event(
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            args: serde_json::json!({ "path": "/tmp/中文文件.rs", "query": "查找" }),
        },
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::ToolExecutionEnd {
            tool_call_id: "t1".into(),
            tool_name: "read".into(),
            result: AgentToolResult {
                content: vec![UserContentBlock::text(tool_result_lines)],
                details: serde_json::Value::Null,
                terminate: None,
            },
            is_error: false,
        },
        &mut buf,
    );

    let partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: reply_text.into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let captured = String::from_utf8(buf).expect("renderer must emit valid UTF-8");
    let plain = strip_ansi(&captured);
    eprintln!("---chinese-captured---\n{captured}\n---chinese-plain---\n{plain}\n---end---");

    assert!(plain.contains(thinking_text), "thinking lost: {plain}");
    assert!(plain.contains(reply_text), "reply lost: {plain}");
    assert!(plain.contains("/tmp/中文文件.rs"), "tool arg lost: {plain}");
    assert!(plain.contains("第一行"), "result line 1 lost: {plain}");
    assert!(
        plain.contains("第二行 with mixed ASCII"),
        "result line 2 lost: {plain}"
    );
    assert!(plain.contains("第三行"), "result line 3 lost: {plain}");
}

/// Specifically regress against the `preview()` byte-truncation panic. A long Chinese
/// argument would hit `String::truncate(60)` mid-codepoint and crash the renderer. With
/// the char-bounded fix, the long arg gets cleanly truncated + an ellipsis.
#[test]
fn long_chinese_tool_arg_does_not_panic() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    // Construct a >60-char Chinese string. Each char is 3 bytes UTF-8 so the byte length
    // is 3× the char count — `String::truncate(60)` would have panicked.
    let long_chinese: String = "中文测试".repeat(30); // 120 chars, 360 bytes
    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    tui.render_event(
        &AgentEvent::ToolExecutionStart {
            tool_call_id: "t1".into(),
            tool_name: "search".into(),
            args: serde_json::json!({ "query": long_chinese }),
        },
        &mut buf,
    );
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    // Output contains the tool name + an ellipsis from truncation.
    assert!(plain.contains("⚙ search"), "{plain}");
    assert!(
        plain.contains('…'),
        "ellipsis expected from truncation: {plain}"
    );
}

/// Streaming arrives in fragments — sometimes splitting *inside* a multi-byte UTF-8 glyph
/// at the network layer. The renderer never sees raw bytes (StreamFn already decodes), but
/// we verify the chunk-by-chunk path doesn't break Chinese.
#[test]
fn streaming_chunks_preserve_chinese() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let partial = assistant(vec![ContentBlock::text("")]);
    // Emit one character at a time — every single Chinese char survives the per-delta
    // write path.
    for ch in "你好，世界！".chars() {
        tui.render_event(
            &message_update(
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: ch.to_string(),
                    partial: partial.clone(),
                },
                partial.clone(),
            ),
            &mut buf,
        );
    }
    tui.render_event(
        &AgentEvent::AgentEnd {
            messages: Vec::new(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.contains("你好，世界！"),
        "single-char chunked text dropped chars: {plain}"
    );
}

#[test]
fn trigger_completion_renders_live_result_line() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_event(&AgentEvent::AgentStart, &mut buf);
    let partial = assistant(vec![ContentBlock::text("")]);
    tui.render_event(
        &message_update(
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "partial reply".into(),
                partial: partial.clone(),
            },
            partial,
        ),
        &mut buf,
    );
    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-live-result".into(),
            summary: Some("wrote /tmp/trigger-output".into()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(plain.contains("partial reply\n"), "{plain}");
    assert!(
        plain.contains("[trigger completed] trace=trace-live-result wrote /tmp/trigger-output"),
        "{plain}"
    );
}

#[test]
fn trigger_start_renders_live_fired_line() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerHandlingStart {
            idempotency_key: "idem-key".into(),
            source_kind: SourceKind::Mcp,
            source_label: "mcp:github".into(),
            event_label: "pr.merged".into(),
            trace_id: "trace-trigger-start".into(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.contains(
            "[trigger fired] trace=trace-trigger-start source=mcp:github kind=mcp event=pr.merged"
        ),
        "{plain}"
    );
}

#[test]
fn trigger_terminal_non_running_state_renders_live_status_line() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerHandled {
            idempotency_key: "idem-key".into(),
            trace_id: "trace-deduped".into(),
            state: TriggerState::Deduped,
            audit_entry_id: None,
            evaluator_decision: Some(serde_json::json!({ "outcome": "deduped" })),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.contains("[trigger deduped] trace=trace-deduped"),
        "{plain}"
    );
}

#[test]
fn trigger_completion_summary_is_not_display_truncated() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();
    let long_summary = (0..40)
        .map(|i| format!("result-line-{i}"))
        .collect::<Vec<_>>()
        .join("\n");

    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-long-result".into(),
            summary: Some(long_summary),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(plain.contains("result-line-0"), "{plain}");
    assert!(plain.contains("result-line-39"), "{plain}");
    assert!(
        !plain.contains("truncated") && !plain.contains('…'),
        "trigger completion is final output and should not use preview truncation:\n{plain}"
    );
}

#[test]
fn trigger_completion_starts_on_new_line_while_readline_prompt_is_idle() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-idle-result".into(),
            summary: Some("hello from trigger".into()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.starts_with("\n[trigger completed] trace=trace-idle-result hello from trigger"),
        "{plain:?}"
    );
}

#[test]
fn trigger_completion_renders_full_summary_without_preview_truncation() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();
    let summary = (0..30)
        .map(|i| format!("trigger output line {i}"))
        .collect::<Vec<_>>()
        .join("\n");

    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-long-result".into(),
            summary: Some(summary.clone()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(plain.contains("trigger output line 0"), "{plain}");
    assert!(plain.contains("trigger output line 29"), "{plain}");
    assert!(
        !plain.contains('…'),
        "trigger completion is the only result surface and should not be preview-truncated:\n{plain}"
    );
    assert!(plain.ends_with(&format!("{summary}\n")));
}

#[test]
fn trigger_failure_renders_live_error_line() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerFailed {
            trace_id: "trace-failed".into(),
            reason: "tool denied".into(),
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.contains("[trigger failed] trace=trace-failed tool denied"),
        "{plain}"
    );
}

#[test]
fn dynamic_poll_no_match_stays_quiet() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerExecutionStarted {
            trace_id: "trace-dynamic-check".into(),
            source_label: "local:dynamic".into(),
            event_label: "dynamic periodic check".into(),
            prompt_preview: "A trigger check event arrived.".into(),
        },
        &mut buf,
    );
    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-dynamic-check".into(),
            summary: Some("no dynamic trigger rule matched".into()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert_eq!(plain, "");
}

#[test]
fn dynamic_poll_no_match_variant_stays_quiet() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerExecutionStarted {
            trace_id: "trace-chrome-check".into(),
            source_label: "local:dynamic".into(),
            event_label: "dynamic periodic check".into(),
            prompt_preview: "Check Chrome Tab Job".into(),
        },
        &mut buf,
    );
    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-chrome-check".into(),
            summary: Some("Checked Chrome tabs; no matching rule found.".into()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert_eq!(plain, "");
}

#[test]
fn dynamic_poll_matched_result_still_renders() {
    let tui = tui::Tui::new();
    let mut buf: Vec<u8> = Vec::new();

    tui.render_harness_event(
        &HarnessEvent::TriggerExecutionStarted {
            trace_id: "trace-chrome-match".into(),
            source_label: "local:dynamic".into(),
            event_label: "dynamic periodic check".into(),
            prompt_preview: "Check Chrome Tab Job".into(),
        },
        &mut buf,
    );
    tui.render_harness_event(
        &HarnessEvent::TriggerCompleted {
            trace_id: "trace-chrome-match".into(),
            summary: Some("matched dyn-123 and archived the Chrome tab".into()),
            cost_usd: None,
            details: serde_json::Value::Null,
        },
        &mut buf,
    );

    let plain = strip_ansi(&String::from_utf8(buf).unwrap());
    assert!(
        plain.contains("[trigger completed] trace=trace-chrome-match matched dyn-123"),
        "{plain}"
    );
    assert!(plain.contains("archived the Chrome tab"), "{plain}");
}
