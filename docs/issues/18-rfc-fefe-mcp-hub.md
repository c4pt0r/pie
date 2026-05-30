# RFC: pie.0xfefe.me public MCP hub

> Parent: [[00-master]] roadmap.
> Tier: 8 (cross-agent connectivity). Extends [[08-mcp-client]] and [[17-harness-expansion]].
> Status: **Implementation in progress.** §1, §2, §3 v0.2, §4 v0.2, §5, §6a, §6b, §8 v0.1 are on `main`. §7 owner = @Tools-MCP-Lead (RFC-OQ-1 resolved 2026-05-30). Worker MVP and live e2e are the current critical path; see RFC #18 §8 release gates and the change-log entry for the deploy workflow PR.
> Coordinator: @alice
>
> Chapter authors:
> - §1 Architecture overview — @Tools-MCP-Lead + @Runtime-dev-lead (co-author)
> - §2 Hub MCP protocol surface — @Tools-MCP-Lead
> - §3 Identity / Auth / Session / Namespace / Agent registry — @Provider-Auth-Lead
> - §4 Visibility model — @alice (seed draft below)
> - §5 Notification routing / delivery semantics — @Runtime-dev-lead
> - §6a Client integration (contract + runtime boundary) — @Tools-MCP-Lead
> - §6b `/hub *` CLI / TUI surface — @CLI-TUI-Dev-Lead
> - §7 Worker implementation + storage model — @Tools-MCP-Lead (assigned 2026-05-30 by @EdHuang; RFC-OQ-1 resolved)
> - §8 Deployment / `CF_API_KEY` boundary / CI / acceptance / release gate — @QA-Release-Lead

## Goal

`pie.0xfefe.me` is a publicly-reachable Cloudflare Worker that exposes an MCP (Model Context Protocol) service. Pie agents — and any other MCP client — connect to it to discover and notify other agents under a per-user namespace.

Ship:

- Public MCP service exposing `register_agent`, `list_agents`, `discover_public_agents`, `send_notification`, and server-pushed notifications to connected agents.
- Username + password registration establishes a human namespace; each registered agent gets a globally-unique UUID and a namespace-scoped readable handle.
- Visibility model that decouples discovery from inbox writeability.
- First-contact gate that reuses the issue #110 `ControlPlaneWrite` user-prompt mechanism — no new prompt protocol.
- Pie client integration via a new `HttpMcpTransport` in `crates/mcp/` (parallel deliverable, not bound to this hub).

## Definition of done

**Per @EdHuang (2026-05-29): completion is gated on e2e success against the real deployed `pie.0xfefe.me`, not on RFC approval, merged PRs, or faux-fixture tests alone.**

Concretely, the RFC is "done" only when:

1. The Cloudflare Worker is deployed to the real `pie.0xfefe.me` domain by the protected GitHub Actions deploy workflow using repository secret `CF_API_KEY`.
2. Two real pie agents on different machines (or different namespaces) can register, discover each other, send and receive notifications, exercise the first-contact gate end-to-end against the deployed Worker.
3. The acceptance matrix in §8 has been run against the deployed Worker — not just faux fixtures.

This does NOT change the "no real Cloudflare in build/test CI" rule (§8). Build/test CI uses faux Worker / `wrangler dev` / Miniflare. Deploy is a **separate, environment-protected CI lane** (GitHub Actions deploy job, `production` environment with branch policy = `main`, manual `workflow_dispatch`; per @EdHuang 2026-05-30 no per-run human approval is required — any authorized team member can trigger) — see §8 gate 5. The deployed-Worker e2e is gate 6.

**Status terminology (per @QA-Release-Lead 2026-05-29).** Two distinct states to avoid future misjudgement:

- **pre-deploy complete** — CI / faux / local Worker green; implementation mergeable to a controlled branch or marked `experimental`. **Not** "feature complete."
- **release complete / done** — real `pie.0xfefe.me` deployed + scripted manual e2e (per §8 acceptance matrix) recorded as passing. Only this state permits closing the parent task in the master roadmap.

**Critical-path dependency.** First-contact gate (§4.3) implementation depends on issue #110 (`ControlPlaneWrite` user-Prompt category) landing. Per @Runtime-dev-lead (2026-05-29), #110 is promoted to P0 alongside §5 implementation.

## Non-goals

- **Not a provider credential plane.** `pie.0xfefe.me` does not store, proxy, or know about OpenAI / Anthropic / Deepseek / Bedrock / Vertex credentials. Those continue to live in `~/.pie/auth.json` on the user's machine.
- **Not a Slack / IRC replacement.** No channels, threads, search, or long-lived chat history beyond what notification idempotency and audit require.
- **No new runtime hook traits.** Inbound notifications flow through existing `McpNotificationHook` → `Trigger` envelope → `register_notification_hook` pipeline. The hub is just another MCP server from the runtime's point of view.
- **No Windows support.** macOS + Linux only (matches the master roadmap de-scoping).

## Architecture (overview — §1 expands)

```
                                  ┌─────────────────────────────┐
                                  │   pie.0xfefe.me (CF Worker) │
                                  │   ┌───────────────────────┐ │
                                  │   │ public MCP service    │ │
                                  │   │  • register_agent     │ │
                                  │   │  • list_agents        │ │
                                  │   │  • send_notification  │ │
                                  │   │  • SSE push channel   │ │
                                  │   └───────────────────────┘ │
                                  │   ┌───────────────────────┐ │
                                  │   │ admin website         │ │
                                  │   │  • account / login    │ │
                                  │   │  • agent registry     │ │
                                  │   │  • token rotate       │ │
                                  │   └───────────────────────┘ │
                                  │   storage: D1 / KV / DO     │
                                  └─────────────┬───────────────┘
                                                │ MCP over HTTP (POST + SSE)
                  ┌─────────────────────────────┼─────────────────────────────┐
                  │                             │                             │
        ┌─────────▼────────┐         ┌──────────▼─────────┐         ┌─────────▼────────┐
        │ pie agent A      │         │ pie agent B        │         │ external agent   │
        │ HttpMcpTransport │         │ HttpMcpTransport   │         │ (any MCP client) │
        └──────────────────┘         └────────────────────┘         └──────────────────┘
```

### Chapter map

| §   | Title                                                          | Owner                                       | Status              |
| --- | -------------------------------------------------------------- | ------------------------------------------- | ------------------- |
| 1   | Architecture overview                                          | @Tools-MCP-Lead + @Runtime-dev-lead         | **v0.1**            |
| 2   | Hub MCP protocol surface                                       | @Tools-MCP-Lead                             | **v0.1**            |
| 3   | Identity / Auth / Session / Namespace / Agent registry         | @Provider-Auth-Lead                         | **v0.2**            |
| 4   | Visibility model                                               | @alice                                      | **v0.2**            |
| 5   | Notification routing / delivery semantics                      | @Runtime-dev-lead                           | **v0.1**            |
| 6a  | Client integration (contract + runtime boundary)               | @Tools-MCP-Lead                             | **v0.1**            |
| 6b  | `/hub *` CLI / TUI surface                                     | @CLI-TUI-Dev-Lead                           | **v0.1**            |
| 7   | Worker implementation + storage                                | @Tools-MCP-Lead                             | in progress         |
| 8   | Deployment / `CF_API_KEY` / CI / acceptance / release gate     | @QA-Release-Lead                            | **v0.1**            |

---

## §1 Architecture overview — v0.1 (@Tools-MCP-Lead + @Runtime-dev-lead)

> Status: **v0.1 draft.** Stitches §2 (MCP surface) + §5 (notification envelope)
> + §4 (visibility) + §3 (identity/auth) into one mental model. Implementation
> chapters are §6a (client engine contract), §6b (CLI/TUI surface), §7 (Worker),
> §8 (release gate). Where this chapter touches the same field as a downstream
> chapter, this chapter **cites**, never redefines.

### §1.1 What `pie.0xfefe.me` is

`pie.0xfefe.me` is a **public MCP server** that lets pie agents on different
machines send each other notifications. Hub state lives in a Cloudflare Worker
(`crates/agent`-external; see §7). Every API on the hub — register an agent,
look one up, send a notification, manage the trust list — is an MCP **tool
call**. Real-time delivery of incoming notifications is an MCP **server-push
notification** on the SSE side of the Streamable HTTP transport
(MCP spec 2025-03-26).

Why a public MCP server, not a custom protocol:

- pie agents are already MCP clients. They load MCP servers from `mcp.toml`
  (`mcp_loader.rs`, PR #63), maintain inflight + cancel semantics
  (`McpClient`, PR #74), and turn server-push notifications into runtime
  `Trigger` envelopes (`McpNotificationHook`, PR #56). Reusing MCP means the
  hub absorbs into stacks that already handle stdio MCP servers; only the
  transport is new (§6a `HttpMcpTransport`).
- "Define a tool / read a resource / push a notification" is exactly what the
  hub needs to expose. Reinventing the framing inside a websocket dialect
  would duplicate authentication, schema discipline, and cancellation that
  MCP already standardizes.
- LLMs already know how to call MCP tools. No second protocol for the model
  to learn; tool descriptions in §2 are the LLM-facing API.

§2 (the hub-facing MCP surface) and §5 (the runtime envelope after the client
reads the wire) are **two views of the same wire bytes**. §2 owns the on-wire
shape and method names; §5 owns the in-process `Trigger`. They were drafted in
parallel under a "same wire bytes, cite don't redefine" protocol and merged as
a pair on 2026-05-29.

### §1.2 Component map

```
                    ┌──────────────────────────────────────────────┐
                    │  pie.0xfefe.me (Cloudflare Worker, §7)       │
                    │  ┌─────────────────────────────────────────┐ │
                    │  │ MCP tool surface (§2 v0.1)              │ │
                    │  │  register/profile/discover/send/inbox/  │ │
                    │  │  ack/list_trust/revoke/block/...        │ │
                    │  └─────────────────────────────────────────┘ │
                    │  ┌─────────────────────────────────────────┐ │
                    │  │ Identity / auth / namespace (§3)        │ │
                    │  │  agent_id (UUID), agent-token, perms    │ │
                    │  │  human session for control plane        │ │
                    │  └─────────────────────────────────────────┘ │
                    │  ┌─────────────────────────────────────────┐ │
                    │  │ Visibility / inbox / trust (§4)         │ │
                    │  │  discoverable × inbox matrix            │ │
                    │  └─────────────────────────────────────────┘ │
                    └──────┬───────────────────────────────────┬───┘
                           │ HTTP POST (tools)                 │ SSE push (notifications)
                           │ MCP Streamable HTTP transport (§6a `HttpMcpTransport`)
                           ▼                                   ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │  pie client (this repository)                                       │
   │                                                                     │
   │  ~/.pie/mcp.toml ──► mcp_loader.rs (PR #63) ──► McpClient (PR #35)  │
   │                                                          │          │
   │                                                          ▼          │
   │              McpNotificationHook (PR #56, configured as              │
   │              `make_pie_hub_notification_hook`, §5.1)                 │
   │                                                          │          │
   │                                                          ▼          │
   │                                              Trigger envelope (§5.4) │
   │                                                          │          │
   │                                                          ▼          │
   │           register_notification_hook supervisor (RFC 1 sub-PR 3)     │
   │                                                          │          │
   │                                                          ▼          │
   │              BeforeTriggerHook (RFC 1 sub-PR 4)                      │
   │              ─ first-contact gate via issue #110                     │
   │                Prompt channel (§5.6, ~/.pie/hub-trust.json §5.7)     │
   │                                                          │          │
   │                                                          ▼          │
   │                           handle_trigger (RFC 1 sub-PR 2)            │
   │                                  │                                   │
   │                                  ▼                                   │
   │                       sub-agent fork (RFC 1 sub-PR 5a) → main loop  │
   │                       + Custom audit (fefe_trust_decision §5.7,      │
   │                         trigger_prompt #110 Artifact E)              │
   └─────────────────────────────────────────────────────────────────────┘

   CLI / TUI surface (§6b)               Release gate (§8)
     /hub login | register | rotate        CI deploy via GitHub Actions
     /hub trust | block | list             repo secret CF_API_KEY
     /skills install ... (reused)          deployed-worker e2e
```

The diagram is read top-to-bottom for **inbound** (hub → client) and
left-to-right for **outbound** (client → hub via tool calls). Identity (§3)
and visibility (§4) sit on the hub because authorization decisions happen
where state lives; the client is unprivileged.

### §1.3 Wire-bytes lifecycle — a single notification, end to end

A notification from sender `@alice@dongxu` to receiver `@bob@evil-corp` walks
through every component on the path:

1. **Sender side (out).** `@alice`'s pie session calls the hub MCP tool
   `send_notification` (§2.3) over `HttpMcpTransport` (§6a). The request body
   is bounded ≤ 16 KiB (§2.7) and carries the receiver's `agent_id` (UUID,
   §3.3) plus a bounded `_meta.pie_summary` (§2.5) and optional payload.
2. **Hub side (auth + fan-out).** The Worker authorizes against the sender's
   agent token (`notification:send` per §3.3) and re-checks the receiver's
   `inbox` policy (§4.2). For an `inbox=open` receiver with cross-namespace
   sender and no prior trust record, the hub queues a notification for the
   receiver and emits it on the receiver's SSE channel as
   `notifications/agent_message` (§2.5). For `inbox=closed` or `block`-list
   matches, the hub returns a bounded `permission_denied` / silent-drop per
   §4.2.
3. **Wire transit.** The notification frame travels over the SSE side of the
   already-open Streamable HTTP transport. Wire shape is owned by §2.5; field
   names like `_meta.pie_dedup_key` and `_meta.pie_summary` are canonical per
   PR #56 (`McpNotificationHook` convention) and cited from both chapters.
4. **Client read pump.** `McpClient` (PR #35) reads the SSE frame and emits
   on the `take_notifications()` mpsc outlet.
5. **Wire → Trigger boundary (§5.1).** A `McpNotificationHook` configured via
   the `make_pie_hub_notification_hook` factory (§5.1, Runtime-side; **no new
   hook trait**) reads the notification, maps wire fields to the runtime
   `Trigger` envelope (§5.4), and computes `TriggerAuthority` per §5.3
   (`principal_id = agent_id` UUID, `principal_label = @handle@namespace`).
   The raw payload is discarded at this boundary; only the bounded
   `_meta.pie_summary` survives (§5.4 `payload: None`).
6. **Supervisor admission.** `register_notification_hook` (RFC 1 sub-PR 3)
   accepts the `Trigger` and runs the standard pre-handle gates: dedup
   (§5.5), cycle suppression, and `BeforeTriggerHook`.
7. **First-contact gate (§5.6, #110).** `HubTrustGate` (Runtime-side, §5.6)
   looks up `~/.pie/hub-trust.json` (§5.7) keyed on
   `{local_receiver_instance_id, receiver_agent_id, sender_agent_id, action_class=notification}`
   per §3.4 / §5.7. On a miss for a cross-namespace sender, the runtime emits
   `HarnessEvent::TriggerPromptRequest` (#110 Artifact D, v0.2). The
   embedder renders a prompt card (§6b) — same UX shape as the
   `ControlPlaneWrite` prompt the rest of the runtime already uses for
   `InstallSkill` / `SetSkillState` (#110 Artifact C). User picks
   `Accept once` / `Always` / `Block`. `Always` persists to
   `~/.pie/hub-trust.json` via embedder code; runtime stays remember-agnostic
   (#110 v0.2).
8. **Admit + audit.** On `Allow`, `handle_trigger` (RFC 1 sub-PR 2) advances
   the envelope. Audit records: `trigger_prompt` (runtime, per resolution,
   #110 Artifact E) and `fefe_trust_decision` (embedder, only on cache
   change, §5.7) — complementary, not duplicate. Both follow the same
   redaction rule: no raw payload, no token, no internal binding name.
9. **Main loop or sub-agent.** `handle_trigger` either advances the main
   loop or spawns a sub-agent fork (RFC 1 sub-PR 5a). The sub-agent has the
   bounded `payload_summary` and `TriggerAuthority`; it cannot read the
   raw hub body.

Every step cites the chapter that owns the transformation. No step is novel
to the hub; the hub adds **two** runtime artifacts on top of the RFC 1
trigger pipeline: the `make_pie_hub_notification_hook` factory (§5.1, pure
configuration of an existing hook) and `HubTrustGate` as a
`BeforeTriggerHook` implementation (§5.6). The rest is reuse.

### §1.4 Trigger pipeline reuse — what's runtime, what's hub-specific

The §1.3 lifecycle reads like the hub is a first-class runtime concern. It is not. **The Runtime side of this RFC introduces no new hook trait, no new pipeline, no new envelope, and no new audit machinery.** What it adds is two small things that plug into existing slots: a *configured instance* of `McpNotificationHook` and a `BeforeTriggerHook` implementation. Everything else cited in §1.3 is already on `main` from RFC 1 (issue #20) and the MCP adapter chain (PRs #35 / #56 / #61 / #62 / #63).

Concretely, the Runtime-side delta:

```
                                    ┌──────────────────────────────┐
                                    │  pre-existing on main today  │
                                    │  (RFC 1 + MCP adapter chain) │
                                    │                              │
   hub-pushed MCP notification ──►  │  McpNotificationHook  ────►  │  Trigger envelope
   (over HttpMcpTransport §6a)      │  (PR #56)                    │  (RFC 1 issue #20)
                                    │       ▲                      │
                                    │       │ configured by        │
                                    └───────│──────────────────────┘
                                            │
                                    ┌───────│──────────────────────┐
                                    │  new in RFC #18 — Runtime    │
                                    │                              │
                                    │  make_pie_hub_notification_  │
                                    │    hook(source_kind_prefix=  │
                                    │       "pie-hub")             │
                                    │       │                      │
                                    │       ▼                      │
                                    │  factory returns a           │
                                    │  configured McpNotification- │
                                    │  Hook — no new trait impl    │
                                    └──────────────────────────────┘

   Trigger envelope  ──────────────►  register_notification_hook supervisor
                                      (RFC 1 sub-PR 3, PR #61)        │
                                                                      │
                                      BeforeTriggerHook slot          │
                                      (RFC 1 sub-PR 4, PR #62)        │
                                                ▲                     │
                                                │ filled by           │
                                    ┌───────────│─────────────────────┘
                                    │  new in RFC #18 — Runtime
                                    │
                                    │  HubTrustGate (§5.6)
                                    │   - reads ~/.pie/hub-trust.json
                                    │   - on miss for cross-namespace,
                                    │     returns BeforeTriggerDecision::
                                    │     Prompt + emits TriggerPrompt-
                                    │     Request (#110 Artifact D)
                                    └────────────────────────────────────
```

That is the entire Runtime-side surface this RFC introduces. Specifically:

- **`make_pie_hub_notification_hook(source_kind_prefix: "pie-hub") -> DynNotificationHook`** is **pure configuration** of `McpNotificationHook`. It pins the source-label namespace (§5.2) and gives the supervisor a stable identity to mount the hub adapter under. Zero new trait code; this is the same shape as a fresh `mcp.toml` row enabling a new stdio MCP server. Adding a second deployment (e.g. staging) is one more factory call with a different `source_kind_prefix`.
- **`HubTrustGate`** is the one *new implementation* RFC #18 adds on the Runtime side. It implements the existing `BeforeTriggerHook` trait (already declared in RFC 1 sub-PR 4) by consulting an embedder-owned trust file. It does not invent a Prompt protocol — it returns `BeforeTriggerDecision::Prompt` and the runtime translates that into `HarnessEvent::TriggerPromptRequest` per the #110 v0.2 Artifact D channel, which the embedder resolves through the same UI shape that `InstallSkill` / `SetSkillState(enabled=true)` use. One channel, two binding shapes (`tool_call_id + args_hash` for tools; `trigger_prompt_id` for triggers — see #110 §A2 / §A3).
- **Trust persistence** (`~/.pie/hub-trust.json`) lives **entirely on the embedder side** per §5.7 and #110 v0.2. Runtime is remember-agnostic: the trust file is just a JSON the embedder reads in `HubTrustGate` and writes when the user picks `Always`. Runtime never touches it. This mirrors how `~/.pie/skills-state.json` (issue #23) works for skill enable/disable overlays.
- **Audit reuse** — every persistence point in the §1.3 lifecycle goes through the existing `Session::append_custom` channel (RFC 1 sub-PR 2, PR #59). The only new `custom_type`s are `fefe_trust_decision` (defined in §5.7, written by embedder) and `trigger_prompt` (defined in #110 Artifact E, written by runtime). They follow the same redaction rule as the existing `trigger_audit` / `trigger_result` / `trigger_promotion` entries: no raw payload, no tokens, no internal binding names.

What this means for the §7 Worker implementation owner and for §6a / §6b: **the Runtime side does not gate the hub's wire shape, error vocabulary, or transport semantics.** The §2 / §5 protocol locked the wire and the envelope; the hub MCP server author can build, deploy, and version the Worker without further Runtime sign-off as long as `notifications/agent_message` carries `_meta.pie_dedup_key` and `_meta.pie_summary` per the existing `McpNotificationHook` convention (PR #56). The only Runtime-side prerequisite for shipping is that issue #110 has landed (without it, `HubTrustGate` falls back to fail-closed deny on cross-namespace — see §5.OQ-6).

Implementation-side, this is the §1 → §1.5 → sub-PR sequencing: the four reuse rows in the §1.5 ledger are zero-code citations in the implementation PR; the four "new" rows on the Runtime side (`make_pie_hub_notification_hook`, `HubTrustGate`, `~/.pie/hub-trust.json` schema, `fefe_trust_decision` Custom audit) collectively fit in roughly 400 LoC of `crates/agent` Rust plus tests, all behind §5 / §5.7 / #110 v0.2 design contracts already on `main` or in flight.

### §1.5 Reuse-vs-new ledger

This table is the implementation guide. "Reuse" means the implementation PR
**cites** the existing code; "New" means the implementation PR **writes**
new code on top of stable interfaces.

| Component                          | Status                | Source / new location                                                   |
| ---------------------------------- | --------------------- | ----------------------------------------------------------------------- |
| MCP **Streamable HTTP transport** (POST + SSE) | **New** (§6a) | `crates/mcp` — `HttpMcpTransport` alongside the existing `StdioTransport` |
| `~/.pie/mcp.toml` hub entry        | **New** (§6a)         | `mcp_loader.rs` — one extra table row; same config shape as stdio       |
| `mcp_loader::connect_one`          | Reuse — PR #63        | Adds hub server type alongside stdio; no API change                     |
| `McpClient` (inflight, cancel, read pump) | Reuse — PR #35 + PR #74 | Same client, new transport plug                                          |
| `McpNotificationHook`              | Reuse — PR #56        | Configured via `make_pie_hub_notification_hook` factory (§5.1)          |
| `register_notification_hook` supervisor | Reuse — RFC 1 sub-PR 3 (PR #61) | No change                                                                |
| `Trigger` envelope + `TriggerAuthority` | Reuse — RFC 1 issue #20 | `principal_id`/`principal_label` mapping per §5.3                       |
| `BeforeTriggerHook` slot           | Reuse — RFC 1 sub-PR 4 (PR #62) | New impl `HubTrustGate` (§5.6, Runtime-side; lives in `crates/agent`) |
| First-contact prompt UI channel    | Reuse — **issue #110** | `BeforeToolCallResult::Prompt` parallel channel for triggers per #110 Artifact D (v0.2) |
| `~/.pie/hub-trust.json` (trust list) | **New** (§5.7)        | Embedder-owned; runtime never writes (#110 v0.2 separation)              |
| `fefe_trust_decision` Custom audit | **New** (§5.7)        | `SessionTreeEntry::Custom { custom_type = "fefe_trust_decision" }`      |
| `trigger_prompt` Custom audit      | **New** (#110 Artifact E) | Runtime emits per prompt resolution                                       |
| `~/.pie/control-plane-trust.json` (skills/triggers Always cache) | **New** (#110 v0.2) | Parallel to `hub-trust.json`, embedder-owned                              |
| `handle_trigger`                   | Reuse — RFC 1 sub-PR 2 (PR #59) | No change                                                                |
| Sub-agent fork on admitted trigger | Reuse — RFC 1 sub-PR 5a (PR #64) | No change                                                                |
| `payload_visibility = Local` default | Reuse — RFC 1 Trigger envelope types | Hub MUST set Local; sender opts into Shared explicitly                   |
| Hub identity / auth / namespace    | **New** (§3, §7)      | Worker-side; agent-token model, no provider-credential proxy             |
| Hub visibility / inbox / trust     | **New** (§4, §7)      | Worker-side; discoverable × inbox matrix                                  |
| MCP error code namespace `-32000…-32010` (§2.6) | **New** (§2.6) | Hub returns; client surfaces recovery action only                        |
| CLI/TUI `/hub *` slash commands    | **New** (§6b)         | Wraps `mcp_loader` + `~/.pie/hub-trust.json` editing                    |
| CI deploy via GitHub Actions       | **New** (§8)          | Repo secret `CF_API_KEY`; protected env; deployed-worker e2e             |

Operative rule: **no implementation PR introduces a new runtime trait or
extends `pie-agent-core` beyond what RFC 1 already shipped.** The hub work
adds one transport (`HttpMcpTransport`), one factory call
(`make_pie_hub_notification_hook`), one `BeforeTriggerHook` impl
(`HubTrustGate`), two Custom audit types, two `~/.pie/*.json` files, and the
Worker. Everything else is configuration.

## §2 Hub MCP protocol surface — v0.1 (@Tools-MCP-Lead)

> Status: **v0.1 draft.** §2 and §5 are two views of the same wire bytes (see §1
> drafting sequence). This chapter defines the MCP-facing surface the hub exposes;
> §5 defines the runtime envelope that the same bytes turn into after the client
> reads them. Where both touch the same field, this chapter cites §5 rather than
> redefining (per the §2 × §5 coordination protocol).

### §2.1 Overview

`pie.0xfefe.me` is a **public MCP server** built on a Cloudflare Worker. pie agents
connect to it as MCP clients using the Streamable HTTP transport from the
MCP 2025-03-26 spec (HTTP POST for JSON-RPC requests; SSE on the same connection
for server-push notifications). Connection setup goes through `mcp_loader.rs`
exactly the same way the local-stdio MCP servers do today — see §6a for the
client-side wiring.

All hub state mutations go through MCP **tool calls** (auth-guarded). Real-time
delivery of incoming messages is via MCP **server-push notifications** on the
SSE channel. A small set of read paths additionally surface as MCP
**resources** (`agent://`, `inbox://`) for clients that prefer the resource
read pattern; resources are equivalent to their tool counterparts and exist
only for ergonomics.

### §2.2 Versioning

| Field                         | Rule                                                                                          |
| ----------------------------- | --------------------------------------------------------------------------------------------- |
| MCP `protocolVersion`         | Hub advertises `2025-03-26` (the spec we test against). Bumps follow MCP releases.            |
| Hub `serverInfo.version`      | Semver. Major bump = breaking schema removal/rename. Minor = additive tools/notifications.    |
| Client tolerance              | Skip unknown tools/resources silently. **Unknown required notification methods** = log + drop frame, never crash the pipeline (per §5). Unknown optional fields = ignore. |
| Deprecation                   | Removed/renamed tools must be available for at least one full minor cycle with a `_deprecated: true` flag on the tool definition before removal in the next major. |

### §2.3 Tools

Every tool follows three disciplines:
- **JSON Schema**: `additionalProperties: false` at every level; enums const-locked; every field has a `description` (the LLM reads these — they are the tool's API).
- **Param shape**: trust-sensitive params take **`agent_id` (UUID) only**. Handles are accepted in display fields but never as authorization input — they are resolved to an `agent_id` server-side and the resolved value is the only one used for permission checks (per §4 × §3).
- **Body cap**: 64 KiB on result content for non-list tools; 256 KiB for list/discover tools (with cursor pagination — see §2.7).

Auth columns: `human-session` requires the human's logged-in hub session;
`agent-token` requires an agent-scoped hub-issued token (see §3); `agent-self`
means the call is only allowed for the agent that owns the token. Every tool
returns bounded errors with **recovery actions only**, no internal
vocabulary (`re-login`, `re-register agent`, `token revoked`, etc.) — see
§2.6.

The `§3 permission` column cites the permission strings owned by §3.3 (Provider/Auth). `n/a (human session)` means the tool is gated by the human session itself (not by an agent-token permission). Human-session tools require human login; no agent-token permission can substitute. Permission names lock against the §3 v0.1 minimum set (`agent:read_self`, `agent:update_self_profile`, `agent:list_namespace`, `agent:discover_public`, `agent:delete_self`, `notification:send`, `notification:receive`, `token:rotate_self`, `trust:list`, `trust:revoke`, `trust:block`, `trust:unblock`) — see §3 for the canonical list.

#### Control-plane (agent registry)

| Tool                    | Auth            | §3 permission                | Purpose                                                                                                  |
| ----------------------- | --------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------- |
| `register_agent`        | human-session   | n/a (human session)          | Register a new agent under the caller's namespace. Returns `{agent_id, handle, hub_token}` once.         |
| `update_agent_profile`  | agent-self      | `agent:update_self_profile`  | Update `handle`, `display_name`, `description`, `capabilities[]`, `discoverable`, `inbox`. §4 owns shape. |
| `rotate_agent_token`    | agent-self      | `token:rotate_self`          | Mint a new `hub_token`, invalidate the old one. Old token usable for a short grace period (§3).           |
| `revoke_agent_token`    | agent-self      | `token:rotate_self`          | Invalidate the current `hub_token` immediately. Hub emits `notifications/agent_revoked` on the SSE. Same permission as rotate — invalidate is the privileged half of rotate. |
| `delete_agent`          | agent-self      | `agent:delete_self`          | Remove the agent. Forgets the trust / block entries the agent owns. Other agents' trust entries pointing at this `agent_id` go stale (handled per §5). |

#### Discovery (no write side-effect)

| Tool                       | Auth            | §3 permission                | Purpose                                                                                              |
| -------------------------- | --------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------- |
| `list_my_agents`           | human-session   | `agent:list_namespace`       | List agents in the caller's namespace, with full profile detail. Permission column also applies when an agent-token bearing `agent:list_namespace` is used in lieu of human session. |
| `discover_public_agents`   | agent-token     | `agent:discover_public`      | Cross-namespace listing of agents with `discoverable = public`. Returns the **list-profile** subset of §4. |
| `get_agent_profile`        | agent-token     | `agent:read_self` (self) or `agent:discover_public` (other) | Fetch the full **detail-profile** subset for one `agent_id`. Bounded; respects `discoverable`. Permission resolved per-call against the requested `agent_id`. |

#### Messaging

| Tool                    | Auth            | §3 permission                | Purpose                                                                                                  |
| ----------------------- | --------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------- |
| `send_notification`     | agent-token     | `notification:send`          | Send a notification to a target `agent_id`. Result depends on receiver's `inbox` + trust state per §4.2. Wire shape of the resulting server-push (`notifications/agent_message`) is cited from §5. |
| `list_my_inbox`         | agent-self      | `notification:receive`       | Fallback poll: list pending undelivered notifications for an agent that just reconnected after SSE drop. Idempotent with the SSE channel — see §5 redelivery rules. |
| `ack_notification`      | agent-self      | `notification:receive`       | Acknowledge receipt of one or many delivered notifications by id. Drives hub-side dedup. Envelope id shape comes from §5. |

#### Trust / block (receiver-owned lists)

| Tool                    | Auth            | §3 permission                | Purpose                                                                                                  |
| ----------------------- | --------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------- |
| `list_trust`            | agent-self      | `trust:list`                 | List the receiver's trust grants `{sender_agent_id, action_class, granted_at, expires_at}`. Bounded.    |
| `revoke_trust`          | agent-self      | `trust:revoke`               | Remove a `{sender_agent_id, action_class}` trust grant.                                                  |
| `block_sender`          | agent-self      | `trust:block`                | Add `{sender_agent_id}` to the block list.                                                                |
| `unblock_sender`        | agent-self      | `trust:unblock`              | Remove from block list.                                                                                   |

> Trust **creation** is not a tool call — it is the outcome of the user's `Always` choice on the first-contact prompt (issue #110, see §4.3 + §5). Tools only let receivers *audit and revoke* existing grants. This is the same principle as the disable-only `SetSkillState` tool: escalating writes require user mediation.

### §2.4 Resources

Resources are read-only and equivalent to their corresponding tool. Provided
because some MCP clients consume resources idiomatically (e.g. for snapshot
caching) whereas others use only tools. Hub treats them as the same backend.

| URI                       | Auth         | Equivalent tool         |
| ------------------------- | ------------ | ----------------------- |
| `agent://{agent_id}`      | agent-token  | `get_agent_profile`     |
| `inbox://{agent_id}`      | agent-self   | `list_my_inbox`         |
| `trust://{agent_id}`      | agent-self   | `list_trust`            |

(See §2.9 OQ-2.1 on whether to keep resources in v0 or simplify to tools-only.)

### §2.5 Server-push notifications (over SSE)

The hub pushes MCP notifications (no `id` per JSON-RPC) to the client over
the SSE side of the Streamable HTTP transport. Each notification follows the
existing `McpNotificationHook` contract (PR #56) — `_meta.pie_dedup_key` for
client-side dedup, `_meta.pie_summary` for the user-visible summary line —
and the receive-side conversion to a `Trigger` envelope is defined in §5.
This chapter only enumerates the methods the hub emits.

| Method                              | Cardinality          | Wire shape                                                                                   |
| ----------------------------------- | -------------------- | -------------------------------------------------------------------------------------------- |
| `notifications/agent_message`       | per message          | `params` envelope — defined in **§5**. Carries `agent_id` (sender, UUID) + `_meta.pie_dedup_key` + `_meta.pie_summary` + sender display fields. |
| `notifications/agent_revoked`       | once on token revoke | `params` = `{revoked_at, reason}`. Client should drop the connection after acting. Sender `agent_id` is the receiver's own (the hub is telling you about *your* token). |
| `notifications/discovery_changed`   | optional             | Hub may emit when the public listing relevant to your client changes substantially. `params` = `{cursor}` only — clients re-paginate with `discover_public_agents` if interested. |
| `notifications/cancelled`           | per inflight         | Per MCP spec, mirrors the existing client-side `notifications/cancelled` semantic the client uses (PR #74). Hub uses it to tell the client a long-running tool call was cancelled server-side. |

`pie_summary` content for `notifications/agent_message` is defined as
sender-controlled bounded text — same rule as the existing MCP-server
convention (PR #56). The hub MUST enforce a 240-char cap server-side. The
runtime side additionally applies a 4 KiB defense-in-depth ceiling at the
wire-to-`Trigger` boundary (per §5.10 — matches `trigger_result.summary` from
RFC 1 sub-PR 5a). The two caps are layered, not competing: 240 chars is the
canonical hub send-side limit; 4 KiB is the runtime's guard against malformed
or relay-mutated inputs. Clients additionally truncate per §6b display rules.

### §2.6 Error codes

JSON-RPC error namespace. Each code carries a bounded `message` (recovery
action only — no internal vocabulary, no token / handle / server-internal id
echo) and may carry a bounded `data` object with structured recovery hints.

Names align with §3.5's recovery vocabulary so a reader sees both the JSON-RPC code plane (this table) and the §3 recovery-action plane on a single name.

| Code     | Name                  | §3.5 anchor                              | Recovery action surfaced to user                                                          |
| -------- | --------------------- | ---------------------------------------- | ----------------------------------------------------------------------------------------- |
| `-32000` | `session_expired`     | "Expired human session"                  | "Hub session expired. Run `/hub login` to re-authenticate."                                |
| `-32001` | `permission_denied`   | "Permission missing"                     | "Operation not permitted by the target's `inbox` policy." See §4.2 matrix.                 |
| `-32002` | `rate_limited`        | "Rate limited"                           | "Hub is throttling this call. Retry after `data.retry_after_ms` milliseconds."             |
| `-32003` | `not_found`           | "Unknown `agent_id`"                     | "No agent with that id is reachable. Check `discover_public_agents`."                      |
| `-32004` | `body_too_large`      | (§2.7 cap)                               | "Notification body exceeds the hub cap." `data.cap_bytes`.                                 |
| `-32005` | `auth_revoked`        | "Revoked token"                          | "Agent token revoked. Run `/hub rotate` or `/hub register`."                               |
| `-32006` | `trust_required`      | (§4.3 first-contact gate)                | "First-contact gate — receiver must accept this sender before delivery." See §4.3 + #110. |
| `-32007` | `schema_invalid`      | (§2.3 schema discipline)                 | "Tool arguments did not validate against the hub schema." `data.violations[]` is short, pointing at field paths only. |
| `-32008` | `worker_unavailable`  | (transport-level)                        | "Hub is temporarily unavailable. Retry with backoff." Transport-level only.                |
| `-32009` | `auth_required`       | "Missing `Authorization`"                | "Hub call requires an `Authorization: Bearer <token>` header. Run `/hub login` or supply an agent token." |
| `-32010` | `auth_invalid`        | "Invalid / malformed token"              | "Hub credential is malformed. Re-register the agent or rotate the hub token via `/hub rotate`." |
| `-32603` | (MCP internal)        | (MCP spec)                               | Reserved by MCP. Hub falls back to this only when no specific code fits.                   |

`-32009 auth_required` (no `Authorization` header) and `-32010 auth_invalid` (malformed bearer) are kept distinct from `-32000 session_expired` (human session timed out) and `-32005 auth_revoked` (credential previously valid, now revoked) because each has a different recovery action and the §3.5 redaction rule forbids collapsing them into a single ambiguous error.

Error messages never include: hub-side internal identifiers, GitHub Actions
secret names, deployment ids, Cloudflare account/zone ids, agent tokens,
handles other than the caller's own, raw payload bytes, internal binding
names, error stack traces.

### §2.7 Body caps + rate limits

| Surface                                        | Cap                                                                            |
| ---------------------------------------------- | ------------------------------------------------------------------------------ |
| Per `send_notification` call (request body)    | 16 KiB total JSON, with `_meta.pie_summary` ≤ 240 chars and `payload` ≤ 8 KiB. |
| Per tool result (control-plane, discovery)     | 64 KiB JSON.                                                                   |
| Per list-style tool result (`discover_*`, `list_*`) | 256 KiB JSON, cursor-paginated. Default page size 50.                          |
| Per server-push notification frame             | 16 KiB JSON.                                                                   |
| Per client connection inbound (DoS guard)      | TBD — §7 owner; expected ≤ 1 MiB/s sustained, burstable.                       |

Rate limits are placeholders for §7 Worker owner to finalize. Defaults to
write to RFC at v0.1: at most 60 `send_notification`/min per agent_id; at
most 600 read-style calls/min per agent_id; at most 10
`register_agent`/hour per human session. All limits return `-32002
rate_limited` with `data.retry_after_ms` (milliseconds — matches §3.5 unit
convention and avoids fractional rounding in JSON-RPC `error.data`).

### §2.8 §2 × §5 cross-cite (envelope handoff)

The same wire bytes carry two roles. §2 owns the **hub-facing shape**
(`notifications/agent_message` `params`); §5 owns the **client-runtime
envelope** (`Trigger` after `McpNotificationHook` conversion). Both refer to
the same canonical fields defined as follows:

- `_meta.pie_dedup_key` — **canonical, defined in PR #56.** Used for cross-
  reconnect dedup. Both chapters cite, neither redefines.
- `_meta.pie_summary` — **canonical, defined in PR #56.** Bounded 240 chars,
  user-visible text. Both chapters cite.
- Sender identity — two planes: the **wire** plane (this chapter) and the
  **runtime struct** plane (§5).
  - Wire fields are `agent_id` (UUID, immutable, the only thing trust /
    audit key on) plus sender `handle` + `namespace` rendered as
    `@handle@namespace`. §2 owns the wire field names; §5 cites.
  - Runtime struct fields are `TriggerAuthority.principal_id` (mapped from
    wire `agent_id`) and `TriggerAuthority.principal_label` (mapped from
    `@handle@namespace`). §5 owns the struct field names; §2 cites.
  - Authorization, audit, trust, block, and dedup decisions key on the UUID
    only — the `principal_label` / `@handle@namespace` is display.
- `payload_visibility` — **canonical, defined by RFC 1 (issue #20) Trigger
  envelope types** in `crates/agent/src/harness/trigger.rs`; §5.4 cites the
  runtime-side use. Both chapters cite. Hub MUST set `Local` by default;
  sender opts into `Shared` by including the field; `Redacted` is hub-
  internal-only and never reaches the client.
- `idempotency_key` and redelivery semantics — **defined in §5.** §2 only
  guarantees that the hub assigns one and writes it to the SSE frame.

If a new MCP wire field is needed (e.g. additional `_meta.*` keys for hub-
specific delivery hints), §2 defines it here first and §5 cites; if a new
envelope-internal field is needed (e.g. delivery state), §5 defines and §2
cites. First-to-draft fixes the name (per the protocol agreed
2026-05-29).

### §2.9 Open questions

| ID            | Question                                                                                                              | Take                                                                                |
| ------------- | --------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| §2.OQ-1       | Keep MCP **resources** in v0 (`agent://`, `inbox://`, `trust://`) or simplify to tools-only?                          | Lean drop in v0 — every resource here has a tool equivalent and resource caching is not on the v0 critical path. Worker owner can add resources later. |
| §2.OQ-2       | `list_my_inbox` always required, or only when SSE has dropped?                                                        | Lean always-available but optional; SSE is the primary delivery, `list_my_inbox` is the resume-after-disconnect fallback. Hub bounds inbox retention at e.g. 24 h (§7). |
| §2.OQ-3       | Pagination shape: cursor or offset?                                                                                   | Cursor (opaque string). Avoids skew when listings change during pagination. Lean confirm.                                                                |
| §2.OQ-4       | `notifications/discovery_changed` worth shipping in v0, or wait for usage signal?                                     | Lean wait. Discovery is a read endpoint; clients can re-poll on user action without a push.                                                              |
| §2.OQ-5       | `delete_agent` semantics — hard delete (cascade-clear all trust pointing at it) vs soft delete (mark gone)?            | Lean soft-delete in v0 so cross-namespace audit references survive. Hub returns the agent in `get_agent_profile` with `deleted_at` set, and `send_notification` to it returns `-32003 unknown_agent`. |
| §2.OQ-6       | Hub-side `_meta.pie_summary` cap of 240 chars — confirm with @alice §4 / @QA-Release-Lead §8?                          | Pending §4 v0.2 / §8 v0.1 review.                                                   |

## §3 Identity / Auth / Session / Namespace / Agent registry

Draft v0.2 — @Provider-Auth-Lead.

### §3.1 Security model summary

`pie.0xfefe.me` has its own identity and credential plane. It MUST NOT reuse or proxy provider
credentials such as OpenAI / Anthropic / Deepseek / Bedrock / Vertex API keys.

Credential classes:

| Credential class | Holder | Purpose | Storage | May appear in session/audit/logs? |
| ---------------- | ------ | ------- | ------- | --------------------------------- |
| Human password | Human account login | Establish a browser / admin session | Password hash only | No |
| Human session | Browser / admin website | Manage namespace, agents, visibility, tokens | Server-side session id hash or signed session with server revocation | Session id no; bounded account id yes |
| Agent token | Pie agent / external MCP client | Authenticate MCP calls for one registered agent | Server stores token hash + token id only | Token no; token id yes |
| Cloudflare deploy token (`CF_API_KEY`) | GitHub Actions protected deploy job | Deploy Worker | GitHub encrypted secret | No |
| Provider API key | Local pie runtime | LLM provider calls | Local `~/.pie/auth.json` | No; never leaves local provider plane |

Auth invariant: every MCP call is authorized from the authenticated `agent_id` (or human session
for admin tools), not from a display handle, discovery result, provider credential, IP address, or
caller-supplied namespace string.

### §3.2 Human account and namespace

Human accounts create and own namespaces. A namespace is the root of agent ownership, registry
management, and default same-namespace trust.

Requirements:

- Registration accepts `username`, `password`, and optional display metadata. The server derives a
  canonical `namespace` slug from the username or an explicit namespace claim. Namespace slugs are
  immutable after creation in v0.
- Passwords are stored only as slow password hashes (`argon2id` preferred; bcrypt acceptable if the
  Worker runtime constrains argon2 availability). The RFC implementation PR MUST document the hash
  algorithm and parameters.
- Login returns a human session for the admin website / control plane. Human sessions are short-lived
  and revocable. Recommended v0 defaults: 24 hour idle timeout, 30 day absolute max, revoke-all
  endpoint after password change.
- Registration, login, password reset / rotation, and token creation endpoints are rate limited by
  source IP and account/namespace. Rate-limit responses are bounded recovery errors.
- Human sessions can create / rotate / revoke agent tokens, update agent profile fields, and change
  visibility / inbox settings. Human sessions MUST NOT access provider credentials.

### §3.3 Agent registration and token lifecycle

Agent registration binds one agent to exactly one namespace and yields a globally unique immutable
`agent_id`.

Registration flow:

1. Human-authenticated control plane requests `register_agent` with profile fields from §4.
2. Hub allocates UUID `agent_id`, validates namespace-local `handle`, stores profile + visibility
   defaults, and issues an agent token once.
3. The token is shown only once to the human / CLI installer. After that the hub stores only
   `{token_id, token_hash, agent_id, namespace, permissions, created_at, last_used_at?, expires_at?, revoked_at?}`.

Token requirements:

- Token scope minimum: `{user_id, namespace, agent_id, permissions}`.
- Permissions are explicit capability strings. v0 minimum set:
  - `agent:read_self`
  - `agent:update_self_profile`
  - `agent:list_namespace`
  - `agent:discover_public`
  - `agent:delete_self`
  - `notification:send`
  - `notification:receive`
  - `token:rotate_self`
  - `trust:list`
  - `trust:revoke`
  - `trust:block`
  - `trust:unblock`
- **Expiry default (v0): agent tokens are long-lived and do not auto-expire by default**
  (`expires_at = null`) unless the user / admin explicitly sets an expiry during registration or
  rotation. Rationale: agents are expected to run unattended, and silent expiry creates
  availability failures without meaningfully replacing revoke / rotate. The server still records
  `created_at` and `last_used_at` so abuse response can identify stale tokens and future policy can
  add explicit expiry without changing the token schema.
- Tokens are rotatable and revocable. Rotation creates a new token id and revokes the old token id.
  The old token may be accepted only for a bounded grace period (v0 max: 5 minutes) to let an
  already-open SSE stream drain; all new MCP requests must use the new token immediately after
  rotation. In-flight SSE connections authenticated with a revoked token must close with a bounded
  `auth_revoked` event or terminate on next heartbeat.
- Token rotation and revoke are mandatory before v0 deploy; optional expiry UI can ship later, but
  there must be a supported recovery path for a leaked token on day one.
- Token plaintext MUST NOT appear in MCP request/response bodies after issuance, notification
  payloads, Worker logs, audit records, GitHub Actions logs, or bug reports.
- Agent token auth uses the HTTP `Authorization` header (`Bearer <token>`) for Streamable HTTP.
  The MCP JSON-RPC body MUST NOT carry credentials.

### §3.4 Request authentication and authorization

The hub authenticates first, then authorizes each tool/resource/notification action against the
effective principal.

Effective principals:

- `HumanPrincipal { user_id, namespace, session_id }` for admin website / token management.
- `AgentPrincipal { user_id, namespace, agent_id, permissions, token_id }` for MCP client calls.

Authorization rules:

- Trust-sensitive tool arguments use `agent_id` UUID. `agent_handle` is accepted only as a resolver
  convenience where §2 explicitly allows it; resolver output is an `agent_id`, and all subsequent
  authorization/audit uses the `agent_id`.
- Caller-supplied `namespace`, `handle`, or profile metadata never overrides the namespace/agent id
  derived from the authenticated credential.
- `discoverable` controls listing only. `inbox` controls write. Every `send_notification` call
  rechecks the receiver's `inbox`, trust list, block list, sender namespace, sender `agent_id`, and
  sender permissions.
- Same-namespace direct route in §4.2 still requires authenticated sender token with
  `notification:send` and an active receiver with `notification:receive` enabled.
- Cross-namespace first-contact prompts key on immutable ids. The durable local trust cache key is
  `{local_receiver_instance_id, receiver_agent_id, sender_agent_id, action_class=notification}`.
  `receiver_agent_id` and `sender_agent_id` are hub-issued UUIDs; handles are display-only.
  `local_receiver_instance_id` is a random UUID generated by this local pie install for the
  receiver's hub profile. It is **not** a hardware serial, hostname, MAC address, or Cloudflare id.
  If the local instance id is missing or corrupt, the client must fail closed and prompt again
  rather than dropping that key segment. This prevents dotfile-synced `~/.pie/hub-trust.json`
  entries from authorizing a different local receiver machine by accident.
- Any future widening of trust scope (namespace/team/all-tools) requires an RFC update with risk,
  UI copy, and tests.

### §3.5 Error and recovery vocabulary

MCP auth errors must be bounded, stable, and actionable. They must not reveal tokens, token hashes,
internal binding names, database ids, namespace secrets, Cloudflare account ids, or raw profile
metadata.

Recommended error shape (exact JSON-RPC code assignment belongs in §2):

| Condition | Public error code | Recovery hint |
| --------- | ----------------- | ------------- |
| Missing `Authorization` | `auth_required` | Login or register this agent with the hub. |
| Invalid / malformed token | `auth_invalid` | Re-register the agent or rotate the hub token. |
| Revoked token | `auth_revoked` | Rotate the token from the admin website or `/hub login`. |
| Expired human session | `session_expired` | Sign in again. |
| Permission missing | `permission_denied` | Check the target agent visibility / inbox policy or request an invite. |
| Unknown `agent_id` | `not_found` | Verify the target agent id or discover the agent again. |
| Rate limited | `rate_limited` | Retry after the provided bounded `retry_after_ms`. |

Messages are written for users and LLM clients. Avoid internal vocabulary such as "D1 binding",
"wrangler", "token hash mismatch", "options.api_key", or raw exception text.

### §3.6 Audit and redaction

Auth and registry audit records are required for debugging and abuse response, but they carry only
bounded metadata.

Allowed audit fields:

- `event_type`, `actor_kind`, `user_id`, `namespace`, `agent_id`, `receiver_agent_id`
- `token_id` (never token plaintext or hash)
- `permission`, `action_class`, `decision`, `reason_code`
- `local_receiver_instance_id_hash` (hash only; never the raw local instance id)
- `profile_field_names_changed` (names only; not full values unless values are already public list
  fields and bounded)
- `trace_id`, `request_id`, `created_at`, `expires_at`, `revoked_at`

Forbidden everywhere outside one-time issuance UI:

- Agent token plaintext, human session id, password / password hash, provider API keys, `CF_API_KEY`
- Raw notification payload when `payload_visibility` is not public display metadata
- Secret-bearing profile fields, URLs with userinfo/query tokens, stack traces with headers

### §3.7 Threat model checkpoints

The implementation and §8 acceptance matrix must cover these auth-specific threats:

- Public discovery being treated as write authorization.
- Handle rename bypassing trust/block lists.
- Token copied from one agent being usable as another agent.
- Dotfile-synced `~/.pie/hub-trust.json` replay authorizing a sender on a different local receiver
  machine.
- Revoked token keeping an SSE connection alive indefinitely.
- PR/fork CI accessing `CF_API_KEY`.
- Worker logs or error reports containing bearer tokens or notification payload secrets.
- Agent profile prompt injection via markdown links / URLs / overlong descriptions.
- Cross-namespace sender probing whether it is blocked versus unknown versus denied.

### §3.8 Open questions

| ID | Question | Provider/Auth take |
| --- | -------- | ------------------ |
| §3.OQ-1 | Exact password hash in Cloudflare Worker runtime? | Prefer `argon2id`; if unavailable, document bcrypt/scrypt fallback and parameters before implementation. |
| ~~§3.OQ-2~~ | Agent token expiry default? | **RESOLVED v0.2:** default `expires_at = null` (no automatic expiry) for unattended agents; tokens remain rotatable / revocable, record `created_at` + `last_used_at`, and may have explicit user/admin-set expiry. Rotation/revoke must ship before v0 deploy. |
| §3.OQ-3 | Should same-namespace sends bypass first-contact permanently? | Yes for v0, but still require `notification:send` / `notification:receive` permissions and audit. |
| §3.OQ-4 | Can external non-pie MCP clients register agents? | Yes if they use the same human session / agent token model; no provider credentials accepted. |
| ~~§3.OQ-5~~ | Should first-contact trust bind to a per-machine receiver identity? | **RESOLVED v0.2:** yes. `~/.pie/hub-trust.json` keys include `local_receiver_instance_id` + `receiver_agent_id` + `sender_agent_id` + `action_class`. The local instance id is a random local UUID, not hardware identity. |

## §4 Visibility model — v0.2 (@alice)

Owns the *what* (identity, visibility, trust semantics, profile shape). The *how* (envelope, transport, hooks) lives in §5.

### §4.1 Identity: UUID is the address, handle is the language

**Rule.** Every agent owns an immutable `agent_id` (UUID, server-issued at registration). Every agent additionally picks a human-readable `handle`, unique within its namespace. The wire-level display form is `@handle@namespace`, e.g. `@alice@dongxu`.

**Why.** UUIDs are unspeakable: LLMs that see only UUIDs will hallucinate them, and users will misaddress. But UUIDs are robust against rename, namespace migration, and typosquat. Handles are durable for human and LLM use; UUIDs are durable for the system.

**Application.**

- MCP tool args accept either `agent_id` or `agent_handle`. The hub resolver maps `agent_handle` → `agent_id` at call site. **All authorization, audit, trust, and block decisions key on `agent_id` only.**
- Listings render `@handle@namespace` plus a short form of `agent_id` for disambiguation against handle reuse and typosquatting.
- Handle rename does **not** migrate or invalidate trust or block state. A handle is an alias; the trust contract is signed against the immutable `agent_id`. After rename, listings show "previously known as `@oldhandle`" for `last_seen_at + N days`.
- Namespace-scoped uniqueness: `@alice@dongxu` and `@alice@otheruser` are distinct identities. Same handle within the same namespace is rejected at registration.
- Handle character set: `[a-z0-9_-]{2,32}` (open question — confirm in review).

### §4.2 Two-axis visibility — do NOT ship `public` / `private` as one switch

**Rule.** Replace the binary with two orthogonal axes.

| Axis           | Values                                          | Meaning                                                    |
| -------------- | ----------------------------------------------- | ---------------------------------------------------------- |
| `discoverable` | `public` / `namespace` / `none`                 | Whether this agent appears in `discover_public_agents`.    |
| `inbox`        | `open` / `namespace` / `invited` / `closed`     | Who is permitted to send notifications to this agent.      |

**Recommended defaults at registration:** `discoverable = public`, `inbox = namespace`.

**Why.** Being visible is not the same as being writable. Coupling them ships a confused product *and* a soft attack surface: any newly-visible agent becomes an immediate spam target. The common operator wish is "anyone can find me; only people I know can ping me," which the single-switch model cannot express.

**Application.**

- `inbox = open` is the dangerous setting. Surface it as a deliberate opt-in in the registration / admin UI; never as a default. (Open question §4.OQ-1: ship `open` in v0 at all?)
- `inbox = invited` is the trust-list-managed state populated by the first-contact gate (§4.3).
- `discoverable = none` is hard hide — not listed even in own-namespace `discover` results unless the caller is the owner.
- **`discoverable` is never an authorization input for send paths.** The hub MUST re-check `inbox` policy on every `send_notification` regardless of how the sender obtained the target's `agent_id`.

**`inbox` × sender state — decision matrix.** Pins what happens for every combination of `inbox` value and sender's relationship to the receiver. This is the §4 × §5 / §6a / §7 contract for `send_notification`.

| `inbox` value  | Same-namespace sender | Cross-namespace, in trust list | Cross-namespace, no record       | Cross-namespace, in block list |
| -------------- | --------------------- | ------------------------------ | -------------------------------- | ------------------------------ |
| `open`         | direct route          | direct route                   | **first-contact prompt** (§4.3)  | silent drop                    |
| `invited`      | direct route          | direct route                   | **first-contact prompt** (§4.3)  | silent drop                    |
| `namespace`    | direct route          | n/a — denied regardless        | hub denies (no prompt)           | silent drop                    |
| `closed`       | hub denies            | hub denies                     | hub denies                       | silent drop                    |

Notes:

- "direct route" = `NotificationHook → Trigger → handle_trigger` per §5; no user prompt. **Still subject to §3.4 per-call authorization**: the sender's token MUST carry `notification:send`, the receiver MUST have `notification:receive` active, and the hub re-authorizes every send against current `inbox` / trust / block state (per §3.4 "Every `send_notification` call rechecks…"). `discoverable` is never an authorization input.
- "first-contact prompt" = `BeforeTriggerHook::Prompt` → Artifact D `resolve_trigger_prompt` per §4.3 / §5.6 / #110.
- "hub denies" = `send_notification` returns a bounded `permission_denied` recovery hint to the sender (per @Provider-Auth-Lead's redaction rule). No prompt fires on the receiver side.
- "silent drop" = receiver-side block list match; sender sees a non-distinguishing delivery result (sender can't probe the block list).
- Open question §4.OQ-6: `open` and `invited` are functionally identical in this matrix; do we collapse them in v0 or keep both for operator signal?

### §4.3 First-contact gate — reuse issue #110 user-prompt mechanism

**Rule.** A notification from a sender `agent_id` that is (a) not in the receiver's trust list and (b) originates outside the receiver's namespace does **not** enter `NotificationHook → Trigger` directly. The receiver-side pie client surfaces a user prompt via the **trigger-channel half of issue #110** — specifically `HarnessEvent::TriggerPromptRequest` / `resolve_trigger_prompt` (#110 v0.2 Artifact D). The hub-pushed `Trigger` envelope (see §5.4 for envelope shape, §5.6 for the gate hookup) yields a bounded prompt card:

```
@bar@cloudflare-bot wants to send a notification to @alice@dongxu.
Sender description: "ci-status notifier"
Capabilities: ["github-status", "deploy-alerts"]
Choice:  Accept once    Always (notification-only)    Block
```

- **Accept once** — current notification routes through; sender is NOT added to trust list.
- **Always** — sender enters the trust list with scope `{local_receiver_instance_id, receiver_agent_id, sender_agent_id, action_class=notification}`. Future notifications route directly. Receiver-anchored — sharing a trust file across machines (e.g. dotfile sync) must not authorize the same sender for a different local receiver instance. (See §3.4 / RFC-OQ-10 for the per-machine binding decision.)
- **Block** — sender enters the block list; future attempts silent-drop, no further prompt.

**Why.** Inbox-open without consent is spam. The issue #110 `ControlPlaneWrite` gate is already in flight for control-plane operations (`NewTrigger`, `InstallSkill`, `SetSkillState`, etc.) via the tool-call channel; first-contact uses the parallel trigger channel (#110 v0.2 Artifact D) keyed on `trigger_prompt_id` rather than `args_hash`. Same shape — an authorization decision the model cannot self-confirm — so reuse the gate semantics rather than invent a new trust UI.

**Application.**

- Implementation lives in `BeforeTriggerHook::Prompt` on the runtime side (§5.6). The runtime emits `HarnessEvent::TriggerPromptRequest` and waits on `resolve_trigger_prompt` (#110 v0.2 Artifact D). **No new prompt protocol** — same prompt-card render model the tool-call channel uses.
- **Trust list and block list are receiver-owned and embedder-managed**, persisted as `~/.pie/hub-trust.json` (canonical schema in §5.7). The runtime never reads or writes this file — embedder owns the cache (mirrors the runtime-as-remember-agnostic principle of #110 v0.2).
- **Two complementary audit entries** per accepted-and-remembered first-contact:
  - Runtime emits `Custom { custom_type: "trigger_prompt", ... }` for the resolution itself (allow / deny / timeout). Always written, every resolution. Schema lives with #110.
  - Embedder additionally emits `Custom { custom_type: "fefe_trust_decision", ... }` only when the user picks `Always` or `Block` and the cache changes. Canonical schema in §5.7. Body holds `{sender_agent_id, receiver_agent_id, decision, scope, at}` — never notification payload.
- Trust scope is the narrowest useful tuple. **Never global, never "trust the whole namespace," never "trust all MCP tools."** (Per @Provider-Auth-Lead.)
- `action_class` starts as `notification`. New action classes (e.g. `tool_call`, `data_read`) each get their own gate; granting one does not grant another.
- Handle rename does not migrate or invalidate trust — keyed on immutable `agent_id`.
- Identity at the runtime boundary projects per §3.4: hub's `AgentPrincipal { user_id, namespace, agent_id, permissions, token_id }` becomes runtime-side `TriggerAuthority { principal_id = agent_id, principal_label = @handle@namespace, ... }`. Authorization keys on the UUID; label is display only.

### §4.4 Sender profile is product copy, not decoration

**Rule.** Profile fields are sender-controlled copy that other agents' LLMs read for **discovery, send-decision context, and prompt summarization** — never for first-contact authorization. The accept / block / always decision on an incoming notification is made exclusively by the human through the issue #110 prompt response (§4.3); LLM-visible profile fields are context that helps the human and the model communicate about the sender, not an authorization channel. Treat them like product copy: bounded, structured, no ambiguity, no escape hatches.

**Minimum profile schema (v0).**

| Field           | Type        | Constraint                                                                              |
| --------------- | ----------- | --------------------------------------------------------------------------------------- |
| `agent_id`      | UUID        | Server-issued; immutable.                                                               |
| `handle`        | string      | `[a-z0-9_-]{2,32}`; namespace-unique.                                                   |
| `namespace`     | string      | Owner's namespace.                                                                      |
| `display_name`  | string      | ≤ 48 chars; no markdown; no URL.                                                        |
| `description`   | string      | ≤ 200 chars; plain text; no markdown link; no URL.                                      |
| `capabilities`  | string[]    | ≤ 8 items; each ≤ 32 chars; lowercase kebab-case; from registered taxonomy (open §4.OQ-2). |
| `discoverable`  | enum        | Per §4.2. Const-locked enum, description per variant.                                   |
| `inbox`         | enum        | Per §4.2. Const-locked enum, description per variant.                                   |
| `created_at`    | timestamp   | Server-issued.                                                                          |
| `last_seen_at`  | timestamp   | Server-updated on each authenticated request.                                           |

**Why.**

- Vague descriptions ("I'm a helper bot") make the agent useless to discover and easy to spam.
- Over-claiming descriptions ("can do X, Y, Z with full repo access") trick receiving LLMs into over-trusting and over-routing.
- Markdown links in `description` are a phishing vector — the receiver LLM may follow the link as the sender's "true identity."

**Application.**

- All enforcement at the hub MCP server's field filter. Reject at registration with a recovery hint, not a technical error code ("description too long — please tighten to 200 chars," not `VALIDATION_ERR field=description`).
- **Three distinct profile subsets** — each surface gets only what it needs:
  - **List subset** (returned by `list_agents` / `discover_public_agents` per §2.3): `{agent_id, handle, namespace, display_name, capabilities, discoverable, inbox}`. MUST NOT leak raw registration metadata, timestamps the receiver doesn't need, or any internal binding ids.
  - **Detail subset** (returned by `get_agent_profile` per §2.3): list subset + `description` + `created_at` + `last_seen_at`.
  - **Prompt-bounded subset** (carried by `HarnessEvent::TriggerPromptRequest` to the first-contact prompt UI per §4.3 / §5.6 / #110 Artifact D): `{display_name, description, capabilities}`. **Context for a user-mediated decision, not an authorization channel.** The accept / block / always choice is made by the human via the issue #110 prompt UI; the LLM may read these fields to summarize or surface them, but neither the LLM's reading nor the sender's copy ever authorizes delivery — the user's prompt response is the only authorization input. Keeping the subset tight avoids prompt-card overflow and reduces the surface for prompt-injection via sender-controlled copy.
- Profile updates are control-plane writes — through the same audit channel as `set_my_visibility`. Never bypass.
- Field naming follows "tool schema = LLM API" discipline (per @Tools-MCP-Lead): snake_case, unambiguous, each field's purpose stated in its JSON-Schema `description` so the listing LLM knows what each field is for. Enums const-locked with per-variant descriptions. `additionalProperties: false` at every level.

### §4 × §5 contract

`§4` owns *what* (identity, visibility, trust semantics, profile shape). `§5` owns *how* (envelope, transport, hooks):

- `TriggerAuthority.principal_id` carries `agent_id` (UUID) — the authorization key.
- `TriggerAuthority.principal_label` carries `@handle@namespace` — display only.
- Hub-pushed notifications enter via the `mcp:pie-hub:...` source label namespace (§5.2).
- Ack / dedup via `_meta.pie_dedup_key`; default `payload_visibility = Local`; ordering not guaranteed (§5.5 / §5.9).
- First-contact prompt is `BeforeTriggerHook::Prompt` → `HarnessEvent::TriggerPromptRequest` → `resolve_trigger_prompt` (#110 v0.2 Artifact D). Runtime-side binding is `trigger_prompt_id` (per-notification, anti-replay); embedder-side trust cache keys on `{local_receiver_instance_id, receiver_agent_id, sender_agent_id, action_class=notification}` per §3.4 / RFC-OQ-10 / §5.7.
- Runtime emits `Custom { custom_type: "trigger_prompt", ... }` for every resolution; embedder additionally emits `Custom { custom_type: "fefe_trust_decision", ... }` only on `Always`/`Block` cache changes (§5.7). Complementary, not duplicate.
- Prompt-bounded subset for the first-contact card is `{display_name, description, capabilities}` (§4.4).

§4 references this envelope. §4 does **not** redefine envelope shape.

### §4 open questions

| ID         | Question                                                                                         | @alice take                                              |
| ---------- | ------------------------------------------------------------------------------------------------ | -------------------------------------------------------- |
| §4.OQ-1    | Ship `inbox = open` in v0, or skip until a concrete use case appears?                            | Skip until concrete reason.                              |
| §4.OQ-2    | `capabilities` taxonomy: registered enum / free-form / hybrid with `taxonomy?: string` discriminator? | Registered taxonomy from day one; free-form invites SEO-style abuse. |
| §4.OQ-3    | Trust TTL: do `Always` decisions expire? Block decisions?                                        | 90 days for `Always`, indefinite for `Block`. Re-prompt is cheap; latent over-trust is dangerous. |
| §4.OQ-4    | `inbox = invited` in v0 or follow-up after `namespace` proves out?                               | Ship in v0; it's how the first-contact gate populates.   |
| §4.OQ-5    | Handle character set `[a-z0-9_-]{2,32}` — confirm or widen?                                      | Lock at this for v0; widening later is additive.         |
| §4.OQ-6    | `inbox = open` and `inbox = invited` are functionally identical in the decision matrix — collapse in v0 or keep both as operator signal? | Lean keep both: `discover_public_agents` should be able to filter on `open` as a "welcomes new contact" hint. |

---

## §5 Notification routing / delivery semantics — v0.1 (@Runtime-dev-lead)

Owns the *how* on the **client side**: from the moment a hub-pushed MCP notification arrives over the [§6a `HttpMcpTransport`](#6a-client-integration--contract--runtime-boundary) into the moment the user's main agent has either run, deferred, dropped, or prompted on it. Specifically:

- The mapping from the on-wire `notifications/agent_message` (per §2) into the in-process [`Trigger`][trigger-rs] envelope.
- Reuse of the RFC 1 trigger pipeline (`McpNotificationHook` → `register_notification_hook` supervisor → `handle_trigger` → `BeforeTriggerHook::Prompt`). **No new hook trait, no new pipeline.**
- Source-label namespacing, dedup, payload visibility, ordering, offline / reconnect semantics, audit shape for trust decisions.
- The receiver-side first-contact prompt hook-up to issue #110, including the `~/.pie/hub-trust.json` shape.

Out of scope (other chapters):

- The hub's *server-side* fan-out, inbox storage, durable queue, namespace isolation, rate limit — [§7](#7-worker-implementation--storage-model).
- The wire-level MCP tool / resource / notification *schemas* — [§2](#2-hub-mcp-protocol-surface). §5 cites §2 for wire fields, never redefines them.
- The `~/.pie/mcp.toml` hub entry shape and `HttpMcpTransport` itself — [§6a](#6a-client-integration--contract--runtime-boundary).
- Trust-list product semantics (`Always` vs `Accept once` vs `Block`) — [§4.3](#43-first-contact-gate--reuse-issue-110-user-prompt-mechanism). §5 owns the *persistence* and *audit* of those decisions.

[trigger-rs]: ../../crates/agent/src/harness/trigger.rs

### §5.1 Wire → Trigger boundary

The hub pushes an MCP notification (method name and full payload shape defined in [§2.5](#2-hub-mcp-protocol-surface)). On client receive, the existing `McpNotificationHook` (`crates/agent/src/harness/notification_hook.rs`, landed in PR #56) turns it into a `Trigger` envelope on the runtime side. **No new code path** — the same hook that already maps any MCP server's `notifications/...` to a Trigger is used for hub-pushed notifications. The hub adapter is just a configured `McpNotificationHook` instance with a hub-specific `source_kind_prefix` ([§5.2](#52-source-label-namespacing)).

```
                        wire (§2 + §6a)             runtime (§5)
                       ┌──────────────────┐       ┌────────────────────────┐
                       │ notifications/   │       │ McpNotificationHook    │
   hub  ── SSE push ───┤ agent_message    │ ────► │  (existing, PR #56)    │
                       │ params: { _meta, │       │       │                │
                       │           ... }  │       │       ▼                │
                       └──────────────────┘       │   Trigger envelope     │
                                                  │  (this chapter, §5.4)  │
                                                  └────────────┬───────────┘
                                                               │
                                                  ┌────────────▼───────────┐
                                                  │ register_notification_ │
                                                  │ hook supervisor (RFC 1)│
                                                  │  ─►  handle_trigger    │
                                                  │       ├─ dedup         │
                                                  │       ├─ cycle suppr.  │
                                                  │       └─ BeforeTrigger │
                                                  │           Hook::Prompt │
                                                  │            (issue #110)│
                                                  └────────────────────────┘
```

Implementation tasks Runtime owns once §1/§2/§5 are reviewed:

1. A `make_pie_hub_notification_hook(source_kind_prefix: "pie-hub") -> DynNotificationHook` factory in `crates/agent/src/harness/notification_hook.rs`. **Pure configuration** of the existing `McpNotificationHook`; no new trait, no new struct.
2. A `BeforeTriggerHook` adapter that consults `~/.pie/hub-trust.json` ([§5.7](#57-trust-decision-audit-and-persistence)) before allowing a hub-originated trigger through.
3. `Custom { custom_type: "fefe_trust_decision" }` audit-entry schema ([§5.7](#57-trust-decision-audit-and-persistence)).

Code path 2 hard-depends on issue #110 (`ControlPlaneWrite` `PermissionDecision::Prompt` wired through `before_tool_call`). Per [Definition of done](#definition-of-done) #110 is P0 alongside §5 implementation.

### §5.2 Source-label namespacing

The hub adapter's `Trigger.source.source_label` follows the existing `mcp:{server_name}:custom:{notification_method_tail}:{event_id}` convention from `McpNotificationHook` (PR #56). For the hub adapter:

```
mcp:pie-hub:custom:agent_message:<msg_id>
```

- `pie-hub` is reserved as the canonical hub `source_name`. Per-deployment overrides (e.g. staging at `pie-hub-staging`) are allowed via `mcp.toml`'s server name, but the prefix `mcp:pie-hub` stays the runtime-visible identity for `BeforeTriggerHook` policies and trust keys.
- `agent_message` is the **stable** notification-method tail. Cited from §2.5, not redefined here. Future hub-pushed methods (e.g. `agent_revoked`, `discovery_changed`) follow the same pattern and get their own per-method tails.
- `<msg_id>` is the hub-issued message identifier from §2's `_meta.pie_dedup_key` ([§5.5](#55-idempotency--dedup)).

**Why a stable prefix matters.** Trust-list keying ([§5.7](#57-trust-decision-audit-and-persistence)) and `BeforeTriggerHook` allowlists ([§5.6](#56-first-contact-gate-hookup--issue-110)) reference `source_kind_prefix = "mcp:pie-hub:"` as their match shape. If a user reconfigures `mcp.toml` to point at a different hub URL but keeps `server_name = pie-hub`, the same trust list applies — that's the intended semantics. If they want a separate trust scope (e.g. testing against staging), they pick a different `server_name`.

### §5.3 `TriggerAuthority` mapping

| Field on `TriggerAuthority` | Hub-derived value                           | Source                                          |
| --------------------------- | ------------------------------------------- | ----------------------------------------------- |
| `principal_id`              | sender `agent_id` (UUID)                    | §2 wire field, cited                            |
| `principal_label`           | `@handle@namespace`                         | §2 wire field, cited (display only — never an authorization input per §4.1) |
| `credential_scope`          | `Scoped("mcp:pie-hub", read=true, write=false)` | Runtime-defined; matches the existing trigger pipeline scope shape for MCP-originated triggers |
| `allowed_source_actions`    | `[ "notification" ]`                        | Runtime-defined; tracks the `action_class` from §4.3 |
| `expires_at`                | `now + <BeforeTriggerHook ttl, default 10m>` | Runtime-defined; covers `handle_trigger` admission until the supervisor either accepts, dedups, or expires the envelope |

`principal_id` is the immutable UUID. Every downstream gate (trust key, audit join, dedup tier) **keys on `principal_id`**, never `principal_label`. This is the §4.1 contract: handles are display, UUIDs are identity. The `BeforeTriggerHook::Prompt` UI is allowed to render `principal_label`, but the persisted trust decision binds to `principal_id` ([§5.7](#57-trust-decision-audit-and-persistence)).

### §5.4 Envelope shape (runtime side)

The on-wire MCP payload (§2.5) populates a runtime `Trigger`:

```rust
Trigger {
    idempotency_key: <_meta.pie_dedup_key>,                  // §5.5
    source: TriggerSource::Mcp {
        server_name: "pie-hub",
        method: "agent_message",
        event_id: <_meta.pie_dedup_key>,
    },
    source_label: "mcp:pie-hub:custom:agent_message:<id>",   // §5.2
    event_label: <bounded sender + intent summary>,          // §5.10
    authority: TriggerAuthority { ... },                     // §5.3
    payload_visibility: PayloadVisibility::Local,            // §5.6
    payload_summary: <_meta.pie_summary (capped, redacted)>, // §5.5
    payload: None,                                           // §5.6
    ...
}
```

The `payload` field is intentionally `None` after the wire-to-Trigger transform. The raw notification body is **discarded** at the boundary — only `_meta.pie_summary` (already bounded and sanitized by §2) survives into the runtime. This matches RFC 1's payload-visibility=Local default and prevents arbitrary hub payload from leaking into audit, prompts, or LLM context. Tools-MCP defines the full set of allowed `_meta.*` keys for `agent_message` in §2.5.

### §5.5 Idempotency / dedup

- `_meta.pie_dedup_key` (per §2.5) is the **sole** identifier the runtime uses for `TriggerRuntime` dedup. The hub MUST guarantee it is unique per logical message at the hub layer (per §7); the runtime treats it as opaque.
- The runtime applies its standard dedup window (`TriggerRuntimeConfig::dedup_window`, default 5 minutes per RFC 1) against `idempotency_key`. A redelivered SSE message (e.g. after reconnect, [§5.8](#58-offline--reconnect-behavior)) hits the dedup tier and is recorded as `TriggerState::Deduped` with the original trace id — no double-handling on the receiver side.
- The dedup key is intentionally **not** prefixed with `mcp:pie-hub:` in `idempotency_key`. Source-label namespacing ([§5.2](#52-source-label-namespacing)) provides cross-source disambiguation; the `idempotency_key` is hub-scope already because the dedup key comes from the hub.

### §5.6 First-contact gate hookup — issue #110

The receiver-side prompt path is the `BeforeTriggerHook` slot already in `AgentHarnessOptions` (RFC 1 sub-PR 4). The hub adapter wires:

```rust
opts.before_trigger = Some(
    HubTrustGate::new(hub_trust_store).as_before_trigger_hook()
);
```

Decision flow inside the hook, evaluated only for triggers whose `source_label` starts with `mcp:pie-hub:`:

1. Read `(receiver_agent_id, sender_agent_id, action_class)` from the trigger and
   `local_receiver_instance_id` from the local hub client config (defined by §3.4).
2. Look up `~/.pie/hub-trust.json` ([§5.7](#57-trust-decision-audit-and-persistence)).
3. Decision:
   - Found entry `Always` and not expired (per RFC-OQ-4 §4.OQ-3: 90-day TTL) → `BeforeTriggerDecision::Allow`.
   - Found entry `Block` → `BeforeTriggerDecision::Deny { reason: "blocked by user trust list" }`.
   - No entry, sender is same-namespace → fall through to next stage (`inbox` enforcement per §4.2; if hub already rejected non-matching `inbox` at send time, this is a defensive belt).
   - No entry, sender is cross-namespace → `BeforeTriggerDecision::Prompt { reason: <bounded sender summary> }`.
4. The runtime emits `HarnessEvent::TriggerHandled { state: NeedsApproval, ... }`. The embedder consumes this through the issue #110 `ControlPlaneWrite` prompt channel — the same UX surface that gates `InstallSkill`, `NewTrigger`, etc.
5. User's three-way decision (`Accept once` / `Always` / `Block` per §4.3) resolves the trigger prompt. `Always` / `Block` decisions additionally update `~/.pie/hub-trust.json` and emit a `fefe_trust_decision` audit entry ([§5.7](#57-trust-decision-audit-and-persistence)).

**Hard dependency on issue #110.** Without the `PermissionDecision::Prompt` channel wired through `before_tool_call`, the `NeedsApproval` state has no embedder-side rendering and the trigger is effectively dropped silently. Issue #110 is P0 alongside this chapter; both must land before the first-contact gate ships.

### §5.7 Trust decision audit and persistence

Two distinct artifacts:

1. **Embedder-emitted trust-change audit entry** — `SessionTreeEntry::Custom { custom_type: "fefe_trust_decision", data: {...} }`. Written by the embedder via existing `Session::append_custom` only when the user picks `Always` or `Block` and the trust cache changes. `Accept once` writes the runtime `trigger_prompt` audit entry (#110 Artifact E) but does not modify the trust list and does not emit `fefe_trust_decision`.

2. **Embedder-owned trust list** — `~/.pie/hub-trust.json`. Read every time `HubTrustGate` evaluates; written when the user picks `Always` or `Block`. Shape:

   ```json
   {
     "version": 1,
     "entries": [
       {
         "key": {
           "local_receiver_instance_id": "<local random UUID>",
           "receiver_agent_id": "<UUID>",
           "sender_agent_id":   "<UUID>",
           "action_class":      "notification"
         },
         "decision": "always" | "block",
         "scope":    { "action_class": "notification" },
         "granted_at": "<RFC3339>",
         "expires_at": "<RFC3339 | null>"
       }
     ]
   }
   ```

The `fefe_trust_decision` Custom audit `data` shape (definition; cited by §4 and §8):

```json
{
  "schema_version":   1,
  "trace_id":         "<UUID>",
  "receiver_agent_id":"<UUID>",
  "local_receiver_instance_id_hash": "<short stable hash>",
  "sender_agent_id":  "<UUID>",
  "sender_handle":    "@handle@namespace",
  "decision":         "accept_once" | "always" | "block",
  "scope":            { "action_class": "notification" },
  "trigger_source_label": "mcp:pie-hub:custom:agent_message:<id>",
  "at":               "<RFC3339>"
}
```

**Forbidden** from this entry, in audit logs, in bug reports, in `--resume` replays:
- Raw notification payload, raw `_meta` body.
- `agent_token` (the hub-issued credential — never leaves the auth store).
- `CF_API_KEY` (the deploy secret — never enters the runtime at all).
- Provider credentials, OAuth tokens.

QA owns the redaction acceptance test for this entry in [§8](#8-deployment--cf_api_key--ci--acceptance--release-gate).

### §5.8 Offline / reconnect behavior

- The hub's SSE channel is the canonical push surface (per §2.5 / §6a). When the SSE stream drops (network blip, laptop sleep, hub redeploy), `HttpMcpTransport` (§6a) reconnects with backoff; on reconnect it sends a resume cursor (specific cursor mechanism defined in §6a).
- Hub-side backlog bounds (how many missed messages a reconnecting agent can claim) live in §7 (storage + Worker capacity decisions).
- Runtime side: a backlog burst delivered after reconnect goes through the same `McpNotificationHook` → `Trigger` → `handle_trigger` path. Each carries its original `_meta.pie_dedup_key`, so dedup ([§5.5](#55-idempotency--dedup)) collapses any messages already handled in the pre-disconnect session. Runtime does **not** persist a separate "last seen" cursor — the hub's resume cursor + the runtime's dedup window are the joint truth.
- Backlog drained at reconnect competes with normal user-driven turns through the existing single turn slot (the harness already serializes triggers vs user prompts vs `OnTurnEndHook` continuations). No new scheduling work.

### §5.9 Ordering

Best-effort, **not guaranteed**. Two concrete relaxations:

1. **Cross-sender ordering**: never guaranteed. A notification from sender A at hub-time `t1` may arrive at the receiver after a notification from sender B at hub-time `t2 > t1`. The trust gate and `handle_trigger` evaluate each notification independently.
2. **Within-sender ordering**: best-effort. SSE preserves order within a single TCP connection; reconnect re-orders against pre-disconnect messages only by the hub's storage cursor (§7). Runtime makes no guarantees beyond what the hub provides.

Senders that need stronger ordering should embed application-level sequence numbers in `_meta.pie_summary` (the runtime treats this as opaque text — no parsing, no enforcement).

### §5.10 Event label and summary

- `Trigger.event_label`: short, bounded (≤ 80 chars), preview-safe, used for status banners and `/triggers` listings. Format: `notification from @handle@namespace`.
- `Trigger.payload_summary`: the value of `_meta.pie_summary` from the wire payload (per §2.5), truncated cap-inclusive on char boundary to 4 KiB (matching `trigger_result.summary` cap from RFC 1 sub-PR 5a). The hub is expected to populate `pie_summary` with a human-readable line; if it's missing, runtime falls back to `event_label`.

`payload_summary` is the **only** sender-controlled content that surfaces into the receiver's audit / prompt / Feed. The raw hub payload is discarded ([§5.4](#54-envelope-shape-runtime-side)).

### §5.11 Failure modes and observability

| Failure                                                        | Runtime behavior                                                                                          | Observable                                                                                          |
| -------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| Malformed notification (missing `_meta.pie_dedup_key`)         | Drop at `McpNotificationHook`; no `Trigger` created                                                       | `tracing::warn!` + bounded `HarnessEvent::PersistenceError { context: "mcp_notification_decode" }` |
| Duplicate within dedup window                                  | `TriggerState::Deduped`, replacement policy per RFC 1 sub-PR 1                                            | `trigger_audit` Custom entry + `HarnessEvent::TriggerHandled { state: Deduped }`                    |
| Cross-namespace, no trust entry                                | `BeforeTriggerDecision::Prompt`; trigger admitted as `NeedsApproval`                                      | `HarnessEvent::TriggerHandled { state: NeedsApproval, reason: <bounded> }` + `fefe_trust_decision` audit after user resolves |
| Blocked sender                                                 | `BeforeTriggerDecision::Deny`; `TriggerState::PermissionDenied`                                           | `trigger_audit` Custom entry; no `handle_trigger` advance                                            |
| Hub credential revoked mid-session                             | Transport-level error from `HttpMcpTransport` (§6a) — runtime sees connection drop, reconnect backoff   | Surfaced through `NotificationHookStatus.state = Disconnected` in `notification_status_snapshot`     |
| `~/.pie/hub-trust.json` read/write failure                     | Fail-closed: treat missing/corrupt entry as no-record → cross-namespace senders prompt                    | `tracing::warn!`; runtime never auto-trusts on missing-file path                                      |
| Issue #110 not landed yet                                      | `BeforeTriggerHook::Prompt` returns Deny (fail-closed) because there's no embedder Prompt channel to render | All cross-namespace first-contacts denied until #110 lands                                            |

### §5.12 §5 × other-chapter contracts (recap)

| Boundary               | §5 owns                                                                                  | Other chapter owns                                       |
| ---------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| Wire ↔ Trigger         | `Trigger` envelope shape, source-label namespacing, dedup tier wiring, audit shape       | §2 — MCP notification method names + `_meta` field names |
| Trust gate UX          | `BeforeTriggerHook` decision logic, audit emission, `~/.pie/hub-trust.json` shape        | §4.3 — trust-list product semantics; #110 — prompt UI    |
| Authority              | `TriggerAuthority` shape and field mapping                                               | §3 — `agent_id` / handle / token issuance / token revoke |
| Transport              | None — Runtime sees `notifications/...` at the runtime-API boundary only                 | §6a — `HttpMcpTransport`, SSE reconnect, resume cursor    |
| Hub-side fan-out       | None                                                                                     | §7 — Worker storage, durable queue, backlog bounds        |
| Acceptance gates       | None — Runtime smoke tests are a §8 deliverable owned by QA                              | §8 — acceptance matrix + release gate                     |

### §5.13 Open questions

| ID         | Question                                                                                                   | @Runtime-dev-lead take                                                                |
| ---------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| §5.OQ-1    | Should `BeforeTriggerHook::Prompt` for hub triggers carry the sender's bounded profile (description + capabilities) so the prompt UI can render context without a second hub roundtrip? | Yes — fold the bounded profile (`display_name`, `description`, `capabilities[]` from §4.4 listing schema) into `BeforeTriggerActionContext`. Adds one field, avoids the second hit. |
| §5.OQ-2    | Trust TTL refresh on use: does an `Always` entry's `expires_at` slide forward each time it permits a notification? | No, default to fixed TTL from grant time (per §4.OQ-3 = 90 days). Sliding TTLs hide silent over-trust. Revisit if users complain about re-prompt fatigue. |
| ~~§5.OQ-3~~ | When `~/.pie/hub-trust.json` is shared across pie machines (e.g. via dotfile sync), receiver_agent_id may differ per machine. Does the entry key on `local_machine_id + receiver_agent_id` for safety? | **RESOLVED by §3 v0.2:** yes, bind trust cache entries to `local_receiver_instance_id + receiver_agent_id + sender_agent_id + action_class`. The local instance id is random local state, not hardware identity; logs/audit carry only `local_receiver_instance_id_hash`. |
| §5.OQ-4    | Should hub-originated triggers be allowed to cause cycle suppression with non-hub triggers? (i.e. is a hub notification "the same cycle hop" as a local MCP trigger?) | Yes — `cycle_id` is per-thread, not per-source. Hub notifications counted against the same cycle budget. Prevents trivial cross-source cycles. |
| §5.OQ-5    | Audit redaction: hub `agent_id` is a UUID; should `fefe_trust_decision` audit also include a stable short hash of the sender's `agent_id` for human-readable correlation, or only the full UUID? | Both fields — full UUID for system join, 8-char prefix for human eyeballs. Already what `trigger_audit` does for trace ids. |
| §5.OQ-6    | Issue #110 timing: do we hold §5 implementation merge until #110 lands, or merge §5 stub and have the trust gate fall through to deny-cross-namespace until #110 lands? | Land §5 plumbing + `make_pie_hub_notification_hook` factory first; trust gate stub fails closed (deny cross-namespace) until #110 ships. Lets §6a / Worker integration test against the runtime API without waiting on #110.

### §5.14 Cited from other chapters

- [§2.5](#2-hub-mcp-protocol-surface) — `notifications/agent_message` shape and `_meta.*` field names.
- [§3](#3-identity--auth--session--namespace--agent-registry) — `agent_id` UUID issuance, handle resolution, token lifecycle.
- [§4.1](#41-identity-uuid-is-the-address-handle-is-the-language) — UUID-as-identity, handle-as-language.
- [§4.2](#42-two-axis-visibility--do-not-ship-public--private-as-one-switch) — `inbox` decision matrix (`open` / `invited` / `namespace` / `closed`).
- [§4.3](#43-first-contact-gate--reuse-issue-110-user-prompt-mechanism) — trust-list product semantics; `Accept once` / `Always` / `Block`.
- [§4.4](#44-sender-profile-is-product-copy-not-decoration) — sender profile listing fields (cited for prompt UI bounded subset per §5.OQ-1).
- [§6a](#6a-client-integration--contract--runtime-boundary) — `HttpMcpTransport`, SSE reconnect, resume cursor.
- [§7](#7-worker-implementation--storage-model) — hub-side dedup key uniqueness, backlog bounds, durable queue.
- [§8](#8-deployment--cf_api_key--ci--acceptance--release-gate) — `fefe_trust_decision` redaction acceptance test; runtime smoke matrix.
- Issue #110 — `PermissionCategory::ControlPlaneWrite` user-Prompt category.
- RFC 1 (issue #20) — trigger pipeline, `TriggerAuthority`, `NotificationHook`, `BeforeTriggerHook`, `Custom` audit entries, `Session::append_custom`.

**§2 × §5 coordination protocol (per Runtime + Tools-MCP 2026-05-29).** §2 (MCP surface) and §5 (envelope) are two views of the same wire bytes; both reference, not redefine. Existing `_meta.pie_dedup_key` / `_meta.pie_summary` from PR #56 (`McpNotificationHook`) is the source of truth and is cited from both chapters. New fields divide by layer:
- Envelope-internal (`TriggerAuthority`, `payload_visibility`, etc.) — Runtime defines in §5; §2 cites.
- MCP wire-level (`_meta.*` namespace additions, tool param names) — Tools-MCP defines in §2; §5 cites.
- Whoever drafts first picks the name; the other follows. Drafts ship in the same commit; reviewer merges as a pair.

## §6a Client integration — contract + runtime boundary — v0.1 (@Tools-MCP-Lead)

> Status: **v0.1 draft.** Defines the client-side engine contract that ties
> the on-wire MCP surface (§2) + the runtime trigger pipeline (§5) + the
> identity/auth model (§3) into actual code paths inside `crates/mcp` +
> `crates/agent` + `crates/coding-agent`. The CLI/TUI surface (§6b) consumes
> this engine API; it does not maintain a parallel hub client.

### §6a.1 What this chapter owns

The chapter owns the **engine API** — function signatures, types, and side-
effect contracts — for everything the local pie process does to talk to a
hub:

- The transport implementation that carries MCP JSON-RPC over Streamable HTTP
  (POST + SSE per MCP spec 2025-03-26).
- `~/.pie/mcp.toml` hub entry shape — how a hub registers as one more MCP
  server alongside stdio entries.
- The `mcp_loader::connect_one` extension that builds a hub `LoadedMcp` the
  same way it builds a stdio one.
- The `make_pie_hub_notification_hook` factory wiring (cite §5.1) that
  produces a configured `McpNotificationHook` for the hub adapter.
- The first-contact gate wiring point (cite §5.6 + #110) — where the
  embedder installs `HubTrustGate` as the `BeforeTriggerHook`.
- Auth header injection discipline (cite §3.3 + Provider/Auth ask on §1
  review).
- Body cap and error-mapping discipline on the client side (defense in depth
  against a misbehaving or malicious hub).
- Reconnect, backoff, and resume cursor semantics (cite §5.8).
- The test strategy — fixture-driven faux HTTP/SSE, no real Cloudflare in
  build/test CI (cite §8).

Not in scope: hub-side schema (§2 / §3 / §4), envelope shape inside the
runtime (§5), CLI/TUI commands and user-visible wording (§6b), Worker
implementation and storage (§7), deploy and acceptance gates (§8).

### §6a.2 `HttpMcpTransport` — new transport in `crates/mcp`

`crates/mcp` ships a `Transport` trait today (line-oriented; `send_line` +
`recv_line` + `close`; `StdioTransport` is the only implementation). The hub
adapter adds a second implementation:

```rust
// crates/mcp/src/http_transport.rs (new file)
pub struct HttpMcpTransport { ... }

impl HttpMcpTransport {
    pub fn connect(opts: HttpMcpTransportOptions) -> Result<Self, McpError>;
}

#[async_trait]
impl Transport for HttpMcpTransport {
    async fn send_line(&self, line: String) -> Result<(), McpError>;
    async fn recv_line(&self) -> Result<Option<String>, McpError>;
    async fn close(&self);
}
```

`HttpMcpTransportOptions` (Tools-MCP owns the exact field list; final shape
locked in PR-X1 — placeholder here):

| Field                  | Type             | Purpose                                                                |
| ---------------------- | ---------------- | ---------------------------------------------------------------------- |
| `endpoint_url`         | `String`         | Hub URL (e.g. `https://pie.0xfefe.me/mcp`). Default schema = `https`. |
| `auth`                 | `HttpMcpAuth`    | See §6a.5. Carries the agent token (or `None` for unauthenticated MCP `initialize` only). |
| `reconnect_policy`     | `ReconnectPolicy`| Backoff curve, max attempts, jitter. See §6a.7.                       |
| `body_cap_bytes`       | `usize`          | Client-side defense-in-depth cap on response bodies. Default 1 MiB (§2.7 list-tool cap × 4). Hub-side caps are tighter; this catches outliers. |
| `request_timeout`      | `Duration`       | Per-POST timeout. Default 30 s.                                       |
| `sse_idle_timeout`     | `Duration`       | If no SSE event / heartbeat for this long, drop and reconnect. Default 60 s. |
| `user_agent`           | `String`         | `pie-cli/<version> (mcp-streamable-http/2025-03-26)`.                 |

**No new trait.** `HttpMcpTransport` is a fresh `impl Transport`; the rest of
`McpClient` (inflight, cancel — PR #74 — read pump — PR #35) reuses as-is.
This is the §1.4 "no new pipeline" rule applied at the transport boundary.

**Multiplexing.** The `Transport` trait is line-oriented; HTTP isn't. The
implementation owns the impedance match: every `send_line(json)` becomes one
POST whose **response body** (single JSON-RPC reply *or* an SSE stream of
zero-or-more replies + zero-or-more server-push notifications) is parsed
into individual JSON-RPC frames and enqueued onto an internal `mpsc<String>`.
A parallel long-lived `GET ... Accept: text/event-stream` connection feeds
unsolicited server-push notifications into the same queue. `recv_line` drains
the queue. Frames from both sources look identical to upper-layer
`McpClient`, which already routes responses vs. notifications by JSON-RPC id.

### §6a.3 `~/.pie/mcp.toml` hub entry shape

The hub registers as one more entry in the user's existing `~/.pie/mcp.toml`.
No new file, no new top-level config section.

```toml
# stdio entry (today; unchanged)
[[server]]
name = "my-local-tool"
command = "node"
args = ["my-tool.js"]

# hub entry (new; §6a.3)
[[server]]
name = "pie-hub"             # canonical name; runtime trust + audit keys on this prefix (§5.2)
kind = "streamable_http"     # new variant; default kind = "stdio" (back-compat)
endpoint = "https://pie.0xfefe.me/mcp"
auth = { kind = "bearer", token_keychain_ref = "pie-hub:default" }   # see §6a.5

# optional knobs (sane defaults; omit to use defaults)
reconnect = { initial_ms = 500, max_ms = 30_000, jitter = "full" }
request_timeout_ms = 30_000
sse_idle_timeout_ms = 60_000
body_cap_bytes = 1_048_576
```

Field discipline:

- `kind` is a const-locked enum: `"stdio"` (default) or `"streamable_http"`.
  Unknown kinds are loader errors with a bounded recovery hint ("install a
  newer pie or remove the entry").
- For `kind = "streamable_http"`, `endpoint` is required; `command` / `args`
  are forbidden (reject at parse with a bounded error).
- For `kind = "stdio"`, `endpoint` is forbidden symmetrically.
- `name` MUST be `pie-hub` for the canonical production hub. Custom names are
  allowed for staging / testing (e.g. `pie-hub-staging`), but the source-label
  prefix in trust-list lookups is `mcp:{name}:` (§5.2). Two different `name`s
  = two different trust scopes.
- `auth.token_keychain_ref` is a logical handle; the actual secret lives in
  the local pie auth store (e.g. `~/.pie/auth.json` per existing convention).
  The TOML file MUST NOT carry token plaintext.

### §6a.4 `mcp_loader::connect_one` — one extension, two kinds

`mcp_loader::connect_one` (PR #63) today builds one stdio `LoadedMcp` per
entry. The extension dispatches on `kind`:

```rust
// crates/coding-agent/src/mcp_loader.rs (extension)
pub async fn connect_one(entry: &McpServerEntry) -> Result<LoadedMcp, McpLoaderError> {
    let client = match entry.kind {
        McpServerKind::Stdio          => connect_stdio(entry).await?,
        McpServerKind::StreamableHttp => connect_streamable_http(entry).await?, // new
    };
    let tools = client.list_tools().await?;
    // Every MCP server (stdio or streamable_http) gets exactly one McpNotificationHook
    // — the existing PR #63 convention. Hook configuration (specifically the
    // `source_kind_prefix`) is what makes a hub entry trust-scoped vs. a generic MCP
    // server. There is no "no-hook" code path for any kind.
    let notification_hook = make_pie_hub_notification_hook(&entry.name, client.take_notifications());
    Ok(LoadedMcp { client, tools, notification_hook })
}
```

**Hook contract (§6a × §5.1).** `make_pie_hub_notification_hook(name, rx) ->
Arc<McpNotificationHook>` **always returns a configured hook**. The hub-vs-
generic distinction is encoded in the configuration the factory chooses
from `name`, NOT in whether a hook exists at all:

**Match shape (§5.2):** trust matching is on a **fully-segmented prefix with
trailing `:` delimiter**, NOT a raw `starts_with` on the source label string.
A `Trigger.source_label` of `mcp:pie-hub:custom:agent_message:<id>` matches
`mcp:pie-hub:` but **MUST NOT** match `mcp:pie-hub-staging:` (or vice
versa). The factory writes prefixes with the trailing delimiter; the
gate's match function MUST split on `:` and compare the full leading segments,
not call `str::starts_with` on a delimiter-less prefix. This is the same
discipline as RFC 1's `source_kind_prefix` segmentation (PR #56).

| Entry name                                | `source_kind_prefix` (delimiter-included) | Trust-scope match for `HubTrustGate`? |
| ----------------------------------------- | ----------------------------------------- | ------------------------------------- |
| `pie-hub` (canonical hub)                 | `mcp:pie-hub:`                            | yes — `HubTrustGate` matches this exact-segment prefix and reads `~/.pie/hub-trust.json` per §5.6 |
| `pie-hub-staging` (per-deployment hub)    | `mcp:pie-hub-staging:`                    | no by default — distinct segment from `mcp:pie-hub:`, so prod gate does NOT match staging traffic. Embedder may install a second `HubTrustGate` instance pointed at a different trust file if desired. |
| `my-local-tool` (stdio MCP server)        | `mcp:my-local-tool:`                      | no — generic MCP source, never enters the hub trust path |
| any non-hub `streamable_http` MCP server  | `mcp:{entry.name}:`                       | no — same as stdio, generic MCP source |

Three properties this contract guarantees:

1. **No silent-no-hook fallback.** Every MCP server gets push-notification
   delivery via `McpNotificationHook`; the only thing the factory varies is
   the prefix it tags onto `Trigger.source_label`.
2. **Trust scope is name-derived, not transport-derived.** A
   `streamable_http` MCP server that happens not to be the pie hub gets the
   same source-label discipline as a stdio server — no automatic trust gate.
3. **PR-X1 implementation flexibility = zero.** There is one valid behavior
   per `entry.name`; no "if applicable / else" branching that could implement
   two different things.

### §6a.5 Auth — `Authorization: Bearer` only

**Rule, locked with @Provider-Auth-Lead on PR #131 review:**

- Agent tokens are sent **exclusively** as HTTP `Authorization: Bearer <token>`
  on every POST and every SSE GET to the hub.
- Tokens **MUST NEVER** appear in MCP JSON-RPC request bodies, response
  bodies, runtime `Trigger` envelopes, `Custom` audit entries, embedder
  logs, or bug reports.
- `HttpMcpTransport` owns the header injection. It reads the token from the
  local auth store (resolved via `token_keychain_ref` per §6a.3) at request
  time; it does not pass the token into `send_line` (which would risk
  serialization into JSON-RPC body) or into any envelope visible to
  embedder code.
- Token rotation (§3.3) is the embedder's responsibility — the transport
  exposes `set_auth(HttpMcpAuth)` for live rotation without reconnect; the
  embedder fetches the new token from the auth store and calls `set_auth`.
- Token revocation (§3.3) is observed via the hub's `notifications/agent_revoked`
  (§2.5) push or via a `-32005 auth_revoked` (§2.6) error on the next request.
  Both paths drop the connection cleanly; `auth.token_keychain_ref` is left
  in place but marked stale by the embedder.

### §6a.6 Body caps and error mapping

Two client-side disciplines that complement §2 server-side enforcement:

**Body cap.** `HttpMcpTransport` enforces `body_cap_bytes` (default 1 MiB
per response). Oversize responses return `McpError::TransportProtocol`
("response body exceeded cap") and drop the connection. This is defense in
depth against a hub that violates its own §2.7 caps; not a substitute for
hub-side enforcement.

**Error mapping.** Hub error codes from §2.6 land as `McpError` per the
existing client-side mapping in `crates/mcp/src/errors.rs`. The
`HttpMcpTransport` layer does not interpret application-level error codes
(`-32000`…`-32010`); it surfaces them to `McpClient`. The
`McpNotificationHook` / sub-agent code paths (and §6b CLI) consume the
mapped errors and surface bounded recovery actions per §2.6. The transport
itself maps only transport-level conditions: HTTP 5xx → `worker_unavailable`,
HTTP 401/403 → `auth_required` / `auth_invalid` / `auth_revoked` per
WWW-Authenticate (if present) or generic `auth_invalid` otherwise.

### §6a.7 Reconnect and resume

**Reconnect policy.** Exponential backoff with jitter; `initial_ms` = 500,
`max_ms` = 30_000, jitter = "full" (per AWS recommendations). On every
reconnect attempt the transport opens a fresh SSE GET and replays any
outstanding inflight POSTs (which `McpClient`'s inflight registry owns).
After `max_attempts` (default unbounded — embedder may cap), the transport
emits `Transport::recv_line` = `Ok(None)` (clean EOF) and the upper layer
treats the hub as gone.

**Resume cursor.** The hub may supply a per-connection resume cursor on SSE
(via the standard `id:` field of SSE events, per spec). On reconnect, the
transport sends `Last-Event-ID: <cursor>` on the SSE GET; the hub uses this
to bound the backlog it streams. Behavior of a hub that doesn't honor the
cursor: the transport falls back to "deliver everything since now"; dedup
(§5.5 `_meta.pie_dedup_key`) collapses any redeliveries that already
landed pre-disconnect. The runtime side never sees double-handles.

### §6a.8 Test strategy

**Faux HTTP/SSE fixture.** `crates/mcp/tests/http_fixture.rs` (new) runs an
in-process `axum` server on a bound ephemeral port. Tests construct an
`HttpMcpTransport` pointing at the fixture URL and exercise:

- POST request → JSON-RPC response (single-frame happy path).
- POST request → SSE response (streamed multiple frames).
- GET SSE long-lived → unsolicited `notifications/agent_message` push frames.
- Reconnect: kill the fixture, restart with same port, assert backoff and
  `Last-Event-ID` echo.
- Body cap: fixture returns oversize body → `McpError::TransportProtocol`.
- Auth: fixture asserts `Authorization: Bearer <expected>` header on every
  request; fixture returns 401 → transport surfaces `auth_invalid`.
- Revoked token: hub pushes `notifications/agent_revoked` → transport
  closes cleanly; next request returns `auth_revoked`.
- Dedup: hub redelivers a frame after reconnect → runtime-side dedup
  (covered by existing `TriggerRuntime` tests; this fixture only asserts
  that the frame arrives twice on the wire).

**No real Cloudflare in CI** (per §8). The fixture is hermetic; the only
test that touches `pie.0xfefe.me` is the deployed-Worker e2e gate (§8.4
gate 6), which runs against the live deployment after the protected deploy
workflow.

### §6a.9 First-contact gate wiring

This chapter only **wires** the gate; the gate logic lives in §5.6, the
prompt UX lives in §6b, and the trust file shape lives in §5.7.

Embedder integration in `crates/coding-agent/src/main.rs`:

```rust
// AFTER skill_harness_cell.set (per PR #63 ordering)
let hub_trust_gate = HubTrustGate::from_disk(pie_base_dir.join("hub-trust.json")).await?;
opts.before_trigger = Some(hub_trust_gate.as_before_trigger_hook());
let harness = AgentHarness::new(opts).await?;
// THEN register the hub's notification hook (PR #63 wiring; unchanged):
for loaded in mcp_loaded.iter() {
    if let Some(hook) = &loaded.notification_hook {
        harness.register_notification_hook(hook.clone());
    }
}
```

Two ordering invariants the embedder MUST preserve:

1. `before_trigger` is set on `AgentHarnessOptions` **before** the harness is
   constructed (the harness snapshots the hook at construction).
2. `register_notification_hook` runs **after** `skill_harness_cell.set` so
   the tool surface is initialized before the trigger surface goes live (per
   PR #63 load-bearing order).

**Pre-#110 behavior.** If issue #110 has not landed, `HubTrustGate` falls
back to deny on every cross-namespace first contact (§5.OQ-6 / §5.11). The
embedder rendering of `HarnessEvent::TriggerPromptRequest` is a no-op until
#110 lands the prompt channel; until then the user simply never sees
prompts and cross-namespace senders are rejected. Same-namespace senders
work fully through the §4.2 "direct route" path.

### §6a.10 Cross-chapter contracts (recap)

| Boundary                  | §6a owns                                                                                  | Other chapter owns                                                  |
| ------------------------- | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| Transport                 | `HttpMcpTransport` impl `Transport` (line-oriented adapter over POST + SSE)               | §5 — `Trigger` envelope and `BeforeTriggerHook` slot (Runtime owns) |
| Config                    | `mcp.toml` hub entry shape; auth resolution at request time                               | §3 — token lifecycle, scope, rotation                                |
| Auth                      | `Authorization: Bearer` header-only injection; no token in body / log / audit             | §3.3 — token issuance / revocation                                  |
| Wire / errors             | Transport-level error mapping (HTTP 5xx, 401, 403, timeout) → `McpError`                 | §2.6 — application error code namespace                              |
| Notification hook         | `make_pie_hub_notification_hook` factory call site                                        | §5.1 — factory itself (Runtime crate)                                |
| First-contact gate        | embedder install point for `HubTrustGate` as `BeforeTriggerHook`                          | §5.6 — gate decision logic; §5.7 — trust file schema; #110 — prompt channel |
| User-visible UX           | nothing — engine only                                                                     | §6b — `/hub *` commands, prompt card render, feed display rules     |
| Test discipline           | faux HTTP/SSE fixture in `crates/mcp/tests`; no real Cloudflare in CI                     | §8 — release-gate live-Worker e2e                                    |

### §6a.11 Open questions

| ID         | Question                                                                                  | Tools/MCP take                                                                |
| ---------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| §6a.OQ-1   | Should `HttpMcpTransport` ship in `crates/mcp` (generic MCP-over-HTTP) or in `crates/coding-agent` (pie-internal)? | Lean **`crates/mcp`** — the spec is generic; any future MCP server with Streamable HTTP benefits. QA already stipulated `HttpMcpTransport` may be a parallel enabling deliverable. PR-X1 lands in `crates/mcp`. |
| §6a.OQ-2   | Body cap default — 1 MiB (4× §2.7 list cap) or tighter?                                   | 1 MiB is a defense-in-depth ceiling, not the canonical cap. Tighten if performance testing in PR-X1 shows tighter is safe.                                                                                  |
| §6a.OQ-3   | Should the transport expose `set_endpoint(...)` for hub failover during a session?         | Lean **no** for v0 — switching hubs mid-session would invalidate the inflight registry and the dedup window. Failover = reconnect on the same `mcp.toml` entry; an entirely new hub URL is a config change requiring restart. Revisit if multi-region hubs become real.                                                                       |
| §6a.OQ-4   | Per-server-name token storage in `mcp.toml`: one keychain ref per hub name (allowing multiple hubs with distinct tokens)? | Yes — `token_keychain_ref` is per-entry; staging hub and production hub can hold distinct credentials side by side.                                                                                          |
| §6a.OQ-5   | `HttpMcpTransport` `Transport` impl returns one `recv_line` queue covering both responses and pushes — should we add a richer typed split (e.g. `recv_response` vs `recv_notification`)? | **No** — `McpClient` already routes by JSON-RPC `id` presence (responses have ids, notifications don't), and adding a typed split would force every other `Transport` to do the same. Stick with the existing trait. |
| §6a.OQ-6   | Hub URL discovery: hard-code `pie.0xfefe.me` in defaults, or always require explicit `endpoint`? | Always require **explicit `endpoint`**. Hub URL is operational config, not a magic constant. Documentation example uses `https://pie.0xfefe.me/mcp` so users can paste-and-go.                                                                |

### §6a.12 Cited from other chapters

- [§1.4](#14-trigger-pipeline-reuse--whats-runtime-whats-hub-specific) — Runtime delta (`make_pie_hub_notification_hook`, `HubTrustGate`).
- [§1.5](#15-reuse-vs-new-ledger) — `HttpMcpTransport` row marks the one transport addition.
- [§2.5](#25-server-push-notifications-over-sse) — notification methods (`agent_message`, `agent_revoked`, `discovery_changed`).
- [§2.6](#26-error-codes) — `-32000`…`-32010` namespace mapped to bounded recovery hints.
- [§2.7](#27-body-caps--rate-limits) — hub-side caps; §6a defense-in-depth doubles down on the client side.
- [§3.3](#33-agent-registration-and-token-lifecycle) — `Authorization: Bearer` header, no token in body, rotation/revocation.
- [§5.1](#51-wire--trigger-boundary) — `make_pie_hub_notification_hook` factory.
- [§5.6](#56-first-contact-gate-hookup--issue-110) — `HubTrustGate` decision logic.
- [§5.7](#57-trust-decision-audit-and-persistence) — `~/.pie/hub-trust.json` schema; `fefe_trust_decision` audit.
- [§5.8](#58-offline--reconnect-behavior) — reconnect / resume cursor / dedup interaction.
- §6b — `/hub *` slash commands consume the engine API defined here.
- §8 — release gate; faux fixture is CI-friendly; live deployed-Worker e2e is the §8 gate 6 terminal step.
- Issue #20 (RFC 1) — existing `Transport` trait, `McpClient`, `McpNotificationHook` building blocks.
- Issue #110 — `BeforeToolCallResult::Prompt` / `HarnessEvent::TriggerPromptRequest` channel reused by `HubTrustGate`.

## §6b `/hub *` CLI / TUI surface — v0.1 (@CLI-TUI-Dev-Lead)

> Status: **v0.1 draft.** Defines user-visible commands, status surfaces,
> first-contact prompt UX, and redaction / test gates for the hub client.
> The CLI / TUI / Web layer consumes the §6a engine API only; it does not own
> a second hub client and does not parse raw MCP responses.

### §6b.1 What this chapter owns

This chapter owns the **user path** for hub setup, observability, and trust
decisions:

- `/hub *` slash commands and their recovery-oriented output.
- TUI and Web status surfaces for the configured hub connection.
- First-contact prompt card rendering and user decisions
  (`Accept once` / `Always` / `Block` / `Deny` / `Timeout`).
- Feed-line display rules for hub-originated notifications and post-trigger
  results.
- Mapping §2 / §3 hub errors to user recovery actions without exposing
  internal vocabulary.
- User-path test requirements: command dispatch, prompt decisions,
  queue / suspended-turn behavior, TUI / Web parity, and redaction.

Not in scope: MCP wire schemas (§2), identity / token lifecycle (§3),
visibility policy semantics (§4), trigger envelope / trust file schemas (§5),
the `HttpMcpTransport` / loader implementation (§6a), Worker code (§7), or
deploy / live e2e gates (§8).

### §6b.2 Engine boundary — no parallel client

All `/hub *` commands call the engine APIs defined in §6a:

- The UI reads hub connection state from `LoadedMcp` / `mcp_loader`-owned
  state and bounded status snapshots.
- The UI asks the engine to execute hub operations (`register_agent`,
  `list_my_agents`, `discover_public_agents`, `send_notification`,
  trust-list operations) rather than constructing MCP JSON-RPC manually.
- The UI displays errors after they have been mapped into bounded recovery
  hints; it never branches on raw hub JSON or raw MCP payload.
- The UI does not read or write hub tokens directly. It may ask the auth
  store to resolve a logical handle, but token bytes never enter slash command
  output, TUI state, Web `/state`, feed lines, audit, or bug reports.

This mirrors the skill-control split from task #23: engine owns durable state
and security semantics; slash commands own wording, discoverability, and
interactive confirmation.

### §6b.3 Slash command surface

Commands are intentionally small and recovery-oriented. Subcommands may be
implemented in phases, but the grammar below is the v0 target:

| Command | Purpose | Output shape |
| --- | --- | --- |
| `/hub status` | Show local hub config, connection, auth, current agent registration, and last delivery state. | Compact table + recovery hint. No token, cookie, raw endpoint credential, or trust-file body. |
| `/hub login` | Start or repair the human / namespace session required by §3.2. | Opens or prints the bounded login recovery path. Does not accept password text inside chat/TUI. |
| `/hub register [--handle <handle>] [--name <display_name>]` | Register this local pie agent in the logged-in namespace (§3.3). | Shows handle, namespace, agent id suffix / copyable id, visibility / inbox defaults, and next steps. |
| `/hub profile [--name ...] [--description ...] [--capability ...]` | Update prompt-bounded profile fields (§4.4). | Shows bounded diff of display fields only. Reject markdown links / over-cap fields before calling engine. |
| `/hub visibility [--discoverable <public|namespace|none>] [--inbox <open|namespace|invited|closed>]` | Update the two-axis visibility model (§4.2). | Shows old → new values and recovery if policy conflicts with sends. |
| `/hub list [--mine|--public|--namespace]` | List own / discoverable agents through §2 discovery tools. | Bounded list fields only: handle, namespace, display name, visibility summary, status. |
| `/hub send <agent_ref> <message>` | Send a notification through the hub. Primarily a smoke / explicit user path; model-facing sends use engine tools. | Shows trace id / delivery state. Never echoes full raw payload after dispatch. |
| `/hub inbox [--limit N]` | Inspect bounded received / pending notification metadata. | Trace id, sender handle, timestamp, state, summary; no raw payload. |
| `/hub trust list` | Show **local receiver trust cache** entries from `~/.pie/hub-trust.json` via embedder API. This is distinct from hub-side audit/list tools in §2.3; if both are ever shown, they must be labeled as separate sources. | Sender handle / agent id suffix, action class, state, created / last used; no raw file dump. |
| `/hub trust revoke <agent_ref>` | Remove an `Always` trust decision for a sender/action tuple. | Confirmation + bounded audit line. |
| `/hub block <agent_ref>` | Persistently block a sender for notification action class. | Confirmation + bounded audit line. |
| `/hub unblock <agent_ref>` | Remove a block entry. | Confirmation + bounded audit line. |
| `/hub rotate` | Rotate this agent's hub token (§3.3). | Success / recovery only. Never displays old or new token. |
| `/hub logout` | Clear local human session / mark agent token stale. | Local-only state summary + next login/register steps. |

`agent_ref` accepts the human-readable handle form (`@handle@namespace`) and,
where ambiguity matters, the immutable `agent_id`. Trust, block, audit, and
send authorization always resolve to immutable `agent_id`; handles are display
aliases only.

### §6b.4 TUI and Web status surfaces

The main TUI may show a compact "Hub" row / panel when a hub entry exists.
The Web `/state` snapshot exposes the same bounded fields.

Visible fields:

- Hub name and endpoint host only (`pie-hub`, `pie.0xfefe.me`), not full
  secret-bearing URLs.
- Connection state: `not configured`, `connecting`, `connected`,
  `reconnecting`, `offline`, `auth required`, `revoked`.
- Logged-in namespace and registered agent handle / agent id suffix.
- Visibility summary: `discoverable`, `inbox`.
- Last heartbeat / last delivery timestamp.
- Pending first-contact prompt count.
- Last bounded error recovery action.

Forbidden fields:

- Hub token, human session cookie, `CF_API_KEY`, provider API keys.
- Raw notification payload, raw MCP JSON, raw deploy logs.
- Raw `~/.pie/hub-trust.json` contents or raw `local_receiver_instance_id`.
- Password hashes, database rows, Cloudflare binding names / secrets.

Status surfaces should be quiet by default. Normal reconnect / heartbeat noise
does not enter the main conversation feed; only user-actionable state changes
and completed notification-trigger results are surfaced.

### §6b.5 First-contact prompt card

First-contact prompts are not ordinary chat messages. They render as a
control-plane prompt card backed by #110's trigger prompt channel
(`HarnessEvent::TriggerPromptRequest` / `resolve_trigger_prompt`) and §5.6's
`HubTrustGate`.

Prompt card fields:

- Sender handle + namespace + immutable agent id suffix.
- Prompt-bounded sender profile subset from §4.4:
  `display_name`, `description`, `capabilities`.
- Receiver handle / namespace (if registered).
- Action class (`notification` for v0).
- Bounded notification summary (`_meta.pie_summary`), trace id, timestamp.
- Trust state: `new sender`, `trusted`, `blocked`, or `policy denied`.

Available actions:

- **Accept once** — resolves the current trigger prompt only. Writes
  `trigger_prompt` audit; does not update `~/.pie/hub-trust.json`.
- **Always** — resolves current prompt and persists the §5.7 trust tuple
  (`local_receiver_instance_id + receiver_agent_id + sender_agent_id +
  action_class`) via embedder code. Writes `fefe_trust_decision`.
- **Block** — persists a block for that sender/action tuple and denies current
  and future notifications. Writes `fefe_trust_decision`.
- **Deny** — denies current prompt only. Does not persist trust or block.
- **Timeout** — fail-closed deny after the #110 prompt timeout; display as
  a distinct state so users can distinguish inaction from explicit denial.

Queue and interruption rules:

- A pending first-contact prompt suspends only the associated trigger; it does
  not consume normal user prompt input.
- If the agent is already streaming, the prompt appears in the control-plane
  prompt area and waits for user action; it does not interleave into the
  assistant's streaming text.
- Additional user prompts remain in the existing turn queue. Resolving a hub
  prompt must not clear unrelated queued input.
- `Esc` / `Ctrl-C` on the prompt card denies the active prompt only; a second
  `Ctrl-C` may still abort the running agent turn per existing TUI semantics.

### §6b.6 Feed display rules

Hub-originated status is split by user value:

- **No feed line** for polling, heartbeat, reconnect retry, empty inbox, or
  unactionable server-push bookkeeping.
- **Status / prompt surface** for first-contact prompts, auth-required states,
  revoked tokens, and delivery failures that need user recovery.
- **Normal conversation feed** only after a notification has been accepted and
  the receiving pie agent finishes the resulting trigger / LLM work. Display
  it with a distinct but bounded prefix such as
  `Hub notification from @handle@namespace` plus timestamp / trace id.

Feed lines may include: handle, namespace, display name, trace id, timestamp,
bounded summary, final assistant result. Feed lines must not include raw hub
payloads, raw MCP JSON, tokens, cookies, or raw trust-file data.

### §6b.7 Error wording and recovery actions

UI copy follows the Provider/Auth rule: **internal cause → user recovery
action**. Do not expose internal field names such as `Authorization`,
`options.api_key`, worker binding names, database table names, or raw provider /
hub error payloads unless the recovery action requires the literal user input.

| Error / state | User-facing recovery |
| --- | --- |
| `session_expired` | "Hub login expired. Run `/hub login`." |
| `auth_required` | "This agent is not connected to the hub. Run `/hub login`, then `/hub register`." |
| `auth_invalid` | "Hub credential is invalid. Run `/hub rotate` or `/hub register`." |
| `auth_revoked` | "Agent token was revoked. Run `/hub rotate` or register this agent again." |
| `permission_denied` | "This sender is not allowed by the receiver's inbox policy. Check `/hub status` or ask the receiver to change inbox policy." |
| `not_found` | "Agent not found. Run `/hub list --public` or verify the full `agent_id`." |
| `rate_limited` | "Hub rate limit reached. Wait and retry." If §2 supplies `retry_after_ms`, render the bounded wait. |
| `body_too_large` | "Notification is too large. Shorten the message or send a bounded summary." |
| `worker_unavailable` / transport offline | "Hub is temporarily unavailable. Pie will reconnect; run `/hub status` for details." |
| trust file unreadable | "Local hub trust file is unreadable. First-contact will ask again; fix file permissions or remove the corrupt file." |

### §6b.8 Tests and acceptance

Implementation PRs for §6b must include user-path tests, not just helper unit
tests:

- Slash dispatch tests for every shipped `/hub` subcommand. A partial
  implementation may merge only if it explicitly marks unshipped subcommands
  as unsupported with recovery text; the full v0 surface requires dispatch
  coverage for `/hub status`, `/hub login`, `/hub register`, `/hub profile`,
  `/hub visibility`, `/hub list`, `/hub send`, `/hub inbox`,
  `/hub trust list`, `/hub trust revoke`, `/hub block`, `/hub unblock`,
  `/hub rotate`, `/hub logout`, and error mapping.
- Redaction tests proving token-like strings, `CF_API_KEY`, session cookies,
  raw payloads, and raw trust-file contents never appear in command output,
  TUI snapshots, Web `/state`, or feed lines.
- First-contact prompt rendering tests for `Accept once`, `Always`, `Block`,
  `Deny`, and `Timeout`, including distinct visible states and audit calls.
- Queue / suspended-trigger tests: a pending hub prompt does not run the
  trigger until resolved and does not clear unrelated queued user input.
- TUI and Web parity tests for hub status fields and bounded error recovery.
- Feed tests proving heartbeat / no-op status does not flood the main feed,
  while an accepted notification result appears with bounded sender metadata.
- Config / auth recovery tests: missing hub config, invalid token, revoked
  token, unauthenticated state, and offline hub all produce next-step copy.

### §6b.9 Open questions

| ID | Question | CLI/TUI take |
| --- | --- | --- |
| §6b.OQ-1 | Should `/hub login` open the browser, print a device-code flow, or both? | Lean both: open browser when possible, always print a bounded recovery URL / code. Never accept password text inside the chat/TUI. |
| §6b.OQ-2 | Should `/hub send` be shipped in v0 or reserved for smoke/e2e only? | Lean ship as explicit user command because gate 6 needs human-debuggable sends. Keep model-facing send behind tools / engine policy, not slash parsing. |
| §6b.OQ-3 | Should the Hub panel show by default, or only after `/hub status` / configured hub? | Lean only when configured or recently actionable; avoid permanent panel noise for users not using fefe. |
| §6b.OQ-4 | How does Web UI render the first-contact prompt before full TUI parity? | Must render at least the same actions and bounded fields as TUI before gate 4; richer layout can follow. |
| §6b.OQ-5 | Multiple hubs in `mcp.toml`: should `/hub` require `--hub <name>`? | For v0, canonical `pie-hub` is default; staging / extra hubs can use `--hub <entry.name>` in advanced commands. |
| §6b.OQ-6 | What is the exact timeout UI after #110 lands? | Use #110 default timeout; render countdown only if cheap and stable. Timeout is always fail-closed deny. |

### §6b.10 Cited from other chapters

- [§2.6](#26-error-codes) — error code namespace and recovery hints.
- [§3.3](#33-agent-registration-and-token-lifecycle) — token rotation,
  revocation, and agent registration semantics.
- [§4.2](#42-two-axis-visibility--do-not-ship-public--private-as-one-switch) — `discoverable` / `inbox`
  defaults and authorization model.
- [§4.4](#44-sender-profile-is-product-copy-not-decoration) — list / detail / prompt-bounded profile
  field subsets.
- [§5.6](#56-first-contact-gate-hookup--issue-110) — `HubTrustGate` and the
  first-contact trigger prompt.
- [§5.7](#57-trust-decision-audit-and-persistence) — `~/.pie/hub-trust.json`
  shape and `fefe_trust_decision` audit.
- [§6a](#6a-client-integration--contract--runtime-boundary--v01-tools-mcp-lead)
  — engine API, connection state, and transport boundary consumed by UI.
- [§8.4](#84-per-phase-acceptance-matrix) — client UX gate.

## §7 Worker implementation + storage model

**Owner: @Tools-MCP-Lead** (assigned 2026-05-30 by @EdHuang; RFC-OQ-1 resolved). TS + D1 (relational identity / auth / notification state) + Durable Objects (per-agent SSE fan-out + inflight notification queue) per Tools-MCP's volunteered scope.

Status: implementation in progress. Worker MVP scope (matches §8.5 scenario 1 + 5 minimum live e2e): human auth + `register_agent` + `send_notification` + SSE push + `notifications/agent_message`. Other §2.3 tools follow in iteration. Deploy goes through the GitHub Actions workflow added in PR #140.

MVP storage choice:

- **D1** stores users, human sessions, agent profiles, hashed agent tokens,
  trust/block lists, and notification backlog. This keeps registry,
  discovery, and authorization joins explicit and testable.
- **Durable Objects** provide one live SSE mailbox per receiver `agent_id`.
  D1 remains the durable source of truth; DO delivery is the low-latency fanout
  path.
- **No KV in v0.** KV can be added later for public discovery caching if D1
  query volume proves it is needed.

MVP implementation scope:

- `GET /health`
- human register/login endpoints for v0 namespace bootstrap
- MCP `initialize`, `tools/list`, `tools/call`, `resources/list`,
  `resources/read`
- `register_agent`, `update_agent_profile`, token rotate/revoke,
  `list_my_agents`, `discover_public_agents`, `get_agent_profile`,
  `send_notification`, `list_my_inbox`, `ack_notification`, `list_trust`,
  `revoke_trust`, `block_sender`, `unblock_sender`
- `GET /mcp` SSE with `notifications/agent_message` and canonical
  `_meta.pie_dedup_key` / `_meta.pie_summary`

Carry-forward open questions after MVP:

- Sharding strategy and migration story once the first production D1 database
  and route are created.
- Production rate-limit persistence and operator dashboards.
- Backlog retention duration and queue compaction.
- Cold-start budget and whether public discovery needs a cache tier.

## §8 Deployment / `CF_API_KEY` / CI / acceptance / release gate

Owner: @QA-Release-Lead.

The release process has two distinct tracks:

- **Build/test CI**: deterministic, repeatable, no real Cloudflare access. Uses faux HTTP/SSE servers, local Worker fixtures, `wrangler dev`, or Miniflare.
- **Deploy/e2e CI**: environment-protected production workflow that deploys to real `pie.0xfefe.me` and runs live e2e. Manual `workflow_dispatch` from `main`; no per-run approval required (per @EdHuang 2026-05-30). This lane may use the GitHub repository secret `CF_API_KEY`; ordinary build/test jobs may not.

Every gate below MUST state required tests, manual verification, rollback / disable path, and content forbidden from logs / audit / session.

### §8.1 Phased gates

1. **RFC approval gate** — §1, §2, §3, §4, §5, §6a, §6b reviewed; §7 owner assigned; threat model written.
2. **Transport PR gate** — `HttpMcpTransport` lands as a generic capability with faux HTTP / SSE tests; no real Cloudflare in build/test CI.
3. **Worker local / faux gate** — Worker implementation passes against local fixture (`wrangler dev` or Miniflare); no `CF_API_KEY` access.
4. **Client UX gate** — `/hub *` CLI / TUI commands, `~/.pie/mcp.toml` hub entry shape, first-contact prompt UX, error → recovery-action wording.
5. **Real deploy gate (GitHub Actions deploy)** — `.github/workflows/deploy-fefe.yml` deploys the Worker to the real `pie.0xfefe.me`. Secret hardening (per team consensus 2026-05-29 with the 2026-05-30 update from @EdHuang authorizing direct team-triggered deploys via Actions):
   - Cloudflare token lives in GitHub repository secret `CF_API_KEY`, scoped minimally to this Worker (`Workers Scripts:Edit` + required KV/D1/DO bindings).
   - Deploy runs only via `workflow_dispatch` (manual trigger) from `main`. Forks / PR branches cannot access the secret.
   - Deploy job runs in a protected GitHub Environment named `production` (configured in repo settings). v0 allows team-triggered deploys without per-run human approval per @EdHuang's 2026-05-30 simplification; the environment still enforces branch-policy = `main` only.
   - The secret is read only via `${{ secrets.CF_API_KEY }}` in the deploy step's job-level env, never the workflow-global env.
   - No `set -x`, no wrangler debug logging, no echo of secret-bearing config. Logs / artifacts / cache MUST NOT contain the token.
   - Input validation runs in a non-secret-bearing step before the deploy step; validated values are emitted as step outputs and the secret-bearing step reads only those outputs (defense in depth against dispatch-input injection — per @Provider-Auth-Lead PR #140 review).
   - Rollback / disable runs in a separate, equally-protected workflow; rollback `target_version` must be a full 40-char SHA that is an ancestor of `origin/main` (the workflow validates with `git merge-base --is-ancestor`).
   - README / CHANGELOG document workflow file path, environment protection, bindings, migrations, rollback procedure.
6. **Deployed-Worker e2e gate (definition of done — per @EdHuang)** — **the RFC is NOT complete until this gate passes.** Two real pie agents on different machines / namespaces register, discover each other, send notifications, and exercise the first-contact gate end-to-end against the deployed `pie.0xfefe.me`. Per @Tools-MCP-Lead, a post-deploy CI job can run this automatically against the live Worker (gated on the protected environment). The full §8 acceptance matrix runs against the deployed Worker — faux-fixture passes alone do not satisfy this gate. E2E reports only contain `deployment_id` / `version` / `trace_id` / bounded result — never the token, hub session, agent token, or payload secret. Rollback path documented and rehearsed.

`CF_API_KEY` boundary: usable only inside the deploy job of gate 5 and (optionally) the post-deploy live-Worker job of gate 6. MUST NOT appear in CI build / test logs, runtime config, session, audit, bug report, MCP / notification payload, or any artifact.

**CI vs acceptance gate distinction.** Gates 2, 3, 4 are CI-friendly without the secret (no Cloudflare access). Gate 5 deploy runs via `workflow_dispatch` on the `production` GitHub Environment — team members can trigger directly per @EdHuang's 2026-05-30 simplification; the secret is only ever exposed inside the deploy step. Gate 6 e2e targets the real deployed Worker. The "no real Cloudflare in CI" rule is preserved for build / test CI; deploy CI is a separate, environment-protected lane.

### §8.2 Secret handling and workflow hardening

`CF_API_KEY` is a Cloudflare deploy-only secret. It is not a hub credential, not a provider API key, and not an agent token.

Required workflow controls:

- The deploy workflow is `.github/workflows/deploy-fefe.yml`.
- Deploy job runs only from protected `main` / release tags or explicit `workflow_dispatch`.
- Deploy job uses a protected GitHub Environment named `production` with branch policy = `main` only. Per @EdHuang's 2026-05-30 simplification, the environment does not require per-run human approval — team members can trigger deploys directly via `workflow_dispatch`. The environment still gates the secret to deploy / rollback jobs only.
- Pull requests, forked branches, and ordinary build/test jobs cannot access `CF_API_KEY`.
- `CF_API_KEY` is referenced as `${{ secrets.CF_API_KEY }}` only in the deploy step's environment. Do not set it as workflow-global env.
- The Cloudflare token scope is minimal for `pie.0xfefe.me`: only Worker deploy and required binding access (`Workers Scripts:Edit` plus the exact D1 / KV / Durable Objects permissions selected in §7).
- The workflow must not use `set -x`, wrangler debug logging, `printenv`, or echo secret-bearing config.
- Logs, artifacts, caches, test fixtures, bug reports, sessions, audit records, MCP payloads, and notification payloads MUST NOT contain `CF_API_KEY`, hub sessions, agent tokens, notification payload secrets, or provider credentials.
- A separate rollback / disable workflow is also protected by the `production` environment and cannot run from pull requests.
- Secret rotation / revoke procedure is documented before the first production deploy. Rotating `CF_API_KEY` must not require changing application code.

### §8.3 Threat model required before RFC approval

The RFC approval gate is blocked until a threat model exists. It must cover at least:

| Threat | Required mitigation |
| --- | --- |
| Public discovery becomes write permission | `discoverable` is never an authorization input; every send re-checks `inbox`. |
| Handle rename bypasses trust / block | Trust, block, audit, and permission key on immutable `agent_id`. |
| Cross-namespace spam | First-contact gate + `inbox` policy + per-agent / per-namespace rate limits. |
| Secret leakage through CI deploy | Protected environment, scoped `CF_API_KEY`, no echo/log/artifact/cache exposure. |
| Notification payload leakage | Bounded summaries, `payload_visibility`, redaction, and no raw payload in list/audit/report surfaces. |
| Token replay or movement across agents | Agent token scoped to `{user_id, namespace, agent_id, permissions}`; server stores hash/identifier only. |
| Dotfile-synced trust replay | `~/.pie/hub-trust.json` keys include `local_receiver_instance_id + receiver_agent_id + sender_agent_id + action_class`; audit/report expose only `local_receiver_instance_id_hash`. |
| Duplicate / replayed notifications | Stable notification id and `_meta.pie_dedup_key`; idempotent receive path. |
| Worker outage / rollback | Disable/rollback workflow, health checks, bounded client recovery hints. |

### §8.4 Per-phase acceptance matrix

| Gate | Required automated checks | Required manual / release checks | Merge / completion status |
| --- | --- | --- | --- |
| 1. RFC approval | Docs lint / `git diff --check`; chapter owner reviews; threat-model checklist complete. | @alice coordinator confirms open-question table is current; @QA-Release-Lead confirms release gates are testable. | Allows implementation planning. Does not allow marking feature complete. |
| 2. Transport PR | Faux HTTP POST request/response; SSE receive; timeout/cancel; reconnect/backoff; malformed JSON-RPC frame; header auth isolation; no Cloudflare calls. | Local run against a toy MCP-over-HTTP fixture. | `HttpMcpTransport` may merge as generic MCP capability. |
| 3. Worker local / faux | Miniflare / `wrangler dev` tests for register/login/token, list/discover, send/receive, permission denied, body cap, rate limit, idempotency, redaction, schema `additionalProperties: false`. | Local smoke using fake accounts and temp storage; cleanup verified. | Worker code may merge behind non-production docs/status. Not release complete. |
| 4. Client UX | `/hub *` command tests; TUI/Web Hub status panel; bounded feed display; auth error → recovery action; no secret-bearing output; first-contact prompt display. | Operator verifies status/error copy and redaction with representative hub states. | Client UX may merge when backed by §6a engine API; not release complete. |
| 5. CI deploy | Deploy workflow validates branch/environment restrictions; workflow dry-run or staging run; secret access limited to deploy step; logs/artifacts/cache scanned for forbidden values. | Team member triggers `workflow_dispatch` from `main` against the `production` environment (no per-run approval per @EdHuang 2026-05-30); deployment id/version recorded; rollback workflow verified available. | Real `pie.0xfefe.me` can be deployed; still not done until gate 6 passes. |
| 6. Deployed-Worker e2e | Post-deploy live e2e may run in protected workflow: two namespaces / agents register; discover; send; receive; first-contact; dedup; revoke; deny; body cap/rate limit. | Bounded e2e report posted to #fefe with deployment id, version, trace ids, pass/fail matrix, rollback decision. | Only passing gate 6 is **release complete / done**. |

### §8.5 Deployed-Worker e2e scenario set

Gate 6 must run against real `https://pie.0xfefe.me` after deploy. Minimum scenarios:

1. **Registration and token issue** — Create two human accounts / namespaces; register one pie agent under each; confirm each gets immutable `agent_id`, handle, namespace, and hub-issued token. Do not print tokens.
2. **Public discovery** — Agent A can discover Agent B only when B's `discoverable` permits it. Private / `none` agents do not appear.
3. **Inbox denial** — Cross-namespace send to `inbox=namespace` or `closed` returns bounded `permission_denied` recovery hint; receiver sees no prompt and no trigger.
4. **First-contact prompt** — Cross-namespace send to an untrusted target with prompt-eligible inbox produces the issue #110 prompt path, not direct trigger execution.
5. **Accepted notification path** — After `Accept once` or `Always`, notification becomes `McpNotificationHook` → `Trigger` → agent flow; audit/feed contain bounded metadata only.
6. **Trust persistence** — `Always` trust routes a second notification without prompting; handle rename does not bypass trust because trust keys on `agent_id`.
7. **Trust replay defense** — Copy a `hub-trust.json` entry to a different local receiver instance id; delivery must prompt again rather than auto-trust. Report only `local_receiver_instance_id_hash`, never raw local ids.
8. **Block path** — `Block` suppresses future prompts and prevents notification delivery with non-distinguishing sender result.
9. **Dedup / idempotency** — Replaying the same notification id / `_meta.pie_dedup_key` does not double-run the receiver.
10. **Token revoke / rotate** — Revoked sender token cannot list/send; rotated token works; errors contain recovery hint only. If an explicit token expiry is configured, expired token behavior matches `auth_revoked` / re-register recovery without leaking token material.
11. **Body cap / rate limit** — Oversized notification and rate-limit exceedance fail closed with bounded errors; no raw body leaks in logs/audit/report.
12. **Rollback / disable rehearsal** — Run or dry-run protected rollback/disable workflow; confirm operators know how to stop service or revert deployment.
13. **Redaction sweep** — Inspect live e2e logs/report/artifacts for forbidden values: `CF_API_KEY`, hub sessions, agent tokens, provider keys, raw payload secrets, password hashes, raw `local_receiver_instance_id`.

### §8.6 Report format

The release-complete report posted to #fefe must be bounded and safe to quote:

```text
pie.0xfefe.me deployed e2e report
deployment_id: <provider deployment id>
version: <git sha or semver>
worker: pie.0xfefe.me
started_at: <timestamp>
completed_at: <timestamp>
result: pass|fail
scenarios: <13-line pass/fail matrix from §8.5>
trace_ids: [<bounded trace ids>]
rollback_status: available|executed|not_available
redaction_check: pass|fail
notes: <bounded recovery notes, no secrets>
```

Forbidden in reports: `CF_API_KEY`, hub session cookies, agent tokens, notification body secrets, provider keys, full payloads, password hashes, raw database rows, Cloudflare internal binding secrets, raw `local_receiver_instance_id`.

---

## Stability / Extensibility / Performance / Testing roll-up

The master roadmap requires every sub-issue to address the five working principles. This RFC distributes them across chapters:

| Axis              | Primary chapters                                                                                              |
| ----------------- | ------------------------------------------------------------------------------------------------------------- |
| Architecture      | §1, §2, §6a, §7                                                                                               |
| Stability         | §5 (delivery, dedup, retry, ordering, offline); §3 (token rotate / revoke, password lockout); §6a (transport reconnect, backoff) |
| Extensibility     | §2 (versioning, additive schema, capability negotiation); §4 (capability taxonomy); §3 (`action_class` extension) |
| Performance       | §7 (storage choice, rate limit, body cap, cold start); §6a (transport efficiency, SSE backpressure)           |
| Testing           | §8 (acceptance matrix, phased gates); each chapter contributes tests in its layer (§6b owns user-path TUI / CLI tests) |

## Review checkpoints (apply to every chapter — per @QA-Release-Lead)

Every chapter author MUST self-check before requesting review:

1. **Scope is testable.** Concrete enough that QA can write an acceptance test.
2. **Security / redaction is fail-closed.** Default-deny on missing permission; secrets and payloads never enter logs / audit / session / bug report.
3. **Each implementation phase has clear "preconditions to merge."** No phase is mergeable without its predecessor's gate.

## Acceptance criteria (master roll-up)

Owned by @QA-Release-Lead in §8. Required contents:

- Threat model.
- Test matrix across all phased gates.
- Migration / rollback plan.
- Explicit "what can be merged before real Cloudflare deploy."
- Per-phase mergeable preconditions.

## Out of scope

- Provider credential plane (remains in `~/.pie/auth.json`).
- Windows support.
- Real Cloudflare API calls in build/test CI. Production deploy/e2e is a separate protected workflow lane.
- Direct agent-to-agent file / blob transfer (notification payload only; large objects deferred past v0).
- Multi-cloud / multi-region hub federation.
- Long-lived chat history beyond notification idempotency + audit retention.

## Coordinator-maintained appendices

### Terminology

| Term                 | Meaning                                                                                          |
| -------------------- | ------------------------------------------------------------------------------------------------ |
| **hub**              | The `pie.0xfefe.me` Cloudflare Worker MCP service.                                              |
| **namespace**        | Per-human-user namespace, established at password registration. Every agent belongs to exactly one. |
| **agent_id**         | Server-issued UUID. Immutable. The address of an agent. All authorization and audit key on this. |
| **handle**           | Human / LLM-readable alias for an agent. Unique within namespace. Format `[a-z0-9_-]{2,32}`.    |
| **`@handle@namespace`** | Wire-level display form, e.g. `@alice@dongxu`. Never an authorization input.                 |
| **discoverable**     | Whether an agent appears in `list_agents` / `discover_public_agents`. Values: `public` / `namespace` / `none`. |
| **inbox**            | Who can send notifications. Values: `open` / `namespace` / `invited` / `closed`.                |
| **trust list**       | Receiver-owned list of `{sender_agent_id, action_class}` granted past first-contact gate.       |
| **block list**       | Receiver-owned list of `{sender_agent_id}` whose notifications are silently dropped.            |
| **action_class**     | Authorization scope for a trust grant. v0: `notification`. Future: `tool_call`, `data_read`, etc. |
| **first-contact gate** | User prompt on first notification from an untrusted, cross-namespace sender. Reuses issue #110 `ControlPlaneWrite` gate. |
| **hub-issued token** | Credential the hub issues per agent for authenticating MCP calls. Stored hashed server-side. Separate from provider keys. |
| **`CF_API_KEY`**     | GitHub repository secret containing the Cloudflare deploy token. Used only by the protected deploy/e2e workflow lane; never by build/test CI or runtime. |

### Defaults

| Field                              | Default                 | Source                                                  |
| ---------------------------------- | ----------------------- | ------------------------------------------------------- |
| `discoverable`                     | `public`                | §4.2                                                    |
| `inbox`                            | `namespace`             | §4.2                                                    |
| Trust TTL (`Always`)               | 90 days                 | §4.OQ-3 / §4.3                                          |
| Trust TTL (`Block`)                | indefinite              | §4.OQ-3 / §4.3                                          |
| `payload_visibility`               | `Local`                 | §5.4                                                    |
| Human session idle timeout         | 24 hours                | §3.2                                                    |
| Human session absolute max         | 30 days                 | §3.2                                                    |
| First-contact prompt timeout       | 5 minutes               | #110 v0.2 §110.OQ-3 (embedder-configurable)            |
| `pie_summary` hub cap              | 240 characters          | §2.5                                                    |
| Runtime-side `payload_summary` cap | 4 KiB                   | §5.10 (defense-in-depth against hub cap regression)     |
| Tool result body cap (non-list)    | 64 KiB                  | §2.3                                                    |
| List-tool body cap                 | 256 KiB                 | §2.3                                                    |
| `send_notification` request body   | 16 KiB                  | §2.7                                                    |
| Agent token expiry                 | no automatic expiry (`expires_at = null`) unless user / admin sets one | §3.3 |

### Cross-chapter artifact pointers

Where to find canonical schemas, file shapes, and report formats so reviewers don't hunt for them:

| Artifact                                                              | Lives in              | Cited by                                |
| --------------------------------------------------------------------- | --------------------- | --------------------------------------- |
| `~/.pie/hub-trust.json` key tuple and shape                           | §3.4, §5.7            | §4.3                                    |
| `Custom { custom_type: "fefe_trust_decision" }` audit schema          | §5.7                  | §4.3, §8.5 (redaction acceptance)       |
| `Custom { custom_type: "trigger_prompt" }` audit schema               | #110 v0.2 Artifact E  | §4.3, §5.6, §8.5                        |
| `Custom { custom_type: "control_plane_prompt" }` audit schema         | #110 v0.2 Artifact E  | §5 (tool-call prompts; not fefe)        |
| `HarnessEvent::TriggerPromptRequest` / `resolve_trigger_prompt`       | #110 v0.2 Artifact D  | §4.3, §5.6, §6a                         |
| Release-complete report format                                        | §8.6                  | §8.4 gate 6                             |
| Phased release gate matrix                                            | §8.4                  | Definition of done                      |
| Permission strings (`agent:*`, `notification:*`, `token:*`, `trust:*`) | §3.3                  | §2.3 tool tables                        |
| MCP error code namespace (`-32000`…`-32010`)                          | §2.6                  | §3.5                                    |
| `_meta.pie_dedup_key` / `_meta.pie_summary` wire fields               | PR #56 (canonical)    | §2.5, §5.5, §5.10                       |

**Audit complementarity (referenced from §4.3 / §5.6 / §5.7).** Two distinct audit types fire on different events; reviewers can confirm redaction once by reading the §5.7 forbidden-fields list and applying it equivalently:

- `trigger_prompt` — runtime-emitted, **every** prompt resolution (allow / deny / timeout). Owned by #110.
- `fefe_trust_decision` — embedder-emitted, **only** when the user picks `Always` or `Block` and the cache changes. Owned by §5.7.

These complement, do not duplicate. `Accept once` writes only `trigger_prompt`. `Always` / `Block` write both.

### Cross-chapter open questions

| ID            | Question                                                                                  | Status / take                                         |
| ------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| ~~RFC-OQ-1~~  | §7 Worker implementation owner.                                                            | **RESOLVED 2026-05-30 by @EdHuang: @Tools-MCP-Lead**. TS + D1 + Durable Objects per Tools-MCP's volunteered scope. Deploy workflow (PR #140) lands in parallel. Per @EdHuang's same-day simplification, deploys go through GitHub Actions `workflow_dispatch` with the `production` environment limiting `CF_API_KEY` to deploy / rollback jobs — no per-run human approval; any authorized team member can trigger. Worker MVP and live e2e gate 6 unblock once @EdHuang adds `CF_API_KEY` to repo secrets, the `production` GitHub Environment is configured with branch policy = `main`, and `pie.0xfefe.me` is pointed at a placeholder Worker on Cloudflare. |
| RFC-OQ-2      | `inbox = open` in v0? (§4.OQ-1)                                                            | @alice: skip until concrete reason.                   |
| RFC-OQ-3      | `capabilities` taxonomy: registered / free-form / hybrid? (§4.OQ-2)                        | @alice: registered taxonomy day 1.                    |
| RFC-OQ-4      | Trust TTL: `Always` expires? Block expires? (§4.OQ-3)                                      | @alice: 90 d Always, indefinite Block.                |
| RFC-OQ-5      | `inbox = invited` in v0? (§4.OQ-4)                                                         | @alice: ship in v0; gate populates it.                |
| RFC-OQ-6      | Handle character set `[a-z0-9_-]{2,32}` — confirm or widen? (§4.OQ-5)                      | @alice: lock at this for v0.                          |
| RFC-OQ-7      | Collapse `inbox=open` and `inbox=invited` in v0? (§4.OQ-6)                                 | @alice: lean keep both as operator signal. Tools-MCP +1 keep-both (sender-side invitation-token semantic distinction). |
| ~~RFC-OQ-8~~  | Deploy mechanism for `pie.0xfefe.me`: manual or CI auto-deploy?                            | **RESOLVED 2026-05-29 by @EdHuang: CI auto-deploy via GitHub Actions.** EdHuang provides a GitHub repository secret named `CF_API_KEY`. Secret stays inside GitHub Actions (encrypted secret + protected environment); never enters repo, PR body, workflow logs, artifacts, cache, test fixtures, runtime config, session, audit, bug report, or any MCP payload. Secret-hardening requirements detailed in §8 (deploy gate). Reversed an earlier 2026-05-29 manual-deploy decision; superseded entry kept in change log. |
| ~~RFC-OQ-9~~  | Agent token expiry default? (§3.OQ-2)                                                       | **RESOLVED in §3 v0.2:** no automatic expiry by default (`expires_at = null`) for unattended agents; token rotate/revoke remains mandatory before v0 deploy, and explicit expiry may be set by user/admin policy. |
| ~~RFC-OQ-10~~ | Per-machine receiver binding for `~/.pie/hub-trust.json` to defeat dotfile-sync trust replay (§5.OQ-3). | **RESOLVED in §3 v0.2 / §5.OQ-3:** include `local_receiver_instance_id` in the trust key. It is a random local UUID, not hardware identity; audit/logs carry only `local_receiver_instance_id_hash`. |

### Change log

| Date       | By     | Change                                                                                  |
| ---------- | ------ | --------------------------------------------------------------------------------------- |
| 2026-05-29 | @alice | v0.1 scaffold: chapter map, §4 seed draft, terminology, defaults, open-questions log.   |
| 2026-05-29 | @alice | Split §6 into §6a (engine contract + transport, @Tools-MCP-Lead) and §6b (`/hub *` CLI/TUI surface, @CLI-TUI-Dev-Lead) per Tools-MCP-Lead's request. |
| 2026-05-29 | @alice | Scaffold consistency fixes per @QA-Release-Lead review: Tier 4 → Tier 8 (matches master.md), top-level gate wording unified with §8 RFC approval gate, §4.3 audit wording fixed (no-new-prompt-protocol; custom_type registration deferred to §5/§8). Added Provider-Auth's `inbox` × sender decision matrix in §4.2 and follow-up open question RFC-OQ-7 / §4.OQ-6 on `open` vs `invited` collapse. |
| 2026-05-29 | @alice | Per @EdHuang: completion criterion is e2e against the real deployed `pie.0xfefe.me`. Added "Definition of done" section; reorganized §8 phased gates so Real-deploy and Deployed-Worker-e2e are explicit terminal gates, with the e2e gate as definition-of-done. Preserved "no real Cloudflare in CI" rule by distinguishing CI-friendly gates (2/3/4) from manual / human-gated terminal gates (5/6). Raises priority on §7 Worker owner assignment (RFC-OQ-1). |
| 2026-05-29 | @alice | Fold in QA-Release-Lead's status terminology (pre-deploy complete vs release complete / done) and Runtime-dev-lead's critical-path note (#110 P0 alongside §5 implementation). Added RFC-OQ-8 for deploy mechanism (manual vs CI auto-deploy), QA default = manual until @EdHuang decides. |
| 2026-05-29 | @alice | RFC-OQ-8 RESOLVED by @EdHuang: manual deploy. `~/cf_token` constraint locked. Open question struck through with resolution recorded inline. |
| 2026-05-29 | @alice | RFC-OQ-8 **superseded** later same day by @EdHuang: CI auto-deploy via GitHub Actions. Secret name = `CF_API_KEY`. Updated OQ-8 resolution; rewrote §8 gate 5 with secret-hardening checklist (protected environment + EdHuang approval + min token scope + no echo / no global env + separate rollback workflow + bounded e2e report). Folded in @Tools-MCP-Lead's note that post-deploy live-Worker e2e can be a CI job (still env-gated). |
| 2026-05-29 | @alice | Ordering note (per @Runtime-dev-lead + @Tools-MCP-Lead): §2 (MCP surface) and §5 (notification envelope) are two views of the same wire bytes. Recommended sequence after scaffold merge: §2 + §5 parallel drafts → cross-cite + co-review → §1 architecture stitch → §3 + §6a + §6b → Worker PR. §7 Worker implementation owner can be named after §1/§2/§5 stabilize, reducing rework risk. Captured in §1 placeholder note (no chapter content change). |
| 2026-05-29 | @alice | @EdHuang chose **option B**: §7 Worker implementation owner deferred until §1/§2/§5 v0.1 land. RFC-OQ-1 row updated to record the deferral and rationale. §1/§2/§5 work proceeds in parallel; no chapter content change. |
| 2026-05-29 | @QA-Release-Lead | §8 v0.1: expanded phased release gates, `CF_API_KEY` GitHub Actions hardening, threat model minimums, per-phase acceptance matrix, deployed-Worker e2e scenarios, and bounded release report format. |
| 2026-05-29 | @Tools-MCP-Lead | §2 v0.1: Hub MCP protocol surface — overview, versioning, tools (control-plane / discovery / messaging / trust-block), resources, server-push notifications, error codes, body caps + rate limits, §2 × §5 cross-cite, open questions. |
| 2026-05-29 | @Tools-MCP-Lead | §2 v0.1 follow-up (per @Provider-Auth-Lead direction on §3 ↔ §2 alignment): added `§3 permission` column to §2.3 tool tables citing `agent:*` / `notification:*` / `token:*` / `trust:*` names; added `-32009 auth_required` and `-32010 auth_invalid` to §2.6; renamed `-32000 invalid_session` → `session_expired` and `-32005 token_revoked` → `auth_revoked` and `-32003 unknown_agent` → `not_found` to match §3.5 vocabulary; changed `retry_after_secs` → `retry_after_ms` in §2.6/§2.7; added 240-char vs 4 KiB cap-layering clarification on `pie_summary` (§2.5); refined `principal_id` / `principal_label` wire-vs-runtime framing in §2.8 (per Runtime cross-cite review on PR #127). |
| 2026-05-29 | @Provider-Auth-Lead | §3 v0.1: identity/auth/session/namespace/agent registry draft. Added credential class separation, human session requirements, agent token lifecycle, per-call authorization rules, bounded auth errors, audit/redaction rules, threat-model checkpoints, and §3 open questions. |
| 2026-05-29 | @Tools-MCP-Lead | §1 v0.1 partial (§1.1 framing / §1.2 ASCII component map / §1.3 9-step wire-bytes lifecycle / §1.5 reuse-vs-new ledger). Stitches §2 + §5 + §4 + §3 into one mental model. §1.5 ledger pins the operative rule: no implementation PR introduces a new runtime trait beyond what RFC 1 shipped — the hub work adds one transport (`HttpMcpTransport`), one factory (`make_pie_hub_notification_hook`), one `BeforeTriggerHook` impl (`HubTrustGate`), two Custom audit types, two `~/.pie/*.json` files, and the Worker; everything else is configuration. §1.4 placeholder calls out @Runtime-dev-lead. |
| 2026-05-29 | @Runtime-dev-lead | §1.4 trigger pipeline reuse — Runtime side. Diagrams the pre-existing-on-`main` vs new-in-RFC #18 split on the runtime boundary; pins the "no new hook trait, no new pipeline, no new envelope, no new audit machinery" rule; lists the four Runtime-side new things (`make_pie_hub_notification_hook` factory, `HubTrustGate` impl, `~/.pie/hub-trust.json` schema, `fefe_trust_decision` Custom audit) and confirms the §1 → §1.5 → sub-PR sequencing fits ~400 LoC of `crates/agent` delta plus tests. §1 v0.1 chapter complete. |
| 2026-05-29 | @alice | §4 v0.2: folded review feedback accumulated across §3 v0.1 (PR #125), §5 v0.1 (PR #127), §8 v0.1 (PR #126), §2 v0.1 (PR #128), and #110 design v0.2 (PR #130). §4.2 matrix "direct route" cells cite §3.4 per-call auth precondition (sender `notification:send`, receiver `notification:receive`). §4.3 expanded: names #110 v0.2 Artifact D specifically (`HarnessEvent::TriggerPromptRequest` / `resolve_trigger_prompt`), cites §5.7 as canonical location for `fefe_trust_decision` and `~/.pie/hub-trust.json` shape, documents the two complementary audit types (`trigger_prompt` runtime + `fefe_trust_decision` embedder), and projects §3.4's `AgentPrincipal` ↔ §5.3's `TriggerAuthority`. §4.4 splits the profile schema into list / detail / prompt-bounded subsets — the prompt-bounded subset `{display_name, description, capabilities}` is what `BeforeTriggerActionContext` carries to the first-contact UI (per §5.OQ-1). §4 × §5 contract section refreshed accordingly. Chapter map status normalized to single-form ("v0.1" / "v0.2" without "draft" suffix); §8 title's `~/cf_token` → `CF_API_KEY` typo fixed in chapter map. Added coordinator-maintained "Cross-chapter artifact pointers" sub-section so reviewers can find canonical schemas (audit types, hub-trust.json shape, prompt channel, permission strings, error namespace, dedup key) without grepping. Defaults table extended with §3.2 human session timeouts, §2.5/§5.10 cap layering, #110 prompt timeout. Promoted **RFC-OQ-10** (per-machine receiver binding for `~/.pie/hub-trust.json` to defeat dotfile-sync trust replay; from §5.OQ-3, surfaced by @Runtime-dev-lead + @Provider-Auth-Lead in cross-doc review on #130 v0.2). |
| 2026-05-29 | @Provider-Auth-Lead | §3 v0.2: resolved RFC-OQ-9 and RFC-OQ-10. Agent tokens default to no automatic expiry (`expires_at = null`) unless user/admin sets one, while rotate/revoke and bounded revoke behavior remain mandatory before v0 deploy. First-contact trust cache keys now include `local_receiver_instance_id + receiver_agent_id + sender_agent_id + action_class`; local instance id is random local state, not hardware identity, and audit/logs expose only `local_receiver_instance_id_hash`. Updated §5.OQ-3 and the `hub-trust.json` shape to match. |
| 2026-05-29 | @Tools-MCP-Lead | §6a v0.1: client integration engine contract — `HttpMcpTransport` impl Transport over Streamable HTTP (POST + SSE), `~/.pie/mcp.toml` hub entry shape (`kind = "streamable_http"`), `mcp_loader::connect_one` dispatch extension, `make_pie_hub_notification_hook` factory call site, `Authorization: Bearer` header-only token discipline (per Provider/Auth §1 review ask), body-cap + error-mapping defense-in-depth, reconnect / `Last-Event-ID` resume / dedup wiring, embedder install point for `HubTrustGate` as `BeforeTriggerHook`, faux HTTP/SSE fixture test strategy. Owns the engine API only — CLI/TUI surface stays in §6b. |
| 2026-05-29 | @CLI-TUI-Dev-Lead | §6b v0.1: `/hub *` CLI / TUI / Web UX surface — command grammar for status/login/register/profile/visibility/list/send/inbox/trust/block/rotate/logout; strict §6a engine-only boundary (no parallel hub client, no raw MCP parsing); Hub panel and Web state bounded fields; first-contact prompt card UX through #110 trigger prompt channel; feed display rules keeping heartbeat/reconnect noise out of the main conversation while showing accepted notification results; error-to-recovery copy table; user-path test matrix and open questions for login flow, `/hub send`, default panel visibility, Web prompt rendering, multi-hub selector, and timeout UI. |
| 2026-05-30 | @alice | RFC-OQ-1 RESOLVED by @EdHuang: §7 Worker implementation owner = @Tools-MCP-Lead. Updated chapter map + §7 body (status = in progress; TS + D1 + Durable Objects; Worker MVP scope = §8.5 scenario 1 + 5 minimum). Added the deploy / rollback GitHub Actions workflows (`.github/workflows/deploy-fefe.yml`, `.github/workflows/deploy-fefe-rollback.yml`) as the §8.2 scaffolding so Worker code (Tools-MCP) and deploy infra advance in parallel. Workflows enforce: strict regex validation on `wrangler_version` and `worker_dir` in a non-secret step that emits validated values as `$GITHUB_OUTPUT`; secret-bearing steps read only `steps.validate.outputs.*`; rollback `target_version` must be a 40-char SHA that is an ancestor of `origin/main` (verified via `git merge-base --is-ancestor`); `CF_API_KEY` exposed only in deploy/rollback/disable step env, never workflow-global. Per @EdHuang's same-day simplification, the `production` GitHub Environment limits the secret to deploy / rollback jobs but does NOT require per-run human approval — team members can trigger deploys directly via `workflow_dispatch`. Updated §8 gate 5 + §8.2 + RFC top status (no more "NOT implementation-ready"); folded blockers from @Provider-Auth-Lead + @QA-Release-Lead inline. Worker MVP + EdHuang's repo-secret / environment / Cloudflare DNS setup unblock the first live deploy. |
