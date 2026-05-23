//! `bash` tool. Mirrors `packages/coding-agent/src/core/tools/bash.ts`. Runs the command via
//! `sh -c`, captures stdout+stderr, honors an optional timeout (seconds), and honors the
//! agent's cancellation token.
//!
//! Concurrency and lifecycle invariants (per the code-review post 2026-05-22 in
//! `#code-review`):
//!
//! 1. **stdout and stderr drain concurrently**, not sequentially. Sequential drain
//!    deadlocks when the child writes enough to fill the stderr pipe while the tool is
//!    blocked on stdout (or vice versa).
//! 2. **Timeout and cancellation kill the entire process tree, not just the direct
//!    child**. The previous implementation flagged `[timed out]` / `[aborted]` in the
//!    synthetic output but left `sh` running in the background — long-running,
//!    destructive, or runaway commands could keep executing after the agent thought they
//!    were done. We now place the child in its own process group on Unix via `setsid`
//!    so a `killpg(pgid, SIGKILL)` reaches background jobs and detached descendants like
//!    `(sleep 60) & wait`. Same pattern as `NativeEnv::exec` in `crates/agent` (PR #40).
//! 3. **`kill_on_drop(true)` is the belt-and-braces backstop**. If any branch returns early
//!    without an explicit `child.kill().await`, the destructor still reaps the child.

use async_trait::async_trait;
use pie_agent_core::{AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

pub struct BashTool;

/// What we collected from a single run, regardless of whether the child finished cleanly,
/// timed out, or was cancelled. Each variant carries the captured output so the LLM still
/// sees what the command produced before the kill.
struct RunOutcome {
    stdout: String,
    stderr: String,
    /// Process exit code, when the child exited normally on its own. `None` when we killed
    /// the child (timeout / cancel) — those branches surface as exit code -1 in the rendered
    /// output and add a `[timed out ...]` / `[aborted]` marker to stderr.
    exit_code: Option<i32>,
    /// Optional marker the renderer appends to `stderr` (e.g. `"[aborted]"`).
    stderr_suffix: Option<String>,
}

impl RunOutcome {
    fn rendered_exit(&self) -> i32 {
        self.exit_code.unwrap_or(-1)
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }

    fn label(&self) -> &str {
        "bash"
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let command = params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::from("missing `command`"))?;
        let timeout_secs = params.get("timeout").and_then(|v| v.as_u64());

        let outcome = run_with_kill_on_timeout_or_cancel(command, timeout_secs, &cancel).await?;

        let exit = outcome.rendered_exit();
        let (stdout_trim, st) =
            truncate_tail(&outcome.stdout, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut stderr_full = outcome.stderr;
        if let Some(suffix) = &outcome.stderr_suffix {
            if !stderr_full.is_empty() && !stderr_full.ends_with('\n') {
                stderr_full.push('\n');
            }
            stderr_full.push_str(suffix);
        }
        let (stderr_trim, _) = truncate_tail(&stderr_full, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut text = format!("$ {command}\n");
        if let Some(note) = st.note() {
            text.push_str(&note);
            text.push('\n');
        }
        if !stdout_trim.is_empty() {
            text.push_str(&stdout_trim);
            if !stdout_trim.ends_with('\n') {
                text.push('\n');
            }
        }
        if !stderr_trim.is_empty() {
            text.push_str("[stderr]\n");
            text.push_str(&stderr_trim);
            if !stderr_trim.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str(&format!("[exit {exit}]"));

        let is_error = exit != 0;
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(text)],
            details: json!({
                "command": command,
                "exitCode": exit,
                "isError": is_error,
            }),
            terminate: None,
        })
    }
}

/// Spawn `sh -c <command>` and collect its output, killing the child on timeout / cancel.
///
/// Returns the captured stdout / stderr and the exit-code-or-`None` per [`RunOutcome`].
/// Only returns `Err` when the spawn itself fails — every other failure mode (kill from
/// timeout / cancel / pipe error) folds into the outcome so the LLM still sees what the
/// command produced.
async fn run_with_kill_on_timeout_or_cancel(
    command: &str,
    timeout_secs: Option<u64>,
    cancel: &CancellationToken,
) -> Result<RunOutcome, AgentToolError> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Defense in depth: any early-return path between here and the explicit kill
        // branches below still destroys the child instead of leaving an orphan.
        .kill_on_drop(true);

    #[cfg(unix)]
    {
        // SAFETY: this closure runs in the child between fork and exec on Unix. `setsid`
        // is async-signal-safe per POSIX and has no Rust state to invalidate. The child
        // becomes session and process-group leader; SIGKILL to `-pgid` then targets the
        // whole tree we just spawned, so background jobs like `(sleep 60) & wait` die
        // with their parent shell on timeout / cancel. `tokio::process::Command` exposes
        // `pre_exec` as an inherent method (delegating to `std::os::unix::process::
        // CommandExt`), so no trait import is needed here.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| AgentToolError::from(format!("spawn: {e}")))?;
    // Snapshot the pid before any wait/select touches `child` so the kill path can target
    // the process group even if tokio later loses the handle.
    let child_pid = child.id();

    // Drain stdout and stderr concurrently on a background task. The task ends naturally
    // when the child closes both pipes (i.e. when it exits — whether voluntarily or from
    // our kill). Running both reads in parallel is what prevents the pipe-full deadlock
    // the previous sequential drain hit on commands like `cargo build` that emit a lot
    // of stderr.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let drain_handle = tokio::spawn(async move {
        let stdout_task = async move {
            let mut s = String::new();
            if let Some(mut h) = stdout {
                let _ = h.read_to_string(&mut s).await;
            }
            s
        };
        let stderr_task = async move {
            let mut s = String::new();
            if let Some(mut h) = stderr {
                let _ = h.read_to_string(&mut s).await;
            }
            s
        };
        tokio::join!(stdout_task, stderr_task)
    });

    // Race three outcomes:
    //   1. Child finishes on its own — record the exit code, drain finishes shortly.
    //   2. Timeout fires — kill the child, drain finishes when its pipes close.
    //   3. Cancellation token tripped — same as timeout, with a different stderr marker.
    //
    // The `wait` future borrows `&mut child`, so we keep it inside an inner block so the
    // borrow is released before we call `child.start_kill()` / `child.wait()` again
    // outside.
    let kill_reason: KillReason;
    let exit_code: Option<i32>;
    {
        let wait = child.wait();
        tokio::pin!(wait);
        // `None` timeout maps to a far-future sleep gated by `if has_timeout` — that arm
        // never resolves in that case, so the select reduces to cancel-vs-wait.
        let timeout_future =
            tokio::time::sleep(Duration::from_secs(timeout_secs.unwrap_or(u64::MAX / 2)));
        tokio::pin!(timeout_future);
        let has_timeout = timeout_secs.is_some();

        let (kr, code) = tokio::select! {
            biased;

            // Cancellation wins over both timeout and natural finish so the user's
            // Ctrl-C is honoured promptly.
            _ = cancel.cancelled() => (KillReason::Cancelled, None),

            _ = &mut timeout_future, if has_timeout => (
                KillReason::TimedOut { secs: timeout_secs.unwrap() },
                None,
            ),

            status = &mut wait => {
                let c = status.ok().and_then(|s| s.code());
                (KillReason::Finished, c)
            }
        };
        kill_reason = kr;
        exit_code = code;
    }

    // If we exited via timeout/cancel, the child (and any descendants it spawned) are
    // still alive — tear down the whole process group now. On Unix the child sits at the
    // head of its own group thanks to the `setsid` we ran in `pre_exec`, so a single
    // `killpg(pgid, SIGKILL)` reaches background jobs and detached descendants. On
    // non-Unix targets we fall back to `start_kill` (which only kills the direct child;
    // proper Windows job-object support is a separate port story). `kill_on_drop` and
    // the post-reap `wait` are the final fallbacks.
    if !matches!(kill_reason, KillReason::Finished) {
        terminate_child_tree(&mut child, child_pid).await;
    }

    // The drain task should be done — pipes close when the child exits. Cap with a short
    // timeout in case the kernel hasn't surfaced the EOF yet on a wedged child.
    let drain_result = timeout(Duration::from_secs(2), drain_handle).await;
    let (stdout, stderr) = match drain_result {
        Ok(Ok((o, e))) => (o, e),
        _ => (String::new(), String::new()),
    };

    let stderr_suffix = match kill_reason {
        KillReason::Finished => None,
        KillReason::Cancelled => Some("[aborted]".into()),
        KillReason::TimedOut { secs } => Some(format!("[timed out after {secs}s]")),
    };

    Ok(RunOutcome {
        stdout,
        stderr,
        exit_code,
        stderr_suffix,
    })
}

enum KillReason {
    Finished,
    TimedOut { secs: u64 },
    Cancelled,
}

/// Best-effort teardown of the child *and any descendants it spawned*. On Unix the child
/// was placed in its own session/process group via `setsid()`, so a single
/// `killpg(pid, SIGKILL)` reaches background jobs and detached children. On non-Unix
/// targets we fall back to the direct `start_kill` path. The final `wait` reaps the
/// zombie; both the kill and the wait are capped by the caller's surrounding 2-second
/// drain window via `kill_on_drop`.
async fn terminate_child_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `killpg` with SIGKILL on a known pgid is sound; the pid was just
        // observed from `child.id()`. A zero / `ESRCH` return (child already gone) is
        // benign and we don't assert on it.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    // Cross-platform reaper request — on Unix this is redundant after killpg, but it
    // marks the handle terminated on the tokio side; on non-Unix it's the only kill.
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
    let _ = pid;
}

use once_cell::sync::Lazy;
static DEFINITION: Lazy<Tool> = Lazy::new(|| Tool {
    name: "bash".into(),
    description: format!(
        "Run a shell command via `sh -c`. Returns stdout+stderr (tail-truncated to {DEFAULT_MAX_LINES} lines / {} KiB) and exit code. Optional `timeout` in seconds. Timeouts and cancellations kill the child process; stdout and stderr are drained concurrently so high-output commands do not deadlock the tool.",
        DEFAULT_MAX_BYTES / 1024
    ),
    parameters: json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Shell command to execute" },
            "timeout": { "type": "integer", "description": "Timeout in seconds (optional). On timeout the child is killed and any output captured so far is returned." },
        },
        "required": ["command"],
    }),
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// Spawn a long-running command, hit the timeout, and assert the child process is
    /// gone afterwards. The previous implementation marked the result `[timed out]` but
    /// left `sh -c sleep ...` running in the background.
    ///
    /// Uses a unique sleep duration (`sleep 47383`) so the `pgrep -f` check can scope to
    /// this test only — `cargo test` runs tests in parallel, and a sibling test using
    /// plain `sleep 60` would otherwise collide. Pick a value that's:
    /// 1. larger than any plausible test wall-clock, so the kill path is the only exit
    /// 2. unique across this file's tests
    #[tokio::test]
    async fn timeout_kills_child_process() {
        const SLEEP_SECS: &str = "47383";
        let tool = BashTool;
        let started = Instant::now();
        let result = tool
            .execute(
                "t1",
                json!({ "command": format!("sleep {SLEEP_SECS}"), "timeout": 1 }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("bash tool execute should not error on timeout");
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "timeout path took {elapsed:?}; child kill did not happen in time"
        );
        let text = match &result.content[0] {
            UserContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("[timed out after 1s]"),
            "expected timeout marker in output, got: {text}"
        );
        assert!(text.contains("[exit -1]"));

        // Give the OS a beat to reap the killed process group.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify no `sleep 47383` process survived. `pgrep -f` matches the full argv,
        // including the shell wrapper if any sibling test happened to spawn one — the
        // unique duration scopes the check to this test.
        let pgrep = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(format!("sleep {SLEEP_SECS}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = pgrep {
            let mut buf = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_string(&mut buf).await;
            }
            let _ = child.wait().await;
            assert!(
                buf.trim().is_empty(),
                "found surviving `sleep {SLEEP_SECS}` process(es) after timeout: pids={buf}"
            );
        }
    }

    /// Timeout must kill not just the direct `sh -c` child but any descendants the shell
    /// spawned (background jobs, detached processes). The previous implementation killed
    /// only the direct child, so `(sleep 60) & wait` left `sleep 60` running after the
    /// tool returned. We solve it the same way `NativeEnv::exec` does (PR #40): run the
    /// child in its own process group via `setsid` and `killpg` the whole group on
    /// timeout. Unix-only because `setsid` / `killpg` are Unix primitives.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_descendant_processes() {
        let tool = BashTool;
        // The pattern `(cmd) & wait` is the canonical shell-detached-child case: the
        // background job inherits the shell's process group, so killing only the shell
        // would leave the `sleep` running. Use a marker arg unique to this test so the
        // `pgrep` check doesn't false-positive against other tests' sleeps.
        let marker = "bash-tool-desc-kill-marker-7f3a9c";
        let cmd = format!("(sleep 60 && echo {marker}) & wait");
        let started = Instant::now();
        let _result = tool
            .execute(
                "tdesc",
                json!({ "command": cmd, "timeout": 1 }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("bash tool execute should not error on timeout");
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "timeout path took {elapsed:?}; descendant kill did not happen in time"
        );

        // Give the OS a beat to actually reap the descendant tree.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // No process should match the marker after teardown. We grep for the literal
        // command string the shell would have launched.
        let pgrep = tokio::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = pgrep {
            let mut buf = String::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_string(&mut buf).await;
            }
            let _ = child.wait().await;
            assert!(
                buf.trim().is_empty(),
                "found surviving descendant process(es) matching {marker:?}: pids={buf}"
            );
        }
    }

    /// Cancel via the token mid-run. Same expectation as timeout: child is killed, output
    /// includes the `[aborted]` marker, and no zombie remains. Distinct sleep duration
    /// from `timeout_kills_child_process` so `pgrep` checks across the file don't collide
    /// when `cargo test` runs in parallel.
    #[tokio::test]
    async fn cancellation_kills_child_process() {
        const SLEEP_SECS: &str = "47384";
        let tool = BashTool;
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        // Trip the token 200ms in.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });
        let started = Instant::now();
        let result = tool
            .execute(
                "t2",
                json!({ "command": format!("sleep {SLEEP_SECS}") }),
                cancel,
                None,
            )
            .await
            .expect("bash tool should not error on cancellation");
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 5,
            "cancellation path took {elapsed:?}; child kill did not happen in time"
        );
        let text = match &result.content[0] {
            UserContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("[aborted]"),
            "expected aborted marker in output, got: {text}"
        );
        assert!(text.contains("[exit -1]"));
    }

    /// A command that writes a lot to stderr while stdout is also active must not deadlock
    /// the tool. The previous sequential drain (`read stdout → read stderr`) would block
    /// on stdout while the child blocked writing to a full stderr pipe.
    #[tokio::test]
    async fn high_volume_stderr_does_not_deadlock_stdout() {
        let tool = BashTool;
        // Emit 256 KiB on each stream so both pipes saturate. The previous implementation
        // would hang forever waiting for stdout to EOF while the child blocked writing
        // stderr.
        let command = "yes hello | head -c 262144 ; yes world | head -c 262144 1>&2";
        let started = Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tool.execute(
                "t3",
                json!({ "command": command, "timeout": 10 }),
                CancellationToken::new(),
                None,
            ),
        )
        .await
        .expect("bash tool must not hang on high-volume stderr")
        .expect("execute returned error");
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs() < 8,
            "high-volume stderr drain took {elapsed:?}; sequential drain regression?"
        );
        let text = match &result.content[0] {
            UserContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        // Exit 0 expected since the command itself completes.
        assert!(
            text.contains("[exit 0]"),
            "expected clean exit, got: {text}"
        );
        // stderr marker should appear because we wrote a lot to it.
        assert!(text.contains("[stderr]"));
    }

    /// Sanity: a small, fast command still works the same as before.
    #[tokio::test]
    async fn ok_path_still_works() {
        let tool = BashTool;
        let r = tool
            .execute(
                "t4",
                json!({ "command": "echo hello" }),
                CancellationToken::new(),
                None,
            )
            .await
            .expect("simple echo should not error");
        let text = match &r.content[0] {
            UserContentBlock::Text(t) => t.text.clone(),
            _ => panic!("expected text content"),
        };
        assert!(text.contains("hello"));
        assert!(text.contains("[exit 0]"));
    }

    /// Concurrency check: spawn 4 short bash invocations from the same task tree to make
    /// sure the new drain pattern doesn't accidentally serialize them. (Each completes in
    /// well under 1 second; if there's a hidden global lock the wall time blows up.)
    #[tokio::test]
    async fn concurrent_invocations_do_not_serialize() {
        let tool = Arc::new(BashTool);
        let started = Instant::now();
        let mut handles = Vec::new();
        for i in 0..4 {
            let tool = tool.clone();
            handles.push(tokio::spawn(async move {
                tool.execute(
                    &format!("c{i}"),
                    json!({ "command": "sleep 0.3 && echo done" }),
                    CancellationToken::new(),
                    None,
                )
                .await
            }));
        }
        for h in handles {
            h.await.expect("task join").expect("execute should succeed");
        }
        let elapsed = started.elapsed();
        // 4 × 0.3s in parallel ≈ 0.3-0.6s; serialized would be ≥1.2s. Allow 1.5s for CI
        // jitter.
        assert!(
            elapsed.as_secs_f64() < 1.5,
            "concurrent bash calls serialized? elapsed = {elapsed:?}"
        );
    }
}
