# Web relay — mount a live session at pie.0xfefe.me/session/&lt;token&gt;

> Parent: master roadmap issue.
> Owner: c4pt0r (fresh design per the #18 de-scope follow-up requirement: new
> public-network surfaces start from a new threat model with a named owner).
> Status: designed 2026-06-11; implemented in the same PR series.

## Goal

`/web-connect` in a running pie session prints a URL like
`https://pie.0xfefe.me/session/3k9f…` . Anyone who has that URL can watch the
conversation live in a browser **and send prompts to the agent**. `/web-disconnect`
(or exiting pie) kills the page. Nothing is stored server-side beyond the lifetime of
the connection.

This is NOT a resurrection of the removed fefe hub (#192–#197): no accounts, no
onboarding, no cross-agent messaging, no MCP ingress, no D1 writes. The tombstoned
paths (`/auth/*`, `/chat*`, `/mcp`, `/login`) stay 410. The only new public surface is
the relay namespace below.

## Architecture

```text
local pie (TUI or --web)         Cloudflare worker pie.0xfefe.me        any browser
┌───────────────┐  outbound WSS   ┌──────────────────────────┐   SSE    ┌────────┐
│ relay client  ├────────────────►│ SessionRelay (DO / token)│◄─────────┤ viewer │
│ push snapshots│                 │ · latest snapshot (mem)  │ GET /session/<token>
│ recv prompts  │◄────────────────│ · broadcast to viewers   │ POST /session/<token>/prompt
└───────────────┘                 │ · forward prompts to WS  │          └────────┘
```

- The laptop only makes an **outbound** WebSocket connection; no inbound port, no NAT
  concerns. Agent offline ⇒ viewers see an "agent offline" banner; DO holds no
  transcript once connections drop (in-memory only, no persistence).
- One Durable Object instance per view token. No D1, no KV.

## Token model

- **view token** — 160-bit random, base32, generated locally per `/web-connect`. It is
  the URL path segment and is a capability: holding it grants watch + prompt.
- **agent key** — second 160-bit random, never leaves the local process except in the
  agent WS handshake header. The DO pins it on first agent connect (trust on first
  use); later agent connects must present the same key. This prevents a view-token
  holder from impersonating the agent side.
- `/web-disconnect` closes the WS with a `shutdown` frame; the DO drops state and
  subsequent viewer requests get 404. Every `/web-connect` mints fresh tokens.

## Local surface

- `/web-connect` — mint tokens, spawn the relay task, print the URL plus a bounded
  warning: the link grants both viewing the full transcript **and prompting the
  agent**. Works in TUI and `--web` modes.
- `/web-connect status` — connected/reconnecting/off + viewer count (as reported by
  the DO).
- `/web-disconnect` — stop the task, invalidate the page.
- Config: `[relay] base_url = "https://pie.0xfefe.me"` in `~/.pie/config.toml`
  (override for self-hosted workers / wrangler dev).
- Remote prompts are injected through the same serialized run queue as local web UI
  submissions (`WebCommand::Submit` path) — no new concurrency surface.
- Snapshot push: on every feed update / turn end (debounced to ≥250ms), the relay
  task sends the same `WebSnapshot` JSON the local web UI uses.
- New dependency: `tokio-tungstenite` (WS client).

## Worker surface (`workers/fefe-hub`)

```text
GET  /relay/agent?token=<view_token>     WebSocket upgrade, header x-pie-agent-key
GET  /session/<token>                    viewer HTML (shared asset, see below)
GET  /session/<token>/state              latest snapshot JSON (404 if unknown token)
GET  /session/<token>/events             SSE: snapshot frames + offline/online events
POST /session/<token>/prompt             {text} → forwarded to agent WS
POST /session/<token>/abort              forwarded (same trust level as prompt)
POST /session/<token>/complete           200 with empty completions (not relayed)
POST /session/<token>/control-plane/resolve   403 — see security boundary
```

- `SessionRelay` DO: keeps the agent WS, the pinned agent key, the latest snapshot,
  and the set of viewer SSE streams. Memory only.
- Tombstoned legacy paths keep returning 410; `/health` keeps reporting
  `status: "disabled"` for the legacy service and gains `relay: "enabled"`.

## Shared viewer HTML

The local web UI's `INDEX_HTML` moves out of `web.rs` into
`crates/coding-agent/src/ui/web_index.html` (`include_str!` locally). The worker
bundles the same file and serves it at `/session/<token>`. The page already talks to
relative `state` / `events` / `prompt` endpoints, so it works unchanged under the
session prefix; endpoints the relay doesn't support degrade gracefully (empty
completions, 403 approvals → the page shows "approve locally").

## Security boundary (decided 2026-06-11)

- The capability URL grants **watch + prompt + abort**, not approval. Control-plane
  prompts (permission confirmations) can only be resolved locally (terminal or local
  web UI). A leaked link can ask the agent to do something dangerous but cannot
  approve it under the default permission policy.
- A future `/web-connect --allow-remote-approval` may relax this per-connection; not
  in v1.
- The relay never carries provider credentials, auth.json contents, or session files —
  only rendered snapshots and prompt text.
- Worker accepts relay creation from anyone (it is the owner's deployment); abuse
  control is out of scope for v1 beyond DO-per-token isolation and snapshot size caps.

## Failure modes

- Agent WS drop: relay client reconnects with exponential backoff (1s..60s); viewers
  see `agent_offline` within one heartbeat interval (~15s ping).
- Worker unreachable at `/web-connect`: command fails with a clear error; nothing
  starts.
- Snapshot oversize: frames over 1 MiB are dropped with a local warning (feed history
  is bounded upstream by the web UI's own limits).
- DO eviction (Cloudflare restart): viewers reconnect via SSE retry; agent reconnect
  re-pins the agent key (a fresh DO accepts the first key it sees again — acceptable
  for v1: the view token is still required, and the agent reconnects within seconds).

## Testing

| Layer | What |
|---|---|
| unit (rust) | token format/entropy; relay state machine transitions; snapshot debounce; /web-connect output contains warning + URL, never transcript |
| unit (rust) | remote prompt frame → injected through the run-queue path (reuse web command test scaffolding) |
| worker (vitest) | TOFU agent-key pinning; snapshot broadcast to N viewers; prompt forward; 404 unknown token; legacy paths still 410; control-plane resolve 403 |
| e2e | pie + `wrangler dev`: /web-connect against localhost worker, browser-equivalent fetch of /state, POST /prompt round-trips into the local turn queue |

## Out of scope (v1)

- Remote approval of control-plane prompts.
- Persistence / replay after disconnect.
- Viewer auth beyond the capability URL.
- Multiple concurrent relays per pie process (one active relay; reconnect reuses it).
- Rate limiting / abuse handling on the worker.
