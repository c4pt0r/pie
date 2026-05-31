//! Browser + loopback onboarding for the built-in fefe hub.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use sha2::Digest as _;
use tokio::net::TcpListener;

use crate::auth::{AuthStore, ProviderCredential};
use crate::hub_auth::{
    HUB_AUTH_CLIENT_KIND, HUB_AUTH_CODE_CHALLENGE_METHOD, HUB_DEFAULT_AUTH_ORIGIN, HUB_TOKEN_REF,
    HubAuthExchangeCodeRequest, HubAuthExchangeCodeResponse, HubAuthStartRequest,
    HubAuthStartResponse,
};

const CALLBACK_PATH: &str = "/callback";
const JOIN_TIMEOUT: Duration = Duration::from_secs(300);
const BROWSER_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

pub struct JoinedHub {
    pub handle: String,
    pub namespace: String,
}

#[derive(Clone)]
pub struct HubJoinOptions {
    pub auth_origin: String,
    pub timeout: Duration,
    pub manual_login: Option<Arc<dyn Fn(ManualLogin) + Send + Sync>>,
}

#[derive(Clone, Debug)]
pub struct ManualLogin {
    pub login_url: String,
    pub reason: String,
    pub callback_port: u16,
}

impl Default for HubJoinOptions {
    fn default() -> Self {
        Self {
            auth_origin: test_auth_origin().unwrap_or_else(|| HUB_DEFAULT_AUTH_ORIGIN.into()),
            timeout: JOIN_TIMEOUT,
            manual_login: None,
        }
    }
}

#[allow(dead_code)]
pub async fn join_default_hub() -> Result<JoinedHub> {
    join_default_hub_with_options(HubJoinOptions::default()).await
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn join_default_hub_for_test() -> Result<JoinedHub> {
    join_default_hub().await
}

pub(crate) async fn join_default_hub_with_options(options: HubJoinOptions) -> Result<JoinedHub> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind local callback listener")?;
    let redirect_uri = format!("http://{}{}", listener.local_addr()?, CALLBACK_PATH);
    let state = opaque_nonce("state");
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("create hub auth client")?;

    let start_url = format!("{}/auth/start", options.auth_origin.trim_end_matches('/'));
    let start = post_json::<HubAuthStartResponse>(
        &client,
        &start_url,
        &HubAuthStartRequest {
            client_kind: HUB_AUTH_CLIENT_KIND.into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            loopback_redirect_uri: redirect_uri.clone(),
            code_challenge: challenge,
            code_challenge_method: HUB_AUTH_CODE_CHALLENGE_METHOD.into(),
            state: state.clone(),
        },
        "start hub auth",
    )
    .await?;

    if let Err(e) = open_browser(&start.login_url).await {
        if options.manual_login.is_some() {
            notify_manual_login(
                &options,
                &start.login_url,
                listener.local_addr()?.port(),
                format!("{e:#}"),
            );
        } else {
            return Err(e).context("open browser for hub login");
        }
    }

    let code = tokio::time::timeout(
        options
            .timeout
            .min(Duration::from_secs(start.expires_in_seconds)),
        wait_for_callback(listener, state.clone()),
    )
    .await
    .context("browser login timed out; try /hub join again")??;

    let exchange_url = format!(
        "{}/auth/exchange_code",
        options.auth_origin.trim_end_matches('/')
    );
    let exchange = post_json::<HubAuthExchangeCodeResponse>(
        &client,
        &exchange_url,
        &HubAuthExchangeCodeRequest {
            exchange_request_id: start.exchange_request_id,
            code,
            state,
            code_verifier: verifier,
        },
        "exchange hub auth code",
    )
    .await?;

    store_hub_token(&exchange.hub_token).context("save hub credential")?;
    Ok(JoinedHub {
        handle: exchange.handle,
        namespace: exchange.namespace,
    })
}

async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: &impl serde::Serialize,
    label: &'static str,
) -> Result<T> {
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .with_context(|| format!("{label} request failed"))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("{label} failed with status {}", status.as_u16());
    }
    response
        .json::<T>()
        .await
        .with_context(|| format!("{label} response was not valid JSON"))
}

async fn wait_for_callback(listener: TcpListener, expected_state: String) -> Result<String> {
    let (stream, _) = listener.accept().await.context("accept hub callback")?;
    let stream = stream.into_std()?;
    stream.set_nonblocking(false)?;
    tokio::task::spawn_blocking(move || read_callback(stream, &expected_state))
        .await
        .context("read hub callback task")?
}

pub(crate) fn read_callback(
    mut stream: std::net::TcpStream,
    expected_state: &str,
) -> Result<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0_u8; 8192];
    let n = stream.read(&mut buf).context("read hub callback request")?;
    let request = std::str::from_utf8(&buf[..n]).context("hub callback was not UTF-8")?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("hub callback missing request target")?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))
        .context("hub callback target was not a valid URL")?;
    if url.path() != CALLBACK_PATH {
        write_callback_response(&mut stream, 404, "Not found")?;
        anyhow::bail!("hub callback path was invalid");
    }
    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        write_callback_response(&mut stream, 400, "State mismatch. Return to pie and retry.")?;
        anyhow::bail!("hub callback state mismatch; try /hub join again");
    }
    let code = code.context("hub callback missing code; try /hub join again")?;
    write_callback_response(
        &mut stream,
        200,
        "Hub login complete. You can return to pie.",
    )?;
    Ok(code)
}

fn write_callback_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("write hub callback response")?;
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

fn store_hub_token(token: &str) -> Result<()> {
    let mut store = AuthStore::load()?;
    store.set(
        HUB_TOKEN_REF,
        ProviderCredential::ApiKey {
            value: token.to_string(),
        },
    );
    store.save()
}

fn pkce_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn opaque_nonce(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn notify_manual_login(
    options: &HubJoinOptions,
    login_url: &str,
    callback_port: u16,
    reason: String,
) {
    if let Some(handler) = &options.manual_login {
        handler(ManualLogin {
            login_url: login_url.to_string(),
            reason,
            callback_port,
        });
    }
}

fn browser_unavailable_reason() -> Option<String> {
    if std::env::var_os("PIE_HUB_JOIN_NO_BROWSER").is_some() {
        return Some("browser auto-open disabled for this session".into());
    }
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return Some("SSH session detected; browser auto-open may be unavailable".into());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Some("no desktop display detected".into());
        }
    }
    None
}

async fn open_browser(url: &str) -> Result<()> {
    if let Some(opener) = test_browser_opener() {
        return opener(url);
    }
    if let Some(reason) = browser_unavailable_reason() {
        anyhow::bail!("{reason}");
    }
    let url = url.to_string();
    let status = tokio::time::timeout(BROWSER_OPEN_TIMEOUT, async move {
        tokio::task::spawn_blocking(move || open_browser_command(&url).status())
            .await
            .context("join system browser opener task")?
            .context("spawn system browser")
    })
    .await
    .context("system browser opener timed out")??;
    if !status.success() {
        anyhow::bail!("system browser opener exited unsuccessfully");
    }
    Ok(())
}

fn open_browser_command(url: &str) -> std::process::Command {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        cmd
    }
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        cmd
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    }
}

#[cfg(test)]
type TestBrowserOpener = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

#[cfg(test)]
static TEST_BROWSER_OPENER: parking_lot::Mutex<Option<TestBrowserOpener>> =
    parking_lot::Mutex::new(None);

#[cfg(test)]
static TEST_AUTH_ORIGIN: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct HubJoinTestGuard {
    _private: (),
}

#[cfg(test)]
impl Drop for HubJoinTestGuard {
    fn drop(&mut self) {
        *TEST_BROWSER_OPENER.lock() = None;
        *TEST_AUTH_ORIGIN.lock() = None;
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn install_test_join_runtime(
    auth_origin: String,
    opener: impl Fn(&str) -> Result<()> + Send + Sync + 'static,
) -> HubJoinTestGuard {
    *TEST_AUTH_ORIGIN.lock() = Some(auth_origin);
    *TEST_BROWSER_OPENER.lock() = Some(Box::new(opener));
    HubJoinTestGuard { _private: () }
}

#[cfg(test)]
fn test_browser_opener() -> Option<TestBrowserOpener> {
    let opener = TEST_BROWSER_OPENER.lock().take()?;
    Some(Box::new(move |url| opener(url)))
}

#[cfg(not(test))]
fn test_browser_opener() -> Option<Box<dyn Fn(&str) -> Result<()> + Send + Sync>> {
    None
}

#[cfg(test)]
fn test_auth_origin() -> Option<String> {
    TEST_AUTH_ORIGIN.lock().clone()
}

#[cfg(not(test))]
fn test_auth_origin() -> Option<String> {
    None
}
