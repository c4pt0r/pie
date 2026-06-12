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

// TODO(model-picker): consumed from Task 4 on
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ModelEntry {
    pub id: String,
    pub name: String,
}

// TODO(model-picker): consumed from Task 4 on
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ProviderGroup {
    pub provider: String,
    pub has_credential: bool,
    pub models: Vec<ModelEntry>,
}

/// Filtered + grouped catalog with live credential detection.
// TODO(model-picker): consumed from Task 4 on
#[allow(dead_code)]
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
}
