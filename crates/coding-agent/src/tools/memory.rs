//! `memory` tool — write to / read from cross-session memory. Models the same idea as Claude
//! Code's `MEMORY.md`-style auto memory: the assistant calls this to persist a fact ("the user
//! prefers X", "the API key lives at Y"), and on startup we inject the existing memory into
//! the system prompt so the agent sees it for free in every new session.
//!
//! Layout under `<memory_dir>/`:
//! - `MEMORY.md` — index, always loaded
//! - `<slug>.md` — individual entries with YAML frontmatter (name / description / type)

use async_trait::async_trait;
use pie_agent_core::{AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub struct MemoryTool {
    pub dir: PathBuf,
}

impl MemoryTool {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() || c == '-' || c == '_' {
            out.push('-');
        }
    }
    // collapse multiple hyphens
    let mut compact = String::with_capacity(out.len());
    let mut prev_hyphen = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_hyphen {
                compact.push(c);
            }
            prev_hyphen = true;
        } else {
            compact.push(c);
            prev_hyphen = false;
        }
    }
    compact.trim_matches('-').to_string()
}

#[async_trait]
impl AgentTool for MemoryTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "memory"
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        _cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AgentToolError::from("missing `action` (save | list | read | forget)")
            })?;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| AgentToolError::from(format!("memory dir: {e}")))?;

        match action {
            "save" => self.save(&params).await,
            "list" => self.list().await,
            "read" => self.read(&params).await,
            "forget" => self.forget(&params).await,
            other => Err(AgentToolError::from(format!("unknown action `{other}`"))),
        }
    }
}

impl MemoryTool {
    async fn save(&self, params: &Value) -> Result<AgentToolResult, AgentToolError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `name`"))?;
        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `description`"))?;
        let body = params
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `content`"))?;
        let kind = params
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("user");

        let slug = slugify(name);
        if slug.is_empty() {
            return Err(AgentToolError::from("name slugifies to empty string"));
        }
        let path = self.dir.join(format!("{slug}.md"));

        let frontmatter = format!(
            "---\nname: {slug}\ndescription: {description}\nmetadata:\n  type: {kind}\n---\n\n"
        );
        let payload = format!("{frontmatter}{body}\n");
        tokio::fs::write(&path, payload.as_bytes())
            .await
            .map_err(|e| AgentToolError::from(format!("write memory: {e}")))?;
        update_index(&self.dir, &slug, description).await?;

        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!(
                "Saved memory `{slug}` ({})",
                path.display()
            ))],
            details: json!({ "name": slug, "path": path.display().to_string() }),
            terminate: None,
        })
    }

    async fn list(&self) -> Result<AgentToolResult, AgentToolError> {
        let mut rd = match tokio::fs::read_dir(&self.dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AgentToolResult {
                    content: vec![UserContentBlock::text("[no memories]")],
                    details: json!({ "memories": [] }),
                    terminate: None,
                });
            }
            Err(e) => return Err(AgentToolError::from(format!("list memories: {e}"))),
        };
        let mut names = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| AgentToolError::from(format!("read: {e}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") && name != "MEMORY.md" {
                names.push(name.trim_end_matches(".md").to_string());
            }
        }
        names.sort();
        let text = if names.is_empty() {
            "[no memories]".to_string()
        } else {
            let mut s = String::from("Memories:\n");
            for n in &names {
                s.push_str(&format!("  {n}\n"));
            }
            s
        };
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({ "memories": names }),
            terminate: None,
        })
    }

    async fn read(&self, params: &Value) -> Result<AgentToolResult, AgentToolError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `name`"))?;
        let path = self.dir.join(format!("{}.md", slugify(name)));
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| AgentToolError::from(format!("read memory: {e}")))?;
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(body)],
            details: json!({ "path": path.display().to_string() }),
            terminate: None,
        })
    }

    async fn forget(&self, params: &Value) -> Result<AgentToolResult, AgentToolError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `name`"))?;
        let slug = slugify(name);
        let path = self.dir.join(format!("{slug}.md"));
        let _ = tokio::fs::remove_file(&path).await;
        remove_index_entry(&self.dir, &slug).await?;
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!("Forgot memory `{slug}`."))],
            details: json!({ "name": slug }),
            terminate: None,
        })
    }
}

async fn update_index(
    dir: &std::path::Path,
    slug: &str,
    description: &str,
) -> Result<(), AgentToolError> {
    let index_path = dir.join("MEMORY.md");
    let existing = tokio::fs::read_to_string(&index_path)
        .await
        .unwrap_or_default();
    let line = format!("- [{slug}]({slug}.md) — {description}\n");
    let mut out = String::with_capacity(existing.len() + line.len());
    let mut replaced = false;
    for l in existing.lines() {
        if l.starts_with(&format!("- [{slug}](")) {
            out.push_str(&line);
            replaced = true;
        } else {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&line);
    }
    tokio::fs::write(&index_path, out.as_bytes())
        .await
        .map_err(|e| AgentToolError::from(format!("write index: {e}")))?;
    Ok(())
}

async fn remove_index_entry(dir: &std::path::Path, slug: &str) -> Result<(), AgentToolError> {
    let index_path = dir.join("MEMORY.md");
    let Ok(existing) = tokio::fs::read_to_string(&index_path).await else {
        return Ok(());
    };
    let prefix = format!("- [{slug}](");
    let out: String = existing
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(|l| format!("{l}\n"))
        .collect();
    tokio::fs::write(&index_path, out.as_bytes())
        .await
        .map_err(|e| AgentToolError::from(format!("rewrite index: {e}")))?;
    Ok(())
}

/// Load existing memory into a text block suitable for the system prompt. Returns an empty
/// string when no memory exists. Walks `<dir>/*.md` (excluding `MEMORY.md`, which is the
/// index file injected separately by the harness, not a memory entry) and concatenates.
pub async fn load_memory_block(dir: &std::path::Path) -> String {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return String::new();
    };
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().into_owned();
        // Mirror the exclusion already applied by `action=list` (see the filter near the
        // top of this file). Without this skip, the index file gets folded into the
        // injected memory block in addition to being loaded separately, surfacing the same
        // content twice in the system prompt. Code-review item #10 (2026-05-22).
        if name.ends_with(".md") && name != "MEMORY.md" {
            entries.push((name, e.path()));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block = String::new();
    let mut count = 0usize;
    for (name, path) in &entries {
        let Ok(body) = tokio::fs::read_to_string(path).await else {
            continue;
        };
        if count == 0 {
            block.push_str("<memory>\n");
            block.push_str(
                "Persistent cross-session memory. These notes were saved in prior conversations and may be helpful. ",
            );
            block.push_str(
                "Use the `memory` tool with action=save to add more, action=forget to remove.\n\n",
            );
        }
        block.push_str(&format!("--- {name} ---\n"));
        block.push_str(body.trim());
        block.push_str("\n\n");
        count += 1;
    }
    if count > 0 {
        block.push_str("</memory>");
    }
    block
}

use once_cell::sync::Lazy;
static DEFINITION: Lazy<Tool> = Lazy::new(|| {
    Tool {
    name: "memory".into(),
    description:
        "Persistent cross-session memory. action=save (requires name/description/content/optional type), action=list, action=read (requires name), action=forget (requires name). Saved entries are auto-injected into the system prompt of future sessions."
            .into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["save", "list", "read", "forget"],
                "description": "Operation to perform.",
            },
            "name": { "type": "string", "description": "Short kebab-case slug (required for save/read/forget)." },
            "description": { "type": "string", "description": "One-line summary (save only)." },
            "type": { "type": "string", "description": "Memory category (e.g. user/feedback/project/reference). Default: user." },
            "content": { "type": "string", "description": "Body of the memory (save only)." },
        },
        "required": ["action"],
    }),
}
});
