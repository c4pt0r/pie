//! Auto-compaction. Partial 1:1 port of
//! `packages/agent/src/harness/compaction/compaction.ts` (~755 lines).
//!
//! Implemented:
//! - `CompactionSettings` + `DEFAULT_COMPACTION_SETTINGS`
//! - `calculate_context_tokens` / `estimate_tokens` / `estimate_context_tokens`
//! - `should_compact`
//! - `find_turn_start_index` / `find_cut_point` (turn-boundary-safe)
//! - `SUMMARIZATION_SYSTEM_PROMPT`
//! - `generate_summary` (calls the StreamFn to summarize a message prefix)
//! - `prepare_compaction` (decides cut point + assembles entries to summarize)
//! - `compact` (the orchestration entry point)
//!
//! TODO:
//! - more nuanced char→token weights for image/tool blocks (currently flat)
//! - `serialize_conversation` formatting parity with TS (used inside summarization prompts)

use futures::StreamExt;
use pie_ai::{
    AssistantMessage, AssistantMessageEvent, Context as PiContext, Message as PiMessage, Model,
    SimpleStreamOptions, Usage,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::super::super::types::default_stream_fn;
use super::super::super::types::*;
use super::super::session::session::SessionTreeEntry;

// ──────────────────────────────────────────────────────────────────────────────────────────
// Settings
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompactionSettings {
    /// Enable automatic compaction decisions.
    pub enabled: bool,
    /// Tokens reserved for summary prompt + output.
    pub reserve_tokens: u32,
    /// Approximate recent-context tokens to keep after compaction.
    pub keep_recent_tokens: u32,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS.clone()
    }
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: 16_384,
    keep_recent_tokens: 20_000,
};

// ──────────────────────────────────────────────────────────────────────────────────────────
// Token estimation
// ──────────────────────────────────────────────────────────────────────────────────────────

pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn assistant_usage(msg: &AgentMessage) -> Option<&Usage> {
    let AgentMessage::Llm(PiMessage::Assistant(a)) = msg else {
        return None;
    };
    if matches!(
        a.stop_reason,
        pie_ai::StopReason::Aborted | pie_ai::StopReason::Error
    ) {
        return None;
    }
    if a.usage.total_tokens == 0
        && a.usage.input == 0
        && a.usage.output == 0
        && a.usage.cache_read == 0
        && a.usage.cache_write == 0
    {
        return None;
    }
    Some(&a.usage)
}

pub fn get_last_assistant_usage(entries: &[SessionTreeEntry]) -> Option<Usage> {
    for e in entries.iter().rev() {
        if let SessionTreeEntry::Message { message, .. } = e {
            if let Some(u) = assistant_usage(message) {
                return Some(u.clone());
            }
        }
    }
    None
}

/// Conservative char-class-aware text estimate: ~4 chars per token for ASCII, ~1 token per char
/// for non-ASCII (CJK and similar scripts tokenize close to one token per character). Rounds up.
pub fn estimate_text_tokens(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

/// Conservative per-message estimate built on [`estimate_text_tokens`].
pub fn estimate_tokens(message: &AgentMessage) -> u64 {
    match message {
        AgentMessage::Llm(PiMessage::User(u)) => match &u.content {
            pie_ai::UserContent::Text(s) => estimate_text_tokens(s),
            pie_ai::UserContent::Blocks(blocks) => blocks.iter().map(user_block_tokens).sum(),
        },
        AgentMessage::Llm(PiMessage::Assistant(a)) => {
            a.content.iter().map(content_block_tokens).sum()
        }
        AgentMessage::Llm(PiMessage::ToolResult(tr)) => {
            estimate_text_tokens(&tr.tool_name)
                + tr.content.iter().map(user_block_tokens).sum::<u64>()
        }
        AgentMessage::Custom(c) => {
            estimate_text_tokens(&c.role) + estimate_text_tokens(&c.payload.to_string())
        }
    }
}

fn user_block_tokens(b: &pie_ai::UserContentBlock) -> u64 {
    match b {
        pie_ai::UserContentBlock::Text(t) => estimate_text_tokens(&t.text),
        // Images are weighted as a flat 768 tokens — matches Anthropic's pricing approximation.
        // TS uses a similar heuristic.
        pie_ai::UserContentBlock::Image(_) => 768,
    }
}

fn content_block_tokens(b: &pie_ai::ContentBlock) -> u64 {
    match b {
        pie_ai::ContentBlock::Text(t) => estimate_text_tokens(&t.text),
        pie_ai::ContentBlock::Thinking(t) => estimate_text_tokens(&t.thinking),
        pie_ai::ContentBlock::Image(_) => 768,
        pie_ai::ContentBlock::ToolCall(tc) => {
            estimate_text_tokens(&tc.name)
                + estimate_text_tokens(&serde_json::Value::Object(tc.arguments.clone()).to_string())
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    pub usage_tokens: u64,
    pub trailing_tokens: u64,
    /// Index of the message that provided the usage block, or `None` when no assistant turn has
    /// finished yet.
    pub last_usage_index: Option<usize>,
}

pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    let mut last_with_usage: Option<(usize, &Usage)> = None;
    for (i, m) in messages.iter().enumerate() {
        if let Some(u) = assistant_usage(m) {
            last_with_usage = Some((i, u));
        }
    }
    let Some((idx, usage)) = last_with_usage else {
        let total = messages.iter().map(estimate_tokens).sum();
        return ContextUsageEstimate {
            tokens: total,
            usage_tokens: 0,
            trailing_tokens: total,
            last_usage_index: None,
        };
    };
    let usage_tokens = calculate_context_tokens(usage);
    let trailing: u64 = messages[idx + 1..].iter().map(estimate_tokens).sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing,
        usage_tokens,
        trailing_tokens: trailing,
        last_usage_index: Some(idx),
    }
}

pub fn should_compact(
    context_tokens: u64,
    context_window: u32,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    let window = context_window as u64;
    // Trigger auto-compaction at 80% of the context window so there's still
    // headroom for the summarizer LLM call and the next turn. Waiting until
    // `window - reserve_tokens` (≈87%+ with defaults) meant the next response
    // could overflow the window before compaction had a chance to run.
    let threshold = (window * 4) / 5;
    context_tokens > threshold
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Cut-point detection (turn-boundary safe)
// ──────────────────────────────────────────────────────────────────────────────────────────

/// Walk backward from `entry_index` until we hit a user-message entry — the turn boundary.
/// Returns that user-message's index. If no user message exists in `entries[start_index..=entry_index]`,
/// returns `start_index`.
pub fn find_turn_start_index(
    entries: &[SessionTreeEntry],
    entry_index: usize,
    start_index: usize,
) -> usize {
    let upper = entry_index.min(entries.len().saturating_sub(1));
    let mut i = upper as isize;
    while i >= start_index as isize {
        let idx = i as usize;
        if let SessionTreeEntry::Message { message, .. } = &entries[idx] {
            if matches!(message, AgentMessage::Llm(PiMessage::User(_))) {
                return idx;
            }
        }
        i -= 1;
    }
    start_index
}

#[derive(Clone, Debug)]
pub struct CutPointResult {
    /// Index in `entries` such that entries[..cut_index] are summarized and entries[cut_index..]
    /// are kept verbatim.
    pub cut_index: usize,
    /// id of the first kept entry, used in the `compaction` record.
    pub first_kept_entry_id: Option<String>,
}

/// Find a safe cut point keeping at least `keep_recent_tokens` of trailing context. Always lands
/// on a turn boundary.
pub fn find_cut_point(
    entries: &[SessionTreeEntry],
    settings: &CompactionSettings,
) -> CutPointResult {
    if entries.is_empty() {
        return CutPointResult {
            cut_index: 0,
            first_kept_entry_id: None,
        };
    }
    // Walk backward summing tokens until we've kept `keep_recent_tokens`, then back up to the
    // turn boundary above that.
    let mut acc: u64 = 0;
    let mut target = entries.len();
    for (i, entry) in entries.iter().enumerate().rev() {
        if let SessionTreeEntry::Message { message, .. } = entry {
            acc += estimate_tokens(message);
        }
        if acc >= settings.keep_recent_tokens as u64 {
            target = i;
            break;
        }
    }
    let cut = find_turn_start_index(entries, target, 0);
    let first_kept_entry_id = entries.get(cut).map(|e| e.id().to_string());
    CutPointResult {
        cut_index: cut,
        first_kept_entry_id,
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// Summarization
// ──────────────────────────────────────────────────────────────────────────────────────────

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary preserving the user's intent, the files and topics discussed, decisions made, and any work still in progress. Be concise but thorough; the assistant will rely on your summary instead of replaying the dropped messages.";
const DEFAULT_SUMMARY_PROMPT_TOKEN_BUDGET: u64 = 64_000;

/// Synchronous helper used by the LLM-backed `generate_summary`. Serialize a message list into a
/// compact text dump for the summarizer prompt.
pub fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            AgentMessage::Llm(PiMessage::User(u)) => {
                out.push_str("USER:\n");
                match &u.content {
                    pie_ai::UserContent::Text(s) => out.push_str(s),
                    pie_ai::UserContent::Blocks(blocks) => {
                        for b in blocks {
                            match b {
                                pie_ai::UserContentBlock::Text(t) => out.push_str(&t.text),
                                pie_ai::UserContentBlock::Image(_) => out.push_str("<image>"),
                            }
                        }
                    }
                }
                out.push_str("\n\n");
            }
            AgentMessage::Llm(PiMessage::Assistant(a)) => {
                out.push_str("ASSISTANT:\n");
                for b in &a.content {
                    match b {
                        pie_ai::ContentBlock::Text(t) => out.push_str(&t.text),
                        pie_ai::ContentBlock::Thinking(t) => {
                            out.push_str("<thinking>");
                            out.push_str(&t.thinking);
                            out.push_str("</thinking>");
                        }
                        pie_ai::ContentBlock::Image(_) => out.push_str("<image>"),
                        pie_ai::ContentBlock::ToolCall(tc) => {
                            out.push_str(&format!(
                                "<tool_call name=\"{}\">{}</tool_call>",
                                tc.name,
                                serde_json::Value::Object(tc.arguments.clone())
                            ));
                        }
                    }
                }
                out.push_str("\n\n");
            }
            AgentMessage::Llm(PiMessage::ToolResult(tr)) => {
                out.push_str(&format!("TOOL_RESULT[{}]:\n", tr.tool_name));
                for b in &tr.content {
                    if let pie_ai::UserContentBlock::Text(t) = b {
                        out.push_str(&t.text);
                    }
                }
                out.push_str("\n\n");
            }
            AgentMessage::Custom(c) => {
                out.push_str(&format!("{}:\n{}\n\n", c.role.to_uppercase(), c.payload));
            }
        }
    }
    out
}

/// Prompt framing slack (message wrappers, the omission-note message, provider envelope).
const SUMMARY_PROMPT_FRAMING_TOKENS: u64 = 512;
/// Floor for the overflow-retry budget halving in [`compact`].
const MIN_SUMMARY_PROMPT_BUDGET_TOKENS: u64 = 1_024;
/// Maximum provider-overflow retries before compaction gives up.
const MAX_SUMMARY_OVERFLOW_RETRIES: u32 = 3;

/// Output cap sent as `max_tokens` on the summarizer call. Providers fall back to
/// `model.max_tokens` when unset, and `input + max_tokens > context_window` is a hard 400 on
/// Anthropic — so the summarizer must always send an explicit, bounded value.
fn summary_output_tokens(model: &Model, settings: &CompactionSettings) -> u32 {
    let reserve = if settings.reserve_tokens > 0 {
        settings.reserve_tokens
    } else {
        DEFAULT_COMPACTION_SETTINGS.reserve_tokens
    };
    let mut output = if model.max_tokens > 0 {
        model.max_tokens.min(reserve)
    } else {
        reserve
    };
    if model.context_window > 0 {
        output = output.min(model.context_window / 4).max(1);
    }
    output
}

fn summarization_prompt_budget(model: &Model, settings: &CompactionSettings) -> u64 {
    if model.context_window == 0 {
        return DEFAULT_SUMMARY_PROMPT_TOKEN_BUDGET;
    }
    let window = model.context_window as u64;
    let output = summary_output_tokens(model, settings) as u64;
    // Keep 20% slack below (window - output): the char-class token estimate can undercount on
    // code-heavy or mixed-script content, and Anthropic rejects input + max_tokens > window.
    window.saturating_sub(output).saturating_mul(4) / 5
}

fn summary_prompt_overhead_tokens(custom_instructions: Option<&str>) -> u64 {
    SUMMARY_PROMPT_FRAMING_TOKENS
        + estimate_text_tokens(SUMMARIZATION_SYSTEM_PROMPT)
        + custom_instructions
            .map(estimate_text_tokens)
            .unwrap_or_default()
}

fn summarize_prompt_estimate_tokens(
    messages: &[AgentMessage],
    custom_instructions: Option<&str>,
) -> u64 {
    let conversation: u64 = messages.iter().map(estimate_tokens).sum();
    summary_prompt_overhead_tokens(custom_instructions) + conversation
}

fn trim_messages_for_summary_budget(
    messages: &[AgentMessage],
    budget_tokens: u64,
    custom_instructions: Option<&str>,
) -> Vec<AgentMessage> {
    if summarize_prompt_estimate_tokens(messages, custom_instructions) <= budget_tokens {
        return messages.to_vec();
    }

    let mut kept = Vec::new();
    let mut total = summary_prompt_overhead_tokens(custom_instructions);
    for message in messages.iter().rev() {
        let message_tokens = estimate_tokens(message);
        if !kept.is_empty() && total + message_tokens > budget_tokens {
            break;
        }
        kept.push(message.clone());
        total = total.saturating_add(message_tokens);
        if total >= budget_tokens {
            break;
        }
    }
    kept.reverse();
    let omitted = messages.len().saturating_sub(kept.len());
    if omitted > 0 {
        kept.insert(
            0,
            AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
                role: pie_ai::UserRole::User,
                content: pie_ai::UserContent::Text(format!(
                    "[compaction note: omitted {omitted} older message(s) before summarization because the session exceeded the summarizer prompt budget]"
                )),
                timestamp: chrono::Utc::now().timestamp_millis(),
            })),
        );
    }
    kept
}

/// Byte index where the suffix of `s` last fits within `budget_tokens` by the char-class
/// estimate. Always lands on a char boundary.
fn suffix_start_for_token_budget(s: &str, budget_tokens: u64) -> usize {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    let mut start = s.len();
    for (idx, c) in s.char_indices().rev() {
        let (next_ascii, next_non_ascii) = if c.is_ascii() {
            (ascii + 1, non_ascii)
        } else {
            (ascii, non_ascii + 1)
        };
        if next_ascii.div_ceil(4) + next_non_ascii > budget_tokens {
            break;
        }
        ascii = next_ascii;
        non_ascii = next_non_ascii;
        start = idx;
    }
    start
}

fn serialize_conversation_for_summary_budget(
    messages: &[AgentMessage],
    budget_tokens: u64,
    custom_instructions: Option<&str>,
) -> String {
    let messages = trim_messages_for_summary_budget(messages, budget_tokens, custom_instructions);
    let conversation = serialize_conversation(&messages);
    let available_tokens =
        budget_tokens.saturating_sub(summary_prompt_overhead_tokens(custom_instructions));
    if estimate_text_tokens(&conversation) <= available_tokens {
        return conversation;
    }

    let note = "[compaction note: omitted older serialized content before summarization because the session exceeded the summarizer prompt budget]\n\n";
    let note_tokens = estimate_text_tokens(note);
    if available_tokens <= note_tokens {
        // The note is ASCII, so ~4 chars per token.
        return note
            .chars()
            .take(available_tokens.saturating_mul(4) as usize)
            .collect();
    }

    let start = suffix_start_for_token_budget(&conversation, available_tokens - note_tokens);
    format!("{note}{}", &conversation[start..])
}

#[derive(Clone)]
pub struct GenerateSummaryRequest {
    pub model: Model,
    pub messages: Vec<AgentMessage>,
    pub custom_instructions: Option<String>,
    pub prompt_budget_tokens: Option<u64>,
    /// Explicit `max_tokens` for the summarizer call. Providers fall back to `model.max_tokens`
    /// when `None`, which can push `input + max_tokens` past the context window.
    pub max_output_tokens: Option<u32>,
    /// Override stream function; falls back to `pie_ai::stream_simple` when `None`.
    pub stream_fn: Option<StreamFn>,
}

#[derive(Clone, Debug)]
pub struct GenerateSummaryOutput {
    pub summary: String,
    pub usage: Usage,
}

/// Call the LLM to produce a single text summary of the supplied messages.
pub async fn generate_summary(
    request: GenerateSummaryRequest,
    cancel: CancellationToken,
) -> Result<GenerateSummaryOutput, SummarizeError> {
    let mut prompt = SUMMARIZATION_SYSTEM_PROMPT.to_string();
    if let Some(extra) = request.custom_instructions.as_deref() {
        prompt.push_str("\n\n");
        prompt.push_str(extra);
    }

    let convo = if let Some(budget) = request.prompt_budget_tokens {
        serialize_conversation_for_summary_budget(
            &request.messages,
            budget,
            request.custom_instructions.as_deref(),
        )
    } else {
        serialize_conversation(&request.messages)
    };
    let user = pie_ai::UserMessage {
        role: pie_ai::UserRole::User,
        content: pie_ai::UserContent::Text(convo),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    let context = PiContext {
        system_prompt: Some(prompt),
        messages: vec![pie_ai::Message::User(user)],
        tools: None,
    };
    let stream_fn = request.stream_fn.unwrap_or_else(default_stream_fn);
    let mut options = SimpleStreamOptions::default();
    options.base.abort = Some(cancel.clone());
    options.base.max_tokens = request.max_output_tokens;

    let mut stream = stream_fn(&request.model, &context, Some(&options));
    let mut last: Option<AssistantMessage> = None;
    while let Some(ev) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(SummarizeError::Aborted);
        }
        match ev {
            AssistantMessageEvent::Done { message, .. } => last = Some(message),
            AssistantMessageEvent::Error { error, .. } => {
                let window = (request.model.context_window > 0)
                    .then_some(request.model.context_window as u64);
                let overflowed = pie_ai::is_context_overflow(&error, window);
                let message = error
                    .error_message
                    .unwrap_or_else(|| "summarization failed".into());
                return Err(if overflowed {
                    SummarizeError::ContextOverflow(message)
                } else {
                    SummarizeError::Provider(message)
                });
            }
            _ => {}
        }
    }
    let msg = last.ok_or(SummarizeError::Empty)?;
    let summary = msg
        .content
        .iter()
        .filter_map(|b| match b {
            pie_ai::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    Ok(GenerateSummaryOutput {
        summary,
        usage: msg.usage,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SummarizeError {
    #[error("aborted")]
    Aborted,
    #[error("provider error: {0}")]
    Provider(String),
    #[error("summarizer prompt overflowed the model context window: {0}")]
    ContextOverflow(String),
    #[error("summarizer produced no message")]
    Empty,
}

// ──────────────────────────────────────────────────────────────────────────────────────────
// prepare_compaction + compact
// ──────────────────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    pub cut: CutPointResult,
    /// Messages that will be summarized (i.e., the prefix that compaction folds).
    pub entries_to_summarize: Vec<SessionTreeEntry>,
    /// Sum of estimated tokens for the prefix being summarized.
    pub tokens_before: u64,
}

pub fn prepare_compaction(
    entries: &[SessionTreeEntry],
    settings: &CompactionSettings,
) -> CompactionPreparation {
    let cut = find_cut_point(entries, settings);
    let entries_to_summarize = entries[..cut.cut_index].to_vec();
    let tokens_before = entries_to_summarize
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Message { message, .. } => Some(estimate_tokens(message)),
            _ => None,
        })
        .sum();
    CompactionPreparation {
        cut,
        entries_to_summarize,
        tokens_before,
    }
}

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub usage: Usage,
}

/// Top-level compaction entry point. Picks a cut point, summarizes the prefix, returns the
/// summary plus metadata for the harness to record on the session.
pub async fn compact(
    model: Model,
    entries: &[SessionTreeEntry],
    settings: &CompactionSettings,
    custom_instructions: Option<String>,
    stream_fn: Option<StreamFn>,
    cancel: CancellationToken,
) -> Result<CompactionResult, SummarizeError> {
    let prep = prepare_compaction(entries, settings);
    if prep.entries_to_summarize.is_empty() {
        return Ok(CompactionResult {
            summary: String::new(),
            first_kept_entry_id: prep.cut.first_kept_entry_id,
            tokens_before: prep.tokens_before,
            usage: Usage::default(),
        });
    }
    // Project the entries into AgentMessage[] for the summarizer.
    let messages: Vec<AgentMessage> = prep
        .entries_to_summarize
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    // The prompt budget is a char-class estimate, so the provider can still reject the call as a
    // context overflow. Halve the budget and retry instead of failing the whole compaction.
    let max_output_tokens = summary_output_tokens(&model, settings);
    let mut budget = summarization_prompt_budget(&model, settings);
    let mut attempts = 0u32;
    let out = loop {
        let result = generate_summary(
            GenerateSummaryRequest {
                model: model.clone(),
                messages: messages.clone(),
                custom_instructions: custom_instructions.clone(),
                prompt_budget_tokens: Some(budget),
                max_output_tokens: Some(max_output_tokens),
                stream_fn: stream_fn.clone(),
            },
            cancel.clone(),
        )
        .await;
        match result {
            Ok(out) => break out,
            Err(SummarizeError::ContextOverflow(message)) => {
                attempts += 1;
                if attempts > MAX_SUMMARY_OVERFLOW_RETRIES
                    || budget <= MIN_SUMMARY_PROMPT_BUDGET_TOKENS
                {
                    return Err(SummarizeError::ContextOverflow(message));
                }
                budget = (budget / 2).max(MIN_SUMMARY_PROMPT_BUDGET_TOKENS);
            }
            Err(e) => return Err(e),
        }
    };
    Ok(CompactionResult {
        summary: out.summary,
        first_kept_entry_id: prep.cut.first_kept_entry_id,
        tokens_before: prep.tokens_before,
        usage: out.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::Llm(PiMessage::User(pie_ai::UserMessage {
            role: pie_ai::UserRole::User,
            content: pie_ai::UserContent::Text(text.into()),
            timestamp: 0,
        }))
    }

    fn model_with_limits(context_window: u32, max_tokens: u32) -> Model {
        Model {
            id: "faux".into(),
            name: "Faux".into(),
            api: pie_ai::Api::from("faux"),
            provider: pie_ai::Provider::from("faux"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: pie_ai::ModelCost::default(),
            context_window,
            max_tokens,
            headers: None,
            compat: None,
        }
    }

    fn model_with_context_window(context_window: u32) -> Model {
        model_with_limits(context_window, 0)
    }

    fn done_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            role: pie_ai::AssistantRole::Assistant,
            content: vec![pie_ai::ContentBlock::text(text)],
            api: pie_ai::Api::from("faux"),
            provider: pie_ai::Provider::from("faux"),
            model: "faux".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: pie_ai::StopReason::Stop,
            error_message: None,
            timestamp: 0,
        }
    }

    fn oversized_entries(count: usize) -> Vec<SessionTreeEntry> {
        let mut entries = Vec::new();
        let mut parent_id = None;
        for i in 0..count {
            let id = format!("entry-{i}");
            entries.push(SessionTreeEntry::Message {
                id: id.clone(),
                parent_id: parent_id.clone(),
                timestamp: "t".into(),
                message: user(&format!("old-msg-{i} {}", "x".repeat(1600))),
            });
            parent_id = Some(id);
        }
        entries
    }

    fn assistant(text: &str, stop: pie_ai::StopReason, usage: Usage) -> AgentMessage {
        AgentMessage::Llm(PiMessage::Assistant(pie_ai::AssistantMessage {
            role: pie_ai::AssistantRole::Assistant,
            content: vec![pie_ai::ContentBlock::text(text)],
            api: pie_ai::Api::from("faux"),
            provider: pie_ai::Provider::from("faux"),
            model: "faux".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage,
            stop_reason: stop,
            error_message: None,
            timestamp: 0,
        }))
    }

    #[test]
    fn should_compact_when_over_threshold() {
        let s = CompactionSettings {
            enabled: true,
            reserve_tokens: 1024,
            keep_recent_tokens: 0,
        };
        // Threshold is 80% of window = 102_400 for a 128K window.
        assert!(should_compact(102_401, 128_000, &s));
        assert!(!should_compact(102_400, 128_000, &s));
        // Also triggers at higher usage.
        assert!(should_compact(127_000, 128_000, &s));
        // Well below threshold does not trigger.
        assert!(!should_compact(80_000, 128_000, &s));
    }

    #[tokio::test]
    async fn compact_trims_summarizer_prompt_before_provider_call() {
        let mut entries = Vec::new();
        let mut parent_id = None;
        for i in 0..80 {
            let id = format!("entry-{i}");
            entries.push(SessionTreeEntry::Message {
                id: id.clone(),
                parent_id: parent_id.clone(),
                timestamp: "t".into(),
                message: user(&format!("old-msg-{i} {}", "x".repeat(1600))),
            });
            parent_id = Some(id);
        }

        let captured = Arc::new(Mutex::new(String::new()));
        let captured_clone = captured.clone();
        let stream_fn: StreamFn = Arc::new(move |_, context, _| {
            let text = match &context.messages[0] {
                PiMessage::User(user) => match &user.content {
                    pie_ai::UserContent::Text(text) => text.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            assert!(
                text.len().div_ceil(4) < 4_000,
                "summarizer prompt must be trimmed before provider dispatch; got {} chars",
                text.len()
            );
            assert!(
                text.contains("[compaction note: omitted"),
                "trimmed prompt must disclose omitted older content"
            );
            assert!(
                !text.contains("old-msg-0"),
                "oldest oversized content should not reach the provider prompt"
            );
            *captured_clone.lock().unwrap() = text;

            let (stream, mut sender) = pie_ai::AssistantMessageEventStream::new();
            tokio::spawn(async move {
                let msg = AssistantMessage {
                    role: pie_ai::AssistantRole::Assistant,
                    content: vec![pie_ai::ContentBlock::text("bounded summary")],
                    api: pie_ai::Api::from("faux"),
                    provider: pie_ai::Provider::from("faux"),
                    model: "faux".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    usage: Usage::default(),
                    stop_reason: pie_ai::StopReason::Stop,
                    error_message: None,
                    timestamp: 0,
                };
                sender.push(AssistantMessageEvent::Done {
                    reason: pie_ai::DoneReason::Stop,
                    message: msg,
                });
            });
            stream
        });

        let result = compact(
            model_with_context_window(5_000),
            &entries,
            &CompactionSettings {
                enabled: true,
                reserve_tokens: 1_000,
                keep_recent_tokens: 1,
            },
            None,
            Some(stream_fn),
            CancellationToken::new(),
        )
        .await
        .expect("compaction should succeed with a bounded summarizer prompt");

        assert_eq!(result.summary, "bounded summary");
        assert!(!captured.lock().unwrap().is_empty());
    }

    #[test]
    fn summary_budget_caps_single_oversized_message() {
        let conversation =
            serialize_conversation_for_summary_budget(&[user(&"x".repeat(50_000))], 2_000, None);
        assert!(
            conversation.len().div_ceil(4) <= 2_000,
            "serialized compaction prompt must fit the budget; got {} chars",
            conversation.len()
        );
        assert!(
            conversation.starts_with("[compaction note: omitted older serialized content"),
            "single-message truncation must disclose omitted content"
        );
    }

    #[tokio::test]
    async fn summarizer_request_sets_bounded_max_tokens() {
        // Claude-4.x shape: 200k window, 64k default max output. The provider falls back to
        // model.max_tokens when options don't set one, which would make input+output overflow
        // the window. The summarizer must send an explicit, bounded max_tokens.
        let entries = oversized_entries(10);
        let captured_max_tokens = Arc::new(Mutex::new(None::<u32>));
        let captured_clone = captured_max_tokens.clone();
        let stream_fn: StreamFn = Arc::new(move |_, _, options| {
            *captured_clone.lock().unwrap() = options.and_then(|o| o.base.max_tokens);
            let (stream, mut sender) = pie_ai::AssistantMessageEventStream::new();
            tokio::spawn(async move {
                sender.push(AssistantMessageEvent::Done {
                    reason: pie_ai::DoneReason::Stop,
                    message: done_message("summary"),
                });
            });
            stream
        });

        compact(
            model_with_limits(200_000, 64_000),
            &entries,
            &CompactionSettings {
                enabled: true,
                reserve_tokens: 16_384,
                keep_recent_tokens: 1,
            },
            None,
            Some(stream_fn),
            CancellationToken::new(),
        )
        .await
        .expect("compaction should succeed");

        let max_tokens = captured_max_tokens.lock().unwrap().take();
        assert_eq!(
            max_tokens,
            Some(16_384),
            "summarizer must cap output at reserve_tokens instead of inheriting model.max_tokens"
        );
    }

    #[test]
    fn summary_budget_leaves_room_for_output_and_estimate_error() {
        let model = model_with_limits(200_000, 64_000);
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        };
        let budget = summarization_prompt_budget(&model, &settings);
        assert!(budget > 0);
        // The char-based token estimate can undercount by ~20-30% on code or CJK text, so the
        // prompt budget must keep slack below (window - reserved output) rather than using it all.
        assert!(
            budget <= (200_000 - 16_384) * 4 / 5,
            "budget {budget} leaves no slack for token-estimate error"
        );
    }

    #[test]
    fn cjk_truncation_respects_token_budget() {
        // CJK chars are ~1 token each but 3 UTF-8 bytes; a bytes/4 estimate undercounts ~3x.
        let conversation =
            serialize_conversation_for_summary_budget(&[user(&"夏".repeat(50_000))], 2_000, None);
        let ascii = conversation.chars().filter(char::is_ascii).count() as u64;
        let non_ascii = conversation.chars().count() as u64 - ascii;
        let estimated_tokens = ascii.div_ceil(4) + non_ascii;
        assert!(
            estimated_tokens <= 2_000,
            "CJK-heavy prompt must fit the token budget; estimated {estimated_tokens} tokens"
        );
        assert!(
            conversation.contains("[compaction note: omitted"),
            "truncation must disclose omitted content"
        );
    }

    #[tokio::test]
    async fn compact_retries_with_smaller_budget_on_provider_overflow() {
        // Even a bounded estimate can undercount real tokens; when the provider still rejects
        // the summarizer call as context overflow, compaction must retry with a smaller prompt
        // instead of failing the whole compaction.
        let entries = oversized_entries(80);
        let prompt_lens = Arc::new(Mutex::new(Vec::<usize>::new()));
        let prompt_lens_clone = prompt_lens.clone();
        let stream_fn: StreamFn = Arc::new(move |_, context, _| {
            let text = match &context.messages[0] {
                PiMessage::User(user) => match &user.content {
                    pie_ai::UserContent::Text(text) => text.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            let call_index = {
                let mut lens = prompt_lens_clone.lock().unwrap();
                lens.push(text.len());
                lens.len()
            };
            let (stream, mut sender) = pie_ai::AssistantMessageEventStream::new();
            tokio::spawn(async move {
                if call_index == 1 {
                    let mut error = done_message("");
                    error.stop_reason = pie_ai::StopReason::Error;
                    error.error_message =
                        Some("prompt is too long: 5500 tokens > 5000 maximum".into());
                    sender.push(AssistantMessageEvent::Error {
                        reason: pie_ai::ErrorReason::Error,
                        error,
                    });
                } else {
                    sender.push(AssistantMessageEvent::Done {
                        reason: pie_ai::DoneReason::Stop,
                        message: done_message("summary after retry"),
                    });
                }
            });
            stream
        });

        let result = compact(
            model_with_context_window(5_000),
            &entries,
            &CompactionSettings {
                enabled: true,
                reserve_tokens: 1_000,
                keep_recent_tokens: 1,
            },
            None,
            Some(stream_fn),
            CancellationToken::new(),
        )
        .await
        .expect("compaction should survive one provider overflow rejection");

        assert_eq!(result.summary, "summary after retry");
        let lens = prompt_lens.lock().unwrap();
        assert_eq!(lens.len(), 2, "expected exactly one retry");
        assert!(
            lens[1] < lens[0],
            "retry must shrink the prompt: {} -> {}",
            lens[0],
            lens[1]
        );
    }

    #[test]
    fn disabled_compaction_returns_false() {
        let s = CompactionSettings {
            enabled: false,
            ..Default::default()
        };
        assert!(!should_compact(1_000_000, 128_000, &s));
    }

    #[test]
    fn estimate_context_tokens_uses_last_usage_block() {
        let msgs = vec![
            user("hi"),
            assistant(
                "ok",
                pie_ai::StopReason::Stop,
                Usage {
                    input: 100,
                    output: 50,
                    total_tokens: 150,
                    ..Default::default()
                },
            ),
            user("more"),
        ];
        let est = estimate_context_tokens(&msgs);
        assert_eq!(est.usage_tokens, 150);
        // Trailing user("more") gets char-estimated, so total > 150.
        assert!(est.tokens > 150);
        assert_eq!(est.last_usage_index, Some(1));
    }

    #[test]
    fn cut_point_lands_on_turn_boundary() {
        // entries: U A U A U  (4 turns). Set keep_recent_tokens to a small value so cut is far
        // back; verify it lands on a turn start (user message).
        let entries = vec![
            SessionTreeEntry::Message {
                id: "1".into(),
                parent_id: None,
                timestamp: "t".into(),
                message: user("a"),
            },
            SessionTreeEntry::Message {
                id: "2".into(),
                parent_id: Some("1".into()),
                timestamp: "t".into(),
                message: assistant("b", pie_ai::StopReason::Stop, Usage::default()),
            },
            SessionTreeEntry::Message {
                id: "3".into(),
                parent_id: Some("2".into()),
                timestamp: "t".into(),
                message: user("c"),
            },
            SessionTreeEntry::Message {
                id: "4".into(),
                parent_id: Some("3".into()),
                timestamp: "t".into(),
                message: assistant("d", pie_ai::StopReason::Stop, Usage::default()),
            },
        ];
        let cut = find_cut_point(
            &entries,
            &CompactionSettings {
                keep_recent_tokens: 1,
                ..Default::default()
            },
        );
        // Should land on a turn boundary, i.e., a user message or 0.
        if cut.cut_index < entries.len() {
            if let SessionTreeEntry::Message { message, .. } = &entries[cut.cut_index] {
                assert!(
                    matches!(message, AgentMessage::Llm(PiMessage::User(_))) || cut.cut_index == 0
                );
            }
        }
    }
}
