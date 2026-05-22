//! `run_agent_loop`. 1:1 port of `packages/agent/src/agent-loop.ts` (~742 lines).
//!
//! Implemented:
//! - Stream from `pie-ai`, accumulate events into the final `AssistantMessage`
//! - Tool execution (sequential or parallel based on `ToolExecutionMode` + per-tool override)
//! - All 4 lifecycle hooks: `transform_context`, `before_tool_call`, `after_tool_call`,
//!   `should_stop_after_turn`, `prepare_next_turn`
//! - Steering / follow-up queue draining at turn boundaries
//! - Early termination via `AgentToolResult::terminate` (when all results in a batch agree)

use std::sync::Arc;

use futures::StreamExt;
use pie_ai::{
    AssistantMessage as PiAssistantMessage, AssistantMessageEvent, Context as PiContext,
    Message as PiMessage, SimpleStreamOptions, ToolResultMessage, UserContentBlock,
};
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentInner, AgentRunError};
use crate::types::*;

pub(crate) async fn run_agent_loop(
    inner: Arc<AgentInner>,
    new_messages: Vec<AgentMessage>,
) -> Result<(), AgentRunError> {
    let cancel = CancellationToken::new();
    {
        let mut g = inner.state.lock();
        g.is_streaming = true;
        g.error_message = None;
    }
    *inner.active_cancel.lock() = Some(cancel.clone());

    emit(&inner, AgentEvent::AgentStart, &cancel).await;

    for msg in new_messages.into_iter() {
        inner.state.lock().messages.push(msg.clone());
        emit(
            &inner,
            AgentEvent::MessageStart {
                message: msg.clone(),
            },
            &cancel,
        )
        .await;
        emit(&inner, AgentEvent::MessageEnd { message: msg }, &cancel).await;
    }

    let result = drive_loop(&inner, cancel.clone()).await;
    finalize(&inner, cancel).await;
    result
}

pub(crate) async fn run_agent_loop_continue(inner: Arc<AgentInner>) -> Result<(), AgentRunError> {
    let cancel = CancellationToken::new();
    {
        let mut g = inner.state.lock();
        if g.messages.is_empty() {
            return Err(AgentRunError::Other("No messages to continue from".into()));
        }
        g.is_streaming = true;
        g.error_message = None;
    }
    *inner.active_cancel.lock() = Some(cancel.clone());
    emit(&inner, AgentEvent::AgentStart, &cancel).await;

    let result = drive_loop(&inner, cancel.clone()).await;
    finalize(&inner, cancel).await;
    result
}

async fn drive_loop(
    inner: &Arc<AgentInner>,
    cancel: CancellationToken,
) -> Result<(), AgentRunError> {
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        emit(inner, AgentEvent::TurnStart, &cancel).await;

        let assistant = match call_llm(inner, &cancel).await {
            Ok(m) => m,
            Err(e) => {
                inner.state.lock().error_message = Some(e.to_string());
                return Err(e);
            }
        };
        let assistant_agent = AgentMessage::Llm(PiMessage::Assistant(assistant.clone()));
        inner.state.lock().messages.push(assistant_agent.clone());
        emit(
            inner,
            AgentEvent::MessageEnd {
                message: assistant_agent.clone(),
            },
            &cancel,
        )
        .await;

        let (tool_results, all_terminate) = execute_tools(inner, &assistant, &cancel).await;
        for tr in &tool_results {
            let m = AgentMessage::Llm(PiMessage::ToolResult(tr.clone()));
            inner.state.lock().messages.push(m.clone());
            emit(
                inner,
                AgentEvent::MessageStart { message: m.clone() },
                &cancel,
            )
            .await;
            emit(inner, AgentEvent::MessageEnd { message: m }, &cancel).await;
        }

        emit(
            inner,
            AgentEvent::TurnEnd {
                message: assistant_agent.clone(),
                tool_results: tool_results.clone(),
            },
            &cancel,
        )
        .await;

        // `should_stop_after_turn` — caller can request graceful exit before the next LLM call.
        if let Some(hook) = inner.options.should_stop_after_turn.clone() {
            let ctx = ShouldStopAfterTurnContext {
                message: assistant.clone(),
                tool_results: tool_results.clone(),
                context: snapshot_context(inner),
                new_messages: inner.state.lock().messages.clone(),
            };
            if hook(ctx).await {
                return Ok(());
            }
        }

        // Whether to continue based on stop_reason + queue + tool-terminate hint.
        let continues = matches!(assistant.stop_reason, pie_ai::StopReason::ToolUse);
        if !tool_results.is_empty() && all_terminate {
            return Ok(());
        }

        // `prepare_next_turn` — caller may rewrite context/model/thinking_level mid-run.
        if let Some(hook) = inner.options.prepare_next_turn.clone() {
            let ctx = PrepareNextTurnContext {
                message: assistant.clone(),
                tool_results: tool_results.clone(),
                context: snapshot_context(inner),
                new_messages: inner.state.lock().messages.clone(),
            };
            if let Some(update) = hook(ctx).await {
                apply_turn_update(inner, update);
            }
        }

        let mut queued: Vec<AgentMessage> = inner.steering.lock().drain();
        if !continues && queued.is_empty() {
            queued = inner.follow_up.lock().drain();
        }
        if !queued.is_empty() {
            for msg in queued {
                inner.state.lock().messages.push(msg.clone());
                emit(
                    inner,
                    AgentEvent::MessageStart {
                        message: msg.clone(),
                    },
                    &cancel,
                )
                .await;
                emit(inner, AgentEvent::MessageEnd { message: msg }, &cancel).await;
            }
            continue;
        }
        if !continues {
            return Ok(());
        }
    }
}

fn apply_turn_update(inner: &Arc<AgentInner>, update: AgentLoopTurnUpdate) {
    let mut state = inner.state.lock();
    if let Some(ctx) = update.context {
        state.messages = ctx.messages;
        state.system_prompt = ctx.system_prompt;
        state.tools = ctx.tools;
    }
    if let Some(model) = update.model {
        state.model = Some(model);
    }
    if let Some(level) = update.thinking_level {
        state.thinking_level = Some(level);
    }
}

fn snapshot_context(inner: &Arc<AgentInner>) -> AgentContext {
    let g = inner.state.lock();
    AgentContext {
        system_prompt: g.system_prompt.clone(),
        messages: g.messages.clone(),
        tools: g.tools.clone(),
    }
}

async fn call_llm(
    inner: &Arc<AgentInner>,
    cancel: &CancellationToken,
) -> Result<PiAssistantMessage, AgentRunError> {
    let (system_prompt, agent_messages, tools, model) = {
        let g = inner.state.lock();
        let model = g.model.clone().ok_or_else(|| {
            AgentRunError::Other("Agent has no model set; assign state.model first".into())
        })?;
        (
            g.system_prompt.clone(),
            g.messages.clone(),
            g.tools.clone(),
            model,
        )
    };

    // `transform_context` runs before convert_to_llm so callers can prune / inject ephemeral
    // context without mutating persisted state.
    let agent_messages = if let Some(transform) = inner.options.transform_context.clone() {
        transform(agent_messages, cancel.clone()).await
    } else {
        agent_messages
    };

    let messages = inner.convert_to_llm(&agent_messages);
    let pi_tools: Vec<pie_ai::Tool> = tools.iter().map(|t| t.definition().clone()).collect();
    let context = PiContext {
        system_prompt: if system_prompt.is_empty() {
            None
        } else {
            Some(system_prompt)
        },
        messages,
        tools: if pi_tools.is_empty() {
            None
        } else {
            Some(pi_tools)
        },
    };

    let stream_fn = inner
        .options
        .stream_fn
        .clone()
        .unwrap_or_else(default_stream_fn);
    let mut options = SimpleStreamOptions::default();
    if let Some(sid) = &inner.options.session_id {
        options.base.session_id = Some(sid.clone());
    }
    options.base.abort = Some(cancel.clone());
    if let Some(level) = inner
        .state
        .lock()
        .thinking_level
        .and_then(|l| l.to_pie_ai())
    {
        options.reasoning = Some(level);
    }

    let mut stream = stream_fn(&model, &context, Some(&options));
    let mut last_message: Option<PiAssistantMessage> = None;
    loop {
        // Race the stream's next event against the cancellation token. Polling order is
        // biased toward cancellation so a Ctrl-C arriving mid-stall doesn't have to wait
        // for the next provider event to flush before we bail out. Closes #18.
        let ev = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(AgentRunError::Other("aborted".into()));
            }
            next = stream.next() => match next {
                Some(ev) => ev,
                None => break,
            }
        };
        match &ev {
            AssistantMessageEvent::Start { partial } => {
                last_message = Some(partial.clone());
                let m = AgentMessage::Llm(PiMessage::Assistant(partial.clone()));
                emit(
                    inner,
                    AgentEvent::MessageStart { message: m.clone() },
                    cancel,
                )
                .await;
                inner.state.lock().streaming_message = Some(m);
            }
            AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                last_message = Some(partial.clone());
                let m = AgentMessage::Llm(PiMessage::Assistant(partial.clone()));
                inner.state.lock().streaming_message = Some(m.clone());
                emit(
                    inner,
                    AgentEvent::MessageUpdate {
                        message: m,
                        assistant_message_event: ev.clone(),
                    },
                    cancel,
                )
                .await;
            }
            AssistantMessageEvent::Done { message, .. } => {
                last_message = Some(message.clone());
            }
            AssistantMessageEvent::Error { error, .. } => {
                // last_message would be overwritten by `return Err` below; don't bother.
                let msg = error.error_message.clone().unwrap_or_default();
                inner.state.lock().streaming_message = None;
                return Err(AgentRunError::Other(msg));
            }
            _ => {}
        }
    }
    inner.state.lock().streaming_message = None;
    last_message.ok_or_else(|| AgentRunError::Other("LLM stream produced no message".into()))
}

/// Execute every tool-call block in the assistant's content. Returns the per-call results
/// (in assistant content order) and `all_terminate = true` when every result hints early
/// termination.
async fn execute_tools(
    inner: &Arc<AgentInner>,
    assistant: &PiAssistantMessage,
    cancel: &CancellationToken,
) -> (Vec<ToolResultMessage>, bool) {
    // Gather the tool calls + matched AgentTool implementations in assistant content order.
    let tool_calls: Vec<&pie_ai::ToolCall> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            pie_ai::ContentBlock::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    if tool_calls.is_empty() {
        return (Vec::new(), false);
    }
    let tools_snapshot = inner.state.lock().tools.clone();

    // Decide per-call execution mode (parallel default unless any tool requests sequential).
    let mode = inner.options.tool_execution;
    let any_sequential = tool_calls.iter().any(|tc| {
        let matched = tools_snapshot
            .iter()
            .find(|t| t.definition().name == tc.name);
        matched
            .and_then(|t| t.execution_mode())
            .map(|m| matches!(m, ToolExecutionMode::Sequential))
            .unwrap_or(false)
    });
    let mode = if any_sequential {
        ToolExecutionMode::Sequential
    } else {
        mode
    };

    // Pre-flight: run `before_tool_call` for every call. If a hook blocks, synthesize an error
    // result and skip the actual execute. Returns Vec<Option<execute_input>> in call order.
    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(tool_calls.len());
    let agent_context = snapshot_context(inner);
    for tc in &tool_calls {
        let tool_id = tc.id.clone();
        let tool_name = tc.name.clone();
        let raw_args = serde_json::Value::Object(tc.arguments.clone());

        // Resolve the tool BEFORE normalizing args so we can run its `prepare_arguments`
        // compatibility shim. Unknown tools keep raw args (the dispatcher will produce a
        // "no such tool" error result downstream).
        let tool = tools_snapshot
            .iter()
            .find(|t| t.definition().name == tool_name)
            .cloned();
        let args = match &tool {
            Some(t) => t.prepare_arguments(raw_args),
            None => raw_args,
        };

        emit(
            inner,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
            },
            cancel,
        )
        .await;

        // before_tool_call hook can veto. The hook sees the prepared args on BOTH
        // `ctx.args` and `ctx.tool_call.arguments` — there is no reason to expose two
        // shapes of the same call (a hook reading `tool_call.arguments` would otherwise
        // miss any normalization the tool's `prepare_arguments` applied). If the tool's
        // `prepare_arguments` returns a non-Object shape (Null, Array, scalar), we cannot
        // represent it inside the `pie_ai::ToolCall.arguments` map; we clear the map to
        // empty so the hook author has only one truthy source (`ctx.args`) and cannot read
        // a stale raw map.
        if let Some(hook) = inner.options.before_tool_call.clone() {
            let mut hook_tc = (*tc).clone();
            hook_tc.arguments = match &args {
                serde_json::Value::Object(map) => map.clone(),
                _ => serde_json::Map::new(),
            };
            let ctx = BeforeToolCallContext {
                assistant_message: assistant.clone(),
                tool_call: hook_tc,
                args: args.clone(),
                context: agent_context.clone(),
            };
            let veto = hook(ctx, cancel.clone()).await;
            if veto.block {
                let reason = veto
                    .reason
                    .unwrap_or_else(|| "tool call blocked by before_tool_call hook".to_string());
                let result = AgentToolResult {
                    content: vec![UserContentBlock::text(reason)],
                    details: serde_json::Value::Null,
                    terminate: None,
                };
                prepared.push(PreparedCall::Blocked {
                    id: tool_id,
                    name: tool_name,
                    args,
                    result,
                });
                continue;
            }
        }

        prepared.push(PreparedCall::Run {
            id: tool_id,
            name: tool_name,
            args,
            tool,
        });
    }

    // Execute. For sequential we await one at a time; for parallel we spawn and join.
    let outcomes = match mode {
        ToolExecutionMode::Sequential => {
            let mut out = Vec::with_capacity(prepared.len());
            for call in prepared {
                out.push(run_one(inner.clone(), call, cancel.clone()).await);
            }
            out
        }
        ToolExecutionMode::Parallel => {
            let handles: Vec<_> = prepared
                .into_iter()
                .map(|call| {
                    let cancel = cancel.clone();
                    let inner = inner.clone();
                    tokio::spawn(async move { run_one(inner, call, cancel).await })
                })
                .collect();
            let mut out = Vec::with_capacity(handles.len());
            for h in handles {
                out.push(h.await.unwrap_or_else(|e| ToolOutcome {
                    id: String::new(),
                    name: String::new(),
                    args: serde_json::Value::Null,
                    result: AgentToolResult {
                        content: vec![UserContentBlock::text(format!("tool task join: {e}"))],
                        details: serde_json::Value::Null,
                        terminate: None,
                    },
                    is_error: true,
                }));
            }
            out
        }
    };

    // Post-process: run after_tool_call hooks (which may override), emit tool_execution_end,
    // build tool-result messages.
    let mut results = Vec::with_capacity(outcomes.len());
    let mut all_terminate = !outcomes.is_empty();
    let agent_context = snapshot_context(inner);
    for outcome in outcomes {
        let ToolOutcome {
            id,
            name,
            args,
            mut result,
            mut is_error,
        } = outcome;

        if let Some(hook) = inner.options.after_tool_call.clone() {
            let ctx = AfterToolCallContext {
                assistant_message: assistant.clone(),
                tool_call: pie_ai::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: args.as_object().cloned().unwrap_or_default(),
                    thought_signature: None,
                },
                args: args.clone(),
                result: result.clone(),
                is_error,
                context: agent_context.clone(),
            };
            let patch = hook(ctx, cancel.clone()).await;
            if let Some(content) = patch.content {
                result.content = content;
            }
            if let Some(details) = patch.details {
                result.details = details;
            }
            if let Some(err) = patch.is_error {
                is_error = err;
            }
            if let Some(t) = patch.terminate {
                result.terminate = Some(t);
            }
        }

        if !result.terminate.unwrap_or(false) {
            all_terminate = false;
        }

        emit(
            inner,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                result: result.clone(),
                is_error,
            },
            cancel,
        )
        .await;

        results.push(ToolResultMessage {
            role: pie_ai::ToolResultRole::ToolResult,
            tool_call_id: id,
            tool_name: name,
            content: result.content,
            details: Some(result.details),
            is_error,
            timestamp: chrono::Utc::now().timestamp_millis(),
        });
    }
    (results, all_terminate)
}

enum PreparedCall {
    Run {
        id: String,
        name: String,
        args: serde_json::Value,
        tool: Option<Arc<dyn AgentTool>>,
    },
    Blocked {
        id: String,
        name: String,
        args: serde_json::Value,
        result: AgentToolResult,
    },
}

struct ToolOutcome {
    id: String,
    name: String,
    args: serde_json::Value,
    result: AgentToolResult,
    is_error: bool,
}

async fn run_one(
    inner: Arc<AgentInner>,
    call: PreparedCall,
    cancel: CancellationToken,
) -> ToolOutcome {
    match call {
        PreparedCall::Blocked {
            id,
            name,
            args,
            result,
        } => ToolOutcome {
            id,
            name,
            args,
            result,
            is_error: true,
        },
        PreparedCall::Run {
            id,
            name,
            args,
            tool,
        } => match tool {
            Some(t) => {
                // Bridge the sync `AgentToolUpdate` callback to the async listener bus via
                // an unbounded mpsc channel + dedicated pump task. The pump emits
                // `ToolExecutionUpdate` events in send order; the sync callback never blocks
                // (`UnboundedSender::send` is non-async and just enqueues). The channel
                // closes when every sender is dropped, at which point `rx.recv()` returns
                // `None` and the pump task exits.
                //
                // Contract: `execute()` must NOT retain `on_update` past return — e.g. by
                // cloning the `Arc` into a `tokio::spawn`ed task. The wiring still has a
                // bounded shutdown path for the misbehaving case (see PUMP_JOIN_TIMEOUT
                // below), but updates the tool emits after `execute()` returns will be
                // dropped without reaching subscribers.
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentToolResult>();
                let pump_inner = inner.clone();
                let pump_id = id.clone();
                let pump_name = name.clone();
                let pump_args = args.clone();
                let pump_cancel = cancel.clone();
                let mut pump_handle = tokio::spawn(async move {
                    while let Some(partial) = rx.recv().await {
                        emit(
                            &pump_inner,
                            AgentEvent::ToolExecutionUpdate {
                                tool_call_id: pump_id.clone(),
                                tool_name: pump_name.clone(),
                                args: pump_args.clone(),
                                partial_result: partial,
                            },
                            &pump_cancel,
                        )
                        .await;
                    }
                });
                let on_update: AgentToolUpdate = {
                    let tx = tx.clone();
                    Arc::new(move |partial: AgentToolResult| {
                        // Best-effort: if the pump has closed (cancel/early exit), drop the
                        // update rather than panicking — tool authors should treat the
                        // callback as fire-and-forget.
                        let _ = tx.send(partial);
                    })
                };
                let exec_result = t.execute(&id, args.clone(), cancel, Some(on_update)).await;
                // Drop the outer-scope sender so the pump can finish in the well-behaved case
                // where the tool released its `Arc<on_update>` before returning. If the tool
                // misbehaved and kept the Arc alive (e.g. handed it to a `tokio::spawn`ed
                // task), the cloned sender inside the closure also stays alive and `rx.recv`
                // never returns `None`. The timeout + abort path below caps that case so
                // `run_one` cannot hang the whole agent loop. Updates that arrive after the
                // abort are dropped.
                drop(tx);
                const PUMP_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
                if tokio::time::timeout(PUMP_JOIN_TIMEOUT, &mut pump_handle)
                    .await
                    .is_err()
                {
                    pump_handle.abort();
                    let _ = pump_handle.await;
                }
                match exec_result {
                    Ok(r) => ToolOutcome {
                        id,
                        name,
                        args,
                        result: r,
                        is_error: false,
                    },
                    Err(e) => ToolOutcome {
                        id,
                        name,
                        args,
                        result: AgentToolResult {
                            content: vec![UserContentBlock::text(format!("{e}"))],
                            details: serde_json::Value::Null,
                            terminate: None,
                        },
                        is_error: true,
                    },
                }
            }
            None => ToolOutcome {
                id,
                name: name.clone(),
                args,
                result: AgentToolResult {
                    content: vec![UserContentBlock::text(format!(
                        "No tool registered named '{name}'"
                    ))],
                    details: serde_json::Value::Null,
                    terminate: None,
                },
                is_error: true,
            },
        },
    }
}

async fn emit(inner: &Arc<AgentInner>, event: AgentEvent, cancel: &CancellationToken) {
    let listeners = inner.listeners.lock().clone();
    for listener in listeners {
        let token = cancel.clone();
        listener(event.clone(), token).await;
    }
}

async fn finalize(inner: &Arc<AgentInner>, cancel: CancellationToken) {
    let messages = inner.state.lock().messages.clone();
    emit(inner, AgentEvent::AgentEnd { messages }, &cancel).await;
    inner.state.lock().is_streaming = false;
    *inner.active_cancel.lock() = None;
    inner.idle.notify_waiters();
}
