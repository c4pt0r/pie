//! Typed client helpers for the built-in pie hub.
//!
//! Slash commands should not parse raw MCP frames or display transport diagnostics directly.
//! This module owns the MCP call boundary and returns bounded, UI-safe structs for `/hub`
//! commands.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pie_mcp::protocol::ToolContent;
use pie_mcp::{
    HttpMcpTransport, HttpMcpTransportOptions, McpClient, McpToolCallResult, ReconnectPolicy,
};
use serde::Deserialize;
use serde_json::json;

use crate::auth::AuthStore;
use crate::mcp_loader::{BUILT_IN_HUB_ENDPOINT, BUILT_IN_HUB_TOKEN_REF};

const CLIENT_NAME: &str = "pie-cli-hub";
const DEFAULT_LIMIT: usize = 10;
const LOOKUP_LIMIT: usize = 50;

#[derive(Clone)]
pub struct HubClient {
    client: Arc<McpClient>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HubAgentSummary {
    pub agent_id: String,
    pub handle: String,
    pub namespace: String,
    pub display_name: Option<String>,
    #[allow(dead_code)]
    pub capabilities: Vec<String>,
    #[allow(dead_code)]
    pub discoverable: String,
    pub inbox: String,
}

impl HubAgentSummary {
    pub fn mention(&self) -> String {
        format!("@{}@{}", self.handle, self.namespace)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HubInboxItem {
    pub sender: String,
    pub summary: String,
    pub payload_visibility: String,
    pub first_contact_required: bool,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HubSendReceipt {
    pub status: String,
    pub first_contact_required: bool,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    items: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct AgentProfileResponse {
    agent: HubAgentSummary,
}

impl HubClient {
    pub async fn connect_default() -> Result<Self> {
        let token = AuthStore::load()
            .context("load hub credential")?
            .resolve_for_provider(BUILT_IN_HUB_TOKEN_REF)
            .context("hub credential missing; run /hub join")?;
        let endpoint = test_endpoint().unwrap_or_else(|| BUILT_IN_HUB_ENDPOINT.to_string());
        Self::connect(&endpoint, token).await
    }

    pub async fn connect(endpoint: &str, token: String) -> Result<Self> {
        let mut opts = HttpMcpTransportOptions::new(endpoint).bearer(token);
        opts.request_timeout = Duration::from_secs(15);
        opts.sse_idle_timeout = Duration::from_secs(15);
        opts.reconnect_policy = ReconnectPolicy {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(1),
            max_attempts: Some(1),
        };
        let transport = HttpMcpTransport::connect(opts)?;
        let client = Arc::new(McpClient::new(Arc::new(transport)));
        client.initialize(CLIENT_NAME).await?;
        Ok(Self { client })
    }

    pub async fn close(&self) {
        self.client.close().await;
    }

    pub async fn resolve_agent(&self, target: &str) -> Result<HubAgentSummary> {
        if let Some(handle) = parse_mention(target) {
            let response: AgentProfileResponse = self
                .call_json("get_agent_profile", json!({ "agent_handle": handle }))
                .await?;
            return Ok(response.agent);
        }
        if uuid::Uuid::parse_str(target).is_ok() {
            let response: AgentProfileResponse = self
                .call_json("get_agent_profile", json!({ "agent_id": target }))
                .await?;
            return Ok(response.agent);
        }
        bail!("target must be name@namespace");
    }

    pub async fn matching_agents(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<HubAgentSummary>> {
        let needle = prefix.trim_start_matches('@').to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let page: Page<HubAgentSummary> = self
            .call_json("discover_public_agents", json!({ "limit": LOOKUP_LIMIT }))
            .await?;
        let agents = page.items;
        let mut matches = agents
            .into_iter()
            .filter(|agent| {
                let mention = format!("{}@{}", agent.handle, agent.namespace).to_ascii_lowercase();
                mention.starts_with(&needle)
                    || agent.handle.to_ascii_lowercase().starts_with(&needle)
                    || agent.namespace.to_ascii_lowercase().starts_with(&needle)
            })
            .take(clamp_display_limit(limit))
            .collect::<Vec<_>>();
        matches.sort_by_key(|agent| agent.mention());
        Ok(matches)
    }

    pub async fn send_notification(
        &self,
        target_agent_id: &str,
        summary: &str,
    ) -> Result<HubSendReceipt> {
        self.call_json(
            "send_notification",
            json!({
                "target_agent_id": target_agent_id,
                "summary": summary,
                "payload_visibility": "Local",
            }),
        )
        .await
    }

    pub async fn list_inbox(&self, limit: usize) -> Result<Vec<HubInboxItem>> {
        let page: Page<HubInboxItem> = self
            .call_json(
                "list_my_inbox",
                json!({ "limit": clamp_display_limit(limit) }),
            )
            .await?;
        Ok(page.items)
    }

    async fn call_json<T: for<'de> Deserialize<'de>>(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<T> {
        let result = self
            .client
            .tools_call(tool, Some(args), None)
            .await
            .with_context(|| format!("hub tool `{tool}` failed"))?;
        parse_tool_json(tool, result)
    }
}

fn parse_tool_json<T: for<'de> Deserialize<'de>>(
    tool: &str,
    result: McpToolCallResult,
) -> Result<T> {
    if result.is_error {
        bail!("hub tool `{tool}` returned an error");
    }
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .context("hub response did not include text content")?;
    serde_json::from_str(text).with_context(|| format!("decode hub `{tool}` response"))
}

pub fn parse_mention(input: &str) -> Option<String> {
    let mention = parse_mention_parts(input)?;
    Some(format!("@{}@{}", mention.handle, mention.namespace))
}

pub fn display_mention(input: &str) -> Option<String> {
    let mention = parse_mention_parts(input)?;
    Some(format!("{}@{}", mention.handle, mention.namespace))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedMention<'a> {
    handle: &'a str,
    namespace: &'a str,
}

fn parse_mention_parts(input: &str) -> Option<ParsedMention<'_>> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let (handle, namespace) = rest.split_once('@')?;
    if handle.is_empty() || namespace.is_empty() || namespace.contains('@') {
        return None;
    }
    if !(2..=32).contains(&handle.len()) || !(2..=32).contains(&namespace.len()) {
        return None;
    }
    if !is_handle_part(handle) || !is_handle_part(namespace) {
        return None;
    }
    Some(ParsedMention { handle, namespace })
}

fn is_handle_part(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn clamp_display_limit(limit: usize) -> usize {
    limit.clamp(1, DEFAULT_LIMIT)
}

#[cfg(test)]
static TEST_ENDPOINT: once_cell::sync::Lazy<parking_lot::Mutex<Option<String>>> =
    once_cell::sync::Lazy::new(|| parking_lot::Mutex::new(None));

#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct HubClientTestGuard;

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn install_test_endpoint(endpoint: impl Into<String>) -> HubClientTestGuard {
    *TEST_ENDPOINT.lock() = Some(endpoint.into());
    HubClientTestGuard
}

#[cfg(test)]
impl Drop for HubClientTestGuard {
    fn drop(&mut self) {
        *TEST_ENDPOINT.lock() = None;
    }
}

fn test_endpoint() -> Option<String> {
    #[cfg(test)]
    {
        TEST_ENDPOINT.lock().clone()
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_parser_accepts_canonical_handle_namespace() {
        assert_eq!(
            parse_mention("@alice-agent@dongxu"),
            Some("@alice-agent@dongxu".into())
        );
        assert_eq!(parse_mention("alice@dongxu"), Some("@alice@dongxu".into()));
        assert_eq!(
            display_mention("@alice-agent@dongxu"),
            Some("alice-agent@dongxu".into())
        );
        assert_eq!(display_mention("alice@dongxu"), Some("alice@dongxu".into()));
        assert_eq!(parse_mention("@alice"), None);
        assert_eq!(parse_mention("@Alice@dongxu"), None);
    }
}
