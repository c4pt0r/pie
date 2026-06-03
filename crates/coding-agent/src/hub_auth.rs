//! Typed contract for the built-in `pie.0xfefe.me` join flow.
//!
//! The structs here mirror the Worker `/auth/start` and `/auth/exchange_code`
//! wire shapes. The join implementation uses these instead of ad-hoc JSON so
//! UI code can test the no-token/no-code redaction boundary without parsing raw
//! HTTP or MCP frames.

use serde::{Deserialize, Serialize};

pub const HUB_SERVER_NAME: &str = "pie-hub";
pub const HUB_TOKEN_REF: &str = "pie-hub:default";
pub const HUB_DEFAULT_MCP_ENDPOINT: &str = "https://pie.0xfefe.me/mcp";
pub const HUB_DEFAULT_AUTH_ORIGIN: &str = "https://pie.0xfefe.me";
pub const HUB_AUTH_CLIENT_KIND: &str = "pie-cli";
pub const HUB_AUTH_CODE_CHALLENGE_METHOD: &str = "S256";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthStartRequest {
    pub client_kind: String,
    pub client_version: String,
    pub loopback_redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub state: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthStartResponse {
    pub exchange_request_id: String,
    pub login_url: String,
    pub expires_in_seconds: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthExchangeCodeRequest {
    pub exchange_request_id: String,
    pub code: String,
    pub state: String,
    pub code_verifier: String,
}

/// Manual paste-code completion for the `/auth/exchange_manual_code` Worker route.
///
/// Used when the loopback callback cannot reach the running pie (SSH / remote TTY):
/// the human signs in on their own browser, the hub shows a short one-time code, and
/// the user pastes it back into pie.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthExchangeManualCodeRequest {
    pub exchange_request_id: String,
    pub manual_code: String,
    pub state: String,
    pub code_verifier: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthExchangeCodeResponse {
    pub agent_id: String,
    pub handle: String,
    pub namespace: String,
    pub hub_token: String,
    pub expires_at: Option<String>,
    pub profile: HubAuthProfile,
    pub visibility: HubAuthVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthProfile {
    pub display_name: String,
    pub description: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubAuthVisibility {
    pub discoverable: HubDiscoverable,
    pub inbox: HubInbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubDiscoverable {
    Public,
    Namespace,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubInbox {
    Open,
    Namespace,
    Invited,
    Closed,
}

#[derive(Clone, Default)]
pub struct HubAuthSecretFragments<'a> {
    pub hub_token: Option<&'a str>,
    pub code: Option<&'a str>,
    pub manual_code: Option<&'a str>,
    pub state: Option<&'a str>,
    pub code_verifier: Option<&'a str>,
    pub loopback_redirect_uri: Option<&'a str>,
    pub login_url: Option<&'a str>,
}

impl<'a> HubAuthSecretFragments<'a> {
    pub fn iter(&self) -> impl Iterator<Item = &'a str> + '_ {
        [
            self.hub_token,
            self.code,
            self.manual_code,
            self.state,
            self.code_verifier,
            self.loopback_redirect_uri,
            self.login_url,
        ]
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
    }

    pub fn assert_absent_from(&self, label: &str, text: &str) {
        for secret in self.iter() {
            assert!(
                !text.contains(secret),
                "{label} leaked a hub auth secret fragment"
            );
        }
    }

    pub fn is_absent_from(&self, text: &str) -> bool {
        self.iter().all(|secret| !text.contains(secret))
    }
}
