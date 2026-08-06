//! `write` tool. Mirrors `packages/coding-agent/src/core/tools/write.ts` — full-file overwrite
//! with parent-directory creation. Simpler than TS (no atomic temp-file + rename, no diff
//! preview); good enough for the simple agent.

use async_trait::async_trait;
use pie_agent_core::{AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub struct WriteTool;

#[async_trait]
impl AgentTool for WriteTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "write"
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `path`"))?;
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `content`"))?;

        // Serialize the overwrite per file so a concurrent `edit`/`write` on the same path
        // cannot interleave. See `tools::fs_guard`.
        crate::tools::fs_guard::with_file_lock(std::path::Path::new(path), || async {
            if let Some(parent) = std::path::Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
            }
            tokio::fs::write(path, content.as_bytes())
                .await
                .map_err(|e| AgentToolError::from(format!("write {path}: {e}")))
        })
        .await?;

        let bytes = content.len();
        let lines = content.lines().count();
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Wrote {bytes} bytes ({lines} lines) to {path}"
            ))],
            details: json!({ "path": path, "bytes": bytes, "lines": lines }),
            terminate: None,
        })
    }
}

use once_cell::sync::Lazy;
static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "write".into(),
    description:
        "Write (or overwrite) a UTF-8 text file. Parent directories are created if missing.".into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
            "content": { "type": "string", "description": "Full file contents" },
        },
        "required": ["path", "content"],
    }),
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

    /// Regression for the QA gate on issue #230 / T0: prove `WriteTool` actually acquires the
    /// shared per-file `fs_guard` lock — not merely that `fs_guard` works in isolation. We hold
    /// the file's lock via the same API the tools use, launch a concurrent `WriteTool` against
    /// that file, and assert it makes no progress until we release. If a change dropped the
    /// `with_file_lock` wrapper from `WriteTool`, the write would complete during the hold window
    /// (and mutate the file), failing this test.
    #[tokio::test]
    async fn write_tool_participates_in_shared_file_lock() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "orig").unwrap();
        let path = p.to_str().unwrap().to_string();

        // A holder task grabs the per-file lock (same API the tools use) and keeps it until we
        // tell it to release, so the lock is deterministically held while we probe the writer.
        let (tx_acquired, rx_acquired) = tokio::sync::oneshot::channel::<()>();
        let (tx_release, rx_release) = tokio::sync::oneshot::channel::<()>();
        let p_holder = p.clone();
        let holder = tokio::spawn(async move {
            crate::tools::fs_guard::with_file_lock(&p_holder, || async move {
                tx_acquired.send(()).unwrap();
                rx_release.await.unwrap();
            })
            .await;
        });
        rx_acquired.await.unwrap(); // lock is now held by `holder`

        let entered = Arc::new(AtomicBool::new(false));
        let completed = Arc::new(AtomicBool::new(false));
        let writer = tokio::spawn({
            let entered = Arc::clone(&entered);
            let completed = Arc::clone(&completed);
            async move {
                entered.store(true, Ordering::SeqCst);
                WriteTool
                    .execute(
                        "w",
                        json!({ "path": path, "content": "new" }),
                        CancellationToken::new(),
                        None,
                    )
                    .await
                    .unwrap();
                completed.store(true, Ordering::SeqCst);
            }
        });

        // Ample time for the spawned write to be scheduled and reach the lock.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            entered.load(Ordering::SeqCst),
            "writer task should have been scheduled"
        );
        assert!(
            !completed.load(Ordering::SeqCst),
            "WriteTool must block on the per-file lock while it is held elsewhere"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "orig",
            "file must be untouched while the lock is held"
        );

        // Release the lock — the write may now complete and must land its content.
        tx_release.send(()).unwrap();
        holder.await.unwrap();
        writer.await.unwrap();
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
    }
}
