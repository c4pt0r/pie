//! Local browser UI for the coding-agent REPL.
//!
//! This is intentionally a small loopback-only surface. The browser layer sends commands into the
//! same single-turn event loop used by the TUI and receives bounded feed snapshots over SSE.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc};

use super::kernel::{QueuedTurn, TurnState, poll_turn};
use super::{App, CommandCtx, CommandOutcome, feed, mentions};

const SNAPSHOT_LINE_LIMIT: usize = 200;

#[derive(Clone, Debug)]
pub struct WebOptions {
    pub host: String,
    pub port: u16,
}

#[derive(Clone)]
struct HttpState {
    commands: mpsc::UnboundedSender<WebCommand>,
    snapshots: broadcast::Sender<WebSnapshot>,
    latest: Arc<Mutex<WebSnapshot>>,
}

#[derive(Debug)]
enum WebCommand {
    Submit { text: String },
    Abort,
}

#[derive(Clone, Debug, Serialize)]
struct WebSnapshot {
    session_id: String,
    model: String,
    busy: bool,
    queued_count: usize,
    latest_trigger_poll: Option<super::feed::TriggerPollStatus>,
    goal: Option<WebGoalSnapshot>,
    feed_lines: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct WebGoalSnapshot {
    condition: String,
    status: String,
    iterations: u32,
    last_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PromptRequest {
    text: String,
}

#[derive(Debug, Serialize)]
struct CommandAccepted {
    accepted: bool,
}

impl App {
    pub async fn run_web(mut self, options: WebOptions) -> Result<()> {
        let addr = bind_addr(&options)?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind web ui on {addr}"))?;
        let actual = listener.local_addr()?;

        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<WebCommand>();
        let (snapshot_tx, _) = broadcast::channel::<WebSnapshot>(128);
        let latest = Arc::new(Mutex::new(self.web_snapshot()));
        let router = web_router(HttpState {
            commands: command_tx,
            snapshots: snapshot_tx.clone(),
            latest: latest.clone(),
        });

        let server = axum::serve(listener, router.into_make_service());
        let mut server_task = tokio::spawn(async move { server.await });
        println!("pie web listening on http://{actual}");

        let mut feed_rx = self.feed_rx.take().expect("feed_rx taken once");
        let mut main_run_rx = self.main_run_rx.take().expect("main_run_rx taken once");
        let mut turn = TurnState::default();
        self.refresh_goal_state().await;
        self.publish_snapshot(&latest, &snapshot_tx).await;

        loop {
            tokio::select! {
                biased;
                result = poll_turn(&mut turn.fut), if turn.fut.is_some() => {
                    self.finish_turn(&mut turn, result).await;
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(command) = command_rx.recv() => {
                    self.handle_web_command(command, &mut turn).await;
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(update) = feed_rx.recv() => {
                    self.apply_feed_update(update);
                    while let Ok(update) = feed_rx.try_recv() {
                        self.apply_feed_update(update);
                    }
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                Some(trace_id) = main_run_rx.recv(), if turn.fut.is_none() => {
                    self.start_triggered_turn(trace_id, &mut turn);
                    self.publish_snapshot(&latest, &snapshot_tx).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    if turn.fut.is_some() {
                        self.request_abort(&mut turn);
                        self.publish_snapshot(&latest, &snapshot_tx).await;
                    }
                    break;
                }
                server_result = &mut server_task => {
                    match server_result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => self.error_line(format!("web server: {e}")),
                        Err(e) => self.error_line(format!("web server task: {e}")),
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle_web_command(&mut self, command: WebCommand, turn: &mut TurnState) {
        match command {
            WebCommand::Submit { text } => self.submit_web_text(text, turn).await,
            WebCommand::Abort => self.request_abort(turn),
        }
    }

    async fn submit_web_text(&mut self, text: String, turn: &mut TurnState) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.history.append(&trimmed);
        self.follow = true;

        if trimmed.starts_with('/') {
            self.feed.push_user(&trimmed);
            self.dispatch_web_slash(&trimmed, turn).await;
            return;
        }

        let expanded = mentions::expand(&trimmed, &self.cwd).await.0;
        let prompt_text =
            crate::commands::attach_skill_prompt(expanded, self.pending_skill.take().as_deref());
        let display = trimmed;
        if turn.fut.is_some() {
            self.queue_user_prompt(display, prompt_text, Vec::new());
        } else {
            self.feed.push_user(display);
            self.start_user_prompt_turn(prompt_text, Vec::new(), turn);
        }
    }

    async fn dispatch_web_slash(&mut self, input: &str, turn: &mut TurnState) {
        let outcome = {
            let ctx = CommandCtx {
                harness: self.kernel.harness(),
                session_id: &self.session_id,
                log_path: self.log_path.as_ref(),
                tool_count: self.tool_count,
                cwd: &self.cwd,
            };
            crate::commands::dispatch(input, &self.registry, &ctx).await
        };
        match outcome {
            CommandOutcome::Quit => {
                self.system_line("web ui stays running; close the browser tab or press Ctrl-C in the terminal to stop the server");
            }
            CommandOutcome::ClearScreen => {
                self.feed.clear();
                self.follow = true;
            }
            CommandOutcome::Error(e) => self.error_line(e),
            CommandOutcome::AttachSkill { name } => {
                self.pending_skill = Some(name);
            }
            CommandOutcome::RunAgentPrompt {
                prompt,
                error_context,
            } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::AgentPrompt {
                        display: input.to_string(),
                        prompt,
                        error_context,
                    });
                } else {
                    self.start_prompt_turn(prompt, error_context, turn);
                }
            }
            CommandOutcome::RunPromptTemplate { name, vars } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::PromptTemplate {
                        display: input.to_string(),
                        name,
                        vars,
                    });
                } else {
                    self.start_template_turn(name, vars, turn);
                }
            }
            CommandOutcome::RunCompaction { custom } => {
                if turn.fut.is_some() {
                    self.enqueue_turn(QueuedTurn::Compaction {
                        display: input.to_string(),
                        custom,
                    });
                } else {
                    self.start_compaction_turn(custom, turn);
                }
            }
            CommandOutcome::LoginSecret { provider } => {
                self.error_line(format!(
                    "web login is not implemented yet; run `/login {provider}` from the terminal UI"
                ));
            }
            CommandOutcome::Handled => {}
        }
        if input.trim_start().starts_with("/goal") {
            self.refresh_goal_state().await;
        }
    }

    fn web_snapshot(&self) -> WebSnapshot {
        let model = {
            let state = self.kernel.harness().agent().state();
            state
                .model
                .as_ref()
                .map(|m| format!("{}:{}", m.provider.0, m.id))
                .unwrap_or_else(|| "no-model".to_string())
        };
        WebSnapshot {
            session_id: self.session_id.clone(),
            model,
            busy: self.busy,
            queued_count: self.queued_turns.len(),
            latest_trigger_poll: self.latest_trigger_poll.clone(),
            goal: self.latest_goal.as_ref().map(|goal| WebGoalSnapshot {
                condition: crate::bug_report::redact(&goal.condition),
                status: goal.status.as_str().to_string(),
                iterations: goal.iterations,
                last_reason: goal.last_reason.as_deref().map(crate::bug_report::redact),
            }),
            feed_lines: bounded_feed_lines(&self.feed, SNAPSHOT_LINE_LIMIT),
        }
    }

    async fn publish_snapshot(
        &self,
        latest: &Arc<Mutex<WebSnapshot>>,
        snapshots: &broadcast::Sender<WebSnapshot>,
    ) {
        let snapshot = self.web_snapshot();
        *latest.lock().await = snapshot.clone();
        let _ = snapshots.send(snapshot);
    }
}

fn web_router(state: HttpState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/state", get(state_snapshot))
        .route("/events", get(events))
        .route("/prompt", post(prompt))
        .route("/abort", post(abort))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn state_snapshot(State(state): State<HttpState>) -> Json<WebSnapshot> {
    Json(state.latest.lock().await.clone())
}

async fn events(
    State(state): State<HttpState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.snapshots.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(snapshot) => {
                    let data = serde_json::to_string(&snapshot)
                        .unwrap_or_else(|_| "{\"error\":\"serialize\"}".to_string());
                    return Some((Ok(Event::default().event("snapshot").data(data)), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn prompt(
    State(state): State<HttpState>,
    Json(req): Json<PromptRequest>,
) -> impl IntoResponse {
    let accepted = state
        .commands
        .send(WebCommand::Submit { text: req.text })
        .is_ok();
    Json(CommandAccepted { accepted })
}

async fn abort(State(state): State<HttpState>) -> impl IntoResponse {
    let accepted = state.commands.send(WebCommand::Abort).is_ok();
    Json(CommandAccepted { accepted })
}

fn bounded_feed_lines(feed: &feed::Feed, limit: usize) -> Vec<String> {
    let lines = feed
        .lines(100)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    if lines.len() <= limit {
        return lines;
    }
    lines[lines.len() - limit..].to_vec()
}

fn bind_addr(options: &WebOptions) -> Result<SocketAddr> {
    let ip = match options.host.as_str() {
        "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        host => host
            .parse::<IpAddr>()
            .with_context(|| format!("parse --web-host `{host}` as an IP address"))?,
    };
    if !ip.is_loopback() {
        bail!("refusing non-loopback web bind {ip}; Web UI is loopback-only");
    }
    Ok(SocketAddr::new(ip, options.port))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>pie web</title>
  <style>
    :root { color-scheme: light dark; --border: #8892a0; --muted: #7b8490; --accent: #0ea5e9; }
    * { box-sizing: border-box; }
    body { margin: 0; font: 14px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
    main { height: 100vh; display: grid; grid-template-rows: auto 1fr auto; }
    header, footer { padding: 10px 14px; border-color: var(--border); }
    header { border-bottom: 1px solid var(--border); display: flex; gap: 14px; align-items: center; flex-wrap: wrap; }
    footer { border-top: 1px solid var(--border); }
    #feed { overflow: auto; padding: 14px; white-space: pre-wrap; }
    .line { min-height: 1.45em; }
    .muted { color: var(--muted); }
    .busy { color: var(--accent); }
    #poll { color: var(--muted); font-size: 13px; margin-bottom: 8px; min-height: 18px; }
    form { display: grid; grid-template-columns: 1fr auto auto; gap: 8px; align-items: end; }
    textarea { width: 100%; min-height: 48px; max-height: 180px; resize: vertical; padding: 10px; font: inherit; }
    button { padding: 9px 12px; font: inherit; }
  </style>
</head>
<body>
<main>
  <header>
    <strong>pie web</strong>
    <span id="model" class="muted"></span>
    <span id="status" class="muted"></span>
  </header>
  <div id="poll"></div>
  <section id="feed" aria-live="polite"></section>
  <footer>
    <form id="form">
      <textarea id="input" placeholder="type a message, or /help"></textarea>
      <button type="submit">Send</button>
      <button type="button" id="abort">Abort</button>
    </form>
  </footer>
</main>
<script>
const feed = document.getElementById('feed');
const model = document.getElementById('model');
const status = document.getElementById('status');
const poll = document.getElementById('poll');
const input = document.getElementById('input');
const form = document.getElementById('form');
const abortButton = document.getElementById('abort');

function render(snapshot) {
  model.textContent = snapshot.model + ' · session ' + snapshot.session_id;
  status.textContent = snapshot.busy
    ? ('working' + (snapshot.queued_count ? ' · ' + snapshot.queued_count + ' queued' : ''))
    : ('ready' + (snapshot.queued_count ? ' · ' + snapshot.queued_count + ' queued' : ''));
  status.className = snapshot.busy ? 'busy' : 'muted';
  poll.textContent = snapshot.latest_trigger_poll
    ? ('Polling · ' + snapshot.latest_trigger_poll.checked_at + ' · '
      + snapshot.latest_trigger_poll.source_label + ' / '
      + snapshot.latest_trigger_poll.event_label + ' · '
      + snapshot.latest_trigger_poll.summary)
    : '';
  feed.replaceChildren(...snapshot.feed_lines.map((line) => {
    const div = document.createElement('div');
    div.className = 'line';
    div.textContent = line;
    return div;
  }));
  feed.scrollTop = feed.scrollHeight;
}

fetch('/state').then((r) => r.json()).then(render);
const events = new EventSource('/events');
events.addEventListener('snapshot', (event) => render(JSON.parse(event.data)));

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const text = input.value;
  if (!text.trim()) return;
  input.value = '';
  await fetch('/prompt', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ text })
  });
});

abortButton.addEventListener('click', () => fetch('/abort', { method: 'POST' }));
input.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey) {
    event.preventDefault();
    form.requestSubmit();
  } else if (event.key === 'Escape') {
    fetch('/abort', { method: 'POST' });
  }
});
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn bind_addr_rejects_remote_by_default() {
        let err = bind_addr(&WebOptions {
            host: "0.0.0.0".into(),
            port: 0,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing non-loopback"));
    }

    #[test]
    fn bind_addr_accepts_loopback_and_localhost() {
        let local = bind_addr(&WebOptions {
            host: "127.0.0.1".into(),
            port: 0,
        })
        .unwrap();
        assert!(local.ip().is_loopback());

        let named = bind_addr(&WebOptions {
            host: "localhost".into(),
            port: 0,
        })
        .unwrap();
        assert!(named.ip().is_loopback());
    }

    #[test]
    fn bounded_feed_lines_keeps_recent_rows_only() {
        let mut feed = feed::Feed::new();
        for i in 0..250 {
            feed.apply(feed::FeedUpdate::Plain {
                text: format!("line {i}"),
                level: feed::Level::Output,
            });
        }

        let lines = bounded_feed_lines(&feed, SNAPSHOT_LINE_LIMIT);
        assert_eq!(lines.len(), SNAPSHOT_LINE_LIMIT);
        assert!(
            lines.first().is_some_and(|line| line.contains("line 50")),
            "{lines:?}"
        );
        assert!(
            lines.last().is_some_and(|line| line.contains("line 249")),
            "{lines:?}"
        );
        assert_eq!(
            lines.first().and_then(|line| line.chars().nth(2)),
            Some(':'),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn endpoints_return_state_accept_commands_and_stream_snapshots() {
        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<WebCommand>();
        let (snapshot_tx, _) = broadcast::channel::<WebSnapshot>(16);
        let latest = Arc::new(Mutex::new(WebSnapshot {
            session_id: "sess-1".into(),
            model: "provider:model".into(),
            busy: false,
            queued_count: 0,
            latest_trigger_poll: None,
            goal: None,
            feed_lines: vec!["ready".into()],
        }));
        let router = web_router(HttpState {
            commands: command_tx,
            snapshots: snapshot_tx.clone(),
            latest,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service())
                .await
                .unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let state: serde_json::Value = client
            .get(format!("{base}/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(state["session_id"], "sess-1");
        assert_eq!(state["feed_lines"][0], "ready");

        let accepted: serde_json::Value = client
            .post(format!("{base}/prompt"))
            .json(&json!({ "text": "hello" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        match command_rx.recv().await.unwrap() {
            WebCommand::Submit { text } => assert_eq!(text, "hello"),
            other => panic!("unexpected command: {other:?}"),
        }

        let accepted: serde_json::Value = client
            .post(format!("{base}/abort"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(accepted["accepted"], true);
        match command_rx.recv().await.unwrap() {
            WebCommand::Abort => {}
            other => panic!("unexpected command: {other:?}"),
        }

        let response = client.get(format!("{base}/events")).send().await.unwrap();
        assert!(response.status().is_success());
        let mut stream = response.bytes_stream();
        snapshot_tx
            .send(WebSnapshot {
                session_id: "sess-1".into(),
                model: "provider:model".into(),
                busy: true,
                queued_count: 1,
                latest_trigger_poll: None,
                goal: None,
                feed_lines: vec!["streamed".into()],
            })
            .unwrap();
        let chunk = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&chunk);
        assert!(text.contains("event: snapshot"), "{text}");
        assert!(text.contains("streamed"), "{text}");

        server.abort();
    }
}
