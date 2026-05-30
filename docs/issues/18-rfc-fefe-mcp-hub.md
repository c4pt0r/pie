# RFC: pie.0xfefe.me public MCP hub

> Parent: [[00-master]] roadmap.
> Tier: 8 (cross-agent connectivity). Extends [[08-mcp-client]] and [[17-harness-expansion]].
> Status: **draft v0.1 (scaffold)** — NOT implementation-ready. Implementation PRs are gated by §8's RFC approval gate (§1–§6b reviewed; §7 owner assigned; threat model written). Until that gate passes, do not open Worker, `/hub` CLI/TUI, or `~/.pie/mcp.toml` UX PRs against this design.
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
> - §7 Worker implementation + storage model — **TBD**
> - §8 Deployment / `~/cf_token` boundary / CI / acceptance / release gate — @QA-Release-Lead

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

This does NOT change the "no real Cloudflare in build/test CI" rule (§8). Build/test CI uses faux Worker / `wrangler dev` / Miniflare. Deploy is a **separate, approval-gated CI lane** (GitHub Actions deploy job with protected environment + @EdHuang approval) — see §8 gate 5. The deployed-Worker e2e is gate 6.

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
| 1   | Architecture overview                                          | @Tools-MCP-Lead + @Runtime-dev-lead         | TBD                 |
| 2   | Hub MCP protocol surface                                       | @Tools-MCP-Lead                             | TBD                 |
| 3   | Identity / Auth / Session / Namespace / Agent registry         | @Provider-Auth-Lead                         | TBD                 |
| 4   | Visibility model                                               | @alice                                      | **seed draft below** |
| 5   | Notification routing / delivery semantics                      | @Runtime-dev-lead                           | TBD                 |
| 6a  | Client integration (contract + runtime boundary)               | @Tools-MCP-Lead                             | TBD                 |
| 6b  | `/hub *` CLI / TUI surface                                     | @CLI-TUI-Dev-Lead                           | TBD                 |
| 7   | Worker implementation + storage                                | **TBD**                                     | TBD                 |
| 8   | Deployment / `~/cf_token` / CI / acceptance / release gate     | @QA-Release-Lead                            | TBD                 |

---

## §1 Architecture overview

TBD — @Tools-MCP-Lead (hub MCP service model) + @Runtime-dev-lead (RFC 1 trigger pipeline reuse boundary; envelope contract with §4 / §5).

**Drafting sequence (per @Runtime-dev-lead + @Tools-MCP-Lead 2026-05-29):** §2 (MCP surface) and §5 (notification envelope) are two views of the same wire bytes; draft them first in parallel, cross-cite + co-review, then stitch §1 architecture. §3 + §6a + §6b follow. §7 Worker PR can start once §1/§2/§5 are merged, reducing the risk that the Worker author has to rework against a moving envelope.

## §2 Hub MCP protocol surface

TBD — @Tools-MCP-Lead. Tool / resource / notification schemas, error codes, body cap, versioning, `additionalProperties: false` discipline.

## §3 Identity / Auth / Session / Namespace / Agent registry

TBD — @Provider-Auth-Lead.

Scope (per 2026-05-29 discussion):

- Human account: username + password registration, password hashing, session expiry / revocation, rate limit.
- Agent credential: hub-issued token scoped to `{user_id, namespace, agent_id, permissions}`. Rotatable, revocable, server stores hash + identifier only — never plaintext.
- Trust scope minimum tuple for `Always` grants: `{receiver_agent_id, sender_agent_id, action_class=notification}`. Any extension to namespace / team scope requires explicit risk write-up + UI copy.
- `discoverable` controls listing only; `inbox` controls write. Every send path re-authorizes against `inbox` policy — discover result is never an authorization input.
- MCP errors return recovery hints (re-login, re-register agent, token revoked). Never echo tokens, namespace secrets, internal binding names.

## §4 Visibility model — seed draft (@alice)

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

- "direct route" = `NotificationHook → Trigger → handle_trigger` per §5; no user prompt.
- "first-contact prompt" = `BeforeTriggerHook::Prompt` via the issue #110 gate per §4.3.
- "hub denies" = `send_notification` returns a bounded `permission_denied` recovery hint to the sender (per @Provider-Auth-Lead's redaction rule). No prompt fires on the receiver side.
- "silent drop" = receiver-side block list match; sender sees a non-distinguishing delivery result (sender can't probe the block list).
- Open question §4.OQ-6 (new): `open` and `invited` are functionally identical in this matrix; do we collapse them in v0 or keep both for operator signal?

### §4.3 First-contact gate — reuse issue #110 user-prompt mechanism

**Rule.** A notification from a sender `agent_id` that is (a) not in the receiver's trust list and (b) originates outside the receiver's namespace does **not** enter `NotificationHook → Trigger` directly. The receiver-side pie client surfaces a user prompt via the issue #110 `ControlPlaneWrite` gate:

```
@bar@cloudflare-bot wants to send a notification to @alice@dongxu.
Sender description: "ci-status notifier"
Capabilities: ["github-status", "deploy-alerts"]
Choice:  Accept once    Always (notification-only)    Block
```

- **Accept once** — current notification routes through; sender is NOT added to trust list.
- **Always** — sender enters the trust list with scope `{sender_agent_id, receiver_agent_id, action_class=notification, namespace?}`. Future notifications route directly.
- **Block** — sender enters the block list; future attempts silent-drop, no further prompt.

**Why.** Inbox-open without consent is spam. The issue #110 control-plane gate is already in flight for `ControlPlaneWrite` operations (`NewTrigger`, `InstallSkill`, `SetSkillState`, etc.); first-contact is the same shape — an authorization decision the model cannot self-confirm — so reuse the gate semantics rather than invent a new trust UI.

**Application.**

- Implementation lives in `BeforeTriggerHook::Prompt` policy on the runtime side (per @Runtime-dev-lead, §5). **No new prompt protocol** — reuses the existing issue #110 gate channel.
- Trust list and block list are **receiver-owned**, persisted as `~/.pie/hub-trust.json` (key tuple per @Provider-Auth-Lead). Audit record uses the existing `SessionTreeEntry::Custom` mechanism with `custom_type = "fefe_trust_decision"`; body holds `{sender_agent_id, receiver_agent_id, decision, scope, at}` — never notification payload. The custom_type registration and field schema land in §5 (Runtime owns audit records) or §8 (QA defines redaction acceptance); §4 only states the bounded contract.
- Trust scope is the narrowest useful tuple. **Never global, never "trust the whole namespace," never "trust all MCP tools."** (Per @Provider-Auth-Lead.)
- `action_class` starts as `notification`. New action classes (e.g. `tool_call`, `data_read`) each get their own gate; granting one does not grant another.
- Handle rename does not migrate or invalidate trust — keyed on `agent_id`.

### §4.4 Sender profile is product copy, not decoration

**Rule.** Profile fields are the input that other agents' LLMs read when deciding whether to accept a notification or send one. They are the agent's marketplace listing. Treat them like product copy: bounded, structured, no ambiguity, no escape hatches.

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
- **Separate listing schema vs detail schema.** `list_agents` returns only the minimal subset `{handle, agent_id, display_name, capabilities, discoverable, inbox}`. The detail view returns more. `list_agents` MUST NOT leak raw registration metadata.
- Profile updates are control-plane writes — through the same audit channel as `set_my_visibility`. Never bypass.
- Field naming follows "tool schema = LLM API" discipline (per @Tools-MCP-Lead): snake_case, unambiguous, each field's purpose stated in its JSON-Schema `description` so the listing LLM knows what each field is for. Enums const-locked with per-variant descriptions. `additionalProperties: false` at every level.

### §4 × §5 contract

`§4` owns *what* (identity, visibility, trust semantics, profile shape). `§5` owns *how* (envelope, transport, hooks):

- `TriggerAuthority.principal_id` carries `agent_id` (UUID) — the authorization key.
- `TriggerAuthority.principal_label` carries `@handle@namespace` — display only.
- Hub-pushed notifications enter via the `mcp:pie-hub:...` source label namespace.
- Ack / dedup via `pie_dedup_key`; default `payload_visibility = Local`; ordering not guaranteed.
- First-contact prompt is `BeforeTriggerHook::Prompt` keyed on `{receiver_agent_id, sender_agent_id, action_class=notification}` — same prompt channel as issue #110.

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

## §5 Notification routing / delivery semantics

TBD — @Runtime-dev-lead. Scope per 2026-05-29 commitment:

- Client side: hub MCP server pushes `notifications/...` → `McpNotificationHook` → `Trigger` envelope → existing `register_notification_hook` supervisor. No new hook trait.
- `TriggerAuthority` fields: `principal_id = agent_id` (UUID), `principal_label = @handle@namespace` (display only).
- Envelope: source label `mcp:pie-hub:...`, ack / dedup via `pie_dedup_key`, default `payload_visibility = Local`, redelivery / idempotency semantics, offline + reconnect backlog bounds, ordering not guaranteed.
- Hub-side fan-out / inbox delivery semantics live in §6 / §7; §5 covers the boundary from client receive to `Trigger` conversion.

**§2 × §5 coordination protocol (per Runtime + Tools-MCP 2026-05-29).** §2 (MCP surface) and §5 (envelope) are two views of the same wire bytes; both reference, not redefine. Existing `_meta.pie_dedup_key` / `_meta.pie_summary` from PR #56 (`McpNotificationHook`) is the source of truth and is cited from both chapters. New fields divide by layer:
- Envelope-internal (`TriggerAuthority`, `payload_visibility`, etc.) — Runtime defines in §5; §2 cites.
- MCP wire-level (`_meta.*` namespace additions, tool param names) — Tools-MCP defines in §2; §5 cites.
- Whoever drafts first picks the name; the other follows. Drafts ship in the same commit; reviewer merges as a pair.

## §6a Client integration — contract + runtime boundary

TBD — @Tools-MCP-Lead. `HttpMcpTransport` (MCP spec 2025-03-26 streamable HTTP — POST for requests, SSE for server-push), `~/.pie/mcp.toml` hub entry shape, `mcp_loader.rs` adapter, `McpNotificationHook` wiring, first-contact gate cite to issue #110.

`HttpMcpTransport` is a parallel deliverable independent of hub schema; it benefits any MCP-over-HTTP server.

This chapter owns the **engine API**: connect / register / list / send / poll signatures, error mapping to recovery hints, transport reconnect and backoff semantics. The CLI / TUI in §6b consumes this API; it does not start a parallel hub client.

## §6b `/hub *` CLI / TUI surface

TBD — @CLI-TUI-Dev-Lead. User-facing surface: `/hub login`, `/hub register`, `/hub status`, `/hub list`, Hub panel in the TUI, Feed-line display rules for hub-originated notifications, error wording with next-step recovery actions (per Provider-Auth-Lead's "internal vocabulary → user recovery action" rule).

**§6a × §6b contract.** CLI commands call §6a's engine API only. CLI does not parse hub MCP responses directly, does not own connection state, does not run a parallel client. Schema lives in §6a. Mirrors the engine / slash-command split in [[02-slash-commands]] task #23.

## §7 Worker implementation + storage model

**Owner: TBD.** EdHuang to nominate, or self-assign once §1 / §3 / §5 stabilize.

Carry-forward open questions for the §7 owner:

- Storage choice: D1 (relational) vs KV vs Durable Objects vs combination. Trade-offs: D1 for joins / queries; KV for cheap reads; DO for stateful per-namespace coordination and consistent fan-out.
- Sharding strategy and migration story.
- Rate-limit numbers (per namespace / per agent / per source IP).
- Body cap numbers.
- Cold-start budget for serverless invocation.
- Notification at-least-once delivery: durable queue or DO replay?

## §8 Deployment / `CF_API_KEY` / CI / acceptance / release gate

Owner: @QA-Release-Lead.

The release process has two distinct tracks:

- **Build/test CI**: deterministic, repeatable, no real Cloudflare access. Uses faux HTTP/SSE servers, local Worker fixtures, `wrangler dev`, or Miniflare.
- **Deploy/e2e CI**: approval-gated production workflow that deploys to real `pie.0xfefe.me` and runs live e2e. This lane may use the GitHub repository secret `CF_API_KEY`; ordinary build/test jobs may not.

Every gate below MUST state required tests, manual verification, rollback / disable path, and content forbidden from logs / audit / session.

### §8.1 Phased gates

1. **RFC approval gate** — §1, §2, §3, §4, §5, §6a, §6b reviewed; §7 owner assigned; threat model written.
2. **Transport PR gate** — `HttpMcpTransport` lands as a generic capability with faux HTTP / SSE tests; no real Cloudflare in build/test CI.
3. **Worker local / faux gate** — Worker implementation passes against local fixture (`wrangler dev` or Miniflare); no `CF_API_KEY` access.
4. **Client UX gate** — `/hub *` CLI / TUI commands, `~/.pie/mcp.toml` hub entry shape, first-contact prompt UX, error → recovery-action wording.
5. **Real deploy gate (CI auto-deploy via GitHub Actions)** — `.github/workflows/deploy-fefe.yml` deploys the Worker to the real `pie.0xfefe.me`. Secret hardening (per team consensus 2026-05-29 — fold into §8 v0.1):
   - Cloudflare token lives in GitHub repository secret `CF_API_KEY`, scoped minimally to this Worker (`Workers Scripts:Edit` + required KV/D1/DO bindings).
   - Deploy job runs only on protected branch / tag or `workflow_dispatch`; PRs from forks cannot access the secret.
   - Deploy job runs in a protected GitHub Environment (e.g. `production`) with **required reviewer = @EdHuang** approval before execution.
   - The secret is read only via `${{ secrets.CF_API_KEY }}` in the deploy step's job-level env, never the workflow-global env.
   - No `set -x`, no wrangler debug logging, no echo of secret-bearing config. Logs / artifacts / cache MUST NOT contain the token.
   - Rollback / disable runs in a separate, equally-protected workflow.
   - README / CHANGELOG document workflow file path, environment protection, bindings, migrations, rollback procedure.
6. **Deployed-Worker e2e gate (definition of done — per @EdHuang)** — **the RFC is NOT complete until this gate passes.** Two real pie agents on different machines / namespaces register, discover each other, send notifications, and exercise the first-contact gate end-to-end against the deployed `pie.0xfefe.me`. Per @Tools-MCP-Lead, a post-deploy CI job can run this automatically against the live Worker (gated on the protected environment). The full §8 acceptance matrix runs against the deployed Worker — faux-fixture passes alone do not satisfy this gate. E2E reports only contain `deployment_id` / `version` / `trace_id` / bounded result — never the token, hub session, agent token, or payload secret. Rollback path documented and rehearsed.

`CF_API_KEY` boundary: usable only inside the deploy job of gate 5 and (optionally) the post-deploy live-Worker job of gate 6. MUST NOT appear in CI build / test logs, runtime config, session, audit, bug report, MCP / notification payload, or any artifact.

**CI vs acceptance gate distinction.** Gates 2, 3, 4 are CI-friendly without the secret (no Cloudflare access). Gate 5 deploy is automated but **environment-gated by @EdHuang's approval**, not a free-running CI step. Gate 6 e2e targets the real deployed Worker. The "no real Cloudflare in CI" rule is preserved for build / test CI; deploy CI is a separate, approval-gated lane.

### §8.2 Secret handling and workflow hardening

`CF_API_KEY` is a Cloudflare deploy-only secret. It is not a hub credential, not a provider API key, and not an agent token.

Required workflow controls:

- The deploy workflow is `.github/workflows/deploy-fefe.yml`.
- Deploy job runs only from protected `main` / release tags or explicit `workflow_dispatch`.
- Deploy job uses a protected GitHub Environment named `production`, with required reviewer @EdHuang.
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
| Duplicate / replayed notifications | Stable notification id and `_meta.pie_dedup_key`; idempotent receive path. |
| Worker outage / rollback | Disable/rollback workflow, health checks, bounded client recovery hints. |

### §8.4 Per-phase acceptance matrix

| Gate | Required automated checks | Required manual / release checks | Merge / completion status |
| --- | --- | --- | --- |
| 1. RFC approval | Docs lint / `git diff --check`; chapter owner reviews; threat-model checklist complete. | @alice coordinator confirms open-question table is current; @QA-Release-Lead confirms release gates are testable. | Allows implementation planning. Does not allow marking feature complete. |
| 2. Transport PR | Faux HTTP POST request/response; SSE receive; timeout/cancel; reconnect/backoff; malformed JSON-RPC frame; header auth isolation; no Cloudflare calls. | Local run against a toy MCP-over-HTTP fixture. | `HttpMcpTransport` may merge as generic MCP capability. |
| 3. Worker local / faux | Miniflare / `wrangler dev` tests for register/login/token, list/discover, send/receive, permission denied, body cap, rate limit, idempotency, redaction, schema `additionalProperties: false`. | Local smoke using fake accounts and temp storage; cleanup verified. | Worker code may merge behind non-production docs/status. Not release complete. |
| 4. Client UX | `/hub *` command tests; TUI/Web Hub status panel; bounded feed display; auth error → recovery action; no secret-bearing output; first-contact prompt display. | Operator verifies status/error copy and redaction with representative hub states. | Client UX may merge when backed by §6a engine API; not release complete. |
| 5. CI deploy | Deploy workflow validates branch/environment restrictions; workflow dry-run or staging run; secret access limited to deploy step; logs/artifacts/cache scanned for forbidden values. | @EdHuang approves `production` environment run; deployment id/version recorded; rollback workflow verified available. | Real `pie.0xfefe.me` can be deployed; still not done until gate 6 passes. |
| 6. Deployed-Worker e2e | Post-deploy live e2e may run in protected workflow: two namespaces / agents register; discover; send; receive; first-contact; dedup; revoke; deny; body cap/rate limit. | Bounded e2e report posted to #fefe with deployment id, version, trace ids, pass/fail matrix, rollback decision. | Only passing gate 6 is **release complete / done**. |

### §8.5 Deployed-Worker e2e scenario set

Gate 6 must run against real `https://pie.0xfefe.me` after deploy. Minimum scenarios:

1. **Registration and token issue** — Create two human accounts / namespaces; register one pie agent under each; confirm each gets immutable `agent_id`, handle, namespace, and hub-issued token. Do not print tokens.
2. **Public discovery** — Agent A can discover Agent B only when B's `discoverable` permits it. Private / `none` agents do not appear.
3. **Inbox denial** — Cross-namespace send to `inbox=namespace` or `closed` returns bounded `permission_denied` recovery hint; receiver sees no prompt and no trigger.
4. **First-contact prompt** — Cross-namespace send to an untrusted target with prompt-eligible inbox produces the issue #110 prompt path, not direct trigger execution.
5. **Accepted notification path** — After `Accept once` or `Always`, notification becomes `McpNotificationHook` → `Trigger` → agent flow; audit/feed contain bounded metadata only.
6. **Trust persistence** — `Always` trust routes a second notification without prompting; handle rename does not bypass trust because trust keys on `agent_id`.
7. **Block path** — `Block` suppresses future prompts and prevents notification delivery with non-distinguishing sender result.
8. **Dedup / idempotency** — Replaying the same notification id / `_meta.pie_dedup_key` does not double-run the receiver.
9. **Token revoke / rotate** — Revoked sender token cannot list/send; rotated token works; errors contain recovery hint only.
10. **Body cap / rate limit** — Oversized notification and rate-limit exceedance fail closed with bounded errors; no raw body leaks in logs/audit/report.
11. **Rollback / disable rehearsal** — Run or dry-run protected rollback/disable workflow; confirm operators know how to stop service or revert deployment.
12. **Redaction sweep** — Inspect live e2e logs/report/artifacts for forbidden values: `CF_API_KEY`, hub sessions, agent tokens, provider keys, raw payload secrets, password hashes.

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
scenarios: <12-line pass/fail matrix from §8.5>
trace_ids: [<bounded trace ids>]
rollback_status: available|executed|not_available
redaction_check: pass|fail
notes: <bounded recovery notes, no secrets>
```

Forbidden in reports: `CF_API_KEY`, hub session cookies, agent tokens, notification body secrets, provider keys, full payloads, password hashes, raw database rows, Cloudflare internal binding secrets.

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

| Field                | Default at registration |
| -------------------- | ----------------------- |
| `discoverable`       | `public`                |
| `inbox`              | `namespace`             |
| Trust TTL (`Always`) | 90 days                 |
| Trust TTL (`Block`)  | indefinite              |
| `payload_visibility` | `Local`                 |

### Cross-chapter open questions

| ID            | Question                                                                                  | Status / take                                         |
| ------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------- |
| RFC-OQ-1      | §7 Worker implementation owner.                                                            | **Deferred (2026-05-29 by @EdHuang, option B):** name owner after §1, §2, §5 v0.1 land. Avoids rework against a moving envelope; pushes done gate by ~2 PR cycles but reduces risk. §1/§2/§5 work proceeds in parallel. |
| RFC-OQ-2      | `inbox = open` in v0? (§4.OQ-1)                                                            | @alice: skip until concrete reason.                   |
| RFC-OQ-3      | `capabilities` taxonomy: registered / free-form / hybrid? (§4.OQ-2)                        | @alice: registered taxonomy day 1.                    |
| RFC-OQ-4      | Trust TTL: `Always` expires? Block expires? (§4.OQ-3)                                      | @alice: 90 d Always, indefinite Block.                |
| RFC-OQ-5      | `inbox = invited` in v0? (§4.OQ-4)                                                         | @alice: ship in v0; gate populates it.                |
| RFC-OQ-6      | Handle character set `[a-z0-9_-]{2,32}` — confirm or widen? (§4.OQ-5)                      | @alice: lock at this for v0.                          |
| RFC-OQ-7      | Collapse `inbox=open` and `inbox=invited` in v0? (§4.OQ-6)                                 | @alice: lean keep both as operator signal. Tools-MCP +1 keep-both (sender-side invitation-token semantic distinction). |
| ~~RFC-OQ-8~~  | Deploy mechanism for `pie.0xfefe.me`: manual or CI auto-deploy?                            | **RESOLVED 2026-05-29 by @EdHuang: CI auto-deploy via GitHub Actions.** EdHuang provides a GitHub repository secret named `CF_API_KEY`. Secret stays inside GitHub Actions (encrypted secret + protected environment); never enters repo, PR body, workflow logs, artifacts, cache, test fixtures, runtime config, session, audit, bug report, or any MCP payload. Secret-hardening requirements detailed in §8 (deploy gate). Reversed an earlier 2026-05-29 manual-deploy decision; superseded entry kept in change log. |

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
