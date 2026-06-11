# Session export / import for replayable agent backups

> Parent: master roadmap issue.
> Tier: 2 (session/state).
> Owner split: Runtime designs the artifact and import semantics; CLI/TUI owns user commands and
> UX review/implementation.

## Goal

Users can back up a `pie` session into a portable artifact, move it to a new environment, import
it into a fresh `pie` install, and resume/replay the session as faithfully as possible.

The artifact must include the recoverable state needed for replay:

1. Conversation context and full session tree.
2. Tool call transcript and tool result history already recorded in the session.
3. Session-scoped dynamic trigger rules.
4. Session-scoped cron jobs.
5. Session-scoped endpoint bindings only as inert metadata unless the endpoint feature is still
   present and explicitly re-bound.

The artifact must not bundle extra credential stores, provider API keys, bearer tokens, OAuth
state, code/verifier values, shell history, MCP config, or live process handles.

The artifact does preserve the session transcript and tool history exactly enough to replay the
session. Those entries may already contain pasted secrets, tool arguments, provider error bodies,
or command output from the original run. Treat every `.piesession` as sensitive user data even
though it does not add separate auth stores or provider credentials. CLI/TUI output must print a
bounded sensitivity warning and must not echo transcript/tool contents while exporting/importing.

## User surface

Recommended CLI/TUI entry points:

```text
pie session export [--session <id>|--current] [--output <file>] [--exclude-triggers]
pie session import <file> [--cwd <path>] [--activate-triggers=off|on]
pie --resume-id <imported-id>
```

`--activate-triggers=ask` is reserved for a future interactive confirmation flow. The v1
implementation rejects it explicitly instead of silently treating it as `off`.

Inside the REPL, mirror the same behavior:

```text
/session export [path]
/session import <path>
```

`/save` and `/share` remain human-readable transcript export features. They are not suitable for
restoring runtime state and should not be reused as the backup format.

## Artifact format

Use one file with a deterministic, inspectable layout. v1 (implemented) is an **uncompressed**
tar archive. Compression is deferred: because `manifest.json` lives inside the archive, a v2
that adds zstd/gzip must detect the framing by magic bytes, not by manifest fields. The archive
file is created owner-only (0600) — it carries the full transcript — and export refuses to
overwrite an existing output file instead of truncating it.

```text
pie-session-<session-id>.piesession
  manifest.json
  session.jsonl
  sidecars/triggers.json       optional
  sidecars/cron.toml           optional
  sidecars/endpoints.json      deferred; not emitted in v1
  attachments/                 reserved; empty in v1 unless image/file blobs become session-owned
```

`manifest.json` is the contract:

```json
{
  "schema": "pie.session_export.v1",
  "created_at": "2026-06-09T00:00:00Z",
  "pie_version": "0.1.0",
  "source": {
    "session_id": "019...",
    "cwd": "/original/path",
    "session_path": "/.../019....jsonl"
  },
  "content": {
    "session_jsonl_sha256": "...",
    "entry_count": 42,
    "active_leaf_id": "019...",
    "has_triggers": true,
    "has_cron": true,
    "has_endpoints": false
  },
  "sensitivity": {
    "session_transcript_preserved": true,
    "separate_auth_stores_included": false,
    "provider_credentials_included": false,
    "mcp_config_included": false
  }
}
```

The manifest may include original paths for user orientation, but import must never write to those
paths. Import chooses a destination under the current environment's session repo.

## Runtime architecture

The module lives in the coding-agent crate, not `pie-agent-core` as originally sketched:
import has to rewrite trigger/cron sidecar semantics (disable rules, clear stale cron state),
and those sidecar types are defined in `crates/coding-agent/src/triggers/`. Moving the module
into the agent crate would either invert that dependency or force the runtime layer to treat
sidecars as opaque bytes it cannot rewrite.

```text
crates/coding-agent/src/session_archive.rs
  Manifest
  export_session(...)
  import_session(...)
  commit_import(...)
```

The only agent-crate surface is `JsonlSessionMetadata.imported_from`
(`SessionImportOrigin`): an imported session's header records the original session id, original
cwd, export timestamp, and exporting pie version, so provenance survives in the session file
itself.

Responsibilities:

- Read and validate a `JsonlSessionStorage` file plus its sidecar file bytes.
- Validate session JSONL header + every `SessionTreeEntry`.
- Preserve entry IDs and parent graph exactly.
- Preserve the active leaf by preserving the append-only `leaf` entries.
- Rewrite only file-path metadata that must be local to the new environment:
  - `metadata.path` becomes the new destination `.jsonl` path.
  - `metadata.cwd` is either the requested `--cwd` or current cwd.
  - `metadata.parentSessionPath` is dropped unless explicitly supported later.
- Generate a new session file name/id on import by default, while recording the original
  session id in import metadata. Optional `--preserve-id` can be added later, but must fail if
  it collides.

The runtime layer should not know CLI flags, terminal prompts, or TUI rendering. CLI/TUI chooses
the source/destination paths and prints the summary.

## Import semantics

Default import is safe and inert:

- Conversation/session tree is restored and resumable.
- Dynamic trigger rules, including both enabled and disabled definitions, export by default as
  part of a complete backup. Import keeps all dynamic trigger rules disabled unless the user
  passes `--activate-triggers=on`. Activation never widens what the source had: `on` restores
  each rule's own `enabled` flag (a rule the user disabled at the source stays disabled), it
  does not force-enable everything. `fired_at` history is preserved in every mode so a
  fire-once rule that already fired does not re-fire after import. `ask` is reserved and
  currently unsupported until the CLI/TUI confirmation path exists.
- Cron jobs follow the same activation rule (`on` restores per-job `enabled`, never widens).
  `running_trace_id`, `last_due_at`, `last_error`, and overlap counters are cleared so a moved
  session does not immediately fire old work; `last_fired_at` / `last_completed_at` are kept as
  history.
- Endpoint bindings are deferred in v1: `sidecars/endpoints.json` is not exported. When added,
  they import as metadata only and disabled/unbound by default — public URLs, remote endpoint
  IDs, and hub credentials must not be assumed valid in the new environment.
- Tool call history is replayed only as transcript/context. Import must not re-run prior tool
  calls automatically.

Resume after import uses the same `Session::build_context()` path as normal resume. This makes
compaction, branch summaries, custom messages, model changes, and thinking-level changes obey
existing replay semantics.

## State classification

| State | Export | Import |
|---|---|---|
| Session JSONL header + entries | yes | byte-preserve entries; rewrite local path/cwd metadata |
| Active leaf / branch graph | yes | preserve via existing `leaf` entries |
| User/assistant/tool transcript | yes | context replay only; no tool re-execution |
| Model/thinking changes | yes | preserve as session entries |
| Compaction summaries | yes | preserve and replay through `build_context()` |
| Dynamic triggers | yes, enabled and disabled definitions by default | import disabled by default; `--activate-triggers=on` restores per-rule `enabled` (never widens); `fired_at` preserved; `ask` reserved/unsupported |
| Cron jobs | yes | import disabled by default; `on` restores per-job `enabled`; clear stale running state, keep fired/completed history |
| Endpoint bindings | deferred in v1 (not exported) | when added: metadata only, disabled/unbound by default |
| Skills/templates installed on disk | manifest references only in v1 | warn if missing; do not bundle by default |
| Provider/API credentials | no | user must login/configure locally |
| MCP server config | no in v1 | warn if referenced tools are unavailable |
| Raw external/local payloads | no unless already in session JSONL | no rehydration beyond recorded transcript |
| Live processes, timers, SSE/websocket state | no | recreated from imported trigger/cron configs only when enabled |

## Stability and failure modes

- Validate before writing. Import extracts to memory (sizes are capped per archive member),
  validates checksums/schema/graph and parses sidecars first, then commits: the session is
  staged at a `.jsonl.tmp` path (invisible to repo listings), replay-validated via
  `build_context()`, sidecars are written, and the rename of the staged session into place is
  the commit point. Any failure removes everything written — a failed import leaves no orphan
  or partial session.
- Fail closed on malformed JSONL, parent graph cycles, missing parent IDs, checksum mismatch,
  unsafe archive paths, duplicate destination session id, or sidecar parse failure.
- Never partially activate triggers/crons. Sidecars are parsed and rewritten before any file is
  created, so a sidecar problem aborts the import as a whole.
- Keep artifact extraction path-safe: reject absolute paths, `..`, symlinks, hardlinks, devices,
  and unknown top-level files unless explicitly allowed by a future schema.
- Redact output summaries. Do not print full session JSONL, full sidecars, tool arguments, or
  model outputs in success/error messages.

## Extensibility

- Schema version is explicit: `pie.session_export.v1`.
- Unknown manifest fields are tolerated; unknown files are rejected in v1.
- Future versions can add:
  - bundled attachments for image/file blocks once session-owned blob storage exists;
  - encrypted artifacts;
  - skill/template bundle options;
  - cross-session branch bundles;
  - verified MCP config snapshots.

## Performance

- Stream archive read/write where possible; do not load large attachments into memory.
- JSONL validation can be line-by-line. A v1 implementation may load sidecars fully because
  triggers/cron files are small.
- Export/import should be cancellable from the CLI/TUI command path and should not block the TUI
  event loop for long file copies.

## Testing

| Layer | What |
|---|---|
| unit | Manifest round-trip; path traversal rejection; checksum mismatch; session graph validation; metadata path/cwd rewrite. |
| integration | Export a temp Jsonl session with messages, tool result, compaction, model/thinking changes, labels, and leaf moves; import into a different repo; `build_context()` matches the source context. |
| integration | Export/import dynamic triggers sidecar; imported rules are present but disabled by default unless activation is requested. |
| integration | Export/import cron sidecar; imported jobs are present but disabled and stale running state is cleared. |
| CLI e2e | `pie session export --current --output x.piesession`; `pie session import x.piesession --cwd <new>`; `pie --resume-id <new-id>` shows the same active context. |
| redaction | Export/import summaries warn that the artifact is sensitive but do not echo credentials, transcript/tool payloads, raw sidecar bodies, or archive internals. |

## Acceptance criteria

- A session with user/assistant/tool messages, model change, thinking-level change, compaction,
  branch/leaf movement, dynamic triggers, and cron jobs exports to a single artifact.
- Importing the artifact into a new session repo creates a resumable session with the same
  active conversation context.
- Imported trigger and cron configs are visible but not active by default.
- The command output clearly reports what was restored, what was disabled, and what requires
  local reconfiguration.
- No extra credential stores, provider tokens, OAuth secrets, or MCP config are bundled. The
  preserved transcript/tool history may contain user-provided sensitive content, so command output
  warns that the artifact is sensitive without printing that content.

## Out of scope

- Re-running historical tool calls.
- Moving provider credentials or OAuth sessions.
- Rebinding external endpoints automatically.
- Guaranteeing deterministic model output after resume. The goal is to replay state/context,
  not reproduce provider responses bit-for-bit.
- Cross-machine restoration of files that tools created outside the session store.
