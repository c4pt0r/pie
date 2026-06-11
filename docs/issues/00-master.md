# Roadmap: bring `pie` up to mature agent-framework parity

This is the master tracking issue for closing the gap between the current `pie` (Rust port of
`pi`) and what a daily-driver coding agent (Claude Code / Aider / Cursor's agent / OpenHands /
the upstream TS `pi`) gives users.

## Working principles

Every sub-issue below MUST address the same five axes in its design write-up before code lands:

1. **Architecture** — where does this slot into the layered stack (`pie-ai` ↔ `pie-agent-core` ↔
   `pie-coding-agent`)? What's the public API surface?
2. **Stability** — what's the failure mode? Retries, fallbacks, abort behaviour, lock scopes
   across `.await`?
3. **Extensibility** — what knobs do downstream consumers / future tiers actually need?
   Resist over-configuration: defaults > knobs.
4. **Performance** — what's the worst-case cost (latency, memory, file IO, token spend)?
   Caching / streaming / parallel where it actually buys us something.
5. **Testing** — unit tests for pure logic, integration tests for sub-system seams, **end-to-end
   tests for every user-visible behavior**. Faux providers + tempdirs only — never hit the
   network in CI.

## Explicit de-scoping (2026-05-20)

- **No Windows support.** Linux + macOS only.
- **No sandboxing.** Permission system (#4) is the primary safety layer; the original
  scoped-writes / `.pieignore` / network-egress sub-issue is closed (was #8).

## Scope buckets

### Tier 1 — daily-UX (high friction without it)

- [ ] #2 — TUI overhaul: input box, multi-line, history, mid-stream abort, spinner, streaming markdown render
- [ ] #3 — Slash-command system + completion (`/model`, `/thinking`, `/compact`, `/diff`, `/undo`, `/diag`, `/save`, `/share`, `/help <cmd>`)
- [ ] #4 — Approval & permission system: diff preview, dangerous-bash detection, per-session allow-list
- [ ] #5 — `@file` mentions + interactive session picker (fuzzy)

### Tier 2 — session / state

- [ ] #6 — `--continue` (global most-recent), named sessions, `/share` export to HTML/Gist
- [ ] #7 — Token & cost tracking, per-session budget cap, fallback model on provider failure
- [ ] **#20 — Session export / import for replayable agent backups**. See
  [[20-session-export-import]].

### Tier 3 — safety / sandboxing

- ~~#8 — Sandboxing: scoped file writes, network egress policy, `.pieignore`~~ — **de-scoped, closed**

### Tier 4 — framework depth (skills + harness elevated to top)

- [ ] **#17 — AgentHarness expansion: first-class skills, templates, hooks, branch summarization** (priority)
- [ ] **#10 — Skills loader (first-class) + extension/plugin system** (priority — Part A first)
- [x] **#21 — `SkillBuilder` builtin tool: author user skills from structured fields**. See
  [[21-skill-builder-tool]].
- [x] **#22 — Web relay: mount a live session at pie.0xfefe.me/session/&lt;token&gt;**. See
  [[22-web-relay]].
- [ ] #9 — MCP (Model Context Protocol) client
- [ ] #11 — Subagent / Task delegation
- [ ] #12 — Built-in capability tools: web fetch, web search, git, compile-error feedback loop

### Tier 5 — auth / cloud

- [ ] #13 — `/login`, persistent `~/.pie/auth.json`, OAuth for Codex / Copilot / Google
- [ ] #14 — Bedrock SigV4 + Vertex ADC

### Tier 6 — observability

- [ ] #15 — Structured logging, `/diag`, latency/cost display, error-report bundling

### Tier 7 — multimodal

- [ ] #16 — Multi-modal IO (image input, terminal image rendering)

### Tier 8 — cross-agent connectivity

- ~~#18 — public MCP cross-agent service~~ — **de-scoped and removed from the shipped
  product surface on 2026-06-10**. The RFC is retained only as historical design context;
  it is no longer an implementation or release target.
- ~~#19 — client onboarding for the public cross-agent service~~ — **de-scoped and removed
  from the shipped product surface on 2026-06-10**. The client command surface was removed;
  generic MCP transport and generic trigger primitives remain in their own tiers.

## Non-goals (explicit)

- No Windows support.
- No filesystem / network sandboxing — permission system (#4) carries the safety burden.
- No hard cap on tool-call iterations.
- No rewrite of the agent loop architecture unless a concrete bug requires it.
- No "configure every string" — defaults stay centralized.
- Provider/model catalog regeneration is out of scope here.

## Order of attack (recommended)

Per maintainer direction (2026-05-20), **skills + harness work is top priority**, paired with
the daily-UX items:

1. **Skills loader (Part A of #10) inside `AgentHarness`** — fastest "we're a real framework"
   delta with the least UI work.
2. **AgentHarness expansion (#17)** — first-class hooks/templates/branching; supports skills.
3. **Mid-stream abort + spinner** — small UX change with outsized perceived improvement (#2).
4. **TUI overhaul** (#2).
5. **Approval & permission system** (#4).
6. **Slash-command system** (#3).
7. **MCP client** (#9).
8. **Token & cost tracking** (#7).

After that, parallelize the remaining clusters by area owner.

## How to use this issue

- Each sub-issue is created with the same skeleton (Goal / Architecture / Stability /
  Extensibility / Performance / Testing / Acceptance / Out-of-scope).
- This master issue's checklist is the source of truth for done/in-progress/blocked status.
- Cross-cutting changes link both sub-issues.
