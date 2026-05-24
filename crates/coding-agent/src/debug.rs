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

use crate::ui::feed::{FeedUpdate, Level, preview, truncate_chars};

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
                        emit(
                            &tx,
                            format!(
                                "[debug llm #{call_id} error] reason={reason:?} elapsed={} message=\"{}\"",
                                elapsed_ms(started_at),
                                truncate_chars(
                                    error.error_message.as_deref().unwrap_or("unknown error"),
                                    220
                                )
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
        .map(|s| truncate_chars(s, 24))
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
        "[debug llm #{call_id} context] last_{}=\"{}\"",
        role_label(last),
        truncate_chars(&message_preview(last), 260)
    ))
}

fn tool_call_line(call_id: u64, tool_call: &ToolCall) -> String {
    let args = serde_json::Value::Object(tool_call.arguments.clone());
    format!(
        "[debug llm #{call_id} tool-call] id={} name={} args={}",
        truncate_chars(&tool_call.id, 40),
        tool_call.name,
        preview(&args)
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
        .map(|s| truncate_chars(s, 48))
        .unwrap_or_else(|| "-".into());
    format!(
        "[debug llm #{call_id} done] reason={reason:?} stop={:?} elapsed={} usage=input:{} output:{} cache_read:{} cache_write:{} total:{} cost:${:.6} response_id={} text=\"{}\"",
        message.stop_reason,
        elapsed_ms(started_at),
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.total_tokens,
        usage.cost.total,
        response_id,
        truncate_chars(&assistant_preview(message), 220)
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

fn message_preview(message: &PiMessage) -> String {
    match message {
        PiMessage::User(user) => user_content_preview(&user.content),
        PiMessage::Assistant(assistant) => assistant_preview(assistant),
        PiMessage::ToolResult(result) => result
            .content
            .iter()
            .map(user_content_block_preview)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn user_content_preview(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => normalize_ws(text),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(user_content_block_preview)
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn user_content_block_preview(block: &UserContentBlock) -> String {
    match block {
        UserContentBlock::Text(text) => normalize_ws(&text.text),
        UserContentBlock::Image(image) => format!("[image:{}]", image.mime_type),
    }
}

fn assistant_preview(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => normalize_ws(&text.text),
            ContentBlock::Thinking(thinking) => normalize_ws(&thinking.thinking),
            ContentBlock::Image(image) => format!("[image:{}]", image.mime_type),
            ContentBlock::ToolCall(tool_call) => {
                format!("[tool-call:{}:{}]", tool_call.id, tool_call.name)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
