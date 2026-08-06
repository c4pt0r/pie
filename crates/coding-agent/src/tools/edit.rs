//! `edit` tool — exact-string replacement. Models `packages/coding-agent/src/core/tools/edit.ts`
//! at a simplified level: read → require unique `old_string` (unless `replace_all`) → write
//! the new file. Reports a 3-line context diff in the result so the LLM sees what changed.

use async_trait::async_trait;
use pie_agent_core::{AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub struct EditTool;

#[async_trait]
impl AgentTool for EditTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "edit"
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
        let old = params
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `old_string`"))?;
        let new_ = params
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `new_string`"))?;
        let replace_all = params
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if old == new_ {
            return Err(AgentToolError::from(
                "old_string must differ from new_string",
            ));
        }

        // Serialize the whole read-modify-write per file so a concurrent `edit`/`write` on the
        // same path cannot read a stale body and clobber the other's change. See
        // `tools::fs_guard`.
        let occurrences = crate::tools::fs_guard::with_file_lock(
            std::path::Path::new(path),
            || async {
                let body = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| AgentToolError::from(format!("read {path}: {e}")))?;

                let occurrences = body.matches(old).count();
                if occurrences == 0 {
                    return Err(AgentToolError::from(format!(
                        "old_string not found in {path}"
                    )));
                }
                if occurrences > 1 && !replace_all {
                    return Err(AgentToolError::from(format!(
                        "old_string matched {occurrences} times in {path}; pass replace_all=true to replace every occurrence, or include more surrounding context to make it unique"
                    )));
                }

                let new_body = if replace_all {
                    body.replace(old, new_)
                } else {
                    body.replacen(old, new_, 1)
                };
                tokio::fs::write(path, new_body.as_bytes())
                    .await
                    .map_err(|e| AgentToolError::from(format!("write {path}: {e}")))?;
                Ok(occurrences)
            },
        )
        .await?;

        let preview = render_diff_preview(old, new_);
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Edited {path} ({occurrences} replacement{}).\n{preview}",
                if occurrences == 1 { "" } else { "s" }
            ))],
            details: json!({
                "path": path,
                "replacements": occurrences,
                "replaceAll": replace_all,
            }),
            terminate: None,
        })
    }
}

/// Render a minimal 3-context-line diff of the changed region. We don't have a real diff
/// algorithm here; we just show the old vs new strings labeled — sufficient for the LLM to
/// confirm the edit landed.
fn render_diff_preview(old: &str, new_: &str) -> String {
    let mut s = String::from("--- before\n");
    for line in old.lines().take(10) {
        s.push_str("- ");
        s.push_str(line);
        s.push('\n');
    }
    s.push_str("+++ after\n");
    for line in new_.lines().take(10) {
        s.push_str("+ ");
        s.push_str(line);
        s.push('\n');
    }
    s
}

use once_cell::sync::Lazy;
static DEFINITION: Lazy<Tool> = Lazy::new(|| {
    Tool {
    name: "edit".into(),
    description:
        "Replace an exact substring in a file. The substring must be unique unless `replace_all` is true. Use `read` first to confirm the exact text to match, including surrounding context."
            .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
            "old_string": { "type": "string", "description": "Exact substring to replace. Include enough surrounding context to make it unique within the file." },
            "new_string": { "type": "string", "description": "Replacement string. Use the empty string to delete." },
            "replace_all": { "type": "boolean", "description": "Replace every occurrence rather than requiring uniqueness." },
        },
        "required": ["path", "old_string", "new_string"],
    }),
}
});

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn replaces_unique_substring() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "hello world\nfoo bar\n").unwrap();
        let tool = EditTool;
        tool.execute(
            "e",
            json!({ "path": p.to_str().unwrap(), "old_string": "hello", "new_string": "hey" }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert_eq!(body, "hey world\nfoo bar\n");
    }

    #[tokio::test]
    async fn rejects_ambiguous_match() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "foo\nfoo\n").unwrap();
        let tool = EditTool;
        let r = tool
            .execute(
                "e",
                json!({ "path": p.to_str().unwrap(), "old_string": "foo", "new_string": "bar" }),
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(r.is_err());
    }

    /// Regression for the concurrent read-modify-write hazard (issue #230 / T0). Fire many
    /// `edit` calls at the SAME file at once, each replacing a distinct, non-overlapping token.
    /// With per-file serialization every replacement must survive; without it, edits that read
    /// a stale body would clobber each other and lose updates.
    #[tokio::test]
    async fn concurrent_edits_same_file_do_not_lose_updates() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("c.txt");
        // Delimited tokens so no `old_string` is a substring of another ("<L1>" is not inside
        // "<L10>"), keeping each edit an unambiguous single-occurrence replacement.
        let initial: String = (0..24).map(|i| format!("<L{i}>\n")).collect();
        std::fs::write(&p, &initial).unwrap();

        let path = p.to_str().unwrap().to_string();
        let mut handles = Vec::new();
        for i in 0..24 {
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                EditTool
                    .execute(
                        "e",
                        json!({
                            "path": path,
                            "old_string": format!("<L{i}>"),
                            "new_string": format!("<X{i}>"),
                        }),
                        CancellationToken::new(),
                        None,
                    )
                    .await
            }));
        }
        for h in handles {
            h.await
                .unwrap()
                .expect("each concurrent edit should succeed");
        }

        let body = std::fs::read_to_string(&p).unwrap();
        for i in 0..24 {
            assert!(
                body.contains(&format!("<X{i}>")),
                "replacement <X{i}> was lost under concurrency:\n{body}"
            );
            assert!(
                !body.contains(&format!("<L{i}>")),
                "original <L{i}> survived (an edit was clobbered):\n{body}"
            );
        }
    }

    #[tokio::test]
    async fn replace_all_handles_multiple() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, "foo\nfoo\nfoo\n").unwrap();
        let tool = EditTool;
        tool.execute(
            "e",
            json!({
                "path": p.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": true,
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "bar\nbar\nbar\n");
    }
}
