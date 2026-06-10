//! Paths and identity. Mirrors `packages/coding-agent/src/config.ts` in spirit — one source of
//! truth for `~/.pie/...` and the cwd-hash directory layout.

use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Base directory: `${PIE_DIR:-$HOME/.pie}`.
pub fn base_dir() -> PathBuf {
    if let Ok(p) = std::env::var("PIE_DIR") {
        return PathBuf::from(p);
    }
    directories::BaseDirs::new()
        .map(|d| d.home_dir().join(".pie"))
        .unwrap_or_else(|| PathBuf::from(".pie"))
}

/// Sessions live under `<base>/sessions/<cwd-hash>/<uuidv7>.jsonl`. Hashing the cwd lets us
/// scope `--resume` to "last session opened from this directory".
pub fn sessions_dir_for_cwd(cwd: &std::path::Path) -> PathBuf {
    let hash = cwd_hash(cwd);
    base_dir().join("sessions").join(hash)
}

/// Memory dir is global (not per-cwd) — that's the whole point of cross-session memory.
pub fn memory_dir() -> PathBuf {
    base_dir().join("memory")
}

/// Deterministic short hash of an absolute cwd path. Same input → same dir, so reopening from
/// the same project always finds prior sessions.
pub fn cwd_hash(cwd: &std::path::Path) -> String {
    let mut h = Sha256::new();
    h.update(cwd.to_string_lossy().as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..6]) // 12 chars; plenty for low-collision per-cwd buckets
}

/// Parse the `[triggers] poll_interval_secs = N` setting from `config.toml`.
///
/// Unknown sections and keys are ignored so feature-specific readers can coexist while the
/// config surface is still small.
pub fn parse_trigger_poll_interval_secs(toml_text: &str) -> Result<Option<u64>, String> {
    let parsed: ConfigFile =
        toml::from_str(toml_text).map_err(|e| format!("parse config.toml: {e}"))?;
    let Some(secs) = parsed
        .triggers
        .and_then(|section| section.poll_interval_secs)
    else {
        return Ok(None);
    };
    if secs == 0 {
        return Err("`[triggers] poll_interval_secs` must be at least 1".into());
    }
    Ok(Some(secs))
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    triggers: Option<TriggerConfigSection>,
}

#[derive(Debug, Deserialize)]
struct TriggerConfigSection {
    poll_interval_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trigger_poll_interval_reads_config_value() {
        let text = r#"
[triggers]
poll_interval_secs = 15
"#;
        assert_eq!(parse_trigger_poll_interval_secs(text).unwrap(), Some(15));
    }

    #[test]
    fn parse_trigger_poll_interval_defaults_when_missing() {
        assert_eq!(parse_trigger_poll_interval_secs("").unwrap(), None);
    }

    #[test]
    fn parse_trigger_poll_interval_rejects_zero() {
        let text = r#"
[triggers]
poll_interval_secs = 0
"#;
        assert!(parse_trigger_poll_interval_secs(text).is_err());
    }
}
