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
use tokio::process::Command;
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
        // Builds a `sh -c <command>` child with piped stdout/stderr and `kill_on_drop` so any
        // early return from this function (cancel, timeout, or our own `?` exits) tears down
        // the subprocess instead of leaving it running. Stdout and stderr are drained on
        // separate tasks; reading them serially would deadlock if either pipe filled before
        // the other was read.
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

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string()))?;

        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");

        let on_stdout = options.on_stdout.clone();
        let on_stderr = options.on_stderr.clone();

        let stdout_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut buf = String::new();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(cb) = &on_stdout {
                    cb(&line);
                }
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            let mut buf = String::new();
            while let Ok(Some(line)) = reader.next_line().await {
                if let Some(cb) = &on_stderr {
                    cb(&line);
                }
                buf.push_str(&line);
                buf.push('\n');
            }
            buf
        });

        let abort_token = options.abort.clone();
        let timeout_secs = options.timeout_secs;

        // `biased` so the abort branch wins over a same-tick timer firing — it's the more
        // specific user intent.
        let outcome: ExecOutcome = tokio::select! {
            biased;
            _ = async {
                match &abort_token {
                    Some(token) => token.cancelled().await,
                    None => pending::<()>().await,
                }
            } => ExecOutcome::Aborted,
            _ = async {
                match timeout_secs {
                    Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
                    None => pending::<()>().await,
                }
            } => ExecOutcome::TimedOut,
            res = child.wait() => ExecOutcome::Completed(res),
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
                // Kill first (so pipe drainage can finish), then wait for the reaper and the
                // reader tasks. We don't return partial output via the error path — the
                // streaming callbacks already saw every line the child produced before kill.
                let _ = child.kill().await;
                let _ = child.wait().await;
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
                let _ = child.kill().await;
                let _ = child.wait().await;
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
        assert!(
            elapsed < TokioDuration::from_secs(3),
            "expected exec to return within ~1s after timeout, took {elapsed:?}"
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
