#![allow(dead_code)]

#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/bug_report.rs"]
mod bug_report;
#[path = "../src/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/export.rs"]
mod export;
#[path = "../src/hub_auth.rs"]
mod hub_auth;
#[path = "../src/hub_join.rs"]
mod hub_join;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::routing::{get, post};
use hub_auth::{
    HUB_AUTH_CODE_CHALLENGE_METHOD, HubAuthExchangeCodeRequest, HubAuthExchangeCodeResponse,
    HubAuthProfile, HubAuthStartRequest, HubAuthStartResponse, HubAuthVisibility,
};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

static PIE_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

#[derive(Default)]
struct FauxAuthState {
    start: Option<HubAuthStartRequest>,
    exchange: Option<HubAuthExchangeCodeRequest>,
}

#[tokio::test]
async fn hub_join_browser_loopback_stores_token_without_rendering_auth_secrets() {
    let _auth_guard = auth::ENV_LOCK.lock().unwrap();
    let _pie_guard = PIE_DIR_ENV_LOCK.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let _pie_dir = EnvGuard::set("PIE_DIR", temp.path());
    auth::AuthStore::default().save().unwrap();

    let server = FauxAuthServer::start().await;

    let (opened_tx, opened_rx) = oneshot::channel::<String>();
    let opened_tx = parking_lot::Mutex::new(Some(opened_tx));
    let _guard = hub_join::install_test_join_runtime(server.origin.clone(), move |url| {
        opened_tx
            .lock()
            .take()
            .expect("open browser once")
            .send(url.to_string())
            .unwrap();
        Ok(())
    });

    let join = tokio::spawn(async { hub_join::join_default_hub().await });
    let login_url = opened_rx.await.unwrap();
    let (_callback, callback_text) = drive_login_callback(&login_url, &server.state).await;
    assert!(
        callback_text.contains("Hub login complete"),
        "{callback_text}"
    );
    assert!(!callback_text.contains("hub_code_"), "{callback_text}");
    assert!(!callback_text.contains("state_"), "{callback_text}");

    let joined = join.await.unwrap().unwrap();
    assert_eq!(joined.handle, "alice");
    assert_eq!(joined.namespace, "dongxu");

    let store = auth::AuthStore::load().unwrap();
    match store
        .get(hub_auth::HUB_TOKEN_REF)
        .expect("stored hub token")
    {
        auth::ProviderCredential::ApiKey { value } => {
            assert_eq!(value, "hub_agent_test_join_secret")
        }
        other => panic!("unexpected credential kind: {other:?}"),
    }

    let state = server.state.lock().await;
    let start = state.start.as_ref().expect("captured start request");
    assert_eq!(start.client_kind, "pie-cli");
    assert_eq!(start.code_challenge_method, HUB_AUTH_CODE_CHALLENGE_METHOD);
    assert!(start.loopback_redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(
        start.loopback_redirect_uri.ends_with("/callback"),
        "{}",
        start.loopback_redirect_uri
    );
    let exchange = state.exchange.as_ref().expect("captured exchange request");
    assert_eq!(exchange.code, "hub_code_test_join_secret");
    assert_eq!(exchange.state, start.state);
    assert!(!exchange.code_verifier.is_empty());

    let visible = format!(
        "Joined hub as {}@{}; hub is connected; run /hub status or /hub send",
        joined.handle, joined.namespace
    );
    let secrets = hub_auth::HubAuthSecretFragments {
        hub_token: Some("hub_agent_test_join_secret"),
        code: Some(&exchange.code),
        state: Some(&exchange.state),
        code_verifier: Some(&exchange.code_verifier),
        loopback_redirect_uri: Some(&start.loopback_redirect_uri),
        login_url: Some(&login_url),
    };
    secrets.assert_absent_from("join visible output", &visible);
    assert!(!visible.contains("restart pie"), "{visible}");
}

struct FauxAuthServer {
    origin: String,
    state: Arc<Mutex<FauxAuthState>>,
}

impl FauxAuthServer {
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(FauxAuthState::default()));
        let app = Router::new()
            .route("/auth/start", post(auth_start))
            .route("/auth/exchange_code", post(auth_exchange))
            .route("/login", get(login_page))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { origin, state }
    }
}

async fn drive_login_callback(
    login_url: &str,
    state: &Arc<Mutex<FauxAuthState>>,
) -> (reqwest::Url, String) {
    let callback_query = {
        let state = state.lock().await;
        let start = state.start.as_ref().expect("captured start request");
        format!("code=hub_code_test_join_secret&state={}", start.state)
    };
    let login = reqwest::Url::parse(login_url).unwrap();
    assert!(login.query_pairs().any(|(key, _)| key == "req"));
    assert!(!login.query_pairs().any(|(key, _)| key == "redirect"));
    let redirect_uri = {
        let state = state.lock().await;
        state
            .start
            .as_ref()
            .expect("captured start request")
            .loopback_redirect_uri
            .clone()
    };
    let mut callback = reqwest::Url::parse(&redirect_uri).unwrap();
    callback.set_query(Some(&callback_query));
    let callback_response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap()
        .get(callback.clone())
        .send()
        .await
        .unwrap();
    assert!(callback_response.status().is_success());
    let callback_text = callback_response.text().await.unwrap();
    (callback, callback_text)
}

async fn auth_start(
    State(state): State<Arc<Mutex<FauxAuthState>>>,
    axum::Json(request): axum::Json<HubAuthStartRequest>,
) -> Result<axum::Json<HubAuthStartResponse>, axum::http::StatusCode> {
    let redirect = reqwest::Url::parse(&request.loopback_redirect_uri)
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    if redirect.path() != "/callback" {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }
    state.lock().await.start = Some(request);
    Ok(axum::Json(HubAuthStartResponse {
        exchange_request_id: "exchange-request-1".into(),
        login_url: "http://127.0.0.1/login?req=exchange-request-1&state=state-public".into(),
        expires_in_seconds: 30,
    }))
}

async fn auth_exchange(
    State(state): State<Arc<Mutex<FauxAuthState>>>,
    axum::Json(request): axum::Json<HubAuthExchangeCodeRequest>,
) -> axum::Json<HubAuthExchangeCodeResponse> {
    state.lock().await.exchange = Some(request);
    axum::Json(HubAuthExchangeCodeResponse {
        agent_id: "018fe23a-1111-4a22-8b33-123456789abc".into(),
        handle: "alice".into(),
        namespace: "dongxu".into(),
        hub_token: "hub_agent_test_join_secret".into(),
        expires_at: None,
        profile: HubAuthProfile {
            display_name: "alice".into(),
            description: None,
            capabilities: Vec::new(),
        },
        visibility: HubAuthVisibility {
            discoverable: hub_auth::HubDiscoverable::Public,
            inbox: hub_auth::HubInbox::Namespace,
        },
    })
}

async fn login_page(request: Request<Body>) -> String {
    request.uri().query().unwrap_or_default().to_string()
}
