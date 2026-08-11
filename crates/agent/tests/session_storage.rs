//! End-to-end session storage. Exercises both the memory and jsonl backends through the
//! `SessionStorage` trait surface.

use async_trait::async_trait;
use pie_agent_core::{
    JsonlSessionRepo, MemorySessionStorage, Session, SessionError, SessionStorage,
    SessionTreeEntry, build_session_context,
};
use serde_json::Value;
use std::sync::Arc;
use tempfile::tempdir;

/// Test adapter that yields after observing the current leaf. Without serialization at the
/// opened `Session` boundary, two concurrent appends both observe the same parent before either
/// write reaches the inner storage. Session clones must share the writer gate and prevent that
/// interleaving.
struct YieldingLeafStorage {
    inner: MemorySessionStorage,
}

impl YieldingLeafStorage {
    fn new() -> Self {
        Self {
            inner: MemorySessionStorage::new(),
        }
    }
}

#[async_trait]
impl SessionStorage for YieldingLeafStorage {
    async fn get_metadata_json(&self) -> Result<Value, SessionError> {
        self.inner.get_metadata_json().await
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        let leaf = self.inner.get_leaf_id().await?;
        tokio::task::yield_now().await;
        Ok(leaf)
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
        self.inner.get_path_to_root(leaf_id).await
    }

    async fn find_entries(&self, entry_type: &str) -> Result<Vec<SessionTreeEntry>, SessionError> {
        self.inner.find_entries(entry_type).await
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        self.inner.get_label(id).await
    }
}

fn user_message(text: &str) -> pie_agent_core::AgentMessage {
    pie_agent_core::AgentMessage::Llm(pie_ai::Message::User(pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(text.into()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }))
}

#[tokio::test]
async fn memory_session_roundtrips_messages() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage.clone() as Arc<dyn SessionStorage>);

    let id1 = session.append_message(user_message("first")).await.unwrap();
    let id2 = session
        .append_message(user_message("second"))
        .await
        .unwrap();
    assert_ne!(id1, id2);

    let leaf = session.leaf_id().await.unwrap();
    assert_eq!(leaf.as_deref(), Some(id2.as_str()));

    let entries = session.entries().await.unwrap();
    assert_eq!(entries.len(), 2);

    let branch = session.branch(None).await.unwrap();
    assert_eq!(branch.len(), 2);
    assert_eq!(branch[0].id(), id1);

    let ctx = build_session_context(&branch);
    assert_eq!(ctx.messages.len(), 2);
}

#[tokio::test]
async fn jsonl_session_persists_across_open() {
    let dir = tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());

    let session = repo.create("/some/cwd").await.unwrap();
    session.append_message(user_message("hello")).await.unwrap();
    let leaf = session.leaf_id().await.unwrap().expect("leaf id");

    // Re-open the file and verify the message is still there.
    let files = repo.list().await.unwrap();
    assert_eq!(files.len(), 1);
    let reopened = repo.open(&files[0]).await.unwrap();
    let entries = reopened.entries().await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id(), leaf);
}

#[tokio::test]
async fn jsonl_metadata_id_matches_session_file_stem() {
    let dir = tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());

    let session = repo.create("/some/cwd").await.unwrap();
    let files = repo.list().await.unwrap();
    let stem = files[0].file_stem().and_then(|s| s.to_str()).unwrap();
    let meta = session.storage().get_metadata_json().await.unwrap();

    assert_eq!(meta.get("id").and_then(|v| v.as_str()), Some(stem));
}

#[tokio::test]
async fn jsonl_explicit_leaf_moves_are_overridden_by_new_entries() {
    let dir = tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());

    let session = repo.create("/some/cwd").await.unwrap();
    let id_a = session.append_message(user_message("a")).await.unwrap();
    let _id_b = session.append_message(user_message("b")).await.unwrap();

    session.move_to(Some(&id_a), None).await.unwrap();
    let id_c = session.append_message(user_message("c")).await.unwrap();

    let files = repo.list().await.unwrap();
    let reopened = repo.open(&files[0]).await.unwrap();
    assert_eq!(
        reopened.leaf_id().await.unwrap().as_deref(),
        Some(id_c.as_str())
    );

    let branch = reopened.branch(None).await.unwrap();
    let ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec![id_a.as_str(), id_c.as_str()]);
}

#[tokio::test]
async fn jsonl_can_move_leaf_to_root() {
    let dir = tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());

    let session = repo.create("/some/cwd").await.unwrap();
    session.append_message(user_message("a")).await.unwrap();
    session.move_to(None, None).await.unwrap();

    let files = repo.list().await.unwrap();
    let reopened = repo.open(&files[0]).await.unwrap();
    assert_eq!(reopened.leaf_id().await.unwrap(), None);
    assert!(reopened.branch(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn branch_walks_parent_chain_in_root_to_leaf_order() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let id_a = session.append_message(user_message("a")).await.unwrap();
    let id_b = session.append_message(user_message("b")).await.unwrap();
    let id_c = session.append_message(user_message("c")).await.unwrap();

    let branch = session.branch(None).await.unwrap();
    let ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec![id_a.as_str(), id_b.as_str(), id_c.as_str()]);
}

#[tokio::test]
async fn compaction_summary_replaces_history_up_to_first_kept() {
    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let _id1 = session
        .append_message(user_message("dropped"))
        .await
        .unwrap();
    let first_kept = session.append_message(user_message("kept")).await.unwrap();
    let _comp = session
        .append_compaction("summary text", &first_kept, 100, None, false)
        .await
        .unwrap();
    let _id3 = session.append_message(user_message("after")).await.unwrap();

    let ctx = session.build_context().await.unwrap();
    // First message is the compaction summary, then the kept message, then "after".
    assert_eq!(ctx.messages.len(), 3);
    match &ctx.messages[0] {
        pie_agent_core::AgentMessage::Custom(c) => assert_eq!(c.role, "compaction_summary"),
        _ => panic!("expected compaction_summary custom message"),
    }
}

#[tokio::test]
async fn concurrent_appends_from_session_clones_form_one_chain() {
    let storage = Arc::new(YieldingLeafStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let root = session.append_message(user_message("root")).await.unwrap();

    let left_session = session.clone();
    let right_session = session.clone();
    let (left, right) = tokio::join!(
        left_session.append_message(user_message("left")),
        right_session.append_message(user_message("right")),
    );
    let left = left.unwrap();
    let right = right.unwrap();

    let left_parent = session
        .get_entry(&left)
        .await
        .unwrap()
        .unwrap()
        .parent_id()
        .map(str::to_owned);
    let right_parent = session
        .get_entry(&right)
        .await
        .unwrap()
        .unwrap()
        .parent_id()
        .map(str::to_owned);

    let left_then_right = left_parent.as_deref() == Some(root.as_str())
        && right_parent.as_deref() == Some(left.as_str());
    let right_then_left = right_parent.as_deref() == Some(root.as_str())
        && left_parent.as_deref() == Some(right.as_str());
    assert!(
        left_then_right || right_then_left,
        "concurrent appends must form one chain: left parent={left_parent:?}, right parent={right_parent:?}"
    );

    let branch = session.branch(None).await.unwrap();
    assert_eq!(branch.len(), 3);
}

async fn assert_leaf_move_is_append_only(session: Session) {
    let id_a = session.append_message(user_message("a")).await.unwrap();
    let id_b = session.append_message(user_message("b")).await.unwrap();

    session.move_to(Some(&id_a), None).await.unwrap();
    assert_eq!(
        session.leaf_id().await.unwrap().as_deref(),
        Some(id_a.as_str())
    );

    let entries = session.entries().await.unwrap();
    assert_eq!(entries.len(), 3);
    match entries.last().unwrap() {
        SessionTreeEntry::Leaf {
            parent_id,
            target_id,
            ..
        } => {
            assert_eq!(parent_id.as_deref(), Some(id_b.as_str()));
            assert_eq!(target_id.as_deref(), Some(id_a.as_str()));
        }
        other => panic!("expected append-only leaf entry, got {other:?}"),
    }

    let id_c = session.append_message(user_message("c")).await.unwrap();
    assert_eq!(
        session.get_entry(&id_c).await.unwrap().unwrap().parent_id(),
        Some(id_a.as_str())
    );
    let branch = session.branch(None).await.unwrap();
    let ids: Vec<&str> = branch.iter().map(SessionTreeEntry::id).collect();
    assert_eq!(ids, vec![id_a.as_str(), id_c.as_str()]);

    session.move_to(None, None).await.unwrap();
    assert_eq!(session.leaf_id().await.unwrap(), None);
    let entries = session.entries().await.unwrap();
    match entries.last().unwrap() {
        SessionTreeEntry::Leaf {
            parent_id,
            target_id,
            ..
        } => {
            assert_eq!(parent_id.as_deref(), Some(id_c.as_str()));
            assert_eq!(target_id, &None);
        }
        other => panic!("expected root leaf entry, got {other:?}"),
    }
    assert!(session.branch(None).await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_leaf_moves_match_append_only_storage_semantics() {
    let storage = Arc::new(MemorySessionStorage::new());
    assert_leaf_move_is_append_only(Session::new(storage as Arc<dyn SessionStorage>)).await;
}

#[tokio::test]
async fn jsonl_leaf_moves_match_append_only_storage_semantics() {
    let dir = tempdir().unwrap();
    let repo = JsonlSessionRepo::new(dir.path());
    assert_leaf_move_is_append_only(repo.create("/some/cwd").await.unwrap()).await;
}
