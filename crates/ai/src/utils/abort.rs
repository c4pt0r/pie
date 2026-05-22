//! Shared abort helpers for provider HTTP send, retry sleep, and stream drain paths.

use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::types::{
    AssistantMessage, AssistantMessageEvent, AssistantRole, ErrorReason, Model, StopReason, Usage,
};
use crate::utils::event_stream::AssistantMessageEventSender;

#[derive(Debug, thiserror::Error)]
#[error("aborted")]
pub struct AbortError;

pub enum AbortableNext<T, E> {
    Item(Result<T, E>),
    Eof,
    Aborted,
}

pub async fn send_or_abort(
    req: reqwest::RequestBuilder,
    abort: Option<&CancellationToken>,
) -> Result<reqwest::Response, AbortErrorOrReqwest> {
    if let Some(token) = abort {
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(AbortErrorOrReqwest::Aborted),
            result = req.send() => result.map_err(AbortErrorOrReqwest::Reqwest),
        }
    } else {
        req.send().await.map_err(AbortErrorOrReqwest::Reqwest)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AbortErrorOrReqwest {
    #[error("aborted")]
    Aborted,
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
}

pub async fn sleep_or_abort(
    duration: std::time::Duration,
    abort: Option<&CancellationToken>,
) -> Result<(), AbortError> {
    if let Some(token) = abort {
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(AbortError),
            _ = tokio::time::sleep(duration) => Ok(()),
        }
    } else {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

pub async fn drain_bytes_or_abort(
    resp: reqwest::Response,
    abort: Option<&CancellationToken>,
) -> Result<(), AbortErrorOrReqwest> {
    if let Some(token) = abort {
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(AbortErrorOrReqwest::Aborted),
            result = resp.bytes() => result.map(|_| ()).map_err(AbortErrorOrReqwest::Reqwest),
        }
    } else {
        resp.bytes()
            .await
            .map(|_| ())
            .map_err(AbortErrorOrReqwest::Reqwest)
    }
}

pub async fn response_text_or_abort(
    resp: reqwest::Response,
    abort: Option<&CancellationToken>,
) -> Result<String, AbortErrorOrReqwest> {
    if let Some(token) = abort {
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(AbortErrorOrReqwest::Aborted),
            result = resp.text() => result.map_err(AbortErrorOrReqwest::Reqwest),
        }
    } else {
        resp.text().await.map_err(AbortErrorOrReqwest::Reqwest)
    }
}

pub async fn next_or_abort<S, T, E>(
    stream: &mut S,
    abort: Option<&CancellationToken>,
) -> AbortableNext<T, E>
where
    S: Stream<Item = Result<T, E>> + Unpin,
{
    let item = if let Some(token) = abort {
        tokio::select! {
            biased;
            _ = token.cancelled() => return AbortableNext::Aborted,
            item = stream.next() => item,
        }
    } else {
        stream.next().await
    };

    match item {
        Some(item) => AbortableNext::Item(item),
        None => AbortableNext::Eof,
    }
}

pub fn push_aborted(sender: &mut AssistantMessageEventSender, model: &Model) {
    let msg = AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Aborted,
        error_message: Some("aborted".to_string()),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    sender.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error: msg,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    #[tokio::test]
    async fn next_or_abort_stops_pending_stream() {
        let token = CancellationToken::new();
        token.cancel();
        let mut pending = stream::pending::<Result<(), reqwest::Error>>();

        let result = next_or_abort(&mut pending, Some(&token)).await;

        assert!(matches!(result, AbortableNext::Aborted));
    }
}
