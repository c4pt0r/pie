# pie-coding-agent (Rust)

Minimal coding agent CLI built on top of [`pie-agent-core`](../agent) and
[`pie-ai`](../ai). Modeled on the TS implementation in
`packages/coding-agent/` of the upstream `pi` monorepo, but trimmed to the essentials.

## What's in scope

| | |
|---|---|
| Tools | `read`, `write`, `bash`, `ls`, `memory` (5 total) |
| TUI | Line-based REPL, streaming output, ANSI colors via `crossterm` |
| Sessions | Append-only jsonl under `~/.pie/sessions/<cwd-hash>/` |
| Resume | `--resume` (last session) or `--resume-id <prefix>` |
| Memory | Cross-session, file-backed at `~/.pie/memory/`; auto-loaded into system prompt |
| Models | Auto-detected from `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `OPENROUTER_API_KEY` / Groq / Mistral / Gemini |

## What's deliberately out

Extensions, skills loader, themes, print/json/rpc modes, full-screen TUI widgets (no
ratatui), tool-confirmation prompts, `edit` (with diff), grep/find. All of those exist in the
TS reference and could be added on top of this skeleton.

## Run

```bash
export ANTHROPIC_API_KEY=sk-ant-…           # or any of the supported providers
cargo run

# Resume the most recent session in this cwd
cargo run -- --resume

# Resume a specific session (full UUIDv7 or a unique prefix)
cargo run -- --resume-id 019e

# List sessions for this cwd
cargo run -- --list-sessions

# Delete a session
cargo run -- --delete-session 019e

# Override model
cargo run -- --provider anthropic --model claude-haiku-4-5

# Turn on reasoning for supported models
cargo run -- --thinking high
```

REPL commands inside the loop: `/help`, `/clear`, `/quit` (or `/q`).

## Layout

```
src/
  main.rs          CLI parsing + REPL loop
  config.rs        ~/.pie paths, cwd hashing
  model.rs         env → provider/model detection
  session/mod.rs   create/resume/list/delete via JsonlSessionRepo
  tui.rs           AgentEvent renderer (colors + streaming)
  tools/
    mod.rs         default_tools()
    read.rs        ReadTool (offset/limit, truncation)
    write.rs       WriteTool (parent dir create)
    bash.rs        BashTool (sh -c, timeout, tail-truncate)
    ls.rs          LsTool
    memory.rs      MemoryTool (save/list/read/forget + system-prompt block loader)
    truncate.rs    head/tail truncation primitives
tests/
  tools.rs         end-to-end tool tests against tempdirs
```

## Memory model

When you tell the agent "remember that I prefer X", it can call:

```
memory(action="save", name="prefers-x", description="user preference",
       content="The user prefers X over Y.", type="user")
```

This writes `~/.pie/memory/prefers-x.md` (with YAML frontmatter) and updates the
index at `~/.pie/memory/MEMORY.md`. On every new session, all `*.md` files under
that directory are concatenated into a `<memory>` block in the system prompt — so the agent
sees them without explicit recall.

## Tests

```
cargo test     # 3 unit + 8 integration tests against the tool surface
```
