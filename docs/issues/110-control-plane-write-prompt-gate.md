# Issue #110 — `ControlPlaneWrite` user-Prompt gate

Status: **design draft v0.2 — @Runtime-dev-lead.**

## v0.2 changes (post-review)

- **Removed generic `SessionScope` / `PersistedScope` from the runtime API.** Multiple reviewers flagged that caching on `{tool_name, args_hash}` would turn one-time approval into a broad reusable trust record, and `args_hash` is per-call unique so it can't even serve fefe `Always`. Runtime API now handles **exact prompt approval only**. Any "Always" / "Remember" cache is fully embedder-owned, per-tool, with tool-defined cache-key shape. Mirrors the `~/.pie/hub-trust.json` ownership pattern in RFC #18 §5.7.
- **Added trigger-shaped resolution channel** alongside tool-call resolution channel. Trigger binding uses `{trace_id, source_label, idempotency_key, receiver_agent_id, sender_agent_id, action_class}` synthesized into `trigger_prompt_id`; tool-call binding stays `{tool_call_id, tool_name, args_hash}`. Two parallel API surfaces, one shared prompt-card render model on the embedder side.
- **`AgentTool::permission_classification(&args)` trait accessor** replaces label-string classifier matching (per Tools-MCP preference). Each tool returns its own `Allow | Prompt { reason } | Block { reason }` based on the prepared args.
- **Sub-PR merge-safety explicit.** Sub-PR 1 ships runtime API + `permission_classification` default = `Allow`; control-plane tools opt into `Prompt` lockstep with sub-PR 2 landing CLI/TUI embedder handler. Either merge order safe; no intermediate-main regression.
- **`NewTrigger` / `RemoveTrigger` / `SetTriggerState` classifier coverage** added to sub-PR 3 scope per Tools-MCP review.
- Folded `control_plane_prompt` audit shape: no `remember_for` field; audit records the single-call decision only. Embedder writes its own audit entry for any cache change (`fefe_trust_decision` for hub, similar shape for tool-level Always).

v0.1 history at git `00f5b73`.

## Why this matters now

`PermissionCategory::ControlPlaneWrite` was introduced in PR #67 to classify persistent agent self-modification (trigger rules, skill install/remove/enable, etc.). The runtime declared `PermissionDecision::Prompt` as a possible outcome but never wired the embedder-side prompt channel. Every control-plane write tool today relies on an **in-tool two-phase `preview → confirm` guard** which is **model-self-confirmed**, not user-mediated.

This was acceptable as a stopgap during issue #20 (RFC 1 trigger runtime) and issue #23 (skill management). Two recent events make it the next P0 runtime gap:

1. **`SetSkillState(enabled=true)` is hard-blocked at the tool surface** (PR #108). Re-enabling a skill the author shipped with `disable_model_invocation=true` is a privilege escalation the model must not self-authorize. The block is a stopgap; the real fix is this gate.
2. **RFC #18 §5.6 (fefe public MCP hub) first-contact prompt depends on this gate.** The cross-namespace first-contact UI surfaces through the trigger-shaped half of this channel. Without #110 landing, hub cross-namespace senders fail-closed deny indefinitely — and the fefe RFC's release-complete gate (§8.4 gate 6) cannot be reached for scenarios 4 / 5.

The Definition of Done in RFC #18 (`docs/issues/18-rfc-fefe-mcp-hub.md`) explicitly cites this issue as the critical path for the first-contact gate. This design note proposes the implementation split so #110 can land alongside §5 / §2 / §3 implementation, not after.

## Design constraints (non-negotiable)

These come from issue #110 comments (Provider-Auth + QA acceptance), v0.1 review feedback, and how the existing runtime hooks already work:

1. **Decision binding (per-call, anti-replay)** — every prompt approval is bound to a single concrete invocation. For tool calls: `{tool_call_id, tool_name, args_hash}` where `args_hash = sha256(canonical_json(prepare_arguments(args)))`. For triggers: `{trace_id, source_label, idempotency_key, receiver_agent_id, sender_agent_id, action_class}` → `trigger_prompt_id = sha256(canonical_json(tuple))`. Both bind to the exact mutation; approval can't be replayed onto a different invocation.
2. **Fail-closed defaults** — denied / timed-out / disconnected paths leave no filesystem mutation, no registry reload, no committed-success audit. A rejected-attempt audit entry is OK if marked `denied` / `timed_out`, bounded.
3. **Bounded prompt payload** — preview-safe fields only: op, source, target path preview, redacted condition/action, hash/diff summary, before/after booleans. No SKILL.md body, no raw rule text, no install source URL tokens, no provider/base_url credentials, no auth-store values, no raw payload bytes.
4. **Headless / non-interactive policy is explicit** — default = fail-closed deny for prompt-required writes. Any `--yes` mode must be opt-in, visible in audit, and tested.
5. **Embedder parity** — CLI/TUI must implement before the runtime gate ships. Web UI must implement or fail-closed deny for prompt-required writes. Confirmation UI must be interrupt-safe.
6. **Classifier separation** — escalating / destructive writes prompt; narrowing / low-risk writes (disable, dedup) do not. The escalating set is pinned in tests so future loosen/tighten changes are explicit.
7. **Audit shape stable** — every prompt outcome (allowed / denied / timed_out) writes a `Custom { custom_type: "control_plane_prompt" }` (tool calls) or `Custom { custom_type: "trigger_prompt" }` (triggers) session entry. QA owns the redaction test.
8. **Runtime is remember-agnostic** — runtime API only handles one-time approval. Embedders own any "Always" / "Remember" cache (file shape, key derivation, TTL, write/read). The runtime never reads or writes a remember store.

## Runtime API surface (proposed v0.2)

### Artifact A — `AgentTool::permission_classification` accessor

```rust
pub trait AgentTool {
    // existing methods …

    /// Per-tool classification override evaluated before `before_tool_call`
    /// hooks. Default `Allow` preserves current behavior for any tool that
    /// hasn't opted in. Tools that mutate persistent state (skills, triggers,
    /// hub-trust) return `Prompt` with a bounded human-readable reason.
    /// `Block` is for hard categorical refusals that no Prompt should be
    /// allowed to bypass (currently unused; replaces the `SetSkillState
    /// (enabled=true)` stopgap once sub-PRs 1 + 2 land).
    fn permission_classification(
        &self,
        prepared_args: &serde_json::Value,
    ) -> PermissionClassification {
        PermissionClassification::Allow
    }
}

pub enum PermissionClassification {
    Allow,
    Prompt { reason: String },
    Block { reason: String },
}
```

Tool authors decide the classification based on prepared args, e.g.:

- `SetSkillState::permission_classification({enabled: true, ..})` → `Prompt { "Re-enable skill (overrides author's disable_model_invocation hint)" }`.
- `SetSkillState::permission_classification({enabled: false, ..})` → `Allow` (narrowing).
- `InstallSkill::permission_classification(_)` → `Prompt { "Install third-party skill into the global catalog" }`.
- `NewTrigger::permission_classification(_)` → `Prompt { "Register persistent trigger rule" }`.

Why a trait accessor over label-string matching in `PermissionPolicy`: per-tool semantics live with the tool; new tools are forced to think about classification at definition site, not in a central match arm someone forgets to update. Tools-MCP confirmed preference on PR #130 v0.1 review.

### Artifact B — `BeforeToolCallResult::Prompt` variant

Today `BeforeToolCallResult` is `{ block: bool, reason: Option<String> }`. Extend to:

```rust
pub enum BeforeToolCallResult {
    Allow,
    Block { reason: String },
    Prompt {
        /// Bounded preview-safe payload rendered by the embedder. Runtime
        /// never inspects fields; embedder owns the shape.
        prompt_payload: serde_json::Value,
        /// Embedder-supplied label for the prompt UI (e.g. "Install skill
        /// from db9.ai/skill.md"). Runtime caps at 200 chars before
        /// persistence.
        label: String,
    },
}
```

Backward compatibility: existing `{ block: false }` → `Allow`; `{ block: true, reason }` → `Block { reason }`. All current `before_tool_call` hook impls compile with no change.

The agent loop calls `tool.permission_classification(prepared_args)` before `before_tool_call`. If the classification is `Prompt`, the loop synthesizes a `BeforeToolCallResult::Prompt` with a default bounded payload (`{tool_name, args_preview, reason}`); custom `before_tool_call` hooks can override with richer payload.

### Artifact C — Tool-call resolution channel

```rust
// On AgentHarness:
pub async fn resolve_control_plane_prompt(
    &self,
    tool_call_id: &str,
    args_hash: &str,
    decision: ControlPlanePromptDecision,
) -> Result<(), ControlPlanePromptError>;

pub enum ControlPlanePromptDecision {
    Allow,
    Deny { reason: Option<String> },
    Timeout,
}

pub enum ControlPlanePromptError {
    UnknownPrompt,        // tool_call_id not in flight
    ArgsHashMismatch,     // approval was for a different args_hash
    AlreadyResolved,      // double-resolution attempt
}
```

`HarnessEvent::ControlPlanePromptRequest { tool_call_id, tool_name, args_hash, label, prompt_payload, reason }` fires when a tool-call prompt is awaiting resolution. The embedder must call `resolve_control_plane_prompt(...)` within the embedder-configured timeout (default 5 min) or the runtime times out fail-closed.

The runtime does NOT carry a `remember_for` field. Embedder cache (if any) is the embedder's responsibility — see [Embedder cache pattern](#embedder-cache-pattern-non-normative-illustration) below.

### Artifact D — Trigger resolution channel (parallel surface)

For fefe sub-PR 4 and any future trigger-shaped prompts. Distinct from Artifact C because triggers have a different binding shape and a different lifecycle (RFC 1's `BeforeTriggerHook::Prompt` outcome already returns `TriggerState::NeedsApproval` — what's been missing is the resolution channel).

```rust
// On AgentHarness:
pub async fn resolve_trigger_prompt(
    &self,
    trigger_prompt_id: &str,
    decision: TriggerPromptDecision,
) -> Result<(), TriggerPromptError>;

pub enum TriggerPromptDecision {
    Allow,
    Deny { reason: Option<String> },
    Timeout,
}

// where trigger_prompt_id = sha256(canonical_json({
//   trace_id,
//   source_label,
//   idempotency_key,
//   receiver_agent_id,
//   sender_agent_id,
//   action_class,
// }))
```

`HarnessEvent::TriggerPromptRequest { trigger_prompt_id, trigger_summary, prompt_payload, reason }` fires when `BeforeTriggerHook::Prompt` returns. The runtime synthesizes `trigger_prompt_id` from the trigger envelope before emitting; the embedder must echo the same id on resolution.

Same fail-closed semantics: timeout / unknown id / id-mismatch → Deny + audit.

### Artifact E — Audit shapes

**`control_plane_prompt`** (one entry per tool-call resolution):

```json
{
  "schema_version": 1,
  "tool_call_id":   "<id>",
  "tool_name":      "<name>",
  "args_hash":      "<64 hex>",
  "label":          "<bounded ≤ 200 chars>",
  "decision":       "allow" | "deny" | "timeout",
  "reason":         "<bounded, embedder-supplied, may be null>",
  "at":             "<RFC3339>"
}
```

**`trigger_prompt`** (one entry per trigger resolution):

```json
{
  "schema_version":       1,
  "trigger_prompt_id":    "<64 hex>",
  "trace_id":             "<UUID>",
  "source_label":         "mcp:pie-hub:custom:agent_message:<id>",
  "receiver_agent_id":    "<UUID>",
  "sender_agent_id":      "<UUID>",
  "action_class":         "notification",
  "decision":             "allow" | "deny" | "timeout",
  "reason":               "<bounded, embedder-supplied, may be null>",
  "at":                   "<RFC3339>"
}
```

Both audits omit `remember_for` deliberately — the runtime does not know whether the embedder also wrote a separate trust entry. If the embedder cached an "Always" decision, it writes its own audit (`fefe_trust_decision` per RFC #18 §5.7, or a tool-specific equivalent like `skill_trust_decision`). The two audit types correlate by timestamp + identifiers within the same session.

QA's redaction acceptance test (from PR #110 comments) covers both shapes equivalently: same forbidden-fields list as `fefe_trust_decision` (RFC #18 §5.7) — no raw payloads, no tokens, no provider credentials, no `CF_API_KEY`, no password hashes.

## Embedder cache pattern (non-normative illustration)

The runtime API has no `remember_for` field, but tools that meaningfully support "Always" need an embedder-side cache. Recommended pattern (followed by RFC #18 §5.7 `~/.pie/hub-trust.json`):

```rust
// in embedder before_tool_call hook:
async fn before_tool_call(ctx: BeforeToolCallContext) -> BeforeToolCallResult {
    // 1. Compute tool-specific cache key.
    let cache_key = match ctx.tool_name {
        "InstallSkill" => None,                       // never offer Always
        "SetSkillState" => Some(json!({
            "skill_source": args["skill_source"],
            "skill_handle": args["skill_handle"],
            "target_enabled": args["enabled"],
        })),
        // …
        _ => None,
    };

    // 2. Cache hit short-circuits the prompt.
    if let Some(key) = &cache_key {
        if let Some(cached) = trust_store.lookup(ctx.tool_name, key).await {
            if !cached.expired() { return BeforeToolCallResult::Allow; }
        }
    }

    // 3. No cache → defer to runtime's classifier-driven Prompt path.
    BeforeToolCallResult::default_for_classifier(&ctx)
}

// in embedder prompt UI:
fn render_prompt(req: ControlPlanePromptRequest) -> ControlPlanePromptDecision {
    let user_picked = show_card(req.label, req.prompt_payload, allow_always: cache_key.is_some());
    match user_picked {
        UserChoice::Once          => ControlPlanePromptDecision::Allow,
        UserChoice::Always(scope) => {
            trust_store.persist(req.tool_name, cache_key.unwrap(), scope.ttl).await;
            // Write embedder-owned audit entry recording the cache change.
            session.append_custom("skill_trust_decision", json!({...})).await;
            ControlPlanePromptDecision::Allow
        }
        UserChoice::Deny          => ControlPlanePromptDecision::Deny { reason: None },
    }
}
```

Key properties of this pattern:

- Cache key is **tool-defined**, not generic `{tool_name, args_hash}`. fefe trust uses `{receiver_agent_id, sender_agent_id, action_class}` per RFC #18 §5.7 — the receiver id is mandatory so a shared `~/.pie/hub-trust.json` (e.g. dotfile-synced across machines) cannot authorize the same sender for a different local receiver. RFC #18 §5.OQ-3 tracks the pending decision on whether to also bind to a per-machine receiver identity for extra safety against dotfile-sync replay. `SetSkillState(enable)` uses `{skill_source, skill_handle, enable_state}`. No tool can accidentally use too broad a scope because each tool's key shape is explicit in the embedder hook.
- Runtime is unchanged across all of this. The `control_plane_prompt` audit records the single approval; the embedder's cache and its own audit entry record the policy decision separately.
- "Always" semantics are entirely up to the embedder. CLI/TUI may render the option; Web UI may choose not to. Headless mode can skip the option entirely (the cache key being `Some` is necessary but not sufficient).

For fefe, the same pattern lives in `BeforeTriggerHook` (it already exists from RFC 1) — embedder reads `~/.pie/hub-trust.json`, returns `Allow` on hit, returns `Prompt` on miss, writes the trust entry on `Always` user choice.

## Sub-PR plan

Four sub-PRs across owners. Sub-PR 1 gates 2/3/4 but is shaped so it can land without breaking existing tools (default classification = `Allow`).

### Sub-PR 1 — Runtime: API surface + audit + classifier default (Runtime)

Files:
- `crates/agent/src/types.rs` — `BeforeToolCallResult` enum extension, `PermissionClassification` enum, `AgentTool::permission_classification` default impl.
- `crates/agent/src/agent_loop.rs` — call `tool.permission_classification` before `before_tool_call`; on `Prompt`, emit `HarnessEvent::ControlPlanePromptRequest`, suspend the tool-call slot, await `resolve_control_plane_prompt(...)`, validate args_hash, dispatch or block.
- `crates/agent/src/harness/agent_harness.rs` — `resolve_control_plane_prompt(...)` API, `resolve_trigger_prompt(...)` API, `HarnessEvent::ControlPlanePromptRequest` + `HarnessEvent::TriggerPromptRequest`, `control_plane_prompt` + `trigger_prompt` audit emission.
- `crates/agent/src/harness/trigger_runtime.rs` (or wherever `handle_trigger` lives) — surface `TriggerPromptRequest` when `BeforeTriggerHook::Prompt` returns; bridge `resolve_trigger_prompt` back to `handle_trigger`'s admission state.
- `crates/agent/tests/harness_e2e.rs` — tests for: tool classification default = Allow (legacy unchanged), Prompt fires when classifier says so, args_hash mismatch rejects, timeout fails-closed, audit shape both types, embedder-disconnect fails-closed, trigger prompt id binding.

Estimated diff: ~700 LOC including tests. **Merge-safety: no existing tool's `permission_classification` is changed.** All control-plane writes stay on the `Allow` path until sub-PR 2 (CLI/TUI) lands and sub-PR 3 (Tools-MCP) flips per-tool classification. No intermediate-main regression.

### Sub-PR 2 — CLI/TUI: prompt card + decision channel + headless policy (CLI-TUI)

Files:
- `crates/coding-agent/src/ui/control_plane_prompt.rs` (new) — render `ControlPlanePromptRequest` as a card, capture Y/N/Always, return `ControlPlanePromptDecision`. Bounded preview rendering.
- `crates/coding-agent/src/ui/trigger_prompt.rs` (new) — render `TriggerPromptRequest` (different shape, similar card style).
- `crates/coding-agent/src/main.rs` — register on both `HarnessEvent::ControlPlanePromptRequest` and `HarnessEvent::TriggerPromptRequest`, route through prompt UI, call appropriate `resolve_*` API.
- `crates/coding-agent/src/web/...` — Web UI analogous decision channel; fail-closed deny in v0 if Web UI doesn't ship the prompt card.
- Headless / `pie --no-interactive`: default = fail-closed deny for both types. `--yes` flag = auto-allow with explicit audit marker.
- (Optional, separate file) `crates/coding-agent/src/control_plane_trust.rs` — embedder-side trust store (`~/.pie/control-plane-trust.json`) per the [Embedder cache pattern](#embedder-cache-pattern-non-normative-illustration). Lookups happen in `before_tool_call` before the runtime sees a Prompt; writes happen on user Always choice.

Tests pin: accept path, deny path, Ctrl-C during prompt = deny, headless default = deny, headless `--yes` = allow with audit marker, cache hit short-circuits prompt entirely, cache write happens before resolution returns Allow (so post-resolution tool execution sees the cache state).

### Sub-PR 3 — Tools-MCP: classifier coverage + lift `SetSkillState` stopgap (Tools-MCP)

Files:
- `crates/coding-agent/src/tools/set_skill_state.rs` — implement `permission_classification`; remove the hard-block on `enabled=true`; the `Prompt` path now handles enable.
- `crates/coding-agent/src/tools/install_skill.rs` — implement `permission_classification`.
- `crates/coding-agent/src/tools/remove_skill.rs` — implement `permission_classification`.
- `crates/coding-agent/src/triggers/dynamic.rs` — implement `permission_classification` on `NewTriggerTool`, `RemoveTriggerTool`, `SetTriggerStateTool` (the three trigger control-plane tools Tools-MCP flagged).
- Lockstep tests: `SetSkillState(enabled=true)` succeeds only after user Prompt approval (regression: model-only path still cannot enable); `SetSkillState(enabled=false)` skips Prompt (narrowing); `NewTrigger(action: <shell-metachar pattern>)` prompts.

Tools-MCP offered a parallel mini-PR for the trigger-tool classifiers if Sub-PR 3 wants to stay scoped to skills — Runtime-side preference is one combined Tools-MCP PR for fewer merge points, but either works.

### Sub-PR 4 — Runtime: fefe first-contact gate wires `BeforeTriggerHook::Prompt` (Runtime, joint with RFC #18 §5 implementation PR)

Files:
- `crates/agent/src/harness/notification_hook.rs` — `make_pie_hub_notification_hook(source_kind_prefix: "pie-hub")` factory (already in RFC #18 §5.1 plan).
- New `HubTrustGate` impl of `BeforeTriggerHook` — reads `~/.pie/hub-trust.json` (per RFC #18 §5.7); returns `Allow` on trust hit, `Deny` on block, `Prompt` on cross-namespace miss.
- Embedder reads `HarnessEvent::TriggerPromptRequest` → routes through the same prompt UI as Sub-PR 2 (different card content, same decision channel surface).
- Embedder cache write on `Always` → `~/.pie/hub-trust.json` + `fefe_trust_decision` audit entry per RFC #18 §5.7.

Depends on Sub-PRs 1 + 2 merged. Lands alongside the RFC #18 §5 implementation PR.

## Sub-PR merge-safety summary

The four sub-PRs may land in any chronological order between Runtime/CLI-TUI/Tools-MCP without main going through a broken state:

| Land sub-PR in this order | What main looks like |
| ------------------------- | -------------------- |
| 1                         | Runtime API exists, no tool returns `Prompt`. All control-plane tools work as today (i.e. `Allow` default). |
| 1 → 3                     | Tools return `Prompt` from their classifier, but no embedder handler → runtime times out fail-closed. **`InstallSkill` etc. would start denying.** ⚠️ Avoid this ordering. |
| 1 → 2                     | Runtime API + embedder handler both exist; no tools opt in yet. Still all `Allow`. |
| 1 → 2 → 3                 | Tools opt in via `permission_classification`; embedder Prompt path live; user sees confirmation cards. Lift `SetSkillState(enabled=true)` stopgap. |
| 1 → 2 → 3 → 4             | fefe first-contact also gated. fefe gate-6 scenarios 4/5 testable. |

**Sequencing rule**: 3 MUST land after 2 (or atomically with 2). Anything else is safe. 4 MUST land after 1 + 2 (needs both API and embedder handler).

## Acceptance (from issue #110 + QA additions + v0.2 changes)

| ID | Acceptance | Owner of test |
| -- | ---------- | ------------- |
| A1 | Runtime Prompt outcome is first-class; default (no handler / timeout / disconnect) = fail-closed deny, no mutation, no committed-success audit | Runtime |
| A2 | Tool-call confirmation bound to `{tool_call_id, tool_name, args_hash}`; replay across calls rejected with `ArgsHashMismatch` | Runtime |
| A3 | **Trigger** confirmation bound to `trigger_prompt_id = sha256({trace_id, source_label, idempotency_key, receiver_agent_id, sender_agent_id, action_class})`; replay across triggers rejected | Runtime |
| A4 | Prompt payload bounded and redacted (no SKILL.md body, no raw trigger body, no tokens, no `CF_API_KEY`); same forbidden-fields list as `fefe_trust_decision` | Snapshot tests in Runtime + Tools-MCP |
| A5 | Classification pinned: `SetSkillState(enable=true)`, `InstallSkill`, `RemoveSkill`, `NewTrigger`, `RemoveTrigger`, `SetTriggerState(enable=true)` all Prompt; their narrowing variants Allow | Runtime + Tools-MCP |
| A6 | Headless = fail-closed deny by default; `--yes` opt-in audited per-prompt | CLI-TUI |
| A7 | `SetSkillState(enabled=true)` unblocked only behind real user Prompt (regression: model alone cannot enable) | Tools-MCP |
| A8 | Embedder parity: CLI/TUI ships before Sub-PR 3 flips classifications; Web UI ships or fail-closed deny | CLI-TUI |
| A9 | Sub-PR merge-safety: sub-PR 1 alone on main does not break any existing tool path (default = Allow) | QA / Runtime |
| A10 | Embedder cache is opt-in per tool with tool-defined key shape; runtime never reads/writes a remember store; cache-hit short-circuits prompt entirely | CLI-TUI |
| A11 | Allow / deny / timeout / args-hash mismatch / unknown-prompt / double-resolve all covered by tests; `cargo fmt` / `clippy --all-targets -- -D warnings` / `cargo test --workspace` baseline | Runtime + CLI-TUI |

## Out of scope (for this issue)

- "Always trust" managed by the runtime — embedder-only.
- LLM-judged auto-approval — even with `--yes`, the LLM never approves its own escalating write.
- Provider credential prompts (different channel; `~/.pie/auth.json` and OAuth flows unchanged).
- Approving past mutations retroactively (approval is for the upcoming call only).
- Trust UI design for the embedder cache file (`~/.pie/control-plane-trust.json` shape, format, migration) — CLI/TUI's domain in sub-PR 2.

## Dependencies and order

- Sub-PR 1 (Runtime) blocks 2/3/4 in capability terms.
- Sub-PR 2 (CLI-TUI) blocks shipping anything user-visible (A7, A8).
- Sub-PR 3 (Tools-MCP) blocks lifting `SetSkillState(enabled=true)` stopgap (A6) and gating trigger tools (A5).
- Sub-PR 4 (Runtime joint with §5) blocks fefe Definition of Done gate 6 scenarios 4 / 5.

## Open questions

| ID | Question | Take |
| -- | -------- | ---- |
| §110.OQ-1 | Should the runtime expose any cache-correlation hint in the audit (e.g. a hash of the embedder's cache key), or stay totally remember-agnostic? | Stay remember-agnostic. Embedder writes its own audit entry for cache changes (`fefe_trust_decision`, etc.). One source of truth per concern. |
| §110.OQ-2 | Does `args_hash` include `prepare_arguments(...)` output or raw args? | Prepared — that's what the tool actually executes against. |
| §110.OQ-3 | Should the prompt channel timeout be embedder-configurable, or runtime-fixed default? | Embedder-configurable, default 5 minutes. CLI/TUI may make it interactive (no timeout while user at the terminal). |
| §110.OQ-4 | Web UI fail-closed-deny vs not shipping the affected tools until Web implements the channel? | v0 = fail-closed deny; tools still exist but always deny on Web; CHANGELOG flags the gap. |
| §110.OQ-5 | Should `AgentTool::permission_classification` accept `&BeforeToolCallContext` instead of `&serde_json::Value`, to give classifier access to the full agent state (model, session id, etc.)? | Lean YES — gives tools richer info without breaking change later. Default impl still ignores context. |

## References

- Issue #110 (this issue's GitHub thread, including comments from @Provider-Auth-Lead and @QA-Release-Lead — those acceptance lists are folded into A1–A11 above).
- PR #67 — introduced `PermissionCategory::ControlPlaneWrite` (default Allow, Prompt deferred).
- PR #108 — `SetSkillState(enabled=true)` hard-block stopgap that this issue lifts.
- PR #56 — `McpNotificationHook` (used by sub-PR 4).
- PR #130 v0.1 (commit `00f5b73`) — original design draft superseded by this v0.2; PR comments from @Provider-Auth-Lead, @QA-Release-Lead, @Tools-MCP-Lead, @alice converged on the v0.2 changes above.
- RFC #18 (`docs/issues/18-rfc-fefe-mcp-hub.md`) §5.6 / §5.7 — first-contact gate that hard-depends on this issue.
- RFC 1 (issue #20) — trigger pipeline `BeforeTriggerHook`, which sub-PR 4 bridges to the resolution channel.

Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>
