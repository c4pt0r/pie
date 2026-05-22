# Changelog

All notable changes to this project. Format loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions sync across all workspace crates per the lockstep policy in `AGENTS.md`.

## [Unreleased]

### Added — Tier 1 (daily UX)

- **#2** Mid-stream Ctrl-C abort with double-Ctrl-C exit. Biased select against stalled
  streams (closes #18).
- **#3** Slash-command registry with 20 builtins: `/help`, `/clear`, `/skills`, `/quit`
  (+ `/exit`, `/q`), `/model`, `/thinking`, `/cost`, `/diag`, `/template`, `/save`,
  `/compact`, `/undo`, `/bug-report`, `/name`, `/sessions`, `/share`, `/login`, `/logout`,
  `/find`.
- **#4** Dangerous-bash detector wired through `before_tool_call`. 11-pattern corpus
  (`rm -rf /`, `sudo`, `curl|sh`, etc.) returns deny reason as the synthesized tool result.
- **#5** `@file` mention injection. Files are read, capped at 64 KiB, prepended to the
  prompt as `<file path="...">…</file>` blocks.

### Added — Tier 2 (session/state)

- **#6** `pie --continue` / `-c`, `pie --list-all-sessions`, `/save` (Markdown transcript
  export), `/name <slug>`, `/sessions`, `/share` (Gist upload via `gh`), `/find <query>`
  (cross-session text search).
- **#7** `CostTracker` on `AgentHarness`, `/cost` + `/cost reset` slash commands,
  `budget_cap_usd` pre-turn gate, `fallback_model` after retry-exhaustion.

### Added — Tier 4 (framework depth)

- **#9** `pie-mcp` crate: stdio transport, JSON-RPC 2.0 framing, initialize handshake,
  `tools/list` + `tools/call`. `McpAgentTool` adapter wraps server tools as `AgentTool`s.
  `~/.pie/mcp.toml` loader spawns each server lazily.
- **#10 Part A** Dual-root skills loader (`<cwd>/.pie/skills/` overrides `~/.pie/skills/`),
  wired into `AgentHarnessOptions::skills`.
- **#11** `task` subagent tool: spawns a fresh `AgentHarness` (in-memory session, read-only
  tool subset), parent abort cascades to subagent within 2s.
- **#12** Built-in tools: `web_fetch` (HTML→text), `web_search` (Brave Search), structured
  `git` (status/diff/log), LSP supervisor + `after_tool_call` hook that attaches diagnostics
  to write/edit tool results.

### Added — Tier 5 (auth/cloud)

- **#13** `auth.json` credential store with atomic write + mode 0600. `/login` and
  `/logout` slash commands. Model resolver consults the store as env-var fallback. OAuth
  2.0 PKCE primitives (`Flow::authorize_url`, `await_callback`, `exchange_code`,
  `refresh_token`).
- **#14** Hand-rolled AWS SigV4 signer (no aws-sdk dep). Bedrock `invoke()` for the
  non-streaming `/model/{id}/invoke` path. Vertex AI `invoke()` with bearer or API-key
  auth.

### Added — Tier 6 (observability)

- **#15** Tracing subscriber writing per-session logs to `~/.pie/logs/<session>.log` via
  non-blocking `tracing-appender`. `/diag` snapshot command. `/bug-report` with secret
  redaction (OpenAI/Anthropic keys, AWS access keys, GitHub PATs, Slack tokens, Google API
  keys, Bearer tokens). OTLP HTTP/JSON span exporter activated by
  `OTEL_EXPORTER_OTLP_ENDPOINT`.

### Added — Tier 7 (multimodal)

- **#16** `--image <path>` CLI flag (repeatable, PNG/JPEG/WebP/GIF, 10 MiB per image, 10
  per message). Magic-byte mime detection.

### Added — Framework

- **#17** `HarnessEvent` typed bus on `AgentHarness` (SessionStart / Compaction /
  Branch). Prompt-template file loader (`<cwd>/.pie/templates/` overrides
  `~/.pie/templates/`) + `/template <name> [k=v ...]` slash command.
  `AgentHarness::after_tool_call` hook slot, paired with the existing `before_tool_call`.

### Fixed

- **#18** Biased select against stream stalls so Ctrl-C unblocks the in-flight prompt
  within 500ms regardless of LLM stream state.
- **#25 (PR A)** Register the `Skill` builtin tool the system prompt already advertises.
  Before this fix the model would call `Skill { name: "..." }` and receive
  `no tool named 'Skill'` because the tool was never wired into `default_tools`. On hit
  the tool returns the skill body wrapped in a `<skill name="...">` block; on miss it
  surfaces a typed error pointing the model at `/skills`. `disable_model_invocation=true`
  is now enforced uniformly across all call paths (was previously a no-op flag).

### Explicitly de-scoped

- Windows support (Linux + macOS only).
- Filesystem / network sandboxing (was #8). The permission system (#4) is the safety
  layer; per-session always-allow + interactive prompt mode remain follow-up work.

### Pending follow-ups (documented; not in this release)

- #2 ratatui-style sticky-input TUI with streaming markdown render + history + Ctrl-R.
- #10 Part B WASM extension host (foundation: `skills` loader + slash-command registry are
  the public extension surface in v1).
- #13 Provider-specific OAuth endpoint URLs for Anthropic Pro/Max, Codex, Copilot, Google
  (the generic `Flow` plumbing is in; consumers supply their own URLs).
- #14 Bedrock streaming (`/invoke-with-response-stream` event-stream binary framing) +
  full ADC chain (service-account JSON → JWT → token exchange) for Vertex.
- #12 Per-tool LSP language config richer than per-extension; multi-server collaboration.

## Workspace test coverage

27 test binaries, ~225 tests, `clippy -D warnings` clean across `pie-ai`,
`pie-agent-core`, `pie-coding-agent`, `pie-mcp`.
