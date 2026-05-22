//! End-to-end AgentHarness test. Wires Agent + Session + a synthetic StreamFn and verifies the
//! prompt → assistant → session-persist cycle.

use std::sync::Arc;

use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentMessage, CompactionSettings, HarnessEvent,
    HarnessListener, JsonlSessionRepo, MemorySessionStorage, Session, SessionError,
    SessionErrorCode, SessionStorage, SessionTreeEntry, Skill, StreamFn, ThinkingLevel,
    build_session_context,
};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, DoneReason, ModelCost, StopReason, Usage,
};

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

fn faux_stream_fn(text: &'static str) -> StreamFn {
    Arc::new(move |_, _, _| {
        let (stream, mut sender) = AssistantMessageEventStream::new();
        tokio::spawn(async move {
            let msg = AssistantMessage {
                role: AssistantRole::Assistant,
                content: vec![ContentBlock::text(text)],
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
            };
            sender.push(AssistantMessageEvent::Start {
                partial: msg.clone(),
            });
            sender.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: msg,
            });
        });
        stream
    })
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(text.into()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }))
}

#[tokio::test]
async fn prompt_persists_user_and_assistant_to_session() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);

    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.system_prompt = "You are helpful.".into();
    opts.stream_fn = Some(faux_stream_fn("hello world"));
    let harness = AgentHarness::new(opts);

    assert!(harness.system_prompt().starts_with("You are helpful."));
    harness.prompt("hi").await.unwrap();

    let entries = session.entries().await.unwrap();
    // Should contain: user message + assistant message (both AgentMessage::Llm).
    assert!(
        entries.len() >= 2,
        "expected at least 2 entries, got {}",
        entries.len()
    );
    let has_assistant = entries.iter().any(|e| {
        matches!(
            e,
            pie_agent_core::SessionTreeEntry::Message {
                message: pie_agent_core::AgentMessage::Llm(pie_ai::Message::Assistant(_)),
                ..
            }
        )
    });
    assert!(has_assistant);
}

#[tokio::test]
async fn prompt_reports_session_persistence_failures() {
    struct FailingAppendStorage;

    #[async_trait::async_trait]
    impl SessionStorage for FailingAppendStorage {
        async fn get_metadata_json(&self) -> Result<serde_json::Value, SessionError> {
            Ok(serde_json::json!({"id": "fail", "createdAt": "now"}))
        }
        async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
            Ok(None)
        }
        async fn set_leaf_id(&self, _id: Option<String>) -> Result<(), SessionError> {
            Ok(())
        }
        async fn create_entry_id(&self) -> Result<String, SessionError> {
            Ok("entry".into())
        }
        async fn append_entry(&self, _entry: SessionTreeEntry) -> Result<(), SessionError> {
            Err(SessionError {
                code: SessionErrorCode::StorageFailure,
                message: "disk full".into(),
            })
        }
        async fn get_entry(&self, _id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
            Ok(None)
        }
        async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
            Ok(Vec::new())
        }
        async fn get_path_to_root(
            &self,
            _leaf_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            Ok(Vec::new())
        }
        async fn find_entries(
            &self,
            _entry_type: &str,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            Ok(Vec::new())
        }
        async fn get_label(&self, _id: &str) -> Result<Option<String>, SessionError> {
            Ok(None)
        }
    }

    let session = Session::new(Arc::new(FailingAppendStorage) as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    let err = harness.prompt("hi").await.unwrap_err().to_string();
    assert!(err.contains("session append message"));
    assert!(err.contains("disk full"));
}

#[tokio::test]
async fn move_to_rehydrates_thinking_level_from_session_context() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    session.append_thinking_level_change("high").await.unwrap();
    let msg_id = session.append_message(user_message("hi")).await.unwrap();

    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.thinking_level = ThinkingLevel::Off;
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    harness.move_to(Some(&msg_id), None).await.unwrap();

    assert_eq!(
        harness.agent().state().thinking_level,
        Some(ThinkingLevel::High)
    );
}

#[tokio::test]
async fn skills_block_appears_in_system_prompt() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    let skill = Skill {
        name: "my-skill".into(),
        description: "does things".into(),
        file_path: "/skills/my-skill/SKILL.md".into(),
        content: "the body".into(),
        disable_model_invocation: false,
    };
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.system_prompt = "Base.".into();
    opts.thinking_level = ThinkingLevel::Medium;
    opts.skills = vec![skill];
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    let prompt = harness.system_prompt();
    assert!(prompt.starts_with("Base."));
    assert!(prompt.contains("<skills>"));
    assert!(prompt.contains("- name: my-skill"));
}

#[tokio::test]
async fn set_model_persists_to_session() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    let model_a = faux_model();
    let mut opts = AgentHarnessOptions::new(model_a.clone(), session.clone());
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    let mut model_b = faux_model();
    model_b.id = "faux-v2".into();
    harness.set_model(model_b.clone()).await.unwrap();
    harness
        .set_thinking_level(pie_agent_core::ThinkingLevel::Medium)
        .await
        .unwrap();

    let entries = session.entries().await.unwrap();
    assert!(entries.iter().any(|e| matches!(e,
        pie_agent_core::SessionTreeEntry::ModelChange { model_id, .. } if model_id == "faux-v2"
    )));
    assert!(entries.iter().any(|e| matches!(e,
        pie_agent_core::SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } if thinking_level == "medium"
    )));
}

#[tokio::test]
async fn prompt_from_template_interpolates_and_runs() {
    use pie_agent_core::PromptTemplate;
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(faux_stream_fn("template-resp"));
    opts.prompt_templates = vec![PromptTemplate {
        name: "greet".into(),
        description: None,
        content: "Say hi to {{name}}".into(),
        file_path: "/tpl/greet.md".into(),
    }];
    let harness = AgentHarness::new(opts);

    let mut vars = serde_json::Map::new();
    vars.insert("name".into(), serde_json::json!("world"));
    harness.prompt_from_template("greet", vars).await.unwrap();

    // First persisted user message should have the interpolated text.
    let entries = session.entries().await.unwrap();
    let has_interpolated = entries.iter().any(|e| match e {
        pie_agent_core::SessionTreeEntry::Message {
            message: pie_agent_core::AgentMessage::Llm(pie_ai::Message::User(u)),
            ..
        } => matches!(&u.content, pie_ai::UserContent::Text(s) if s == "Say hi to world"),
        _ => false,
    });
    assert!(
        has_interpolated,
        "expected interpolated user message; entries={:#?}",
        entries
    );
}

#[tokio::test]
async fn rehydrate_from_session_restores_messages_model_thinking() {
    use pie_agent_core::{AgentMessage, ThinkingLevel};

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    // Seed the session with a thinking-level change, a model change, and one user message —
    // simulating an earlier session the next harness is meant to pick up.
    session.append_thinking_level_change("high").await.unwrap();
    session.append_model_change("faux", "faux").await.unwrap();
    session
        .append_message(AgentMessage::Llm(pie_ai::Message::User(
            pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("earlier user prompt".into()),
                timestamp: 0,
            },
        )))
        .await
        .unwrap();

    // Build a harness whose initial state has *neither* the seeded model nor the high thinking
    // level — rehydrate must overwrite both.
    let cold_model = faux_model();
    let mut opts = AgentHarnessOptions::new(cold_model.clone(), session.clone());
    opts.thinking_level = ThinkingLevel::Off;
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    let ctx = harness.rehydrate_from_session().await.unwrap();
    assert_eq!(ctx.thinking_level, "high");
    assert_eq!(ctx.model.as_ref().unwrap().model_id, "faux");

    let state = harness.agent().state();
    assert_eq!(state.messages.len(), 1);
    assert_eq!(state.thinking_level, Some(ThinkingLevel::High));
    // Model is restored only when the catalog has the (provider, id) pair. The faux model is
    // not in the catalog, so we just check the API didn't blow away the cold-start model.
    assert!(state.model.is_some());
}

/// Subscribing to the harness event bus must surface SessionStart on first prompt and Branch
/// on move_to. SessionStart is exactly-once over the harness lifetime.
#[tokio::test]
async fn harness_event_bus_delivers_session_and_branch() {
    use parking_lot::Mutex;

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(faux_stream_fn("ack"));
    let harness = AgentHarness::new(opts);

    let received: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let r2 = received.clone();
    let listener: HarnessListener = Arc::new(move |ev| {
        r2.lock().push(ev);
    });
    let _unsub = harness.subscribe_harness(listener);

    harness.prompt("hello").await.unwrap();
    harness.move_to(None, None).await.unwrap();

    let events = received.lock().clone();
    let kinds: Vec<&'static str> = events
        .iter()
        .map(|e| match e {
            HarnessEvent::SessionStart { .. } => "SessionStart",
            HarnessEvent::Compaction { .. } => "Compaction",
            HarnessEvent::Branch { .. } => "Branch",
            HarnessEvent::TriggerHandlingStart { .. } => "TriggerHandlingStart",
            HarnessEvent::TriggerHandled { .. } => "TriggerHandled",
            HarnessEvent::PersistenceError { .. } => "PersistenceError",
        })
        .collect();
    assert!(
        kinds.contains(&"SessionStart"),
        "expected SessionStart in {kinds:?}"
    );
    assert!(kinds.contains(&"Branch"), "expected Branch in {kinds:?}");

    harness.prompt("again").await.unwrap();
    let count_after = received
        .lock()
        .iter()
        .filter(|e| matches!(e, HarnessEvent::SessionStart { .. }))
        .count();
    assert_eq!(
        count_after, 1,
        "SessionStart must be exactly-once over the lifetime of a harness"
    );
}

/// Budget cap (issue #7): once the running cost crosses the configured USD cap, the next
/// prompt is rejected with a clear error before any LLM call is dispatched.
#[tokio::test]
async fn budget_cap_blocks_new_prompts_after_cap_reached() {
    use pie_ai::UsageCost;

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    // Deterministic usage that exceeds a $0.05 cap on the first turn.
    let usage = Usage {
        input: 10,
        output: 5,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 15,
        cost: UsageCost {
            input: 0.04,
            output: 0.02,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.06,
        },
    };
    let stream: StreamFn = {
        let usage = usage.clone();
        Arc::new(move |_, _, _| {
            let usage = usage.clone();
            let (stream, mut sender) = AssistantMessageEventStream::new();
            tokio::spawn(async move {
                let msg = AssistantMessage {
                    role: AssistantRole::Assistant,
                    content: vec![ContentBlock::text("ok")],
                    api: pie_ai::Api::from("faux"),
                    provider: pie_ai::Provider::from("faux"),
                    model: "faux".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage,
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                };
                sender.push(AssistantMessageEvent::Start {
                    partial: msg.clone(),
                });
                sender.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: msg,
                });
            });
            stream
        })
    };
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(stream);
    opts.budget_cap_usd = Some(0.05);
    let harness = AgentHarness::new(opts);

    // First prompt succeeds; cost crosses the cap in this turn.
    harness.prompt("one").await.unwrap();
    let snap = harness.cost();
    assert!(snap.tokens.cost.total >= 0.05, "cost should be >= cap");

    // Second prompt is rejected at the gate, with a useful message.
    let err = harness.prompt("two").await.unwrap_err().to_string();
    assert!(err.contains("budget cap reached"), "{err}");

    // Resetting the cost tracker unblocks the next prompt.
    harness.reset_cost();
    harness.prompt("three").await.unwrap();
}

/// Regression test for c4pt0r/pie#18. Prior behaviour: `Agent::abort()` cancelled the token
/// but `agent_loop` only re-checked it after `stream.next()` returned, so an LLM stream that
/// stalled mid-flight kept the prompt future blocked. The fix races `stream.next()` against
/// `cancel.cancelled()` with a `biased` select.
///
/// This test uses a "never-emits" stream: the spawned task pushes nothing and parks itself.
/// Before the fix, `harness.abort()` would not unblock the prompt — the test would hang and
/// trigger the tokio test timeout. With the fix, the abort lands in <100ms.
#[tokio::test(flavor = "current_thread")]
async fn abort_promptly_unblocks_a_stalled_stream() {
    let stalled: StreamFn = Arc::new(move |_, _, _| {
        let (stream, sender) = AssistantMessageEventStream::new();
        // Keep the sender alive inside a parked task so `stream.next()` never resolves on its
        // own; only abort can unblock the consumer.
        tokio::spawn(async move {
            let _sender = sender; // hold ownership
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        stream
    });

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(stalled);
    let harness = Arc::new(AgentHarness::new(opts));

    let h2 = harness.clone();
    let prompt_task = tokio::spawn(async move { h2.prompt("hi").await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let abort_at = std::time::Instant::now();
    harness.abort();

    // The prompt future must resolve quickly after the abort signal. Anything beyond a
    // generous bound here means cancellation isn't being honored mid-stream.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), prompt_task)
        .await
        .expect("prompt task must resolve within 2s of abort")
        .expect("prompt task did not panic");
    let elapsed = abort_at.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "abort took {elapsed:?} — should be near-instant"
    );
    let err = outcome.unwrap_err().to_string();
    assert!(
        err.to_lowercase().contains("abort"),
        "expected abort error: {err}"
    );
}

/// The harness's CostTracker accumulates Usage from every assistant turn. Two faux turns
/// with non-zero usage should produce a snapshot whose totals are the sum.
#[tokio::test]
async fn cost_tracker_accumulates_across_turns() {
    use pie_ai::UsageCost;

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    // Custom stream_fn that returns a deterministic Usage on every turn.
    let usage_per_turn = Usage {
        input: 25,
        output: 7,
        cache_read: 3,
        cache_write: 0,
        total_tokens: 35,
        cost: UsageCost {
            input: 0.01,
            output: 0.02,
            cache_read: 0.001,
            cache_write: 0.0,
            total: 0.031,
        },
    };
    let stream: StreamFn = {
        let usage = usage_per_turn.clone();
        Arc::new(move |_, _, _| {
            let usage = usage.clone();
            let (stream, mut sender) = AssistantMessageEventStream::new();
            tokio::spawn(async move {
                let msg = AssistantMessage {
                    role: AssistantRole::Assistant,
                    content: vec![ContentBlock::text("ok")],
                    api: pie_ai::Api::from("faux"),
                    provider: pie_ai::Provider::from("faux"),
                    model: "faux".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage,
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                };
                sender.push(AssistantMessageEvent::Start {
                    partial: msg.clone(),
                });
                sender.push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: msg,
                });
            });
            stream
        })
    };

    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(stream);
    let harness = AgentHarness::new(opts);

    harness.prompt("one").await.unwrap();
    harness.prompt("two").await.unwrap();

    let s = harness.cost();
    assert_eq!(s.turn_count, 2);
    assert_eq!(s.tokens.input, 50);
    assert_eq!(s.tokens.output, 14);
    assert_eq!(s.tokens.cache_read, 6);
    assert_eq!(s.tokens.total_tokens, 70);
    assert!((s.tokens.cost.total - 0.062).abs() < 1e-9);

    harness.reset_cost();
    assert_eq!(harness.cost().turn_count, 0);
    assert_eq!(harness.cost().tokens.input, 0);
}

/// `Agent::abort` cancels the in-flight prompt cleanly: the prompt future resolves with an
/// `Err` and the session jsonl contains a user message (before the abort) but no further
/// assistant content for the cancelled turn.
#[tokio::test]
async fn abort_cancels_in_flight_prompt() {
    // A stream_fn that delays before emitting Done. The cancel token flip during this delay
    // should land us in the agent loop's abort branch.
    let slow_stream: StreamFn = Arc::new(move |_, _, _| {
        let (stream, mut sender) = AssistantMessageEventStream::new();
        tokio::spawn(async move {
            // Long enough that the test has time to call abort() before Done arrives.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let msg = AssistantMessage {
                role: AssistantRole::Assistant,
                content: vec![ContentBlock::text("should-not-arrive")],
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
            };
            sender.push(AssistantMessageEvent::Start {
                partial: msg.clone(),
            });
            sender.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: msg,
            });
        });
        stream
    });

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(slow_stream);
    let harness = Arc::new(AgentHarness::new(opts));

    let h2 = harness.clone();
    let prompt_task = tokio::spawn(async move { h2.prompt("hi").await });

    // Give the agent loop a moment to install the cancel token.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    harness.abort();

    let outcome = prompt_task.await.expect("prompt task did not panic");
    assert!(outcome.is_err(), "aborted prompt should return Err");
    let err = outcome.unwrap_err().to_string();
    assert!(
        err.to_lowercase().contains("abort"),
        "error should mention abort: {err}"
    );

    // Session should contain the user message we sent, but the slow assistant message must
    // NOT have been persisted (Done never reached MessageEnd before abort).
    let entries = session.entries().await.unwrap();
    let user_count = entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::Llm(pie_ai::Message::User(_)),
                    ..
                }
            )
        })
        .count();
    assert_eq!(user_count, 1, "user message should be persisted");
    let assistant_count = entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::Llm(pie_ai::Message::Assistant(_)),
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        assistant_count, 0,
        "no assistant turn should land on the aborted branch"
    );
}

/// A panicking listener does not poison the bus — other listeners still receive events.
#[tokio::test]
async fn harness_event_bus_isolates_panicking_listener() {
    use parking_lot::Mutex;

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(faux_stream_fn("ack"));
    let harness = AgentHarness::new(opts);

    let received: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let r2 = received.clone();
    let good: HarnessListener = Arc::new(move |_ev| {
        *r2.lock() += 1;
    });
    let _unsub_good = harness.subscribe_harness(good);
    let _unsub_bad = harness.subscribe_harness(Arc::new(|_ev| panic!("isolated")));

    harness.prompt("hi").await.unwrap();
    harness.move_to(None, None).await.unwrap();

    assert!(
        *received.lock() >= 2,
        "good listener should still receive events past a panicking sibling"
    );
}

/// `subscribe_harness` returns an unsubscriber; after dropping it, the listener stops receiving.
#[tokio::test]
async fn subscribe_harness_unsub_stops_delivery() {
    use parking_lot::Mutex;

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.stream_fn = Some(faux_stream_fn("ok"));
    let harness = AgentHarness::new(opts);

    let count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let c2 = count.clone();
    let listener: HarnessListener = Arc::new(move |_ev| {
        *c2.lock() += 1;
    });
    let unsub = harness.subscribe_harness(listener);

    harness.prompt("first").await.unwrap();
    let before = *count.lock();
    assert!(before > 0, "listener should have received SessionStart");

    unsub();
    harness.prompt("second").await.unwrap();
    assert_eq!(
        *count.lock(),
        before,
        "no events should reach the listener after unsubscribe"
    );
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Issue #19 regression tests — compaction `first_kept_entry_id` must be reachable in the
// session jsonl so `--resume` reconstructs the kept tail.
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Round-trip: drive the harness through a few turns + force_compact, then drop the harness,
/// reopen the same session jsonl, and verify `build_session_context` reproduces what was in
/// in-memory state after compaction. The pre-fix bug was that `first_kept_entry_id` written to
/// the session jsonl referenced a synthetic id that no real entry carried, so the rebuilt
/// branch dropped the entire pre-compaction tail.
#[tokio::test]
async fn force_compact_writes_reachable_first_kept_entry_id_and_resume_preserves_tail() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());
    let session = repo.create("/tmp/test-cwd").await.unwrap();
    let session_files = repo.list().await.unwrap();
    assert_eq!(session_files.len(), 1);
    let session_path = session_files[0].clone();

    // Build a harness with a low keep_recent_tokens so a small transcript triggers compaction.
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(faux_stream_fn("summary or assistant reply"));
    opts.compaction = CompactionSettings {
        enabled: true,
        reserve_tokens: 0,
        keep_recent_tokens: 4, // forces the cut close to the end
    };
    let harness = AgentHarness::new(opts);

    // Drive three short prompts so we have ≥3 user/assistant pairs in the session.
    harness.prompt("first").await.unwrap();
    harness.prompt("second").await.unwrap();
    harness.prompt("third").await.unwrap();

    let entries_before = session.entries().await.unwrap();
    let pre_compact_msg_count = entries_before
        .iter()
        .filter(|e| matches!(e, SessionTreeEntry::Message { .. }))
        .count();
    assert!(
        pre_compact_msg_count >= 6,
        "expected at least 3 user+assistant pairs in session, got {pre_compact_msg_count}"
    );

    // Force compaction.
    let ran = harness.force_compact(None).await.unwrap();
    assert!(ran, "force_compact should have produced a summary");

    // Verify the persisted Compaction entry's first_kept_entry_id is reachable.
    let entries_after = session.entries().await.unwrap();
    let compaction_entry = entries_after
        .iter()
        .rev()
        .find(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
        .expect("session should have a Compaction entry");
    let SessionTreeEntry::Compaction {
        first_kept_entry_id,
        ..
    } = compaction_entry
    else {
        unreachable!()
    };
    assert!(
        !first_kept_entry_id.is_empty(),
        "first_kept_entry_id must be set when compaction ran"
    );
    let kept = entries_after
        .iter()
        .find(|e| e.id() == first_kept_entry_id.as_str())
        .expect(
            "first_kept_entry_id MUST be reachable in the session entries (issue #19 regression)",
        );
    // The kept entry must be a `Message` and specifically a user-turn boundary.
    let kept_msg = match kept {
        SessionTreeEntry::Message { message, .. } => message,
        other => panic!(
            "first_kept_entry_id should point to a `Message` entry, got {:?}",
            other.type_str()
        ),
    };
    assert!(
        matches!(kept_msg, AgentMessage::Llm(pie_ai::Message::User(_))),
        "first_kept_entry_id should land on a user-turn-boundary Message"
    );

    // Snapshot in-memory state right after compaction.
    let in_memory_after = harness.agent().state().messages.clone();
    drop(harness);

    // Reopen the session from disk and rebuild the context.
    let reopened = repo.open(&session_path).await.unwrap();
    let branch = reopened.branch(None).await.unwrap();
    let rebuilt = build_session_context(&branch);

    // The rebuilt message list must be non-trivial (the bug dropped everything except the
    // summary) and must contain the same tail messages the live agent kept.
    assert!(
        rebuilt.messages.len() >= in_memory_after.len(),
        "rebuilt context lost messages (live={}, rebuilt={}) — pre-fix regression",
        in_memory_after.len(),
        rebuilt.messages.len(),
    );
    // First message in both should be the compaction summary.
    match (&in_memory_after[0], &rebuilt.messages[0]) {
        (AgentMessage::Custom(a), AgentMessage::Custom(b)) => {
            assert_eq!(a.role, "compaction_summary");
            assert_eq!(b.role, "compaction_summary");
        }
        _ => panic!("expected both in-memory and rebuilt to start with compaction_summary"),
    }
}

/// `build_session_context` must never inject `Custom { custom_type: "trigger" }` entries into
/// the LLM message stream — those are audit trail only. Adding this assertion now so the RFC 1
/// trigger work (issue #20) can rely on it as a prerequisite invariant.
#[tokio::test]
async fn build_session_context_skips_trigger_custom_entries() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);

    let id_user = session.append_message(user_message("hello")).await.unwrap();
    let _id_trigger = session
        .append_custom(
            "trigger",
            Some(serde_json::json!({ "trace_id": "trace-1", "source_kind": "Hub" })),
        )
        .await
        .unwrap();
    let id_after = session
        .append_message(user_message("after trigger"))
        .await
        .unwrap();

    // The raw branch must include the trigger Custom entry (audit trail intact).
    let branch = session.branch(None).await.unwrap();
    let trigger_present = branch.iter().any(|e| {
        matches!(
            e,
            SessionTreeEntry::Custom { custom_type, .. } if custom_type == "trigger"
        )
    });
    assert!(
        trigger_present,
        "session.branch must still enumerate trigger Custom entries (audit trail)"
    );
    assert_eq!(branch.len(), 3);

    // build_session_context must NOT translate the trigger Custom into an LLM message.
    let ctx = build_session_context(&branch);
    assert_eq!(
        ctx.messages.len(),
        2,
        "expected only the two user Message entries in the LLM stream"
    );
    let ids: Vec<&str> = branch
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Message { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![id_user.as_str(), id_after.as_str()]);
}

/// `find_cut_point` (and `find_turn_start_index`) must always anchor `first_kept_entry_id` on
/// a user-turn-boundary `Message` even when the cut threshold falls on or next to a trigger
/// `Custom` entry. RFC 1 prerequisite — agent state mapping/rehydrate becomes ambiguous if
/// `first_kept_entry_id` is allowed to reference a non-Message entry.
#[tokio::test]
async fn cut_point_anchors_on_user_message_even_around_trigger_custom() {
    use pie_agent_core::find_cut_point;

    // Build entries: user → assistant → Custom(trigger) → user → assistant.
    // With keep_recent_tokens=1, the algorithm walks backward and hits the last
    // user message; verify it does not land on the trigger Custom.
    let user_a = SessionTreeEntry::Message {
        id: "msg-user-a".into(),
        parent_id: None,
        timestamp: "t".into(),
        message: user_message("user a"),
    };
    let assistant_a = SessionTreeEntry::Message {
        id: "msg-asst-a".into(),
        parent_id: Some("msg-user-a".into()),
        timestamp: "t".into(),
        message: AgentMessage::Llm(pie_ai::Message::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![ContentBlock::text("asst a")],
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
        })),
    };
    let trigger_custom = SessionTreeEntry::Custom {
        id: "custom-trigger-1".into(),
        parent_id: Some("msg-asst-a".into()),
        timestamp: "t".into(),
        custom_type: "trigger".into(),
        data: Some(serde_json::json!({"trace_id": "trace-1"})),
    };
    let user_b = SessionTreeEntry::Message {
        id: "msg-user-b".into(),
        parent_id: Some("custom-trigger-1".into()),
        timestamp: "t".into(),
        message: user_message("user b"),
    };
    let assistant_b = SessionTreeEntry::Message {
        id: "msg-asst-b".into(),
        parent_id: Some("msg-user-b".into()),
        timestamp: "t".into(),
        message: AgentMessage::Llm(pie_ai::Message::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![ContentBlock::text("asst b")],
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
        })),
    };
    let entries = vec![user_a, assistant_a, trigger_custom, user_b, assistant_b];

    let cut = find_cut_point(
        &entries,
        &CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1, // tiny: forces walk-back to nearest user message
        },
    );

    let first_kept_id = cut
        .first_kept_entry_id
        .as_deref()
        .expect("non-empty entries must yield a first_kept_entry_id");
    let kept = entries
        .iter()
        .find(|e| e.id() == first_kept_id)
        .expect("first_kept_entry_id must be reachable in entries");
    // Crucial: must be a Message (not Custom), and the message must be a user turn boundary.
    match kept {
        SessionTreeEntry::Message { message, .. } => {
            assert!(
                matches!(message, AgentMessage::Llm(pie_ai::Message::User(_))),
                "first_kept_entry_id must land on a user-turn boundary Message"
            );
        }
        other => panic!(
            "first_kept_entry_id pointed to {:?}, expected Message",
            other.type_str()
        ),
    }
}

/// `session.branch(None)` failure during compaction must short-circuit cleanly: no
/// `Compaction` entry appended, no agent state mutation, no panic, and the harness emits a
/// diagnostic `HarnessEvent::Compaction` whose summary starts with `compaction skipped:` so
/// observers know why. This is the issue #19 acceptance item for runtime fallback.
#[tokio::test]
async fn force_compact_fallback_when_session_branch_read_fails() {
    use async_trait::async_trait;
    use parking_lot::Mutex as PlMutex;
    use pie_agent_core::SessionError;
    use serde_json::Value;

    /// Wraps `MemorySessionStorage`; lets the test toggle `get_path_to_root` into an error
    /// state to simulate disk read failure mid-compaction.
    struct FailingBranchStorage {
        inner: MemorySessionStorage,
        fail_branch: PlMutex<bool>,
    }

    impl FailingBranchStorage {
        fn new() -> Self {
            Self {
                inner: MemorySessionStorage::new(),
                fail_branch: PlMutex::new(false),
            }
        }
        fn arm(&self) {
            *self.fail_branch.lock() = true;
        }
    }

    #[async_trait]
    impl SessionStorage for FailingBranchStorage {
        async fn get_metadata_json(&self) -> Result<Value, SessionError> {
            self.inner.get_metadata_json().await
        }
        async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
            self.inner.get_leaf_id().await
        }
        async fn set_leaf_id(&self, id: Option<String>) -> Result<(), SessionError> {
            self.inner.set_leaf_id(id).await
        }
        async fn create_entry_id(&self) -> Result<String, SessionError> {
            self.inner.create_entry_id().await
        }
        async fn append_entry(&self, entry: SessionTreeEntry) -> Result<(), SessionError> {
            self.inner.append_entry(entry).await
        }
        async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
            self.inner.get_entry(id).await
        }
        async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
            self.inner.get_entries().await
        }
        async fn get_path_to_root(
            &self,
            leaf_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            if *self.fail_branch.lock() {
                return Err(SessionError {
                    code: SessionErrorCode::StorageFailure,
                    message: "simulated branch read failure".into(),
                });
            }
            self.inner.get_path_to_root(leaf_id).await
        }
        async fn find_entries(
            &self,
            entry_type: &str,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            self.inner.find_entries(entry_type).await
        }
        async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
            self.inner.get_label(id).await
        }
    }

    let storage = Arc::new(FailingBranchStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);

    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(faux_stream_fn("would-be summary"));
    opts.compaction = CompactionSettings {
        enabled: true,
        reserve_tokens: 0,
        keep_recent_tokens: 4,
    };
    let harness = AgentHarness::new(opts);

    // Drive one normal prompt so we have a non-empty session before failure.
    harness.prompt("first").await.unwrap();
    let pre_entries = storage.inner.get_entries().await.unwrap();
    let pre_state_len = harness.agent().state().messages.len();

    // Collect HarnessEvent::Compaction emissions.
    let events: Arc<PlMutex<Vec<HarnessEvent>>> = Arc::new(PlMutex::new(Vec::new()));
    let events_clone = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |ev: HarnessEvent| {
        events_clone.lock().push(ev);
    }) as HarnessListener);

    // Arm the failure and force compaction. Must not panic, must return Ok(false).
    storage.arm();
    let ran = harness.force_compact(None).await.unwrap();
    assert!(
        !ran,
        "force_compact must return Ok(false) when session branch read fails"
    );

    // Session must NOT have a new Compaction entry.
    let post_entries = storage.inner.get_entries().await.unwrap();
    assert_eq!(
        post_entries.len(),
        pre_entries.len(),
        "session must not gain entries when compaction is aborted by branch read failure"
    );
    let added_compaction = post_entries[pre_entries.len()..]
        .iter()
        .any(|e| matches!(e, SessionTreeEntry::Compaction { .. }));
    assert!(
        !added_compaction,
        "no Compaction entry must be appended on branch read failure"
    );

    // Agent state must be unchanged (same message count, same prefix).
    assert_eq!(
        harness.agent().state().messages.len(),
        pre_state_len,
        "agent state.messages must not be mutated when compaction is aborted"
    );

    // A diagnostic Compaction event must have been emitted with the `compaction skipped:`
    // prefix so observers can tell why.
    let events_snapshot = events.lock().clone();
    let saw_diagnostic = events_snapshot.iter().any(|ev| match ev {
        HarnessEvent::Compaction {
            summary,
            tokens_before,
            ..
        } => summary.starts_with("compaction skipped:") && *tokens_before == 0,
        _ => false,
    });
    assert!(
        saw_diagnostic,
        "expected a diagnostic HarnessEvent::Compaction (summary starts with 'compaction skipped:') — events: {:?}",
        events_snapshot
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// handle_trigger — RFC 1 sub-PR 2
// ─────────────────────────────────────────────────────────────────────────────────────────

fn sample_trigger(idempotency_key: &str, trace_id: &str) -> pie_agent_core::Trigger {
    pie_agent_core::Trigger {
        source: pie_agent_core::TriggerSource::Mcp {
            server_name: "github".into(),
            method: "notifications/pr.merged".into(),
        },
        source_kind: pie_agent_core::SourceKind::Mcp,
        source_label: "MCP github".into(),
        event_label: "pr merged".into(),
        payload_visibility: pie_agent_core::PayloadVisibility::Local,
        payload_summary: Some("PR #42 merged".into()),
        payload: None,
        idempotency_key: idempotency_key.into(),
        replacement_policy: pie_agent_core::ReplacementPolicy::Drop,
        trace_id: trace_id.into(),
        authority: pie_agent_core::TriggerAuthority {
            principal_id: "mcp:github".into(),
            principal_label: "github".into(),
            credential_scope: pie_agent_core::CredentialScope::Project,
            allowed_source_actions: vec!["read".into()],
            expires_at: None,
        },
        received_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn handle_trigger_accept_persists_audit_custom_entry_with_accepted_state() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = AgentHarness::new(opts);

    let events = Arc::new(std::sync::Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |ev: HarnessEvent| {
        sink.lock().unwrap().push(ev);
    }));

    let outcome = harness
        .handle_trigger(sample_trigger("k-accept", "trace-accept"))
        .await;
    assert!(matches!(outcome, pie_agent_core::EvaluationOutcome::Accept));

    // One Custom { custom_type: "trigger" } entry in the session.
    let entries = session.entries().await.unwrap();
    let trigger_entries: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        trigger_entries.len(),
        1,
        "must persist exactly one trigger audit entry"
    );
    let data = trigger_entries[0]
        .as_ref()
        .expect("audit entry must carry data payload");
    let record: pie_agent_core::TriggerRecord =
        serde_json::from_value(data.clone()).expect("audit payload must decode as TriggerRecord");
    assert_eq!(record.state, pie_agent_core::TriggerState::Accepted);
    assert_eq!(record.idempotency_key, "k-accept");
    assert_eq!(record.trace_id, "trace-accept");
    assert_eq!(
        record
            .evaluator_decision
            .as_ref()
            .and_then(|v| v.get("outcome"))
            .and_then(|v| v.as_str()),
        Some("accept")
    );

    let evs = events.lock().unwrap().clone();
    let started = evs.iter().any(|e| matches!(e, HarnessEvent::TriggerHandlingStart { idempotency_key, .. } if idempotency_key == "k-accept"));
    assert!(started, "must emit TriggerHandlingStart");
    let handled = evs.iter().find_map(|e| match e {
        HarnessEvent::TriggerHandled {
            idempotency_key,
            state,
            audit_entry_id,
            ..
        } if idempotency_key == "k-accept" => Some((*state, audit_entry_id.clone())),
        _ => None,
    });
    let (state, audit_id) = handled.expect("must emit TriggerHandled for k-accept");
    assert_eq!(state, pie_agent_core::TriggerState::Accepted);
    assert!(
        audit_id.is_some(),
        "audit_entry_id must be Some on successful write"
    );
}

#[tokio::test]
async fn handle_trigger_dedup_emits_deduped_state_and_persists_record() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = AgentHarness::new(opts);

    let _ = harness
        .handle_trigger(sample_trigger("k-dup", "trace-first"))
        .await;
    let second = harness
        .handle_trigger(sample_trigger("k-dup", "trace-second"))
        .await;
    let prev_trace_id = match second {
        pie_agent_core::EvaluationOutcome::Deduped {
            previous_trace_id, ..
        } => previous_trace_id,
        other => panic!("expected Deduped, got {other:?}"),
    };
    assert_eq!(prev_trace_id, "trace-first");

    let entries = session.entries().await.unwrap();
    let states: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => {
                let r: pie_agent_core::TriggerRecord =
                    serde_json::from_value(data.as_ref().unwrap().clone()).unwrap();
                Some(r.state)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            pie_agent_core::TriggerState::Accepted,
            pie_agent_core::TriggerState::Deduped
        ],
        "must persist both audit entries in order"
    );
}

#[tokio::test]
async fn handle_trigger_cycle_suppression_persists_cycle_suppressed_state() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.trigger_runtime = pie_agent_core::TriggerRuntimeConfig {
        dedup_window: std::time::Duration::from_secs(300),
        cycle_hop_limit: 1,
    };
    let harness = AgentHarness::new(opts);

    let _ = harness
        .handle_trigger(sample_trigger("k1", "trace-loop"))
        .await;
    // Same trace at limit → suppressed.
    let suppressed = harness
        .handle_trigger(sample_trigger("k2", "trace-loop"))
        .await;
    assert!(matches!(
        suppressed,
        pie_agent_core::EvaluationOutcome::CycleSuppressed { .. }
    ));

    let entries = session.entries().await.unwrap();
    let last_state = entries
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => {
                let r: pie_agent_core::TriggerRecord =
                    serde_json::from_value(data.as_ref().unwrap().clone()).unwrap();
                Some(r.state)
            }
            _ => None,
        })
        .expect("must have at least one trigger audit entry");
    assert_eq!(last_state, pie_agent_core::TriggerState::CycleSuppressed);
}

#[tokio::test]
async fn notification_status_snapshot_reflects_trigger_runtime_counters() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = AgentHarness::new(opts);

    // Fresh harness: no hooks, zero counters.
    let snap0 = harness.notification_status_snapshot();
    assert!(snap0.hooks.is_empty(), "no hooks registered yet");
    assert_eq!(snap0.runtime.accepted_total, 0);
    assert_eq!(snap0.runtime.deduped_total, 0);
    assert_eq!(snap0.runtime.cycle_suppressed_total, 0);

    let _ = harness
        .handle_trigger(sample_trigger("k1", "trace-1"))
        .await;
    let _ = harness
        .handle_trigger(sample_trigger("k2", "trace-2"))
        .await;
    let _ = harness
        .handle_trigger(sample_trigger("k1", "trace-3"))
        .await;

    let snap1 = harness.notification_status_snapshot();
    assert_eq!(snap1.runtime.accepted_total, 2);
    assert_eq!(snap1.runtime.deduped_total, 1);
    assert_eq!(snap1.runtime.cycle_suppressed_total, 0);
    assert!(snap1.runtime.dedup_entries >= 2);
}

#[tokio::test]
async fn handle_trigger_persistence_failure_still_returns_outcome_and_emits_error() {
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Storage that fails every `append_entry` to verify the audit-failure reflux path.
    struct FailingAppendStorage {
        inner: Arc<MemorySessionStorage>,
    }

    #[async_trait]
    impl SessionStorage for FailingAppendStorage {
        async fn get_metadata_json(&self) -> Result<serde_json::Value, SessionError> {
            self.inner.get_metadata_json().await
        }
        async fn append_entry(&self, _entry: SessionTreeEntry) -> Result<(), SessionError> {
            Err(SessionError {
                code: SessionErrorCode::StorageFailure,
                message: "synthetic write failure".into(),
            })
        }
        async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, SessionError> {
            self.inner.get_entry(id).await
        }
        async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, SessionError> {
            self.inner.get_entries().await
        }
        async fn get_path_to_root(
            &self,
            entry_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            self.inner.get_path_to_root(entry_id).await
        }
        async fn find_entries(
            &self,
            entry_type: &str,
        ) -> Result<Vec<SessionTreeEntry>, SessionError> {
            self.inner.find_entries(entry_type).await
        }
        async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
            self.inner.get_leaf_id().await
        }
        async fn set_leaf_id(&self, id: Option<String>) -> Result<(), SessionError> {
            self.inner.set_leaf_id(id).await
        }
        async fn create_entry_id(&self) -> Result<String, SessionError> {
            self.inner.create_entry_id().await
        }
        async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
            self.inner.get_label(id).await
        }
    }

    let storage = Arc::new(FailingAppendStorage {
        inner: Arc::new(MemorySessionStorage::new()),
    });
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = AgentHarness::new(opts);

    let events = Arc::new(std::sync::Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |ev: HarnessEvent| {
        sink.lock().unwrap().push(ev);
    }));

    let outcome = harness
        .handle_trigger(sample_trigger("k-persist-fail", "trace-x"))
        .await;
    assert!(
        matches!(outcome, pie_agent_core::EvaluationOutcome::Accept),
        "evaluator outcome must be authoritative even when audit persistence fails"
    );

    let evs = events.lock().unwrap().clone();
    let saw_persist_err = evs.iter().any(|e| {
        matches!(
            e,
            HarnessEvent::PersistenceError { context, .. } if context == "trigger_audit"
        )
    });
    assert!(
        saw_persist_err,
        "must emit PersistenceError on audit write failure"
    );
    let handled_audit_id = evs.iter().find_map(|e| match e {
        HarnessEvent::TriggerHandled { audit_entry_id, .. } => Some(audit_entry_id.clone()),
        _ => None,
    });
    assert!(
        handled_audit_id.is_some() && handled_audit_id.as_ref().unwrap().is_none(),
        "TriggerHandled.audit_entry_id must be None when persistence failed"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// register_notification_hook — RFC 1 sub-PR 3 (hook supervisor)
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn register_notification_hook_drives_pump_into_handle_trigger() {
    use pie_agent_core::{
        DynNotificationHook, HookError, HookState, NotificationHook, NotificationHookStatus,
        TriggerSink,
    };

    /// Mock hook: pushes a fixed number of triggers and then closes the sink so the pump
    /// exits cleanly. Verifies that the harness's supervisor actually drives `run(sink)`
    /// and routes everything to `handle_trigger`.
    struct CountedHook {
        label: String,
        triggers: std::sync::Mutex<Vec<pie_agent_core::Trigger>>,
    }
    #[async_trait::async_trait]
    impl NotificationHook for CountedHook {
        fn label(&self) -> &str {
            &self.label
        }
        async fn run(&self, sink: TriggerSink) -> Result<(), HookError> {
            let triggers: Vec<_> = self.triggers.lock().unwrap().drain(..).collect();
            for t in triggers {
                sink.send(t).map_err(|_| HookError::SinkClosed)?;
            }
            Ok(())
        }
        fn status(&self) -> NotificationHookStatus {
            NotificationHookStatus {
                state: HookState::Connected,
                last_event_at: None,
                last_ack_at: None,
                last_error: None,
                queued_count: 0,
                dropped_count: 0,
                deduped_count: 0,
                subscription_labels: vec![self.label.clone()],
                requires_attention: None,
            }
        }
    }

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let triggers = vec![
        sample_trigger("hook-k1", "hook-trace-1"),
        sample_trigger("hook-k2", "hook-trace-2"),
        sample_trigger("hook-k1", "hook-trace-3"), // duplicate of k1 → dedup path
    ];

    let hook: DynNotificationHook = Arc::new(CountedHook {
        label: "mock".into(),
        triggers: std::sync::Mutex::new(triggers),
    });

    harness.register_notification_hook(hook);

    // Wait for the pump to drain. The hook produces three triggers synchronously then
    // closes the sink; the pump exits when rx.recv() returns None. We poll the snapshot
    // counters as the completion signal; with a wide timeout to handle CI load.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let snap = loop {
        let s = harness.notification_status_snapshot();
        if s.runtime.accepted_total + s.runtime.deduped_total + s.runtime.cycle_suppressed_total
            >= 3
        {
            break s;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "pump did not process 3 triggers within 5s — snapshot: {:?}",
                s
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };

    assert_eq!(snap.runtime.accepted_total, 2);
    assert_eq!(snap.runtime.deduped_total, 1);
    assert_eq!(snap.runtime.cycle_suppressed_total, 0);
    assert_eq!(snap.hooks.len(), 1, "hook must be tracked in snapshot");
    assert_eq!(
        snap.hooks[0].subscription_labels,
        vec!["mock".to_string()],
        "snapshot hook label must round-trip from hook.status()"
    );

    // Both accepted triggers must have produced audit Custom entries.
    let entries = session.entries().await.unwrap();
    let trigger_audit_count = entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                SessionTreeEntry::Custom { custom_type, .. }
                    if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE
            )
        })
        .count();
    // 3 audit entries: Accepted (k1), Accepted (k2), Deduped (k1 again).
    assert_eq!(trigger_audit_count, 3);
}

#[tokio::test]
async fn register_notification_hook_snapshot_reflects_hook_status_state() {
    use pie_agent_core::{
        DynNotificationHook, HookError, HookState, NotificationHook, NotificationHookStatus,
        TriggerSink,
    };

    /// Hook that immediately reports `Disconnected` and never sends anything. The supervisor
    /// pump exits as soon as the hook's `run` future resolves and the sink is dropped.
    struct DegradedHook;
    #[async_trait::async_trait]
    impl NotificationHook for DegradedHook {
        fn label(&self) -> &str {
            "degraded"
        }
        async fn run(&self, _sink: TriggerSink) -> Result<(), HookError> {
            Ok(())
        }
        fn status(&self) -> NotificationHookStatus {
            NotificationHookStatus {
                state: HookState::Disconnected {
                    reason: "transport closed at startup".into(),
                },
                last_event_at: None,
                last_ack_at: None,
                last_error: Some("transport closed at startup".into()),
                queued_count: 0,
                dropped_count: 0,
                deduped_count: 0,
                subscription_labels: vec!["degraded".into()],
                requires_attention: Some("degraded: transport closed at startup".into()),
            }
        }
    }

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let hook: DynNotificationHook = Arc::new(DegradedHook);
    harness.register_notification_hook(hook);

    // Give the driver/pump tasks a moment to schedule. The hook's run returns immediately
    // so we mostly need to give the snapshot a chance to see the registered hook.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let snap = harness.notification_status_snapshot();
    assert_eq!(snap.hooks.len(), 1);
    assert!(
        matches!(snap.hooks[0].state, HookState::Disconnected { .. }),
        "snapshot must reflect the hook's reported state"
    );
    assert_eq!(
        snap.hooks[0].requires_attention.as_deref(),
        Some("degraded: transport closed at startup")
    );
    // Hook produced nothing so runtime counters stay at zero.
    assert_eq!(snap.runtime.accepted_total, 0);
}

// ─────────────────────────────────────────────────────────────────────────────────────────
// before_trigger hook — RFC 1 sub-PR 4 (permission evaluator extension)
// ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn before_trigger_default_allow_keeps_state_accepted() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = AgentHarness::new(opts);

    let _ = harness
        .handle_trigger(sample_trigger("perm-default", "trace-default"))
        .await;

    let entries = session.entries().await.unwrap();
    let state = entries
        .iter()
        .find_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => {
                let r: pie_agent_core::TriggerRecord =
                    serde_json::from_value(data.as_ref().unwrap().clone()).unwrap();
                Some(r.state)
            }
            _ => None,
        })
        .expect("audit entry");
    assert_eq!(
        state,
        pie_agent_core::TriggerState::Accepted,
        "no hook → default Allow → Accepted"
    );
}

#[tokio::test]
async fn before_trigger_deny_records_permission_denied_state_and_reason() {
    use pie_agent_core::{BeforeTriggerContext, BeforeTriggerDecision, BeforeTriggerHook};

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());

    let deny_hook: BeforeTriggerHook = Arc::new(|_ctx: BeforeTriggerContext, _cancel| {
        Box::pin(async move {
            BeforeTriggerDecision::Deny {
                reason: "principal not on allow-list".into(),
            }
        })
    });
    opts.before_trigger = Some(deny_hook);
    let harness = AgentHarness::new(opts);

    let events = Arc::new(std::sync::Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |ev: HarnessEvent| {
        sink.lock().unwrap().push(ev);
    }));

    let outcome = harness
        .handle_trigger(sample_trigger("perm-deny", "trace-deny"))
        .await;
    assert!(
        matches!(outcome, pie_agent_core::EvaluationOutcome::Accept),
        "EvaluationOutcome is still Accept (evaluator decided to admit); the harness state is what reflects the deny"
    );

    let entries = session.entries().await.unwrap();
    let record = entries
        .iter()
        .find_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => {
                let r: pie_agent_core::TriggerRecord =
                    serde_json::from_value(data.as_ref().unwrap().clone()).unwrap();
                Some(r)
            }
            _ => None,
        })
        .expect("audit entry");

    assert_eq!(record.state, pie_agent_core::TriggerState::PermissionDenied);
    let decision = record
        .evaluator_decision
        .as_ref()
        .expect("evaluator_decision must capture deny reason");
    assert_eq!(decision["permission"].as_str(), Some("deny"));
    assert_eq!(
        decision["reason"].as_str(),
        Some("principal not on allow-list")
    );

    // The live event must carry the same evaluator_decision the audit got, so TUI / JSONL
    // subscribers can render the deny reason without re-reading the session.
    let evs = events.lock().unwrap().clone();
    let event_decision = evs
        .iter()
        .find_map(|e| match e {
            HarnessEvent::TriggerHandled {
                state,
                evaluator_decision,
                ..
            } if *state == pie_agent_core::TriggerState::PermissionDenied => {
                Some(evaluator_decision.clone())
            }
            _ => None,
        })
        .expect("TriggerHandled event with PermissionDenied state must exist");
    let event_decision = event_decision.expect("event must carry evaluator_decision");
    assert_eq!(event_decision["permission"].as_str(), Some("deny"));
    assert_eq!(
        event_decision["reason"].as_str(),
        Some("principal not on allow-list")
    );
}

#[tokio::test]
async fn before_trigger_prompt_records_needs_approval_state_and_reason() {
    use pie_agent_core::{BeforeTriggerContext, BeforeTriggerDecision, BeforeTriggerHook};

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());

    let prompt_hook: BeforeTriggerHook = Arc::new(|_ctx: BeforeTriggerContext, _cancel| {
        Box::pin(async move {
            BeforeTriggerDecision::Prompt {
                reason: "Cloudflare hub trigger from new principal".into(),
            }
        })
    });
    opts.before_trigger = Some(prompt_hook);
    let harness = AgentHarness::new(opts);

    let events = Arc::new(std::sync::Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |ev: HarnessEvent| {
        sink.lock().unwrap().push(ev);
    }));

    let _ = harness
        .handle_trigger(sample_trigger("perm-prompt", "trace-prompt"))
        .await;

    let entries = session.entries().await.unwrap();
    let record = entries
        .iter()
        .find_map(|e| match e {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == pie_agent_core::TriggerRecord::CUSTOM_TYPE => {
                let r: pie_agent_core::TriggerRecord =
                    serde_json::from_value(data.as_ref().unwrap().clone()).unwrap();
                Some(r)
            }
            _ => None,
        })
        .expect("audit entry");

    assert_eq!(record.state, pie_agent_core::TriggerState::NeedsApproval);
    assert_eq!(
        record.evaluator_decision.as_ref().unwrap()["permission"].as_str(),
        Some("prompt")
    );

    let evs = events.lock().unwrap().clone();
    let (handled_state, handled_decision) = evs
        .iter()
        .find_map(|e| match e {
            HarnessEvent::TriggerHandled {
                state,
                evaluator_decision,
                ..
            } => Some((*state, evaluator_decision.clone())),
            _ => None,
        })
        .expect("must emit TriggerHandled");
    assert_eq!(
        handled_state,
        pie_agent_core::TriggerState::NeedsApproval,
        "TriggerHandled event must carry the policy-terminal state"
    );
    // Live subscribers (TUI banner, JSONL logs) must be able to render the prompt reason
    // straight from the event without a secondary session lookup.
    let decision = handled_decision.expect("TriggerHandled must carry evaluator_decision");
    assert_eq!(decision["permission"].as_str(), Some("prompt"));
    assert_eq!(
        decision["reason"].as_str(),
        Some("Cloudflare hub trigger from new principal")
    );
}

#[tokio::test]
async fn before_trigger_hook_does_not_run_on_deduped_path() {
    use pie_agent_core::{BeforeTriggerContext, BeforeTriggerDecision, BeforeTriggerHook};

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());

    let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = call_count.clone();
    let hook: BeforeTriggerHook = Arc::new(move |_ctx: BeforeTriggerContext, _cancel| {
        let counter = counter.clone();
        Box::pin(async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            BeforeTriggerDecision::Allow
        })
    });
    opts.before_trigger = Some(hook);
    let harness = AgentHarness::new(opts);

    // First call: Accept → hook runs once.
    let _ = harness
        .handle_trigger(sample_trigger("dup-key", "trace-1"))
        .await;
    // Second call (duplicate idempotency key): Deduped → hook MUST NOT run.
    let _ = harness
        .handle_trigger(sample_trigger("dup-key", "trace-2"))
        .await;

    assert_eq!(
        call_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "hook must only run after evaluator Accept, never on Deduped/CycleSuppressed paths"
    );
}
