//! Google Gemini provider (`google-generative-ai`). Partial 1:1 port of
//! `packages/ai/src/providers/google.ts` (~500 LOC).
//!
//! Implemented: request shape, SSE chunk handling (text/thought parts, functionCall,
//! usageMetadata, finishReason), thinking budget, tool-call id generation. The SSE consumer and
//! request builder are `pub(crate)` so `google_vertex` can reuse them.
//!
//! TODO: Gemini 3 / Gemma thinking-level selection, multimodal functionResponse parts,
//! thoughtSignature replay correctness, tool-choice config.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use crate::api_registry::ApiProvider;
use crate::providers::google_shared::{
    convert_messages, convert_tools, is_thinking_part, map_stop_reason,
};
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest, AbortableNext};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::sse::SseStream;

const GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com";

#[derive(Clone, Debug, Default, Serialize)]
pub struct GoogleOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<i32>,
}

#[derive(Default)]
pub struct GoogleProvider {}

#[async_trait]
impl ApiProvider for GoogleProvider {
    fn api(&self) -> &str {
        KnownApi::GoogleGenerativeAi.as_str()
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
        self.stream(model, context, Some(&translate_simple(options)))
    }
}

/// Translate `SimpleStreamOptions` into a Gemini `StreamOptions` (thinking budget in extras).
pub(crate) fn translate_simple(options: Option<&SimpleStreamOptions>) -> StreamOptions {
    options
        .map(|o| {
            let mut base = o.base.clone();
            if let Some(level) = o.reasoning {
                let budget = o
                    .thinking_budgets
                    .as_ref()
                    .and_then(|b| match level {
                        ThinkingLevel::Minimal => b.minimal,
                        ThinkingLevel::Low => b.low,
                        ThinkingLevel::Medium => b.medium,
                        ThinkingLevel::High | ThinkingLevel::Xhigh => b.high,
                    })
                    .unwrap_or(8192);
                base.provider_extras
                    .insert("thinking_budget".to_string(), json!(budget));
            }
            base
        })
        .unwrap_or_default()
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    mut sender: AssistantMessageEventSender,
) {
    let api_key = match options
        .api_key
        .clone()
        .or_else(|| crate::env_api_keys::get_env_api_key("google"))
    {
        Some(k) => k,
        None => {
            push_error(
                &mut sender,
                &model,
                "GOOGLE_API_KEY / GEMINI_API_KEY is not set".into(),
            );
            return;
        }
    };

    let body = build_request_body(&context, &options);
    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };

    let base = if model.base_url.is_empty() {
        GOOGLE_BASE_URL
    } else {
        model.base_url.as_str()
    };
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        base.trim_end_matches('/'),
        model.id
    );
    let mut req = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
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
        push_error(&mut sender, &model, format!("HTTP {status}: {txt}"));
        return;
    }

    consume_gemini_sse(resp, &model, sender, options.abort.as_ref()).await;
}

/// Shared Gemini SSE consumer. Reused by `google_vertex`, which differs only in URL + auth.
pub(crate) async fn consume_gemini_sse(
    resp: reqwest::Response,
    model: &Model,
    mut sender: AssistantMessageEventSender,
    abort_token: Option<&tokio_util::sync::CancellationToken>,
) {
    let mut partial = empty_partial(model);
    sender.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    // Track open text/thinking block kind: 0 = none, 1 = text, 2 = thinking.
    let mut open: u8 = 0;
    let mut tool_counter: u64 = 0;
    let mut sse = SseStream::new(resp.bytes_stream());
    loop {
        if sender.is_closed() {
            return;
        }
        let item = match abort_utils::next_or_abort(&mut sse, abort_token).await {
            AbortableNext::Item(item) => item,
            AbortableNext::Eof => break,
            AbortableNext::Aborted => {
                abort_utils::push_aborted(&mut sender, model);
                return;
            }
        };
        let ev = match item {
            Ok(e) => e,
            Err(e) => {
                push_error(&mut sender, model, format!("sse: {e}"));
                return;
            }
        };
        let Ok(chunk): Result<Value, _> = serde_json::from_str(&ev.data) else {
            continue;
        };
        if partial.response_id.is_none() {
            if let Some(id) = chunk.get("responseId").and_then(|v| v.as_str()) {
                partial.response_id = Some(id.to_string());
            }
        }

        if let Some(parts) = chunk
            .pointer("/candidates/0/content/parts")
            .and_then(|v| v.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    let is_thinking = is_thinking_part(part);
                    let want = if is_thinking { 2 } else { 1 };
                    if open != want {
                        close_open_block(open, &mut partial, &mut sender);
                        if is_thinking {
                            partial
                                .content
                                .push(ContentBlock::Thinking(ThinkingContent::default()));
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index: partial.content.len() - 1,
                                partial: partial.clone(),
                            });
                        } else {
                            partial.content.push(ContentBlock::text(""));
                            sender.push(AssistantMessageEvent::TextStart {
                                content_index: partial.content.len() - 1,
                                partial: partial.clone(),
                            });
                        }
                        open = want;
                    }
                    let idx = partial.content.len() - 1;
                    if is_thinking {
                        if let Some(ContentBlock::Thinking(tc)) = partial.content.get_mut(idx) {
                            tc.thinking.push_str(text);
                        }
                        sender.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: idx,
                            delta: text.to_string(),
                            partial: partial.clone(),
                        });
                    } else {
                        if let Some(ContentBlock::Text(tc)) = partial.content.get_mut(idx) {
                            tc.text.push_str(text);
                        }
                        sender.push(AssistantMessageEvent::TextDelta {
                            content_index: idx,
                            delta: text.to_string(),
                            partial: partial.clone(),
                        });
                    }
                }

                if let Some(fc) = part.get("functionCall") {
                    close_open_block(open, &mut partial, &mut sender);
                    open = 0;
                    let name = fc
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let provided = fc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let id = if provided.is_empty() {
                        tool_counter += 1;
                        format!(
                            "{name}_{}_{tool_counter}",
                            chrono::Utc::now().timestamp_millis()
                        )
                    } else {
                        provided.to_string()
                    };
                    let args = fc
                        .get("args")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let tool_call = ToolCall {
                        id,
                        name,
                        arguments: args,
                        thought_signature: part
                            .get("thoughtSignature")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    };
                    partial
                        .content
                        .push(ContentBlock::ToolCall(tool_call.clone()));
                    let idx = partial.content.len() - 1;
                    sender.push(AssistantMessageEvent::ToolCallStart {
                        content_index: idx,
                        partial: partial.clone(),
                    });
                    sender.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: idx,
                        delta: serde_json::to_string(&tool_call.arguments).unwrap_or_default(),
                        partial: partial.clone(),
                    });
                    sender.push(AssistantMessageEvent::ToolCallEnd {
                        content_index: idx,
                        tool_call,
                        partial: partial.clone(),
                    });
                }
            }
        }

        if let Some(reason) = chunk
            .pointer("/candidates/0/finishReason")
            .and_then(|v| v.as_str())
        {
            partial.stop_reason = map_stop_reason(reason);
            if partial
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall(_)))
            {
                partial.stop_reason = StopReason::ToolUse;
            }
        }

        if let Some(u) = chunk.get("usageMetadata") {
            update_usage(&mut partial.usage, u);
        }
    }

    close_open_block(open, &mut partial, &mut sender);

    let reason = match partial.stop_reason {
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Length => DoneReason::Length,
        StopReason::Error => {
            partial
                .error_message
                .get_or_insert_with(|| "google error".into());
            sender.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: partial,
            });
            return;
        }
        _ => DoneReason::Stop,
    };
    sender.push(AssistantMessageEvent::Done {
        reason,
        message: partial,
    });
}

fn close_open_block(
    open: u8,
    partial: &mut AssistantMessage,
    sender: &mut AssistantMessageEventSender,
) {
    if open == 0 {
        return;
    }
    let idx = partial.content.len() - 1;
    match partial.content.get(idx).cloned() {
        Some(ContentBlock::Text(tc)) if open == 1 => {
            sender.push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content: tc.text,
                partial: partial.clone(),
            });
        }
        Some(ContentBlock::Thinking(tc)) if open == 2 => {
            sender.push(AssistantMessageEvent::ThinkingEnd {
                content_index: idx,
                content: tc.thinking,
                partial: partial.clone(),
            });
        }
        _ => {}
    }
}

fn update_usage(usage: &mut Usage, u: &Value) {
    let prompt = u
        .get("promptTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = u
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let candidates = u
        .get("candidatesTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let thoughts = u
        .get("thoughtsTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    usage.input = prompt.saturating_sub(cached);
    usage.output = candidates + thoughts;
    usage.cache_read = cached;
    usage.total_tokens = u
        .get("totalTokenCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(usage.input + usage.output + usage.cache_read);
}

pub(crate) fn build_request_body(context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "contents": convert_messages(&context.messages),
    });
    if let Some(sys) = &context.system_prompt {
        body["systemInstruction"] = json!({ "parts": [{ "text": sys }] });
    }
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            body["tools"] = json!(convert_tools(tools));
        }
    }

    let mut gen_config = serde_json::Map::new();
    if let Some(max) = options.max_tokens {
        gen_config.insert("maxOutputTokens".into(), json!(max));
    }
    if let Some(t) = options.temperature {
        gen_config.insert("temperature".into(), json!(t));
    }
    if let Some(budget) = options.provider_extras.get("thinking_budget") {
        gen_config.insert(
            "thinkingConfig".into(),
            json!({ "thinkingBudget": budget, "includeThoughts": true }),
        );
    }
    if !gen_config.is_empty() {
        body["generationConfig"] = Value::Object(gen_config);
    }
    body
}

pub(crate) fn empty_partial(model: &Model) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: chrono::Utc::now().timestamp_millis(),
    }
}

pub(crate) fn push_error(sender: &mut AssistantMessageEventSender, model: &Model, msg: String) {
    let mut p = empty_partial(model);
    p.stop_reason = StopReason::Error;
    p.error_message = Some(msg);
    sender.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error: p,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_has_contents_and_system_instruction() {
        let ctx = Context {
            system_prompt: Some("be brief".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let body = build_request_body(&ctx, &Default::default());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
    }

    #[test]
    fn thinking_budget_sets_generation_config() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let mut opts = StreamOptions::default();
        opts.provider_extras
            .insert("thinking_budget".into(), json!(4096));
        let body = build_request_body(&ctx, &opts);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn tools_become_function_declarations() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![],
            tools: Some(vec![Tool {
                name: "lookup".into(),
                description: "look".into(),
                parameters: json!({ "type": "object" }),
            }]),
        };
        let body = build_request_body(&ctx, &Default::default());
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["name"],
            "lookup"
        );
    }
}
