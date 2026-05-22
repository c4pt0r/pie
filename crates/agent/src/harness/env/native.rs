//! Native `ExecutionEnv` — std::fs + tokio::process. Partial 1:1 port of
//! `packages/agent/src/harness/env/nodejs.ts` (~528 lines).
//!
//! Currently exposes everything skills need (file_info, list_dir, read_text_file, canonical,
//! absolute_path, exists). Other methods (write, append, temp dirs, exec) have minimal
//! implementations sufficient for the current test surface; advanced cases (concurrent fs
//! watchers, sandboxed exec) land as TODOs.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::harness::types::*;

pub struct NativeEnv {
    cwd: String,
}

impl NativeEnv {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn current() -> std::io::Result<Self> {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        Ok(Self::new(cwd))
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            Path::new(&self.cwd).join(p)
        }
    }
}

fn map_io_error(e: std::io::Error, path: Option<&str>) -> FileError {
    use std::io::ErrorKind;
    let code = match e.kind() {
        ErrorKind::NotFound => FileErrorCode::NotFound,
        ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        ErrorKind::InvalidInput | ErrorKind::InvalidData => FileErrorCode::InvalidPath,
        _ => FileErrorCode::Unknown,
    };
    let mut err = FileError::new(code, e.to_string());
    if let Some(p) = path {
        err = err.with_path(p);
    }
    err
}

fn file_info_from_meta(name: String, path: String, m: std::fs::Metadata) -> FileInfo {
    let kind = if m.file_type().is_symlink() {
        FileKind::Symlink
    } else if m.is_dir() {
        FileKind::Directory
    } else {
        FileKind::File
    };
    let mtime_ms = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    FileInfo {
        name,
        path,
        kind,
        size: m.len(),
        mtime_ms,
    }
}

#[async_trait]
impl ExecutionEnv for NativeEnv {
    fn cwd(&self) -> &str {
        &self.cwd
    }

    async fn absolute_path(&self, path: &str, _cancel: CancellationToken) -> FsResult<String> {
        Ok(self.resolve(path).to_string_lossy().to_string())
    }

    async fn join_path(&self, parts: &[&str], _cancel: CancellationToken) -> FsResult<String> {
        let mut p = PathBuf::new();
        for part in parts {
            p.push(part);
        }
        Ok(p.to_string_lossy().to_string())
    }

    async fn read_text_file(&self, path: &str, _cancel: CancellationToken) -> FsResult<String> {
        let p = self.resolve(path);
        fs::read_to_string(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        _cancel: CancellationToken,
    ) -> FsResult<Vec<String>> {
        let p = self.resolve(path);
        let file = fs::File::open(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))?;
        let mut reader = tokio::io::BufReader::new(file).lines();
        let mut out = Vec::new();
        let cap = max_lines.unwrap_or(usize::MAX);
        while out.len() < cap {
            match reader.next_line().await {
                Ok(Some(line)) => out.push(line),
                Ok(None) => break,
                Err(e) => return Err(map_io_error(e, Some(path))),
            }
        }
        Ok(out)
    }

    async fn read_binary_file(&self, path: &str, _cancel: CancellationToken) -> FsResult<Vec<u8>> {
        let p = self.resolve(path);
        fs::read(&p).await.map_err(|e| map_io_error(e, Some(path)))
    }

    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        _cancel: CancellationToken,
    ) -> FsResult<()> {
        let p = self.resolve(path);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        fs::write(&p, content)
            .await
            .map_err(|e| map_io_error(e, Some(path)))
    }

    async fn append_file(
        &self,
        path: &str,
        content: &[u8],
        _cancel: CancellationToken,
    ) -> FsResult<()> {
        use tokio::io::AsyncWriteExt;
        let p = self.resolve(path);
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent).await;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))?;
        f.write_all(content)
            .await
            .map_err(|e| map_io_error(e, Some(path)))
    }

    async fn file_info(&self, path: &str, _cancel: CancellationToken) -> FsResult<FileInfo> {
        let p = self.resolve(path);
        let m = fs::symlink_metadata(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))?;
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(file_info_from_meta(
            name,
            p.to_string_lossy().to_string(),
            m,
        ))
    }

    async fn list_dir(&self, path: &str, _cancel: CancellationToken) -> FsResult<Vec<FileInfo>> {
        let p = self.resolve(path);
        let mut rd = fs::read_dir(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))?;
        let mut out = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| map_io_error(e, Some(path)))?
        {
            let m = entry
                .metadata()
                .await
                .map_err(|e| map_io_error(e, Some(&entry.path().to_string_lossy())))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let abs = entry.path().to_string_lossy().to_string();
            out.push(file_info_from_meta(name, abs, m));
        }
        Ok(out)
    }

    async fn exists(&self, path: &str, _cancel: CancellationToken) -> FsResult<bool> {
        let p = self.resolve(path);
        match fs::symlink_metadata(&p).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(map_io_error(e, Some(path))),
        }
    }

    async fn canonical_path(&self, path: &str, _cancel: CancellationToken) -> FsResult<String> {
        let p = self.resolve(path);
        let resolved = fs::canonicalize(&p)
            .await
            .map_err(|e| map_io_error(e, Some(path)))?;
        Ok(resolved.to_string_lossy().to_string())
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        _cancel: CancellationToken,
    ) -> FsResult<()> {
        let p = self.resolve(path);
        let res = if recursive {
            fs::create_dir_all(&p).await
        } else {
            fs::create_dir(&p).await
        };
        res.map_err(|e| map_io_error(e, Some(path)))
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        _force: bool,
        _cancel: CancellationToken,
    ) -> FsResult<()> {
        let p = self.resolve(path);
        let m = match fs::symlink_metadata(&p).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(map_io_error(e, Some(path))),
        };
        let res = if m.is_dir() {
            if recursive {
                fs::remove_dir_all(&p).await
            } else {
                fs::remove_dir(&p).await
            }
        } else {
            fs::remove_file(&p).await
        };
        res.map_err(|e| map_io_error(e, Some(path)))
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        _cancel: CancellationToken,
    ) -> FsResult<String> {
        let p = std::env::temp_dir().join(format!(
            "{}-{}",
            prefix.unwrap_or("tmp-"),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&p)
            .await
            .map_err(|e| map_io_error(e, None))?;
        Ok(p.to_string_lossy().to_string())
    }

    async fn create_temp_file(
        &self,
        prefix: Option<&str>,
        suffix: Option<&str>,
        _cancel: CancellationToken,
    ) -> FsResult<String> {
        let name = format!(
            "{}{}{}",
            prefix.unwrap_or(""),
            uuid::Uuid::new_v4().simple(),
            suffix.unwrap_or("")
        );
        let p = std::env::temp_dir().join(name);
        fs::write(&p, b"")
            .await
            .map_err(|e| map_io_error(e, None))?;
        Ok(p.to_string_lossy().to_string())
    }

    async fn exec(&self, command: &str, options: ExecOptions) -> ExecResult<ExecOutput> {
        // Builds a `sh -c <command>` child with piped stdout/stderr. The child lives in its
        // own process group on Unix so a timeout/abort sends SIGKILL to the entire group —
        // killing only the direct shell would leak descendants like
        // `(sleep 30; touch leak) & wait`. `kill_on_drop(true)` is the last-line backstop if
        // we ever return without explicitly killing (e.g. an `?` exit before the select).
        // Stdout and stderr are drained on separate spawned tasks because a serial drain
        // deadlocks any time one pipe fills before the other is read.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        } else {
            cmd.current_dir(&self.cwd);
        }
        if let Some(env) = &options.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        #[cfg(unix)]
        {
            // SAFETY: this closure runs in the child between fork and exec on Unix. `setsid`
            // is async-signal-safe (POSIX) and has no Rust state to invalidate. The child
            // becomes session and process-group leader; SIGKILL to `-pgid` then targets the
            // whole tree we just spawned. `pre_exec` is exposed on tokio::process::Command
            // via std::os::unix::process::CommandExt without needing a trait import.
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
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string()))?;

        // Snapshot the pid before any drain/select so the kill paths can target the process
        // group even if the underlying `tokio::process::Child` later loses access (e.g. after
        // a wait that consumed it).
        let child_pid = child.id();

        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");

        let on_stdout = options.on_stdout.clone();
        let on_stderr = options.on_stderr.clone();

        let stdout_handle = tokio::spawn(drain_stream(stdout, on_stdout));
        let stderr_handle = tokio::spawn(drain_stream(stderr, on_stderr));

        let abort_token = options.abort.clone();
        let timeout_secs = options.timeout_secs;

        // Use `tokio::time::timeout` instead of racing a `sleep` inside `select!`: the
        // dedicated helper drives the timer the same way Tokio's own primitives do, which is
        // what the failing Ubuntu CI run convinced us we need. Race that against the optional
        // abort token; `biased` keeps abort first so user-issued cancels win same-tick ties.
        let outcome: ExecOutcome = tokio::select! {
            biased;
            _ = async {
                match &abort_token {
                    Some(token) => token.cancelled().await,
                    None => pending::<()>().await,
                }
            } => ExecOutcome::Aborted,
            res = wait_with_optional_timeout(&mut child, timeout_secs) => res,
        };

        match outcome {
            ExecOutcome::Completed(Ok(status)) => {
                // Reader tasks finish naturally when the child closes its pipes on exit.
                let stdout = stdout_handle.await.unwrap_or_default();
                let stderr = stderr_handle.await.unwrap_or_default();
                Ok(ExecOutput {
                    stdout,
                    stderr,
                    exit_code: status.code().unwrap_or(-1),
                })
            }
            ExecOutcome::Completed(Err(e)) => {
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;
                Err(ExecutionError::new(
                    ExecutionErrorCode::Unknown,
                    e.to_string(),
                ))
            }
            ExecOutcome::TimedOut => {
                terminate_child_tree(&mut child, child_pid).await;
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;
                Err(ExecutionError::new(
                    ExecutionErrorCode::Timeout,
                    format!(
                        "command timed out after {}s",
                        timeout_secs.unwrap_or_default()
                    ),
                ))
            }
            ExecOutcome::Aborted => {
                terminate_child_tree(&mut child, child_pid).await;
                let _ = stdout_handle.await;
                let _ = stderr_handle.await;
                Err(ExecutionError::new(
                    ExecutionErrorCode::Aborted,
                    "command aborted",
                ))
            }
        }
    }
}

enum ExecOutcome {
    Completed(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Aborted,
}

async fn wait_with_optional_timeout(child: &mut Child, timeout_secs: Option<u64>) -> ExecOutcome {
    match timeout_secs {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(res) => ExecOutcome::Completed(res),
            Err(_) => ExecOutcome::TimedOut,
        },
        None => ExecOutcome::Completed(child.wait().await),
    }
}

/// Best-effort teardown of the child *and any descendants it spawned*. On Unix the child was
/// placed in its own session/process group via `setsid()`, so a single `killpg(-pid, SIGKILL)`
/// reaches background jobs and detached children. On non-Unix targets we fall back to the
/// direct `kill_on_drop` + `Child::kill` path (no descendants problem because Windows job
/// objects aren't wired up here — that's the larger Windows port story).
async fn terminate_child_tree(child: &mut Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `killpg` with SIGKILL on a known pgid is sound; the pid was just observed
        // from `child.id()`. A zero/-ESRCH return (child already gone) is benign and we don't
        // assert on it.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    // Always also issue `Child::start_kill` so tokio considers the handle terminated. On
    // Unix the SIGKILL above already did the work; this is the cross-platform reaper. The
    // subsequent `wait` reaps the zombie.
    let _ = child.start_kill();
    let _ = child.wait().await;
    let _ = pid;
}

/// Drain a child pipe into a UTF-8 string while also feeding each line into an optional
/// streaming callback. Uses `read_until(b'\n')` + lossy decode so binary-ish output does not
/// truncate at the first invalid byte the way `AsyncBufReadExt::lines()` does (it stops on
/// `Err`, dropping the rest of the stream). The lossy decode is applied per line so the
/// callback receives the same text the buffered tail eventually reports.
async fn drain_stream<R>(
    reader: R,
    callback: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
) -> String
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut br = BufReader::new(reader);
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = Vec::with_capacity(256);
    loop {
        chunk.clear();
        match br.read_until(b'\n', &mut chunk).await {
            Ok(0) => break,
            Ok(_) => {
                // Strip a single trailing '\n' for callback display; keep it in the buffer so
                // the returned string matches what the child wrote.
                let line = if chunk.last() == Some(&b'\n') {
                    String::from_utf8_lossy(&chunk[..chunk.len() - 1]).into_owned()
                } else {
                    String::from_utf8_lossy(&chunk).into_owned()
                };
                if let Some(cb) = &callback {
                    cb(&line);
                }
                buf.extend_from_slice(&chunk);
            }
            // Treat any read error as end-of-stream; we don't want to swallow already-buffered
            // bytes by erroring out mid-drain.
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Instant;
    use tokio::time::{Duration as TokioDuration, timeout};

    fn env() -> NativeEnv {
        NativeEnv::new(std::env::temp_dir().to_string_lossy().to_string())
    }

    #[tokio::test]
    async fn exec_normal_completion_returns_stdout_and_exit_code() {
        let out = env()
            .exec("printf hello; printf world 1>&2", ExecOptions::default())
            .await
            .expect("exec must succeed");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello"), "stdout: {:?}", out.stdout);
        assert!(out.stderr.contains("world"), "stderr: {:?}", out.stderr);
    }

    #[tokio::test]
    async fn exec_preserves_stdout_stderr_without_inventing_trailing_newlines() {
        // Regression for the previous-implementation regression: `AsyncBufReadExt::lines()`
        // would have returned `["hello"]` plus a synthesized `'\n'` push from the wrapping
        // loop, producing `"hello\n"`. `drain_stream` keeps the delimiter the child actually
        // wrote, so a `printf hello` (no trailing newline) round-trips as exactly `"hello"`.
        let out = env()
            .exec("printf hello; printf err 1>&2", ExecOptions::default())
            .await
            .expect("exec must succeed");
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            out.stdout, "hello",
            "stdout must not gain a trailing newline"
        );
        assert_eq!(out.stderr, "err", "stderr must not gain a trailing newline");
    }

    #[tokio::test]
    async fn exec_streaming_callbacks_receive_lines_in_order() {
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = captured.clone();
        let on_stdout: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line: &str| {
            sink.lock().unwrap().push(line.to_string());
        });
        let opts = ExecOptions {
            on_stdout: Some(on_stdout),
            ..ExecOptions::default()
        };
        let out = env()
            .exec("printf 'a\\nb\\nc\\n'", opts)
            .await
            .expect("exec must succeed");
        assert_eq!(out.exit_code, 0);
        let lines = captured.lock().unwrap().clone();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn exec_timeout_returns_timeout_error_and_kills_child_quickly() {
        let opts = ExecOptions {
            timeout_secs: Some(1),
            ..ExecOptions::default()
        };
        let start = Instant::now();
        // 10s sleep; the runtime must kill the child after 1s instead of waiting it out.
        let err = env()
            .exec("sleep 10", opts)
            .await
            .expect_err("must time out");
        assert_eq!(err.code, ExecutionErrorCode::Timeout);
        let elapsed = start.elapsed();
        // Loose ceiling (was tight 3s, but CI under load needs more headroom). What matters
        // is that we don't wait the full 10s the child would have slept for.
        assert!(
            elapsed < TokioDuration::from_secs(6),
            "expected exec to return well before the 10s child sleep, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_timeout_kills_backgrounded_descendant_processes() {
        // Without process-group teardown, a backgrounded child like
        // `(sleep 30; touch ...) & wait` survives the kill of the direct shell. The leak
        // file would appear after the test exits. With `setsid` + `killpg(SIGKILL)` the
        // whole tree dies, so the marker is never written.
        use tempfile::tempdir;
        let dir = tempdir().expect("tempdir");
        let marker = dir.path().join("leak-marker");
        let marker_str = marker.to_string_lossy().to_string();

        let opts = ExecOptions {
            timeout_secs: Some(1),
            ..ExecOptions::default()
        };
        let cmd = format!("(sleep 4; touch {marker_str}) & wait");
        let err = env()
            .exec(&cmd, opts)
            .await
            .expect_err("backgrounded sleep should time out");
        assert_eq!(err.code, ExecutionErrorCode::Timeout);

        // Give the (now-orphaned, but killed) descendant time to touch the marker if it
        // somehow survived. The sleep budget is wider than the descendant's 4s so a missed
        // killpg would unambiguously surface.
        tokio::time::sleep(TokioDuration::from_secs(5)).await;
        assert!(
            !marker.exists(),
            "descendant process was not killed by killpg — leak marker at {marker_str} exists"
        );
    }

    #[tokio::test]
    async fn exec_abort_token_cancellation_returns_aborted_error() {
        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        // Cancel shortly after the call begins.
        tokio::spawn(async move {
            tokio::time::sleep(TokioDuration::from_millis(100)).await;
            cancel_for_task.cancel();
        });
        let opts = ExecOptions {
            abort: Some(cancel),
            timeout_secs: Some(30), // long timeout — abort should win
            ..ExecOptions::default()
        };
        let err = env().exec("sleep 30", opts).await.expect_err("must abort");
        assert_eq!(err.code, ExecutionErrorCode::Aborted);
    }

    #[tokio::test]
    async fn exec_high_stderr_volume_does_not_deadlock_stdout_drain() {
        // Without concurrent stdio drain, a child that fills stderr's pipe buffer before
        // closing stdout would block forever. With concurrent readers, both pipes drain in
        // parallel so the command can finish.
        let opts = ExecOptions {
            timeout_secs: Some(15),
            ..ExecOptions::default()
        };
        // Write ~200 KiB to stderr (well beyond typical 64 KiB pipe buffer) then a small
        // stdout payload after. Use yes/dd? Stick to portable POSIX: a python loop is too
        // assumption-heavy; use printf in a loop via `sh`.
        let cmd = "for i in $(seq 1 4000); do printf 'noise-noise-noise-noise-noise\\n' 1>&2; done; printf done\\n";
        let env_ = env();
        let fut = env_.exec(cmd, opts);
        let out = timeout(TokioDuration::from_secs(20), fut)
            .await
            .expect("must not deadlock — concurrent stdio drain")
            .expect("exec must succeed");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("done"), "stdout: {:?}", out.stdout);
        // Stderr buffer should contain all 4000 lines without truncation.
        let stderr_lines = out.stderr.lines().count();
        assert_eq!(
            stderr_lines, 4000,
            "expected 4000 stderr lines, got {stderr_lines}"
        );
    }
}
