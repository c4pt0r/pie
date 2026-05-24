//! `web_fetch` built-in tool. GETs a URL, returns the body as text (HTML stripped to a
//! readable plain-text form for v1; a proper readability pass is a follow-up under #11).
//!
//! Guards: 15s timeout, 5 MiB body cap, plain GET only (no auth headers, no redirects beyond
//! 10). Errors surface as tool errors so the LLM sees a clear message and can adjust.
//!
//! Body cap is enforced **streaming** via `Response::chunk` — we stop reading as soon as the
//! accumulator passes `MAX_BODY_BYTES` and drop the response so the connection closes. The
//! prior implementation called `resp.bytes().await`, which buffered the entire body in
//! memory before checking the cap; a hostile or buggy server could OOM the agent with a
//! single response.

use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use pie_agent_core::{
    AgentTool, AgentToolError, AgentToolResult, AgentToolUpdate, ToolExecutionMode,
};
use pie_ai::{Tool, UserContentBlock};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TIMEOUT_SECS: u64 = 15;
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;

pub struct WebFetchTool;

#[async_trait]
impl AgentTool for WebFetchTool {
    fn definition(&self) -> &Tool {
        &DEFINITION
    }
    fn label(&self) -> &str {
        "web_fetch"
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        Some(ToolExecutionMode::Parallel)
    }

    async fn execute(
        &self,
        _id: &str,
        params: Value,
        cancel: CancellationToken,
        _on_update: Option<AgentToolUpdate>,
    ) -> Result<AgentToolResult, AgentToolError> {
        let url = params
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentToolError::Message("missing required arg: url".into()))?
            .to_string();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .user_agent(format!("pie/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AgentToolError::Message(format!("http client init: {e}")))?;

        let fut = client.get(&url).send();
        let mut resp = tokio::select! {
            r = fut => r.map_err(|e| AgentToolError::Message(format!("fetch failed: {e}")))?,
            _ = cancel.cancelled() => {
                return Err(AgentToolError::Message("cancelled".into()));
            }
        };

        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let (body, truncated) = read_body_capped(&mut resp, MAX_BODY_BYTES, &cancel).await?;
        // Drop the response once we have what we need so the connection closes and the
        // server stops streaming (matters most in the truncated branch).
        drop(resp);

        let text = String::from_utf8_lossy(&body).to_string();
        let rendered = if content_type.contains("html") {
            html_to_text(&text)
        } else {
            text
        };

        let header = format!(
            "GET {url}\nstatus: {status}\ncontent-type: {content_type}\nbytes: {}{}\n\n",
            body.len(),
            if truncated { " (truncated)" } else { "" }
        );
        Ok(AgentToolResult {
            content: vec![UserContentBlock::text(format!("{header}{rendered}"))],
            details: json!({
                "url": url,
                "status": status.as_u16(),
                "content_type": content_type,
                "bytes": body.len(),
                "truncated": truncated,
            }),
            terminate: None,
        })
    }
}

/// Stream-read the response body until either EOF or `cap` bytes have been accumulated.
/// Returns the captured bytes and whether the body was longer than `cap`.
///
/// This is the core of the streaming cap: the previous `resp.bytes().await` buffered every
/// byte the server sent before the caller could check the size. By draining via
/// `Response::chunk` we cap memory at `cap + one chunk` and let the caller drop the response
/// so the connection closes immediately when we have enough.
async fn read_body_capped(
    resp: &mut reqwest::Response,
    cap: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, bool), AgentToolError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk_result = tokio::select! {
            r = resp.chunk() => r,
            _ = cancel.cancelled() => {
                return Err(AgentToolError::Message("cancelled".into()));
            }
        };
        match chunk_result {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > cap {
                    // Take only what fits; flag truncation and stop reading. Caller drops
                    // the response so the connection closes.
                    let remaining = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    return Ok((buf, true));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok((buf, false)),
            Err(e) => return Err(AgentToolError::Message(format!("read body: {e}"))),
        }
    }
}

/// Minimal HTML → text. Strips tags, decodes a small set of entities, collapses whitespace.
/// Not a readability pass (no main-content detection); good enough that the LLM can read a
/// docs page without drowning in markup.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script_or_style: Option<&'static str> = None;
    let lower = html.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Inside <script> or <style>, skip everything until matching close tag.
        if let Some(close) = in_script_or_style {
            if starts_with_at(lower_bytes, i, close.as_bytes()) {
                in_script_or_style = None;
                i += close.len();
                continue;
            }
            let ch = html[i..]
                .chars()
                .next()
                .expect("loop index must be at a char boundary");
            i += ch.len_utf8();
            continue;
        }
        let c = html[i..]
            .chars()
            .next()
            .expect("loop index must be at a char boundary");
        if !in_tag && c == '<' {
            if starts_with_at(lower_bytes, i, b"<script") {
                in_script_or_style = Some("</script>");
                i += "<script".len();
                continue;
            }
            if starts_with_at(lower_bytes, i, b"<style") {
                in_script_or_style = Some("</style>");
                i += "<style".len();
                continue;
            }
            in_tag = true;
            // Treat block-level boundaries as newlines for readability.
            if starts_with_at(lower_bytes, i, b"<br")
                || starts_with_at(lower_bytes, i, b"<p")
                || starts_with_at(lower_bytes, i, b"</p")
                || starts_with_at(lower_bytes, i, b"<div")
                || starts_with_at(lower_bytes, i, b"</div")
                || starts_with_at(lower_bytes, i, b"<li")
                || starts_with_at(lower_bytes, i, b"</li")
                || starts_with_at(lower_bytes, i, b"<h")
            {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if in_tag {
            if c == '>' {
                in_tag = false;
            }
            i += c.len_utf8();
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    // Decode a tiny set of HTML entities — full table is overkill for v1.
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    collapse_whitespace(&out)
}

fn starts_with_at(bytes: &[u8], i: usize, pat: &[u8]) -> bool {
    bytes.get(i..).is_some_and(|tail| tail.starts_with(pat))
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    let mut consecutive_newlines = 0u8;
    for c in s.chars() {
        if c == '\n' {
            consecutive_newlines = consecutive_newlines.saturating_add(1);
            if consecutive_newlines <= 2 {
                out.push('\n');
            }
            last_was_space = false;
            continue;
        }
        consecutive_newlines = 0;
        if c.is_whitespace() {
            if !last_was_space && !out.ends_with('\n') {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        last_was_space = false;
        out.push(c);
    }
    out.trim().to_string()
}

static DEFINITION: Lazy<Tool> = Lazy::new(|| {
    Tool {
    name: "web_fetch".into(),
    description: "Fetch a URL via HTTP GET. Returns headers + body. For HTML pages, tags are stripped to plain text. Body cap 5 MiB; 15s timeout.".into(),
    parameters: json!({
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "Absolute http(s) URL to fetch.",
            },
        },
        "required": ["url"],
        "additionalProperties": false,
    }),
}
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_tags_and_decodes_entities() {
        let html = "<html><body><h1>Title</h1><p>Hello &amp; world</p><script>alert(1)</script></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & world"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn html_to_text_preserves_non_ascii_text() {
        let html = "<html><body><p>你好，世界</p><div>emoji: 🦀</div></body></html>";
        let text = html_to_text(html);

        assert!(text.contains("你好，世界"));
        assert!(text.contains("emoji: 🦀"));
    }

    #[test]
    fn html_to_text_handles_replacement_char_from_truncated_utf8() {
        let html =
            "<html><body><script>const x = 'ignored � text';</script><p>done �</p></body></html>";
        let text = html_to_text(html);

        assert_eq!(text, "done �");
    }

    #[test]
    fn html_to_text_handles_nbsp_inside_script_without_byte_boundary_panic() {
        let html =
            "<html><body><script>const x = 'ignored\u{a0}text';</script><p>done</p></body></html>";
        let text = html_to_text(html);

        assert_eq!(text, "done");
    }

    #[test]
    fn collapse_whitespace_keeps_paragraph_breaks() {
        let s = "a   b\n\n\n\nc";
        assert_eq!(collapse_whitespace(s), "a b\n\nc");
    }
}
