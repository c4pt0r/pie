//! Per-file mutation serialization for the file tools.
//!
//! Ports the safety property of upstream pi's `file-mutation-queue.ts`: the whole
//! read-modify-write of `edit` and the overwrite of `write` must be serialized *per file* so
//! two concurrent tool calls against the same path cannot interleave and lose an update.
//! Distinct files still run in parallel.
//!
//! The lock key is the canonical (realpath) path when the target exists, so two aliases /
//! symlinks to the same inode share one lock. When the target does not exist yet (the common
//! case for `write` creating a new file) we fall back to the canonical parent joined with the
//! file name, and finally to an absolutized-but-not-canonical path — mirroring upstream's
//! "falls back to the resolved path on ENOENT" behavior. The fallbacks are best-effort: they
//! guarantee correctness for the realistic cases (same existing file, same new file in an
//! existing dir) without pretending to resolve unresolvable paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use once_cell::sync::Lazy;
use tokio::sync::Mutex as AsyncMutex;

/// Registry of per-file locks, keyed by resolved path. The `std::sync::Mutex` only guards the
/// brief get-or-insert of the per-key `Arc<AsyncMutex>`; the actual mutation runs while holding
/// the async lock, not this one.
static REGISTRY: Lazy<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
    Lazy::new(|| StdMutex::new(HashMap::new()));

/// Resolve the lock key for `path`. Prefer the realpath so aliases to one inode serialize;
/// degrade gracefully when the file (or its parent) does not exist yet.
async fn lock_key(path: &Path) -> PathBuf {
    if let Ok(canon) = tokio::fs::canonicalize(path).await {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        // `parent()` of a bare relative filename is `""`; canonicalizing that resolves to the
        // current dir, which is the correct anchor for a new file created there.
        let anchor = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        if let Ok(canon_parent) = tokio::fs::canonicalize(anchor).await {
            return canon_parent.join(name);
        }
    }
    absolutize(path)
}

/// Best-effort absolutization without touching the filesystem (used only when canonicalization
/// is impossible). Joins the process cwd for a relative path; leaves an absolute path as-is.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Run `f` while holding the exclusive per-file lock for `path`. Concurrent calls for the same
/// resolved file are serialized; calls for different files proceed in parallel.
pub async fn with_file_lock<F, Fut, T>(path: &Path, f: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let key = lock_key(path).await;
    let lock = {
        let mut reg = REGISTRY.lock().expect("file-lock registry poisoned");
        Arc::clone(
            reg.entry(key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    };
    let _guard = lock.lock().await;
    f().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    /// The lock is exclusive per file: overlapping critical sections never run concurrently.
    #[tokio::test]
    async fn serializes_same_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, "x").unwrap();

        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let p = p.clone();
            let inside = Arc::clone(&inside);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                with_file_lock(&p, || async {
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(n, Ordering::SeqCst);
                    // Yield so a broken (non-exclusive) lock would let peers enter here.
                    tokio::task::yield_now().await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "at most one critical section may run at a time for the same file"
        );
    }

    /// Different files are not blocked by each other.
    #[tokio::test]
    async fn distinct_files_run_in_parallel() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();

        // Hold a's lock, then confirm b's lock is still acquirable without waiting for a.
        let held = with_file_lock(&a, || async {
            // While inside a's critical section, b must be independently lockable.
            with_file_lock(&b, || async { 42 }).await
        })
        .await;
        assert_eq!(held, 42);
    }
}
