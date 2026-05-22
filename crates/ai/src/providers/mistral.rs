//! Mistral provider (`mistral-conversations`). Partial 1:1 port of
//! `packages/ai/src/providers/mistral.ts` (~630 LOC).
//!
//! Mistral's streaming endpoint is OpenAI Chat-Completions-shaped (`/v1/chat/completions`,
//! `choices[0].delta`), so the chunk handling mirrors `openai_completions`. Mistral-specific:
//! - `x-affinity` header for KV-cache prefix reuse (from `session_id`)
//! - `reasoning_effort` / `prompt_mode: "reasoning"` for Magistral models
//! - tool-call ids must be alphanumeric, length 9
//!
//! TODO:
//! - Magistral prompt_mode reasoning vs reasoning_effort model detection
//! - structured reasoning-part assembly ([THINK]...[/THINK])
//! - tool-call id normalization map across handoffs (we only truncate here)

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::api_registry::ApiProvider;
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest, AbortableNext};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::hash::short_hash;
use crate::utils::sse::SseStream;

const MISTRAL_BASE_URL: &str = "https://api.mistral.ai";
const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;

#[derive(Default)]
pub struct MistralProvider {}

#[async_trait]
impl ApiProvider for MistralProvider {
    fn api(&self) -> &str {
        KnownApi::MistralConversations.as_str()
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
                if let (true, Some(level)) = (model.reasoning, o.reasoning) {
                    base.provider_extras.insert(
                        "reasoning_effort".to_string(),
                        json!(reasoning_effort(level)),
                    );
                }
                base
            })
            .unwrap_or_default();
        self.stream(model, context, Some(&translated))
    }
}

fn reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh => "high",
    }
}

/// Mistral requires tool-call ids to be alphanumeric and exactly 9 chars.
fn normalize_tool_call_id(id: &str) -> String {
    let normalized: String = id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    short_hash(seed)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
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
        .or_else(|| crate::env_api_keys::get_env_api_key("mistral"))
    {
        Some(k) => k,
        None => {
            push_error(&mut sender, &model, "MISTRAL_API_KEY is not set".into());
            return;
        }
    };

    let body = build_request_body(&model, &context, &options);
    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };
    let base = if model.base_url.is_empty() {
        MISTRAL_BASE_URL
    } else {
        model.base_url.as_str()
    };
    let url = format!("{}/v1/chat/completions", base.trim_end_matches('/'));
    let mut req = client
        .post(&url)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    if let Some(sid) = &options.session_id {
        req = req.header("x-affinity", sid.as_str());
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
            format!("Mistral API error ({status}): {txt}"),
        );
        return;
    }

    let mut partial = empty_partial(&model);
    sender.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    let mut text_index: Option<usize> = None;
    let mut tool_content_index: Option<usize> = None;
    let mut tool_args = String::new();
    let mut finish_reason: Option<String> = None;

    let mut sse = SseStream::new(resp.bytes_stream());
    loop {
        if sender.is_closed() {
            return;
        }
        let item = match abort_utils::next_or_abort(&mut sse, options.abort.as_ref()).await {
            AbortableNext::Item(item) => item,
            AbortableNext::Eof => break,
            AbortableNext::Aborted => {
                abort_utils::push_aborted(&mut sender, &model);
                return;
            }
        };
        let ev = match item {
            Ok(e) => e,
            Err(e) => {
                push_error(&mut sender, &model, format!("sse: {e}"));
                return;
            }
        };
        if ev.data.trim() == "[DONE]" {
            break;
        }
        let Ok(chunk): Result<Value, _> = serde_json::from_str(&ev.data) else {
            continue;
        };
        if partial.response_id.is_none() {
            if let Some(id) = chunk.get("id").and_then(|v| v.as_str()) {
                partial.response_id = Some(id.to_string());
            }
        }
        if let Some(u) = chunk.get("usage").filter(|v| !v.is_null()) {
            update_usage(&mut partial.usage, u);
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        else {
            continue;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            finish_reason = Some(fr.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
            if !content.is_empty() {
                let idx = *text_index.get_or_insert_with(|| {
                    let i = partial.content.len();
                    partial.content.push(ContentBlock::text(""));
                    i
                });
                let is_first = matches!(
                    partial.content.get(idx),
                    Some(ContentBlock::Text(tc)) if tc.text.is_empty()
                );
                if is_first {
                    sender.push(AssistantMessageEvent::TextStart {
                        content_index: idx,
                        partial: partial.clone(),
                    });
                }
                if let Some(ContentBlock::Text(tc)) = partial.content.get_mut(idx) {
                    tc.text.push_str(content);
                }
                sender.push(AssistantMessageEvent::TextDelta {
                    content_index: idx,
                    delta: content.to_string(),
                    partial: partial.clone(),
                });
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for tc in tcs {
                if tool_content_index.is_none() {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tc
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let i = partial.content.len();
                    partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id: normalize_tool_call_id(id),
                        name: name.to_string(),
                        arguments: Map::new(),
                        thought_signature: None,
                    }));
                    tool_content_index = Some(i);
                    sender.push(AssistantMessageEvent::ToolCallStart {
                        content_index: i,
                        partial: partial.clone(),
                    });
                }
                if let Some(args) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    if !args.is_empty() {
                        tool_args.push_str(args);
                        sender.push(AssistantMessageEvent::ToolCallDelta {
                            content_index: tool_content_index.unwrap(),
                            delta: args.to_string(),
                            partial: partial.clone(),
                        });
                    }
                }
            }
        }
    }

    if let Some(idx) = text_index {
        if let Some(ContentBlock::Text(tc)) = partial.content.get(idx).cloned() {
            sender.push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content: tc.text,
                partial: partial.clone(),
            });
        }
    }
    if let Some(idx) = tool_content_index {
        if let Ok(Value::Object(map)) = crate::utils::json_parse::parse_partial_json(&tool_args) {
            if let Some(ContentBlock::ToolCall(b)) = partial.content.get_mut(idx) {
                b.arguments = map;
            }
        }
        if let Some(ContentBlock::ToolCall(b)) = partial.content.get(idx).cloned() {
            sender.push(AssistantMessageEvent::ToolCallEnd {
                content_index: idx,
                tool_call: b,
                partial: partial.clone(),
            });
        }
    }

    let stop = match finish_reason.as_deref() {
        Some("stop") => StopReason::Stop,
        Some("length" | "model_length") => StopReason::Length,
        Some("tool_calls") => StopReason::ToolUse,
        _ => StopReason::Stop,
    };
    partial.stop_reason = stop;
    let reason = match stop {
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Length => DoneReason::Length,
        _ => DoneReason::Stop,
    };
    sender.push(AssistantMessageEvent::Done {
        reason,
        message: partial,
    });
}

fn update_usage(usage: &mut Usage, u: &Value) {
    if let Some(n) = u.get("prompt_tokens").and_then(|v| v.as_u64()) {
        usage.input = n;
    }
    if let Some(n) = u.get("completion_tokens").and_then(|v| v.as_u64()) {
        usage.output = n;
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let mut messages = Vec::new();
    if let Some(sys) = &context.system_prompt {
        messages.push(json!({ "role": "system", "content": sys }));
    }
    messages.extend(convert_messages(&context.messages));

    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
    });
    if let Some(max) = options.max_tokens {
        body["max_tokens"] = json!(max);
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
        body["reasoning_effort"] = effort.clone();
    }
    body
}

fn serialize_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                },
            })
        })
        .collect()
}

fn convert_messages(msgs: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(msgs.len());
    for m in msgs {
        match m {
            Message::User(u) => {
                let content = match &u.content {
                    UserContent::Text(s) => json!(s),
                    UserContent::Blocks(blocks) => {
                        let text: String = blocks
                            .iter()
                            .filter_map(|b| match b {
                                UserContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        json!(text)
                    }
                };
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(a) => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Text(t) => {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(&t.text);
                        }
                        ContentBlock::ToolCall(tc) => tool_calls.push(json!({
                            "id": normalize_tool_call_id(&tc.id),
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                            },
                        })),
                        _ => {}
                    }
                }
                let mut msg = json!({ "role": "assistant" });
                msg["content"] = if text.is_empty() {
                    json!("")
                } else {
                    json!(text)
                };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Message::ToolResult(tr) => {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        UserContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": normalize_tool_call_id(&tr.tool_call_id),
                    "name": tr.tool_name,
                    "content": text,
                }));
            }
        }
    }
    out
}

fn empty_partial(model: &Model) -> AssistantMessage {
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

fn push_error(sender: &mut AssistantMessageEventSender, model: &Model, msg: String) {
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
    fn tool_call_id_normalizes_to_len_9() {
        let id = normalize_tool_call_id("call_abc-123-xyz");
        assert_eq!(id.len(), MISTRAL_TOOL_CALL_ID_LENGTH);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn nine_char_alnum_passes_through() {
        assert_eq!(normalize_tool_call_id("abcdef123"), "abcdef123");
    }

    #[test]
    fn body_has_affinity_independent_messages() {
        let m = Model {
            id: "mistral-large".into(),
            name: "Mistral Large".into(),
            api: Api::known(KnownApi::MistralConversations),
            provider: Provider::from("mistral"),
            base_url: "https://api.mistral.ai".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 8192,
            headers: None,
            compat: None,
        };
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let body = build_request_body(&m, &ctx, &Default::default());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["stream"], true);
    }
}
