//! Anthropic Messages provider. Partial 1:1 port of
//! `packages/ai/src/providers/anthropic.ts` (~1.2k lines).
//!
//! Implemented:
//! - Provider trait + registration
//! - HTTP request shape (POST /v1/messages, streaming)
//! - SSE → AssistantMessageEvent translation (text / thinking / tool_use)
//! - `cache_control` markers on system, last user content, and last tool def
//! - Long-cache retention TTL (`ttl: "1h"`) via `cache_retention: long`
//! - Budget-based thinking from `provider_extras.thinking` or `SimpleStreamOptions.reasoning`
//! - Compat overrides (Fireworks etc. disable eager-stream + cache_control on tools)
//! - Tool name preservation (no Claude-Code "stealth" rename — that lives in the harness)
//!
//! TODO:
//! - Adaptive thinking (Opus 4.6+/Sonnet 4.6) with `effort` knob
//! - Interleaved-thinking beta toggle
//! - OAuth bearer-token auth path
//! - `redacted_thinking` content blocks
//! - Tool-choice (`auto` / `any` / `none` / specific tool)
//! - Fine-grained tool-streaming beta header negotiation

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use crate::api_registry::ApiProvider;
use crate::types::*;
use crate::utils::abort::{self, AbortErrorOrReqwest, AbortableNext};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};
use crate::utils::sse::SseStream;

/// Default Anthropic API host. Used as the fallback when `Model::base_url` is empty.
#[allow(dead_code)]
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Beta flags we always send. Cache + interleaved-thinking + fine-grained-tool-streaming.
const ANTHROPIC_BETAS: &[&str] = &[
    "prompt-caching-2024-07-31",
    "interleaved-thinking-2025-05-14",
    "fine-grained-tool-streaming-2025-05-14",
];

/// Anthropic-compat overrides. Mirrors TS `getAnthropicCompat`.
#[derive(Clone, Debug)]
struct Compat {
    supports_long_cache_retention: bool,
    send_session_affinity_headers: bool,
    supports_cache_control_on_tools: bool,
    #[allow(dead_code)]
    supports_eager_tool_input_streaming: bool,
}

fn resolve_compat(model: &Model) -> Compat {
    let provider = model.provider.0.as_str();
    let base_url = model.base_url.as_str();
    let is_fireworks = provider == "fireworks";
    let is_cf_anthropic = provider == "cloudflare-ai-gateway" && base_url.contains("anthropic");

    // model.compat is an opaque JSON value carrying the AnthropicMessagesCompat shape.
    let compat = model.compat.as_ref();
    let read_bool = |key: &str, default: bool| -> bool {
        compat
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_bool())
            .unwrap_or(default)
    };

    Compat {
        supports_long_cache_retention: read_bool("supportsLongCacheRetention", !is_fireworks),
        send_session_affinity_headers: read_bool(
            "sendSessionAffinityHeaders",
            is_fireworks || is_cf_anthropic,
        ),
        supports_cache_control_on_tools: read_bool("supportsCacheControlOnTools", !is_fireworks),
        supports_eager_tool_input_streaming: read_bool(
            "supportsEagerToolInputStreaming",
            !is_fireworks,
        ),
    }
}

/// Anthropic-specific opts mirror `AnthropicOptions` on the TS side. Threaded through
/// `provider_extras` on the universal path.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AnthropicOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AnthropicThinking {
    /// Per Anthropic API: `"enabled"` or `"disabled"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Default)]
pub struct AnthropicProvider {}

#[async_trait]
impl ApiProvider for AnthropicProvider {
    fn api(&self) -> &str {
        KnownApi::AnthropicMessages.as_str()
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
                    let budget = o
                        .thinking_budgets
                        .as_ref()
                        .and_then(|b| budget_for(b, level))
                        .unwrap_or(default_budget_for(level));
                    base.provider_extras.insert(
                        "thinking".to_string(),
                        json!({ "type": "enabled", "budget_tokens": budget }),
                    );
                }
                base
            })
            .unwrap_or_default();
        self.stream(model, context, Some(&translated))
    }
}

fn budget_for(b: &ThinkingBudgets, level: ThinkingLevel) -> Option<u32> {
    match level {
        ThinkingLevel::Minimal => b.minimal,
        ThinkingLevel::Low => b.low,
        ThinkingLevel::Medium => b.medium,
        ThinkingLevel::High | ThinkingLevel::Xhigh => b.high,
    }
}

fn default_budget_for(level: ThinkingLevel) -> u32 {
    match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 4096,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High => 16_384,
        ThinkingLevel::Xhigh => 32_768,
    }
}

// ────────────────────────────────────────────────────────────────────────────────────────────
// HTTP + SSE pipeline
// ────────────────────────────────────────────────────────────────────────────────────────────

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    mut sender: AssistantMessageEventSender,
) {
    let api_key = match options
        .api_key
        .clone()
        .or_else(|| crate::env_api_keys::get_env_api_key("anthropic"))
    {
        Some(k) => k,
        None => {
            push_error(&mut sender, &model, "ANTHROPIC_API_KEY is not set".into());
            return;
        }
    };

    let compat = resolve_compat(&model);
    let body = match build_request_body(&model, &context, &options, &compat) {
        Ok(b) => b,
        Err(e) => {
            push_error(&mut sender, &model, format!("build request body: {e}"));
            return;
        }
    };

    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };

    let base = if model.base_url.is_empty() {
        ANTHROPIC_BASE_URL
    } else {
        model.base_url.as_str()
    };
    let url = format!("{}/v1/messages", base.trim_end_matches('/'));
    let mut req = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_API_VERSION)
        .header("anthropic-beta", ANTHROPIC_BETAS.join(","))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");

    if compat.send_session_affinity_headers {
        if let Some(sid) = &options.session_id {
            req = req.header("x-session-affinity", sid.as_str());
        }
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
                abort::push_aborted(&mut sender, &model);
            } else {
                push_error(&mut sender, &model, format!("http error: {e}"));
            }
            return;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let txt = match abort::response_text_or_abort(resp, options.abort.as_ref()).await {
            Ok(txt) => txt,
            Err(AbortErrorOrReqwest::Aborted) => {
                abort::push_aborted(&mut sender, &model);
                return;
            }
            Err(AbortErrorOrReqwest::Reqwest(_)) => String::new(),
        };
        push_error(&mut sender, &model, format!("HTTP {status}: {txt}"));
        return;
    }

    let mut partial = empty_partial(&model);
    let mut tool_arg_buffers: HashMap<usize, String> = HashMap::new();
    sender.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    let mut sse = SseStream::new(resp.bytes_stream());
    loop {
        if sender.is_closed() {
            return; // consumer dropped — abort silently.
        }
        let item = match abort::next_or_abort(&mut sse, options.abort.as_ref()).await {
            AbortableNext::Item(item) => item,
            AbortableNext::Eof => break,
            AbortableNext::Aborted => {
                abort::push_aborted(&mut sender, &model);
                return;
            }
        };
        match item {
            Err(e) => {
                push_error(&mut sender, &model, format!("sse: {e}"));
                return;
            }
            Ok(ev) => {
                if !handle_sse(&ev, &mut partial, &mut tool_arg_buffers, &mut sender) {
                    return;
                }
            }
        }
    }

    partial.stop_reason = StopReason::Stop;
    sender.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: partial,
    });
}

// ────────────────────────────────────────────────────────────────────────────────────────────
// SSE → event translation
// ────────────────────────────────────────────────────────────────────────────────────────────

/// Returns `false` after pushing a terminal event so the caller stops draining the stream.
fn handle_sse(
    ev: &crate::utils::sse::SseEvent,
    partial: &mut AssistantMessage,
    tool_arg_buffers: &mut HashMap<usize, String>,
    sender: &mut AssistantMessageEventSender,
) -> bool {
    let Ok(payload): Result<Value, _> = serde_json::from_str(&ev.data) else {
        return true;
    };
    let kind = ev
        .event
        .as_deref()
        .unwrap_or_else(|| payload.get("type").and_then(|v| v.as_str()).unwrap_or(""));
    match kind {
        "message_start" => {
            if let Some(u) = payload.pointer("/message/usage") {
                update_usage(&mut partial.usage, u);
            }
            if let Some(id) = payload.pointer("/message/id").and_then(|v| v.as_str()) {
                partial.response_id = Some(id.to_string());
            }
        }
        "content_block_start" => on_content_block_start(&payload, partial, sender),
        "content_block_delta" => {
            on_content_block_delta(&payload, partial, tool_arg_buffers, sender)
        }
        "content_block_stop" => on_content_block_stop(&payload, partial, tool_arg_buffers, sender),
        "message_delta" => {
            if let Some(reason) = payload
                .pointer("/delta/stop_reason")
                .and_then(|v| v.as_str())
            {
                partial.stop_reason = map_stop_reason(reason);
            }
            if let Some(u) = payload.get("usage") {
                update_usage(&mut partial.usage, u);
            }
        }
        "message_stop" => {
            let reason = match partial.stop_reason {
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Length => DoneReason::Length,
                _ => DoneReason::Stop,
            };
            sender.push(AssistantMessageEvent::Done {
                reason,
                message: partial.clone(),
            });
            return false;
        }
        "error" => {
            let msg = payload
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("anthropic error")
                .to_string();
            partial.stop_reason = StopReason::Error;
            partial.error_message = Some(msg);
            sender.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: partial.clone(),
            });
            return false;
        }
        _ => {}
    }
    true
}

fn on_content_block_start(
    payload: &Value,
    partial: &mut AssistantMessage,
    sender: &mut AssistantMessageEventSender,
) {
    let block = &payload["content_block"];
    let idx = payload["index"].as_u64().unwrap_or(0) as usize;
    match block["type"].as_str().unwrap_or("") {
        "text" => {
            ensure_block(partial, idx, ContentBlock::text(""));
            sender.push(AssistantMessageEvent::TextStart {
                content_index: idx,
                partial: partial.clone(),
            });
        }
        "thinking" => {
            ensure_block(
                partial,
                idx,
                ContentBlock::Thinking(ThinkingContent::default()),
            );
            sender.push(AssistantMessageEvent::ThinkingStart {
                content_index: idx,
                partial: partial.clone(),
            });
        }
        "redacted_thinking" => {
            let data = block["data"].as_str().unwrap_or("").to_string();
            ensure_block(
                partial,
                idx,
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "[Reasoning redacted]".into(),
                    thinking_signature: Some(data),
                    redacted: true,
                }),
            );
            sender.push(AssistantMessageEvent::ThinkingStart {
                content_index: idx,
                partial: partial.clone(),
            });
        }
        "tool_use" => {
            let id = block["id"].as_str().unwrap_or("").to_string();
            let name = block["name"].as_str().unwrap_or("").to_string();
            ensure_block(
                partial,
                idx,
                ContentBlock::ToolCall(ToolCall {
                    id,
                    name,
                    arguments: Map::new(),
                    thought_signature: None,
                }),
            );
            sender.push(AssistantMessageEvent::ToolCallStart {
                content_index: idx,
                partial: partial.clone(),
            });
        }
        _ => {}
    }
}

fn on_content_block_delta(
    payload: &Value,
    partial: &mut AssistantMessage,
    tool_arg_buffers: &mut HashMap<usize, String>,
    sender: &mut AssistantMessageEventSender,
) {
    let idx = payload["index"].as_u64().unwrap_or(0) as usize;
    let delta = &payload["delta"];
    match delta["type"].as_str().unwrap_or("") {
        "text_delta" => {
            let t = delta["text"].as_str().unwrap_or("").to_string();
            if let Some(ContentBlock::Text(tc)) = partial.content.get_mut(idx) {
                tc.text.push_str(&t);
            }
            sender.push(AssistantMessageEvent::TextDelta {
                content_index: idx,
                delta: t,
                partial: partial.clone(),
            });
        }
        "thinking_delta" => {
            let t = delta["thinking"].as_str().unwrap_or("").to_string();
            if let Some(ContentBlock::Thinking(tc)) = partial.content.get_mut(idx) {
                tc.thinking.push_str(&t);
            }
            sender.push(AssistantMessageEvent::ThinkingDelta {
                content_index: idx,
                delta: t,
                partial: partial.clone(),
            });
        }
        "input_json_delta" => {
            let t = delta["partial_json"].as_str().unwrap_or("").to_string();
            tool_arg_buffers.entry(idx).or_default().push_str(&t);
            sender.push(AssistantMessageEvent::ToolCallDelta {
                content_index: idx,
                delta: t,
                partial: partial.clone(),
            });
        }
        "signature_delta" => {
            let sig = delta["signature"].as_str().unwrap_or("").to_string();
            if let Some(ContentBlock::Thinking(tc)) = partial.content.get_mut(idx) {
                let mut s = tc.thinking_signature.clone().unwrap_or_default();
                s.push_str(&sig);
                tc.thinking_signature = Some(s);
            }
        }
        _ => {}
    }
}

fn on_content_block_stop(
    payload: &Value,
    partial: &mut AssistantMessage,
    tool_arg_buffers: &mut HashMap<usize, String>,
    sender: &mut AssistantMessageEventSender,
) {
    let idx = payload["index"].as_u64().unwrap_or(0) as usize;
    // Tool-call arguments arrive as JSON fragments; assemble + parse on stop. Mirrors the TS
    // `partial-json` flow.
    if let Some(ContentBlock::ToolCall(tc)) = partial.content.get_mut(idx) {
        if let Some(assembled) = tool_arg_buffers.remove(&idx) {
            if !assembled.is_empty()
                && let Ok(Value::Object(map)) =
                    crate::utils::json_parse::parse_partial_json(&assembled)
            {
                tc.arguments = map;
            }
        }
    }
    let snapshot = partial.content.get(idx).cloned();
    match snapshot {
        Some(ContentBlock::Text(tc)) => {
            sender.push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content: tc.text,
                partial: partial.clone(),
            });
        }
        Some(ContentBlock::Thinking(tc)) => {
            sender.push(AssistantMessageEvent::ThinkingEnd {
                content_index: idx,
                content: tc.thinking,
                partial: partial.clone(),
            });
        }
        Some(ContentBlock::ToolCall(tc)) => {
            sender.push(AssistantMessageEvent::ToolCallEnd {
                content_index: idx,
                tool_call: tc,
                partial: partial.clone(),
            });
        }
        _ => {}
    }
}

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Stop,
        _ => StopReason::Stop,
    }
}

fn ensure_block(partial: &mut AssistantMessage, idx: usize, default: ContentBlock) {
    while partial.content.len() <= idx {
        partial.content.push(ContentBlock::text(""));
    }
    partial.content[idx] = default;
}

fn update_usage(usage: &mut Usage, val: &Value) {
    if let Some(n) = val.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.input += n;
    }
    if let Some(n) = val.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.output += n;
    }
    if let Some(n) = val.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_read += n;
    }
    if let Some(n) = val
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
    {
        usage.cache_write += n;
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

// ────────────────────────────────────────────────────────────────────────────────────────────
// Request body construction
// ────────────────────────────────────────────────────────────────────────────────────────────

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &Compat,
) -> Result<Value, String> {
    let retention = options.cache_retention.unwrap_or(CacheRetention::Short);
    let cache_control = build_cache_control(retention, compat);

    let messages = convert_messages(&context.messages, cache_control.as_ref());

    let mut body = json!({
        "model": model.id,
        "stream": true,
        "max_tokens": options.max_tokens.unwrap_or(model.max_tokens),
        "messages": messages,
    });
    if let Some(sys) = &context.system_prompt {
        let mut sys_block = json!({ "type": "text", "text": sys });
        if let Some(cc) = cache_control.as_ref() {
            sys_block["cache_control"] = cc.clone();
        }
        body["system"] = json!([sys_block]);
    }

    let thinking_enabled = options
        .provider_extras
        .get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str())
        == Some("enabled");

    if let Some(t) = options.temperature {
        // Anthropic rejects temperature when extended thinking is enabled.
        if !thinking_enabled {
            body["temperature"] = json!(t);
        }
    }

    if let Some(tools) = &context.tools {
        body["tools"] = json!(serialize_tools(tools, cache_control.as_ref(), compat));
    }
    if let Some(thinking) = options.provider_extras.get("thinking") {
        body["thinking"] = thinking.clone();
    }
    if let Some(meta) = &options.metadata {
        if let Some(user_id) = meta.get("user_id") {
            body["metadata"] = json!({ "user_id": user_id });
        }
    }
    Ok(body)
}

fn build_cache_control(retention: CacheRetention, compat: &Compat) -> Option<Value> {
    if matches!(retention, CacheRetention::None) {
        return None;
    }
    let mut cc = json!({ "type": "ephemeral" });
    if matches!(retention, CacheRetention::Long) && compat.supports_long_cache_retention {
        cc["ttl"] = json!("1h");
    }
    Some(cc)
}

fn serialize_tools(tools: &[Tool], cc: Option<&Value>, compat: &Compat) -> Vec<Value> {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut v = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            });
            if let Some(cc) = cc {
                if i == last && compat.supports_cache_control_on_tools {
                    v["cache_control"] = cc.clone();
                }
            }
            v
        })
        .collect()
}

fn convert_messages(msgs: &[Message], cc: Option<&Value>) -> Vec<Value> {
    let last_user_idx = msgs
        .iter()
        .rposition(|m| matches!(m, Message::User(_) | Message::ToolResult(_)));
    let mut out = Vec::with_capacity(msgs.len());
    for (i, m) in msgs.iter().enumerate() {
        let apply_cc = cc.filter(|_| Some(i) == last_user_idx);
        match m {
            Message::User(u) => {
                let content = user_content_to_value(&u.content, apply_cc);
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(a) => {
                let arr: Vec<Value> = a.content.iter().map(content_block_to_value).collect();
                out.push(json!({ "role": "assistant", "content": arr }));
            }
            Message::ToolResult(tr) => {
                let inner: Vec<Value> = tr.content.iter().map(user_block_to_value).collect();
                let mut result_block = json!({
                    "type": "tool_result",
                    "tool_use_id": tr.tool_call_id,
                    "is_error": tr.is_error,
                    "content": inner,
                });
                if let Some(cc) = apply_cc {
                    result_block["cache_control"] = cc.clone();
                }
                out.push(json!({
                    "role": "user",
                    "content": [result_block],
                }));
            }
        }
    }
    out
}

fn user_content_to_value(content: &UserContent, cc: Option<&Value>) -> Value {
    match content {
        UserContent::Text(s) => {
            let mut block = json!({ "type": "text", "text": s });
            if let Some(cc) = cc {
                block["cache_control"] = cc.clone();
            }
            Value::Array(vec![block])
        }
        UserContent::Blocks(blocks) => {
            let mut arr: Vec<Value> = blocks.iter().map(user_block_to_value).collect();
            if let (Some(cc), Some(last)) = (cc, arr.last_mut()) {
                last["cache_control"] = cc.clone();
            }
            Value::Array(arr)
        }
    }
}

fn user_block_to_value(b: &UserContentBlock) -> Value {
    match b {
        UserContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
        UserContentBlock::Image(i) => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": i.mime_type, "data": i.data },
        }),
    }
}

fn content_block_to_value(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text(t) => json!({ "type": "text", "text": t.text }),
        ContentBlock::Thinking(t) => {
            let mut v = json!({ "type": "thinking", "thinking": t.thinking });
            if let Some(sig) = &t.thinking_signature {
                v["signature"] = json!(sig);
            }
            v
        }
        ContentBlock::Image(i) => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": i.mime_type, "data": i.data },
        }),
        ContentBlock::ToolCall(tc) => json!({
            "type": "tool_use",
            "id": tc.id,
            "name": tc.name,
            "input": tc.arguments,
        }),
    }
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

    fn mk_model() -> Model {
        Model {
            id: "claude-test".into(),
            name: "Claude Test".into(),
            api: Api::known(KnownApi::AnthropicMessages),
            provider: Provider::from("anthropic"),
            base_url: "https://api.anthropic.com".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn cache_control_applied_to_system_and_last_user() {
        let m = mk_model();
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![
                Message::User(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("first".into()),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text("last".into()),
                    timestamp: 0,
                }),
            ],
            tools: None,
        };
        let opts = StreamOptions {
            cache_retention: Some(CacheRetention::Short),
            ..Default::default()
        };
        let body = build_request_body(&m, &ctx, &opts, &resolve_compat(&m)).unwrap();

        let sys = &body["system"];
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");

        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(
            msgs[0]["content"][0]["cache_control"].is_null(),
            "first user should not have cache_control"
        );
        assert_eq!(
            msgs[1]["content"][0]["cache_control"]["type"], "ephemeral",
            "last user should have cache_control"
        );
    }

    #[test]
    fn long_retention_adds_ttl() {
        let m = mk_model();
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let opts = StreamOptions {
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let body = build_request_body(&m, &ctx, &opts, &resolve_compat(&m)).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn temperature_dropped_when_thinking_enabled() {
        let m = mk_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: None,
        };
        let mut opts = StreamOptions {
            temperature: Some(0.7),
            ..Default::default()
        };
        opts.provider_extras.insert(
            "thinking".into(),
            json!({ "type": "enabled", "budget_tokens": 4096 }),
        );
        let body = build_request_body(&m, &ctx, &opts, &resolve_compat(&m)).unwrap();
        assert!(
            body.get("temperature").is_none(),
            "temperature must be dropped"
        );
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn tools_get_cache_control_on_last() {
        let m = mk_model();
        let tools = vec![
            Tool {
                name: "a".into(),
                description: "a".into(),
                parameters: json!({ "type": "object" }),
            },
            Tool {
                name: "b".into(),
                description: "b".into(),
                parameters: json!({ "type": "object" }),
            },
        ];
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: Some(tools),
        };
        let opts = StreamOptions {
            cache_retention: Some(CacheRetention::Short),
            ..Default::default()
        };
        let body = build_request_body(&m, &ctx, &opts, &resolve_compat(&m)).unwrap();
        let tools_v = body["tools"].as_array().unwrap();
        assert!(
            tools_v[0].get("cache_control").is_none(),
            "first tool should not have cc"
        );
        assert_eq!(tools_v[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn fireworks_compat_disables_cache_on_tools() {
        let mut m = mk_model();
        m.provider = Provider::from("fireworks");
        let compat = resolve_compat(&m);
        assert!(!compat.supports_cache_control_on_tools);
        assert!(!compat.supports_long_cache_retention);
        assert!(compat.send_session_affinity_headers);
    }
}
