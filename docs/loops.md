# Loops: stateful cron jobs + the triage inbox

> "Stop prompting the agent. Build loops that prompt the agent for you."
> — the design north star, borrowed from Addy Osmani's
> [Loop Engineering](https://addyosmani.com/blog/loop-engineering/) (2026).

`pie` already had the heartbeat: cron jobs and triggers can run a prompt on a schedule or
when something happens. But a plain cron job is an amnesiac — every run starts from a fresh
context, so "tell me what changed since yesterday" was impossible, because there was no
yesterday. And whatever the job found either interrupted your chat or sank into the audit
log.

Loops fix both halves:

1. **Stateful cron jobs** give a recurring job a memory file — a small notebook the agent
   writes at the end of each run and reads back at the start of the next.
2. **The triage inbox** gives findings a place to land that is *not* your conversation.
   You review them when convenient, like email, and promote the ones that matter into a
   real agent turn with one command.

## Quick start

```text
/cron add --stateful "0 9 * * *" check the GitHub issues of this repo and report anything new or newly closed since the last run
```

That's the whole setup. Every morning at 09:00:

- the job runs in a **sub-agent with a fresh context** — your main conversation is never
  interrupted;
- the prompt is automatically prefixed with the agent's own notes from the previous run
  (`(first run)` the first time);
- the agent does the work, then ends its reply with two kinds of structured tags:
  - `<loop-state>…</loop-state>` — its notes for tomorrow's run (replaces the saved state);
  - `<inbox>one-line finding</inbox>` — one tag per thing a human should look at.
- `pie` extracts the tags: state goes to the job's state file, findings go to the inbox.
  A run with no `<inbox>` tags is a quiet run — your inbox stays clean.

Then, whenever you sit down:

```text
/inbox                 # list new findings
/inbox claim 1         # feed finding #1 into the main chat as a real agent turn
/inbox dismiss 2       # not interesting
/inbox clear           # dismiss everything new
/inbox all             # include claimed/dismissed history
```

`/inbox claim` is the payoff: it converts a finding into a normal prompt
("Address this finding from loop …") in your main session, with the same streaming,
abort, and approval semantics as anything you type yourself.

The TUI trigger panel and the web/relay sidebar show a live `Inbox: N new` badge, so you
notice findings without polling.

## The design idea

Osmani's article breaks a production agent loop into six elements; `pie` had four of them
(a heartbeat, skills, connectors, an evaluator) and was missing two. This feature is
exactly those two, and nothing else:

**The state spine.** A loop is only a loop if run *N+1* knows what run *N* saw. Before
this, people hand-pasted "baseline" blobs into their cron prompts, and the blobs rotted
immediately. Now the agent maintains its own baseline, in its own words, in a file it
rewrites every run. The contract is deliberately humble: it's a scratchpad capped at
2000 characters, not a database. If you need real state, the agent can keep files or call
tools — the spine is just enough memory to know what "new" means.

**The routing layer.** Automation output needs a destination that is neither "interrupt
the human now" nor "write-only log." The inbox is that third place. It's a global
JSONL file (`~/.pie/inbox.jsonl`) shared across all sessions and projects — the inbox is
what you open in the morning, wherever the loops ran. Findings are one-liners, capped and
bounded, with a `new → claimed/dismissed` lifecycle. Triage is a human act; acting on a
finding is an agent act; the inbox is the boundary between them.

A few deliberate choices fall out of this:

- **Stateful jobs never touch the main chat.** Regular cron jobs use the inject-and-run
  path (the result appears in your conversation). Stateful jobs switch to the sub-agent
  path: fresh context in, tags out. Loops are background creatures; the inbox is their
  only voice.
- **The protocol is plain text, not an API.** The output contract is injected into the
  job prompt as instructions, and the tags are parsed from the sub-agent's final reply.
  Any model that can follow instructions can run a loop — nothing provider-specific.
- **Tag extraction never fails a run.** Malformed or missing tags mean: state file
  untouched, nothing enters the inbox, run still completes. A truncated tag is ignored.
  Corrupt inbox lines are skipped on read, never deleted.
- **Everything is bounded.** State ≤ 2000 chars (truncated with a marker), inbox entries
  ≤ 500 chars, at most 16 findings honored per run. A runaway loop can't flood your disk
  or your attention.

## Reference

### Creating loops

| Surface | How |
|---------|-----|
| Slash command | `/cron add --stateful "<minute hour dom month dow>" <prompt>` |
| Natural language | ask in chat, e.g. *"every hour, check … and keep notes between runs"* — the `NewCronJob` tool has a `stateful` flag the agent sets |
| Inspect | `/cron list` shows a `[stateful]` marker on loop jobs |
| Remove | `/cron remove <id>` deletes the job and its state file |

### The injected prompt shape

What the sub-agent actually receives each run:

```text
[loop-state] (your notes from the previous run)
<contents of the state file, or "(first run)">
[/loop-state]

<your job prompt>

Output protocol (mandatory):
- End your reply with <loop-state>…</loop-state>: notes for the next run.
- For each finding a human should look at, emit <inbox>one-line finding</inbox>.
- No <inbox> tag means a quiet run; the inbox stays clean.
```

### Inbox commands

| Command | What it does |
|---------|--------------|
| `/inbox` | List new findings with numbers |
| `/inbox all` | Include claimed and dismissed entries |
| `/inbox claim <n\|inb-id>` | Mark claimed and start a main-chat turn on the finding |
| `/inbox dismiss <n\|inb-id>` | Mark dismissed |
| `/inbox clear` | Dismiss all new entries |

### Storage

| Path | What |
|------|------|
| `~/.pie/sessions/<cwd-hash>/<session>.cron.toml` | The job itself (session-scoped, restored by `--resume`) |
| `~/.pie/sessions/<cwd-hash>/<session>.loop-<job-id>.md` | The job's loop state (plain Markdown — you can read or edit it) |
| `~/.pie/inbox.jsonl` | The global inbox (append-friendly JSONL, status rewrites are serialized) |

Loop state is session-scoped like the job that owns it, and is human-readable on purpose:
`cat` it to see what your loop "remembers," or edit it to correct the agent's notes.

### What's next (not yet built)

- **Maker/checker verification**: an optional second sub-agent adversarially reviews
  findings before they enter the inbox (phase 3 of the design,
  [docs/issues/23-loops-inbox.md](issues/23-loops-inbox.md)).
- Bundling loop state into `/session export` archives.
- A web inbox panel with claim/dismiss buttons (today the web sidebar shows the count;
  triage happens via `/inbox`).
