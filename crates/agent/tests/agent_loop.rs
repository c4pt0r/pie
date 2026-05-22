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
