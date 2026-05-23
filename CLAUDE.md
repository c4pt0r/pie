# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace layout

Rust 2024 Cargo workspace. Four crates; no non-crate workspace members.

- `crates/ai` (`pie-ai`) — unified streaming LLM client. Provider implementations live under `src/providers/`; model + image catalogs are generated (`models.generated.json`, `models_generated.rs`, `image_models_generated.rs` — regenerate via `crates/ai/scripts/regen_models.sh`). Anthropic/OAuth, AWS SigV4, Vertex ADC, SSE/event-stream helpers, and retry/overflow utilities are here. Public surface is re-exported through `src/lib.rs`.
- `crates/agent` (`pie-agent-core`) — agent runtime layered on `pie-ai`. The bare `Agent` in `src/agent.rs` is IO-free and owns conversation state, listeners, queues, and the cancellation token. The agent loop is `src/agent_loop.rs`. The `harness` module (feature `harness`) assembles opinionated extras: session storage (JSONL + in-memory), cost tracker, compaction, permission policy, prompt templates, skills loading, system prompt, and the trigger runtime / notification-hook abstraction. Filesystem IO is in the harness, not the bare agent.
- `crates/coding-agent` (`pie-coding-agent`) — the `pie` CLI binary, REPL/TUI, slash-command registry, hooks, LSP supervisor, tool implementations under `src/tools/`, and trigger source adapters under `src/triggers/`. Entry point is `src/main.rs`.
- `crates/mcp` (`pie-mcp`) — minimal MCP client: stdio transport, JSON-RPC framing, `tools/list` / `tools/call`. The coding-agent wraps server tools as `AgentTool`s via `tools/mcp_adapter.rs`.
- `examples/` — runnable demos, not workspace members. `examples/mcp-notify-python/` is a stdlib-only Python MCP server that pushes a synthetic heartbeat into pie's trigger runtime; `examples/mcp-weather-python/` is the same shape but polls a live weather API and pushes a one-line summary for the harness to act on. See each README for wiring.

Layering goes one direction: `coding-agent` → `agent` (`harness` feature on) → `ai`; `mcp` is consumed by `coding-agent`. Keep the bare `Agent` IO-free — anything touching the filesystem, env, or network adapters belongs in `harness/` or in `coding-agent`.

## Common commands

Prefer the `Makefile` (it mirrors `.github/workflows/ci.yml`):

- `make build` / `make release` — workspace build (dev / release).
- `make test` — `cargo test --workspace` (all crates including the example).
- `make test-coding-agent` / `make test-agent` / `make test-ai` / `make test-mcp` — single-crate tests.
- `make lint` — `cargo clippy --workspace --all-targets -- -D warnings`. The lint policy is set workspace-wide in the root `Cargo.toml` (`[workspace.lints.clippy]`); several pedantic lints are downgraded there.
- `make fmt` / `make fmt-check` — rustfmt rewrite / check-only.
- `make ci` — full local pipeline (fmt-check + lint + test).
- `make run` — `cargo run -p pie-coding-agent` (interactive REPL).
- `make install` — install `pie` into `~/.cargo/bin`.

Run a single test: `cargo test -p <crate> <test_name>` (e.g. `cargo test -p pie-agent-core compaction::tests::cuts_at_turn_boundary`). Integration tests live in each crate's `tests/`; unit tests sit next to the code.

`cargo build` / `cargo test` without `--workspace` build only `default-members` (no examples) — keep that flow fast. Use `--workspace` explicitly when changes affect the example crate.

## Testing discipline

Tests must not hit real provider APIs. CI clears `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `MISTRAL_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `AWS_BEARER_TOKEN_BEDROCK`, `GOOGLE_VERTEX_ACCESS_TOKEN`, `CODEX_AUTH_TOKEN`, `AZURE_OPENAI_API_KEY` before running. A test that requires a live call must be explicitly gated.

CI does **not** set `RUSTFLAGS=-D warnings` (it would break `cargo test` on transitive crates). Deny-warn applies only to first-party clippy via the explicit `-- -D warnings` arg.

## Runtime layout

`pie` writes state under `~/.pie/` by default; set `PIE_DIR` to redirect. Sessions are JSONL files under `~/.pie/sessions/<cwd-hash>/<uuidv7>.jsonl`. Memory (`~/.pie/memory/*.md`), auth (`~/.pie/auth.json`, 0600), custom model defs (`~/.pie/models.json`, also `<cwd>/.pie/models.json` with project precedence), hooks (`~/.pie/hooks.toml`, plus optional project-local hooks gated on `allow_project_hooks`), and config (`~/.pie/config.toml`, e.g. `[triggers] poll_interval_secs`) all live there.

## Key conventions

- Module / file / function / test names: `snake_case`. Public types and traits: `PascalCase`.
- Provider-specific code goes under `crates/ai/src/providers/`; CLI tools go under `crates/coding-agent/src/tools/`. Don't mix layers.
- Workspace-pinned deps live in `[workspace.dependencies]` at the root. Prefer `dep.workspace = true` over re-declaring versions per crate.
- Versions are kept in lockstep across all workspace crates (see `AGENTS.md`).
- Generated files in `crates/ai/src/` (model and image catalogs) are not hand-edited — regenerate via the script in `crates/ai/scripts/`.

## Triggers

Triggers are a first-class runtime concept, not a feature flag. The runtime lives in `crates/agent/src/harness/trigger_runtime.rs` and `trigger.rs`; source adapters (MCP push notifications, dynamic file/command polling) live in `crates/coding-agent/src/triggers/`. Rules are session-scoped (stored next to the active session JSONL), fire-once by default, and execute in a sub-agent that inherits the parent harness config but starts with a fresh context. Trigger output is only promoted into the main chat when the rule has `promote_to_chat=true`. The `/triggers` slash command surfaces runtime state.

An accepted trigger's delivery is chosen by the `before_trigger_action` hook, which returns a `TriggerAction` whose `delivery: TriggerDelivery` is one of: `SubAgent` (default — run the sub-agent in a fresh context); `InjectSummary` (skip the sub-agent, inject `trigger.payload_summary` straight into the parent chat via the promotion path, no model call); or `InjectAndRun` (inject `action.prompt` into the parent chat AND run one model turn in the parent's full context). For `InjectAndRun` the runtime never drives the single-tenant parent agent itself: when the parent is streaming it enqueues a follow-up; when idle it emits `HarnessEvent::TriggerRequestsMainRun`, which the coding-agent REPL funnels into a single serialized run channel (shared with user input) so triggered turns and user prompts never race. MCP servers marked `inject_summary` / `inject_and_run` in `mcp.toml` are treated as notification feeds and take those paths (see `triggers::direct_inject_action_hook`). The runtime stays domain-agnostic across all modes — it only moves the opaque `payload_summary` / prompt strings.
