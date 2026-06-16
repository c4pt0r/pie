# Interactive Model Picker (TUI + WebUI) — Design

Date: 2026-06-11
Status: approved (design discussion in session; owner confirmed scope)

## Problem

`/model` is a text-only command: switching models requires knowing the exact
`provider:model-id` string, and `/model list` prints a 200+ line catalog. The
TUI already has interactive surfaces (resume picker at startup, control-plane
approval overlay), so model selection should be interactive too — in both the
TUI and the WebUI.

## Scope and filtering rules

- The picker is a **curated** entry point, not a replacement. The
  `/model <provider:id>` text path is unchanged and can still switch to any
  registered model.
- The picker only surfaces models whose `Model.api` is in the two supported
  API families:
  - OpenAI-compatible: `openai-completions`, `openai-responses`,
    `openai-codex-responses`
  - Claude-compatible: `anthropic-messages`
- The provider list (first menu level) is derived from the filtered model set,
  not hardcoded. `codex`, `anthropic`, and `ds4` (plus any custom
  OpenAI/Anthropic-compatible entries from `~/.pie/models.json` or
  `<cwd>/.pie/models.json`) appear automatically; `bedrock`, `google-vertex`,
  etc. are excluded.
- Credentials / login: **out of scope this iteration** except for a passive
  status indicator. Each provider row shows whether a credential was found
  (`AuthStore` lookup + provider env vars). No-credential providers display a
  `no key — /login <provider>` hint. Selecting a model without a credential is
  NOT blocked — identical semantics to today's `/model <spec>` text command.

## TUI design

Pattern: follow the existing `control_plane_prompt` overlay (ratatui state on
`App`), NOT the blocking crossterm `resume_picker` — the main UI is already a
ratatui event loop and a blocking picker would fight it.

- Bare `/model` (no args) returns a new `CommandOutcome::OpenModelPicker`
  carrying the filtered catalog. `ModelCommand` keeps all current behavior for
  the `list` and `<spec>` argument forms.
- `App` holds `Option<ModelPickerState>`; `handle_key` intercepts keys when the
  picker is open (same precedence pattern as
  `handle_control_plane_prompt_key`).
- Two levels:
  1. Provider list — name + credential status marker.
  2. Model list for the chosen provider — current active model marked `●`.
- Keys: Up/Down (and j/k) navigate, Enter descends/selects, Esc goes back one
  level or closes from the top level.
- Selection executes the exact same path as the text command:
  `parse → pie_ai::get_model → harness.set_model`, then a confirmation line in
  the feed.
- Headless / plain-REPL path (no ratatui): bare `/model` falls back to current
  behavior (print active model + usage hint). `OpenModelPicker` in that
  dispatch arm degrades to the same printout.

## WebUI design

- `WebSnapshot` gains a `model_catalog` field:
  `Vec<{ provider, models: [{ id, name }], has_credential }>` — built by the
  same shared filter/group function the TUI uses, so both surfaces always show
  identical data. Catalog is a few KB after filtering; snapshots already carry
  the whole feed, so no separate fetch protocol is added.
- The header model label becomes a clickable dropdown, grouped by provider,
  with the active model highlighted.
- Selection paths (both converge on one set-model handler on the app side):
  - Local web (axum, `web.rs`): new `POST /model` endpoint, mirroring how
    prompts are posted.
  - Relay (cloud viewer, `relay.rs`): new `WorkerFrame::SetModel { model }`
    (string is `provider:id`), handled like `ControlPlaneResolve`.
- The post-switch snapshot push refreshes the label — that is the success
  feedback. Errors (unknown model) surface as a system line in the feed and an
  inline error in the dropdown where the surface supports it.

## Components

| Unit | Responsibility |
|---|---|
| `model_picker::catalog()` (new, coding-agent) | Filter + group models by API family; annotate credential status. Single source for TUI and Web. |
| `ModelPickerState` (new, `ui/`) | Two-level navigation state machine; pure, unit-testable. |
| `CommandOutcome::OpenModelPicker` | Bridges bare `/model` to the UI overlay. |
| `WorkerFrame::SetModel` / `POST /model` | Remote switch transport. |
| Shared set-model handler on `App` | Validate spec, call `harness.set_model`, emit feed line. |

## Error handling

- Unknown/unregistered model spec at selection time: error line in feed (TUI)
  or system line + snapshot (web); picker/dropdown stays usable.
- Empty catalog after filtering (no compatible models registered): picker
  opens with a one-line notice and a hint to use `/model <spec>`.
- Mid-turn switch: same semantics as the existing text command (no new
  guard added).

## Testing

- `ModelPickerState` unit tests: navigation, descend/back, select, boundary
  (empty provider, single model).
- `catalog()` filter tests: ds4/custom OpenAI-compatible model included,
  `bedrock-converse-stream` model excluded, credential flag set from a temp
  `AuthStore`.
- `WorkerFrame::SetModel` serde round-trip (alongside existing relay tests).
- Local web `POST /model` handler test if the axum router has test precedent.
- No test may hit a real provider API (CI clears all provider keys).

## Out of scope

- Login / API-key entry flows from inside the picker.
- Surfacing providers outside the two API families in the menu.
- Fuzzy filtering inside the picker (two-level menu keeps lists short; can be
  added later if lists grow).
