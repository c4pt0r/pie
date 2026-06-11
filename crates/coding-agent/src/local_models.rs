//! Local/custom model definitions loaded by the CLI before model resolution.
//!
//! This is intentionally a `coding-agent` concern: `pie-ai` already has the in-process custom
//! registry, while the CLI owns user/project configuration and user-visible diagnostics.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pie_ai::{
    Api, InputModality, Model, ModelCost, ModelThinkingLevel, Provider, ThinkingLevelMap,
};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct LoadedLocalModels {
    pub models: Vec<Model>,
}

#[derive(Debug, Deserialize)]
struct ModelsFile {
    #[serde(default)]
    models: Vec<Model>,
}

pub async fn load_all(cwd: &Path, cli_base_url: Option<&str>) -> Result<LoadedLocalModels> {
    let paths = [
        crate::config::base_dir().join("models.json"),
        cwd.join(".pie").join("models.json"),
    ];
    load_all_from_paths_with_base_url(&paths, cli_base_url)
}

#[cfg(test)]
fn load_all_from_paths(paths: &[PathBuf]) -> Result<LoadedLocalModels> {
    load_all_from_paths_with_base_url(paths, None)
}

pub fn load_all_from_paths_with_base_url(
    paths: &[PathBuf],
    cli_base_url: Option<&str>,
) -> Result<LoadedLocalModels> {
    let mut models = Vec::<Model>::new();
    register_builtin_local_defaults(cli_base_url);
    for path in paths {
        if !path.exists() {
            continue;
        }
        let file = load_file(path)?;
        for model in file.models {
            if let Some(existing) = models
                .iter()
                .position(|m| m.provider == model.provider && m.id == model.id)
            {
                models[existing] = model;
            } else {
                models.push(model);
            }
        }
    }
    for model in &models {
        pie_ai::register_custom_model(model.clone());
    }
    Ok(LoadedLocalModels { models })
}

fn register_builtin_local_defaults(cli_base_url: Option<&str>) {
    // DS4 is a local OpenAI-compatible server, so its base URL is user/environment specific.
    // Register the conventional provider/model only when the URL is explicit; user/project
    // `models.json` entries with the same provider/id are loaded afterwards and override it.
    if let Some(base_url) = ds4_base_url(cli_base_url) {
        pie_ai::register_custom_model(ds4_model(base_url));
    }
}

fn ds4_base_url(cli_base_url: Option<&str>) -> Option<String> {
    cli_base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(ds4_base_url_from_env)
}

fn ds4_base_url_from_env() -> Option<String> {
    ["DS4_BASE_URL", "DS4_URL"].into_iter().find_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn ds4_model(base_url: String) -> Model {
    let thinking_level_map = [
        (ModelThinkingLevel::Off, None),
        (ModelThinkingLevel::Minimal, Some("low".into())),
        (ModelThinkingLevel::Low, Some("low".into())),
        (ModelThinkingLevel::Medium, Some("medium".into())),
        (ModelThinkingLevel::High, Some("high".into())),
        (ModelThinkingLevel::Xhigh, Some("xhigh".into())),
    ]
    .into_iter()
    .collect::<ThinkingLevelMap>();
    Model {
        id: "deepseek-v4-flash".into(),
        name: "DeepSeek V4 Flash (local DS4)".into(),
        api: Api::from("openai-responses"),
        provider: Provider::from("ds4"),
        base_url,
        reasoning: true,
        thinking_level_map: Some(thinking_level_map),
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 384_000,
        headers: None,
        compat: Some(serde_json::json!({
            "supportsStore": false,
            "supportsDeveloperRole": false,
            "supportsReasoningEffort": true,
            "supportsUsageInStreaming": true,
            "maxTokensField": "max_tokens",
            "supportsStrictMode": false,
            "thinkingFormat": "deepseek",
            "requiresReasoningContentOnAssistantMessages": true
        })),
    }
}

fn load_file(path: &Path) -> Result<ModelsFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file: ModelsFile =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use pie_ai::{
        AssistantMessageEvent, Context as AiContext, DoneReason, Message, Tool, UserContent,
        UserMessage, UserRole,
    };
    use std::sync::OnceLock;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{Mutex as TokioMutex, oneshot};

    fn env_lock() -> &'static TokioMutex<()> {
        static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| TokioMutex::new(()))
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, old }
        }

        fn remove(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(old) = &self.old {
                unsafe { std::env::set_var(self.key, old) };
            } else {
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    fn unregister_ds4_default() {
        pie_ai::unregister_custom_model(&pie_ai::Provider::from("ds4"), "deepseek-v4-flash");
    }

    fn model_json(provider: &str, id: &str, api: &str, base_url: &str) -> String {
        format!(
            r#"{{
  "models": [
    {{
      "id": "{id}",
      "name": "Local {id}",
      "api": "{api}",
      "provider": "{provider}",
      "baseUrl": "{base_url}",
      "reasoning": true,
      "thinkingLevelMap": {{
        "off": null,
        "minimal": "low",
        "low": "low",
        "medium": "medium",
        "high": "high",
        "xhigh": "xhigh"
      }},
      "input": ["text"],
      "cost": {{ "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }},
      "contextWindow": 100000,
      "maxTokens": 384000,
      "compat": {{
        "supportsStore": false,
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "supportsUsageInStreaming": true,
        "maxTokensField": "max_tokens",
        "supportsStrictMode": false,
        "thinkingFormat": "deepseek",
        "requiresReasoningContentOnAssistantMessages": true
      }}
    }}
  ]
}}"#
        )
    }

    #[tokio::test]
    async fn registers_ds4_model_from_explicit_env_url_and_allows_user_override() {
        let _lock = env_lock().lock().await;
        let _base_url = EnvGuard::set("DS4_BASE_URL", "http://127.0.0.1:8000/v1");
        let _legacy_url = EnvGuard::remove("DS4_URL");
        unregister_ds4_default();
        load_all_from_paths(&[]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from("ds4"), "deepseek-v4-flash")
            .expect("ds4 default model registered");
        assert_eq!(model.api.0, "openai-responses");
        assert_eq!(model.base_url, "http://127.0.0.1:8000/v1");
        assert_eq!(model.max_tokens, 384_000);

        let resolved =
            crate::model::auto_detect_model(Some("ds4"), Some("deepseek-v4-flash")).unwrap();
        assert_eq!(resolved.provider, pie_ai::Provider::from("ds4"));
        assert_eq!(resolved.id, "deepseek-v4-flash");

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(
                "ds4",
                "deepseek-v4-flash",
                "openai-responses",
                "http://127.0.0.1:7777/v1",
            ),
        )
        .unwrap();

        load_all_from_paths(&[path]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from("ds4"), "deepseek-v4-flash")
            .expect("ds4 model registered");
        assert_eq!(model.base_url, "http://127.0.0.1:7777/v1");

        unregister_ds4_default();
    }

    #[tokio::test]
    async fn ds4_url_env_alias_registers_model() {
        let _lock = env_lock().lock().await;
        let _base_url = EnvGuard::remove("DS4_BASE_URL");
        let _legacy_url = EnvGuard::set("DS4_URL", "http://127.0.0.1:8123/v1");
        unregister_ds4_default();

        load_all_from_paths(&[]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from("ds4"), "deepseek-v4-flash")
            .expect("ds4 model registered");
        assert_eq!(model.base_url, "http://127.0.0.1:8123/v1");

        unregister_ds4_default();
    }

    #[tokio::test]
    async fn cli_base_url_registers_ds4_model_and_overrides_env_url() {
        let _lock = env_lock().lock().await;
        let _base_url = EnvGuard::set("DS4_BASE_URL", "http://127.0.0.1:8000/v1");
        let _legacy_url = EnvGuard::remove("DS4_URL");
        unregister_ds4_default();

        load_all_from_paths_with_base_url(&[], Some("http://127.0.0.1:9999/v1")).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from("ds4"), "deepseek-v4-flash")
            .expect("ds4 model registered");
        assert_eq!(model.base_url, "http://127.0.0.1:9999/v1");

        unregister_ds4_default();
    }

    #[test]
    fn loads_and_registers_custom_model() {
        // load_all_from_paths registers the ds4 default from DS4_* env vars, so every test
        // that calls it must hold env_lock or it races the env-guarded ds4 tests.
        let _lock = env_lock().blocking_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(
                "local-test-register",
                "deepseek-v4-flash",
                "openai-responses",
                "http://127.0.0.1:9999/v1",
            ),
        )
        .unwrap();

        let loaded = load_all_from_paths(&[path]).unwrap();
        assert_eq!(loaded.models.len(), 1);
        let resolved = pie_ai::get_model(
            &pie_ai::Provider::from("local-test-register"),
            "deepseek-v4-flash",
        )
        .unwrap();
        assert_eq!(resolved.api.0, "openai-responses");
        pie_ai::unregister_custom_model(
            &pie_ai::Provider::from("local-test-register"),
            "deepseek-v4-flash",
        );
    }

    #[test]
    fn project_model_overrides_user_model_with_same_provider_and_id() {
        let _lock = env_lock().blocking_lock();
        let dir = TempDir::new().unwrap();
        let user = dir.path().join("user.json");
        let project = dir.path().join("project.json");
        std::fs::write(
            &user,
            model_json(
                "local-test-override",
                "same",
                "openai-completions",
                "http://127.0.0.1:1/v1",
            ),
        )
        .unwrap();
        std::fs::write(
            &project,
            model_json(
                "local-test-override",
                "same",
                "openai-responses",
                "http://127.0.0.1:2/v1",
            ),
        )
        .unwrap();

        let loaded = load_all_from_paths(&[user, project]).unwrap();
        assert_eq!(loaded.models.len(), 1);
        assert_eq!(loaded.models[0].api.0, "openai-responses");
        assert_eq!(loaded.models[0].base_url, "http://127.0.0.1:2/v1");
        pie_ai::unregister_custom_model(&pie_ai::Provider::from("local-test-override"), "same");
    }

    #[test]
    fn malformed_config_fails_closed_without_registering() {
        let _lock = env_lock().blocking_lock();
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r#"{ "models": [ { "provider": "broken" } ] }"#).unwrap();

        let err = load_all_from_paths(&[bad]).unwrap_err().to_string();
        assert!(err.contains("parse"));
        assert!(pie_ai::get_model(&pie_ai::Provider::from("broken"), "").is_none());
    }

    #[tokio::test]
    async fn loaded_openai_responses_model_streams_text_from_local_fixture() {
        let _lock = env_lock().lock().await;
        let body = r#"data: {"type":"response.created","response":{"id":"resp_test","model":"model","output":[]}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_test","type":"message","status":"in_progress","role":"assistant","content":[]}}

data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"OK"}

data: {"type":"response.output_text.done","output_index":0,"content_index":0,"text":"OK"}

data: {"type":"response.completed","response":{"id":"resp_test","status":"completed","model":"model","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"OK","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#;
        let base_url = serve_once(body).await;
        let provider = "local-test-text";
        let id = "text";
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(provider, id, "openai-responses", &base_url),
        )
        .unwrap();
        load_all_from_paths(&[path]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from(provider), id).unwrap();
        let mut stream = pie_ai::stream(
            &model,
            &context(None),
            Some(&pie_ai::StreamOptions {
                api_key: Some("local".into()),
                max_tokens: Some(8),
                ..Default::default()
            }),
        );
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AssistantMessageEvent::Done { .. } => break,
                AssistantMessageEvent::Error { error, .. } => {
                    panic!("provider error: {:?}", error.error_message);
                }
                _ => {}
            }
        }
        assert_eq!(text, "OK");
        pie_ai::unregister_custom_model(&pie_ai::Provider::from(provider), id);
    }

    #[tokio::test]
    async fn loaded_openai_responses_model_streams_tool_call_from_local_fixture() {
        let _lock = env_lock().lock().await;
        let body = r#"data: {"type":"response.created","response":{"id":"resp_test","model":"model","output":[]}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc_test","type":"function_call","call_id":"call_1","name":"get_weather","arguments":""}}

data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"city\":\"Paris\"}"}

data: {"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"city\":\"Paris\"}"}

data: {"type":"response.completed","response":{"id":"resp_test","status":"completed","model":"model","output":[{"id":"fc_test","type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Paris\"}"}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#;
        let base_url = serve_once(body).await;
        let provider = "local-test-tool";
        let id = "tool";
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(provider, id, "openai-responses", &base_url),
        )
        .unwrap();
        load_all_from_paths(&[path]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from(provider), id).unwrap();
        let mut stream = pie_ai::stream(
            &model,
            &context(Some(vec![Tool {
                name: "get_weather".into(),
                description: "Get weather".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"]
                }),
            }])),
            Some(&pie_ai::StreamOptions {
                api_key: Some("local".into()),
                max_tokens: Some(32),
                ..Default::default()
            }),
        );
        let mut tool_name = None;
        let mut done_reason = None;
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    tool_name = Some(tool_call.name);
                    assert_eq!(
                        tool_call.arguments.get("city").and_then(|v| v.as_str()),
                        Some("Paris")
                    );
                }
                AssistantMessageEvent::Done { reason, .. } => {
                    done_reason = Some(reason);
                    break;
                }
                AssistantMessageEvent::Error { error, .. } => {
                    panic!("provider error: {:?}", error.error_message);
                }
                _ => {}
            }
        }
        assert_eq!(tool_name.as_deref(), Some("get_weather"));
        assert_eq!(done_reason, Some(DoneReason::ToolUse));
        pie_ai::unregister_custom_model(&pie_ai::Provider::from(provider), id);
    }

    #[tokio::test]
    async fn ds4_responses_model_uses_ds4_env_not_openai_env() {
        let _lock = env_lock().lock().await;
        let _openai = EnvGuard::set("OPENAI_API_KEY", "real-openai-should-not-leak");
        let _ds4 = EnvGuard::set("DS4_API_KEY", "dsv4-local");
        let _base_url = EnvGuard::remove("DS4_BASE_URL");
        let _legacy_url = EnvGuard::remove("DS4_URL");
        let _pie_dir = TempDir::new().unwrap();
        let _pie_dir_env = EnvGuard::set("PIE_DIR", &_pie_dir.path().to_string_lossy());
        unregister_ds4_default();

        let body = r#"data: {"type":"response.created","response":{"id":"resp_test","model":"model","output":[]}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_test","type":"message","status":"in_progress","role":"assistant","content":[]}}

data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"OK"}

data: {"type":"response.completed","response":{"id":"resp_test","status":"completed","model":"model","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"OK","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#;
        let (base_url, request_rx) = serve_once_capture_request(body).await;
        let provider = "ds4";
        let id = "deepseek-v4-flash-env-fixture";
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(provider, id, "openai-responses", &base_url),
        )
        .unwrap();
        load_all_from_paths(&[path]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from(provider), id).unwrap();
        let mut stream = pie_ai::stream(&model, &context(None), None);
        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::Done { .. } => break,
                AssistantMessageEvent::Error { error, .. } => {
                    panic!("provider error: {:?}", error.error_message);
                }
                _ => {}
            }
        }
        let request = request_rx.await.unwrap();
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer dsv4-local"),
            "{request}"
        );
        assert!(!request.contains("real-openai-should-not-leak"));
        pie_ai::unregister_custom_model(&pie_ai::Provider::from(provider), id);
    }

    #[tokio::test]
    async fn ds4_env_without_url_reports_base_url_config() {
        let _lock = env_lock().lock().await;
        let _openai = EnvGuard::remove("OPENAI_API_KEY");
        let _anthropic = EnvGuard::remove("ANTHROPIC_API_KEY");
        let _ds4 = EnvGuard::set("DS4_API_KEY", "dsv4-local");
        let _base_url = EnvGuard::remove("DS4_BASE_URL");
        let _legacy_url = EnvGuard::remove("DS4_URL");
        let _pie_dir = TempDir::new().unwrap();
        let _pie_dir_env = EnvGuard::set("PIE_DIR", &_pie_dir.path().to_string_lossy());
        unregister_ds4_default();

        let err = crate::model::auto_detect_model(None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("provider=ds4"), "{err}");
        assert!(err.contains("--base-url"), "{err}");
        assert!(err.contains("DS4_BASE_URL"), "{err}");
        assert!(err.contains("models.json"), "{err}");

        let explicit_err = crate::model::auto_detect_model(Some("ds4"), Some("deepseek-v4-flash"))
            .unwrap_err()
            .to_string();
        assert!(explicit_err.contains("provider=ds4"), "{explicit_err}");
        assert!(explicit_err.contains("--base-url"), "{explicit_err}");
        assert!(explicit_err.contains("DS4_BASE_URL"), "{explicit_err}");
        assert!(explicit_err.contains("models.json"), "{explicit_err}");
    }

    #[tokio::test]
    async fn ds4_responses_model_fails_closed_without_ds4_env_even_when_openai_env_exists() {
        let _lock = env_lock().lock().await;
        let _openai = EnvGuard::set("OPENAI_API_KEY", "real-openai-should-not-leak");
        let _ds4 = EnvGuard::remove("DS4_API_KEY");
        let _base_url = EnvGuard::remove("DS4_BASE_URL");
        let _legacy_url = EnvGuard::remove("DS4_URL");
        unregister_ds4_default();

        let provider = "ds4";
        let id = "deepseek-v4-flash-missing-key";
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            model_json(provider, id, "openai-responses", "http://127.0.0.1:9/v1"),
        )
        .unwrap();
        load_all_from_paths(&[path]).unwrap();

        let model = pie_ai::get_model(&pie_ai::Provider::from(provider), id).unwrap();
        let mut stream = pie_ai::stream(&model, &context(None), None);
        let mut error = None;
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::Error { error: e, .. } = event {
                error = e.error_message;
                break;
            }
        }
        let error = error.expect("expected provider error");
        assert!(error.contains("DS4_API_KEY"), "{error}");
        assert!(!error.contains("real-openai-should-not-leak"));
        assert!(!error.contains("HTTP"), "{error}");
        pie_ai::unregister_custom_model(&pie_ai::Provider::from(provider), id);
    }

    fn context(tools: Option<Vec<Tool>>) -> AiContext {
        AiContext {
            system_prompt: Some("You are terse.".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("Use the tool or reply OK.".into()),
                timestamp: 0,
            })],
            tools,
        }
    }

    async fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let _ = socket.read(&mut buf).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{addr}/v1")
    }

    async fn serve_once_capture_request(body: &'static str) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{addr}/v1"), rx)
    }
}
