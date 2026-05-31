//! `/bug-report` builder. Writes a single text dump to
//! `~/.pie/bug-reports/<utc-stamp>.txt` containing:
//!
//! 1. Diagnostic snapshot (model / thinking / tools / cost).
//! 2. Tail of the active session log file (up to 200 lines).
//! 3. The session transcript (rendered via `crate::export::render`).
//!
//! Everything goes through a redactor that strips well-known secret patterns. Bug reports are
//! the canonical "give me something to attach to an issue" artifact, so we trade detail for
//! safety: the redactor is conservative.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use once_cell::sync::Lazy;
use pie_agent_core::Session;
use regex::Regex;

use crate::config::base_dir;

const MAX_LOG_LINES: usize = 200;

pub fn default_dest() -> PathBuf {
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    base_dir().join("bug-reports").join(format!("{stamp}.txt"))
}

/// Snapshot of harness state that lives outside the harness itself. The caller fills it in
/// from CommandCtx so this module stays decoupled from the slash-command layer.
pub struct DiagInputs {
    pub session_id: String,
    pub model: Option<String>,
    pub thinking: String,
    pub tool_count: usize,
    pub skill_count: usize,
    pub cost_summary: String,
    pub log_path: Option<PathBuf>,
}

pub async fn build(diag: DiagInputs, session: &Session, dest: &Path) -> Result<PathBuf> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create bug-reports dir {}", parent.display()))?;
    }

    let mut body = String::new();
    body.push_str("pie bug report\n");
    body.push_str(&format!("generated_at: {}\n", Utc::now().to_rfc3339()));
    body.push_str(&format!("pie_version: {}\n", env!("CARGO_PKG_VERSION")));
    body.push('\n');

    body.push_str("---- diagnostic ----\n");
    body.push_str(&format!("session_id    {}\n", diag.session_id));
    body.push_str(&format!(
        "model         {}\n",
        diag.model.as_deref().unwrap_or("(none)")
    ));
    body.push_str(&format!("thinking      {}\n", diag.thinking));
    body.push_str(&format!("tools         {}\n", diag.tool_count));
    body.push_str(&format!("skills        {}\n", diag.skill_count));
    body.push_str(&format!("cost          {}\n", diag.cost_summary));
    body.push_str(&format!(
        "log_path      {}\n",
        diag.log_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(disabled)".into())
    ));
    body.push('\n');

    if let Some(log) = diag.log_path.as_ref() {
        body.push_str(&format!(
            "---- log tail ({} lines from {}) ----\n",
            MAX_LOG_LINES,
            log.display()
        ));
        match tokio::fs::read_to_string(log).await {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let tail = if lines.len() > MAX_LOG_LINES {
                    &lines[lines.len() - MAX_LOG_LINES..]
                } else {
                    &lines[..]
                };
                for line in tail {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            Err(e) => {
                body.push_str(&format!("(cannot read log: {e})\n"));
            }
        }
        body.push('\n');
    }

    body.push_str("---- transcript ----\n");
    match crate::export::render(session).await {
        Ok(transcript) => body.push_str(&transcript),
        Err(e) => body.push_str(&format!("(cannot render transcript: {e})\n")),
    }

    let redacted = redact(&body);
    tokio::fs::write(dest, redacted)
        .await
        .with_context(|| format!("write {}", dest.display()))?;
    Ok(dest.to_path_buf())
}

/// Apply every secret-pattern regex to `input`. Each match is replaced with a fixed
/// placeholder that names which class of secret was caught so the user can verify which
/// rules fired without leaking detail.
pub fn redact(input: &str) -> String {
    let mut out = input.to_string();
    for (label, re) in REDACTORS.iter() {
        out = re
            .replace_all(&out, format!("[REDACTED:{label}]"))
            .into_owned();
    }
    out
}

static REDACTORS: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    let raw: Vec<(&'static str, &'static str)> = vec![
        // OpenAI / Anthropic / Stripe-style keys ("sk-..." prefix, 20+ alnum after).
        ("openai_anthropic_key", r"sk-[A-Za-z0-9_-]{20,}"),
        // AWS access key id (always 20 chars, AKIA or ASIA prefix).
        ("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        // GitHub PATs (40 chars after `gho_` / `ghp_` / `ghu_` / `ghs_`).
        ("github_token", r"\bgh[ousp]_[A-Za-z0-9]{30,}\b"),
        // Slack tokens.
        ("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b"),
        // Google API keys (39 chars after AIza).
        ("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
        // Generic Bearer tokens in HTTP-style strings.
        ("bearer_token", r"Bearer\s+[A-Za-z0-9._\-]{16,}"),
        // Hub browser login and loopback callback URLs can carry auth state or one-time codes.
        ("pie_hub_login_url", r"https?://[^\s]+/login\?[^\s]+"),
        (
            "pie_hub_callback_url",
            r"http://127\.0\.0\.1:[0-9]+/callback(?:\?[^\s]+)?",
        ),
        // pie hub session / agent credentials can appear as bare values in transport errors.
        ("pie_hub_token", r"\bhub_(?:agent|hs)_[A-Za-z0-9._\-]{8,}\b"),
        // Hub/user-visible diagnostics should not expose raw immutable IDs.
        (
            "uuid",
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        ),
    ];
    raw.into_iter()
        .map(|(label, src)| (label, Regex::new(src).expect("regex must compile")))
        .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_known_patterns() {
        let s = "key=sk-abcdefghij1234567890abcd , aws=AKIAEXAMPLEEXAMPLE1A, gh=gho_abcdefghijklmnopqrstuvwxyz0123456789, slack=xoxb-1234567890-abcdef, header=Authorization: Bearer eyJabc.defghijklmnopqr, login=https://pie.0xfefe.me/login?req=018fe23a-1111-4a22-8b33-123456789abc&state=state_secret, callback=http://127.0.0.1:49152/callback?code=hub_code_secret&state=state_secret, hub=hub_agent_abcdefghijklmnopqrstuvwxyz, session=hub_hs_abcdefghijklmnopqrstuvwxyz, id=018fe23a-1111-4a22-8b33-123456789abc";
        let r = redact(s);
        assert!(!r.contains("sk-abcdefghij"), "openai key leaked: {r}");
        assert!(!r.contains("AKIAEXAMPLE"), "aws key leaked: {r}");
        assert!(!r.contains("gho_"), "github token leaked: {r}");
        assert!(!r.contains("xoxb-"), "slack token leaked: {r}");
        assert!(!r.contains("eyJabc.defghijklmnopqr"), "bearer leaked: {r}");
        assert!(
            !r.contains("pie.0xfefe.me/login"),
            "hub login URL leaked: {r}"
        );
        assert!(
            !r.contains("127.0.0.1:49152/callback"),
            "hub callback URL leaked: {r}"
        );
        assert!(!r.contains("hub_agent_"), "hub agent token leaked: {r}");
        assert!(!r.contains("hub_hs_"), "hub session token leaked: {r}");
        assert!(!r.contains("018fe23a-1111"), "uuid leaked: {r}");
        assert!(r.contains("[REDACTED:openai_anthropic_key]"));
        assert!(r.contains("[REDACTED:aws_access_key]"));
        assert!(r.contains("[REDACTED:pie_hub_login_url]"));
        assert!(r.contains("[REDACTED:pie_hub_callback_url]"));
        assert!(r.contains("[REDACTED:pie_hub_token]"));
        assert!(redact("id=018fe23a-1111-4a22-8b33-123456789abc").contains("[REDACTED:uuid]"));
    }

    #[test]
    fn redact_leaves_normal_text_alone() {
        let s = "hello world, no secrets here";
        assert_eq!(redact(s), s);
    }
}
