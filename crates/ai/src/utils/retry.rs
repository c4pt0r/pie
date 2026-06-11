//! Provider-level HTTP retry. Mirrors the per-provider SDK retry behaviour that the TS pie-ai
//! gets for free from the OpenAI / Anthropic SDKs (default `maxRetries: 2`).
//!
//! Strategy: exponential backoff with jitter on
//!   - 429 (rate limit)
//!   - 5xx (server errors)
//!   - reqwest connection / timeout errors
//!   - `Retry-After` header is honored when present (capped by `max_retry_delay_ms`)
//!
//! On non-retryable status (e.g. 400/401/403) we return the response untouched.

use std::time::Duration;

use crate::types::StreamOptions;
use crate::utils::abort::{self, AbortErrorOrReqwest};

const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_BASE_DELAY_MS: u64 = 500;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum RetrySendError {
    #[error("{0}")]
    Status(String),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error("aborted")]
    Aborted,
    #[error("server requested {requested_ms}ms wait, exceeds cap {cap_ms}ms")]
    DelayTooLong { requested_ms: u64, cap_ms: u64 },
}

impl RetrySendError {
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted)
    }
}

/// Send a `reqwest::RequestBuilder` with retries. Internally uses `try_clone` to rebuild the
/// request between attempts. For JSON / form / text bodies this is cheap; for streaming bodies
/// `try_clone` returns `None` and we degrade to a single-shot send.
pub async fn send_with_retry(
    options: &StreamOptions,
    req: reqwest::RequestBuilder,
) -> Result<reqwest::Response, RetrySendError> {
    let max_retries = options.max_retries.unwrap_or(DEFAULT_MAX_RETRIES);
    let cap_ms = options
        .max_retry_delay_ms
        .unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);

    let Some(template) = req.try_clone() else {
        // Streaming body — can't replay; single-shot.
        return abort::send_or_abort(req, options.abort.as_ref())
            .await
            .map_err(retry_send_error);
    };
    drop(req);

    let mut attempt: u32 = 0;
    loop {
        let attempt_req = match template.try_clone() {
            Some(r) => r,
            None => {
                return Err(RetrySendError::Reqwest(
                    // try_clone failed mid-loop — shouldn't happen since we proved it cloneable
                    // above, but be defensive.
                    reqwest::Client::new()
                        .get("http://_")
                        .build()
                        .err()
                        .unwrap(),
                ));
            }
        };
        let result = abort::send_or_abort(attempt_req, options.abort.as_ref()).await;
        match result {
            Ok(resp) if !is_retryable_status(resp.status()) => return Ok(resp),
            Ok(resp) => {
                if attempt >= max_retries {
                    return Ok(resp);
                }
                let server_delay_ms = retry_after_ms(&resp).unwrap_or(0);
                let delay = backoff_delay(attempt, server_delay_ms, cap_ms);
                tracing::debug!(
                    target: "pie_ai::retry",
                    "retrying after status {} attempt={} delay_ms={}",
                    resp.status(),
                    attempt + 1,
                    delay.as_millis()
                );
                if server_delay_ms > cap_ms && cap_ms > 0 {
                    return Err(RetrySendError::DelayTooLong {
                        requested_ms: server_delay_ms,
                        cap_ms,
                    });
                }
                // Drain the body so the connection can be pooled.
                abort::drain_bytes_or_abort(resp, options.abort.as_ref())
                    .await
                    .map_err(retry_send_error)?;
                abort::sleep_or_abort(delay, options.abort.as_ref())
                    .await
                    .map_err(|_| RetrySendError::Aborted)?;
                attempt += 1;
                continue;
            }
            Err(e) => {
                let AbortErrorOrReqwest::Reqwest(e) = e else {
                    return Err(RetrySendError::Aborted);
                };
                if attempt >= max_retries || !is_retryable_reqwest_error(&e) {
                    return Err(RetrySendError::Reqwest(e));
                }
                let delay = backoff_delay(attempt, 0, cap_ms);
                tracing::debug!(
                    target: "pie_ai::retry",
                    "retrying after transport error attempt={} delay_ms={} err={}",
                    attempt + 1,
                    delay.as_millis(),
                    e
                );
                abort::sleep_or_abort(delay, options.abort.as_ref())
                    .await
                    .map_err(|_| RetrySendError::Aborted)?;
                attempt += 1;
                continue;
            }
        }
    }
}

fn retry_send_error(error: AbortErrorOrReqwest) -> RetrySendError {
    match error {
        AbortErrorOrReqwest::Aborted => RetrySendError::Aborted,
        AbortErrorOrReqwest::Reqwest(e) => RetrySendError::Reqwest(e),
    }
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    let c = status.as_u16();
    // 409: local inference servers (ds4) ask the client to replay the full
    // history; pie always sends the full history, so a plain retry is that replay.
    c == 408 || c == 409 || c == 425 || c == 429 || (500..600).contains(&c)
}

fn is_retryable_reqwest_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request() || e.is_body() || e.is_decode()
}

fn retry_after_ms(resp: &reqwest::Response) -> Option<u64> {
    let header = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let value = header.to_str().ok()?;
    if let Ok(secs) = value.parse::<u64>() {
        return Some(secs * 1000);
    }
    // HTTP-date form is uncommon for LLM providers; skip.
    None
}

fn backoff_delay(attempt: u32, server_delay_ms: u64, cap_ms: u64) -> Duration {
    if server_delay_ms > 0 {
        return Duration::from_millis(server_delay_ms.min(cap_ms.max(1)));
    }
    let base = DEFAULT_BASE_DELAY_MS << attempt.min(6);
    let jitter = (rand::random::<u64>() % 100).saturating_add(1);
    let total = (base + jitter).min(cap_ms.max(1));
    Duration::from_millis(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_categorize() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(reqwest::StatusCode::OK));
    }

    #[test]
    fn conflict_is_retryable() {
        // Local inference servers (e.g. ds4) return 409 when live continuation
        // state was evicted; the documented recovery is to replay the full
        // history, which is exactly what resending the same request does.
        assert!(is_retryable_status(reqwest::StatusCode::CONFLICT));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let d0 = backoff_delay(0, 0, 60_000);
        let d3 = backoff_delay(3, 0, 60_000);
        assert!(d0 < d3);
        let capped = backoff_delay(10, 0, 5_000);
        assert!(capped.as_millis() <= 5_000);
    }
}
