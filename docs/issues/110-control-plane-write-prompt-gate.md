# Issue #110 — `ControlPlaneWrite` user-Prompt gate

Status: **design draft v0.1 — @Runtime-dev-lead.**

## Why this matters now

`PermissionCategory::ControlPlaneWrite` was introduced in PR #67 to classify persistent agent self-modification (trigger rules, skill install/remove/enable, etc.). The runtime declared `PermissionDecision::Prompt` as a possible outcome but never wired the embedder-side prompt channel. Every control-plane write tool today relies on an **in-tool two-phase `preview → confirm` guard** which is **model-self-confirmed**, not user-mediated.

This was acceptable as a stopgap during issue #20 (RFC 1 trigger runtime) and issue #23 (skill management). Two recent events make it the next P0 runtime gap:

1. **`SetSkillState(enabled=true)` is hard-blocked at the tool surface** (PR #108). Re-enabling a skill the author shipped with `disable_model_invocation=true` is a privilege escalation the model must not self-authorize. The block is a stopgap; the real fix is this gate.
2. **RFC #18 §5.6 (fefe public MCP hub) first-contact prompt depends on this gate.** The cross-namespace first-contact UI surfaces through the same `ControlPlaneWrite` Prompt channel that `NewTrigger` / `InstallSkill` / `RemoveSkill` will use. Without #110 landing, hub cross-namespace senders fail-closed deny indefinitely — and the fefe RFC's release-complete gate (§8.4 gate 6) cannot be reached for scenarios 4 / 5.

The Definition of Done in RFC #18 (`docs/issues/18-rfc-fefe-mcp-hub.md`) explicitly cites this issue as the critical path for the first-contact gate. This design note proposes the implementation split so #110 can land alongside §5 / §2 / §3 implementation, not after.

## Design constraints (non-negotiable)

These come from the existing comments on issue #110 (Provider-Auth + QA acceptance) and from how the existing runtime hooks already work:

1. **Decision binding** — the user's Y/N is bound to `{tool_call_id, tool_name, args_hash}`. An approval granted for one call MUST NOT be replayable onto a different mutation, even with the same tool name. The hash includes the normalized argument bytes.
2. **Fail-closed defaults** — denied / timed-out / disconnected paths leave no filesystem mutation, no registry reload, and no committed-success audit. A rejected-attempt audit entry is OK if marked `denied` / `timed_out`, bounded.
3. **Bounded prompt payload** — the prompt card may contain only preview-safe fields: op, source, target path preview, redacted condition/action, hash/diff summary, before/after booleans. No SKILL.md body, no raw rule text, no install source URL tokens, no provider/base_url credentials, no auth-store values, no raw payload bytes.
4. **Headless / non-interactive policy is explicit** — default is fail-closed deny for prompt-required writes. Any `--yes` mode must be opt-in, visible in audit, and tested.
5. **Embedder parity** — CLI/TUI must implement before the runtime gate ships; Web UI must implement or fail-closed deny for prompt-required writes. Confirmation UI must be interrupt-safe.
6. **Classifier separation** — escalating / destructive writes prompt; narrowing / low-risk writes (disable, dedup) do not. The dangerous-pattern set is pinned in tests so future loosen/tighten changes are explicit.
7. **Audit shape stable** — every prompt outcome (accepted / denied / timed_out) writes a `Custom { custom_type: "control_plane_prompt" }` session entry. QA owns the redaction test.

## Runtime API surface (proposed)

Two distinct artifacts:

### Artifact A — `BeforeToolCallResult::Prompt` variant

Today `BeforeToolCallResult` is `{ block: bool, reason: Option<String> }`. Extend to:

```rust
pub enum BeforeToolCallResult {
    Allow,
    Block { reason: String },
    Prompt {
        /// Bounded preview-safe payload rendered by the embedder. Runtime never
        /// inspects fields; embedder owns the shape.
        prompt_payload: serde_json::Value,
        /// Embedder-supplied label for the prompt UI (e.g. "Install skill from
        /// db9.ai/skill.md"). Bounded by embedder; runtime caps at 200 chars
        /// before persistence.
        label: String,
    },
}
```

Backward compatibility: the current `{ block: false, reason: None }` → `Allow`, `{ block: true, reason }` → `Block`. Existing tool-hook implementations compile unchanged by mapping to the legacy variants.

### Artifact B — `PermissionPolicy` → `Prompt` outcome for `ControlPlaneWrite`

`PermissionPolicy::evaluate_with_category(category, tool_name, args)` currently returns `Allow | Deny` for `ControlPlaneWrite`. Extend to allow `Prompt { reason }`. The classifier (Provider-Auth-owned in `crates/agent/src/harness/permission.rs`) decides which control-plane writes prompt:

- **Prompt-required** (escalating / destructive): `InstallSkill`, `RemoveSkill`, `SetSkillState(enabled=true)`, `NewTrigger`, `RemoveTrigger`, `SetTriggerState(enabled=true)`, dangerous-pattern `NewTrigger.action` (e.g. shell metachars), and fefe `before_trigger` hub cross-namespace first-contact.
- **Allow-without-prompt** (narrowing / low-risk): `SetSkillState(enabled=false)`, `SetTriggerState(enabled=false)`, dedup operations, list/show reads.

The Prompt-required set lands lockstep with tests so changes are explicit.

### Artifact C — Embedder confirmation channel

The runtime emits a new `HarnessEvent::ControlPlanePromptRequest { tool_call_id, tool_name, args_hash, label, prompt_payload }` and waits on an embedder-owned async confirmation channel. Embedder calls `harness.resolve_control_plane_prompt(tool_call_id, decision)` where `decision: ControlPlanePromptDecision::{Allow, Deny, Timeout}`.

```rust
pub enum ControlPlanePromptDecision {
    Allow { remember_for: Option<RememberScope> },
    Deny { reason: Option<String> },
    Timeout,
}

pub enum RememberScope {
    /// Cache the decision for this tool_call_id only. Default if the embedder
    /// doesn't offer "always" UI. (Always safe — replay-binding still applies.)
    JustThisCall,
    /// Cache the decision for {tool_name, args_hash} for the rest of this
    /// session. Survives `--resume` only if the embedder persists the cache.
    SessionScope,
    /// Cache the decision for {tool_name, args_hash} on disk
    /// (`~/.pie/control-plane-trust.json`). Survives session restart.
    /// fefe first-contact `Always` uses this (with action_class scoping).
    PersistedScope,
}
```

The embedder writes the persisted cache; runtime never touches disk. (Mirrors the `~/.pie/hub-trust.json` ownership in RFC #18 §5.7.)

### Artifact D — `args_hash` discipline

Runtime computes `args_hash = sha256(canonical_json(prepared_args))` where `prepared_args` is the result of `AgentTool::prepare_arguments(args)`. The hash is bound into both the prompt request and the resolution; the resolution is rejected if `args_hash` doesn't match. Rejection drops the tool call with a synthesized `BeforeToolCallResult::Block { reason: "approval did not match" }` and emits an audit entry.

### Artifact E — `control_plane_prompt` audit entry

```json
{
  "schema_version": 1,
  "tool_call_id": "...",
  "tool_name": "...",
  "args_hash": "<64 hex>",
  "label": "<bounded>",
  "decision": "allow" | "deny" | "timeout",
  "remember_for": "just_this_call" | "session" | "persisted" | null,
  "reason": "<bounded, embedder-supplied>",
  "at": "<RFC3339>"
}
```

QA's redaction acceptance test (per the existing PR #110 comment) covers what MUST NOT appear in `prompt_payload` or the audit `label` / `reason`. Same redaction rules as `fefe_trust_decision` (RFC #18 §5.7).

## Sub-PR plan

This is large enough to split into four sub-PRs, sequencable across owners. Order matters but the Runtime piece (sub-PR 1) is the gating one.

### Sub-PR 1 — Runtime: `BeforeToolCallResult::Prompt` + classifier + audit (Runtime)

Files:
- `crates/agent/src/types.rs` — `BeforeToolCallResult` enum extension, `ControlPlanePromptDecision`, `RememberScope`.
- `crates/agent/src/agent_loop.rs` — handle `Prompt` outcome: emit `HarnessEvent::ControlPlanePromptRequest`, suspend the tool-call slot, await `resolve_control_plane_prompt(...)`, validate args_hash, dispatch or block.
- `crates/agent/src/harness/agent_harness.rs` — `resolve_control_plane_prompt(...)` API + audit emission.
- `crates/agent/src/harness/permission.rs` — extend `evaluate_with_category` for `ControlPlaneWrite` with the classifier.
- `crates/agent/tests/harness_e2e.rs` — tests for: prompt fires for escalating, doesn't fire for narrowing, args_hash mismatch rejects, timeout fails-closed, audit shape, embedder-disconnect fails-closed.

Estimated diff: ~600 LOC including tests. Single PR, no breaking change to existing tools (they hit the `Allow` path by default unless classifier rules them as Prompt-required).

### Sub-PR 2 — CLI/TUI: prompt card + decision channel (CLI-TUI)

Files:
- `crates/coding-agent/src/ui/control_plane_prompt.rs` (new) — render prompt card, capture Y/N, optional `Always` checkbox, return `ControlPlanePromptDecision`.
- `crates/coding-agent/src/main.rs` — register on `HarnessEvent::ControlPlanePromptRequest`, route through the prompt UI, call `harness.resolve_control_plane_prompt(...)`.
- `crates/coding-agent/src/web/...` — analogous decision channel for Web UI (or fail-closed deny in v0 if Web doesn't implement).
- Headless: `--yes` flag opts into auto-allow; default headless = fail-closed deny.

CLI-TUI tests pin: accept path, deny path, Ctrl-C during prompt = deny, headless default = deny, headless `--yes` = allow with audit marker.

### Sub-PR 3 — Tools-MCP: lift `SetSkillState(enabled=true)` stopgap (Tools-MCP)

Files:
- `crates/coding-agent/src/tools/set_skill_state.rs` — remove the hard-block on `enabled=true`. The tool's `before_tool_call` classifier now returns `Prompt` for `enabled=true`, which goes through the new channel.
- Lockstep test: `SetSkillState(enabled=true)` succeeds only after user approval (regression: model-only path still cannot enable).

### Sub-PR 4 — Embedder integration for fefe first-contact (Runtime, joint with §5 implementation PR)

Files:
- `crates/agent/src/harness/notification_hook.rs` — hub trust gate `BeforeTriggerHook` calls the same prompt channel (via a Runtime-internal helper that maps trigger Prompt to control-plane Prompt with action_class=notification).
- Lands together with the RFC #18 §5 implementation PR. Depends on sub-PRs 1 + 2 merged.

## Acceptance (from issue #110 + QA additions)

| ID | Acceptance | Owner of test |
| -- | ---------- | ------------- |
| A1 | Runtime Prompt outcome is first-class, fail-closed (no embedder = deny, timeout = deny) | Runtime |
| A2 | Confirmation bound to `{tool_call_id, tool_name, args_hash}` — replay across tool calls rejected | Runtime |
| A3 | Prompt payload is bounded and redacted (no SKILL.md body, no raw trigger body, no tokens, etc.) | Snapshot tests in Runtime + Tools-MCP |
| A4 | Classification pinned in tests; escalating prompts; narrowing does not | Runtime + Tools-MCP |
| A5 | Headless = fail-closed deny by default; `--yes` is explicit and audited | CLI-TUI |
| A6 | `SetSkillState(enabled=true)` unblocked only behind real user-Prompt (regression: model alone cannot enable) | Tools-MCP |
| A7 | Embedder parity: CLI/TUI implements before runtime ships; Web UI implements or fail-closed | CLI-TUI |
| A8 | Allow / deny / timeout / replay / args-hash mismatch all covered by tests; `cargo fmt` / `clippy --all-targets -- -D warnings` / `cargo test --workspace` baseline | Runtime + CLI-TUI |

## Out of scope (for this issue)

- "Always trust" with broad scope (cross-tool, cross-session-without-binding) — explicitly NOT designed here. Trust scope tuples extend only within the per-tool-call binding pattern.
- LLM-judged auto-approval — even with optional `--yes` mode, the LLM never approves its own escalating write.
- Provider credential prompts (those use a different channel today; this issue does not touch `~/.pie/auth.json` or OAuth flows).
- Approving past mutations retroactively (no time-shift; approval is for the upcoming call only).

## Dependencies and order

- Sub-PR 1 (Runtime) blocks 2/3/4.
- Sub-PR 2 (CLI-TUI) blocks shipping #110 (acceptance A7).
- Sub-PR 3 (Tools-MCP) blocks lifting the `SetSkillState(enabled=true)` stopgap (acceptance A6).
- Sub-PR 4 ships alongside RFC #18 §5 implementation PR — needed for fefe Definition of Done gate 6 scenarios 4 / 5.

## Open questions

| ID | Question | Take |
| -- | -------- | ---- |
| §110.OQ-1 | Should `RememberScope::PersistedScope` allow per-tool-name TTL (e.g. 90 days for fefe first-contact `Always`, indefinite for skill enable)? | Yes — embedder writes the TTL into the cache entry; runtime never reads it. Mirrors RFC #18 §4.OQ-3. |
| §110.OQ-2 | Does `args_hash` include `prepare_arguments(...)` output or raw args? | Prepared — that's what the tool actually executes against. Matches the in-tool two-phase guard's hash today. |
| §110.OQ-3 | Should the prompt channel timeout be embedder-configurable, or runtime-fixed default? | Embedder-configurable, default 5 minutes. CLI/TUI may make it interactive (no timeout if user is at the terminal). |
| §110.OQ-4 | Web UI fail-closed-deny vs not shipping the affected tools until Web implements the channel? | v0 = fail-closed deny; tools still exist but always deny on Web; CHANGELOG flags the gap. |

## References

- Issue #110 (this issue's GitHub thread, including comments from @Provider-Auth-Lead and @QA-Release-Lead — those acceptance lists are folded above).
- PR #67 — introduced `PermissionCategory::ControlPlaneWrite` (default Allow, Prompt deferred).
- PR #108 — `SetSkillState(enabled=true)` hard-block stopgap that this issue lifts.
- PR #56 — `McpNotificationHook` (used by sub-PR 4).
- RFC #18 (`docs/issues/18-rfc-fefe-mcp-hub.md`) §5.6 — first-contact gate that hard-depends on #110.
- RFC 1 (issue #20) — trigger pipeline `BeforeTriggerHook`, which sub-PR 4 bridges to this channel.

Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>
