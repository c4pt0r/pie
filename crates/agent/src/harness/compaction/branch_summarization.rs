//! Branch summarization. Partial 1:1 port of
//! `packages/agent/src/harness/compaction/branch-summarization.ts` (~262 lines).
//!
//! Given a child session's entries, generate a summary suitable for a sibling fork to
//! incorporate without replaying every message. Reuses the same `generate_summary` primitive
//! as auto-compaction.

use pie_ai::{Model, Usage};
use tokio_util::sync::CancellationToken;

use super::super::session::session::SessionTreeEntry;
use super::compaction::{GenerateSummaryRequest, SummarizeError, generate_summary};
use crate::types::{AgentMessage, StreamFn};

const BRANCH_SUMMARY_INSTRUCTIONS: &str = "Produce a concise branch summary of the conversation below. Capture the goal of this branch, what was accomplished, and the most recent state so a sibling branch can pick up without replaying every message.";

#[derive(Clone, Debug)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Usage,
}

pub async fn summarize_branch(
    model: Model,
    entries: &[SessionTreeEntry],
    stream_fn: Option<StreamFn>,
    cancel: CancellationToken,
) -> Result<BranchSummaryResult, SummarizeError> {
    let messages: Vec<AgentMessage> = entries
        .iter()
        .filter_map(|e| match e {
            SessionTreeEntry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    if messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: String::new(),
            usage: Usage::default(),
        });
    }
    let out = generate_summary(
        GenerateSummaryRequest {
            model,
            messages,
            custom_instructions: Some(BRANCH_SUMMARY_INSTRUCTIONS.to_string()),
            prompt_budget_tokens: None,
            max_output_tokens: None,
            stream_fn,
        },
        cancel,
    )
    .await?;
    Ok(BranchSummaryResult {
        summary: out.summary,
        usage: out.usage,
    })
}
