# Interactive Model Picker Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Interactive model selection in both the TUI (two-level ratatui overlay: provider → model) and the WebUI (clickable header dropdown), curated to OpenAI-compatible and Claude-compatible API families.

**Architecture:** A new pure module `model_picker` owns the filtered catalog and the picker state machine. The TUI gets a modal overlay following the existing `control_plane_prompt` pattern. The WebUI ships the catalog inside `WebSnapshot`; selection arrives via `POST /model` (local axum) or a new `WorkerFrame::SetModel` (relay), both converging on one `App::set_model_from_spec` handler — the same parse→get_model→set_model path as the `/model <spec>` text command, which stays unchanged.

**Tech Stack:** Rust (ratatui 0.29, crossterm 0.28, axum, serde), Cloudflare Worker (TypeScript, node:test), vanilla JS embedded in `web_index.html`.

**Spec:** `docs/superpowers/specs/2026-06-11-model-picker-design.md`
**Worktree/branch:** `/Users/dongxu/pie/.claude/worktrees/model-picker`, branch `worktree-model-picker`.

Conventions for every task: run `make fmt` before each commit; commit messages end with the trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`. Tests must never hit a real provider API.

---

### Task 1: `model_picker::catalog()` — filtered, grouped model catalog

**Files:**
- Create: `crates/coding-agent/src/model_picker.rs`
- Modify: `crates/coding-agent/src/main.rs` (add `mod model_picker;` alongside `mod model;` at line ~31)
- Modify: `crates/coding-agent/src/commands.rs:928` (`fn parse_model_spec` → `pub(crate) fn`), `crates/coding-agent/src/commands.rs:942` (`fn model_credential_hint` → `pub(crate) fn`)

- [ ] **Step 1: Create the module with failing tests**

Create `crates/coding-agent/src/model_picker.rs`:

```rust
//! Curated model catalog + state machine for the interactive picker (TUI
//! overlay and web dropdown).
//!
//! Only models speaking one of the two supported API families are surfaced:
//! OpenAI-compatible (`openai-completions`, `openai-responses`,
//! `openai-codex-responses`) and Claude-compatible (`anthropic-messages`).
//! `/model <provider:model-id>` remains the uncurated escape hatch.

use serde::Serialize;
use std::collections::BTreeMap;

const SUPPORTED_APIS: [&str; 4] = [
    "openai-completions",
    "openai-responses",
    "openai-codex-responses",
    "anthropic-messages",
];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ModelEntry {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ProviderGroup {
    pub provider: String,
    pub has_credential: bool,
    pub models: Vec<ModelEntry>,
}

/// Filtered + grouped catalog with live credential detection.
pub(crate) fn catalog() -> Vec<ProviderGroup> {
    catalog_with(|provider| crate::commands::model_credential_hint(provider).is_none())
}

/// Testable core: credential detection injected.
fn catalog_with(has_credential: impl Fn(&str) -> bool) -> Vec<ProviderGroup> {
    let mut groups: BTreeMap<String, Vec<ModelEntry>> = BTreeMap::new();
    for model in pie_ai::list_models() {
        if !SUPPORTED_APIS.contains(&model.api.0.as_str()) {
            continue;
        }
        groups.entry(model.provider.0.clone()).or_default().push(ModelEntry {
            id: model.id,
            name: model.name,
        });
    }
    groups
        .into_iter()
        .map(|(provider, mut models)| {
            models.sort_by(|a, b| a.id.cmp(&b.id));
            ProviderGroup {
                has_credential: has_credential(&provider),
                provider,
                models,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom_model(provider: &str, id: &str, api: &str) -> pie_ai::Model {
        pie_ai::Model {
            id: id.into(),
            name: id.into(),
            api: pie_ai::Api::from(api),
            provider: pie_ai::Provider::from(provider),
            base_url: "http://localhost:9999/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pie_ai::ModelCost::default(),
            context_window: 8192,
            max_tokens: 1024,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn catalog_keeps_openai_and_anthropic_families_only() {
        pie_ai::register_custom_model(custom_model(
            "picker-test-ds4",
            "deepseek-v4-flash",
            "openai-completions",
        ));
        pie_ai::register_custom_model(custom_model(
            "picker-test-bedrock",
            "claude-x",
            "bedrock-converse-stream",
        ));

        let groups = catalog_with(|_| true);
        let providers: Vec<&str> = groups.iter().map(|g| g.provider.as_str()).collect();
        assert!(providers.contains(&"picker-test-ds4"));
        assert!(!providers.contains(&"picker-test-bedrock"));

        pie_ai::unregister_custom_model(
            &pie_ai::Provider::from("picker-test-ds4"),
            "deepseek-v4-flash",
        );
        pie_ai::unregister_custom_model(
            &pie_ai::Provider::from("picker-test-bedrock"),
            "claude-x",
        );
    }

    #[test]
    fn catalog_sorts_models_and_flags_credentials() {
        let groups = catalog_with(|provider| provider == "anthropic");
        let anthropic = groups
            .iter()
            .find(|g| g.provider == "anthropic")
            .expect("embedded anthropic models present");
        assert!(anthropic.has_credential);
        assert!(!anthropic.models.is_empty());
        let mut sorted = anthropic.models.clone();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(anthropic.models, sorted);

        let openai = groups
            .iter()
            .find(|g| g.provider == "openai")
            .expect("embedded openai models present");
        assert!(!openai.has_credential);
    }
}
```

Add to `crates/coding-agent/src/main.rs` next to `mod model;`:

```rust
mod model_picker;
```

In `crates/coding-agent/src/commands.rs` change line 928 and 942:

```rust
pub(crate) fn parse_model_spec(spec: &str) -> Option<(&str, &str)> {
```

```rust
pub(crate) fn model_credential_hint(provider: &str) -> Option<String> {
```

- [ ] **Step 2: Run the tests, expect failure first, then pass**

Run: `cargo test -p pie-coding-agent model_picker::`
First run may fail to compile until the module is registered; once compiling, both tests must PASS. (Note: `catalog_with` filters use unique `picker-test-*` provider names so parallel tests touching the global model registry don't collide.)

- [ ] **Step 3: Clippy clean**

Run: `cargo clippy -p pie-coding-agent --all-targets -- -D warnings`
Expected: no warnings. (`catalog()` is not yet called anywhere — if clippy flags dead code, add `#[allow(dead_code)]` TEMPORARILY and remove it in Task 4, or simply proceed: binary crates report unused `pub(crate)` as dead_code, so the allow is expected here and MUST be removed by Task 5.)

- [ ] **Step 4: Commit**

```bash
make fmt && git add -A && git commit -m "feat(coding-agent): curated model catalog for the interactive picker

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `ModelPickerState` — two-level navigation state machine

**Files:**
- Modify: `crates/coding-agent/src/model_picker.rs` (append below `catalog_with`)

- [ ] **Step 1: Write failing tests**

Append to the `tests` module in `model_picker.rs`:

```rust
    fn two_groups() -> Vec<ProviderGroup> {
        vec![
            ProviderGroup {
                provider: "anthropic".into(),
                has_credential: true,
                models: vec![
                    ModelEntry { id: "claude-haiku-4-5".into(), name: "Haiku".into() },
                    ModelEntry { id: "claude-opus-4-8".into(), name: "Opus".into() },
                ],
            },
            ProviderGroup {
                provider: "openai".into(),
                has_credential: false,
                models: vec![ModelEntry { id: "gpt-5.2".into(), name: "GPT".into() }],
            },
        ]
    }

    #[test]
    fn picker_navigates_descends_and_selects() {
        let mut p = ModelPickerState::new(two_groups(), None);
        assert_eq!(p.enter(), None); // descend into anthropic
        assert!(matches!(p.level, PickerLevel::Models { provider_idx: 0 }));
        p.down();
        assert_eq!(p.enter().as_deref(), Some("anthropic:claude-opus-4-8"));
    }

    #[test]
    fn picker_back_returns_to_providers_then_closes() {
        let mut p = ModelPickerState::new(two_groups(), None);
        p.down(); // openai
        p.enter();
        assert!(!p.back()); // back to providers…
        assert!(matches!(p.level, PickerLevel::Providers));
        assert_eq!(p.cursor, 1); // …with cursor restored to openai
        assert!(p.back()); // top level: close
    }

    #[test]
    fn picker_cursor_clamps_at_bounds() {
        let mut p = ModelPickerState::new(two_groups(), None);
        p.up();
        assert_eq!(p.cursor, 0);
        p.down();
        p.down();
        p.down();
        assert_eq!(p.cursor, 1); // two providers, clamped
    }

    #[test]
    fn picker_starts_on_active_model_when_descending() {
        let active = Some(("anthropic".into(), "claude-opus-4-8".into()));
        let mut p = ModelPickerState::new(two_groups(), active);
        p.enter();
        assert_eq!(p.cursor, 1); // active model preselected
        let (_, rows) = p.view(10);
        assert!(rows[1].0.contains('●'));
        assert!(rows[1].1); // selected row
    }

    #[test]
    fn picker_view_windows_around_cursor() {
        let groups = vec![ProviderGroup {
            provider: "anthropic".into(),
            has_credential: true,
            models: (0..20)
                .map(|i| ModelEntry { id: format!("m-{i:02}"), name: format!("m-{i:02}") })
                .collect(),
        }];
        let mut p = ModelPickerState::new(groups, None);
        p.enter();
        for _ in 0..15 {
            p.down();
        }
        let (_, rows) = p.view(5);
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().any(|(text, selected)| *selected && text.contains("m-15")));
    }

    #[test]
    fn picker_empty_catalog_is_inert() {
        let mut p = ModelPickerState::new(vec![], None);
        assert_eq!(p.enter(), None);
        p.down();
        assert_eq!(p.cursor, 0);
        assert!(p.back()); // closes immediately
    }
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p pie-coding-agent model_picker::`
Expected: compile error (`ModelPickerState` not defined).

- [ ] **Step 3: Implement the state machine**

Insert above the `tests` module in `model_picker.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PickerLevel {
    Providers,
    Models { provider_idx: usize },
}

/// Pure two-level navigation state. Rendering and IO live in `ui/`.
pub(crate) struct ModelPickerState {
    pub groups: Vec<ProviderGroup>,
    pub level: PickerLevel,
    pub cursor: usize,
    /// Active `(provider, id)` — marked `●` in the model list.
    pub active: Option<(String, String)>,
}

impl ModelPickerState {
    pub fn new(groups: Vec<ProviderGroup>, active: Option<(String, String)>) -> Self {
        Self { groups, level: PickerLevel::Providers, cursor: 0, active }
    }

    fn len(&self) -> usize {
        match self.level {
            PickerLevel::Providers => self.groups.len(),
            PickerLevel::Models { provider_idx } => self.groups[provider_idx].models.len(),
        }
    }

    pub fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.cursor + 1 < self.len() {
            self.cursor += 1;
        }
    }

    /// Enter: descend at provider level (returns `None`), select at model
    /// level (returns the `provider:id` spec).
    pub fn enter(&mut self) -> Option<String> {
        match self.level {
            PickerLevel::Providers => {
                if self.groups.is_empty() {
                    return None;
                }
                let provider_idx = self.cursor;
                let group = &self.groups[provider_idx];
                self.cursor = self
                    .active
                    .as_ref()
                    .filter(|(p, _)| *p == group.provider)
                    .and_then(|(_, id)| group.models.iter().position(|m| m.id == *id))
                    .unwrap_or(0);
                self.level = PickerLevel::Models { provider_idx };
                None
            }
            PickerLevel::Models { provider_idx } => {
                let group = &self.groups[provider_idx];
                Some(format!("{}:{}", group.provider, group.models[self.cursor].id))
            }
        }
    }

    /// Esc: model list → provider list (returns `false`), provider list →
    /// close (returns `true`).
    pub fn back(&mut self) -> bool {
        match self.level {
            PickerLevel::Providers => true,
            PickerLevel::Models { provider_idx } => {
                self.level = PickerLevel::Providers;
                self.cursor = provider_idx;
                false
            }
        }
    }

    /// Window of rows around the cursor: `(title, [(text, is_selected)])`.
    pub fn view(&self, visible: usize) -> (String, Vec<(String, bool)>) {
        let (title, rows): (String, Vec<String>) = match self.level {
            PickerLevel::Providers => (
                "Select provider".into(),
                self.groups
                    .iter()
                    .map(|g| {
                        let key = if g.has_credential { "" } else { " · no key" };
                        format!("{} ({}){}", g.provider, g.models.len(), key)
                    })
                    .collect(),
            ),
            PickerLevel::Models { provider_idx } => {
                let group = &self.groups[provider_idx];
                (
                    format!("{} models", group.provider),
                    group
                        .models
                        .iter()
                        .map(|m| {
                            let active = self
                                .active
                                .as_ref()
                                .is_some_and(|(p, id)| *p == group.provider && *id == m.id);
                            if active { format!("{} ●", m.id) } else { m.id.clone() }
                        })
                        .collect(),
                )
            }
        };
        let visible = visible.max(1);
        let start = (self.cursor + 1).saturating_sub(visible);
        let windowed = rows
            .into_iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, text)| (text, i == self.cursor))
            .collect();
        (title, windowed)
    }
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p pie-coding-agent model_picker::`
Expected: all model_picker tests PASS.

- [ ] **Step 5: Commit**

```bash
make fmt && git add -A && git commit -m "feat(coding-agent): model picker two-level state machine

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `CommandOutcome::OpenModelPicker` — bare `/model` requests the picker

**Files:**
- Modify: `crates/coding-agent/src/commands.rs` (enum at line ~66, `Debug` impl at ~121, `ModelCommand::run` at ~881)
- Modify: `crates/coding-agent/src/ui/mod.rs` (plain-REPL dispatch match at ~1875)
- Modify: `crates/coding-agent/src/ui/web.rs` (`dispatch_web_slash` match at ~427)

- [ ] **Step 1: Add the outcome variant**

In the `CommandOutcome` enum (after `Handled` documentation block style — insert before the closing brace, next to other UI-action variants):

```rust
    /// Bare `/model` — the REPL owns the interactive picker UI, so the
    /// command requests it instead of printing the catalog.
    OpenModelPicker,
```

In `impl std::fmt::Debug for CommandOutcome` add:

```rust
            Self::OpenModelPicker => f.write_str("OpenModelPicker"),
```

- [ ] **Step 2: Change `ModelCommand::run`'s bare-argument arm**

Replace the `if argv.is_empty()` block (commands.rs:882-890) with:

```rust
        if argv.is_empty() {
            return CommandOutcome::OpenModelPicker;
        }
```

- [ ] **Step 3: Handle the variant in the two non-TUI dispatch matches**

`crates/coding-agent/src/ui/mod.rs` plain-REPL match (the one whose `Error` arm is `eprintln!("error: {e}")`, ~line 1875) — add an arm that reproduces today's text behavior:

```rust
                    CommandOutcome::OpenModelPicker => {
                        match self.kernel.harness().agent().state().model.clone() {
                            Some(m) => println!("active model: {}:{}", m.provider.0, m.id),
                            None => println!("(no model active)"),
                        }
                        println!(
                            "interactive picker needs the TUI; use /model <provider:model-id> or /model list"
                        );
                    }
```

`crates/coding-agent/src/ui/web.rs` `dispatch_web_slash` match — add:

```rust
            CommandOutcome::OpenModelPicker => {
                let active = match self.kernel.harness().agent().state().model.clone() {
                    Some(m) => format!("active model: {}:{}", m.provider.0, m.id),
                    None => "(no model active)".into(),
                };
                self.system_line(format!(
                    "{active} — click the model name in the header to switch"
                ));
            }
```

The TUI `dispatch_slash` match (ui/mod.rs:1052) will not compile until it has an arm too; add a TEMPORARY no-op arm that Task 4 replaces:

```rust
            CommandOutcome::OpenModelPicker => {}
```

- [ ] **Step 4: Build and run the full crate tests**

Run: `cargo test -p pie-coding-agent`
Expected: PASS (no existing test asserts the old bare-`/model` print behavior; if one fails, update it to expect `OpenModelPicker`).

- [ ] **Step 5: Commit**

```bash
make fmt && git add -A && git commit -m "feat(coding-agent): bare /model requests the interactive picker

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: TUI overlay — open, navigate, render, select

**Files:**
- Modify: `crates/coding-agent/src/ui/mod.rs`:
  - `App` struct fields (~line 139, next to `control_plane_prompt`)
  - `App::new` initializer (~line 209)
  - `handle_key` (~line 713, after the control-plane check)
  - `dispatch_slash` (replace the Task 3 no-op arm)
  - `render` (~line 1440, BEFORE `render_control_plane_prompt` so the approval dialog wins when both are open)
  - tests module (fixtures at ~2155 are reusable as-is)

- [ ] **Step 1: Write failing App-level tests**

Add to the tests module in `ui/mod.rs` (near the control-plane prompt tests at ~2885):

```rust
    fn picker_groups() -> Vec<crate::model_picker::ProviderGroup> {
        vec![crate::model_picker::ProviderGroup {
            provider: "anthropic".into(),
            has_credential: true,
            models: vec![crate::model_picker::ModelEntry {
                id: "claude-haiku-4-5".into(),
                name: "Claude Haiku 4.5".into(),
            }],
        }]
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn model_picker_keys_are_modal_and_navigate() {
        let mut app = test_app();
        app.model_picker = Some(crate::model_picker::ModelPickerState::new(
            picker_groups(),
            None,
        ));
        // Modal: keys are consumed while open.
        assert!(app.handle_model_picker_key(&key(KeyCode::Down)).await);
        // Esc at the top level closes.
        assert!(app.handle_model_picker_key(&key(KeyCode::Esc)).await);
        assert!(app.model_picker.is_none());
        // Closed: keys pass through.
        assert!(!app.handle_model_picker_key(&key(KeyCode::Down)).await);
    }

    #[tokio::test]
    async fn model_picker_enter_descends_then_switches_model() {
        let mut app = test_app();
        app.model_picker = Some(crate::model_picker::ModelPickerState::new(
            picker_groups(),
            None,
        ));
        assert!(app.handle_model_picker_key(&key(KeyCode::Enter)).await); // descend
        assert!(app.handle_model_picker_key(&key(KeyCode::Enter)).await); // select
        assert!(app.model_picker.is_none());
        let model = app.kernel.harness().agent().state().model.clone().unwrap();
        assert_eq!(model.provider.0, "anthropic");
        assert_eq!(model.id, "claude-haiku-4-5");
        assert!(feed_text(&app).contains("switched to anthropic:claude-haiku-4-5"));
    }

    #[test]
    fn model_picker_renders_centered_overlay() {
        let mut app = test_app();
        app.model_picker = Some(crate::model_picker::ModelPickerState::new(
            picker_groups(),
            None,
        ));
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Select provider"));
        assert!(text.contains("anthropic (1)"));
    }
```

Note: `claude-haiku-4-5` must exist in the embedded catalog for `pie_ai::get_model` to resolve (it is the example model in the `/model` usage hint; verify with `cargo run -p pie-coding-agent -- --help` or the generated catalog if the select test fails on lookup).

- [ ] **Step 2: Run tests, verify compile failure**

Run: `cargo test -p pie-coding-agent ui::tests::model_picker`
Expected: FAIL (`model_picker` field and `handle_model_picker_key` missing).

- [ ] **Step 3: Implement the overlay**

`App` struct — add next to `control_plane_prompt`:

```rust
    model_picker: Option<crate::model_picker::ModelPickerState>,
    /// Cached for web snapshots; refreshed on picker open and model switch.
    model_catalog: Vec<crate::model_picker::ProviderGroup>,
```

`App::new` initializer — add next to `control_plane_prompt: None`:

```rust
            model_picker: None,
            model_catalog: crate::model_picker::catalog(),
```

(Remove any temporary `#[allow(dead_code)]` left from Task 1.)

`handle_key` — directly after the `handle_control_plane_prompt_key` check:

```rust
        if self.handle_model_picker_key(&key).await {
            return Ok(());
        }
```

New methods on `App` (place after `handle_control_plane_prompt_key`):

```rust
    fn open_model_picker(&mut self) {
        self.model_catalog = crate::model_picker::catalog();
        if self.model_catalog.is_empty() {
            self.system_line(
                "no openai/anthropic-compatible models registered; use /model <provider:model-id>",
            );
            return;
        }
        let active = self
            .kernel
            .harness()
            .agent()
            .state()
            .model
            .clone()
            .map(|m| (m.provider.0, m.id));
        self.model_picker = Some(crate::model_picker::ModelPickerState::new(
            self.model_catalog.clone(),
            active,
        ));
    }

    async fn handle_model_picker_key(&mut self, key: &KeyEvent) -> bool {
        if self.model_picker.is_none() {
            return false;
        }
        if key.kind == KeyEventKind::Release {
            return true;
        }
        let Some(picker) = self.model_picker.as_mut() else {
            return true;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => picker.up(),
            KeyCode::Down | KeyCode::Char('j') => picker.down(),
            KeyCode::Enter => {
                if let Some(spec) = picker.enter() {
                    self.model_picker = None;
                    self.set_model_from_spec(&spec).await;
                }
            }
            KeyCode::Esc => {
                if picker.back() {
                    self.model_picker = None;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.model_picker = None;
            }
            _ => {}
        }
        true
    }

    async fn set_model_from_spec(&mut self, spec: &str) {
        let Some((provider, id)) = commands::parse_model_spec(spec) else {
            self.error_line(format!("invalid model spec: {spec}"));
            return;
        };
        let (provider, id) = (provider.to_string(), id.to_string());
        let Some(model) = pie_ai::get_model(&pie_ai::Provider::from(provider.as_str()), &id)
        else {
            self.error_line(format!("unknown model: {provider}:{id}"));
            return;
        };
        match self.kernel.harness().set_model(model).await {
            Ok(_) => {
                if let Some(hint) = commands::model_credential_hint(&provider) {
                    self.system_line(format!(
                        "switched to {provider}:{id} — login required: {hint}"
                    ));
                } else {
                    self.system_line(format!("switched to {provider}:{id}"));
                }
                self.model_catalog = crate::model_picker::catalog();
            }
            Err(e) => self.error_line(format!("set_model failed: {e}")),
        }
    }
```

(If `pie_ai::get_model` / `pie_ai::Provider` are not yet imported in `ui/mod.rs`, use the fully qualified paths as written. If `harness().set_model` differs in name, mirror exactly what `ModelCommand::run` calls at commands.rs:914.)

`dispatch_slash` — replace the Task 3 no-op arm:

```rust
            CommandOutcome::OpenModelPicker => self.open_model_picker(),
```

`render` — insert BEFORE `self.render_control_plane_prompt(frame);`:

```rust
        self.render_model_picker(frame);
```

New render function (place next to `render_control_plane_prompt`):

```rust
    fn render_model_picker(&self, frame: &mut ratatui::Frame) {
        let Some(picker) = self.model_picker.as_ref() else {
            return;
        };
        let area = frame.area();
        let width = area.width.clamp(40, 64);
        let height = area.height.clamp(8, 18);
        let rect = centered_rect(area, width, height);
        // borders (2) + title line + blank + footer = 5 rows of chrome
        let visible = rect.height.saturating_sub(5).max(1) as usize;
        let (title, rows) = picker.view(visible);
        let mut text = vec![
            Line::styled(title, Style::default().fg(Color::Yellow)),
            Line::raw(""),
        ];
        for (label, selected) in rows {
            if selected {
                text.push(Line::styled(
                    format!("❯ {label}"),
                    Style::default().fg(Color::Cyan),
                ));
            } else {
                text.push(Line::raw(format!("  {label}")));
            }
        }
        text.push(Line::styled(
            "↑↓/jk navigate · Enter select · Esc back",
            Style::default().fg(Color::DarkGray),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Select model ")
            .border_style(Style::default().fg(Color::Cyan));
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(text).block(block), rect);
    }
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p pie-coding-agent ui::tests::model_picker && cargo test -p pie-coding-agent model_picker::`
Expected: PASS.

- [ ] **Step 5: Full crate test + clippy**

Run: `cargo test -p pie-coding-agent && cargo clippy -p pie-coding-agent --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 6: Commit**

```bash
make fmt && git add -A && git commit -m "feat(tui): interactive two-level model picker overlay for /model

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Web — catalog in snapshot + local `POST /model`

**Files:**
- Modify: `crates/coding-agent/src/ui/web.rs`:
  - `WebCommand` enum (~line 41)
  - `WebSnapshot` struct (~line 56) and `web_snapshot()` (~line 497)
  - request structs (~line 196)
  - `handle_web_command` (~line 317)
  - `web_router` (~line 630) + new handler

- [ ] **Step 1: Add `model_catalog` to the snapshot**

`WebSnapshot` — add after `model: String,`:

```rust
    model_catalog: Vec<crate::model_picker::ProviderGroup>,
```

In `web_snapshot()` add alongside the `model:` field assignment:

```rust
            model_catalog: self.model_catalog.clone(),
```

- [ ] **Step 2: Add the command + route**

`WebCommand` — add variant:

```rust
    SetModel {
        spec: String,
    },
```

Request struct (next to `PromptRequest`):

```rust
#[derive(Debug, Deserialize)]
struct SetModelRequest {
    model: String,
}
```

`handle_web_command` — add arm:

```rust
            WebCommand::SetModel { spec } => self.set_model_from_spec(&spec).await,
```

(`publish_snapshot` already runs after every `handle_web_command` in the select loop, so the label refresh is automatic.)

`web_router` — add route after `.route("/prompt", post(prompt))`:

```rust
        .route("/model", post(set_model))
```

Handler (next to the `prompt` handler — copy its State/Json signature style):

```rust
async fn set_model(
    State(state): State<HttpState>,
    Json(request): Json<SetModelRequest>,
) -> impl IntoResponse {
    let accepted = state
        .commands
        .send(WebCommand::SetModel { spec: request.model })
        .is_ok();
    Json(CommandAccepted { accepted })
}
```

- [ ] **Step 3: Test**

Check how existing web.rs tests exercise the router (search `mod tests` in web.rs). If a router-level test fixture exists, add:

```rust
    // POST /model enqueues a SetModel command for the app loop.
    // Build HttpState with a capturing mpsc channel, POST {"model":"anthropic:claude-haiku-4-5"},
    // assert the channel received WebCommand::SetModel { spec } and the response is {"accepted":true}.
```

If web.rs has no router test precedent, add a serde test for the request type instead:

```rust
    #[test]
    fn set_model_request_deserializes() {
        let req: SetModelRequest =
            serde_json::from_str(r#"{"model":"anthropic:claude-haiku-4-5"}"#).unwrap();
        assert_eq!(req.model, "anthropic:claude-haiku-4-5");
    }
```

Run: `cargo test -p pie-coding-agent`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
make fmt && git add -A && git commit -m "feat(web): model catalog in snapshot and POST /model switch endpoint

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Relay — `WorkerFrame::SetModel` end to end

**Files:**
- Modify: `crates/coding-agent/src/ui/relay.rs` (`WorkerFrame` ~line 50, `start` ~line 165, `run_relay` ~line 208, frame match ~line 290, serde test ~line 405)
- Modify: `crates/coding-agent/src/ui/mod.rs` (channel fields ~line 164, init ~line 187, `relay::start` call ~line 483, TUI select loop ~line 409)
- Modify: `crates/coding-agent/src/ui/web.rs` (take rx ~line 235, select loop ~line 273)
- Modify: `workers/fefe-hub/src/relay.ts` (route switch ~line 126, forward helpers ~line 240)
- Modify: `workers/fefe-hub/src/index.ts` (exports, line 3)
- Modify: `workers/fefe-hub/tests/relay.test.mjs`

- [ ] **Step 1: Rust side — failing serde test**

In relay.rs tests (next to the existing frame tests at ~405):

```rust
        let set_model: WorkerFrame =
            serde_json::from_str(r#"{"type":"set_model","model":"anthropic:claude-haiku-4-5"}"#)
                .unwrap();
        assert_eq!(
            set_model,
            WorkerFrame::SetModel { model: "anthropic:claude-haiku-4-5".into() }
        );
```

Run: `cargo test -p pie-coding-agent relay` — expected: FAIL (variant missing).

- [ ] **Step 2: Implement the Rust side**

`WorkerFrame` — add variant (serde tag becomes `set_model` via `rename_all = "snake_case"`):

```rust
    /// Remote model switch from the shared web UI — first-class like
    /// `ControlPlaneResolve`; the capability URL grants it.
    SetModel {
        model: String,
    },
```

`relay::start` and `run_relay` — add a `model_tx: mpsc::UnboundedSender<String>` parameter after `resolve_tx`, and in the frame match:

```rust
                                Ok(WorkerFrame::SetModel { model }) => {
                                    let _ = model_tx.send(model);
                                }
```

`ui/mod.rs` — add channel pair next to the other relay channels:

```rust
    relay_model_tx: UnboundedSender<String>,
    relay_model_rx: Option<UnboundedReceiver<String>>,
```

```rust
        let (relay_model_tx, relay_model_rx) = tokio::sync::mpsc::unbounded_channel();
```

```rust
            relay_model_tx,
            relay_model_rx: Some(relay_model_rx),
```

`relay::start` call (~483) — pass `self.relay_model_tx.clone()` after `relay_resolve_tx`.

TUI run loop — take the receiver next to `relay_resolve_rx` (~375) and add a select arm next to the `relay_resolve_rx` arm (~415):

```rust
                Some(spec) = relay_model_rx.recv() => {
                    self.system_line(format!("[web] set model: {spec}"));
                    self.set_model_from_spec(&spec).await;
                }
```

`web.rs` `run_web` — same: take the rx next to `relay_resolve_rx` (~243) and add the arm:

```rust
                Some(spec) = relay_model_rx.recv() => {
                    self.system_line(format!("[web] set model: {spec}"));
                    self.set_model_from_spec(&spec).await;
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
```

Run: `cargo test -p pie-coding-agent` — expected: PASS.

- [ ] **Step 3: Worker side — failing test**

In `workers/fefe-hub/tests/relay.test.mjs` add:

```js
import { validateSetModel } from "../dist/index.js"; // extend the existing import list

test("set_model body validation accepts specs and rejects junk", () => {
  assert.equal(validateSetModel("anthropic:claude-haiku-4-5"), "anthropic:claude-haiku-4-5");
  assert.equal(validateSetModel("  ds4:deepseek-v4-flash  "), "ds4:deepseek-v4-flash");
  assert.equal(validateSetModel(""), null);
  assert.equal(validateSetModel(42), null);
  assert.equal(validateSetModel("x".repeat(300)), null);
});
```

Also extend the router forwarding test's path lists (both arrays in `router forwards session subpaths…`) with `"/model"`.

Run (from `workers/fefe-hub/`): `npm test`
Expected: FAIL (`validateSetModel` not exported).

- [ ] **Step 4: Implement the worker side**

`workers/fefe-hub/src/relay.ts` — route case (after `case "/abort"`):

```ts
      case "/model":
        return this.forwardSetModel(request);
```

Helpers (next to `forwardResolve`):

```ts
  private async forwardSetModel(request: Request): Promise<Response> {
    let body: { model?: unknown };
    try {
      body = (await request.json()) as { model?: unknown };
    } catch {
      return json({ ok: false, error: "invalid_json" }, 400);
    }
    const spec = validateSetModel(body.model);
    if (!spec) {
      return json({ ok: false, error: "invalid_model" }, 400);
    }
    return this.forward({ type: "set_model", model: spec });
  }
```

Exported validator (module level, near `parseSessionPath`):

```ts
export function validateSetModel(model: unknown): string | null {
  if (typeof model !== "string") return null;
  const spec = model.trim();
  if (!spec || spec.length > 256) return null;
  return spec;
}
```

`workers/fefe-hub/src/index.ts` line 3 — add `validateSetModel` to the re-export list.

Run (from `workers/fefe-hub/`): `npm test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
make fmt && git add -A && git commit -m "feat(relay): first-class remote model switch (WorkerFrame::SetModel + /model route)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Web frontend — clickable model dropdown

**Files:**
- Modify: `crates/coding-agent/src/ui/web_index.html` (header markup ~line 764, CSS after the `.meta` rule ~line 120, JS in `render()` ~line 1531 and listeners near ~line 1681)

- [ ] **Step 1: Markup**

Replace line 764 (`<span id="model" class="meta"></span>`) with:

```html
        <span class="model-picker" id="modelPicker">
          <button type="button" id="model" class="meta model-label" title="Switch model"></button>
          <div id="modelMenu" class="model-menu" hidden></div>
        </span>
```

(`model.textContent = snapshot.model` keeps working — the id stays on the button. The responsive `.brand .meta { display: none; }` rule at ~749 keeps hiding it on narrow screens.)

- [ ] **Step 2: CSS**

Add after the `.meta` rule block:

```css
    .model-picker { position: relative; }
    .model-label {
      cursor: pointer; background: none; border: 1px solid transparent;
      border-radius: 6px; padding: 2px 8px; font: inherit; color: inherit;
    }
    .model-label:hover { border-color: var(--line); background: var(--soft); }
    .model-menu {
      position: absolute; top: calc(100% + 4px); left: 0; z-index: 40;
      min-width: 260px; max-height: 60vh; overflow-y: auto;
      background: var(--panel); border: 1px solid var(--line); border-radius: 8px;
      box-shadow: 0 8px 24px var(--shadow); padding: 6px 0; text-align: left;
    }
    .model-menu .menu-provider { padding: 6px 12px 2px; color: var(--muted); font-size: 12px; }
    .model-menu .menu-provider .nokey { color: var(--faint); margin-left: 6px; }
    .model-menu .menu-model {
      padding: 4px 12px 4px 20px; cursor: pointer;
      white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
    }
    .model-menu .menu-model:hover { background: var(--soft); }
    .model-menu .menu-model.active { font-weight: 600; }
```

- [ ] **Step 3: JS**

Near the other element refs / listeners (e.g. next to the abort button listener at ~1681), add:

```js
const modelMenu = document.getElementById('modelMenu');
let modelCatalog = [];

function buildModelMenu() {
  const current = model.textContent;
  const nodes = [];
  for (const group of modelCatalog) {
    const head = node('div', { class: 'menu-provider', text: group.provider });
    if (!group.has_credential) {
      head.appendChild(node('span', { class: 'nokey', text: 'no key' }));
    }
    nodes.push(head);
    for (const entry of group.models) {
      const spec = group.provider + ':' + entry.id;
      const item = node('div', {
        class: 'menu-model' + (spec === current ? ' active' : ''),
        text: entry.id,
      });
      item.title = spec;
      item.addEventListener('click', () => {
        modelMenu.hidden = true;
        fetch('model', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ model: spec }),
        }).catch(() => {});
      });
      nodes.push(item);
    }
  }
  if (!nodes.length) {
    nodes.push(node('div', { class: 'menu-provider', text: 'no compatible models' }));
  }
  modelMenu.replaceChildren(...nodes);
}

model.addEventListener('click', (event) => {
  event.stopPropagation();
  if (modelMenu.hidden) {
    buildModelMenu();
    modelMenu.hidden = false;
  } else {
    modelMenu.hidden = true;
  }
});
document.addEventListener('click', (event) => {
  if (!modelMenu.hidden && !modelMenu.contains(event.target)) {
    modelMenu.hidden = true;
  }
});
```

(Check the existing `node()` helper's signature in this file — it is used by `renderFeedBlocks`; match its attribute conventions exactly.)

In `render(snapshot)` after `model.textContent = snapshot.model;` add:

```js
  modelCatalog = snapshot.model_catalog || [];
```

- [ ] **Step 4: Verify in the running app**

```bash
cargo run -p pie-coding-agent -- --web
```

In the browser: click the model name in the header → grouped dropdown appears; pick a model → label updates after the next snapshot and the feed shows `switched to <provider>:<id>`. Then switch the theme toggle and re-open the menu to confirm both palettes look right. Ctrl-C the server when done.

- [ ] **Step 5: Commit**

```bash
make fmt && git add -A && git commit -m "feat(web): clickable model dropdown in the header

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Full pipeline + TUI manual check

- [ ] **Step 1: Full CI pipeline**

Run: `make ci`
Expected: fmt-check, clippy (`-D warnings`), and the full workspace test suite all PASS.

- [ ] **Step 2: Worker tests once more**

Run (from `workers/fefe-hub/`): `npm test`
Expected: PASS.

- [ ] **Step 3: TUI manual check**

```bash
cargo run -p pie-coding-agent
```

Type `/model` + Enter → provider list overlay appears (anthropic/openai/… with `no key` markers where unauthenticated). Enter on a provider → model list with `●` on the active model. Enter on a model → feed shows `switched to …`; Esc walks back out. Also confirm `/model list` and `/model anthropic:claude-haiku-4-5` still behave exactly as before.

- [ ] **Step 4: Final commit (if the manual check produced fixes)**

```bash
make fmt && git add -A && git commit -m "fix(coding-agent): model picker polish from manual verification

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-review notes

- Spec coverage: filtering rules (Task 1), TUI overlay incl. headless fallback (Tasks 3–4), snapshot catalog + `POST /model` (Task 5), `WorkerFrame::SetModel` (Task 6), dropdown UI (Task 7), error handling (`set_model_from_spec` covers unknown spec/model; empty catalog covered in `open_model_picker`), credential indicator (Tasks 1, 2 view, 7), tests throughout. `/model <spec>` text path untouched.
- Known accepted gap (per spec): credential status in the web dropdown can go stale after `/login` until the next model switch or restart; the TUI picker refreshes on every open.
- Line numbers are from the worktree at commit `f3162b1`; re-grep if drift occurs.
