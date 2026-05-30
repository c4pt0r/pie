//! End-to-end Agent loop test. Uses a synthetic `StreamFn` (via the AssistantMessageEventStream
//! sender API exposed by pie-ai) to drive the loop deterministically — no LLM calls.

use std::sync::Arc;

use pie_agent_core::{Agent, AgentEvent, AgentMessage, AgentOptions, AgentState, AgentTool};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, DoneReason, ModelCost, StopReason, ToolCall, Usage,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn faux_model() -> pie_ai::Model {
    pie_ai::Model {
        id: "faux".into(),
        name: "Faux".into(),
        api: pie_ai::Api::from("faux"),
        provider: pie_ai::Provider::from("faux"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

fn assistant_with(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
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
        stop_reason,
        error_message: None,
        timestamp: 0,
    }
}

fn faux_stream_fn_with(responses: Arc<Mutex<Vec<AssistantMessage>>>) -> pie_agent_core::StreamFn {
    Arc::new(move |_, _, _| {
        let (stream, mut sender) = AssistantMessageEventStream::new();
        let responses = responses.clone();
        tokio::spawn(async move {
            let msg = {
                let mut g = responses.lock().await;
                if g.is_empty() {
                    AssistantMessage {
                        stop_reason: StopReason::Stop,
                        ..assistant_with(vec![ContentBlock::text("done")], StopReason::Stop)
                    }
                } else {
                    g.remove(0)
                }
            };
            sender.push(AssistantMessageEvent::Start {
                partial: msg.clone(),
            });
            let reason = match msg.stop_reason {
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Length => DoneReason::Length,
                _ => DoneReason::Stop,
            };
            sender.push(AssistantMessageEvent::Done {
                reason,
                message: msg,
            });
        });
        stream
    })
}

#[tokio::test]
async fn single_turn_no_tools_emits_lifecycle_events() {
    let responses = Arc::new(Mutex::new(vec![assistant_with(
        vec![ContentBlock::text("hello there")],
        StopReason::Stop,
    )]));

    let mut state = AgentState::default();
    state.model = Some(faux_model());
    state.system_prompt = "be friendly".into();

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });

    let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let events_clone = events.clone();
    let _unsub = agent.subscribe(Arc::new(move |ev, _| {
        let events = events_clone.clone();
        Box::pin(async move {
            let tag = match ev {
                AgentEvent::AgentStart => "agent_start",
                AgentEvent::AgentEnd { .. } => "agent_end",
                AgentEvent::TurnStart => "turn_start",
                AgentEvent::TurnEnd { .. } => "turn_end",
                AgentEvent::MessageStart { .. } => "message_start",
                AgentEvent::MessageEnd { .. } => "message_end",
                AgentEvent::MessageUpdate { .. } => "message_update",
                AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
                AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
                AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
                AgentEvent::ControlPlanePromptResolved { .. } => "control_plane_prompt_resolved",
            };
            events.lock().unwrap().push(tag.to_string());
        })
    }));

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("hi".into()),
        timestamp: 0,
    }));
    agent.prompt(user).await.unwrap();

    let events = events.lock().unwrap();
    assert_eq!(events.first().map(String::as_str), Some("agent_start"));
    assert_eq!(events.last().map(String::as_str), Some("agent_end"));
    // Should contain at least one turn boundary.
    assert!(events.iter().any(|e| e == "turn_start"));
    assert!(events.iter().any(|e| e == "turn_end"));
    // Transcript should now include user + assistant.
    let g = agent.state();
    assert_eq!(g.messages.len(), 2);
}

#[tokio::test]
async fn tool_call_loops_until_non_tool_use_stop() {
    // The faux model first emits an assistant message with a tool call, then on the next call
    // emits a plain stop.
    let mut args = serde_json::Map::new();
    args.insert("x".into(), serde_json::json!(1));
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: args,
                thought_signature: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("ok")], StopReason::Stop),
    ]));

    // Faux echo tool — returns its `x` as text.
    struct EchoTool {
        def: pie_ai::Tool,
    }
    #[async_trait::async_trait]
    impl AgentTool for EchoTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "echo"
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            let x = params.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            Ok(pie_agent_core::AgentToolResult {
                content: vec![pie_ai::UserContentBlock::text(format!("got x={x}"))],
                details: serde_json::Value::Null,
                terminate: None,
            })
        }
    }

    let tool = Arc::new(EchoTool {
        def: pie_ai::Tool {
            name: "echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
    });

    let mut state = AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("compute".into()),
        timestamp: 0,
    }));
    agent.prompt(user).await.unwrap();

    let g = agent.state();
    // user → assistant#1 (tool_use) → toolResult → assistant#2 (stop)
    assert_eq!(g.messages.len(), 4);
    let tool_result_present = g.messages.iter().any(|m| {
        matches!(m, AgentMessage::Llm(pie_ai::Message::ToolResult(tr)) if tr.tool_call_id == "call_1")
    });
    assert!(tool_result_present);
}

#[tokio::test]
async fn before_tool_call_can_veto_execution() {
    use pie_agent_core::{BeforeToolCallContext, BeforeToolCallResult};

    let mut args = serde_json::Map::new();
    args.insert("x".into(), serde_json::json!(1));
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                arguments: args,
                thought_signature: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));

    struct EchoTool {
        def: pie_ai::Tool,
        called: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl pie_agent_core::AgentTool for EchoTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "echo"
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(pie_agent_core::AgentToolResult::default())
        }
    }

    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tool = Arc::new(EchoTool {
        def: pie_ai::Tool {
            name: "echo".into(),
            description: "echo".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        called: called.clone(),
    });

    let veto_hook: pie_agent_core::BeforeToolCallHook =
        Arc::new(|_ctx: BeforeToolCallContext, _cancel: CancellationToken| {
            Box::pin(async move {
                BeforeToolCallResult {
                    block: true,
                    reason: Some("policy: no echo".into()),
                    prompt: None,
                }
            })
        });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        before_tool_call: Some(veto_hook),
        ..Default::default()
    });

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("go".into()),
        timestamp: 0,
    }));
    agent.prompt(user).await.unwrap();

    assert!(
        !called.load(std::sync::atomic::Ordering::SeqCst),
        "tool must not run when hook blocks"
    );
    let g = agent.state();
    // The synthesized tool result should be is_error=true with the hook reason.
    let synth = g
        .messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::Llm(pie_ai::Message::ToolResult(tr)) => Some(tr),
            _ => None,
        })
        .expect("synth tool result");
    assert!(synth.is_error);
    let text = match &synth.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("policy: no echo"));
}

#[tokio::test]
async fn parallel_tools_execute_concurrently() {
    let mut args = serde_json::Map::new();
    args.insert("id".into(), serde_json::json!(1));
    let mut args2 = serde_json::Map::new();
    args2.insert("id".into(), serde_json::json!(2));
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "a".into(),
                    name: "slow".into(),
                    arguments: args,
                    thought_signature: None,
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "b".into(),
                    name: "slow".into(),
                    arguments: args2,
                    thought_signature: None,
                }),
            ],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));

    // Sleep 200ms per call — under parallel, total ≈200ms; sequential would be ≈400ms.
    struct SlowTool {
        def: pie_ai::Tool,
    }
    #[async_trait::async_trait]
    impl pie_agent_core::AgentTool for SlowTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "slow"
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(pie_agent_core::AgentToolResult::default())
        }
    }
    let tool = Arc::new(SlowTool {
        def: pie_ai::Tool {
            name: "slow".into(),
            description: "sleep".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];
    // tool_execution defaults to Parallel.

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("go".into()),
        timestamp: 0,
    }));
    let start = std::time::Instant::now();
    agent.prompt(user).await.unwrap();
    let elapsed = start.elapsed();
    // Parallel should finish in well under 400ms; allow 350ms for scheduler slack.
    assert!(
        elapsed < std::time::Duration::from_millis(350),
        "expected parallel tool exec, took {:?}",
        elapsed
    );
}

#[tokio::test]
async fn prepare_arguments_normalizes_args_for_hook_and_execute() {
    use pie_agent_core::{BeforeToolCallContext, BeforeToolCallResult};

    let mut raw = serde_json::Map::new();
    raw.insert("payload".into(), serde_json::json!("hello"));
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "uppercaser".into(),
                arguments: raw,
                thought_signature: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));

    /// Tool whose `prepare_arguments` upper-cases `payload`. If the agent loop forgot to
    /// invoke `prepare_arguments`, both the hook and execute paths would see "hello".
    struct UppercaserTool {
        def: pie_ai::Tool,
        execute_args: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }
    #[async_trait::async_trait]
    impl pie_agent_core::AgentTool for UppercaserTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "uppercaser"
        }
        fn prepare_arguments(&self, args: serde_json::Value) -> serde_json::Value {
            let mut map = args.as_object().cloned().unwrap_or_default();
            if let Some(v) = map.get("payload").and_then(|v| v.as_str()) {
                map.insert(
                    "payload".into(),
                    serde_json::Value::String(v.to_uppercase()),
                );
            }
            serde_json::Value::Object(map)
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            *self.execute_args.lock().unwrap() = Some(params);
            Ok(pie_agent_core::AgentToolResult::default())
        }
    }

    let hook_args = Arc::new(std::sync::Mutex::new(None));
    let execute_args = Arc::new(std::sync::Mutex::new(None));

    let tool = Arc::new(UppercaserTool {
        def: pie_ai::Tool {
            name: "uppercaser".into(),
            description: "uppercase payload".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        execute_args: execute_args.clone(),
    });

    let hook_sink = hook_args.clone();
    let observing_hook: pie_agent_core::BeforeToolCallHook = Arc::new(
        move |ctx: BeforeToolCallContext, _cancel: CancellationToken| {
            let sink = hook_sink.clone();
            Box::pin(async move {
                *sink.lock().unwrap() = Some(ctx.args);
                BeforeToolCallResult::default()
            })
        },
    );

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        before_tool_call: Some(observing_hook),
        ..Default::default()
    });

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("go".into()),
        timestamp: 0,
    }));
    agent.prompt(user).await.unwrap();

    let hook_seen = hook_args.lock().unwrap().clone().expect("hook fired");
    let exec_seen = execute_args.lock().unwrap().clone().expect("execute ran");
    assert_eq!(
        hook_seen.get("payload").and_then(|v| v.as_str()),
        Some("HELLO"),
        "before_tool_call hook must see prepared args, got {hook_seen:?}"
    );
    assert_eq!(
        exec_seen.get("payload").and_then(|v| v.as_str()),
        Some("HELLO"),
        "execute() must see prepared args, got {exec_seen:?}"
    );
}

#[tokio::test]
async fn tool_execution_update_callback_emits_listener_events_in_order() {
    let args = serde_json::Map::new();
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "progress".into(),
                arguments: args,
                thought_signature: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));

    /// Tool that fires three partial updates via `on_update` before returning. Verifies the
    /// callback Some/None plumbing reaches subscribers as `ToolExecutionUpdate` events.
    struct ProgressTool {
        def: pie_ai::Tool,
    }
    #[async_trait::async_trait]
    impl pie_agent_core::AgentTool for ProgressTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "progress"
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            let cb = on_update.expect(
                "agent loop must supply a real on_update callback — previously always None",
            );
            for label in ["step-1", "step-2", "step-3"] {
                cb(pie_agent_core::AgentToolResult {
                    content: vec![pie_ai::UserContentBlock::text(label.to_string())],
                    details: serde_json::Value::Null,
                    terminate: None,
                });
            }
            Ok(pie_agent_core::AgentToolResult::default())
        }
    }
    let tool = Arc::new(ProgressTool {
        def: pie_ai::Tool {
            name: "progress".into(),
            description: "emits partial updates".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });

    let captured_updates = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let sink = captured_updates.clone();
    let _unsub = agent.subscribe(Arc::new(move |ev, _| {
        let sink = sink.clone();
        Box::pin(async move {
            if let AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } = ev
            {
                if let Some(pie_ai::UserContentBlock::Text(t)) = partial_result.content.first() {
                    sink.lock().unwrap().push((tool_call_id, t.text.clone()));
                }
            }
        })
    }));

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("go".into()),
        timestamp: 0,
    }));
    agent.prompt(user).await.unwrap();

    let updates = captured_updates.lock().unwrap().clone();
    assert_eq!(
        updates,
        vec![
            ("call_1".to_string(), "step-1".to_string()),
            ("call_1".to_string(), "step-2".to_string()),
            ("call_1".to_string(), "step-3".to_string()),
        ],
        "ToolExecutionUpdate events must be delivered in send order with the correct tool_call_id"
    );
}

#[tokio::test]
async fn run_one_does_not_hang_when_tool_retains_on_update_past_return() {
    // Regression for the pump-handle hang concern @Tools-MCP-Lead and @QA-Release-Lead
    // raised on PR #49: a tool that hands `on_update` to a `tokio::spawn`ed task keeps an
    // Arc<closure> alive past `execute()` return, so the cloned `tx` inside the closure
    // stays alive and the pump task's `rx.recv()` would never return `None`. The agent
    // loop must time out the pump join and abort the task so `run_one` (and the whole
    // agent loop) cannot hang on a misbehaving tool.
    //
    // The bound is internal: `run_one` itself caps the join at ~2s. With the test wrapper
    // around `agent.prompt(...)` we expect the whole call to finish well under the safety
    // ceiling.
    let args = serde_json::Map::new();
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "leaker".into(),
                arguments: args,
                thought_signature: None,
            })],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));

    /// Misbehaving tool: hands `on_update` to a background task that holds it indefinitely.
    /// The retained Arc keeps the channel's cloned sender alive after `execute` returns.
    struct LeakerTool {
        def: pie_ai::Tool,
    }
    #[async_trait::async_trait]
    impl pie_agent_core::AgentTool for LeakerTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "leaker"
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            on_update: Option<pie_agent_core::AgentToolUpdate>,
        ) -> Result<pie_agent_core::AgentToolResult, pie_agent_core::AgentToolError> {
            let cb = on_update.expect("agent loop must supply callback");
            cb(pie_agent_core::AgentToolResult {
                content: vec![pie_ai::UserContentBlock::text("first-and-only".to_string())],
                details: serde_json::Value::Null,
                terminate: None,
            });
            // Hold the callback alive for far longer than the pump-join timeout. The agent
            // loop must abort the pump on timeout instead of waiting for this task to drop
            // the Arc.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                drop(cb);
            });
            Ok(pie_agent_core::AgentToolResult::default())
        }
    }
    let tool = Arc::new(LeakerTool {
        def: pie_ai::Tool {
            name: "leaker".into(),
            description: "retains on_update".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool];

    let agent = Agent::new(AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });

    let user = AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text("go".into()),
        timestamp: 0,
    }));

    let start = std::time::Instant::now();
    // Outer timeout much wider than the pump's internal 2s, so a hang would still surface
    // as the wrapper firing rather than the test runner's global timeout.
    tokio::time::timeout(std::time::Duration::from_secs(10), agent.prompt(user))
        .await
        .expect("agent.prompt must complete — pump join must time out, not block forever")
        .expect("agent.prompt itself must succeed");
    let elapsed = start.elapsed();
    // Loose ceiling: pump join is capped at 2s. Allow a generous 5s for full agent loop
    // turn including faux LLM round-trip + listener emit + state lock contention.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "expected run_one to return within ~2s after the tool returned, took {elapsed:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// Issue #110 — ControlPlaneWrite user-Prompt gate (design v0.2)
// ─────────────────────────────────────────────────────────────────────────────────────────

/// Shared test fixture: a counted EchoTool whose `permission_classification` is dictated by
/// the test. Test asserts on whether `execute` ran by inspecting `called`.
mod cpw_test_util {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use pie_agent_core::{
        AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, PermissionClassification,
    };
    use tokio_util::sync::CancellationToken;

    pub(super) struct ClassifierTool {
        pub(super) def: pie_ai::Tool,
        pub(super) classification: PermissionClassification,
        pub(super) called: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AgentTool for ClassifierTool {
        fn definition(&self) -> &pie_ai::Tool {
            &self.def
        }
        fn label(&self) -> &str {
            "classifier"
        }
        fn permission_classification(
            &self,
            _prepared_args: &serde_json::Value,
        ) -> PermissionClassification {
            self.classification.clone()
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _cancel: CancellationToken,
            _on_update: Option<AgentToolUpdate>,
        ) -> Result<AgentToolResult, AgentToolError> {
            self.called.store(true, Ordering::SeqCst);
            Ok(AgentToolResult {
                content: vec![pie_ai::UserContentBlock::text("did run")],
                details: serde_json::Value::Null,
                terminate: None,
            })
        }
    }

    pub(super) fn tool_call_for(name: &str) -> pie_ai::ToolCall {
        let mut args = serde_json::Map::new();
        args.insert("x".into(), serde_json::json!(1));
        pie_ai::ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        }
    }
}

#[tokio::test]
async fn permission_classification_default_allow_keeps_legacy_behavior() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{AgentTool, PermissionClassification};

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Allow,
        called: called.clone(),
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        called.load(Ordering::SeqCst),
        "default Allow classification must let the tool execute"
    );
}

#[tokio::test]
async fn permission_classification_block_short_circuits_before_hook_and_execute() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{AgentTool, PermissionClassification};

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Block {
            reason: "blocked: hard refusal".into(),
        },
        called: called.clone(),
    });

    // Even if the user wires a `before_tool_call` hook that returns Allow, the Block
    // classification must short-circuit before the hook is invoked.
    let hook_called = Arc::new(AtomicBool::new(false));
    let hook_called_clone = hook_called.clone();
    let before_hook: pie_agent_core::BeforeToolCallHook = Arc::new(
        move |_ctx: pie_agent_core::BeforeToolCallContext, _cancel: CancellationToken| {
            let hc = hook_called_clone.clone();
            Box::pin(async move {
                hc.store(true, Ordering::SeqCst);
                pie_agent_core::BeforeToolCallResult::default()
            })
        },
    );

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        before_tool_call: Some(before_hook),
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "Block classification must skip tool execution"
    );
    assert!(
        !hook_called.load(Ordering::SeqCst),
        "Block classification must short-circuit before before_tool_call"
    );
}

#[tokio::test]
async fn permission_classification_prompt_with_no_hook_fails_closed() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{AgentTool, PermissionClassification};

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "control-plane write".into(),
        },
        called: called.clone(),
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        // No on_control_plane_prompt hook configured.
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "Prompt classification with no resolution hook must fail-closed deny",
    );
}

#[tokio::test]
async fn permission_classification_prompt_with_hook_allow_executes_and_emits_audit_event() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{
        AgentEvent, AgentTool, ControlPlanePromptDecision, OnControlPlanePromptHook,
        PermissionClassification,
    };

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "control-plane write".into(),
        },
        called: called.clone(),
    });

    let observed_label: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let observed_args_hash: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let prompt_hook: OnControlPlanePromptHook = {
        let label = observed_label.clone();
        let hash = observed_args_hash.clone();
        Arc::new(move |req, _cancel| {
            *label.lock().unwrap() = Some(req.label.clone());
            *hash.lock().unwrap() = Some(req.args_hash.clone());
            Box::pin(async move { ControlPlanePromptDecision::Allow })
        })
    };

    let events: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let events_clone = events.clone();
    let listener: pie_agent_core::AgentListener = Arc::new(move |ev, _cancel| {
        let events = events_clone.clone();
        Box::pin(async move {
            if let AgentEvent::ControlPlanePromptResolved {
                tool_name,
                decision,
                args_hash,
                ..
            } = ev
            {
                events
                    .lock()
                    .unwrap()
                    .push(format!("{tool_name}:{decision}:{args_hash}"));
            }
        })
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        on_control_plane_prompt: Some(prompt_hook),
        ..Default::default()
    });
    let _unsub = agent.subscribe(listener);
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        called.load(Ordering::SeqCst),
        "Prompt + Allow decision must let the tool execute"
    );
    let lbl = observed_label.lock().unwrap().clone().unwrap_or_default();
    assert!(
        lbl.contains("classifier"),
        "prompt label must mention the tool name, got {lbl:?}"
    );
    let hash = observed_args_hash
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    assert_eq!(
        hash.len(),
        64,
        "args_hash must be 64-hex (SHA-256), got len={}",
        hash.len()
    );
    let evs = events.lock().unwrap().clone();
    assert_eq!(
        evs.len(),
        1,
        "expected one ControlPlanePromptResolved event"
    );
    assert!(
        evs[0].starts_with("classifier:allow:"),
        "audit event missing decision=allow, got {:?}",
        evs[0]
    );
}

#[tokio::test]
async fn permission_classification_prompt_with_hook_deny_blocks_and_emits_audit_event() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{
        AgentEvent, AgentTool, ControlPlanePromptDecision, OnControlPlanePromptHook,
        PermissionClassification,
    };

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "control-plane write".into(),
        },
        called: called.clone(),
    });
    let prompt_hook: OnControlPlanePromptHook = Arc::new(|_req, _cancel| {
        Box::pin(async move {
            ControlPlanePromptDecision::Deny {
                reason: Some("user said no".into()),
            }
        })
    });
    let events: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ec = events.clone();
    let listener: pie_agent_core::AgentListener = Arc::new(move |ev, _cancel| {
        let ec = ec.clone();
        Box::pin(async move {
            if let AgentEvent::ControlPlanePromptResolved { decision, .. } = ev {
                ec.lock().unwrap().push(decision);
            }
        })
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        on_control_plane_prompt: Some(prompt_hook),
        ..Default::default()
    });
    let _unsub = agent.subscribe(listener);
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "Deny decision must skip tool execution"
    );
    let evs = events.lock().unwrap().clone();
    assert_eq!(evs, vec!["deny".to_string()]);
}

/// Regression test for the merge-semantics blocker @Provider-Auth-Lead /
/// @CLI-TUI-Dev-Lead / @QA-Release-Lead flagged on PR #135 v1: a benign
/// `before_tool_call` hook that returns `BeforeToolCallResult::default()` MUST NOT
/// silently erase a classifier-required Prompt. The runtime must still route through
/// the prompt channel.
#[tokio::test]
async fn classifier_prompt_preserved_through_default_before_tool_call_hook() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use cpw_test_util::*;
    use pie_agent_core::{
        AgentTool, ControlPlanePromptDecision, OnControlPlanePromptHook, PermissionClassification,
    };

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "control-plane write".into(),
        },
        called: called.clone(),
    });

    // Benign `before_tool_call` hook that just returns `default()` — analogous to a
    // permission policy that has nothing to say about a particular tool. Must NOT
    // erase the classifier's Prompt requirement.
    let before_hook: pie_agent_core::BeforeToolCallHook = Arc::new(
        move |_ctx: pie_agent_core::BeforeToolCallContext, _cancel: CancellationToken| {
            Box::pin(async move { pie_agent_core::BeforeToolCallResult::default() })
        },
    );

    // Track whether the prompt channel was actually reached.
    let prompt_called = Arc::new(AtomicBool::new(false));
    let pc = prompt_called.clone();
    let prompt_hook: OnControlPlanePromptHook = Arc::new(move |_req, _cancel| {
        let pc = pc.clone();
        Box::pin(async move {
            pc.store(true, Ordering::SeqCst);
            // Deny so the test asserts on both the prompt-channel-was-called and
            // tool-was-not-executed sides of the invariant.
            ControlPlanePromptDecision::Deny {
                reason: Some("test denied".into()),
            }
        })
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        before_tool_call: Some(before_hook),
        on_control_plane_prompt: Some(prompt_hook),
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    assert!(
        prompt_called.load(Ordering::SeqCst),
        "classifier Prompt must reach the on_control_plane_prompt hook even when a \
         benign before_tool_call returns default()",
    );
    assert!(
        !called.load(Ordering::SeqCst),
        "Deny decision must still skip tool execution",
    );
}

/// Regression test for the binding-spoofing blocker @Provider-Auth-Lead flagged on
/// PR #135 v1: when a `before_tool_call` hook supplies its own richer prompt payload,
/// the runtime MUST re-bind `tool_call_id` / `tool_name` / `args_hash` to the
/// authoritative values. A malicious or buggy hook cannot lie about binding fields.
#[tokio::test]
async fn runtime_overrides_hook_supplied_binding_fields_in_prompt() {
    use std::sync::atomic::Ordering;

    use cpw_test_util::*;
    use parking_lot::Mutex as PMutex;
    use pie_agent_core::{
        AgentTool, ControlPlanePromptDecision, ControlPlanePromptRequest, OnControlPlanePromptHook,
        PermissionClassification,
    };

    let responses = Arc::new(Mutex::new(vec![
        assistant_with(
            vec![ContentBlock::ToolCall(tool_call_for("classifier"))],
            StopReason::ToolUse,
        ),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "real reason".into(),
        },
        called: called.clone(),
    });

    // Hook tries to spoof binding fields. Runtime must overwrite them with authoritative
    // values; only `label` and `payload` survive.
    let before_hook: pie_agent_core::BeforeToolCallHook = Arc::new(
        move |_ctx: pie_agent_core::BeforeToolCallContext, _cancel: CancellationToken| {
            Box::pin(async move {
                pie_agent_core::BeforeToolCallResult {
                    block: false,
                    reason: None,
                    prompt: Some(ControlPlanePromptRequest {
                        tool_call_id: "SPOOFED_CALL_ID".into(),
                        tool_name: "spoofed_tool".into(),
                        args_hash: "DEADBEEF".into(),
                        label: "richer label".into(),
                        payload: serde_json::json!({ "richer": "payload" }),
                        reason: "spoofed reason".into(),
                    }),
                }
            })
        },
    );

    let observed_req: Arc<PMutex<Option<ControlPlanePromptRequest>>> = Arc::new(PMutex::new(None));
    let or = observed_req.clone();
    let prompt_hook: OnControlPlanePromptHook = Arc::new(move |req, _cancel| {
        *or.lock() = Some(req);
        Box::pin(async move { ControlPlanePromptDecision::Allow })
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        before_tool_call: Some(before_hook),
        on_control_plane_prompt: Some(prompt_hook),
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    let req = observed_req.lock().take().expect("prompt hook ran");
    assert_eq!(
        req.tool_call_id, "call_1",
        "runtime must overwrite hook-supplied tool_call_id with the real call id",
    );
    assert_eq!(
        req.tool_name, "classifier",
        "runtime must overwrite hook-supplied tool_name with the real tool name",
    );
    assert_eq!(
        req.args_hash.len(),
        64,
        "runtime must compute authoritative args_hash (64-hex SHA-256), not accept the \
         spoofed 'DEADBEEF', got {:?}",
        req.args_hash
    );
    assert_ne!(
        req.args_hash, "DEADBEEF",
        "runtime must reject spoofed args_hash",
    );
    assert_eq!(
        req.reason, "real reason",
        "runtime must keep the classifier's reason; hook cannot rewrite the gate reason",
    );
    // The hook IS allowed to enrich label and payload.
    assert_eq!(req.label, "richer label");
    assert_eq!(req.payload["richer"], "payload");
    assert!(
        called.load(Ordering::SeqCst),
        "Allow decision lets the tool execute"
    );
}

/// Regression test for the default-payload secrecy blocker @Provider-Auth-Lead /
/// @CLI-TUI-Dev-Lead flagged on PR #135 v1: the runtime-synthesized default prompt
/// payload MUST NOT include raw prepared args values. Only safe metadata: tool_name,
/// args_keys (names only), args_hash.
#[tokio::test]
async fn default_prompt_payload_does_not_include_raw_args_values() {
    use parking_lot::Mutex as PMutex;

    use cpw_test_util::*;
    use pie_agent_core::{
        AgentTool, ControlPlanePromptDecision, ControlPlanePromptRequest, OnControlPlanePromptHook,
        PermissionClassification,
    };

    // Args carry a secret-bearing value. Default payload must not leak it.
    let mut args_map = serde_json::Map::new();
    args_map.insert(
        "install_url".into(),
        serde_json::json!("https://example.com/skill.md?token=super-secret-leakable-12345"),
    );
    args_map.insert("confirm".into(), serde_json::json!(true));
    let tool_call = pie_ai::ToolCall {
        id: "call_1".into(),
        name: "classifier".into(),
        arguments: args_map,
        thought_signature: None,
    };
    let responses = Arc::new(Mutex::new(vec![
        assistant_with(vec![ContentBlock::ToolCall(tool_call)], StopReason::ToolUse),
        assistant_with(vec![ContentBlock::text("done")], StopReason::Stop),
    ]));
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tool = Arc::new(ClassifierTool {
        def: pie_ai::Tool {
            name: "classifier".into(),
            description: "".into(),
            parameters: serde_json::json!({ "type": "object" }),
        },
        classification: PermissionClassification::Prompt {
            reason: "install third-party skill".into(),
        },
        called: called.clone(),
    });

    let observed_req: Arc<PMutex<Option<ControlPlanePromptRequest>>> = Arc::new(PMutex::new(None));
    let or = observed_req.clone();
    let prompt_hook: OnControlPlanePromptHook = Arc::new(move |req, _cancel| {
        *or.lock() = Some(req);
        Box::pin(async move {
            ControlPlanePromptDecision::Deny {
                reason: Some("test".into()),
            }
        })
    });

    let mut state = pie_agent_core::AgentState::default();
    state.model = Some(faux_model());
    state.tools = vec![tool as Arc<dyn AgentTool>];
    let agent = pie_agent_core::Agent::new(pie_agent_core::AgentOptions {
        initial_state: Some(state),
        stream_fn: Some(faux_stream_fn_with(responses)),
        on_control_plane_prompt: Some(prompt_hook),
        ..Default::default()
    });
    agent
        .prompt(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("run".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();
    let req = observed_req.lock().take().expect("prompt hook ran");
    let payload_str = serde_json::to_string(&req.payload).unwrap();
    assert!(
        !payload_str.contains("super-secret-leakable-12345"),
        "default prompt payload must not contain raw arg values; got: {payload_str}",
    );
    assert!(
        !payload_str.contains("token=super-secret"),
        "default prompt payload must not contain URL with secret; got: {payload_str}",
    );
    // Payload must contain the safe metadata (keys + hash).
    let keys = req.payload["args_keys"]
        .as_array()
        .expect("args_keys array");
    let key_names: Vec<&str> = keys.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        key_names.contains(&"install_url"),
        "args_keys must list key names; got {key_names:?}",
    );
    assert!(
        key_names.contains(&"confirm"),
        "args_keys must list key names; got {key_names:?}",
    );
    let hash = req.payload["args_hash"].as_str().expect("args_hash string");
    assert_eq!(
        hash.len(),
        64,
        "args_hash in payload must be 64-hex SHA-256"
    );
    assert_eq!(
        hash, req.args_hash,
        "payload args_hash must match the binding field args_hash",
    );
}
