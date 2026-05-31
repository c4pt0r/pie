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

### Tier 3 — safety / sandboxing

- ~~#8 — Sandboxing: scoped file writes, network egress policy, `.pieignore`~~ — **de-scoped, closed**

### Tier 4 — framework depth (skills + harness elevated to top)

- [ ] **#17 — AgentHarness expansion: first-class skills, templates, hooks, branch summarization** (priority)
- [ ] **#10 — Skills loader (first-class) + extension/plugin system** (priority — Part A first)
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

### Tier 8 — cross-agent connectivity (RFC)

- [x] **#18 — `pie.0xfefe.me` public MCP hub** — Worker/API v0 live (RFC + Worker MVP + deploy workflow merged; gate 6 live e2e passed 2026-05-30). See [[18-rfc-fefe-mcp-hub]].
- [x] **#19 — pie client onboard for the fefe hub** — happy path, auth wire contract, Skip semantics, acceptance gates. Phases 1-6 merged; two-clean-client live TUI e2e on `pie.0xfefe.me` passed 2026-05-31 covering `/hub join → status → send → first-contact card → Accept once → bounded feed`, plus Always / Block / Skip decision matrix; redaction sweep passed. See [[19-fefe-client-onboard]].

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
