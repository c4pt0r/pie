//! Integration test for the slash-command registry. Drives `dispatch` against a real
//! `AgentHarness` (faux stream) and verifies user-visible effects: `/thinking high` flips the
//! harness's thinking level *and* writes a thinking_level_change row to the session, so
//! `--resume` later restores it.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentTool, MemorySessionStorage, Session, SessionStorage,
    SessionTreeEntry, Skill, ThinkingLevel,
};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, Context, DoneReason, Message, StopReason, ToolCall, Usage,
};

static PATH_ENV_LOCK: Mutex<()> = Mutex::new(());
static PIE_DIR_ENV_LOCK: Mutex<()> = Mutex::new(());
static DYNAMIC_TRIGGER_LOCK: Mutex<()> = Mutex::new(());

// The binary crate doesn't expose `commands` — pull it in via path-include so this test
// exercises the actual code path without restructuring the crate as a [lib]. `commands.rs`
// references sibling modules through `crate::...`, so we include those siblings too. They appear unused-from-tests
// (no items are called directly here) — that's fine; the commands module reaches into them.
#[allow(dead_code)]
#[path = "../src/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/bug_report.rs"]
mod bug_report;
#[path = "../src/commands.rs"]
mod commands;
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/export.rs"]
mod export;
#[allow(dead_code)]
#[path = "../src/history.rs"]
mod history;
#[allow(dead_code)]
#[path = "../src/session/mod.rs"]
mod session;
#[allow(dead_code)]
#[path = "../src/triggers/mod.rs"]
mod triggers;

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
        cost: pie_ai::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

fn new_trigger_extraction_stream() -> pie_agent_core::StreamFn {
    Arc::new(|_, context: &Context, _| {
        let has_tool_result = context
            .messages
            .iter()
            .any(|m| matches!(m, Message::ToolResult(_)));
        let message = if has_tool_result {
            assistant_text("created")
        } else {
            assistant_tool_call(
                "call-new-trigger",
                "NewTrigger",
                serde_json::json!({
                    "condition": "\u{73b0}\u{5728}\u{662f} 11pm",
                    "action": "\u{5199}\u{4e00}\u{4e2a} tmp \u{6587}\u{4ef6}",
                }),
            )
        };
        stream_one(message)
    })
}

fn stream_one(message: AssistantMessage) -> AssistantMessageEventStream {
    let (stream, mut sender) = AssistantMessageEventStream::new();
    tokio::spawn(async move {
        sender.push(AssistantMessageEvent::Start {
            partial: message.clone(),
        });
        sender.push(AssistantMessageEvent::Done {
            reason: match message.stop_reason {
                StopReason::ToolUse => DoneReason::ToolUse,
                _ => DoneReason::Stop,
            },
            message,
        });
    });
    stream
}

fn assistant_tool_call(id: &str, name: &str, args: serde_json::Value) -> AssistantMessage {
    let arguments = args.as_object().cloned().unwrap_or_default();
    assistant(vec![ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
    })])
}

fn assistant_text(text: &str) -> AssistantMessage {
    assistant(vec![ContentBlock::text(text)])
}

fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    let stop_reason = if content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    };
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

fn skill(name: &str, content: &str, disabled: bool) -> Skill {
    Skill {
        name: name.into(),
        description: format!("description for {name}"),
        file_path: format!("/tmp/project/.pie/skills/{name}/SKILL.md"),
        content: content.into(),
        disable_model_invocation: disabled,
    }
}

#[tokio::test]
async fn dispatch_thinking_command_updates_state_and_session() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.thinking_level = ThinkingLevel::Off;
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/thinking high", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    assert_eq!(
        harness.agent().state().thinking_level,
        Some(ThinkingLevel::High)
    );
    let entries = session.entries().await.unwrap();
    let saw_change = entries.iter().any(|e| {
        matches!(
            e,
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } if thinking_level == "high"
        )
    });
    assert!(
        saw_change,
        "thinking_level_change entry must be persisted: {entries:#?}"
    );
}

#[tokio::test]
async fn dispatch_unknown_command_returns_error_outcome() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/notarealcommand", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(msg) => assert!(msg.contains("unknown command")),
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_triggers_status_is_read_only_and_available() {
    // Serialize with the other trigger tests: they share the process-global rule registry, so
    // an unlocked `clear_for_tests()` here can wipe another test's rule mid-run.
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![Arc::new(triggers::NewTriggerTool) as Arc<dyn AgentTool>];
    opts.stream_fn = Some(new_trigger_extraction_stream());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/triggers", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(
        session.entries().await.unwrap().is_empty(),
        "/triggers status must not mutate the session"
    );
}

#[tokio::test]
async fn dispatch_template_returns_repl_owned_agent_work() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/template release version=1.2.3", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::RunPromptTemplate { name, vars } => {
            assert_eq!(name, "release");
            assert_eq!(vars.get("version").and_then(|v| v.as_str()), Some("1.2.3"));
        }
        other => panic!("expected RunPromptTemplate outcome, got {other:?}"),
    }
    assert!(
        session.entries().await.unwrap().is_empty(),
        "/template dispatch should not run the agent directly; the TUI owns Ctrl-C abort handling"
    );
}

#[tokio::test]
async fn dispatch_compact_returns_repl_owned_agent_work() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/compact keep decisions", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::RunCompaction { custom } => {
            assert_eq!(custom.as_deref(), Some("keep decisions"));
        }
        other => panic!("expected RunCompaction outcome, got {other:?}"),
    }
    assert!(
        session.entries().await.unwrap().is_empty(),
        "/compact dispatch should not run compaction directly; the TUI owns Ctrl-C abort handling"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_new_trigger_registers_dynamic_rule() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![Arc::new(triggers::NewTriggerTool) as Arc<dyn AgentTool>];
    opts.stream_fn = Some(new_trigger_extraction_stream());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let condition = "\u{73b0}\u{5728}\u{662f} 11pm";
    let action = "\u{5199}\u{4e00}\u{4e2a} tmp \u{6587}\u{4ef6}";
    let prompt =
        format!("/new-trigger \u{968f}\u{4fbf}\u{8bf4}\u{4e00}\u{53e5}: {condition}; {action}");

    let outcome = commands::dispatch(&prompt, &registry, &ctx).await;
    let agent_prompt = match outcome {
        commands::CommandOutcome::RunAgentPrompt {
            prompt,
            error_context,
        } => {
            assert_eq!(error_context, "create trigger: ");
            assert!(prompt.contains(condition));
            assert!(prompt.contains(action));
            prompt
        }
        other => panic!("expected RunAgentPrompt outcome, got {other:?}"),
    };
    assert!(
        triggers::global_registry().list().is_empty(),
        "/new-trigger dispatch should not run the agent directly; the TUI owns Ctrl-C abort handling"
    );

    harness.prompt(agent_prompt).await.unwrap();

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].condition, condition);
    assert_eq!(rules[0].action, action);
    let status_lines = commands::render_triggers_status(&harness.notification_status_snapshot());
    assert!(
        status_lines
            .iter()
            .any(|line| line.contains("dynamic rules: 1"))
    );
    assert!(status_lines.iter().any(|line| line.contains(&rules[0].id)));
    assert!(status_lines.iter().any(|line| line.contains("tmp")));
    assert!(
        !session.entries().await.unwrap().is_empty(),
        "/new-trigger routes through the agent so the model can extract condition/action"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_triggers_remove_deletes_dynamic_rule() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    let rule = triggers::global_registry()
        .add_rule("event says delete this", "echo deleted")
        .expect("rule");

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome =
        commands::dispatch(&format!("/triggers remove {}", rule.id), &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(triggers::global_registry().list().is_empty());
    assert!(
        session.entries().await.unwrap().is_empty(),
        "/triggers remove only mutates the dynamic rule registry"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_triggers_disable_and_enable_updates_rule_state() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    let rule = triggers::global_registry()
        .add_rule("event says toggle this", "echo toggled")
        .expect("rule");

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome =
        commands::dispatch(&format!("/triggers disable {}", rule.id), &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(!triggers::global_registry().list()[0].enabled);

    let outcome =
        commands::dispatch(&format!("/triggers enable {}", rule.id), &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(triggers::global_registry().list()[0].enabled);
    assert!(
        session.entries().await.unwrap().is_empty(),
        "/triggers enable/disable only mutates the dynamic rule registry"
    );
}

#[tokio::test]
async fn dispatch_triggers_abort_missing_trace_returns_error() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/triggers abort missing-trace", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("no running trigger"));
            assert!(message.contains("missing-trace"));
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
    assert!(
        session.entries().await.unwrap().is_empty(),
        "failed abort lookup must not mutate the session"
    );
}

#[tokio::test]
async fn dispatch_triggers_abort_all_empty_harness_is_handled_and_read_only() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/triggers abort --all", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(
        session.entries().await.unwrap().is_empty(),
        "abort --all on an empty harness must not mutate the session"
    );
}

#[tokio::test]
async fn dispatch_undo_removes_last_turn_from_active_branch() {
    use pie_agent_core::StreamFn;
    use pie_ai::{
        AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
        ContentBlock, DoneReason, StopReason, Usage,
    };

    fn faux_stream(text: &'static str) -> StreamFn {
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

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(faux_stream("ack-1"));
    let harness = Arc::new(AgentHarness::new(opts));
    harness.prompt("hi").await.unwrap();

    // Sanity: there are now 2 messages on the active branch (1 user, 1 assistant).
    let before = session.build_context().await.unwrap().messages.len();
    assert_eq!(before, 2);

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/undo", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let after = session.build_context().await.unwrap().messages.len();
    assert_eq!(
        after, 0,
        "after /undo, both user + assistant should be off the active branch"
    );
}

#[tokio::test]
async fn dispatch_name_sets_session_name() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/name my-thing", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert_eq!(
        session.session_name().await.unwrap().as_deref(),
        Some("my-thing")
    );
}

#[tokio::test]
async fn dispatch_quit_returns_quit_outcome() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    for input in ["/quit", "/exit", "/q"] {
        let outcome = commands::dispatch(input, &registry, &ctx).await;
        assert!(
            matches!(outcome, commands::CommandOutcome::Quit),
            "{input} should map to Quit"
        );
    }
}

#[tokio::test]
async fn dispatch_login_prompts_for_secret_instead_of_accepting_inline_key() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/login ds4", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::LoginSecret { provider } => assert_eq!(provider, "ds4"),
        other => panic!("expected LoginSecret outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_login_rejects_inline_secret_material() {
    let secret = "sk-inline-secret-should-not-be-accepted";
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch(&format!("/login ds4 {secret}"), &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("usage: /login <provider>"), "{message}");
            assert!(
                !message.contains(secret),
                "error must not repeat inline secret: {message}"
            );
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn save_api_key_persists_without_printing_secret_material() {
    let _guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let secret = "sk-sentinel-login-secret-should-not-leak";

    let path = commands::save_api_key("ds4", secret).expect("save api key");
    assert_eq!(path, temp.path().join("auth.json"));

    let stored = auth::AuthStore::load_from(&path).expect("load auth store");
    match stored.get("ds4").expect("stored ds4 credential") {
        auth::ProviderCredential::ApiKey { value } => assert_eq!(value, secret),
        other => panic!("unexpected credential kind: {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_share_default_uses_gh_private_default_without_secret_flag() {
    let _guard = PATH_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv.txt");
    write_fake_gh(
        temp.path(),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" > '{}'
printf '%s\n' 'https://gist.github.com/example/private'
"#,
            argv_log.display()
        ),
    );
    let _path_guard = prepend_path(temp.path());

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-share-default",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/share", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert!(argv.contains("gist create"), "argv: {argv}");
    assert!(
        !argv.contains("--secret"),
        "argv must not include removed gh flag: {argv}"
    );
    assert!(
        !argv.contains("--public"),
        "default share should remain private: {argv}"
    );
}

#[tokio::test]
async fn dispatch_share_public_passes_public_flag() {
    let _guard = PATH_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let argv_log = temp.path().join("argv.txt");
    write_fake_gh(
        temp.path(),
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$*" > '{}'
printf '%s\n' 'https://gist.github.com/example/public'
"#,
            argv_log.display()
        ),
    );
    let _path_guard = prepend_path(temp.path());

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-share-public",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/share --public", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let argv = std::fs::read_to_string(argv_log).unwrap();
    assert!(argv.contains("--public"), "argv: {argv}");
    assert!(
        !argv.contains("--secret"),
        "argv must not include removed gh flag: {argv}"
    );
}

#[tokio::test]
async fn dispatch_share_preserves_gh_stderr_on_failure() {
    let _guard = PATH_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    write_fake_gh(
        temp.path(),
        r#"#!/bin/sh
printf '%s\n' 'unknown flag: --secret' >&2
exit 1
"#,
    );
    let _path_guard = prepend_path(temp.path());

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-share-failure",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/share", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("gh gist create exited 1"), "{message}");
            assert!(message.contains("unknown flag: --secret"), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_skill_attaches_loaded_skill_without_exposing_body() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![skill("review-pr", "SECRET SKILL BODY", false)];
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skill review-pr", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::AttachSkill { name } => assert_eq!(name, "review-pr"),
        other => panic!("expected AttachSkill outcome, got {other:?}"),
    }

    let prompt = commands::attach_skill_prompt("summarize the diff", Some("review-pr"));
    assert!(prompt.contains("Skill tool"));
    assert!(prompt.contains("review-pr"));
    assert!(prompt.contains("summarize the diff"));
    assert!(
        !prompt.contains("SECRET SKILL BODY"),
        "slash command must not inline skill body into the user-visible prompt"
    );
}

#[tokio::test]
async fn dispatch_skill_refuses_disabled_skill() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![skill("disabled-skill", "SECRET SKILL BODY", true)];
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skill disabled-skill", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(msg) => {
            assert!(msg.contains("disabled-skill"));
            assert!(msg.contains("disable_model_invocation=true"));
            assert!(!msg.contains("SECRET SKILL BODY"));
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_skill_unknown_name_suggests_prefix_matches() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![skill("review-pr", "SECRET SKILL BODY", false)];
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skill rev", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(msg) => {
            assert!(msg.contains("no skill named 'rev'"));
            assert!(msg.contains("Did you mean: review-pr"));
            assert!(!msg.contains("SECRET SKILL BODY"));
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

// The path-include duplicates the module, so we silence the dead-code warning about helpers
// that only the binary calls.
#[allow(dead_code)]
fn _path_check(_p: &Path) {}

fn write_fake_gh(dir: &Path, body: &str) {
    let path = dir.join("gh");
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
}

struct PathGuard {
    original: Option<std::ffi::OsString>,
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

fn prepend_path(dir: &Path) -> PathGuard {
    let original = std::env::var_os("PATH");
    let mut paths = vec![dir.to_path_buf()];
    if let Some(value) = original.as_ref() {
        paths.extend(std::env::split_paths(value));
    }
    let joined = std::env::join_paths(paths).unwrap();
    unsafe { std::env::set_var("PATH", joined) };
    PathGuard { original }
}

struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}
