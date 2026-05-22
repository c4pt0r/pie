//! Provider → env-var mapping. 1:1 stub of `packages/ai/src/env-api-keys.ts`.
//!
//! The repo-root `test.sh` and `pi-test.sh` duplicate this list when stripping keys; keep them
//! in sync.

const ENTRIES: &[(&str, &[&str])] = &[
    ("anthropic", &["ANTHROPIC_API_KEY"]),
    ("openai", &["OPENAI_API_KEY"]),
    ("openai-codex", &["OPENAI_API_KEY"]),
    ("azure-openai-responses", &["AZURE_OPENAI_API_KEY"]),
    ("google", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
    ("google-vertex", &["GOOGLE_APPLICATION_CREDENTIALS"]),
    ("amazon-bedrock", &["AWS_ACCESS_KEY_ID"]),
    ("mistral", &["MISTRAL_API_KEY"]),
    ("xai", &["XAI_API_KEY"]),
    ("groq", &["GROQ_API_KEY"]),
    ("cerebras", &["CEREBRAS_API_KEY"]),
    ("openrouter", &["OPENROUTER_API_KEY"]),
    ("vercel-ai-gateway", &["AI_GATEWAY_API_KEY"]),
    ("zai", &["ZAI_API_KEY"]),
    ("deepseek", &["DEEPSEEK_API_KEY"]),
    ("ds4", &["DS4_API_KEY"]),
    ("fireworks", &["FIREWORKS_API_KEY"]),
    ("together", &["TOGETHER_API_KEY"]),
    ("github-copilot", &["GITHUB_COPILOT_TOKEN"]),
    ("huggingface", &["HUGGINGFACE_API_KEY", "HF_TOKEN"]),
    ("cloudflare-workers-ai", &["CLOUDFLARE_API_TOKEN"]),
];

pub fn get_env_api_key(provider: &str) -> Option<String> {
    for (p, vars) in ENTRIES {
        if *p == provider {
            for v in *vars {
                if let Ok(val) = std::env::var(v) {
                    if !val.is_empty() {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

pub fn env_var_names(provider: &str) -> Vec<&'static str> {
    for (p, vars) in ENTRIES {
        if *p == provider {
            return vars.to_vec();
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    #[test]
    fn ds4_uses_dedicated_local_env_var() {
        assert_eq!(super::env_var_names("ds4"), vec!["DS4_API_KEY"]);
    }
}
