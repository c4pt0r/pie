# Web UI parity gate

The browser UI is a TUI alternative, not a reduced chat-only surface. Every Web UI release must
either implement the existing TUI behavior below or explicitly mark it as a blocked follow-up
before release.

## PR A: shared kernel, no web server

- Extract shared REPL execution logic without adding HTTP/SSE/browser dependencies.
- Keep TUI behavior unchanged for normal prompts, slash-driven turns, queued prompts, abort, and
  inject-and-run trigger turns.
- Preserve one serialized turn slot. User prompts, `/template`, `/new-trigger`, `/compact`, and
  inject-and-run trigger continuations must not race the same `AgentHarness`.

## Input and run control

- Enter sends; Shift/Alt+Enter inserts a newline.
- Submitting while busy queues the next turn instead of dropping or racing it.
- Queued turns run FIFO when the active turn finishes.
- Users can remove queued turns before they start.
- Abort cancels the current turn through `AgentHarness::abort()`.
- Busy, queued count, and aborted/error status remain visible.

## Input affordances

- Prompt history remains reachable.
- Slash command completion remains reachable.
- `@file` mention expansion still runs before prompt submission.
- `/skill <name>` attaches the skill to the next user prompt only, and the UI shows that pending
  attachment until it is consumed.

## Attachments

- Image upload/paste follows existing validation: model capability preflight, per-image cap, and
  max images per message.
- Image-only prompts are valid and visibly labelled.
- Feed/SSE/debug surfaces never echo image base64.

## Feed output

- Assistant text, thinking deltas, tool calls, tool progress, tool results, turn errors, and turn
  completion all render.
- Tool previews and results use the same display caps and redaction as the TUI/debug path.
- Replay/resume output remains display-compacted; live and replayed tool results should not diverge.
- Browser text selection should work naturally; this is one reason to provide the Web UI.

## Triggers, MCP, and hooks

- Trigger fired/running/completed/failed/deduped/permission-denied lines render live.
- Dynamic periodic no-match checks stay quiet unless debug mode is enabled.
- The automation/status surface has equivalents for dynamic rules, enabled/disabled state,
  once/repeat mode, redacted rule previews, MCP server/tool count, MCP notification hooks, active
  hook points, and trigger runtime features.
- `/triggers status|rules|sources|running|audit|abort` remains available.

## Slash commands

The Web UI must support the current command registry:

- `/help`, `/clear`, `/skills`, `/skill`, `/quit`
- `/model`, `/thinking`, `/cost`, `/diag`
- `/template`, `/save`, `/compact`, `/undo`, `/bug-report`
- `/name`, `/sessions`, `/share`
- `/login`, `/logout`
- `/find`, `/history`
- `/triggers`, `/new-trigger`

Browser-specific treatment is allowed where appropriate: `/quit` may disconnect/close the session,
and `/login` should use a non-echoing password input rather than writing the secret into feed or
history.

## Session, model, auth, and diagnostics

- Startup diagnostics remain visible: model, session id/resume state, tools, skills/templates, MCP,
  triggers, hooks, and debug state.
- Model switching performs credential preflight and gives recovery actions (`/login <provider>` or
  provider env vars).
- Cost, debug, and bug-report output stay redacted and bounded.
- Resume/continue/name/session list/export/share have equivalent browser entry points or are
  explicitly held for a follow-up before release.

## Security and transport

- `pie web` must bind to loopback by default.
- Remote bind without an explicit token/auth story is a release blocker.
- SSE/state endpoints send incremental or bounded compact snapshots, not repeated full transcripts.
- API keys, auth-store values, base64 images, raw local trigger payloads, and raw oversized tool
  payloads must never be exposed over Web UI events.
