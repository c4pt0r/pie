//! End-to-end coverage for dynamic trigger creation from ordinary conversation.
//!
//! The model is simulated with a deterministic stream: the first user prompt creates a
//! dynamic rule via the model-facing `NewTrigger` tool, and a later runtime `Trigger`
//! causes the trigger sub-agent to call `bash` for the matching rule action.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate,
    CredentialScope, HarnessEvent, MemorySessionStorage, PayloadVisibility, ReplacementPolicy,
    Session, SessionStorage, Skill, SkillSource, SourceKind, StreamFn, Trigger, TriggerAuthority,
    TriggerSource,
};
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, AssistantRole,
    ContentBlock, Context, DoneReason, Message, ModelCost, StopReason, Tool, ToolCall, Usage,
    UserContent,
};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
#[path = "../src/bug_report.rs"]
mod bug_report;
#[allow(dead_code)]
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/export.rs"]
mod export;
#[allow(dead_code)]
#[path = "../src/triggers/mod.rs"]
mod triggers;

static ENV_LOCK: Mutex<()> = Mutex::new(());
static DYNAMIC_TRIGGER_LOCK: Mutex<()> = Mutex::new(());

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

struct RecordingBashTool {
    def: Tool,
    calls: Arc<parking_lot::Mutex<Vec<String>>>,
}

struct HomeFileBashTool {
    def: Tool,
    calls: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl HomeFileBashTool {
    fn new(calls: Arc<parking_lot::Mutex<Vec<String>>>) -> Self {
        Self {
            def: Tool {
                name: "bash".into(),
                description: "run a shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
            calls,
        }
    }
}

#[async_trait]
impl AgentTool for HomeFileBashTool {
    fn definition(&self) -> &Tool {
        &self.def
    }

    fn label(&self) -> &str {
        "bash"
    }

    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.calls.lock().push(command.clone());
        let home = std::env::var("HOME").map_err(|e| AgentToolError::from(e.to_string()))?;
        let path = std::path::Path::new(&home).join("helloworld");
        let output = std::fs::read_to_string(&path)
            .map_err(|e| AgentToolError::from(format!("read {}: {e}", path.display())))?;
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(format!(
                "$ {command}\n{output}\n[exit 0]"
            ))],
            details: serde_json::json!({ "command": command }),
            terminate: None,
        })
    }
}

struct EnvGuard {
    key: &'static str,
    old: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let old = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(old) = &self.old {
            unsafe { std::env::set_var(self.key, old) };
        } else {
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

impl RecordingBashTool {
    fn new(calls: Arc<parking_lot::Mutex<Vec<String>>>) -> Self {
        Self {
            def: Tool {
                name: "bash".into(),
                description: "run a shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
            calls,
        }
    }
}

#[async_trait]
impl AgentTool for RecordingBashTool {
    fn definition(&self) -> &Tool {
        &self.def
    }

    fn label(&self) -> &str {
        "bash"
    }

    async fn execute(
        &self,
        _id: &str,
        params: serde_json::Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        self.calls.lock().push(command.clone());
        Ok(AgentToolResult {
            content: vec![pie_ai::UserContentBlock::text(format!("ran: {command}"))],
            details: serde_json::json!({ "command": command }),
            terminate: None,
        })
    }
}

fn dynamic_trigger_stream() -> StreamFn {
    Arc::new(|_, context: &Context, _| stream_one(dynamic_trigger_response(context)))
}

fn recording_dynamic_trigger_stream(
    seen_system_prompts: Arc<parking_lot::Mutex<Vec<String>>>,
) -> StreamFn {
    Arc::new(move |_, context: &Context, _| {
        if let Some(system_prompt) = &context.system_prompt {
            seen_system_prompts.lock().push(system_prompt.clone());
        }
        stream_one(dynamic_trigger_response(context))
    })
}

fn dynamic_trigger_response(context: &Context) -> AssistantMessage {
    let last_text = last_message_text(context);
    let transcript_text = context
        .messages
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    let has_tool_result = context
        .messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult(_)));
    if has_tool_result && transcript_text.contains("hello from home e2e") {
        let id = first_dynamic_rule_id(&transcript_text).unwrap_or("dyn-missing");
        assistant_text(&format!("matched {id}: hello from home e2e"))
    } else if has_tool_result {
        let id = first_dynamic_rule_id(&transcript_text).unwrap_or("dyn-missing");
        assistant_text(&format!("matched {id}: done"))
    } else if !last_text.contains("Dynamic trigger rules") && last_text.contains("helloworld") {
        assistant_tool_call(
            "call-new-trigger-home",
            "NewTrigger",
            serde_json::json!({
                "condition": "$HOME contains a file named helloworld",
                "action": "print the contents of $HOME/helloworld",
                "spec": last_text,
            }),
        )
    } else if last_text.contains("visible to future turns") {
        assistant_tool_call(
            "call-new-trigger-promote",
            "NewTrigger",
            serde_json::json!({
                "condition": "the event says build finished",
                "action": "echo dynamic-fired",
                "promote_to_chat": true
            }),
        )
    } else if last_text.contains("Create a trigger") {
        assistant_tool_call(
            "call-new-trigger",
            "NewTrigger",
            serde_json::json!({
                "condition": "the event says build finished",
                "action": "echo dynamic-fired"
            }),
        )
    } else if last_text.contains("Dynamic trigger rules") && last_text.contains("helloworld") {
        assistant_tool_call(
            "call-home-helloworld-bash",
            "bash",
            serde_json::json!({
                "command": "test -f \"$HOME/helloworld\" && cat \"$HOME/helloworld\""
            }),
        )
    } else if last_text.contains("Dynamic trigger rules")
        && last_text.contains("dynamic periodic check")
        && last_text.contains("echo periodic-fired")
    {
        assistant_tool_call(
            "call-periodic-bash",
            "bash",
            serde_json::json!({ "command": "echo periodic-fired" }),
        )
    } else if last_text.contains("Dynamic trigger rules")
        && last_text.contains("build finished")
        && last_text.contains("echo dynamic-fired")
    {
        assistant_tool_call(
            "call-bash",
            "bash",
            serde_json::json!({ "command": "echo dynamic-fired" }),
        )
    } else {
        assistant_text("no dynamic trigger rule matched")
    }
}

fn first_dynamic_rule_id(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] != b"dyn-" {
            i += 1;
            continue;
        }
        let start = i;
        i += 4;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        if i - start == 36 {
            return Some(&text[start..i]);
        }
    }
    None
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

fn last_message_text(context: &Context) -> String {
    context
        .messages
        .last()
        .map(message_text)
        .unwrap_or_default()
}

fn message_text(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|block| format!("{block:?}"))
                .collect::<Vec<_>>()
                .join("\n"),
        },
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| format!("{block:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Message::ToolResult(tool) => tool
            .content
            .iter()
            .map(|block| format!("{block:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn sample_event_trigger() -> Trigger {
    Trigger {
        source: TriggerSource::Local {
            subkind: "e2e".into(),
        },
        source_kind: SourceKind::Local,
        source_label: "local:e2e".into(),
        event_label: "build finished".into(),
        payload_visibility: PayloadVisibility::Local,
        payload_summary: Some("build finished successfully".into()),
        payload: None,
        idempotency_key: "dynamic-e2e-build-finished".into(),
        replacement_policy: ReplacementPolicy::Drop,
        trace_id: "trace-dynamic-e2e".into(),
        authority: TriggerAuthority {
            principal_id: "e2e".into(),
            principal_label: "e2e".into(),
            credential_scope: CredentialScope::User,
            allowed_source_actions: vec![],
            expires_at: None,
        },
        received_at: chrono::Utc::now(),
    }
}

async fn wait_for_completed(
    events: &Arc<parking_lot::Mutex<Vec<HarnessEvent>>>,
    trace_id: &str,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if events.lock().iter().any(|event| {
            matches!(
                event,
                HarnessEvent::TriggerCompleted { trace_id: t, .. } if t == trace_id
            )
        }) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_bash_call(calls: &Arc<parking_lot::Mutex<Vec<String>>>, command: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if calls.lock().iter().any(|call| call == command) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn any_trigger_result_summary(entries: &[pie_agent_core::SessionTreeEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            pie_agent_core::SessionTreeEntry::Custom {
                custom_type, data, ..
            } if custom_type == "trigger_result" => data
                .as_ref()
                .and_then(|d| d.get("summary"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn natural_language_prompt_creates_dynamic_trigger_and_runtime_event_executes_action() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![
        Arc::new(triggers::NewTriggerTool) as Arc<dyn AgentTool>,
        Arc::new(RecordingBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>,
    ];
    opts.stream_fn = Some(dynamic_trigger_stream());
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(
        triggers::global_registry().clone(),
    ));
    let harness = AgentHarness::new(opts);

    harness
        .prompt("Create a trigger: when the event says build finished, run echo dynamic-fired")
        .await
        .expect("prompt should create dynamic trigger");

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].condition, "the event says build finished");
    assert_eq!(rules[0].action, "echo dynamic-fired");

    let events: Arc<parking_lot::Mutex<Vec<HarnessEvent>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let event_sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |event| {
        event_sink.lock().push(event);
    }));
    let _fire_once_unsub = harness.subscribe_harness(triggers::fire_once_harness_listener(
        triggers::global_registry().clone(),
    ));

    let _ = harness.handle_trigger(sample_event_trigger()).await;
    assert!(
        wait_for_completed(&events, "trace-dynamic-e2e").await,
        "dynamic trigger sub-agent should complete"
    );
    assert_eq!(bash_calls.lock().as_slice(), ["echo dynamic-fired"]);
    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert!(!rules[0].enabled, "fire_once rule should be disabled");
    assert!(
        rules[0].fired_at.is_some(),
        "fire_once rule should record fired_at"
    );

    let entries = session.entries().await.expect("session entries");
    assert!(
        entries.iter().any(|entry| {
            matches!(
                entry,
                pie_agent_core::SessionTreeEntry::Custom { custom_type, data, .. }
                    if custom_type == "trigger_result"
                        && data
                            .as_ref()
                            .and_then(|d| d.get("trace_id"))
                            .and_then(|v| v.as_str())
                            == Some("trace-dynamic-e2e")
            )
        }),
        "trigger_result audit should be written: {entries:#?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn promoted_dynamic_trigger_result_enters_parent_chat_context() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![
        Arc::new(triggers::NewTriggerTool) as Arc<dyn AgentTool>,
        Arc::new(RecordingBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>,
    ];
    opts.stream_fn = Some(dynamic_trigger_stream());
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(
        triggers::global_registry().clone(),
    ));
    let harness = AgentHarness::new(opts);

    harness
        .prompt(
            "Create a trigger: when the event says build finished, run echo dynamic-fired, and make the result visible to future turns",
        )
        .await
        .expect("prompt should create promoted dynamic trigger");

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].promote_to_chat);

    let events: Arc<parking_lot::Mutex<Vec<HarnessEvent>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let event_sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |event| {
        event_sink.lock().push(event);
    }));

    let _ = harness.handle_trigger(sample_event_trigger()).await;
    assert!(
        wait_for_completed(&events, "trace-dynamic-e2e").await,
        "dynamic trigger sub-agent should complete"
    );

    let parent_messages = harness.agent().state().messages.clone();
    assert!(
        parent_messages.iter().any(|message| {
            matches!(
                message,
                pie_agent_core::AgentMessage::Llm(Message::User(user))
                    if matches!(&user.content, UserContent::Text(text) if text.contains("[Trigger trace-dynamic-e2e]") && text.contains("matched dyn-"))
            )
        }),
        "promoted trigger result should be present in parent agent context: {parent_messages:#?}"
    );

    let entries = session.entries().await.expect("session entries");
    assert!(
        entries.iter().any(|entry| {
            matches!(
                entry,
                pie_agent_core::SessionTreeEntry::Custom { custom_type, data, .. }
                    if custom_type == "trigger_promotion"
                        && data
                            .as_ref()
                            .and_then(|d| d.get("state"))
                            .and_then(|v| v.as_str())
                            == Some("success")
            )
        }),
        "promotion audit should be written: {entries:#?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audit_only_match_is_not_promoted_when_other_rule_requests_chat_promotion() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let audit_rule = triggers::global_registry()
        .add_rule_with_flags(
            "the event says build finished",
            "echo dynamic-fired",
            true,
            false,
        )
        .expect("audit rule");
    let promote_rule = triggers::global_registry()
        .add_rule_with_flags(
            "the event says deploy finished",
            "echo deploy-fired",
            true,
            true,
        )
        .expect("promote rule");

    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![Arc::new(RecordingBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>];
    opts.stream_fn = Some(dynamic_trigger_stream());
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(
        triggers::global_registry().clone(),
    ));
    let harness = AgentHarness::new(opts);

    let events: Arc<parking_lot::Mutex<Vec<HarnessEvent>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let event_sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |event| {
        event_sink.lock().push(event);
    }));

    let _ = harness.handle_trigger(sample_event_trigger()).await;
    assert!(
        wait_for_completed(&events, "trace-dynamic-e2e").await,
        "dynamic trigger sub-agent should complete"
    );
    assert_eq!(bash_calls.lock().as_slice(), ["echo dynamic-fired"]);

    let parent_messages = harness.agent().state().messages.clone();
    assert!(
        !parent_messages.iter().any(|message| {
            matches!(
                message,
                pie_agent_core::AgentMessage::Llm(Message::User(user))
                    if matches!(&user.content, UserContent::Text(text) if text.contains("[Trigger trace-dynamic-e2e]"))
            )
        }),
        "audit-only matched rule {} must not be promoted just because {} requested promotion: {parent_messages:#?}",
        audit_rule.id,
        promote_rule.id
    );
}

#[tokio::test(flavor = "current_thread")]
async fn trigger_sub_agent_sees_parent_skill_catalog() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    triggers::global_registry()
        .add_rule(
            "the event says build finished",
            "echo dynamic-fired after considering available skills",
        )
        .expect("rule");

    let seen_system_prompts: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.skills = vec![Skill {
        name: "alpha".into(),
        description: "handles alpha workflows".into(),
        file_path: "/tmp/skills/alpha/SKILL.md".into(),
        content: "Alpha skill body.".into(),
        disable_model_invocation: false,
        source: SkillSource::User,
    }];
    opts.tools = vec![Arc::new(RecordingBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>];
    opts.stream_fn = Some(recording_dynamic_trigger_stream(
        seen_system_prompts.clone(),
    ));
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(
        triggers::global_registry().clone(),
    ));
    let harness = AgentHarness::new(opts);

    let events: Arc<parking_lot::Mutex<Vec<HarnessEvent>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let event_sink = events.clone();
    let _unsub = harness.subscribe_harness(Arc::new(move |event| {
        event_sink.lock().push(event);
    }));

    let _ = harness.handle_trigger(sample_event_trigger()).await;
    assert!(
        wait_for_completed(&events, "trace-dynamic-e2e").await,
        "dynamic trigger sub-agent should complete"
    );
    assert_eq!(bash_calls.lock().as_slice(), ["echo dynamic-fired"]);

    let prompts = seen_system_prompts.lock().clone();
    assert!(
        prompts.iter().any(|prompt| {
            prompt.contains("<skills>")
                && prompt.contains("- name: alpha")
                && prompt.contains("description: handles alpha workflows")
        }),
        "trigger sub-agent should inherit the parent skill catalog in its system prompt: {prompts:#?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn home_helloworld_trigger_prints_file_contents() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _dynamic_guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let home = tempfile::tempdir().expect("home tempdir");
    std::fs::write(home.path().join("helloworld"), "hello from home e2e").expect("write fixture");
    let _home_guard = EnvGuard::set("HOME", home.path().to_str().expect("home path"));

    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session.clone());
    opts.tools = vec![
        Arc::new(triggers::NewTriggerTool) as Arc<dyn AgentTool>,
        Arc::new(HomeFileBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>,
    ];
    opts.stream_fn = Some(dynamic_trigger_stream());
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(
        triggers::global_registry().clone(),
    ));
    let harness = Arc::new(AgentHarness::new(opts));
    let _fire_once_unsub = harness.subscribe_harness(triggers::fire_once_harness_listener(
        triggers::global_registry().clone(),
    ));
    harness.register_notification_hook(Arc::new(triggers::DynamicTriggerCheckHook::with_interval(
        triggers::global_registry().clone(),
        Duration::from_millis(10),
    )));

    let user_request = concat!(
        "\u{5f53} $home \u{76ee}\u{5f55}\u{4e0b}\u{6709}\u{4e2a} helloworld ",
        "\u{6587}\u{4ef6}\u{ff0c}\u{90a3}\u{4e48}\u{5c31}\u{6253}\u{5370}",
        "\u{5b83}\u{7684}\u{5185}\u{5bb9}\u{51fa}\u{6765}"
    );
    harness
        .prompt(user_request)
        .await
        .expect("prompt should create home trigger");

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].condition.contains("helloworld"));
    assert!(rules[0].action.contains("$HOME/helloworld"));

    assert!(
        wait_for_bash_call(
            &bash_calls,
            "test -f \"$HOME/helloworld\" && cat \"$HOME/helloworld\""
        )
        .await,
        "periodic dynamic check should inspect and print the home fixture"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rules = triggers::global_registry().list();
        if !rules[0].enabled {
            assert!(
                rules[0].fired_at.is_some(),
                "fire_once rule should record fired_at"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fire_once rule was not disabled after successful trigger"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let entries = session.entries().await.expect("session entries");
        let summaries = any_trigger_result_summary(&entries);
        if let Some(summary) = summaries
            .iter()
            .find(|summary| summary.contains("hello from home e2e"))
        {
            assert!(summary.contains("hello from home e2e"), "{summary}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "trigger_result summary did not include file contents: {summaries:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn periodic_dynamic_hook_checks_rules_and_executes_matching_action() {
    let registry = triggers::dynamic::DynamicTriggerRegistry::new();
    registry
        .add_rule("a dynamic periodic check arrives", "echo periodic-fired")
        .expect("rule");

    let bash_calls: Arc<parking_lot::Mutex<Vec<String>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let mut opts = AgentHarnessOptions::new(faux_model(), session);
    opts.tools = vec![Arc::new(RecordingBashTool::new(bash_calls.clone())) as Arc<dyn AgentTool>];
    opts.stream_fn = Some(dynamic_trigger_stream());
    opts.before_trigger_action = Some(triggers::before_trigger_action_hook(registry.clone()));
    let harness = Arc::new(AgentHarness::new(opts));
    harness.register_notification_hook(Arc::new(triggers::DynamicTriggerCheckHook::with_interval(
        registry,
        Duration::from_millis(10),
    )));

    assert!(
        wait_for_bash_call(&bash_calls, "echo periodic-fired").await,
        "periodic dynamic hook should emit a check trigger that executes the matching rule"
    );
}
