//! MCP server configuration loader. Reads `~/.pie/mcp.toml` (and `<cwd>/.pie/mcp.toml`),
//! spawns each configured stdio server, runs the initialize+tools/list handshake, and
//! returns the resulting AgentTool list ready to append to `default_tools()`.
//!
//! Failure is non-fatal at the load level: a server that fails to start emits a startup
//! diagnostic and is skipped. The agent runs without it.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use pie_agent_core::AgentTool;
use pie_mcp::{
    HttpMcpAuth, HttpMcpTransport, HttpMcpTransportOptions, McpClient, ReconnectPolicy,
    StdioTransport,
};
use serde::{Deserialize, Serialize};

use crate::auth::AuthStore;
use crate::config::base_dir;
use crate::tools::mcp_adapter::McpAgentTool;
use crate::triggers::McpNotificationHook;

pub const BUILT_IN_HUB_SERVER_NAME: &str = "pie-hub";
pub const BUILT_IN_HUB_TOKEN_REF: &str = "pie-hub:default";
pub const BUILT_IN_HUB_ENDPOINT: &str = "https://pie.0xfefe.me/mcp";

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(default)]
    pub server: Vec<ServerConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub kind: ServerKind,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub endpoint: Option<String>,
    pub auth: Option<HttpAuthConfig>,
    pub request_timeout_ms: Option<u64>,
    pub sse_idle_timeout_ms: Option<u64>,
    pub body_cap_bytes: Option<usize>,
    pub reconnect: Option<ReconnectConfig>,
    /// Treat this server as a pure notification feed: its pushed `payload_summary` is
    /// injected straight into the parent chat (no sub-agent, no model call) instead of
    /// dispatching the dynamic-rule sub-agent. Off by default. See
    /// `triggers::direct_inject_action_hook`.
    #[serde(default)]
    pub inject_summary: bool,
    /// Like `inject_summary`, but additionally run ONE model turn in the parent's full
    /// context so the agent reacts to the notification. Off by default; wins over
    /// `inject_summary` if both are set. Authority note: this lets a trusted source's data
    /// wake the main agent (with tools + history) — opt in per server only.
    #[serde(default)]
    pub inject_and_run: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerKind {
    #[default]
    Stdio,
    StreamableHttp,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HttpAuthConfig {
    pub kind: String,
    pub token_keychain_ref: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReconnectConfig {
    pub initial_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub max_attempts: Option<usize>,
}

/// Output of loading. Holds tools (to register with the agent), diagnostics (startup
/// failures to print to the user), and notification hooks (one per MCP server that
/// successfully connected — the caller is expected to register each with
/// `AgentHarness::register_notification_hook` once the harness is built so MCP server
/// pushes drive the runtime trigger pipeline).
pub struct LoadedMcp {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub diagnostics: Vec<String>,
    pub client_count: usize,
    pub notification_hooks: Vec<Arc<McpNotificationHook>>,
    /// Names of servers configured with `inject_summary = true`. The caller wires these into
    /// `triggers::direct_inject_action_hook` so their pushes bypass the sub-agent.
    pub inject_summary_servers: std::collections::HashSet<String>,
    /// Names of servers configured with `inject_and_run = true` — injected summary plus one
    /// model turn in the parent context.
    pub inject_and_run_servers: std::collections::HashSet<String>,
}

/// Load and connect every MCP server from the project + user configs. Project entries with
/// the same `name` as a user entry override.
pub async fn load_all(cwd: &Path) -> LoadedMcp {
    let mut diagnostics = Vec::new();
    let project_path = cwd.join(".pie").join("mcp.toml");
    let user_path = base_dir().join("mcp.toml");

    let mut configs: Vec<ServerConfig> = Vec::new();
    for (path, label) in [(&user_path, "user"), (&project_path, "project")] {
        if let Some(cfg) = read_config(path, &mut diagnostics, label).await {
            for s in cfg.server {
                if let Some(i) = configs.iter().position(|x| x.name == s.name) {
                    configs[i] = s;
                } else {
                    configs.push(s);
                }
            }
        }
    }
    add_built_in_hub_if_ready(&mut configs);

    let inject_summary_servers: std::collections::HashSet<String> = configs
        .iter()
        .filter(|c| c.inject_summary)
        .map(|c| c.name.clone())
        .collect();
    let inject_and_run_servers: std::collections::HashSet<String> = configs
        .iter()
        .filter(|c| c.inject_and_run)
        .map(|c| c.name.clone())
        .collect();

    let (tools, notification_hooks, connect_diagnostics, client_count) =
        connect_all(&configs).await;
    diagnostics.extend(connect_diagnostics);
    LoadedMcp {
        tools,
        diagnostics,
        client_count,
        notification_hooks,
        inject_summary_servers,
        inject_and_run_servers,
    }
}

fn built_in_hub_server() -> ServerConfig {
    ServerConfig {
        name: BUILT_IN_HUB_SERVER_NAME.into(),
        kind: ServerKind::StreamableHttp,
        command: None,
        args: Vec::new(),
        endpoint: Some(BUILT_IN_HUB_ENDPOINT.into()),
        auth: Some(HttpAuthConfig {
            kind: "bearer".into(),
            token_keychain_ref: Some(BUILT_IN_HUB_TOKEN_REF.into()),
        }),
        request_timeout_ms: None,
        sse_idle_timeout_ms: None,
        body_cap_bytes: None,
        reconnect: None,
        inject_summary: false,
        inject_and_run: false,
    }
}

fn add_built_in_hub_if_ready(configs: &mut Vec<ServerConfig>) {
    if configs
        .iter()
        .any(|server| server.name == BUILT_IN_HUB_SERVER_NAME)
    {
        return;
    }
    if AuthStore::load()
        .ok()
        .and_then(|store| store.resolve_for_provider(BUILT_IN_HUB_TOKEN_REF))
        .is_some()
    {
        configs.push(built_in_hub_server());
    }
}

/// Connect to each configured server. Returns the tools collected, the
/// `McpNotificationHook` per successful connection, per-server failure diagnostics, and
/// the number of servers that actually connected.
///
/// `client_count` reports **successful** connections, not attempted ones. The TUI startup
/// banner prints "connected to N server(s)" using this field; previously it reported
/// `configs.len()`, so the user saw "connected to 3" alongside two error diagnostics when
/// 2 of 3 servers failed to start. See code-review item #9 (2026-05-22).
async fn connect_all(
    configs: &[ServerConfig],
) -> (
    Vec<Arc<dyn AgentTool>>,
    Vec<Arc<McpNotificationHook>>,
    Vec<String>,
    usize,
) {
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut notification_hooks: Vec<Arc<McpNotificationHook>> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    let mut client_count = 0usize;
    for s in configs.iter() {
        match connect_one(s).await {
            Ok((server_tools, hook)) => {
                tools.extend(server_tools);
                notification_hooks.push(hook);
                client_count += 1;
            }
            Err(e) => {
                diagnostics.push(format!("mcp server '{}' failed: {e}", s.name));
            }
        }
    }
    (tools, notification_hooks, diagnostics, client_count)
}

async fn read_config(path: &Path, diagnostics: &mut Vec<String>, label: &str) -> Option<McpConfig> {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return None;
    }
    match tokio::fs::read_to_string(path).await {
        Ok(text) => match toml::from_str::<McpConfig>(&text) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                diagnostics.push(format!(
                    "mcp config ({label}, {}): parse failed: {e}",
                    path.display()
                ));
                None
            }
        },
        Err(e) => {
            diagnostics.push(format!(
                "mcp config ({label}, {}): read failed: {e}",
                path.display()
            ));
            None
        }
    }
}

async fn connect_one(
    s: &ServerConfig,
) -> Result<(Vec<Arc<dyn AgentTool>>, Arc<McpNotificationHook>)> {
    let client = match s.kind {
        ServerKind::Stdio => connect_stdio(s).await?,
        ServerKind::StreamableHttp => connect_streamable_http(s).await?,
    };
    client.initialize("pie-coding-agent").await?;
    // Take the server-push notification receiver before any other consumer can claim it.
    // `take_notifications` returns `Some` exactly once per client; subsequent callers (and
    // an unconsumed channel for a long-running session) would silently buffer frames, so
    // the only correct moment is here, immediately after `initialize`. If the receiver is
    // already taken something invariant has been violated — we fail spawn rather than
    // silently disconnect the trigger surface.
    let rx = client.take_notifications().ok_or_else(|| {
        anyhow::anyhow!("McpClient::take_notifications returned None — receiver already consumed")
    })?;
    let hook = Arc::new(McpNotificationHook::new(s.name.clone(), rx));

    let tools = client.tools_list().await?;
    let mut out: Vec<Arc<dyn AgentTool>> = Vec::with_capacity(tools.len());
    for tool in &tools {
        let adapter = McpAgentTool::new(client.clone(), tool);
        out.push(Arc::new(adapter));
    }
    Ok((out, hook))
}

async fn connect_stdio(s: &ServerConfig) -> Result<Arc<McpClient>> {
    if s.endpoint.is_some() || s.auth.is_some() {
        anyhow::bail!(
            "stdio MCP server '{}' must not set endpoint or auth; remove streamable_http fields",
            s.name
        );
    }
    let command = s
        .command
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("stdio MCP server '{}' missing command", s.name))?;
    let args: Vec<&str> = s.args.iter().map(String::as_str).collect();
    let transport = StdioTransport::spawn(command, &args).await?;
    let client = Arc::new(McpClient::new(Arc::new(transport)));
    Ok(client)
}

async fn connect_streamable_http(s: &ServerConfig) -> Result<Arc<McpClient>> {
    validate_official_hub_scope(s)?;
    if s.command.is_some() || !s.args.is_empty() {
        anyhow::bail!(
            "streamable_http MCP server '{}' must set endpoint, not command/args",
            s.name
        );
    }
    let endpoint = s
        .endpoint
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("streamable_http MCP server '{}' missing endpoint", s.name))?
        .clone();
    let mut opts = HttpMcpTransportOptions::new(endpoint);
    opts.auth = resolve_http_auth(s.auth.as_ref())?;
    if let Some(ms) = s.request_timeout_ms {
        if ms == 0 {
            anyhow::bail!(
                "streamable_http MCP server '{}' request_timeout_ms must be positive",
                s.name
            );
        }
        opts.request_timeout = std::time::Duration::from_millis(ms);
    }
    if let Some(ms) = s.sse_idle_timeout_ms {
        if ms == 0 {
            anyhow::bail!(
                "streamable_http MCP server '{}' sse_idle_timeout_ms must be positive",
                s.name
            );
        }
        opts.sse_idle_timeout = std::time::Duration::from_millis(ms);
    }
    if let Some(cap) = s.body_cap_bytes {
        if cap == 0 {
            anyhow::bail!(
                "streamable_http MCP server '{}' body_cap_bytes must be positive",
                s.name
            );
        }
        opts.body_cap_bytes = cap;
    }
    if let Some(reconnect) = &s.reconnect {
        if reconnect.initial_ms == Some(0) || reconnect.max_ms == Some(0) {
            anyhow::bail!(
                "streamable_http MCP server '{}' reconnect delays must be positive",
                s.name
            );
        }
        opts.reconnect_policy = ReconnectPolicy {
            initial_delay: std::time::Duration::from_millis(reconnect.initial_ms.unwrap_or(500)),
            max_delay: std::time::Duration::from_millis(reconnect.max_ms.unwrap_or(30_000)),
            max_attempts: reconnect.max_attempts,
        };
    }
    let transport = HttpMcpTransport::connect(opts)?;
    Ok(Arc::new(McpClient::new(Arc::new(transport))))
}

fn validate_official_hub_scope(s: &ServerConfig) -> Result<()> {
    let uses_official_name = s.name == BUILT_IN_HUB_SERVER_NAME;
    let uses_official_endpoint = s.endpoint.as_deref() == Some(BUILT_IN_HUB_ENDPOINT);
    let uses_official_credential = s.auth.as_ref().is_some_and(|auth| {
        auth.kind == "bearer" && auth.token_keychain_ref.as_deref() == Some(BUILT_IN_HUB_TOKEN_REF)
    });
    if !uses_official_name && !uses_official_credential {
        return Ok(());
    }
    if !uses_official_name || !uses_official_endpoint || !uses_official_credential {
        anyhow::bail!(
            "official pie-hub MCP credential is reserved for pie.0xfefe.me; custom or staging hubs must use a different server name and credential"
        );
    }
    Ok(())
}

fn resolve_http_auth(auth: Option<&HttpAuthConfig>) -> Result<HttpMcpAuth> {
    let Some(auth) = auth else {
        return Ok(HttpMcpAuth::None);
    };
    if auth.kind != "bearer" {
        anyhow::bail!("unsupported streamable_http auth kind; expected bearer");
    }
    let token_ref = auth
        .token_keychain_ref
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("bearer auth requires token_keychain_ref"))?;
    let recovery = if token_ref == BUILT_IN_HUB_TOKEN_REF {
        "run /hub join"
    } else {
        "run /login <configured-token-ref>"
    };
    let store = AuthStore::load()
        .map_err(|e| anyhow::anyhow!("failed to load local credential store: {e}; {recovery}"))?;
    let token = store
        .resolve_for_provider(token_ref)
        .ok_or_else(|| anyhow::anyhow!("configured bearer credential was not found; {recovery}"))?;
    Ok(HttpMcpAuth::Bearer { token })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two configured servers both fail to start (executable does not exist). Verify
    /// `client_count` reports 0 (not 2), and each failure surfaces a diagnostic. Pinned
    /// behavior for code-review item #9: the TUI startup banner reads from this field.
    #[tokio::test]
    async fn client_count_reflects_successful_connections_not_attempts() {
        let configs = vec![
            ServerConfig {
                name: "broken-a".into(),
                kind: ServerKind::Stdio,
                command: Some("/definitely/not/a/real/path/for/mcp/test-a".into()),
                args: vec![],
                endpoint: None,
                auth: None,
                request_timeout_ms: None,
                sse_idle_timeout_ms: None,
                body_cap_bytes: None,
                reconnect: None,
                inject_summary: false,
                inject_and_run: false,
            },
            ServerConfig {
                name: "broken-b".into(),
                kind: ServerKind::Stdio,
                command: Some("/definitely/not/a/real/path/for/mcp/test-b".into()),
                args: vec![],
                endpoint: None,
                auth: None,
                request_timeout_ms: None,
                sse_idle_timeout_ms: None,
                body_cap_bytes: None,
                reconnect: None,
                inject_summary: false,
                inject_and_run: false,
            },
        ];
        let (tools, hooks, diagnostics, client_count) = connect_all(&configs).await;
        assert_eq!(client_count, 0, "no server should be reported as connected");
        assert!(tools.is_empty(), "no tools should load from failed servers");
        assert!(
            hooks.is_empty(),
            "no notification hooks should be created for failed servers"
        );
        assert_eq!(
            diagnostics.len(),
            2,
            "each failed server should emit a diagnostic, got: {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("broken-a")),
            "diagnostic should mention server name 'broken-a': {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.contains("broken-b")),
            "diagnostic should mention server name 'broken-b': {diagnostics:?}"
        );
    }

    /// Empty config list ⇒ zero attempts, zero connections, zero diagnostics. Sanity check
    /// the helper doesn't crash on the empty path.
    #[tokio::test]
    async fn empty_configs_reports_zero() {
        let (tools, hooks, diagnostics, client_count) = connect_all(&[]).await;
        assert!(tools.is_empty());
        assert!(hooks.is_empty());
        assert!(diagnostics.is_empty());
        assert_eq!(client_count, 0);
    }

    #[test]
    fn streamable_http_config_deserializes_with_bearer_ref() {
        let cfg: McpConfig = toml::from_str(
            r#"
[[server]]
name = "pie-hub"
kind = "streamable_http"
endpoint = "https://pie.0xfefe.me/mcp"
auth = { kind = "bearer", token_keychain_ref = "pie-hub:default" }
request_timeout_ms = 30000
sse_idle_timeout_ms = 60000
body_cap_bytes = 1048576
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.len(), 1);
        let server = &cfg.server[0];
        assert_eq!(server.name, "pie-hub");
        assert_eq!(server.kind, ServerKind::StreamableHttp);
        assert_eq!(
            server.endpoint.as_deref(),
            Some("https://pie.0xfefe.me/mcp")
        );
        assert_eq!(
            server
                .auth
                .as_ref()
                .and_then(|auth| auth.token_keychain_ref.as_deref()),
            Some("pie-hub:default")
        );
    }

    #[test]
    fn built_in_hub_added_only_after_credential_exists() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let original = std::env::var_os("PIE_DIR");
        unsafe { std::env::set_var("PIE_DIR", dir.path()) };
        crate::auth::AuthStore::default().save().unwrap();

        let mut configs = Vec::new();
        add_built_in_hub_if_ready(&mut configs);
        assert!(
            configs.is_empty(),
            "clean installs should not emit a missing-token startup diagnostic"
        );

        let mut store = crate::auth::AuthStore::default();
        store.set(
            BUILT_IN_HUB_TOKEN_REF,
            crate::auth::ProviderCredential::ApiKey {
                value: "hub_agent_test_token_should_not_leak".into(),
            },
        );
        store.save().unwrap();

        add_built_in_hub_if_ready(&mut configs);
        assert_eq!(configs.len(), 1);
        let server = &configs[0];
        assert_eq!(server.name, BUILT_IN_HUB_SERVER_NAME);
        assert_eq!(server.kind, ServerKind::StreamableHttp);
        assert_eq!(server.endpoint.as_deref(), Some(BUILT_IN_HUB_ENDPOINT));
        assert_eq!(
            server
                .auth
                .as_ref()
                .and_then(|auth| auth.token_keychain_ref.as_deref()),
            Some(BUILT_IN_HUB_TOKEN_REF)
        );

        match original {
            Some(value) => unsafe { std::env::set_var("PIE_DIR", value) },
            None => unsafe { std::env::remove_var("PIE_DIR") },
        }
    }

    #[test]
    fn official_hub_config_prevents_built_in_duplicate() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let original = std::env::var_os("PIE_DIR");
        unsafe { std::env::set_var("PIE_DIR", dir.path()) };
        let mut store = crate::auth::AuthStore::default();
        store.set(
            BUILT_IN_HUB_TOKEN_REF,
            crate::auth::ProviderCredential::ApiKey {
                value: "hub_agent_test_token_should_not_leak".into(),
            },
        );
        store.save().unwrap();

        let mut configs = vec![ServerConfig {
            name: BUILT_IN_HUB_SERVER_NAME.into(),
            kind: ServerKind::StreamableHttp,
            command: None,
            args: Vec::new(),
            endpoint: Some(BUILT_IN_HUB_ENDPOINT.into()),
            auth: Some(HttpAuthConfig {
                kind: "bearer".into(),
                token_keychain_ref: Some(BUILT_IN_HUB_TOKEN_REF.into()),
            }),
            request_timeout_ms: None,
            sse_idle_timeout_ms: None,
            body_cap_bytes: None,
            reconnect: None,
            inject_summary: false,
            inject_and_run: false,
        }];
        add_built_in_hub_if_ready(&mut configs);
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].endpoint.as_deref(), Some(BUILT_IN_HUB_ENDPOINT));

        match original {
            Some(value) => unsafe { std::env::set_var("PIE_DIR", value) },
            None => unsafe { std::env::remove_var("PIE_DIR") },
        }
    }

    #[tokio::test]
    async fn official_hub_scope_rejects_custom_endpoint_or_credential_scope() {
        let staging = ServerConfig {
            name: BUILT_IN_HUB_SERVER_NAME.into(),
            kind: ServerKind::StreamableHttp,
            command: None,
            args: Vec::new(),
            endpoint: Some("https://staging.0xfefe.me/mcp".into()),
            auth: Some(HttpAuthConfig {
                kind: "bearer".into(),
                token_keychain_ref: Some(BUILT_IN_HUB_TOKEN_REF.into()),
            }),
            request_timeout_ms: None,
            sse_idle_timeout_ms: None,
            body_cap_bytes: None,
            reconnect: None,
            inject_summary: false,
            inject_and_run: false,
        };
        let err = match connect_streamable_http(&staging).await {
            Ok(_) => panic!("staging endpoint cannot reuse official pie-hub scope"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("custom or staging hubs must use a different server name"),
            "{err}"
        );
        assert!(!err.contains(BUILT_IN_HUB_TOKEN_REF), "{err}");

        let wrong_credential = ServerConfig {
            endpoint: Some(BUILT_IN_HUB_ENDPOINT.into()),
            auth: Some(HttpAuthConfig {
                kind: "bearer".into(),
                token_keychain_ref: Some("pie-hub-staging:default".into()),
            }),
            ..staging
        };
        let err = match connect_streamable_http(&wrong_credential).await {
            Ok(_) => panic!("official endpoint cannot reuse pie-hub name with another credential"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("custom or staging hubs must use a different server name"),
            "{err}"
        );
        assert!(!err.contains("pie-hub-staging:default"), "{err}");

        let custom_name_with_official_credential = ServerConfig {
            name: "pie-hub-staging".into(),
            endpoint: Some("https://staging.0xfefe.me/mcp".into()),
            auth: Some(HttpAuthConfig {
                kind: "bearer".into(),
                token_keychain_ref: Some(BUILT_IN_HUB_TOKEN_REF.into()),
            }),
            ..wrong_credential
        };
        let err = match connect_streamable_http(&custom_name_with_official_credential).await {
            Ok(_) => panic!("custom server cannot use official pie-hub credential"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("custom or staging hubs must use a different server name"),
            "{err}"
        );
        assert!(!err.contains(BUILT_IN_HUB_TOKEN_REF), "{err}");
    }

    #[tokio::test]
    async fn streamable_http_rejects_command_args() {
        let server = ServerConfig {
            name: "custom-http".into(),
            kind: ServerKind::StreamableHttp,
            command: Some("node".into()),
            args: vec!["server.js".into()],
            endpoint: Some("https://pie.0xfefe.me/mcp".into()),
            auth: None,
            request_timeout_ms: None,
            sse_idle_timeout_ms: None,
            body_cap_bytes: None,
            reconnect: None,
            inject_summary: false,
            inject_and_run: false,
        };
        let err = match connect_streamable_http(&server).await {
            Ok(_) => panic!("streamable_http with command/args should fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("must set endpoint, not command/args"),
            "{err}"
        );
    }

    #[test]
    fn streamable_http_auth_resolves_from_auth_store_without_debug_leak() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let original = std::env::var_os("PIE_DIR");
        unsafe { std::env::set_var("PIE_DIR", dir.path()) };
        let token = "hub_agent_should_not_leak";
        let mut store = crate::auth::AuthStore::default();
        store.set(
            "pie-hub:default",
            crate::auth::ProviderCredential::ApiKey {
                value: token.into(),
            },
        );
        store.save().unwrap();

        let auth = resolve_http_auth(Some(&HttpAuthConfig {
            kind: "bearer".into(),
            token_keychain_ref: Some("pie-hub:default".into()),
        }))
        .unwrap();
        let debug = format!("{auth:?}");
        assert!(!debug.contains(token), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");

        match original {
            Some(value) => unsafe { std::env::set_var("PIE_DIR", value) },
            None => unsafe { std::env::remove_var("PIE_DIR") },
        }
    }

    #[test]
    fn streamable_http_missing_auth_diagnostic_does_not_echo_token_ref() {
        let _guard = crate::auth::ENV_LOCK.lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let original = std::env::var_os("PIE_DIR");
        unsafe { std::env::set_var("PIE_DIR", dir.path()) };
        crate::auth::AuthStore::default().save().unwrap();

        let secret_like_ref = "hub_agent_should_not_leak";
        let err = resolve_http_auth(Some(&HttpAuthConfig {
            kind: "bearer".into(),
            token_keychain_ref: Some(secret_like_ref.into()),
        }))
        .unwrap_err()
        .to_string();
        assert!(!err.contains(secret_like_ref), "{err}");
        assert!(err.contains("<configured-token-ref>"), "{err}");

        match original {
            Some(value) => unsafe { std::env::set_var("PIE_DIR", value) },
            None => unsafe { std::env::remove_var("PIE_DIR") },
        }
    }
}
