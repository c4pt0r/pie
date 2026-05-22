//! Local/custom model definitions loaded by the CLI before model resolution.
//!
//! This is intentionally a `coding-agent` concern: `pie-ai` already has the in-process custom
//! registry, while the CLI owns user/project configuration and user-visible diagnostics.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pie_ai::Model;
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

pub async fn load_all(cwd: &Path) -> Result<LoadedLocalModels> {
    let paths = [
        crate::config::base_dir().join("models.json"),
        cwd.join(".pie").join("models.json"),
    ];
    load_all_from_paths(&paths)
}

pub fn load_all_from_paths(paths: &[PathBuf]) -> Result<LoadedLocalModels> {
    let mut models = Vec::<Model>::new();
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

    #[test]
    fn loads_and_registers_custom_model() {
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
        let dir = TempDir::new().unwrap();
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, r#"{ "models": [ { "provider": "broken" } ] }"#).unwrap();

        let err = load_all_from_paths(&[bad]).unwrap_err().to_string();
        assert!(err.contains("parse"));
        assert!(pie_ai::get_model(&pie_ai::Provider::from("broken"), "").is_none());
    }

    #[tokio::test]
    async fn loaded_openai_responses_model_streams_text_from_local_fixture() {
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

        let body = r#"data: {"type":"response.created","response":{"id":"resp_test","model":"model","output":[]}}

data: {"type":"response.output_item.added","output_index":0,"item":{"id":"msg_test","type":"message","status":"in_progress","role":"assistant","content":[]}}

data: {"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"OK"}

data: {"type":"response.completed","response":{"id":"resp_test","status":"completed","model":"model","output":[{"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"OK","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}

"#;
        let (base_url, request_rx) = serve_once_capture_request(body).await;
        let provider = "ds4";
        let id = "deepseek-v4-flash";
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
    async fn ds4_responses_model_fails_closed_without_ds4_env_even_when_openai_env_exists() {
        let _lock = env_lock().lock().await;
        let _openai = EnvGuard::set("OPENAI_API_KEY", "real-openai-should-not-leak");
        let _ds4 = EnvGuard::remove("DS4_API_KEY");

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
