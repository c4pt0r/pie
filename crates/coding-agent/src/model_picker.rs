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
        groups
            .entry(model.provider.0.clone())
            .or_default()
            .push(ModelEntry {
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
        Self {
            groups,
            level: PickerLevel::Providers,
            cursor: 0,
            active,
        }
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
                Some(format!(
                    "{}:{}",
                    group.provider, group.models[self.cursor].id
                ))
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
                            if active {
                                format!("{} ●", m.id)
                            } else {
                                m.id.clone()
                            }
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
        pie_ai::unregister_custom_model(&pie_ai::Provider::from("picker-test-bedrock"), "claude-x");
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

    fn two_groups() -> Vec<ProviderGroup> {
        vec![
            ProviderGroup {
                provider: "anthropic".into(),
                has_credential: true,
                models: vec![
                    ModelEntry {
                        id: "claude-haiku-4-5".into(),
                        name: "Haiku".into(),
                    },
                    ModelEntry {
                        id: "claude-opus-4-8".into(),
                        name: "Opus".into(),
                    },
                ],
            },
            ProviderGroup {
                provider: "openai".into(),
                has_credential: false,
                models: vec![ModelEntry {
                    id: "gpt-5.2".into(),
                    name: "GPT".into(),
                }],
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
                .map(|i| ModelEntry {
                    id: format!("m-{i:02}"),
                    name: format!("m-{i:02}"),
                })
                .collect(),
        }];
        let mut p = ModelPickerState::new(groups, None);
        p.enter();
        for _ in 0..15 {
            p.down();
        }
        let (_, rows) = p.view(5);
        assert_eq!(rows.len(), 5);
        assert!(
            rows.iter()
                .any(|(text, selected)| *selected && text.contains("m-15"))
        );
    }

    #[test]
    fn picker_empty_catalog_is_inert() {
        let mut p = ModelPickerState::new(vec![], None);
        assert_eq!(p.enter(), None);
        p.down();
        assert_eq!(p.cursor, 0);
        assert!(p.back()); // closes immediately
    }
}
