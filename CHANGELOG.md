# Changelog

All notable changes to this project. Format loosely based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versions sync across all workspace crates per the lockstep policy in `AGENTS.md`.

## [Unreleased]

### Added — Tier 1 (daily UX)

- **#2** Mid-stream Ctrl-C abort with double-Ctrl-C exit. Biased select against stalled
  streams (closes #18).
- **#3** Slash-command registry with 21 builtins: `/help`, `/clear`, `/skills`, `/skill`,
  `/quit` (+ `/exit`, `/q`), `/model`, `/thinking`, `/cost`, `/diag`, `/template`,
  `/save`, `/compact`, `/undo`, `/bug-report`, `/name`, `/sessions`, `/share`, `/login`,
  `/logout`, `/find`, `/history`.
- **#25 PR B** `/skill <name>` attaches an already-loaded skill to the next prompt, and
  `/skills` now shows source and `disable_model_invocation` status without printing skill bodies.
- **#32** Optional bundled `karpathy-guidelines` built-in skill. Off by default; enable
  per-run with `--builtin-skill karpathy-guidelines` or persistently via
  `~/.pie/config.toml` `[builtin_skills] enabled = ["karpathy-guidelines"]`. CLI and config
  inputs are unioned and de-duplicated. Unknown names from `--builtin-skill` hard-fail with
  the available list; unknown names in config produce a startup diagnostic but never
  silently enable anything. User and project skills with the same name still win over the
  built-in. Skill source (verbatim `SKILL.md` from
  [`multica-ai/andrej-karpathy-skills`](https://github.com/multica-ai/andrej-karpathy-skills))
  is vendored under `crates/coding-agent/skills/karpathy-guidelines/` with MIT attribution.
- **#37** Local OpenAI-compatible model configs. `pie` now loads
  `~/.pie/models.json` and `<cwd>/.pie/models.json`, registers those entries through
  `pie_ai::register_custom_model`, and lets users select them with
  `--provider <local-provider> --model <model-id>`. This enables local servers such as
  DS4 (`deepseek-v4-flash` at `http://127.0.0.1:8000/v1`) without adding a one-off
  provider implementation. Project-local model entries override user-global entries with
  the same provider/model id. DS4 also supports CLI `--base-url` plus `DS4_BASE_URL` /
  `DS4_URL` for the conventional `ds4:deepseek-v4-flash` descriptor, while
  `DS4_API_KEY` remains only the credential.
- **#43** Slash-command completion in the interactive prompt. Typing `/` and pressing Tab
  now lists commands and aliases from the same registry used for dispatch; prefixes such
  as `/thi` complete to `/thinking`, while normal prompts and command arguments are left
  untouched.
- **#47** CLI help now advertises accepted values for finite-set options. `--thinking`
  uses clap possible values (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`), so
  `pie --help` and invalid-value errors both show the supported set.
- **#52** `/login` now prompts for API keys with terminal echo disabled instead of
  accepting `/login <provider> <api-key>` inline. Inline keys are rejected with a usage
  message that does not repeat the secret, preventing interactive terminal scrollback from
  retaining raw credential material.
- **#66** `/triggers` slash command for the RFC 1 trigger surface. It now shows runtime
  counters, hook health, running trigger actions, recent trigger audit rows, and supports
  aborting one or all in-flight trigger actions from the terminal while rendering only
  preview-safe fields.
- **#4** Dangerous-bash detector wired through `before_tool_call`. 11-pattern corpus
  (`rm -rf /`, `sudo`, `curl|sh`, etc.) returns deny reason as the synthesized tool result.
- **#5** `@file` mention injection. Files are read, capped at 64 KiB, prepended to the
  prompt as `<file path="...">…</file>` blocks.

### Added — Tier 2 (session/state)

- **#6** `pie --continue` / `-c`, `pie --list-all-sessions`, `/save` (Markdown transcript
  export), `/name <slug>`, `/sessions`, `/share` (Gist upload via `gh`), `/find <query>`
  (cross-session text search).
- **#7** `CostTracker` on `AgentHarness`, `/cost` + `/cost reset` slash commands,
  `budget_cap_usd` pre-turn gate, `fallback_model` after retry-exhaustion.

### Added — Tier 4 (framework depth)

- **#9** `pie-mcp` crate: stdio transport, JSON-RPC 2.0 framing, initialize handshake,
  `tools/list` + `tools/call`. `McpAgentTool` adapter wraps server tools as `AgentTool`s.
  `~/.pie/mcp.toml` loader spawns each server lazily.
- **#10 Part A** Dual-root skills loader (`<cwd>/.pie/skills/` overrides `~/.pie/skills/`),
  wired into `AgentHarnessOptions::skills`.
- **#11** `task` subagent tool: spawns a fresh `AgentHarness` (in-memory session, read-only
  tool subset), parent abort cascades to subagent within 2s.
- **#12** Built-in tools: `web_fetch` (HTML→text), `web_search` (Brave Search), structured
  `git` (status/diff/log), LSP supervisor + `after_tool_call` hook that attaches diagnostics
  to write/edit tool results.

### Added — Tier 5 (auth/cloud)

- **#13** `auth.json` credential store with atomic write + mode 0600. `/login` and
  `/logout` slash commands. Model resolver consults the store as env-var fallback. OAuth
  2.0 PKCE primitives (`Flow::authorize_url`, `await_callback`, `exchange_code`,
  `refresh_token`).
- **#14** Hand-rolled AWS SigV4 signer (no aws-sdk dep). Bedrock `invoke()` for the
  non-streaming `/model/{id}/invoke` path. Vertex AI `invoke()` with bearer or API-key
  auth.

### Added — Tier 6 (observability)

- **#15** Tracing subscriber writing per-session logs to `~/.pie/logs/<session>.log` via
  non-blocking `tracing-appender`. `/diag` snapshot command. `/bug-report` with secret
  redaction (OpenAI/Anthropic keys, AWS access keys, GitHub PATs, Slack tokens, Google API
  keys, Bearer tokens). OTLP HTTP/JSON span exporter activated by
  `OTEL_EXPORTER_OTLP_ENDPOINT`.

### Added — Tier 7 (multimodal)

- **#16** `--image <path>` CLI flag (repeatable, PNG/JPEG/WebP/GIF, 10 MiB per image, 10
  per message). Magic-byte mime detection.

### Added — Framework

- **`InstallSkill` builtin tool (issue #87 sub-PR B)** New harness tool that installs a skill
  into the user-global skills directory (`~/.pie/skills/<name>/SKILL.md`) from one of three
  sources: an `https://` URL, an absolute local path, or inline content. After the atomic
  write, the tool calls `AgentHarness::reload_skills_from_disk` (sub-PR A) so the catalog
  refreshes without a `pie` restart. Two-phase safety model: the first tool call returns a
  preview JSON (`name`/`description`/`target_path`/`content_hash`/`size`/`existing`/
  `overwrite_required`) with no filesystem side effects; the agent must explicitly call again
  with `confirm: true` (and `overwrite: true` if a same-name skill with different on-disk
  hash exists) for the install to actually run. The skill body is never echoed verbatim into
  the tool result. Hard caps: 64 KiB body size, `https://`-only URLs (loopback / RFC1918 /
  `.localhost` hostnames are pre-flight rejected as an SSRF guard), name must validate as
  lowercase-kebab. Sequential execution mode so concurrent installs in the same turn don't
  race. Persistent audit: every successful install appends a
  `SessionTreeEntry::Custom { custom_type: "skill_install" }` record (status / name /
  target_path / source_kind / source / before_hash / after_hash / size / overwrote /
  idempotent / installed_visible_in_catalog / diagnostics_count / warnings — body is never
  included; inline-content source records `null` to avoid echoing the body); `--resume`,
  bug-report, and post-hoc forensics now see every model-driven install. The audit write is
  best-effort — if `append_custom` fails after the file is on disk and the catalog has been
  reloaded, the install is still considered successful (the alternative of rolling back a
  successful install on audit-write failure is worse UX). The tool result reports the missed
  audit via `details.audit_entry_id = null` and a `tracing::warn`. The
  `PermissionCategory::ControlPlaneWrite` Prompt path is a separate cross-cutting follow-up;
  for now the two-phase schema is the primary defense, and PR-C (`/skills install <url>`)
  adds the CLI-side user confirmation. 10 unit tests cover preview-no-side-effects, name
  traversal rejection, `http://` rejection, SSRF guard, oversized content cap, malformed
  frontmatter, overwrite required / idempotent re-install, full atomic-install-and-reload
  path, `skill_install` Custom audit shape + body-no-leak, and tempfile cleanup.
- **Skill catalog hot-reload (issue #87 sub-PR A)** New `AgentHarnessOptions::reload_skills_fn:
  Option<ReloadSkillsFn>` closure slot + `AgentHarness::reload_skills_from_disk() ->
  Result<LoadSkillsOutput, ReloadSkillsError>` async API. Lets the install path (forthcoming
  `InstallSkillTool` / `/skills reload`) refresh the skill catalog without restarting `pie`,
  while keeping source directories + dedup policy in a single embedder-owned closure (so
  startup load + runtime reload never disagree on which dirs got scanned). Runtime stays
  IO-free — the closure owns all filesystem access. In-flight turns aren't interrupted; only
  the catalog and rebuilt `<skills>` system-prompt block change, surfaced on the next prompt.
  Pinned by 4 regression tests covering single-call invocation, diagnostic propagation,
  `NotConfigured` error path, and `state.messages`/`is_streaming` non-perturbation.
- **Trigger promotion — structured authorization (RFC 1 / commit `5397199` fix-forward
  infra)** Runtime gains a structured promotion-gate path that closes the
  free-form-summary authorization channel introduced by the dynamic-trigger workflow:
  `PromoteAction::PromoteSummaryWhenResultDetailsMatch { template_body, condition }` and
  `PromotionCondition::AnyOf { json_pointer, any_of }` evaluate against the sub-agent's
  structured `trigger_result.details` (RFC 6901 pointer + array intersection). All four
  skip paths (`PointerMissing`, `ValueNotArray`, `EmptyIntersection`, plus the matched
  `Ok(_)` branch) are pinned with unit tests; the audit `state: "skipped"` records a
  stable machine-readable `reason` (`result_details_missing` / `result_details_not_array`
  / `no_matching_rule_id`) so downstream tools can compare against an enum rather than a
  sentence. `HarnessEvent::TriggerCompleted` and the `trigger_result` audit blob gain a
  `details: serde_json::Value` field — defaults to `Null`; populated through marker tools
  Tools-MCP wires in a follow-up PR. The previous
  `PromoteAction::PromoteSummaryWhenSummaryContains` variant is `#[deprecated]` and
  retained for transition only — coding-agent's `dynamic.rs` keeps using it locally with
  `#[allow(deprecated)]` until Tools-MCP migrates the path.
- **Trigger promotion — race fix.** `apply_promotion`'s final step that mirrors the
  promoted user message into the parent `Agent`'s in-memory state used to push directly
  into `state.messages`, racing the parent agent loop if the parent was mid-stream.
  Switched to a `is_streaming()` branch: streaming parent → `enqueue_follow_up(...)` so
  the loop drains the message at a turn boundary; idle parent → direct push so the next
  `prompt()` sees it immediately without an explicit rehydrate. Either path the message
  has already been durably persisted via `Session::append_message`; this just avoids the
  mid-turn ordering hazard where `[…, user_promoted, assistant_response]` would otherwise
  look like the assistant answered a question that hadn't been asked.
- **`PermissionCategory::ControlPlaneWrite`.** New permission category for persistent
  agent self-modification (trigger rule create/remove/enable/disable, skill / hook
  install). `PermissionPolicy::evaluate_with_category(category, tool, args)` is the new
  entry point; the existing `evaluate(tool, args)` delegates to `Tool` for backward
  compatibility. Runtime defaults `ControlPlaneWrite` to `Allow` so adding the category is
  non-breaking; Tools-MCP and CLI-TUI follow-up PRs wire the danger classifier + Prompt
  path when they opt their writers in.
- **#17** `HarnessEvent` typed bus on `AgentHarness` (SessionStart / Compaction /
  Branch). Prompt-template file loader (`<cwd>/.pie/templates/` overrides
  `~/.pie/templates/`) + `/template <name> [k=v ...]` slash command.
  `AgentHarness::after_tool_call` hook slot, paired with the existing `before_tool_call`.
- **#20 (skeleton)** Public types for RFC 1 trigger runtime: `Trigger` envelope,
  `TriggerSource` enum, `SourceKind`, `PayloadVisibility`, `TriggerAuthority`,
  `CredentialScope`, `TriggerState` (`received` → `accepted | deduped | cycle_suppressed
  | permission_denied | needs_approval | running | failed | completed`), and
  `TriggerRecord` (v=1 schema, additive-only, persisted as
  `SessionTreeEntry::Custom { custom_type: "trigger" }`). Plus the `NotificationHook`
  trait, `NotificationHookStatus`, `HookState` (`Connected / Reconnecting /
  Disconnected { reason } / Disabled / AuthFailed { reason }`, with `AuthFailed`
  reserved for credential failures and protocol mismatches mapping to `Disconnected`),
  `HookError`, and the `TriggerSink = mpsc::UnboundedSender<Trigger>` alias. **Types
  only — no agent loop entrypoint yet**; the supervisor + state machine wiring + the
  `AgentHarness::handle_trigger` API land in a follow-up PR. Adapter authors (MCP read
  pump, Cloudflare hub WebSocket hook) can build against the trait in parallel.
- **#20 (dedup + cycle engine)** Pure-logic `TriggerRuntime` evaluator that decides
  whether an incoming `Trigger` should be admitted, deduplicated against a prior trigger
  within a configurable window, or suppressed because its trace chain has exceeded the
  follow-up hop limit. `TriggerRuntimeConfig { dedup_window, cycle_hop_limit }` defaults
  to a 5-minute dedup window (clamped to 24h) and 5 hops. `evaluate(&Trigger)` returns
  `EvaluationOutcome::Accept`, `Deduped { replacement_policy, previous_trace_id }`
  (carrying the first arrival's `ReplacementPolicy` so callers can implement
  `LatestReplaces` / `Coalesce` / `Drop` uniformly), or `CycleSuppressed { hop_count }`.
  Harness-spawned follow-up triggers bump the same trace chain via
  `record_follow_up_hop(trace_id, now)` so the cycle counter is monotonic across the
  whole reaction graph. Dedup keys are the `Trigger.idempotency_key` field set by the
  source adapter (per RFC 1 §3 the source is responsible for synthesizing a stable
  key); the evaluator treats the field as opaque and does not synthesize one itself.
  **Pure logic — no I/O, no session writes, no harness wiring yet**; the
  `AgentHarness::handle_trigger` entrypoint that consumes this evaluator and persists
  the audit `Custom` entry lands in sub-PR 2.
- **#20 (handle_trigger + audit + status snapshot)** `AgentHarness::handle_trigger(trigger)`
  is now the runtime entrypoint for accepted notifications: it emits
  `HarnessEvent::TriggerHandlingStart`, runs the dedup + cycle evaluator, persists a
  `SessionTreeEntry::Custom { custom_type: "trigger", data: TriggerRecord }` audit entry
  capturing the evaluator decision, and emits `HarnessEvent::TriggerHandled` with the
  resulting `TriggerState` (`Accepted` / `Deduped` / `CycleSuppressed`). Audit
  persistence is best-effort: when the session storage write fails, the trigger
  evaluation outcome is still returned and `HarnessEvent::PersistenceError { context:
  "trigger_audit", message }` is emitted alongside `TriggerHandled { audit_entry_id:
  None }` so observability surfaces (TUI banner, `/triggers`, JSONL logs) can mark the
  audit as best-effort lost rather than dropping it silently. New
  `AgentHarness::notification_status_snapshot()` returns a copy-friendly
  `NotificationStatusSnapshot { hooks, runtime }` for status banners — `runtime` is the
  fresh `TriggerRuntimeSnapshot { dedup_entries, active_traces, accepted_total,
  deduped_total, cycle_suppressed_total }`; `hooks` is an empty `Vec` in this PR (hook
  registration + supervisor land in a follow-up so the shape is here today). New
  `AgentHarnessOptions::trigger_runtime` lets callers override
  `dedup_window` / `cycle_hop_limit`. The `Accepted → Running → Completed/Failed`
  transition (real action execution) lands with the permission evaluator extension in
  sub-PR 3 — `Accepted` is terminal for this slice. 5 new integration tests pin the
  audit shape, dedup state propagation, cycle suppression, snapshot counter
  monotonicity, and the persistence-failure reflux contract.
- **#20 (hook supervisor)** `AgentHarness::register_notification_hook(self: &Arc<Self>,
  hook: DynNotificationHook)` is the convenience wiring on top of `handle_trigger`. It
  spawns two detached tokio tasks: a **driver** that runs `hook.run(sink)`, and a **pump**
  that drains the sink's receiver and calls `handle_trigger` for each trigger in send
  order. Tasks tear down naturally when the hook's `run` future ends and the sink drops.
  Registered hooks are tracked in the harness so `notification_status_snapshot().hooks`
  now reflects each hook's live `NotificationHook::status()`. There is no unregister API
  in this slice — hooks live until the harness drops or the driver returns; YAGNI shape
  for v1. 2 new integration tests exercise (a) a pump-and-dedup happy path with three
  triggers (Accepted/Accepted/Deduped) ending in the expected audit + snapshot state,
  and (b) a degraded hook reporting `HookState::Disconnected` to verify
  `notification_status_snapshot` surfaces the hook's reported state and
  `requires_attention` message without producing any triggers.
- **#20 (trigger permission hook)** New `AgentHarnessOptions::before_trigger:
  Option<BeforeTriggerHook>` plugs a permission decision between dedup/cycle evaluator
  Accept and audit persistence. The hook returns `BeforeTriggerDecision::Allow` (keeps
  state `Accepted`, default if no hook configured), `Deny { reason }` (transitions to
  terminal `PermissionDenied`, reason captured in `evaluator_decision`), or `Prompt
  { reason }` (transitions to soft-terminal `NeedsApproval` for future UI replay).
  Hook only runs on the Accept path — `Deduped` / `CycleSuppressed` outcomes skip it
  entirely, since dedup/cycle decisions are pure-runtime concerns with no policy
  involvement. The hook receives `BeforeTriggerContext { trigger, runtime }` so policy
  can reason over the full trigger envelope (authority / source / payload summary)
  plus a live `TriggerRuntimeSnapshot` (e.g. for burst-rate rules). `reason` strings
  appear in audit + observability surfaces and must not carry secrets — Provider/Auth
  reviewed the boundary. The deny/prompt reason is also surfaced on
  `HarnessEvent::TriggerHandled.evaluator_decision` (new field; mirrors the audit
  record's `evaluator_decision`) so live subscribers (TUI banner, JSONL logs) can
  render why a trigger was denied/needs approval without a secondary session lookup.
  4 new integration tests pin default-Allow, Deny→PermissionDenied (asserting both the
  audit record AND the event carry the reason), Prompt→NeedsApproval (same dual
  assertion), and that the hook is bypassed on the Deduped path.
- **#20 (sub-agent execution, no promotion yet)** Accepted triggers now spawn a detached
  sub-agent that runs the trigger's action prompt without blocking the `handle_trigger`
  caller or the `register_notification_hook` pump (RFC 1 §5.A). New
  `AgentHarnessOptions::before_trigger_action: Option<BeforeTriggerActionHook>` resolves
  the prompt; absence falls back to the stable
  `format!("{source_label} fired: {event_label}")` mapping. New `TriggerAction { prompt,
  promote, promote_requires_approval }` and `PromoteAction { None | PromoteSummaryNow {
  template } }` are accepted for forward compatibility but only `PromoteAction::None` has
  any effect — the promotion pipeline lands in sub-PR 5b. On sub-agent completion the
  parent session gets a `SessionTreeEntry::Custom { custom_type: "trigger_result", data:
  { trace_id, branch_id, success, summary, message_count, cost_usd } }` audit entry; the
  sub-agent's full transcript lives in an in-memory session that is discarded when the
  task ends (jsonl-backed retained branches per the issue #20 amendment are tracked as a
  sub-PR 5c follow-up — `branch_id` is `null` in 5a). Three new `HarnessEvent` variants
  (`TriggerExecutionStarted` / `TriggerCompleted` / `TriggerFailed`) carry preview-safe
  fields for live banner / JSONL rendering; causality `TriggerHandled(Accepted)` →
  `TriggerExecutionStarted` → `TriggerCompleted | TriggerFailed` is pinned by rustdoc
  and a test. `NotificationStatusSnapshot.running: Vec<RunningTriggerState>` exposes
  in-flight triggers with bounded preview fields (no raw payload, no template vars).
  `AgentHarness::abort_trigger(trace_id)` and `AgentHarness::abort_all_triggers()`
  cancel running sub-agents; cancelled sub-agents emit `TriggerFailed { reason:
  "aborted" }` and write `trigger_result { success: false }`. Sub-agents inherit the
  parent's `model` / `system_prompt` / `tools` / `before_tool_call` /
  `after_tool_call` so permission policies continue to apply to trigger-driven tool
  calls. 6 new integration tests cover: accepted trigger writes audit (summary +
  trace_id link), event ordering, pump non-blocking (two triggers; first slow, second
  reaches audit promptly), `running` snapshot bounded preview + leaves after completion,
  abort path (cancellation within 3s + failure audit), and non-Accepted states never
  spawn sub-agents.
- **#20 (promotion: PromoteSummaryNow + template engine + trigger_promotion audit)**
  `PromoteAction::PromoteSummaryNow { template: Option<String> }` now actually does
  something. After a sub-agent completes the spawn task runs the per-trigger
  `PromoteAction` (returned by `BeforeTriggerActionHook`); when set to `PromoteSummaryNow`,
  the runtime renders a template over an **allowlisted** context (`trace_id`,
  `trigger.source.kind/server_name/method/topic/subkind`, `trigger.source_label`,
  `trigger.event_label`, `trigger.payload_summary`, `trigger.received_at`,
  `trigger.idempotency_key`, `trigger.authority.principal_id/principal_label/credential_scope`,
  `result.summary/status/message_count/cost_usd/branch_id`) and appends a
  `Message::User` into the parent session jsonl carrying the rendered body.
  Default template (when `template: None`) is the built-in
  `"[Trigger {{trace_id}}] {{trigger.source_label}} fired {{trigger.event_label}}.\nResult: {{result.summary}}"`.
  Every promotion attempt writes a `SessionTreeEntry::Custom { custom_type:
  "trigger_promotion", data: { state, trace_id, promote_kind, template_name, template_hash,
  inserted_entry_id, rule_id, redaction_status, dedup_collapsed } }` audit so JSONL
  readers can attribute every parent transcript mutation to a specific trigger.
  Three states are persisted: `success` (rendered + inserted), `pending` (when
  `promote_requires_approval = true` and no `/triggers approve` command is shipped yet),
  and `failed` (render error — unknown field or forbidden field). Forbidden field
  references (`{{trigger.payload}}`, `{{trigger.authority.allowed_source_actions}}`,
  any `_meta.*`) fail render with `redaction_status: "forbidden_field"`; unknown fields
  fail with `redaction_status: "render_error"`; rendered bodies exceeding 4 KiB are
  truncated on a UTF-8 char boundary with `redaction_status: "truncated"`. All failure
  paths leave the parent transcript unchanged. Two new `HarnessEvent` variants
  (`TriggerPromoted` / `PromotionPending`) carry preview-safe fields for live banner
  rendering; causality `TriggerCompleted | TriggerFailed` → `TriggerPromoted |
  PromotionPending` is preserved. `promote_requires_approval = true` v1 fail-closed
  behavior is documented as a deliberate security choice: changing
  "previously auto-merged, now requires approval" later is much worse than starting from
  pending-only and adding the approval CLI surface in sub-PR 6. 5 new integration tests
  cover all 5 promotion-related acceptance criteria from the issue #20 amendment (#8
  no-promote stability, #9 success path inserts audited entry, #10 unknown var
  fail-closed, #11 forbidden field fail-closed, #12 summary truncation marker, #13
  approval required fail-closed-to-pending).

### Fixed

- TUI tool output is now display-capped independently from the model-facing tool result.
  Long tool output previews keep the first 20 lines and last 4 lines (errors get a larger
  40/8-line budget), truncate overlong lines on UTF-8 character boundaries, and show a
  marker explaining that the full output remains available to the agent. Repeated
  `ToolExecutionUpdate` progress for the same tool call replaces the existing feed block
  instead of appending unbounded intermediate output. Resume replay uses the same display
  cap, so reopened sessions do not print historical tool output in full by default.
- `McpClient::tools_call` now races the caller's `CancellationToken` against the response
  and request_timeout. When cancel fires before the server replies, the inflight slot is
  released, a best-effort `notifications/cancelled` frame (MCP spec 2025-03-26) is sent to
  the server with the original JSON-RPC `requestId` (bounded by a 200ms send budget so a
  stuck transport can't keep the cancel path open), and the call returns
  `McpError::Cancelled`. The `McpAgentTool` adapter plumbs the harness cancel token through
  so a user Ctrl-C now (a) returns immediately at the adapter, (b) does not leak inflight
  HashMap entries on cancel/dropped futures (an `InflightGuard` RAII covers every exit
  path), and (c) gives the MCP server a chance to stop work instead of running to
  completion. Late responses to an already-cancelled id are silently dropped by the read
  pump. Tests in `crates/mcp/tests/client_fixture.rs` and `mcp_adapter::tests` pin the wire
  shape, bounded return time, no-spurious-cancel-on-success, and adapter-level plumbing.
- Slash commands that start an agent turn (`/new-trigger` and `/template <name>`) now route
  through the same REPL-owned Ctrl-C abort path as normal prompts, so thinking/streaming/tool
  execution can be interrupted consistently instead of being awaited inside command dispatch.
- Anthropic `input_json_delta` fragments are now assembled into the final
  `ToolCall.arguments` object before `ToolCallEnd` / `Done`. Previously tool calls streamed
  their name and id correctly but ended with empty `{}` arguments, so downstream tools could
  be invoked without the model-provided parameters.
- `get_api_provider()` handles now capture the provider `Arc` at lookup time, so
  `unregister_api_providers()` / `clear_api_providers()` cannot make a previously returned
  `RegisteredHandle` panic with `provider removed while handle was held`. In-flight handles
  keep the TypeScript registry semantics: unregistering prevents future lookup while already
  captured handles continue to stream or return the normal mismatch error stream.
- Provider HTTP paths now honor `StreamOptions::abort` while sending requests, sleeping between
  retries, draining retryable response bodies, reading error response text, and consuming SSE /
  AWS event-stream bodies. Aborted streams emit an `ErrorReason::Aborted` terminal event instead
  of waiting for provider I/O to finish.
- **#48** `/share` no longer passes the removed `gh gist create --secret` flag. Secret
  gists are the GitHub CLI default; `/share --public` still passes `--public`, and errors
  continue to preserve the underlying `gh` stderr.
- **#39 (PR-A)** `NativeEnv::exec` now honors `ExecOptions::{timeout_secs, abort,
  on_stdout, on_stderr}`. The previous implementation called `Command::output()` with a
  `// TODO: timeout, onStdout/onStderr streaming, abort honoring.` comment, so callers
  believing they had cancel/timeout semantics could leak runaway processes. The new
  implementation spawns the child in its own Unix session/process group via `setsid()`
  (so background descendants are reachable through `killpg`), drains stdout and stderr
  concurrently on dedicated tasks (a serial drain would deadlock on a child that fills
  stderr before closing stdout), and races `child.wait()` via `tokio::time::timeout`
  against the abort token; on timeout / abort it `killpg(SIGKILL)`s the whole tree,
  waits the reaper, and returns `ExecutionError { code: Timeout | Aborted, ... }`. New
  tests cover normal completion, streaming callbacks, 1-second timeout on `sleep 10`
  (returning well before 10s), abort-token cancellation, 4000-line stderr volume that
  would have deadlocked the old serial drain, backgrounded descendant `(sleep N) & wait`
  teardown, and exact stdout/stderr preservation without invented trailing newlines.
- **#39 (PR-A)** `PermissionPolicy` no longer misses `rm` recursive+force flag
  permutations or shell-quoted operands. The old regex caught `-rf`, `-fr`, and
  `--recursive --force` (one ordering only); it missed `rm -r -f /`, `rm -f -r /`,
  `rm --force --recursive /`, `rm -r --force /`, `rm --force -r /`, and any operand
  carrying quotes like `rm -rf "/etc"`. The corpus now routes `rm` detection through a
  token-aware classifier that splits on `;` / `&&` / `||` / `|`, walks each clause's
  argv, sets `has_recursive` / `has_force` from any combination of short, long, and
  separated flags, then runs each operand through a normalizer that strips one balanced
  layer of `'`/`"` and rewrites `${HOME}` to `$HOME` before checking absolute path or
  `~` / `$HOME` targets. Non-`rm` rules (sudo, curl|sh, dd, mkfs, chmod 777, shutdown,
  git push --force, eval pipe, forkbomb) remain in the regex set since they don't have
  the same permutation problem. Tests now include 25 dangerous variants (every short /
  long / separated / mixed flag combination, leading-path `/bin/rm`, pipelined `rm`,
  and 8 quoted/`${HOME}` operand shapes) plus 4 near-miss cases verifying the classifier
  does not over-block (e.g. `rm -r` alone, `rm -f` alone, relative-path `rm -rf ./build`).
- **#39 (PR-B)** `AgentTool::prepare_arguments` is now actually invoked. The agent loop
  used to dispatch `t.execute(raw_args, ...)` straight from the assistant tool call,
  so tools relying on the documented "raw tool-call arguments compatibility shim"
  silently got the unnormalized payload. `execute_tools` now resolves the matched tool
  before constructing the call site, runs `prepare_arguments` once, and threads the
  result through both the `before_tool_call` hook (on `ctx.args` and
  `ctx.tool_call.arguments`) and `execute()`. When `prepare_arguments` returns a
  non-Object shape we clear `ctx.tool_call.arguments` to an empty map so hook authors
  have a single truthy source. Unknown tools keep raw args so the dispatcher's
  "no such tool" error message still references what the model sent. New
  `prepare_arguments_normalizes_args_for_hook_and_execute` integration test pins
  the contract with a tool whose `prepare_arguments` upper-cases a field.
- **#39 (PR-B)** `AgentToolUpdate` callbacks now flow through to subscribers as
  `AgentEvent::ToolExecutionUpdate`. Previously `run_one()` always passed `None` for
  the update callback, so the event variant was unreachable even though the type
  was public. The new wiring builds a per-tool-call `mpsc::unbounded_channel` and
  spawns a pump task that emits each `partial_result` as a `ToolExecutionUpdate` in
  send order; the sync callback never blocks (just enqueues). The channel and pump
  are torn down when `execute()` returns; if the tool misbehaves and retains the
  `Arc<on_update>` past return, a 2-second pump-join timeout + `abort()` caps the
  shutdown so the agent loop cannot hang. `AgentTool::execute` rustdoc spells out
  the contract: do not retain `on_update` past return. Two new integration tests
  (`tool_execution_update_callback_emits_listener_events_in_order` and
  `run_one_does_not_hang_when_tool_retains_on_update_past_return`) pin delivery order,
  `tool_call_id` correlation, and the hang-bound respectively.
- **#18** Biased select against stream stalls so Ctrl-C unblocks the in-flight prompt
  within 500ms regardless of LLM stream state.
- **#19** `AgentHarness` compaction now sources entries from the real session jsonl
  (`session.branch(None)`) instead of synthesizing fresh `SessionTreeEntry::Message`
  records with throwaway uuidv7 ids. The previous implementation wrote a
  `first_kept_entry_id` to the `Compaction` record that was never reachable in the session,
  so `--resume` silently dropped all pre-compaction tail messages. The in-memory tail
  retained after compaction now maps back to the corresponding `state.messages` index by
  counting `Message` entries strictly before `first_kept_entry_id`, replacing the previous
  token-only heuristic. Sessions still containing legacy bad `firstKeptEntryId` values
  recover best-effort: replay keeps only the compaction summary plus post-compaction
  entries (no panic, no crash). Same PR also asserts `build_session_context` skips
  `SessionTreeEntry::Custom { custom_type: "trigger" }` entries from the LLM message stream
  while keeping them enumerable via `session.branch(None)` — a prerequisite invariant for
  RFC 1 (issue #20) trigger audit work. Session-side read failures during compaction now
  emit a `HarnessEvent::Compaction` with a `compaction skipped: ...` summary and leave both
  the session jsonl and agent state untouched rather than crashing.
- **#25 (PR C)** Regression test (`resume_rebuilds_skill_block_byte_identical_from_same_directory`)
  asserting that the `<skills>` block in the system prompt is byte-identical across two
  independent `load_skills` runs against the same skills directory. Resume / daemon restart
  scenarios must reconstruct the catalog deterministically; the test pins this so future
  refactors of `load_skills` ordering or `format_skills_for_system_prompt` rendering cannot
  silently break the resumed system prompt. Test-only PR — no production code change.
- **#25 (PR A)** Register the `Skill` builtin tool the system prompt already advertises.
  Before this fix the model would call `Skill { name: "..." }` and receive
  `no tool named 'Skill'` because the tool was never wired into `default_tools`. On hit
  the tool returns the skill body wrapped in a `<skill name="...">` block; on miss it
  surfaces a typed error pointing the model at `/skills`. `disable_model_invocation=true`
  is now enforced uniformly across all call paths (was previously a no-op flag).

### Explicitly de-scoped

- Windows support (Linux + macOS only).
- Filesystem / network sandboxing (was #8). The permission system (#4) is the safety
  layer; per-session always-allow + interactive prompt mode remain follow-up work.

### Pending follow-ups (documented; not in this release)

- #2 ratatui-style sticky-input TUI with streaming markdown render + history + Ctrl-R.
- #10 Part B WASM extension host (foundation: `skills` loader + slash-command registry are
  the public extension surface in v1).
- #13 Provider-specific OAuth endpoint URLs for Anthropic Pro/Max, Codex, Copilot, Google
  (the generic `Flow` plumbing is in; consumers supply their own URLs).
- #14 Bedrock streaming (`/invoke-with-response-stream` event-stream binary framing) +
  full ADC chain (service-account JSON → JWT → token exchange) for Vertex.
- #12 Per-tool LSP language config richer than per-extension; multi-server collaboration.

## Workspace test coverage

27 test binaries, ~225 tests, `clippy -D warnings` clean across `pie-ai`,
`pie-agent-core`, `pie-coding-agent`, `pie-mcp`.
