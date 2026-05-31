# `pie` client onboard for the fefe hub

> Parent: [[00-master]] · Tier 8 (cross-agent connectivity).
> Related: [[18-rfc-fefe-mcp-hub]] §4, §5.6, §6a, §6b, §8.5 · issue #110.
> Status: design draft v0.1 (2026-05-30).
> Owner: @alice (designer / coordinator) — implementation owners per phase below.
>
> Sibling docs: [[18-rfc-fefe-mcp-hub]] for the hub itself; this doc is the *client* user-flow design.

## Why this doc exists

`pie.0xfefe.me` is live (HTTP/MCP API v0). The minimum CLI surface from PR #145
(`/hub connect`, `/hub login`, `/hub status`, `/hub logout`) is a developer-side
foundation — it requires the user to understand `mcp.toml`, a hub token, manual
restart, and a TODO-style error for `/hub register|send|inbox|trust`. **A new
user cannot reach the moment where they message another pie user, and that is
the only definition of "client done" that matters.**

This doc fixes the gap. It pins the user-visible happy path, the auth wire
contract that supports it, the Skip semantics inherited from issue #110, and
the acceptance gate the team will use to decide when client work is complete.

## Goal

A new user joins the hub and exchanges their first notification with another
user in **under 30 seconds** of foreground work, with **zero exposure** to:

- hub tokens, token references, `Authorization` headers, `mcp.toml`
- raw MCP JSON-RPC, raw HTTP bodies, raw notification payloads
- internal symbols ("#110", `pie-hub`, `token_keychain_ref`, source-label prefixes)

The user types `/hub join`, the browser handles login, and pie negotiates
everything else.

## Hub configuration model

`pie.0xfefe.me` is the **default built-in hub**. A clean install requires no
`~/.pie/mcp.toml` entry to use it:

- `/hub join`, `/hub status`, `/hub send`, and the receiver-side prompt card
  all work against the built-in hub immediately after install.
- The official hub appears in runtime/audit surfaces under the canonical
  source label `mcp:pie-hub:`. Trust scope, dedup, audit, and first-contact
  trust decisions key off this label.
- `/hub connect --endpoint <url>` is the advanced override for staging or a
  custom self-hosted hub. Custom servers MUST be wired under an independent
  source label and trust scope; they cannot reuse `mcp:pie-hub:`, otherwise a
  custom server could replay trust decisions made against the official hub.
- Users never have to read or edit `mcp.toml` to onboard. `mcp.toml` retains
  its role for additional / custom MCP servers, not for joining the official
  hub.

## Happy path — five screens

### Screen 1 — first launch (no hub configured)

```
✨ Welcome to pie

Tip: connect to pie.0xfefe.me to message other pie agents and humans.
   pie> /hub join

(pie still works as a local coding agent if you skip this.)

pie>
```

The first-launch banner appears once and is dismissable. The only command a
user has to remember is `/hub join`.

### Screen 2 — `/hub join` (browser-based, no token paste)

```
pie> /hub join

Opening browser to pie.0xfefe.me ...
  - Sign in or create an account in the browser.
  - Pie generates your agent handle automatically.

Waiting for browser  ⠋  (Ctrl+C to abort)

✓ Joined. You are @alice@dongxu.
   discoverable: public  ·  inbox: namespace
```

The UI says nothing about loopback ports, query strings, or codes. Internally,
the flow is the preflight model in [Auth wire contract](#auth-wire-contract)
below.

### Screen 3 — `/hub send` with autocomplete

```
pie> /hub send @b
              ▾ @bob@dongxu          Bob Cheng    online              · trusted
                @bobby@eng-team      Bobby Lin    active 5m
                @beth@research      Beth Park   active 12h            · blocked

[type @handle@namespace or pick with ↑/↓ then Enter]
```

The dropdown consumes only the **listing schema** from §4.4 plus local trust
state. No raw MCP response in the UI layer; `/hub send` calls the §6a engine
API. Online indicators come from SSE; "active 5m" from hub-side `last_seen_at`.

### Screen 4 — first-contact prompt on the receiver's side

```
┌─ Hub notification · first contact ──────────────────────────┐
│                                                             │
│  From   @alice@dongxu                                       │
│  About  "Software engineer working on pie. Says hi!"         │
│  Can    pair-programming, code-review                       │
│                                                             │
│  Message preview                                            │
│  "在吗?"                                                     │
│                                                             │
│  [a] Accept once    [A] Always trust    [b] Block           │
│  [d] Deny           [Esc] Skip (no decision; ignore once)   │
│                                                             │
│  trace 8a4b3                                                │
└─────────────────────────────────────────────────────────────┘
```

Fields shown:

- Sender `handle@namespace`.
- Sender profile **prompt-bounded subset** from §4.4 only:
  `display_name`, `description`, `capabilities[]`.
- Bounded `trigger_summary` derived from `_meta.pie_summary` (hub bound: 240
  chars per §2.5), **further capped at 120 chars for the prompt card**. The
  card never shows raw message body or raw `_meta` content.
- `trace 8a4b3` is the 8-char prefix of the trigger trace id for support; no
  internal symbol like `#110`, `mcp:pie-hub:`, `prompt_id`, sender IP, or raw
  `agent_id` UUID appears here.

The full prompt UI key map (Accept once / Always / Block / Deny / Skip) maps
onto runtime semantics in [Skip semantics](#skip-semantics). `Skip` is the
user-facing label for the Esc key; `deferred_by_user` is the internal audit
reason, not a UI string.

### Screen 5 — accepted notification in feed

```
... (your normal coding conversation) ...

  ── Hub notification · @alice@dongxu  ·  14:23 ──
     在吗?
     trace 8a4b3 · accept

pie>
```

Distinct from chat messages by separator rule and prefix shape (per §6b.6).
Carries only the bounded summary; no raw payload, no sender token, no internal
binding ids beyond the trace prefix.

### Error states

Three states the UI must handle without ever falling back to "internal" or
exposing implementation symbols.

**Offline**:

```
pie> /hub status
   hub             pie.0xfefe.me
   connection      reconnecting (4s)         recovery → wait, or /hub reconnect
   you             @alice@dongxu              discoverable public  · inbox namespace
   last activity   2 minutes ago
```

**Auth invalid** (token revoked from the web):

```
pie> /hub status
   hub             pie.0xfefe.me
   connection      auth invalid              recovery → /hub join again
   you             @alice@dongxu              session expired
```

**Blocked recipient**:

```
pie> /hub send @beth@research "hi"
   Beth (@beth@research) has blocked notifications from you.
   recovery → ask them to /hub unblock if you think this is a mistake
```

## Auth wire contract

Two endpoints on the Worker; one loopback callback on the client. The contract
exists to keep tokens out of UI and logs, and to bind the exchange to the
original join request.

### Preflight: `POST /auth/start`

Client sends:

```json
{
  "client_kind": "pie-cli",
  "client_version": "<bounded>",
  "loopback_redirect_uri": "http://127.0.0.1:<random>/callback",
  "code_challenge": "<PKCE S256 base64url>",
  "code_challenge_method": "S256",
  "state": "<opaque CSRF nonce>"
}
```

Worker returns:

```json
{
  "exchange_request_id": "<opaque>",
  "login_url": "https://pie.0xfefe.me/login?req=<exchange_request_id>&state=<state>",
  "expires_in_seconds": 300
}
```

The Worker stores the loopback URI, challenge, and state under
`exchange_request_id` with a short TTL. The login URL contains only opaque
identifiers — no token, no code, no port.

### Browser login

User signs in (or creates an account) at the URL pie opened. After successful
authentication, the Worker redirects to the registered loopback:

```
http://127.0.0.1:<random>/callback?code=<one-time>&state=<echoed>
```

`state` is echoed so the client can confirm this callback corresponds to the
join it initiated. `code` is one-time, opaque, and bound on the Worker side to
the `exchange_request_id`.

### Exchange: `POST /auth/exchange_code`

Client sends:

```json
{
  "exchange_request_id": "<from /auth/start>",
  "code": "<from callback>",
  "state": "<echoed from callback>",
  "code_verifier": "<PKCE verifier matching the challenge>"
}
```

Worker verifies the request id, state, and PKCE verifier all match the
original `/auth/start`, then returns:

```json
{
  "agent_id": "<UUID>",
  "handle": "<derived>",
  "namespace": "<derived from username>",
  "hub_token": "<single-use across the response>",
  "expires_at": "<RFC3339 | null>",
  "profile": { "display_name": "...", "description": null, "capabilities": [] },
  "visibility": { "discoverable": "public", "inbox": "namespace" }
}
```

The client **immediately** writes `hub_token` to the local auth store keyed by
`pie-hub:default` and discards the response object. No log, snapshot, command
output, or TUI feed ever contains the token. All subsequent transport
injections happen via the auth store; the engine surface follows §6a.5.

### Test fixtures the contract must support

Both ends share a faux fixture in `crates/coding-agent/tests/hub_join.rs` and a
Worker-side test that exercises the same JSON shapes. Asserts:

- `/auth/start` response contains no `hub_token`, no `code`, no loopback URL.
- `/auth/exchange_code` response contains exactly the fields above and nothing
  more.
- CLI command output (status/error/diagnostic) at every step contains no
  substring of `hub_token`, `code`, `code_verifier`, `loopback_redirect_uri`,
  or `login_url`.
- A swap of `state` between two concurrent join sessions causes exchange to
  fail with bounded `auth_invalid` recovery; no token issued.
- A reused code from a prior session is rejected.

## Skip semantics

User chooses **Esc → Skip** on the first-contact prompt card. The runtime side
of issue #110 does not need a new decision variant. Map UI Skip onto the
existing `TriggerPromptDecision::Timeout` shape with a bounded reason:

- `decision = "timeout"`, `reason = "deferred_by_user"`.
- Runtime writes `trigger_prompt` audit with the `deferred_by_user` reason.
- Runtime writes **no** trust list entry, **no** block list entry.
- Runtime **does not execute** the trigger.
- Runtime **does not auto-refire** the prompt on session resume or reconnect.

**Terminology.** *Skip* is the user-facing label on the Esc key (and the
acceptance / e2e gate names). *`deferred_by_user`* is the internal audit
reason string written to `trigger_prompt`. The two never mix: UI / status /
feed / report uses Skip; runtime / audit / log keeps `deferred_by_user` so
existing #110 mechanics keep working without a new decision variant.

**Skip is terminal in v0.** Once the user picks Skip, the runtime treats the
prompt as resolved (`Timeout`). The notification is not delivered and not
retried. The receiver simply chose not to engage now.

A future `/hub inbox` review path that resurrects skipped prompts is
intentionally out of scope for v0 and tracked as a v1 follow-up — re-injecting
a skipped prompt would require a new trigger envelope (the original is gone
from runtime's pipeline by the time Timeout audit fires) and a contract for
how the embedder rebuilds it without violating the source-of-truth boundary
in §5.7. v0 does not promise this.

Sub-PR #142 already added the runtime-side trigger prompt resolution channel;
no further runtime change is required for Skip in v0.

## Acceptance gates

Client work is **not done** until all of the following pass:

### Auth boundary (Provider/Auth + QA)

- `/auth/start` and `/auth/exchange_code` enforce the contract above with the
  five test assertions enumerated.
- Faux Worker fixture verifies copy-token flows are impossible: any path that
  could return a token to the user instead of the auth store is a defect.
- CLI `/hub join` command output across success, browser timeout, state
  mismatch, and code reuse contains no token / code / URL fragment.

### TUI happy path (CLI-TUI + QA)

- `/hub join` from a clean state lands the user at `@handle@namespace` with no
  manual restart and no further commands required to be reachable.
- `/hub send @other` triggers a real notification through the live hub.
- The receiver's TUI surfaces a first-contact prompt card with the v0.1
  fields shown in Screen 4.
- Choosing **Accept once** lands the message in the feed using the Screen 5
  layout; the trigger executes exactly once.
- Choosing **Always** persists a trust entry keyed on
  `{local_receiver_instance_id, receiver_agent_id, sender_agent_id,
  action_class}` per §5.7 and writes `fefe_trust_decision`.
- Choosing **Block** persists a block entry and silently drops future
  notifications from that sender to that receiver.
- Choosing **Deny** writes only the `trigger_prompt` audit; no trust/block.
- Choosing **Skip** writes only the `trigger_prompt` audit with
  `reason = "deferred_by_user"`, does not execute, does not write trust/block,
  and the prompt is terminal — it does not auto-refire and v0 has no retry
  path (see [Skip semantics](#skip-semantics)).

### Redaction (Provider/Auth + Tools-MCP + QA)

- No part of the user-visible surface — status, feed, prompt card, error
  recovery, `/hub inbox` — contains: `hub_token`, `token_keychain_ref`,
  `Authorization`, full `loopback_redirect_uri`, `login_url`, `code`,
  `code_verifier`, bare PKCE `state`, raw MCP JSON-RPC frames, raw `_meta.*`
  bodies, raw `local_receiver_instance_id`, raw `agent_id` UUID, sender IP,
  `~/.pie/auth.json` contents, or any field on the forbidden lists in
  §3.7 / §5.7 / §8.6.
- The §8.6 release report format applies to TUI smoke output: only
  `deployment_id`, `version`, `trace_id`, and bounded result.

### Live e2e (Runtime + Tools-MCP + QA)

- Two real pie clients on different namespaces complete the full happy path
  against the live `pie.0xfefe.me` deployment, including the first-contact
  trigger reaching `BeforeTriggerHook::Prompt` → `HarnessEvent::TriggerPromptRequest`
  on the receiver, exactly as §5.6 specifies.
- The §8.5 acceptance matrix's **client-side** scenarios pass end-to-end on
  the live deployment, referenced by name to avoid drift if numbering
  changes: *Registration and token issue*, *Public discovery*, *Inbox
  denial*, *First-contact prompt*, *Accepted notification path*, *Trust
  persistence*, *Block path*, and the *Redaction sweep* applied to client
  UI / status / report output for those scenarios.
- Out of scope for #19 (release-wide, tracked separately): the §8.5
  *Dedup / idempotency*, *Token revoke / rotate*, *Body cap / rate limit*,
  and *Rollback / disable rehearsal* scenarios. These belong to Worker
  hardening and the full release-complete gate; #19 client-done does not
  gate them and they do not gate #19.

## Implementation breakdown (subtasks under #19)

Each phase is a separate PR. The phases are ordered by dependency, not by
calendar — multiple phases may be in flight simultaneously when their
dependencies are met.

### Phase 1: shared auth contract test fixtures

Owner: @Tools-MCP-Lead (joint with @Provider-Auth-Lead on auth shape).

Adds the contract definitions above as Rust types in
`crates/coding-agent/src/hub_auth.rs` plus a faux Worker fixture and the
five contract assertions. No CLI command behavior changes; this is the
test-and-types foundation Phases 2 and 3 build on.

### Phase 2: Worker `/auth/start` and `/auth/exchange_code`

Owner: @Tools-MCP-Lead (Worker is in `workers/fefe-hub`).

Adds the two endpoints. PKCE verifier, state, and request-id binding all
verified server-side; no token leakage. Reuses the existing `/auth/register`
storage layer for the human account and the existing `register_agent` path
for agent creation; the new code is the join orchestration.

### Phase 3: CLI `/hub join` and loopback callback

Owner: @CLI-TUI-Dev-Lead.

Replaces the `/hub connect` + `/hub login` two-step from PR #145 with a
single `/hub join` that opens the browser, runs the loopback callback,
exchanges the code, and writes the credential to the auth store. `/hub
connect` and `/hub login` remain as advanced or recovery commands but are
not the documented happy path.

### Phase 4: `/hub send` autocomplete and bounded inbox view

Owner: @CLI-TUI-Dev-Lead.

`/hub send` autocomplete reads `discover_public_agents` listing schema +
local trust state. `/hub inbox` provides a bounded read-only view of the
hub-backlog (per §2.3 `list_my_inbox`) plus an audit list of recent
`trigger_prompt` outcomes (Accept once / Always / Block / Deny / Skip with
their bounded metadata). No retry-from-inbox in v0 — see [Skip semantics](#skip-semantics).

### Phase 5: first-contact prompt card

Owner: @CLI-TUI-Dev-Lead (UX layer) joint with @Runtime-dev-lead (runtime
hookup).

Renders the `TriggerPromptRequest` event (from #110 sub-PR 4 / PR #142) as
the trigger prompt card with the layout in Screen 4. The card preview field
is the bounded `trigger_summary` (UI cap 120 chars) — never the raw Local
payload, raw `_meta`, sender IP, or raw `agent_id` UUID. Skip maps to
`TriggerPromptDecision::Timeout` with the `deferred_by_user` audit reason.
Accept once / Always / Block / Deny map to their respective trust/audit
semantics from §5.7.

### Phase 6: live e2e

Owner: @Runtime-dev-lead (live trigger pipeline) joint with @QA-Release-Lead
(acceptance).

Two real pie clients exercise the full happy path against
`https://pie.0xfefe.me`. Bounded report posted per §8.6. Closes task #19.

## Out of scope (deliberately deferred past v0)

- Inviting a user who has not yet joined the hub (referral / share link).
- Group / multi-recipient send.
- File / blob transfer (notification payload only; large objects deferred per
  RFC #18 out-of-scope list).
- Hub federation across regions.
- `/hub` desktop notifications (OS-level toasts).

## Open questions

| ID            | Question                                                                                          | Take                                                                                                       |
| ------------- | ------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| #19.OQ-1      | First-launch banner: persistent until dismissed, or shown N times?                                | Shown once on first launch, dismissible with a single key, then never again unless `/hub` config is empty. |
| #19.OQ-2      | If browser does not return within `expires_in_seconds`, what does the user see?                   | `pie` cancels the loopback, prints `Browser login timed out. Try /hub join again.`                          |
| #19.OQ-3      | Handle auto-derivation: from username, or always prompt?                                          | Auto-derive at registration; user can change later via `/hub profile --handle`. Conflict → suffix.        |
| #19.OQ-4      | Should `/hub send` permit free-text targets, or only autocompleted ones?                          | Allow both. Free-text triggers a `not_found` recovery if unresolved.                                      |
| ~~#19.OQ-5~~  | ~~Defer TTL?~~                                                                                    | **Removed.** v0 Skip (internally `Timeout` with `reason = "deferred_by_user"`) is terminal; no retry, no TTL. Retry-from-inbox is a deliberate v1 follow-up tracked outside this OQ table. |

## Change log

| Date       | By     | Change                                                                                       |
| ---------- | ------ | -------------------------------------------------------------------------------------------- |
| 2026-05-30 | @alice | v0.1 draft: happy path sketch, auth wire contract (preflight + exchange + PKCE), Defer semantics over Timeout, acceptance gates, six implementation phases. |
| 2026-05-30 | @alice | v0.2: locked-defaults follow-up — built-in default hub configuration model (no `mcp.toml` required, canonical `mcp:pie-hub:` source label, custom servers get independent trust scope); first-contact card preview is bounded `trigger_summary` capped at 120 chars (UI cap on top of hub's 240-char `_meta.pie_summary` bound), not raw Local payload; explicit redaction additions for bare PKCE `state`, raw `agent_id` UUID, sender IP. |
| 2026-05-31 | @alice | v0.3: terminology cleanup after QA bug sweep (task #44). User-facing label is **Skip** everywhere (key map, acceptance bullets, feed/audit list, Phase 5 spec); `deferred_by_user` is internal audit reason only. Renamed `## Defer semantics` → `## Skip semantics` with an explicit terminology note so future readers don't mix the two. |
