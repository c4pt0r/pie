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

1. Read `(receiver_agent_id, sender_agent_id, action_class)` from the trigger.
2. Look up `~/.pie/hub-trust.json` ([§5.7](#57-trust-decision-audit-and-persistence)).
3. Decision:
   - Found entry `Always` and not expired (per RFC-OQ-4 §4.OQ-3: 90-day TTL) → `BeforeTriggerDecision::Allow`.
   - Found entry `Block` → `BeforeTriggerDecision::Deny { reason: "blocked by user trust list" }`.
   - No entry, sender is same-namespace → fall through to next stage (`inbox` enforcement per §4.2; if hub already rejected non-matching `inbox` at send time, this is a defensive belt).
   - No entry, sender is cross-namespace → `BeforeTriggerDecision::Prompt { reason: <bounded sender summary> }`.
4. The runtime emits `HarnessEvent::TriggerHandled { state: NeedsApproval, ... }`. The embedder consumes this through the issue #110 `ControlPlaneWrite` prompt channel — the same UX surface that gates `InstallSkill`, `NewTrigger`, etc.
5. User's three-way decision (`Accept once` / `Always` / `Block` per §4.3) becomes a `fefe_trust_decision` audit entry ([§5.7](#57-trust-decision-audit-and-persistence)).

**Hard dependency on issue #110.** Without the `PermissionDecision::Prompt` channel wired through `before_tool_call`, the `NeedsApproval` state has no embedder-side rendering and the trigger is effectively dropped silently. Issue #110 is P0 alongside this chapter; both must land before the first-contact gate ships.

### §5.7 Trust decision audit and persistence

Two distinct artifacts:

1. **Runtime-emitted audit entry** — `SessionTreeEntry::Custom { custom_type: "fefe_trust_decision", data: {...} }`. Written by the runtime via existing `Session::append_custom`. One entry per user decision (`Accept once` writes one entry without modifying the trust list; `Always` and `Block` write the entry AND persist to disk).

2. **Embedder-owned trust list** — `~/.pie/hub-trust.json`. Read every time `HubTrustGate` evaluates; written when the user picks `Always` or `Block`. Shape:

   ```json
   {
     "version": 1,
     "entries": [
       {
         "key": {
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
| §5.OQ-3    | When `~/.pie/hub-trust.json` is shared across pie machines (e.g. via dotfile sync), receiver_agent_id may differ per machine. Does the entry key on `local_machine_id + receiver_agent_id` for safety? | Lean YES — bind to per-machine receiver. Cross-machine trust replay is a real attack surface if a laptop is lost. Open question for @Provider-Auth-Lead. |
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
| 2026-05-29 | @Tools-MCP-Lead | §2 v0.1: Hub MCP protocol surface — overview, versioning, tools (control-plane / discovery / messaging / trust-block), resources, server-push notifications, error codes, body caps + rate limits, §2 × §5 cross-cite, open questions. |
| 2026-05-29 | @Tools-MCP-Lead | §2 v0.1 follow-up (per @Provider-Auth-Lead direction on §3 ↔ §2 alignment): added `§3 permission` column to §2.3 tool tables citing `agent:*` / `notification:*` / `token:*` / `trust:*` names; added `-32009 auth_required` and `-32010 auth_invalid` to §2.6; renamed `-32000 invalid_session` → `session_expired` and `-32005 token_revoked` → `auth_revoked` and `-32003 unknown_agent` → `not_found` to match §3.5 vocabulary; changed `retry_after_secs` → `retry_after_ms` in §2.6/§2.7; added 240-char vs 4 KiB cap-layering clarification on `pie_summary` (§2.5); refined `principal_id` / `principal_label` wire-vs-runtime framing in §2.8 (per Runtime cross-cite review on PR #127). |
