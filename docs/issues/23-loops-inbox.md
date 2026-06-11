# Loops: stateful cron jobs + triage inbox

> Parent: master roadmap issue.
> Inspiration: Addy Osmani, "Loop Engineering" (2026) — design loops that prompt the
> agent instead of prompting it yourself. pie already has the heartbeat (cron/triggers),
> skills, MCP connectors, and a separate `/goal` evaluator; this issue adds the two
> missing pieces: **external state across runs** (the spine) and a **triage inbox**
> (results route somewhere a human actually looks).
> Status: designed + implemented 2026-06-11 (phase 1+2; maker/checker verification is
> phase 3, tracked separately).

## Problems

1. A cron job's sub-agent starts from a fresh context every run. "Check for new issues
   and tell me what changed since yesterday" is impossible — there is no yesterday.
   Workarounds push state into the prompt by hand (the GitHub-issues baseline blob seen
   in real usage) and rot immediately.
2. Automation output today either interrupts the main chat (`InjectAndRun`) or is
   buried in the audit log. There is no "findings land in an inbox, human triages when
   convenient" workflow — the article's core routing pattern.

## Design

### Stateful cron jobs

`CronJob` gains `stateful: bool` (default false; serde-default so existing sidecars
load unchanged). For a stateful job the cron action hook switches delivery from
`InjectAndRun` to **`SubAgent`** — a fresh-context run that does not touch the main
conversation — and assembles the prompt as:

```text
[loop-state] (your notes from the previous run)
<contents of the state file, or "(first run)">
[/loop-state]

<job action text>

Output protocol (mandatory):
- End your reply with <loop-state>…</loop-state>: notes for the next run
  (replaces the saved state; keep under 2000 chars).
- For each finding a human should look at, emit <inbox>one-line finding</inbox>.
- No <inbox> tag means a quiet run; the inbox stays clean.
```

The SubAgent path's `TriggerCompleted { summary }` carries the sub-agent's final text
(≤4 KiB, the existing cap). `cron_harness_listener` — which already resolves
`trace_id → job` to call `mark_completed` — additionally extracts:

- `<loop-state>…</loop-state>` → written to the job's state file;
- every `<inbox>…</inbox>` → appended to the global inbox.

State file: `<session-stem>.loop-<job-id-prefix>.md` next to the cron sidecar
(session-scoped like the job itself; bounded at 2000 chars, truncated with a marker).
`/cron remove` deletes it; `/session export` does not bundle it in v1.

Surface: `/cron add --stateful "<schedule>" <prompt>`, and a `stateful` parameter on
the `NewCronJob` tool. `/cron list` shows a `stateful` marker.

### Triage inbox

Global, append-friendly JSONL at `~/.pie/inbox.jsonl` (cross-session: the inbox is
what you open in the morning, wherever the loops ran):

```json
{"id":"inb-…","created_at":"…","source":"cron:check-issues","text":"…(≤500 chars)",
 "trace_id":"cron-…","session_id":"019…","status":"new"}
```

- `/inbox` — list `new` entries (and counts); `/inbox all` includes claimed/dismissed.
- `/inbox claim <n>` — marks claimed and feeds the finding into the main conversation
  as a prompt ("Address this finding from loop …: <text>") via the existing
  `RunAgentPrompt` path, so Ctrl-C/abort semantics match normal turns.
- `/inbox dismiss <n>` — marks dismissed. `/inbox clear` dismisses all new.
- Sidebar (TUI trigger panel + web/relay sidebar) shows `inbox: N new` read live from
  the store; the existing TriggerCompleted repaint flow keeps it fresh.

Status changes rewrite the file (small, bounded); writes serialize through a process
mutex. Concurrent pie processes appending is tolerated (append-only adds; rewrite
races are last-writer-wins on status — acceptable for v1).

### Bounds and failure modes

- State capped at 2000 chars; inbox text capped at 500 chars per entry; at most 16
  inbox tags honored per run (the rest dropped with a count note in the audit).
- Malformed/missing tags: state file untouched, nothing enters the inbox; the run is
  still marked completed. Tag extraction never fails a run.
- The 4 KiB summary cap means an over-long reply can truncate tags mid-stream; the
  injected protocol tells the model to keep the tail short. A truncated open tag is
  ignored (fail quiet).
- Inbox file corruption: unparseable lines are skipped on read, never deleted.

## Testing

| Layer | What |
|---|---|
| unit | tag extraction (single/multiple/absent/truncated/oversized); inbox append/list/claim/dismiss round-trip; corrupt-line tolerance; state file write/cap |
| unit | stateful action hook returns SubAgent delivery with injected state + protocol; non-stateful jobs keep InjectAndRun |
| integration | listener on TriggerCompleted writes state + inbox for the matching job only |
| e2e | real model: stateful job sees first-run marker, writes state; second run sees the previous state; finding lands in inbox; `/inbox claim` starts a turn |

## Phase 3 (separate)

`verify = true` on a job: a second sub-agent (adversarial prompt) reviews the first's
findings before they enter the inbox — maker/checker separation per the article.

## Out of scope

- Bundling loop state in `/session export` (v2 of #20's schema).
- A web inbox panel with claim buttons (the sidebar count + `/inbox` is v1).
- Worktree isolation for parallel loops.
