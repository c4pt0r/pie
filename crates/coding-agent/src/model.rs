//! Model auto-detection. Picks the first provider with credentials in env and resolves a
//! reasonable default model id from the embedded pie-ai catalog.

use anyhow::{Result, bail};
use pie_ai::{Model, Provider, get_model};

/// Resolution candidates in priority order. Each is (env var, provider id, default model id).
/// First env var that's set wins.
const CANDIDATES: &[(&str, &str, &str)] = &[
    ("ANTHROPIC_API_KEY", "anthropic", "claude-haiku-4-5"),
    ("OPENAI_API_KEY", "openai", "gpt-4o-mini"),
    ("DS4_API_KEY", "ds4", "deepseek-v4-flash"),
    ("OPENROUTER_API_KEY", "openrouter", "openai/gpt-4o-mini"),
    ("GROQ_API_KEY", "groq", "llama-3.3-70b-versatile"),
    ("MISTRAL_API_KEY", "mistral", "mistral-large-latest"),
    ("GEMINI_API_KEY", "google", "gemini-2.0-flash"),
    ("GOOGLE_API_KEY", "google", "gemini-2.0-flash"),
];

/// Returns the resolved model + provider id of the chosen entry. If the catalog doesn't
/// contain the default model id, returns an error so the caller can ask the user to specify
/// a model explicitly.
pub fn auto_detect_model(
    override_provider: Option<&str>,
    override_model: Option<&str>,
) -> Result<Model> {
    // Explicit overrides win.
    if let (Some(p), Some(id)) = (override_provider, override_model) {
        let provider = Provider::from(p);
        if let Some(m) = get_model(&provider, id) {
            return Ok(m);
        }
        bail!("{}", explicit_model_not_found_message(p, id, true));
    }
    // Detect by env, with the auth.json store as fallback (issue #13).
    let store = crate::auth::AuthStore::load().unwrap_or_default();
    for (env, provider, model_id) in CANDIDATES {
        let env_set = std::env::var(env)
            .ok()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let stored = store.get(provider).is_some();
        if !env_set && !stored {
            continue;
        }
        if let Some(m) = get_model(&Provider::from(*provider), model_id) {
            return Ok(m);
        }
        // Catalog miss — pick *any* model for this provider as a fallback so the agent
        // still runs.
        if let Some(any) = first_model_for_provider(provider) {
            return Ok(any);
        }
        if *provider == "ds4" {
            bail!(
                "{}",
                explicit_model_not_found_message(provider, model_id, true)
            );
        }
    }
    bail!(
        "no API key found. Set one of: {} env vars, or run `/login <provider> <key>` from inside pie.",
        CANDIDATES
            .iter()
            .map(|c| c.0)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn explicit_model_not_found_message(provider: &str, id: &str, show_local_hint: bool) -> String {
    let mut by_provider = std::collections::BTreeMap::<String, Vec<String>>::new();
    for model in pie_ai::list_models() {
        by_provider
            .entry(model.provider.0)
            .or_default()
            .push(model.id);
    }
    let Some(models) = by_provider.get_mut(provider) else {
        let providers = by_provider
            .iter()
            .map(|(provider, models)| format!("{provider}({})", models.len()))
            .collect::<Vec<_>>()
            .join(", ");
        let hint = if show_local_hint && provider == "ds4" {
            " For local DS4, pass --base-url http://127.0.0.1:8000/v1, set DS4_BASE_URL, or add ds4 to ~/.pie/models.json."
        } else {
            ""
        };
        return format!(
            "model provider not found in catalog: provider={provider}. Known providers: {providers}"
        ) + hint;
    };
    models.sort();
    let candidates = models
        .iter()
        .take(12)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let more = if models.len() > 12 {
        format!(
            "; run `/model list {provider}` inside pie for all {} models",
            models.len()
        )
    } else {
        String::new()
    };
    format!(
        "model not found in catalog: provider={provider} id={id}. Candidates: {candidates}{more}"
    )
}

fn first_model_for_provider(provider: &str) -> Option<Model> {
    let p = Provider::from(provider);
    pie_ai::list_models().into_iter().find(|m| m.provider == p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_model(provider: &str, id: &str) -> Model {
        Model {
            id: id.into(),
            name: format!("Local {id}"),
            api: pie_ai::Api::from("openai-responses"),
            provider: Provider::from(provider),
            base_url: "http://127.0.0.1:8000/v1".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![pie_ai::InputModality::Text],
            cost: pie_ai::ModelCost::default(),
            context_window: 100_000,
            max_tokens: 100_000,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn explicit_override_resolves_custom_model_registered_before_detection() {
        let provider = Provider::from("local-test-model-detect");
        let id = "deepseek-v4-flash";
        pie_ai::register_custom_model(local_model(&provider.0, id));

        let resolved = auto_detect_model(Some(&provider.0), Some(id)).unwrap();
        assert_eq!(resolved.provider, provider);
        assert_eq!(resolved.id, id);

        pie_ai::unregister_custom_model(&Provider::from("local-test-model-detect"), id);
    }
}
