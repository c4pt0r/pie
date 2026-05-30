#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::post;
use pie_agent_core::{
    AgentHarness, AgentHarnessOptions, MemorySessionStorage, Session, SessionStorage,
};
use serde_json::json;

#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/bug_report.rs"]
mod bug_report;
#[path = "../src/commands.rs"]
mod commands;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/export.rs"]
mod export;
#[path = "../src/goal.rs"]
mod goal;
#[path = "../src/history.rs"]
mod history;
#[path = "../src/hub_auth.rs"]
mod hub_auth;
#[path = "../src/hub_client.rs"]
mod hub_client;
#[path = "../src/hub_join.rs"]
mod hub_join;
#[path = "../src/mcp_loader.rs"]
mod mcp_loader;
#[path = "../src/session/mod.rs"]
mod session;
#[path = "../src/skills_state.rs"]
mod skills_state;
#[path = "../src/tools/mod.rs"]
mod tools;
#[path = "../src/triggers/mod.rs"]
mod triggers;

static PIE_DIR_ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

struct OutputCapture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl OutputCapture {
    fn install() -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink_lines = lines.clone();
        commands::console::set_sink(Box::new(move |line| {
            sink_lines.lock().unwrap().push(line);
        }));
        Self { lines }
    }

    fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        commands::console::clear_sink();
    }
}

fn faux_model() -> pie_ai::Model {
    pie_ai::Model {
        id: "faux".into(),
        name: "Faux".into(),
        api: pie_ai::Api::from("faux"),
        provider: pie_ai::Provider::from("faux"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![],
        cost: pie_ai::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
        compat: None,
    }
}

#[derive(Default)]
struct FauxHubJoinState {
    start: Option<hub_auth::HubAuthStartRequest>,
    exchange: Option<hub_auth::HubAuthExchangeCodeRequest>,
}

struct FauxHubJoinServer {
    origin: String,
    state: Arc<tokio::sync::Mutex<FauxHubJoinState>>,
}

#[derive(Default)]
struct FauxHubMcpState {
    calls: Vec<serde_json::Value>,
}

struct FauxHubMcpServer {
    endpoint: String,
    state: Arc<tokio::sync::Mutex<FauxHubMcpState>>,
}

impl FauxHubJoinServer {
    async fn start() -> Self {
        let state = Arc::new(tokio::sync::Mutex::new(FauxHubJoinState::default()));
        let app = Router::new()
            .route("/auth/start", post(faux_hub_join_auth_start))
            .route("/auth/exchange_code", post(faux_hub_join_auth_exchange))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { origin, state }
    }
}

async fn faux_hub_join_auth_start(
    State(state): State<Arc<tokio::sync::Mutex<FauxHubJoinState>>>,
    axum::Json(request): axum::Json<hub_auth::HubAuthStartRequest>,
) -> Result<axum::Json<hub_auth::HubAuthStartResponse>, axum::http::StatusCode> {
    let redirect = reqwest::Url::parse(&request.loopback_redirect_uri)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    if redirect.path() != "/callback" {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }
    let callback = format!(
        "{}?code=hub_code_command_join_secret",
        request.loopback_redirect_uri
    );
    state.lock().await.start = Some(request);
    Ok(axum::Json(hub_auth::HubAuthStartResponse {
        exchange_request_id: "command-exchange-request-1".into(),
        login_url: format!("http://127.0.0.1/login?redirect={callback}"),
        expires_in_seconds: 30,
    }))
}

async fn faux_hub_join_auth_exchange(
    State(state): State<Arc<tokio::sync::Mutex<FauxHubJoinState>>>,
    axum::Json(request): axum::Json<hub_auth::HubAuthExchangeCodeRequest>,
) -> axum::Json<hub_auth::HubAuthExchangeCodeResponse> {
    state.lock().await.exchange = Some(request);
    axum::Json(hub_auth::HubAuthExchangeCodeResponse {
        agent_id: "018fe23a-1111-4a22-8b33-123456789abc".into(),
        handle: "alice".into(),
        namespace: "dongxu".into(),
        hub_token: "hub_agent_command_join_secret".into(),
        expires_at: None,
        profile: hub_auth::HubAuthProfile {
            display_name: "alice".into(),
            description: None,
            capabilities: Vec::new(),
        },
        visibility: hub_auth::HubAuthVisibility {
            discoverable: hub_auth::HubDiscoverable::Public,
            inbox: hub_auth::HubInbox::Namespace,
        },
    })
}

async fn drive_faux_hub_join_browser(
    login_url: &str,
    state: Arc<tokio::sync::Mutex<FauxHubJoinState>>,
) -> String {
    let callback_query = {
        let state = state.lock().await;
        let start = state.start.as_ref().expect("captured start request");
        format!("code=hub_code_command_join_secret&state={}", start.state)
    };
    let login = reqwest::Url::parse(login_url).unwrap();
    let redirect_uri = login
        .query_pairs()
        .find_map(|(key, value)| (key == "redirect").then(|| value.into_owned()))
        .expect("faux login URL includes redirect");
    let mut callback = reqwest::Url::parse(&redirect_uri).unwrap();
    callback.set_query(Some(&callback_query));
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
        .get(callback)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.text().await.unwrap()
}

impl FauxHubMcpServer {
    async fn start() -> Self {
        let state = Arc::new(tokio::sync::Mutex::new(FauxHubMcpState::default()));
        let app = Router::new()
            .route("/mcp", post(faux_hub_mcp_post).get(faux_hub_mcp_get))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { endpoint, state }
    }
}

async fn faux_hub_mcp_get(headers: HeaderMap) -> Response {
    if let Some(response) = check_mcp_auth(&headers) {
        return response;
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        "event: keepalive\ndata: {}\n\n",
    )
        .into_response()
}

async fn faux_hub_mcp_post(
    State(state): State<Arc<tokio::sync::Mutex<FauxHubMcpState>>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(response) = check_mcp_auth(&headers) {
        return response;
    }
    let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
    let Some(id) = payload.get("id").cloned() else {
        return axum::http::StatusCode::ACCEPTED.into_response();
    };
    let method = payload.get("method").and_then(|m| m.as_str()).unwrap();
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "serverInfo": {"name": "pie-hub", "version": "test"}
        }),
        "tools/call" => {
            let params = payload["params"].clone();
            state.lock().await.calls.push(params.clone());
            let tool = params["name"].as_str().unwrap();
            let output = match tool {
                "get_agent_profile" => json!({
                    "agent": {
                        "agent_id": "018fe23a-2222-4a22-8b33-123456789abc",
                        "handle": "hub_agent_profile_secret",
                        "namespace": "dongxu",
                        "display_name": "Bob Cheng hub_agent_profile_secret 018fe23a-9999-4a22-8b33-123456789abc xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                        "capabilities": ["notify"],
                        "discoverable": "public",
                        "inbox": "hub_hs_inbox_secret"
                    }
                }),
                "discover_public_agents" => json!({
                    "items": [
                        {
                            "agent_id": "018fe23a-2222-4a22-8b33-123456789abc",
                        "handle": "hub_agent_candidate_secret",
                            "namespace": "dongxu",
                            "display_name": "Bob Cheng hub_agent_candidate_secret 018fe23a-aaaa-4a22-8b33-123456789abc",
                            "capabilities": ["notify"],
                            "discoverable": "public",
                            "inbox": "open"
                        },
                        {
                            "agent_id": "018fe23a-3333-4a22-8b33-123456789abc",
                            "handle": "beth",
                            "namespace": "research",
                            "display_name": "Beth Park",
                            "capabilities": ["notify"],
                            "discoverable": "public",
                            "inbox": "namespace"
                        }
                    ],
                    "next_cursor": null
                }),
                "send_notification" => json!({
                    "notification_id": "018fe23a-4444-4a22-8b33-123456789abc",
                    "status": "hub_agent_status_secret",
                    "first_contact_required": true
                }),
                "list_my_inbox" => json!({
                    "items": [{
                        "notification_id": "018fe23a-5555-4a22-8b33-123456789abc",
                        "sender_agent_id": "018fe23a-6666-4a22-8b33-123456789abc",
                        "sender": "@hub_agent_sender_secret@dongxu",
                        "summary": "hello from alice hub_agent_summary_secret 018fe23a-bbbb-4a22-8b33-123456789abc",
                        "payload_visibility": "hub_hs_payload_secret",
                        "first_contact_required": true,
                        "status": "hub_agent_inbox_status_secret",
                        "created_at": "hub_agent_time_secret",
                        "delivered_at": null
                    }],
                    "next_cursor": null
                }),
                other => panic!("unexpected tool {other}"),
            };
            json!({"content": [{"type": "text", "text": output.to_string()}]})
        }
        other => panic!("unexpected method {other}"),
    };
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn check_mcp_auth(headers: &HeaderMap) -> Option<Response> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != "Bearer hub_agent_command_secret" {
        return Some(axum::http::StatusCode::UNAUTHORIZED.into_response());
    }
    None
}

#[tokio::test]
async fn hub_join_command_success_outputs_safe_user_text_and_stores_credential() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    auth::AuthStore::default().save().unwrap();

    let server = FauxHubJoinServer::start().await;
    let login_url_seen = Arc::new(Mutex::new(None::<String>));
    let (callback_tx, callback_rx) = tokio::sync::oneshot::channel::<String>();
    let callback_tx = parking_lot::Mutex::new(Some(callback_tx));
    let state_for_opener = server.state.clone();
    let login_for_opener = login_url_seen.clone();
    let _join_guard = hub_join::install_test_join_runtime(server.origin.clone(), move |url| {
        *login_for_opener.lock().unwrap() = Some(url.to_string());
        let url = url.to_string();
        let state = state_for_opener.clone();
        let tx = callback_tx.lock().take();
        tokio::spawn(async move {
            let callback_text = drive_faux_hub_join_browser(&url, state).await;
            if let Some(tx) = tx {
                let _ = tx.send(callback_text);
            }
        });
        Ok(())
    });
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-join-command-success",
        log_path: None::<&PathBuf>,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub join", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));
    let callback_text = callback_rx.await.unwrap();
    assert!(
        callback_text.contains("Hub login complete"),
        "{callback_text}"
    );

    let store = auth::AuthStore::load().unwrap();
    match store
        .get(hub_auth::HUB_TOKEN_REF)
        .expect("stored hub token")
    {
        auth::ProviderCredential::ApiKey { value } => {
            assert_eq!(value, "hub_agent_command_join_secret")
        }
        other => panic!("unexpected credential kind: {other:?}"),
    }

    let state = server.state.lock().await;
    let start = state.start.as_ref().expect("captured start request");
    assert_eq!(start.client_kind, "pie-cli");
    assert_eq!(
        start.code_challenge_method,
        hub_auth::HUB_AUTH_CODE_CHALLENGE_METHOD
    );
    assert!(start.loopback_redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(
        start.loopback_redirect_uri.ends_with("/callback"),
        "{}",
        start.loopback_redirect_uri
    );
    let exchange = state.exchange.as_ref().expect("captured exchange request");
    assert_eq!(exchange.code, "hub_code_command_join_secret");
    assert_eq!(exchange.state, start.state);
    assert!(!exchange.code_verifier.is_empty());

    let text = capture.text();
    assert!(
        text.contains("Opening browser to join pie.0xfefe.me"),
        "{text}"
    );
    assert!(text.contains("Joined hub as @alice@dongxu"), "{text}");
    assert!(text.contains("restart pie, then run /hub status"), "{text}");
    let login_url = login_url_seen
        .lock()
        .unwrap()
        .clone()
        .expect("captured login url");
    let secrets = hub_auth::HubAuthSecretFragments {
        hub_token: Some("hub_agent_command_join_secret"),
        code: Some(&exchange.code),
        state: Some(&exchange.state),
        code_verifier: Some(&exchange.code_verifier),
        loopback_redirect_uri: Some(&start.loopback_redirect_uri),
        login_url: Some(&login_url),
    };
    secrets.assert_absent_from("/hub join command output", &text);
    assert!(!text.contains("pie-hub:default"), "{text}");
    assert!(!text.contains("hub_agent_"), "{text}");
    assert!(!text.contains("hub_code_"), "{text}");
    assert!(!text.contains("state_"), "{text}");
    assert!(!text.contains("127.0.0.1"), "{text}");
    assert!(!text.contains("http://"), "{text}");
    assert!(!text.contains("https://"), "{text}");
    assert!(!text.contains("MCP"), "{text}");
    assert!(!text.contains("mcp"), "{text}");
    assert!(!text.contains("config"), "{text}");
}

#[tokio::test]
async fn hub_send_command_resolves_mentions_and_outputs_bounded_status() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let mut store = auth::AuthStore::default();
    store.set(
        hub_auth::HUB_TOKEN_REF,
        auth::ProviderCredential::ApiKey {
            value: "hub_agent_command_secret".into(),
        },
    );
    store.save().unwrap();

    let server = FauxHubMcpServer::start().await;
    let _endpoint_guard = hub_client::install_test_endpoint(server.endpoint.clone());
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-send-command",
        log_path: None::<&PathBuf>,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch(
        "/hub send @bob@dongxu \"hello from alice\"",
        &registry,
        &ctx,
    )
    .await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let text = capture.text();
    assert!(
        text.contains("sent hub notification to @unknown@hub"),
        "{text}"
    );
    assert!(text.contains("hello from alice"), "{text}");
    assert!(text.contains("queued for first-contact review"), "{text}");
    assert!(text.contains("payload       Local (not sent)"), "{text}");
    assert!(!text.contains("hub_agent_command_secret"), "{text}");
    assert!(!text.contains("hub_agent_profile_secret"), "{text}");
    assert!(!text.contains("hub_agent_status_secret"), "{text}");
    assert!(!text.contains("pie-hub:default"), "{text}");
    assert!(!text.contains("018fe23a"), "{text}");
    assert!(!text.contains("123456789abc"), "{text}");
    assert!(!text.contains("target_agent_id"), "{text}");
    assert!(!text.contains("MCP"), "{text}");

    let calls = server.state.lock().await.calls.clone();
    let send = calls
        .iter()
        .find(|call| call["name"] == "send_notification")
        .expect("send_notification call");
    assert_eq!(
        send["arguments"]["target_agent_id"].as_str(),
        Some("018fe23a-2222-4a22-8b33-123456789abc")
    );
    assert_eq!(
        send["arguments"]["payload_visibility"].as_str(),
        Some("Local")
    );
    assert!(send["arguments"].get("payload").is_none());
}

#[tokio::test]
async fn hub_inbox_command_outputs_bounded_read_only_feed() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let mut store = auth::AuthStore::default();
    store.set(
        hub_auth::HUB_TOKEN_REF,
        auth::ProviderCredential::ApiKey {
            value: "hub_agent_command_secret".into(),
        },
    );
    store.save().unwrap();

    let server = FauxHubMcpServer::start().await;
    let _endpoint_guard = hub_client::install_test_endpoint(server.endpoint.clone());
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-inbox-command",
        log_path: None::<&PathBuf>,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub inbox", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let text = capture.text();
    assert!(text.contains("Hub inbox:"), "{text}");
    assert!(text.contains("<hub sender>"), "{text}");
    assert!(text.contains("hello from alice"), "{text}");
    assert!(text.contains("first-contact · payload unknown"), "{text}");
    assert!(text.contains("status unknown"), "{text}");
    assert!(text.contains("<unknown time>"), "{text}");
    assert!(!text.contains("hub_agent_command_secret"), "{text}");
    assert!(!text.contains("hub_agent_sender_secret"), "{text}");
    assert!(!text.contains("hub_agent_summary_secret"), "{text}");
    assert!(!text.contains("hub_agent_inbox_status_secret"), "{text}");
    assert!(!text.contains("hub_hs_payload_secret"), "{text}");
    assert!(!text.contains("pie-hub:default"), "{text}");
    assert!(!text.contains("018fe23a"), "{text}");
    assert!(!text.contains("123456789abc"), "{text}");
    assert!(!text.contains("notification_id"), "{text}");
    assert!(!text.contains("MCP"), "{text}");
}

#[tokio::test]
async fn hub_send_prefix_lookup_lists_safe_mentions() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    let mut store = auth::AuthStore::default();
    store.set(
        hub_auth::HUB_TOKEN_REF,
        auth::ProviderCredential::ApiKey {
            value: "hub_agent_command_secret".into(),
        },
    );
    store.save().unwrap();

    let server = FauxHubMcpServer::start().await;
    let _endpoint_guard = hub_client::install_test_endpoint(server.endpoint.clone());
    let capture = OutputCapture::install();

    let storage = Arc::new(MemorySessionStorage::new());
    let session = Session::new(storage as Arc<dyn SessionStorage>);
    let opts = AgentHarnessOptions::new(faux_model(), session);
    let harness = Arc::new(AgentHarness::new(opts));
    let registry = commands::Registry::with_builtins();
    let cwd = std::env::current_dir().unwrap();
    let ctx = commands::CommandCtx {
        harness: &harness,
        session_id: "test-hub-send-prefix-lookup",
        log_path: None::<&PathBuf>,
        tool_count: 0,
        cwd: &cwd,
    };

    let outcome = commands::dispatch("/hub send @hub_agent", &registry, &ctx).await;
    assert!(matches!(outcome, commands::CommandOutcome::Handled));

    let text = capture.text();
    assert!(text.contains("Matching hub agents:"), "{text}");
    assert!(text.contains("@unknown@hub"), "{text}");
    assert!(text.contains("Bob Cheng"), "{text}");
    assert!(
        text.contains("use /hub send @handle@namespace \"message\""),
        "{text}"
    );
    assert!(!text.contains("hub_agent_command_secret"), "{text}");
    assert!(!text.contains("hub_agent_candidate_secret"), "{text}");
    assert!(!text.contains("018fe23a"), "{text}");
    assert!(!text.contains("123456789abc"), "{text}");
    assert!(!text.contains("MCP"), "{text}");
}
