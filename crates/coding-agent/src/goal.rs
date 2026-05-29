//! Session-level `/goal` stop hook.
//!
//! A goal is stored as append-only session metadata, then evaluated after each successful
//! model turn. The evaluator is a separate model call with no tools and only a bounded text
//! transcript. It returns structured JSON; missing evidence defaults to "not done".

use std::sync::{Arc, OnceLock};

use pie_agent_core::{
    AgentHarness, AgentMessage, EvaluatorError, OnTurnEndContext, OnTurnEndHook, SessionTreeEntry,
    ThinkingLevel, TurnEndAction, TurnEndDecision,
};
use pie_ai::{ContentBlock, Message, UserContent, UserContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const CUSTOM_TYPE: &str = "goal_state";
const TRANSCRIPT_CHAR_LIMIT: usize = 40_000;
pub const MAX_CONTINUATIONS: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Pursuing,
    Paused,
    Achieved,
    BudgetLimited,
    Cleared,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pursuing => "pursuing",
            Self::Paused => "paused",
            Self::Achieved => "achieved",
            Self::BudgetLimited => "budget_limited",
            Self::Cleared => "cleared",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoalState {
    pub condition: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub iterations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reason: Option<String>,
    pub updated_at: String,
}

impl GoalState {
    pub fn active(&self) -> bool {
        matches!(
            self.status,
            GoalStatus::Pursuing | GoalStatus::Paused | GoalStatus::BudgetLimited
        )
    }
}

#[derive(Debug, Deserialize)]
struct EvaluatorDecision {
    ok: bool,
    reason: String,
}

pub async fn current(harness: &Arc<AgentHarness>) -> Option<GoalState> {
    latest_from_entries(&harness.session().entries().await.ok()?)
        .filter(|state| !matches!(state.status, GoalStatus::Cleared))
}

pub async fn set(harness: &Arc<AgentHarness>, condition: String) -> Result<GoalState, String> {
    let state = GoalState {
        condition,
        status: GoalStatus::Pursuing,
        iterations: 0,
        last_reason: None,
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    append_state(harness, &state).await?;
    Ok(state)
}

pub async fn pause(harness: &Arc<AgentHarness>) -> Result<GoalState, String> {
    let mut state = current(harness)
        .await
        .ok_or_else(|| "no active goal; set one with /goal <condition>".to_string())?;
    state.status = GoalStatus::Paused;
    state.updated_at = chrono::Utc::now().to_rfc3339();
    append_state(harness, &state).await?;
    Ok(state)
}

pub async fn resume(harness: &Arc<AgentHarness>) -> Result<GoalState, String> {
    let mut state = current(harness)
        .await
        .ok_or_else(|| "no paused goal; set one with /goal <condition>".to_string())?;
    if !matches!(state.status, GoalStatus::Paused | GoalStatus::BudgetLimited) {
        return Err("goal is not paused".into());
    }
    state.status = GoalStatus::Pursuing;
    state.updated_at = chrono::Utc::now().to_rfc3339();
    append_state(harness, &state).await?;
    Ok(state)
}

pub async fn clear(harness: &Arc<AgentHarness>) -> Result<GoalState, String> {
    let mut state = current(harness).await.unwrap_or_else(|| GoalState {
        condition: String::new(),
        status: GoalStatus::Cleared,
        iterations: 0,
        last_reason: None,
        updated_at: chrono::Utc::now().to_rfc3339(),
    });
    state.status = GoalStatus::Cleared;
    state.updated_at = chrono::Utc::now().to_rfc3339();
    append_state(harness, &state).await?;
    Ok(state)
}

async fn append_state(harness: &Arc<AgentHarness>, state: &GoalState) -> Result<(), String> {
    harness
        .session()
        .append_custom(CUSTOM_TYPE, Some(json!(state)))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn latest_from_entries(entries: &[SessionTreeEntry]) -> Option<GoalState> {
    entries.iter().rev().find_map(|entry| {
        let SessionTreeEntry::Custom {
            custom_type, data, ..
        } = entry
        else {
            return None;
        };
        if custom_type != CUSTOM_TYPE {
            return None;
        }
        serde_json::from_value(data.clone()?).ok()
    })
}

fn transcript_from_messages(messages: &[AgentMessage], max_chars: usize) -> String {
    let mut lines = Vec::new();
    for message in messages {
        if let Some(line) = agent_message_text(message) {
            lines.push(line);
        }
    }
    let text = lines.join("\n\n");
    tail_chars(&text, max_chars)
}

fn agent_message_text(message: &AgentMessage) -> Option<String> {
    let AgentMessage::Llm(message) = message else {
        return None;
    };
    match message {
        Message::User(user) => Some(format!("User: {}", user_content_text(&user.content))),
        Message::Assistant(assistant) => {
            let text = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.text.as_str()),
                    ContentBlock::Thinking(t) => Some(t.thinking.as_str()),
                    ContentBlock::ToolCall(t) => Some(t.name.as_str()),
                    ContentBlock::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                None
            } else {
                Some(format!("Assistant: {text}"))
            }
        }
        Message::ToolResult(result) => {
            let text = result
                .content
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Text(t) => Some(t.text.as_str()),
                    UserContentBlock::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                None
            } else {
                Some(format!(
                    "ToolResult({} error={}): {text}",
                    result.tool_name, result.is_error
                ))
            }
        }
    }
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContentBlock::Text(t) => t.text.as_str(),
                UserContentBlock::Image(_) => "[image]",
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Build the runtime stop hook used by `/goal`.
///
/// The harness owns hook execution, but the hook itself needs a handle back to the live
/// harness so it can read goal state, run a tool-less evaluator, and persist the updated
/// goal state. `main.rs` fills the cell immediately after constructing the harness.
pub fn stop_hook(harness_cell: Arc<OnceLock<Arc<AgentHarness>>>) -> OnTurnEndHook {
    Arc::new(move |ctx, cancel| {
        let harness_cell = harness_cell.clone();
        Box::pin(async move {
            let Some(harness) = harness_cell.get().cloned() else {
                return TurnEndDecision::from(TurnEndAction::Pause {
                    reason: "goal hook was not initialized".into(),
                });
            };
            evaluate_stop_hook(harness, ctx, cancel).await
        })
    })
}

async fn evaluate_stop_hook(
    harness: Arc<AgentHarness>,
    ctx: OnTurnEndContext,
    cancel: tokio_util::sync::CancellationToken,
) -> TurnEndDecision {
    let Some(mut state) = current(&harness).await else {
        return TurnEndDecision::from(TurnEndAction::Noop);
    };
    if state.status != GoalStatus::Pursuing {
        return TurnEndDecision::from(TurnEndAction::Noop);
    }

    let transcript = transcript_from_messages(&ctx.transcript, TRANSCRIPT_CHAR_LIMIT);
    let model = {
        let agent_state = harness.agent().state();
        agent_state.model.clone()
    };
    let model = match model {
        Some(model) => model,
        None => {
            let reason = "goal evaluator has no current model".to_string();
            persist_pause(&harness, &mut state, reason.clone()).await;
            return pause_decision(reason, &state);
        }
    };

    let output = match harness
        .run_evaluator(
            evaluator_system_prompt().to_string(),
            evaluator_user_prompt(&state.condition, &transcript),
            model,
            ThinkingLevel::Off,
            cancel,
        )
        .await
    {
        Ok(output) => output,
        Err(EvaluatorError::Cancelled) => {
            let reason = "goal evaluator cancelled".to_string();
            persist_pause(&harness, &mut state, reason.clone()).await;
            return pause_decision(reason, &state);
        }
        Err(e) => {
            let reason = format!("goal evaluator failed: {e}");
            persist_pause(&harness, &mut state, reason.clone()).await;
            return pause_decision(reason, &state);
        }
    };
    let Some(text) = output.last_assistant_text else {
        let reason = "goal evaluator returned no text".to_string();
        persist_pause(&harness, &mut state, reason.clone()).await;
        return pause_decision(reason, &state);
    };
    let decision = match parse_decision(&text) {
        Ok(decision) => decision,
        Err(reason) => {
            let reason = format!("goal evaluator failed: {reason}");
            persist_pause(&harness, &mut state, reason.clone()).await;
            return pause_decision(reason, &state);
        }
    };

    state.iterations = state.iterations.saturating_add(1);
    state.last_reason = Some(decision.reason.clone());
    state.updated_at = chrono::Utc::now().to_rfc3339();

    if decision.ok {
        state.status = GoalStatus::Achieved;
        persist_state_best_effort(&harness, &state).await;
        return TurnEndDecision {
            action: TurnEndAction::Stop,
            payload: Some(goal_payload(&state, Some(true))),
        };
    }

    if state.iterations >= MAX_CONTINUATIONS {
        state.status = GoalStatus::BudgetLimited;
        persist_state_best_effort(&harness, &state).await;
        return TurnEndDecision {
            action: TurnEndAction::Pause {
                reason: format!(
                    "goal continuation limit reached ({MAX_CONTINUATIONS}); resume with /goal resume"
                ),
            },
            payload: Some(goal_payload(&state, Some(false))),
        };
    }

    persist_state_best_effort(&harness, &state).await;
    TurnEndDecision {
        action: TurnEndAction::Continue {
            prompt: continuation_prompt(&state.condition, &decision.reason),
        },
        payload: Some(goal_payload(&state, Some(false))),
    }
}

async fn persist_pause(harness: &Arc<AgentHarness>, state: &mut GoalState, reason: String) {
    state.status = GoalStatus::Paused;
    state.last_reason = Some(reason);
    state.updated_at = chrono::Utc::now().to_rfc3339();
    persist_state_best_effort(harness, state).await;
}

async fn persist_state_best_effort(harness: &Arc<AgentHarness>, state: &GoalState) {
    if let Err(e) = append_state(harness, state).await {
        tracing::warn!("persist goal state failed: {e}");
    }
}

fn pause_decision(reason: String, state: &GoalState) -> TurnEndDecision {
    TurnEndDecision {
        action: TurnEndAction::Pause { reason },
        payload: Some(goal_payload(state, None)),
    }
}

fn goal_payload(state: &GoalState, ok: Option<bool>) -> serde_json::Value {
    json!({
        "goal_status": state.status.as_str(),
        "condition": state.condition,
        "ok": ok,
        "reason": state.last_reason,
        "iterations": state.iterations,
        "max_continuations": MAX_CONTINUATIONS,
        "updated_at": state.updated_at,
    })
}

fn evaluator_user_prompt(condition: &str, transcript: &str) -> String {
    format!("Goal condition:\n{condition}\n\nConversation transcript:\n{transcript}")
}

fn evaluator_system_prompt() -> &'static str {
    r#"You are evaluating a stop-condition hook in pie.
Read the conversation transcript carefully, then judge whether the user-provided condition is satisfied.
You cannot call tools. Only use explicit evidence in the transcript.
Your response must be a JSON object with one of these shapes:
{"ok": true, "reason": "<quote evidence from the transcript that satisfies the condition>"}
{"ok": false, "reason": "<quote what is missing or what blocks the condition>"}
Always include a reason field, quoting specific text from the transcript whenever possible.
If the transcript does not contain clear evidence that the condition is satisfied, return {"ok": false, "reason": "insufficient evidence in transcript"}."#
}

fn parse_decision(text: &str) -> Result<EvaluatorDecision, String> {
    let trimmed = text.trim();
    let parsed = serde_json::from_str::<EvaluatorDecision>(trimmed)
        .or_else(|_| {
            let start = trimmed.find('{').ok_or(())?;
            let end = trimmed.rfind('}').ok_or(())?;
            serde_json::from_str::<EvaluatorDecision>(&trimmed[start..=end]).map_err(|_| ())
        })
        .map_err(|_| {
            format!(
                "goal evaluator returned invalid JSON: {}",
                tail_chars(trimmed, 300)
            )
        })?;
    if parsed.reason.trim().is_empty() {
        return Err("goal evaluator returned an empty reason".into());
    }
    Ok(parsed)
}

fn continuation_prompt(condition: &str, reason: &str) -> String {
    format!(
        "The current /goal is not satisfied yet.\n\nGoal condition:\n{condition}\n\nGoal evaluator says what is missing or blocking completion:\n{reason}\n\nContinue working toward the goal. Do not claim completion until the transcript contains explicit evidence that satisfies the condition."
    )
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let tail = text
        .chars()
        .skip(count.saturating_sub(max_chars))
        .collect::<String>();
    format!("[transcript truncated to last {max_chars} chars]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_decision_inside_text() {
        let decision = parse_decision("```json\n{\"ok\":false,\"reason\":\"missing tests\"}\n```")
            .expect("decision");
        assert!(!decision.ok);
        assert_eq!(decision.reason, "missing tests");
    }

    #[test]
    fn transcript_tail_is_bounded() {
        let text = tail_chars("abcdef", 3);
        assert!(text.contains("def"));
        assert!(!text.ends_with("abcdef"));
    }
}
