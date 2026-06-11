//! OpenAI Codex (ChatGPT subscription) provider (`openai-codex-responses`). Partial 1:1 port of
//! `packages/ai/src/providers/openai-codex-responses.ts` (~1370 LOC).
//!
//! Codex speaks the Responses wire protocol against the ChatGPT backend, so it reuses
//! `openai_responses::{consume_responses_sse, convert_messages, serialize_tools}`. Codex-specific:
//! - endpoint: `{base}/codex/responses` (default base `https://chatgpt.com/backend-api`)
//! - auth: OAuth Bearer access token (ChatGPT login), plus `chatgpt-account-id`, `originator: pi`,
//!   `OpenAI-Beta: responses=experimental`, optional `session_id`
//! - body: `instructions` (not a system message), `store:false`, `include:[reasoning.encrypted_content]`,
//!   `tool_choice:auto`, `parallel_tool_calls:true`
//!
//! TODO:
//! - WebSocket cache-affinity transport (`websocket-cached`) — the primary perf path
//! - decode `chatgpt-account-id` from the JWT access token (currently from env/extras)
//! - text verbosity / service tier knobs

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::api_registry::ApiProvider;
use crate::providers::openai_responses::{
    consume_responses_sse, convert_messages, push_error, serialize_tools,
};
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";

#[derive(Default)]
pub struct OpenAICodexResponsesProvider {}

#[async_trait]
impl ApiProvider for OpenAICodexResponsesProvider {
    fn api(&self) -> &str {
        KnownApi::OpenAICodexResponses.as_str()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let (stream, sender) = AssistantMessageEventStream::new();
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned().unwrap_or_default();
        tokio::spawn(async move {
            run(model, context, options, sender).await;
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let translated = options
            .map(|o| {
                let mut base = o.base.clone();
                if let Some(level) = o.reasoning {
                    let effort = match level {
                        ThinkingLevel::Minimal => "minimal",
                        ThinkingLevel::Low => "low",
                        ThinkingLevel::Medium => "medium",
                        ThinkingLevel::High | ThinkingLevel::Xhigh => "high",
                    };
                    base.provider_extras
                        .insert("reasoning_effort".to_string(), json!(effort));
                }
                base
            })
            .unwrap_or_default();
        self.stream(model, context, Some(&translated))
    }
}

fn resolve_codex_url(base_url: &str) -> String {
    let raw = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL
    } else {
        base_url
    };
    let normalized = raw.trim_end_matches('/');
    if normalized.ends_with("/codex/responses") {
        normalized.to_string()
    } else if normalized.ends_with("/codex") {
        format!("{normalized}/responses")
    } else {
        format!("{normalized}/codex/responses")
    }
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    mut sender: AssistantMessageEventSender,
) {
    let token = match options
        .api_key
        .clone()
        .or_else(|| std::env::var("CODEX_AUTH_TOKEN").ok())
        .or_else(|| crate::env_api_keys::get_env_api_key("openai-codex"))
    {
        Some(t) if !t.is_empty() => t,
        _ => {
            push_error(
                &mut sender,
                &model,
                "Codex auth missing: set CODEX_AUTH_TOKEN or pass options.api_key (ChatGPT OAuth flow not yet implemented)".into(),
            );
            return;
        }
    };

    let account_id = options
        .provider_extras
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| std::env::var("CODEX_ACCOUNT_ID").ok());

    let body = build_request_body(&model, &context, &options);
    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };

    let url = resolve_codex_url(&model.base_url);
    let mut req = client
        .post(&url)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .header("originator", "pi")
        .header("OpenAI-Beta", "responses=experimental");
    if let Some(acct) = &account_id {
        req = req.header("chatgpt-account-id", acct.as_str());
    }
    if let Some(sid) = &options.session_id {
        req = req.header("session_id", sid.as_str());
    }
    if let Some(extra) = &options.headers {
        for (k, v) in extra {
            req = req.header(k.as_str(), v.as_str());
        }
    }

    let req = req.json(&body);
    let resp = match crate::utils::retry::send_with_retry(&options, req).await {
        Ok(r) => r,
        Err(e) => {
            if e.is_aborted() {
                abort_utils::push_aborted(&mut sender, &model);
            } else {
                push_error(&mut sender, &model, format!("http error: {e}"));
            }
            return;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = match abort_utils::response_text_or_abort(resp, options.abort.as_ref()).await {
            Ok(txt) => txt,
            Err(AbortErrorOrReqwest::Aborted) => {
                abort_utils::push_aborted(&mut sender, &model);
                return;
            }
            Err(AbortErrorOrReqwest::Reqwest(_)) => String::new(),
        };
        push_error(
            &mut sender,
            &model,
            format!("Codex API error ({status}): {txt}"),
        );
        return;
    }

    consume_responses_sse(resp, &model, &mut sender, options.abort.as_ref()).await;
}

fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    // Codex omits the system message and uses `instructions` instead.
    let messages = convert_messages(&context.messages, None, false);
    let mut body = json!({
        "model": model.id,
        "store": false,
        "stream": true,
        "instructions": context.system_prompt.clone().unwrap_or_else(|| "You are a helpful assistant.".to_string()),
        "input": messages,
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
    });
    if let Some(sid) = &options.session_id {
        body["prompt_cache_key"] = json!(sid);
    }
    if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            body["tools"] = json!(serialize_tools(tools));
        }
    }
    if let Some(effort) = options.provider_extras.get("reasoning_effort") {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_model() -> Model {
        Model {
            id: "gpt-5-codex".into(),
            name: "Codex".into(),
            api: Api::known(KnownApi::OpenAICodexResponses),
            provider: Provider::from("openai-codex"),
            base_url: String::new(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn url_resolution() {
        assert_eq!(
            resolve_codex_url(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://x/backend-api"),
            "https://x/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://x/codex"),
            "https://x/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://x/codex/responses"),
            "https://x/codex/responses"
        );
    }

    #[test]
    fn body_uses_instructions_not_system_message() {
        let m = mk_model();
        let ctx = Context {
            system_prompt: Some("be a coder".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let body = build_request_body(&m, &ctx, &Default::default());
        assert_eq!(body["instructions"], "be a coder");
        assert_eq!(body["store"], false);
        assert_eq!(body["tool_choice"], "auto");
        // input must NOT contain a system role item.
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|m| m["role"] != "system"));
    }
}
