//! End-to-end tool tests. The tools are simple enough that we can exercise them directly
//! through their `AgentTool::execute` method without going through the agent loop.

use pie_agent_core::AgentTool;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

// Pull tool types out of the binary crate by including the source. Test-only.
// `skills_state` is a top-level module that `tools::set_skill_state` references via
// `crate::skills_state`, so it must be in this test binary's module tree too.
#[path = "../src/skills_state.rs"]
#[allow(dead_code)]
mod skills_state;
#[path = "../src/tools/mod.rs"]
#[allow(dead_code)]
mod tools;
#[path = "../src/triggers/mod.rs"]
#[allow(dead_code)]
mod triggers;

static DYNAMIC_TRIGGER_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn read_writes_then_reads() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("hello.txt");

    let write = tools::write::WriteTool;
    let read = tools::read::ReadTool;

    write
        .execute(
            "w1",
            serde_json::json!({ "path": path.to_str().unwrap(), "content": "hi\nthere\n" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();

    let r = read
        .execute(
            "r1",
            serde_json::json!({ "path": path.to_str().unwrap() }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let text = match &r.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("hi"));
    assert!(text.contains("there"));
}

#[tokio::test]
async fn ls_lists_entries() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();

    let ls = tools::ls::LsTool;
    let r = ls
        .execute(
            "l1",
            serde_json::json!({ "path": dir.path().to_str().unwrap() }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let text = match &r.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("a.txt"));
    assert!(text.contains("sub/"));
}

#[tokio::test]
async fn bash_captures_stdout_and_exit() {
    let bash = tools::bash::BashTool;
    let r = bash
        .execute(
            "b1",
            serde_json::json!({ "command": "echo hello && exit 0" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let text = match &r.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("hello"));
    assert!(text.contains("[exit 0]"));
}

#[tokio::test(flavor = "current_thread")]
async fn new_trigger_tool_registers_dynamic_rule() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let tool = tools::new_trigger_tool();
    let result = tool
        .execute(
            "new-trigger-1",
            serde_json::json!({
                "condition": "any future event matches this condition",
                "action": "echo fired"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should create rule");

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[0].condition,
        "any future event matches this condition"
    );
    assert_eq!(rules[0].action, "echo fired");

    let text = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("created dynamic trigger"));
}

#[tokio::test(flavor = "current_thread")]
async fn new_trigger_tool_can_request_chat_promotion() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();

    let tool = tools::new_trigger_tool();
    let result = tool
        .execute(
            "new-trigger-promote-1",
            serde_json::json!({
                "condition": "event says promote",
                "action": "echo promote",
                "promote_to_chat": true
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should register promoted rule");

    let rules = triggers::global_registry().list();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].promote_to_chat);
    assert_eq!(result.details["promote_to_chat"], true);
}

#[tokio::test(flavor = "current_thread")]
async fn list_triggers_tool_returns_dynamic_rules() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    let rule = triggers::global_registry()
        .add_rule("event says list me", "echo listed")
        .expect("rule");

    let tool = tools::list_triggers_tool();
    let result = tool
        .execute(
            "list-triggers-1",
            serde_json::json!({}),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should list rules");

    let text = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("dynamic trigger rules: 1"));
    assert!(text.contains(&rule.id));
    assert!(text.contains("event says list me"));
    assert_eq!(result.details["count"], 1);
    assert_eq!(result.details["rules"][0]["id"], rule.id);
}

#[tokio::test(flavor = "current_thread")]
async fn remove_trigger_tool_removes_dynamic_rule() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    let rule = triggers::global_registry()
        .add_rule("event says remove me", "echo removed")
        .expect("rule");

    let tool = tools::remove_trigger_tool();
    let result = tool
        .execute(
            "remove-trigger-1",
            serde_json::json!({ "id": rule.id }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should remove rule");

    assert!(triggers::global_registry().list().is_empty());
    let text = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("removed dynamic trigger"));
}

#[tokio::test(flavor = "current_thread")]
async fn set_trigger_state_tool_disables_and_enables_rule() {
    let _guard = DYNAMIC_TRIGGER_LOCK.lock().unwrap();
    triggers::global_registry().clear_for_tests();
    let rule = triggers::global_registry()
        .add_rule("event says pause me", "echo paused")
        .expect("rule");

    let tool = tools::set_trigger_state_tool();
    let disabled = tool
        .execute(
            "set-trigger-state-1",
            serde_json::json!({ "id": rule.id, "enabled": false }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should disable rule");
    assert_eq!(disabled.details["enabled"], false);
    assert!(!triggers::global_registry().list()[0].enabled);

    let enabled = tool
        .execute(
            "set-trigger-state-2",
            serde_json::json!({ "id": rule.id, "enabled": true }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("tool should enable rule");
    assert_eq!(enabled.details["enabled"], true);
    assert!(triggers::global_registry().list()[0].enabled);
}

#[tokio::test]
async fn bash_reports_nonzero_exit() {
    let bash = tools::bash::BashTool;
    let r = bash
        .execute(
            "b2",
            serde_json::json!({ "command": "exit 3" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    let text = match &r.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("[exit 3]"));
}

#[tokio::test]
async fn skill_tool_returns_wrapped_body_on_hit() {
    // Integration acceptance for issue #25 PR A: when a model issues `Skill { name: "..." }`
    // for a real registered skill, the tool result content is the body wrapped in a
    // `<skill name="...">` block (same shape `format_skills_for_system_prompt` advertises).
    use once_cell::sync::OnceCell as SyncOnceCell;
    use pie_agent_core::{
        AgentHarness, AgentHarnessOptions, MemorySessionStorage, Session, SessionStorage, Skill,
        SkillSource,
    };

    let storage = Arc::new(MemorySessionStorage::new()) as Arc<dyn SessionStorage>;
    let session = Session::new(storage);
    let mut opts = AgentHarnessOptions::new(faux_model_for_skill_test(), session);
    opts.skills = vec![Skill {
        name: "test-skill".into(),
        description: "description for the test skill".into(),
        file_path: "/tmp/skills/test-skill/SKILL.md".into(),
        content: "# test-skill\n\nDo the test thing.".into(),
        disable_model_invocation: false,
        source: SkillSource::User,
    }];
    let harness: Arc<AgentHarness> = Arc::new(AgentHarness::new(opts));

    let cell: tools::skill::SkillHarnessCell = Arc::new(SyncOnceCell::new());
    assert!(cell.set(harness).is_ok(), "set once");

    let tool = tools::skill::SkillTool::new(cell);
    let result = tool
        .execute(
            "call-1",
            serde_json::json!({ "name": "test-skill" }),
            CancellationToken::new(),
            None,
        )
        .await
        .expect("hit");

    let text = match &result.content[0] {
        pie_ai::UserContentBlock::Text(t) => t.text.clone(),
        _ => panic!("expected text content"),
    };
    assert!(text.contains("<skill name=\"test-skill\""));
    assert!(text.contains("Do the test thing."));
}

fn faux_model_for_skill_test() -> pie_ai::Model {
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

#[tokio::test]
async fn memory_save_then_load_block() {
    let dir = tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    let mem: Arc<dyn AgentTool> = Arc::new(tools::memory::MemoryTool::new(dir_path.clone()));

    mem.execute(
        "m1",
        serde_json::json!({
            "action": "save",
            "name": "User Likes Tabs",
            "description": "indentation preference",
            "content": "The user prefers tabs over spaces.",
            "type": "user",
        }),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    let block = tools::memory::load_memory_block(&dir_path).await;
    assert!(block.contains("<memory>"));
    assert!(block.contains("tabs"));
    assert!(block.contains("</memory>"));
}

/// `load_memory_block` must skip `MEMORY.md` so the index file isn't folded into the
/// system-prompt memory block alongside its actual entries. The MEMORY.md file is loaded
/// separately by the harness; duplicating it into this block surfaces the same content
/// twice. Code-review item #10 (2026-05-22).
#[tokio::test]
async fn memory_block_excludes_memory_md_index_file() {
    let dir = tempdir().unwrap();
    let dir_path = dir.path().to_path_buf();
    // Hand-write a MEMORY.md with a recognizable string we'd see if it leaks into the
    // block, and one real entry so the block actually opens.
    tokio::fs::write(
        dir_path.join("MEMORY.md"),
        "INDEX_SENTINEL_SHOULD_NOT_LEAK\n- [User Likes Tabs](user_likes_tabs.md)\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        dir_path.join("user_likes_tabs.md"),
        "---\nname: user-likes-tabs\ndescription: indentation\nmetadata:\n  type: user\n---\n\nThe user prefers tabs.\n",
    )
    .await
    .unwrap();

    let block = tools::memory::load_memory_block(&dir_path).await;
    assert!(
        block.contains("<memory>"),
        "block should be populated by the user entry: {block}"
    );
    assert!(
        block.contains("prefers tabs"),
        "real entry must appear in block: {block}"
    );
    assert!(
        !block.contains("INDEX_SENTINEL_SHOULD_NOT_LEAK"),
        "MEMORY.md index must not leak into the injected memory block: {block}"
    );
    assert!(
        !block.contains("--- MEMORY.md ---"),
        "MEMORY.md should not appear as a section header: {block}"
    );
}
