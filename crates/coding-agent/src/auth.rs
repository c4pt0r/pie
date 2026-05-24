//! Persistent credential store for `pie`. Foundation for c4pt0r/pie#13: stores per-provider
//! credentials at `~/.pie/auth.json` with mode 0600. /login + /logout populate it;
//! resolve_for_provider plumbs into model auto-detection and the CLI stream wrapper.
//!
//! Resolution precedence (in `resolve_for_provider`):
//!   1. The provider's environment variable, if set and non-empty.
//!   2. A matching entry in `auth.json`.
//!   3. None.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::base_dir;

pub fn auth_path() -> PathBuf {
    base_dir().join("auth.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderCredential {
    ApiKey {
        value: String,
    },
    Oauth {
        access_token: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        /// Unix epoch seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<i64>,
        #[serde(default)]
        scopes: Vec<String>,
    },
}

impl ProviderCredential {
    /// True if this is an OAuth credential whose expires_at is in the past or within `slack`
    /// seconds from now. Wired in by the OAuth refresher (lands in a follow-up).
    #[allow(dead_code)]
    pub fn needs_refresh(&self, slack_seconds: i64) -> bool {
        match self {
            Self::ApiKey { .. } => false,
            Self::Oauth { expires_at, .. } => match expires_at {
                Some(exp) => {
                    let now = chrono::Utc::now().timestamp();
                    now + slack_seconds >= *exp
                }
                None => false,
            },
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AuthStore {
    /// Schema version — incremented on breaking changes; the loader tolerates older versions.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Provider id → credential.
    #[serde(default)]
    pub providers: HashMap<String, ProviderCredential>,
}

fn default_version() -> u32 {
    1
}

impl AuthStore {
    pub fn load() -> Result<Self> {
        Self::load_from(&auth_path())
    }

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let store: AuthStore =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&auth_path())
    }

    /// Atomic write: rename-temp + chmod 600. Best-effort on platforms without unix perms.
    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp, perms).ok();
        }
        std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    pub fn set(&mut self, provider: impl Into<String>, cred: ProviderCredential) {
        self.providers.insert(provider.into(), cred);
    }

    pub fn remove(&mut self, provider: &str) -> Option<ProviderCredential> {
        self.providers.remove(provider)
    }

    pub fn get(&self, provider: &str) -> Option<&ProviderCredential> {
        self.providers.get(provider)
    }

    /// Resolve a credential for `provider`. Env var wins; auth.json is the fallback. Returns
    /// the bare API-key string for `api_key` and the access token for `oauth`.
    pub fn resolve_for_provider(&self, provider: &str) -> Option<String> {
        for env_var in pie_ai::env_api_keys::env_var_names(provider) {
            if let Ok(v) = std::env::var(env_var) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            }
        }
        match self.providers.get(provider)? {
            ProviderCredential::ApiKey { value } => Some(value.clone()),
            ProviderCredential::Oauth { access_token, .. } => Some(access_token.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

        fn remove(key: &'static str) -> Self {
            let original = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
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

    #[test]
    fn round_trip_api_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set(
            "anthropic",
            ProviderCredential::ApiKey {
                value: "sk-test".into(),
            },
        );
        store.save_to(&path).unwrap();
        let reloaded = AuthStore::load_from(&path).unwrap();
        assert_eq!(reloaded.providers.len(), 1);
        match reloaded.get("anthropic").unwrap() {
            ProviderCredential::ApiKey { value } => assert_eq!(value, "sk-test"),
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn round_trip_oauth() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set(
            "anthropic",
            ProviderCredential::Oauth {
                access_token: "tok".into(),
                refresh_token: Some("rtok".into()),
                expires_at: Some(1_900_000_000),
                scopes: vec!["chat".into()],
            },
        );
        store.save_to(&path).unwrap();
        let reloaded = AuthStore::load_from(&path).unwrap();
        match reloaded.get("anthropic").unwrap() {
            ProviderCredential::Oauth {
                access_token,
                refresh_token,
                expires_at,
                scopes,
            } => {
                assert_eq!(access_token, "tok");
                assert_eq!(refresh_token.as_deref(), Some("rtok"));
                assert_eq!(expires_at, &Some(1_900_000_000));
                assert_eq!(scopes, &vec!["chat".to_string()]);
            }
            _ => panic!("wrong kind"),
        }
    }

    #[test]
    fn needs_refresh_evaluates_expiry_slack() {
        let now = chrono::Utc::now().timestamp();
        let expires_soon = ProviderCredential::Oauth {
            access_token: "x".into(),
            refresh_token: None,
            expires_at: Some(now + 30),
            scopes: vec![],
        };
        assert!(expires_soon.needs_refresh(60));
        assert!(!expires_soon.needs_refresh(10));
        let api_key = ProviderCredential::ApiKey { value: "x".into() };
        assert!(!api_key.needs_refresh(0));
    }

    #[test]
    fn missing_file_loads_empty_store() {
        let dir = TempDir::new().unwrap();
        let store = AuthStore::load_from(&dir.path().join("nope.json")).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn resolve_for_provider_uses_shared_provider_env_map() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _deepseek = EnvGuard::set("DEEPSEEK_API_KEY", "sk-deepseek-env");
        let _openai = EnvGuard::set("OPENAI_API_KEY", "sk-openai-should-not-count");

        let store = AuthStore::default();
        assert_eq!(
            store.resolve_for_provider("deepseek").as_deref(),
            Some("sk-deepseek-env")
        );

        drop(_deepseek);
        let _deepseek_removed = EnvGuard::remove("DEEPSEEK_API_KEY");
        assert_eq!(store.resolve_for_provider("deepseek"), None);
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.set("p", ProviderCredential::ApiKey { value: "k".into() });
        store.save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {:o}", mode);
    }
}
