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

/// Canonical name of the built-in hub MCP server. Lives here (the universally-included
/// low-level module) so both `mcp_loader` and the trigger hook can reference it.
pub const HUB_SERVER_NAME: &str = "pie-hub";

/// How the built-in `pie-hub` notifications are delivered into the session.
///
/// Unlike per-server `inject_summary` / `inject_and_run` flags in `mcp.toml`, this is a
/// live, user-facing setting (`--hub-inject`, `/config hub.inject`) resolved at call time
/// in [`crate::triggers::direct_inject_action_hook`], so changes apply without a restart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
#[repr(u8)]
pub enum HubInjectMode {
    /// Default: notifications run through the normal trigger path; nothing is injected
    /// into the main conversation.
    #[default]
    Off = 0,
    /// Inject the notification summary into the main conversation verbatim (no model turn).
    Summary = 1,
    /// Inject the summary and run one model turn so the agent reacts in its full context.
    Run = 2,
}

impl HubInjectMode {
    /// Stable token used in `config.toml` and CLI/`/config` surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            HubInjectMode::Off => "off",
            HubInjectMode::Summary => "summary",
            HubInjectMode::Run => "run",
        }
    }

    /// Parse a `config.toml` / CLI token. Returns `None` for unknown values.
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim() {
            "off" => Some(HubInjectMode::Off),
            "summary" => Some(HubInjectMode::Summary),
            "run" => Some(HubInjectMode::Run),
            _ => None,
        }
    }
}

static HUB_NOTIFY_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Set the process-wide built-in hub notification delivery mode. Read live by the trigger
/// action hook, so callers (startup resolution, `/config`) take effect immediately.
pub fn set_hub_inject_mode(mode: HubInjectMode) {
    HUB_NOTIFY_MODE.store(mode as u8, std::sync::atomic::Ordering::Relaxed);
}

/// Current built-in hub notification delivery mode.
pub fn hub_inject_mode() -> HubInjectMode {
    match HUB_NOTIFY_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => HubInjectMode::Summary,
        2 => HubInjectMode::Run,
        _ => HubInjectMode::Off,
    }
}

/// The live hub inject mode if `server_name` is the built-in hub, else `None`. Lets the
/// trigger hook route the hub without depending on `mcp_loader`.
pub fn hub_inject_mode_for_server(server_name: &str) -> Option<HubInjectMode> {
    (server_name == HUB_SERVER_NAME).then(hub_inject_mode)
}

/// Parse the `[hub] inject = "off|summary|run"` setting from `config.toml`.
///
/// Returns the raw token (validated against the known set) or `None` when unset. Unknown
/// sections/keys are ignored, matching [`parse_trigger_poll_interval_secs`].
pub fn parse_hub_inject_setting(toml_text: &str) -> Result<Option<String>, String> {
    let parsed: ConfigFile =
        toml::from_str(toml_text).map_err(|e| format!("parse config.toml: {e}"))?;
    let Some(token) = parsed.hub.and_then(|section| section.inject) else {
        return Ok(None);
    };
    if HubInjectMode::from_token(&token).is_none() {
        return Err(format!(
            "`[hub] inject` must be one of off, summary, run (got {token:?})"
        ));
    }
    Ok(Some(token))
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    triggers: Option<TriggerConfigSection>,
    hub: Option<HubConfigSection>,
}

#[derive(Debug, Deserialize)]
struct TriggerConfigSection {
    poll_interval_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct HubConfigSection {
    inject: Option<String>,
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

    #[test]
    fn hub_inject_mode_token_round_trip() {
        for mode in [
            HubInjectMode::Off,
            HubInjectMode::Summary,
            HubInjectMode::Run,
        ] {
            assert_eq!(HubInjectMode::from_token(mode.as_str()), Some(mode));
        }
        assert_eq!(
            HubInjectMode::from_token("  run  "),
            Some(HubInjectMode::Run)
        );
        assert_eq!(HubInjectMode::from_token("loud"), None);
        assert_eq!(HubInjectMode::default(), HubInjectMode::Off);
    }

    #[test]
    fn hub_inject_mode_for_server_matches_only_built_in_hub() {
        set_hub_inject_mode(HubInjectMode::Run);
        assert_eq!(
            hub_inject_mode_for_server(HUB_SERVER_NAME),
            Some(HubInjectMode::Run)
        );
        assert_eq!(hub_inject_mode_for_server("other-server"), None);
        set_hub_inject_mode(HubInjectMode::Off);
    }

    #[test]
    fn parse_hub_inject_reads_known_values() {
        for mode in ["off", "summary", "run"] {
            let text = format!("[hub]\ninject = \"{mode}\"\n");
            assert_eq!(
                parse_hub_inject_setting(&text).unwrap(),
                Some(mode.to_string())
            );
        }
    }

    #[test]
    fn parse_hub_inject_defaults_when_missing() {
        assert_eq!(parse_hub_inject_setting("").unwrap(), None);
        assert_eq!(parse_hub_inject_setting("[triggers]\n").unwrap(), None);
    }

    #[test]
    fn parse_hub_inject_rejects_unknown_value() {
        let text = "[hub]\ninject = \"loud\"\n";
        assert!(parse_hub_inject_setting(text).is_err());
    }
}
