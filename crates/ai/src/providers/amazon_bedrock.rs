//! Amazon Bedrock provider (`bedrock-converse-stream`). Partial 1:1 port of
//! `packages/ai/src/providers/amazon-bedrock.ts` (~950 LOC).
//!
//! Implemented:
//! - Bearer-token auth (AWS Bedrock API keys — `AWS_BEARER_TOKEN_BEDROCK` or `options.api_key`)
//! - Converse Stream request body (messages / system / inferenceConfig / toolConfig)
//! - AWS binary eventstream frame decoding → AssistantMessageEvent
//!   (messageStart / contentBlockStart / contentBlockDelta / contentBlockStop / messageStop /
//!   metadata for usage)
//! - text / reasoningContent / toolUse content blocks
//!
//! TODO:
//! - SigV4 request signing (the standard AWS credential path); only Bearer is wired today
//! - prompt caching (`cachePoint` blocks)
//! - thinking budget / display modes
//! - image content blocks in tool results

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::api_registry::ApiProvider;
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest, AbortableNext};
use crate::utils::aws_eventstream::AwsEventStream;
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};

#[derive(Default)]
pub struct AmazonBedrockProvider {}

#[async_trait]
impl ApiProvider for AmazonBedrockProvider {
    fn api(&self) -> &str {
        KnownApi::BedrockConverseStream.as_str()
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
        let base = options.map(|o| o.base.clone());
        self.stream(model, context, base.as_ref())
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
        .or_else(|| std::env::var("AWS_BEARER_TOKEN_BEDROCK").ok())
    {
        Some(t) if !t.is_empty() => t,
        _ => {
            push_error(
                &mut sender,
                &model,
                "Bedrock auth missing: set AWS_BEARER_TOKEN_BEDROCK or pass options.api_key (SigV4 signing not yet implemented)".into(),
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
    let base = model.base_url.trim_end_matches('/');
    let url = format!("{base}/model/{}/converse-stream", model.id);
    let mut req = client
        .post(&url)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/vnd.amazon.eventstream");
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
            format!("Bedrock API error ({status}): {txt}"),
        );
        return;
    }

    consume(resp, &model, sender, options.abort.as_ref()).await;
}

async fn consume(
    resp: reqwest::Response,
    model: &Model,
    mut sender: AssistantMessageEventSender,
    abort_token: Option<&tokio_util::sync::CancellationToken>,
) {
    let mut partial = empty_partial(model);
    sender.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    // Bedrock indexes content blocks; map contentBlockIndex → our content vec position.
    let mut tool_args: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut index_map: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();

    let mut es = AwsEventStream::new(resp.bytes_stream());
    loop {
        if sender.is_closed() {
            return;
        }
        let item = match abort_utils::next_or_abort(&mut es, abort_token).await {
            AbortableNext::Item(item) => item,
            AbortableNext::Eof => break,
            AbortableNext::Aborted => {
                abort_utils::push_aborted(&mut sender, model);
                return;
            }
        };
        let frame = match item {
            Ok(f) => f,
            Err(e) => {
                push_error(&mut sender, model, format!("eventstream: {e}"));
                return;
            }
        };
        if let Some(exc) = &frame.exception_type {
            let body = String::from_utf8_lossy(&frame.payload);
            partial.stop_reason = StopReason::Error;
            partial.error_message = Some(format!("{exc}: {body}"));
            sender.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: partial,
            });
            return;
        }
        let Ok(payload): Result<Value, _> = serde_json::from_slice(&frame.payload) else {
            continue;
        };
        match frame.event_type.as_deref() {
            Some("contentBlockStart") => {
                let bidx = payload["contentBlockIndex"].as_u64().unwrap_or(0);
                if let Some(tu) = payload.pointer("/start/toolUse") {
                    let id = tu["toolUseId"].as_str().unwrap_or("").to_string();
                    let name = tu["name"].as_str().unwrap_or("").to_string();
                    let pos = partial.content.len();
                    partial.content.push(ContentBlock::ToolCall(ToolCall {
                        id,
                        name,
                        arguments: Map::new(),
                        thought_signature: None,
                    }));
                    index_map.insert(bidx, pos);
                    tool_args.insert(bidx, String::new());
                    sender.push(AssistantMessageEvent::ToolCallStart {
                        content_index: pos,
                        partial: partial.clone(),
                    });
                }
            }
            Some("contentBlockDelta") => {
                let bidx = payload["contentBlockIndex"].as_u64().unwrap_or(0);
                let delta = &payload["delta"];
                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                    let pos = *index_map.entry(bidx).or_insert_with(|| {
                        let p = partial.content.len();
                        partial.content.push(ContentBlock::text(""));
                        p
                    });
                    let is_first = matches!(partial.content.get(pos), Some(ContentBlock::Text(t)) if t.text.is_empty());
                    if is_first {
                        sender.push(AssistantMessageEvent::TextStart {
                            content_index: pos,
                            partial: partial.clone(),
                        });
                    }
                    if let Some(ContentBlock::Text(t)) = partial.content.get_mut(pos) {
                        t.text.push_str(text);
                    }
                    sender.push(AssistantMessageEvent::TextDelta {
                        content_index: pos,
                        delta: text.to_string(),
                        partial: partial.clone(),
                    });
                } else if let Some(rc) = delta.get("reasoningContent") {
                    if let Some(text) = rc.get("text").and_then(|v| v.as_str()) {
                        let pos = *index_map.entry(bidx).or_insert_with(|| {
                            let p = partial.content.len();
                            partial
                                .content
                                .push(ContentBlock::Thinking(ThinkingContent::default()));
                            p
                        });
                        let is_first = matches!(
                            partial.content.get(pos),
                            Some(ContentBlock::Thinking(t)) if t.thinking.is_empty()
                        );
                        if is_first {
                            sender.push(AssistantMessageEvent::ThinkingStart {
                                content_index: pos,
                                partial: partial.clone(),
                            });
                        }
                        if let Some(ContentBlock::Thinking(t)) = partial.content.get_mut(pos) {
                            t.thinking.push_str(text);
                        }
                        sender.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: pos,
                            delta: text.to_string(),
                            partial: partial.clone(),
                        });
                    }
                } else if let Some(input) = delta.pointer("/toolUse/input").and_then(|v| v.as_str())
                {
                    if let Some(pos) = index_map.get(&bidx).copied() {
                        tool_args.entry(bidx).or_default().push_str(input);
                        sender.push(AssistantMessageEvent::ToolCallDelta {
                            content_index: pos,
                            delta: input.to_string(),
                            partial: partial.clone(),
                        });
                    }
                }
            }
            Some("contentBlockStop") => {
                let bidx = payload["contentBlockIndex"].as_u64().unwrap_or(0);
                if let Some(pos) = index_map.get(&bidx).copied() {
                    let snapshot = partial.content.get(pos).cloned();
                    match snapshot {
                        Some(ContentBlock::Text(t)) => {
                            sender.push(AssistantMessageEvent::TextEnd {
                                content_index: pos,
                                content: t.text,
                                partial: partial.clone(),
                            });
                        }
                        Some(ContentBlock::Thinking(t)) => {
                            sender.push(AssistantMessageEvent::ThinkingEnd {
                                content_index: pos,
                                content: t.thinking,
                                partial: partial.clone(),
                            });
                        }
                        Some(ContentBlock::ToolCall(_)) => {
                            let raw = tool_args.get(&bidx).map(|s| s.as_str()).unwrap_or("");
                            if let Ok(Value::Object(m)) =
                                crate::utils::json_parse::parse_partial_json(raw)
                            {
                                if let Some(ContentBlock::ToolCall(tc)) =
                                    partial.content.get_mut(pos)
                                {
                                    tc.arguments = m;
                                }
                            }
                            if let Some(ContentBlock::ToolCall(tc)) =
                                partial.content.get(pos).cloned()
                            {
                                sender.push(AssistantMessageEvent::ToolCallEnd {
                                    content_index: pos,
                                    tool_call: tc,
                                    partial: partial.clone(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("messageStop") => {
                partial.stop_reason = map_stop_reason(payload["stopReason"].as_str().unwrap_or(""));
            }
            Some("metadata") => {
                if let Some(u) = payload.get("usage") {
                    update_usage(&mut partial.usage, u);
                }
            }
            _ => {}
        }
    }

    let reason = match partial.stop_reason {
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Length => DoneReason::Length,
        _ => DoneReason::Stop,
    };
    sender.push(AssistantMessageEvent::Done {
        reason,
        message: partial,
    });
}

fn map_stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Stop,
    }
}

fn update_usage(usage: &mut Usage, u: &Value) {
    usage.input = u.get("inputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    usage.output = u.get("outputTokens").and_then(|v| v.as_u64()).unwrap_or(0);
    usage.cache_read = u
        .get("cacheReadInputTokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    usage.cache_write = u
        .get("cacheWriteInputTokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

fn build_request_body(context: &Context, options: &StreamOptions) -> Value {
    let mut body = json!({
        "messages": convert_messages(&context.messages),
        "inferenceConfig": inference_config(options),
    });
    if let Some(sys) = &context.system_prompt {
        body["system"] = json!([{ "text": sys }]);
    }
    if let Some(tools) = &context.tools {
        if !tools.is_empty() {
            body["toolConfig"] = json!({ "tools": serialize_tools(tools) });
        }
    }
    body
}

fn inference_config(options: &StreamOptions) -> Value {
    let mut cfg = serde_json::Map::new();
    cfg.insert(
        "maxTokens".into(),
        json!(options.max_tokens.unwrap_or(4096)),
    );
    if let Some(t) = options.temperature {
        cfg.insert("temperature".into(), json!(t));
    }
    Value::Object(cfg)
}

fn serialize_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "toolSpec": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": { "json": t.parameters },
                }
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
                    UserContent::Text(s) => vec![json!({ "text": s })],
                    UserContent::Blocks(blocks) => blocks.iter().map(user_block).collect(),
                };
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(a) => {
                let mut content = Vec::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Text(t) => content.push(json!({ "text": t.text })),
                        ContentBlock::ToolCall(tc) => content.push(json!({
                            "toolUse": { "toolUseId": tc.id, "name": tc.name, "input": tc.arguments }
                        })),
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Message::ToolResult(tr) => {
                let inner: Vec<Value> = tr
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        UserContentBlock::Text(t) => Some(json!({ "text": t.text })),
                        _ => None,
                    })
                    .collect();
                out.push(json!({
                    "role": "user",
                    "content": [{
                        "toolResult": {
                            "toolUseId": tr.tool_call_id,
                            "content": inner,
                            "status": if tr.is_error { "error" } else { "success" },
                        }
                    }],
                }));
            }
        }
    }
    out
}

fn user_block(b: &UserContentBlock) -> Value {
    match b {
        UserContentBlock::Text(t) => json!({ "text": t.text }),
        UserContentBlock::Image(i) => {
            let format = i.mime_type.strip_prefix("image/").unwrap_or("png");
            json!({ "image": { "format": format, "source": { "bytes": i.data } } })
        }
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

    #[test]
    fn body_has_converse_shape() {
        let ctx = Context {
            system_prompt: Some("sys".into()),
            messages: vec![Message::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Text("hi".into()),
                timestamp: 0,
            })],
            tools: Some(vec![Tool {
                name: "t".into(),
                description: "d".into(),
                parameters: json!({ "type": "object" }),
            }]),
        };
        let opts = StreamOptions {
            max_tokens: Some(512),
            ..Default::default()
        };
        let body = build_request_body(&ctx, &opts);
        assert_eq!(body["system"][0]["text"], "sys");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 512);
        assert_eq!(body["toolConfig"]["tools"][0]["toolSpec"]["name"], "t");
    }

    #[test]
    fn tool_result_converts() {
        let msgs = vec![Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "tu_1".into(),
            tool_name: "t".into(),
            content: vec![UserContentBlock::text("ok")],
            details: None,
            is_error: false,
            timestamp: 0,
        })];
        let out = convert_messages(&msgs);
        assert_eq!(out[0]["content"][0]["toolResult"]["toolUseId"], "tu_1");
        assert_eq!(out[0]["content"][0]["toolResult"]["status"], "success");
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
    }
}
