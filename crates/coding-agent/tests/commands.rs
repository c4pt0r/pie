//! Integration test for the slash-command registry. Drives `dispatch` against a real
//! `AgentHarness` (faux stream) and verifies user-visible effects: `/thinking high` flips the
//! harness's thinking level *and* writes a thinking_level_change row to the session, so
//! `--resume` later restores it.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentMessage, AgentTool, LoadSkillsOutput,
    MemorySessionStorage, OnTurnEndContext, ReloadSkillsFn, Session, SessionStorage,
    SessionTreeEntry, Skill, SkillSource, ThinkingLevel, TurnEndAction,
};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, Context, DoneReason, Message, StopReason, ToolCall, Usage,
};

static PATH_ENV_LOCK: Mutex<()> = Mutex::new(());
static PIE_DIR_ENV_LOCK: Mutex<()> = Mutex::new(());
static DYNAMIC_TRIGGER_LOCK: Mutex<()> = Mutex::new(());
static CRON_LOCK: Mutex<()> = Mutex::new(());

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
#[path = "../src/goal.rs"]
mod goal;
#[allow(dead_code)]
#[path = "../src/history.rs"]
mod history;
#[allow(dead_code)]
#[path = "../src/hub_auth.rs"]
mod hub_auth;
#[allow(dead_code)]
#[path = "../src/hub_client.rs"]
mod hub_client;
#[allow(dead_code)]
#[path = "../src/hub_join.rs"]
mod hub_join;
#[allow(dead_code)]
#[path = "../src/mcp_loader.rs"]
mod mcp_loader;
#[allow(dead_code)]
#[path = "../src/session/mod.rs"]
mod session;
#[allow(dead_code)]
#[path = "../src/skills_state.rs"]
mod skills_state;
#[allow(dead_code)]
#[path = "../src/tools/mod.rs"]
mod tools;
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
        source: SkillSource::User,
    }
}

fn user_skill_at(base_dir: &Path, name: &str, disabled: bool) -> Skill {
    Skill {
        name: name.into(),
        description: format!("description for {name}"),
        file_path: base_dir
            .join("skills")
            .join(name)
            .join("SKILL.md")
            .to_string_lossy()
            .to_string(),
        content: format!("SECRET SKILL BODY for {name}"),
        disable_model_invocation: disabled,
        source: SkillSource::User,
    }
}

fn harness_with_reloadable_skills(base_dir: &Path, seed: Vec<Skill>) -> Arc<AgentHarness> {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let source = Arc::new(Mutex::new(seed.clone()));
    let base = base_dir.to_path_buf();
    let loader_source = source.clone();
    let loader: ReloadSkillsFn = Arc::new(move || {
        let source = loader_source.clone();
        let base = base.clone();
        Box::pin(async move {
            let mut skills = source.lock().unwrap().clone();
            let state = skills_state::load(&base).await;
            skills_state::apply(&state, &mut skills);
            LoadSkillsOutput {
                skills,
                diagnostics: Vec::new(),
            }
        })
    });
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = seed;
    opts.reload_skills_fn = Some(loader);
    Arc::new(AgentHarness::new(opts))
}

fn harness_with_disk_skill_reload(base_dir: &Path, seed: Vec<Skill>) -> Arc<AgentHarness> {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let base = base_dir.to_path_buf();
    let loader: ReloadSkillsFn = Arc::new(move || {
        let base = base.clone();
        Box::pin(async move {
            let env = pie_agent_core::NativeEnv::new(
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            );
            let skills_dir = base.join("skills");
            let mut out = pie_agent_core::load_skills(
                &env,
                &[skills_dir.to_string_lossy().as_ref()],
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
            for skill in out.skills.iter_mut() {
                skill.source = SkillSource::User;
            }
            let state = skills_state::load(&base).await;
            skills_state::apply(&state, &mut out.skills);
            out
        })
    });
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = seed;
    opts.reload_skills_fn = Some(loader);
    Arc::new(AgentHarness::new(opts))
}

static COMMAND_OUTPUT_LOCK: Mutex<()> = Mutex::new(());

struct OutputCapture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl OutputCapture {
    fn install() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = lines.clone();
        commands::console::set_sink(Box::new(move |line| {
            sink_lines.lock().unwrap().push(line);
        }));
        Self { lines }
    }

    fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        commands::console::clear_sink();
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
async fn dispatch_goal_sets_and_reports_session_goal() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let capture = OutputCapture::install();
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
        commands::dispatch("/goal finish only after cargo test passes", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let state = goal::current(&harness).await.expect("goal state");
    assert_eq!(state.status, goal::GoalStatus::Pursuing);
    assert_eq!(state.condition, "finish only after cargo test passes");

    let outcome = commands::dispatch("/goal", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let output = capture.text();
    assert!(output.contains("goal set: finish only after cargo test passes"));
    assert!(
        output.contains("start by sending a normal prompt, or run /goal start <prompt>"),
        "{output}"
    );
    assert!(output.contains("status: pursuing"), "{output}");
    assert!(output.contains("iterations: 0"), "{output}");

    let entries = session.entries().await.unwrap();
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            SessionTreeEntry::Custom { custom_type, data, .. }
                if custom_type == goal::CUSTOM_TYPE
                    && data.as_ref().is_some_and(|d| d["condition"] == "finish only after cargo test passes")
        )),
        "goal command must persist session metadata: {entries:#?}"
    );
}

#[tokio::test]
async fn dispatch_goal_start_runs_prompt_when_goal_active() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let _capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session.clone());
    let harness = Arc::new(AgentHarness::new(opts));

    goal::set(&harness, "finish only after cargo test passes".into())
        .await
        .unwrap();

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/goal start run cargo test", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::RunAgentPrompt {
            prompt,
            error_context,
        } => {
            assert_eq!(prompt, "run cargo test");
            assert_eq!(error_context, "goal start: ");
        }
        other => panic!("expected RunAgentPrompt, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_goal_start_requires_active_goal() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let _capture = OutputCapture::install();
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

    let outcome = commands::dispatch("/goal start run cargo test", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("no active goal"), "{message}");
            assert!(message.contains("/goal <condition>"), "{message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_goal_clear_hides_current_goal() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));
    goal::set(&harness, "ship a release".into()).await.unwrap();

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/goal clear", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    assert!(goal::current(&harness).await.is_none());
    let output = capture.text();
    assert!(output.contains("goal cleared"), "{output}");
}

#[tokio::test]
async fn goal_evaluator_false_returns_continuation_and_audits_reason() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.stream_fn = Some(Arc::new(|_, _, _| {
        stream_one(assistant_text(
            "{\"ok\":false,\"reason\":\"missing cargo test output\"}",
        ))
    }));
    let harness = Arc::new(AgentHarness::new(opts));
    let harness_cell = Arc::new(OnceLock::new());
    assert!(harness_cell.set(harness.clone()).is_ok());
    let hook = goal::stop_hook(harness_cell);
    goal::set(&harness, "finish only after cargo test passes".into())
        .await
        .unwrap();

    let decision = hook(
        OnTurnEndContext {
            transcript: vec![AgentMessage::Llm(Message::User(pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text("ran cargo build only".into()),
                timestamp: 0,
            }))],
            continuation_count: 0,
            last_user_prompt: Some("ran cargo build only".into()),
        },
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    let TurnEndAction::Continue { prompt } = decision.action else {
        panic!("expected continuation, got {:?}", decision.action);
    };
    assert!(prompt.contains("finish only after cargo test passes"));
    assert!(prompt.contains("missing cargo test output"));
    assert_eq!(decision.payload.as_ref().unwrap()["ok"], false);
    assert_eq!(
        decision.payload.as_ref().unwrap()["reason"],
        "missing cargo test output"
    );

    let state = goal::current(&harness).await.expect("goal state");
    assert_eq!(state.iterations, 1);
    assert_eq!(
        state.last_reason.as_deref(),
        Some("missing cargo test output")
    );

    let entries = session.entries().await.unwrap();
    assert!(
        entries.iter().any(|entry| matches!(
            entry,
            SessionTreeEntry::Custom { custom_type, data, .. }
                if custom_type == goal::CUSTOM_TYPE
                    && data.as_ref().is_some_and(|d| d["status"] == "pursuing"
                        && d["last_reason"] == "missing cargo test output")
        )),
        "goal hook must persist updated goal state: {entries:#?}"
    );
}

#[tokio::test]
async fn dynamic_skill_slash_command_attaches_skill_without_body_echo() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let _capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![skill("db9", "SECRET SKILL BODY", false)];
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
    let outcome = commands::dispatch("/db9", &registry, &ctx).await;

    match outcome {
        commands::CommandOutcome::AttachSkill { name } => assert_eq!(name, "db9"),
        other => panic!("expected AttachSkill outcome, got {other:?}"),
    }
    let output = _capture.text();
    assert!(output.contains("using skill: db9 (user)"), "{output}");
    assert!(!output.contains("SECRET SKILL BODY"), "{output}");
}

#[tokio::test]
async fn dynamic_skill_slash_command_with_prompt_runs_skill_wrapped_turn() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let _capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![skill("db9", "SECRET SKILL BODY", false)];
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
    let outcome = commands::dispatch("/db9 create a table", &registry, &ctx).await;

    match outcome {
        commands::CommandOutcome::RunAgentPrompt { prompt, .. } => {
            assert!(prompt.contains("Skill tool"));
            assert!(prompt.contains("db9"));
            assert!(prompt.contains("create a table"));
            assert!(!prompt.contains("SECRET SKILL BODY"));
        }
        other => panic!("expected RunAgentPrompt outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dynamic_skill_slash_command_hides_disabled_and_builtin_conflicts() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![
        skill("disabled-skill", "body", true),
        skill("help", "conflicting body", false),
    ];
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let shortcuts = commands::skill_shortcuts(&harness.skills(), &registry);
    assert!(
        shortcuts
            .iter()
            .all(|shortcut| shortcut.command != "/disabled-skill")
    );
    assert!(shortcuts.iter().all(|shortcut| shortcut.command != "/help"));

    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };
    let outcome = commands::dispatch("/disabled-skill", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(msg) => {
            assert!(msg.contains("/skills enable"), "{msg}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn help_lists_dynamic_skill_commands_without_body() {
    let _guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![
        skill("db9", "SECRET SKILL BODY", false),
        skill("hidden-skill", "SECRET HIDDEN BODY", true),
    ];
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
    let outcome = commands::dispatch("/help", &registry, &ctx).await;

    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let text = capture.text();
    assert!(text.contains("Skill commands:"), "{text}");
    assert!(text.contains("/db9 [prompt]"), "{text}");
    assert!(!text.contains("/hidden-skill"), "{text}");
    assert!(!text.contains("SECRET"), "{text}");
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

#[tokio::test(flavor = "current_thread")]
async fn dispatch_cron_add_lists_toggles_and_removes_job() {
    let _guard = CRON_LOCK.lock().unwrap();
    triggers::global_cron_registry().clear_for_tests();

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

    let outcome = commands::dispatch(
        "/cron add \"*/10 * * * *\" summarize the repo state",
        &registry,
        &ctx,
    )
    .await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let jobs = triggers::global_cron_registry().list();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].schedule, "*/10 * * * *");
    assert_eq!(jobs[0].action, "summarize the repo state");
    assert!(jobs[0].enabled);

    let list = commands::dispatch("/cron list", &registry, &ctx).await;
    assert!(matches!(list, commands::CommandOutcome::Handled));
    let rendered =
        commands::render_cron_jobs(&[triggers::global_cron_registry().list()[0].clone()])
            .join("\n");
    assert!(
        rendered.contains("Cron jobs (session, 1):"),
        "cron list should label session scope: {rendered}"
    );
    assert!(rendered.contains("summarize the repo state"));

    let disable =
        commands::dispatch(&format!("/cron disable {}", jobs[0].id), &registry, &ctx).await;
    assert!(matches!(disable, commands::CommandOutcome::Handled));
    assert!(!triggers::global_cron_registry().list()[0].enabled);

    let enable = commands::dispatch(&format!("/cron enable {}", jobs[0].id), &registry, &ctx).await;
    assert!(matches!(enable, commands::CommandOutcome::Handled));
    assert!(triggers::global_cron_registry().list()[0].enabled);

    let remove = commands::dispatch(&format!("/cron remove {}", jobs[0].id), &registry, &ctx).await;
    assert!(matches!(remove, commands::CommandOutcome::Handled));
    assert!(triggers::global_cron_registry().list().is_empty());
    let entries = session.entries().await.unwrap();
    let audits = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "cron_control_plane" => data.as_ref(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        audits.len(),
        4,
        "cron writes should be audited: {entries:#?}"
    );
    assert_eq!(audits[0].get("op").and_then(|v| v.as_str()), Some("add"));
    assert_eq!(
        audits[0].get("actor").and_then(|v| v.as_str()),
        Some("slash")
    );
    assert_eq!(
        audits[0].get("after_enabled").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        audits[0].get("next_run").and_then(|v| v.as_str()).is_some(),
        "enabled cron audit should include next_run: {:#?}",
        audits[0]
    );
    assert_eq!(
        audits[1].get("op").and_then(|v| v.as_str()),
        Some("disable")
    );
    assert_eq!(
        audits[1].get("before_enabled").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        audits[1].get("after_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(audits[2].get("op").and_then(|v| v.as_str()), Some("enable"));
    assert_eq!(audits[3].get("op").and_then(|v| v.as_str()), Some("remove"));
    assert_eq!(
        audits[3].get("removed").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_cron_list_redacts_secret_like_action_preview() {
    let _guard = CRON_LOCK.lock().unwrap();
    triggers::global_cron_registry().clear_for_tests();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
    triggers::global_cron_registry()
        .add_job("* * * * *", &format!("use {secret}"))
        .unwrap();

    let rendered = commands::render_cron_jobs(&triggers::global_cron_registry().list()).join("\n");
    assert!(!rendered.contains(secret), "{rendered}");
    assert!(rendered.contains("[REDACTED:"), "{rendered}");
}

#[tokio::test(flavor = "current_thread")]
async fn dispatch_cron_add_audit_redacts_secret_like_action_preview() {
    let _guard = CRON_LOCK.lock().unwrap();
    triggers::global_cron_registry().clear_for_tests();

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

    let secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
    let outcome = commands::dispatch(
        &format!("/cron add \"0 * * * *\" call API with Bearer abcdefghijklmnop and {secret}"),
        &registry,
        &ctx,
    )
    .await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let entries = session.entries().await.unwrap();
    let audit = entries
        .iter()
        .find_map(|entry| match entry {
            SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "cron_control_plane" => data.as_ref(),
            _ => None,
        })
        .expect("cron add should write audit");
    let serialized = serde_json::to_string(audit).unwrap();
    assert!(!serialized.contains(secret), "{serialized}");
    assert!(
        !serialized.contains("Bearer abcdefghijklmnop"),
        "{serialized}"
    );
    assert!(serialized.contains("[REDACTED:"), "{serialized}");
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
        commands::CommandOutcome::LoginSecret {
            provider,
            storage_key,
            recovery_command,
        } => {
            assert_eq!(provider, "ds4");
            assert!(storage_key.is_none());
            assert!(recovery_command.is_none());
        }
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
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
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
async fn dispatch_hub_connect_rejects_secret_bearing_endpoint_without_echo() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let secret = "hub_agent_url_secret_should_not_leak";

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-connect-secret-endpoint",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch(
        &format!("/hub connect --endpoint https://pie.0xfefe.me/mcp?token={secret}"),
        &registry,
        &ctx,
    )
    .await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("must not include query"), "{message}");
            assert!(!message.contains(secret), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_hub_connect_unknown_args_do_not_echo_token_like_text() {
    let secret = "hub_agent_arg_secret_should_not_leak";
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-connect-unknown-arg",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch(&format!("/hub connect {secret}"), &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(
                message.contains("unknown option for /hub connect"),
                "{message}"
            );
            assert!(!message.contains(secret), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_hub_connect_parse_errors_do_not_echo_malformed_config() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let secret = "hub_agent_bad_toml_secret_should_not_leak";
    std::fs::write(
        temp.path().join("mcp.toml"),
        format!("this is not toml = \"{secret}\"\n[[server]]\n"),
    )
    .unwrap();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-connect-bad-config",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub connect", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("unable to parse mcp config"), "{message}");
            assert!(!message.contains(secret), "{message}");
            assert!(!message.contains("[[server]]"), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_hub_connect_writes_streamable_http_config_without_token_output() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-connect",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub connect", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let text = capture.text();
    assert!(text.contains("hub configured"), "{text}");
    assert!(text.contains("next: run /hub join"), "{text}");
    assert!(!text.contains("/hub login"), "{text}");
    assert!(!text.contains("pie-hub:default"), "{text}");
    assert!(!text.contains("hub_agent_"), "{text}");

    let config_text = std::fs::read_to_string(temp.path().join("mcp.toml")).unwrap();
    assert!(
        config_text.contains("kind = \"streamable_http\""),
        "{config_text}"
    );
    assert!(
        config_text.contains("endpoint = \"https://pie.0xfefe.me/mcp\""),
        "{config_text}"
    );
    assert!(
        config_text.contains("token_keychain_ref = \"pie-hub:default\""),
        "{config_text}"
    );
}

#[tokio::test]
async fn dispatch_hub_connect_rejects_custom_endpoint_reusing_official_scope() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-connect-custom-scope",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch(
        "/hub connect --endpoint https://staging.0xfefe.me/mcp",
        &registry,
        &ctx,
    )
    .await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(
                message.contains("built-in pie-hub profile is reserved"),
                "{message}"
            );
            assert!(
                message.contains("separate mcp.toml server name"),
                "{message}"
            );
            assert!(!message.contains("pie-hub:default"), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_hub_login_uses_secret_prompt_path() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-login",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub login", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::LoginSecret {
            provider,
            storage_key,
            recovery_command,
        } => {
            assert_eq!(provider, "pie-hub");
            assert_eq!(storage_key.as_deref(), Some("pie-hub:default"));
            assert_eq!(recovery_command.as_deref(), Some("/hub login"));
        }
        other => panic!("expected LoginSecret outcome, got {other:?}"),
    }
}

#[test]
fn hub_status_error_redacts_token_like_text() {
    let secret = "hub_agent_status_error_secret_should_not_leak";
    let session = "hub_hs_status_error_secret_should_not_leak";
    let message = commands::redact_hub_status_text(&format!(
        "transport failed with Authorization: Bearer {secret}; session={session}; bare={secret}"
    ));
    assert!(message.contains("[REDACTED"), "{message}");
    assert!(!message.contains(secret), "{message}");
    assert!(!message.contains(session), "{message}");
    assert!(!message.contains("Bearer hub_agent"), "{message}");
    assert!(!message.contains("hub_agent_"), "{message}");
    assert!(!message.contains("hub_hs_"), "{message}");
}

#[tokio::test]
async fn dispatch_hub_status_uses_built_in_default_without_mcp_config() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-status-built-in",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub status", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let text = capture.text();
    assert!(
        text.contains("config        configured (built-in)"),
        "{text}"
    );
    assert!(text.contains("server        pie-hub"), "{text}");
    assert!(text.contains("endpoint      pie.0xfefe.me"), "{text}");
    assert!(text.contains("credential    missing"), "{text}");
    assert!(text.contains("recovery      run /hub join"), "{text}");
    assert!(!temp.path().join("mcp.toml").exists());
    assert!(!text.contains("mcp.toml"), "{text}");
    assert!(!text.contains("pie-hub:default"), "{text}");
}

#[tokio::test]
async fn dispatch_hub_join_is_present_without_exposing_mcp_or_tokens() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-join-present",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub join unexpected", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(message) => {
            assert!(message.contains("usage: /hub join"), "{message}");
            assert!(!message.contains("mcp.toml"), "{message}");
            assert!(!message.contains("pie-hub:default"), "{message}");
            assert!(!message.contains("hub_agent_"), "{message}");
        }
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_hub_status_redacts_token_and_token_ref() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let capture = OutputCapture::install();

    std::fs::write(
        temp.path().join("mcp.toml"),
        r#"
[[server]]
name = "pie-hub"
kind = "streamable_http"
endpoint = "https://pie.0xfefe.me/mcp"
auth = { kind = "bearer", token_keychain_ref = "pie-hub:default" }
"#,
    )
    .unwrap();
    let secret = "hub_agent_status_secret_should_not_leak";
    let mut store = auth::AuthStore::default();
    store.set(
        "pie-hub:default",
        auth::ProviderCredential::ApiKey {
            value: secret.into(),
        },
    );
    store.save().unwrap();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-status",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub status", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let text = capture.text();
    assert!(text.contains("endpoint      pie.0xfefe.me"), "{text}");
    assert!(text.contains("credential    stored"), "{text}");
    assert!(!text.contains(secret), "{text}");
    assert!(!text.contains("pie-hub:default"), "{text}");
    assert!(!text.contains("Authorization"), "{text}");
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
async fn dispatch_skills_disable_persists_overlay_and_reloads() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let harness = harness_with_reloadable_skills(
        temp.path(),
        vec![skill("review-pr", "SECRET SKILL BODY", false)],
    );

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skills disable review-pr", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let skills = harness.skills();
    let skill = skills.iter().find(|s| s.name == "review-pr").unwrap();
    assert!(
        skill.disable_model_invocation,
        "reload should apply overlay"
    );

    let state = skills_state::load(temp.path()).await;
    assert_eq!(
        state
            .lookup("review-pr", SkillSource::User)
            .map(|entry| entry.enabled),
        Some(false)
    );
    let entries = harness.session().entries().await.unwrap();
    let audit = entries.iter().any(|entry| {
        matches!(
            entry,
            SessionTreeEntry::Custom { custom_type, data, .. }
                if custom_type == "skill_control_plane"
                    && data.as_ref().and_then(|d| d.get("actor")).and_then(|v| v.as_str()) == Some("slash")
                    && data.as_ref().and_then(|d| d.get("after_enabled")).and_then(|v| v.as_bool()) == Some(false)
        )
    });
    assert!(
        audit,
        "slash skill disable should write audit: {entries:#?}"
    );
}

#[tokio::test]
async fn dispatch_skills_enable_is_user_mediated_and_reuses_overlay() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let harness = harness_with_reloadable_skills(
        temp.path(),
        vec![skill("formatter", "SECRET SKILL BODY", true)],
    );

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skills enable formatter user", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let skills = harness.skills();
    let skill = skills.iter().find(|s| s.name == "formatter").unwrap();
    assert!(
        !skill.disable_model_invocation,
        "user slash command may explicitly enable a frontmatter-disabled skill"
    );

    let state = skills_state::load(temp.path()).await;
    assert_eq!(
        state
            .lookup("formatter", SkillSource::User)
            .map(|entry| entry.enabled),
        Some(true)
    );
    let entries = harness.session().entries().await.unwrap();
    let audit = entries.iter().any(|entry| {
        matches!(
            entry,
            SessionTreeEntry::Custom { custom_type, data, .. }
                if custom_type == "skill_control_plane"
                    && data.as_ref().and_then(|d| d.get("actor")).and_then(|v| v.as_str()) == Some("slash")
                    && data.as_ref().and_then(|d| d.get("after_enabled")).and_then(|v| v.as_bool()) == Some(true)
        )
    });
    assert!(audit, "slash skill enable should write audit: {entries:#?}");
}

#[tokio::test]
async fn dispatch_skills_show_prints_metadata_without_body() {
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let capture = OutputCapture::install();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    let mut s = skill("review-pr", "SECRET SKILL BODY", false);
    s.source = SkillSource::Project;
    opts.skills = vec![s];
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

    let outcome = commands::dispatch("/skills show review-pr project", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let text = capture.text();
    assert!(text.contains("Skill: review-pr (project)"), "{text}");
    assert!(text.contains("Status: enabled"), "{text}");
    assert!(text.contains("Path:"), "{text}");
    assert!(
        text.contains("Body: not shown"),
        "show should explain body omission:\n{text}"
    );
    assert!(
        !text.contains("SECRET SKILL BODY"),
        "show must not print SKILL.md body:\n{text}"
    );
}

#[tokio::test]
async fn dispatch_skills_reload_uses_harness_reload_and_prints_summary() {
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let capture = OutputCapture::install();
    let temp = tempfile::tempdir().unwrap();
    let harness = harness_with_reloadable_skills(
        temp.path(),
        vec![skill("one", "body", false), skill("two", "body", false)],
    );
    // Make the live catalog stale so the assertion proves `/skills reload` called the harness
    // reload closure rather than just recounting the current catalog.
    harness.replace_skills(Vec::new());

    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/skills reload", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert_eq!(harness.skills().len(), 2, "reload should refresh catalog");
    let text = capture.text();
    assert!(
        text.contains("reloaded skills: 2 loaded, 0 diagnostics"),
        "{text}"
    );
}

#[tokio::test]
async fn dispatch_skills_install_previews_then_confirms_without_body_echo() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _env_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let source_dir = temp.path().join("incoming");
    tokio::fs::create_dir_all(&source_dir).await.unwrap();
    let source_path = source_dir.join("SKILL.md");
    tokio::fs::write(
        &source_path,
        "---\nname: db9\ndescription: DB9 helper\n---\nSECRET SKILL BODY\n",
    )
    .await
    .unwrap();

    let harness = harness_with_disk_skill_reload(temp.path(), Vec::new());
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let capture = OutputCapture::install();
    let outcome = commands::dispatch(
        &format!("/skills install {}", source_path.display()),
        &registry,
        &ctx,
    )
    .await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(
        harness.skills().is_empty(),
        "preview should not mutate catalog"
    );
    let text = capture.text();
    assert!(text.contains("skill install preview: db9"), "{text}");
    assert!(text.contains("/skills install --confirm"), "{text}");
    assert!(!text.contains("SECRET SKILL BODY"), "{text}");

    let outcome = commands::dispatch(
        &format!("/skills install --confirm {}", source_path.display()),
        &registry,
        &ctx,
    )
    .await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let skills = harness.skills();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "db9");
    let text = capture.text();
    assert!(text.contains("installed skill 'db9'"), "{text}");
    assert!(!text.contains("SECRET SKILL BODY"), "{text}");
}

#[tokio::test]
async fn dispatch_skills_remove_previews_then_confirms_user_skill() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _env_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let skill_dir = temp.path().join("skills").join("db9");
    tokio::fs::create_dir_all(&skill_dir).await.unwrap();
    tokio::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: db9\ndescription: DB9 helper\n---\nSECRET SKILL BODY\n",
    )
    .await
    .unwrap();
    let harness =
        harness_with_disk_skill_reload(temp.path(), vec![user_skill_at(temp.path(), "db9", false)]);
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test",
        log_path: None,
        tool_count: 0,
        cwd: &cwd,
    };

    let capture = OutputCapture::install();
    let outcome = commands::dispatch("/skills remove db9", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(skill_dir.exists(), "preview should not remove files");
    let text = capture.text();
    assert!(text.contains("skill remove preview: db9 (user)"), "{text}");
    assert!(!text.contains("SECRET SKILL BODY"), "{text}");

    let outcome = commands::dispatch("/skills remove --confirm db9", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    assert!(!skill_dir.exists(), "confirm should remove user skill dir");
    assert!(
        harness.skills().iter().all(|s| s.name != "db9"),
        "reload should drop removed skill"
    );
    let text = capture.text();
    assert!(text.contains("removed skill 'db9'"), "{text}");
    assert!(!text.contains("SECRET SKILL BODY"), "{text}");
}

#[tokio::test]
async fn dispatch_skills_remove_project_skill_points_to_disable() {
    let _output_guard = COMMAND_OUTPUT_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    let mut s = skill("project-skill", "SECRET SKILL BODY", false);
    s.source = SkillSource::Project;
    s.file_path = temp
        .path()
        .join(".pie")
        .join("skills")
        .join("project-skill")
        .join("SKILL.md")
        .to_string_lossy()
        .to_string();
    opts.skills = vec![s];
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

    let outcome = commands::dispatch("/skills remove project-skill", &registry, &ctx).await;
    match outcome {
        commands::CommandOutcome::Error(msg) => {
            assert!(msg.contains("cannot be removed"), "{msg}");
            assert!(msg.contains("/skills disable project-skill"), "{msg}");
            assert!(!msg.contains("SECRET SKILL BODY"), "{msg}");
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
