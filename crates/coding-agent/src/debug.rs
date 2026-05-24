//! UI-facing debug helpers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::StreamExt;
use pie_agent_core::StreamFn;
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, ContentBlock,
    Context as PiContext, Message as PiMessage, Model, SimpleStreamOptions, ToolCall, UserContent,
    UserContentBlock,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::ui::feed::{FeedUpdate, Level};

const DEBUG_PREVIEW_MAX_CHARS: usize = 4_000;
const DEBUG_PREVIEW_MAX_LINES: usize = 80;

pub fn wrap_stream_fn(base: StreamFn, tx: UnboundedSender<FeedUpdate>) -> StreamFn {
    let seq = Arc::new(AtomicU64::new(1));
    Arc::new(move |model, context, options| {
        let call_id = seq.fetch_add(1, Ordering::Relaxed);
        emit(&tx, start_line(call_id, model, context, options));
        if let Some(line) = context_line(call_id, context) {
            emit(&tx, line);
        }

        let mut inner = base(model, context, options);
        let (stream, mut sender) = AssistantMessageEventStream::new();
        let tx = tx.clone();
        let started_at = Instant::now();
        tokio::spawn(async move {
            let mut saw_terminal = false;
            while let Some(event) = inner.next().await {
                match &event {
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                        emit(&tx, tool_call_line(call_id, tool_call));
                    }
                    AssistantMessageEvent::Done { reason, message } => {
                        saw_terminal = true;
                        emit(&tx, done_line(call_id, *reason, message, started_at));
                    }
                    AssistantMessageEvent::Error { reason, error } => {
                        saw_terminal = true;
                        let message = error
                            .error_message
                            .as_deref()
                            .map(debug_preview)
                            .unwrap_or_else(|| "unknown error".into());
                        emit(
                            &tx,
                            format!(
                                "[debug llm #{call_id} error] reason={reason:?} elapsed={} message=\"{}\"",
                                elapsed_ms(started_at),
                                message
                            ),
                        );
                    }
                    _ => {}
                }
                sender.push(event);
                if sender.is_closed() {
                    break;
                }
            }
            if !saw_terminal {
                emit(
                    &tx,
                    format!(
                        "[debug llm #{call_id} closed] elapsed={} stream ended without terminal event",
                        elapsed_ms(started_at)
                    ),
                );
            }
        });

        stream
    })
}

fn emit(tx: &UnboundedSender<FeedUpdate>, text: impl Into<String>) {
    let _ = tx.send(FeedUpdate::Plain {
        text: text.into(),
        level: Level::System,
    });
}

fn start_line(
    call_id: u64,
    model: &Model,
    context: &PiContext,
    options: Option<&SimpleStreamOptions>,
) -> String {
    let tool_count = context.tools.as_ref().map_or(0, Vec::len);
    let system_chars = context.system_prompt.as_deref().map_or(0, str::len);
    let reasoning = options
        .and_then(|o| o.reasoning)
        .map(|r| format!("{r:?}"))
        .unwrap_or_else(|| "off".into());
    let session = options
        .and_then(|o| o.base.session_id.as_deref())
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".into());
    format!(
        "[debug llm #{call_id} start] provider={} api={} model={} messages={} tools={} system_chars={} reasoning={} session={}",
        model.provider.0,
        model.api.0,
        model.id,
        context.messages.len(),
        tool_count,
        system_chars,
        reasoning,
        session
    )
}

fn context_line(call_id: u64, context: &PiContext) -> Option<String> {
    let last = context.messages.last()?;
    Some(format!(
        "[debug llm #{call_id} context] last_{}:\n{}",
        role_label(last),
        message_log(last)
    ))
}

fn tool_call_line(call_id: u64, tool_call: &ToolCall) -> String {
    let args = serde_json::Value::Object(tool_call.arguments.clone());
    let args = serde_json::to_string_pretty(&args).unwrap_or_else(|_| args.to_string());
    format!(
        "[debug llm #{call_id} tool-call] id={} name={} args=\n{}",
        tool_call.id,
        tool_call.name,
        debug_preview(&args)
    )
}

fn done_line(
    call_id: u64,
    reason: pie_ai::DoneReason,
    message: &AssistantMessage,
    started_at: Instant,
) -> String {
    let usage = &message.usage;
    let response_id = message
        .response_id
        .as_deref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".into());
    format!(
        "[debug llm #{call_id} done] reason={reason:?} stop={:?} elapsed={} usage=input:{} output:{} cache_read:{} cache_write:{} total:{} cost:${:.6} response_id={} text:\n{}",
        message.stop_reason,
        elapsed_ms(started_at),
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.total_tokens,
        usage.cost.total,
        response_id,
        debug_preview(&assistant_log(message))
    )
}

fn elapsed_ms(started_at: Instant) -> String {
    format!("{}ms", started_at.elapsed().as_millis())
}

fn role_label(message: &PiMessage) -> &'static str {
    match message {
        PiMessage::User(_) => "user",
        PiMessage::Assistant(_) => "assistant",
        PiMessage::ToolResult(_) => "tool_result",
    }
}

fn message_log(message: &PiMessage) -> String {
    let raw = match message {
        PiMessage::User(user) => user_content_log(&user.content),
        PiMessage::Assistant(assistant) => assistant_log(assistant),
        PiMessage::ToolResult(result) => result
            .content
            .iter()
            .map(user_content_block_log)
            .collect::<Vec<_>>()
            .join("\n"),
    };
    debug_preview(&raw)
}

fn user_content_log(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(user_content_block_log)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_block_log(block: &UserContentBlock) -> String {
    match block {
        UserContentBlock::Text(text) => text.text.clone(),
        UserContentBlock::Image(image) => format!("[image:{}]", image.mime_type),
    }
}

fn assistant_log(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Thinking(thinking) => thinking.thinking.clone(),
            ContentBlock::Image(image) => format!("[image:{}]", image.mime_type),
            ContentBlock::ToolCall(tool_call) => {
                format!("[tool-call:{}:{}]", tool_call.id, tool_call.name)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn debug_preview(input: &str) -> String {
    bounded_preview(&crate::bug_report::redact(input))
}

fn bounded_preview(input: &str) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    let mut chars = 0usize;
    let mut truncated_for_lines = false;
    let mut truncated_for_chars = false;

    for segment in input.split_inclusive('\n') {
        if lines >= DEBUG_PREVIEW_MAX_LINES {
            truncated_for_lines = true;
            break;
        }

        let mut segment_chars = segment.chars();
        while chars < DEBUG_PREVIEW_MAX_CHARS {
            let Some(ch) = segment_chars.next() else {
                break;
            };
            out.push(ch);
            chars += 1;
            if ch == '\n' {
                lines += 1;
            }
        }

        if segment_chars.next().is_some() {
            truncated_for_chars = true;
            break;
        }

        if !segment.ends_with('\n') {
            lines += 1;
        }
    }

    if chars >= DEBUG_PREVIEW_MAX_CHARS && input.chars().count() > DEBUG_PREVIEW_MAX_CHARS {
        truncated_for_chars = true;
    }

    if truncated_for_lines || truncated_for_chars {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "[debug preview truncated: max {DEBUG_PREVIEW_MAX_LINES} lines / {DEBUG_PREVIEW_MAX_CHARS} chars]"
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pie_ai::{
        Api, AssistantMessageEvent, AssistantRole, ContentBlock, Context, DoneReason, Message,
        Provider, StopReason, TextContent, ThinkingContent, ToolResultMessage, ToolResultRole,
        Usage, UserContent, UserContentBlock, UserMessage, UserRole,
    };
    use serde_json::json;

    fn assistant_message(content: Vec<ContentBlock>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content,
            api: Api::from("faux"),
            provider: Provider::from("debug-provider"),
            model: "debug-model".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    #[test]
    fn debug_context_redacts_user_and_tool_result_secrets() {
        let user = Message::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("token sk-abcdefghijklmnopqrstuvwxyz123456".into()),
            timestamp: 0,
        });
        let line = context_line(
            1,
            &Context {
                system_prompt: None,
                messages: vec![user],
                tools: None,
            },
        )
        .unwrap();
        assert!(line.contains("[REDACTED:openai_anthropic_key]"));
        assert!(!line.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));

        let tool_result = Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![UserContentBlock::text(
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
            )],
            details: None,
            is_error: false,
            timestamp: 0,
        });
        let line = context_line(
            2,
            &Context {
                system_prompt: None,
                messages: vec![tool_result],
                tools: None,
            },
        )
        .unwrap();
        assert!(line.contains("[REDACTED:bearer_token]"));
        assert!(!line.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn debug_tool_call_and_assistant_text_are_redacted() {
        let mut args = serde_json::Map::new();
        args.insert(
            "token".into(),
            json!("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        );
        let tool_call = ToolCall {
            id: "call-1".into(),
            name: "example".into(),
            arguments: args,
            thought_signature: None,
        };
        let line = tool_call_line(1, &tool_call);
        assert!(line.contains("[REDACTED:github_token]"));
        assert!(!line.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"));

        let assistant = assistant_message(vec![
            ContentBlock::Text(TextContent {
                text: "assistant sk-abcdefghijklmnopqrstuvwxyz123456".into(),
                text_signature: None,
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: "thinking xoxb-1234567890-abcdef".into(),
                thinking_signature: None,
                redacted: false,
            }),
        ]);
        let line = done_line(2, DoneReason::Stop, &assistant, Instant::now());
        assert!(line.contains("[REDACTED:openai_anthropic_key]"));
        assert!(line.contains("[REDACTED:slack_token]"));
        assert!(!line.contains("sk-abcdefghijklmnopqrstuvwxyz123456"));
        assert!(!line.contains("xoxb-1234567890-abcdef"));
    }

    #[test]
    fn debug_error_message_is_redacted() {
        let mut message = assistant_message(vec![]);
        message.error_message =
            Some("provider said Authorization: Bearer abcdefghijklmnopqrstuvwxyz".into());
        let event = AssistantMessageEvent::Error {
            reason: pie_ai::ErrorReason::Error,
            error: message,
        };

        let AssistantMessageEvent::Error { reason, error } = event else {
            unreachable!();
        };
        let text = format!(
            "reason={reason:?} message=\"{}\"",
            error
                .error_message
                .as_deref()
                .map(debug_preview)
                .unwrap_or_default()
        );
        assert!(text.contains("[REDACTED:bearer_token]"));
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn debug_preview_is_bounded() {
        let huge = (0..200)
            .map(|i| format!("line-{i} {}", "x".repeat(100)))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = debug_preview(&huge);
        assert!(preview.contains("[debug preview truncated:"));
        assert!(preview.lines().count() <= DEBUG_PREVIEW_MAX_LINES + 1);
        assert!(preview.chars().count() <= DEBUG_PREVIEW_MAX_CHARS + 128);
    }
}
