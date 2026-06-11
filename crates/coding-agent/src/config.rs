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

/// Default public relay endpoint for `/web-connect` (issue #22). Override with
/// `[relay] base_url` in `~/.pie/config.toml` (e.g. a wrangler dev instance).
pub const DEFAULT_RELAY_BASE_URL: &str = "https://pie.0xfefe.me";

/// Parse `[relay] base_url` from config.toml text. Returns the default when absent.
pub fn parse_relay_base_url(toml_text: &str) -> Result<String, String> {
    let parsed: ConfigFile =
        toml::from_str(toml_text).map_err(|e| format!("parse config.toml: {e}"))?;
    let Some(url) = parsed.relay.and_then(|section| section.base_url) else {
        return Ok(DEFAULT_RELAY_BASE_URL.to_string());
    };
    let trimmed = url.trim().trim_end_matches('/').to_string();
    if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
        return Err("`[relay] base_url` must start with http(s)://".into());
    }
    Ok(trimmed)
}

/// Read the relay base URL from `<base_dir>/config.toml`, falling back to the default
/// on missing file. Parse errors are returned so the command can surface them.
pub async fn relay_base_url() -> Result<String, String> {
    let path = base_dir().join("config.toml");
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => parse_relay_base_url(&text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(DEFAULT_RELAY_BASE_URL.to_string())
        }
        Err(err) => Err(format!("read {}: {err}", path.display())),
    }
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    triggers: Option<TriggerConfigSection>,
    relay: Option<RelayConfigSection>,
}

#[derive(Debug, Deserialize)]
struct RelayConfigSection {
    base_url: Option<String>,
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
    fn parse_relay_base_url_reads_override_and_defaults() {
        assert_eq!(parse_relay_base_url("").unwrap(), DEFAULT_RELAY_BASE_URL);
        let text = "[relay]\nbase_url = \"http://127.0.0.1:8787/\"\n";
        assert_eq!(parse_relay_base_url(text).unwrap(), "http://127.0.0.1:8787");
        assert!(parse_relay_base_url("[relay]\nbase_url = \"ftp://x\"\n").is_err());
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
