//! Google Vertex AI provider (`google-vertex`). Partial 1:1 port of
//! `packages/ai/src/providers/google-vertex.ts` (~570 LOC).
//!
//! Reuses the Gemini message/tool conversion and SSE consumer from `google`/`google_shared`.
//! Vertex differs from the Gemini API in two ways:
//! - endpoint: `{base}/v1/projects/{project}/locations/{loc}/publishers/google/models/{id}:streamGenerateContent`
//! - auth: OAuth Bearer access token (service account / ADC) instead of `x-goog-api-key`
//!
//! TODO:
//! - Full Application Default Credentials: service-account JSON → signed JWT → token exchange.
//!   For now the access token must be supplied via `options.api_key` or the
//!   `GOOGLE_VERTEX_ACCESS_TOKEN` env var. Project/location come from
//!   `GOOGLE_VERTEX_PROJECT` / `GOOGLE_VERTEX_LOCATION` (default `us-central1`).
//! - global endpoint vs regional endpoint host selection

use async_trait::async_trait;

use crate::api_registry::ApiProvider;
use crate::providers::google::{
    build_request_body, consume_gemini_sse, push_error, translate_simple,
};
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};

#[derive(Default)]
pub struct GoogleVertexProvider {}

#[async_trait]
impl ApiProvider for GoogleVertexProvider {
    fn api(&self) -> &str {
        KnownApi::GoogleVertex.as_str()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let (stream, sender) = AssistantMessageEventStream::new();
        let model = model.clone();
        let context = context.clone();
        let options = options.cloned().unwrap_or_default();
        tokio::spawn(async move {
            run(model, context, options, sender).await;
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, Some(&translate_simple(options)))
    }
}

fn vertex_location() -> String {
    std::env::var("GOOGLE_VERTEX_LOCATION").unwrap_or_else(|_| "us-central1".to_string())
}

fn vertex_host(location: &str) -> String {
    if location == "global" {
        "https://aiplatform.googleapis.com".to_string()
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    }
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    mut sender: AssistantMessageEventSender,
) {
    // Access token: explicit option, then GOOGLE_VERTEX_ACCESS_TOKEN. Full ADC is a TODO.
    let token = match options
        .api_key
        .clone()
        .or_else(|| std::env::var("GOOGLE_VERTEX_ACCESS_TOKEN").ok())
    {
        Some(t) if !t.is_empty() => t,
        _ => {
            push_error(
                &mut sender,
                &model,
                "Vertex access token missing: set GOOGLE_VERTEX_ACCESS_TOKEN or pass options.api_key (ADC/JWT flow not yet implemented)".into(),
            );
            return;
        }
    };

    let project = match std::env::var("GOOGLE_VERTEX_PROJECT") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            push_error(
                &mut sender,
                &model,
                "GOOGLE_VERTEX_PROJECT is not set".into(),
            );
            return;
        }
    };
    let location = vertex_location();

    let body = build_request_body(&context, &options);
    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };

    let host = if model.base_url.is_empty() {
        vertex_host(&location)
    } else {
        model.base_url.trim_end_matches('/').to_string()
    };
    let url = format!(
        "{host}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:streamGenerateContent?alt=sse",
        model.id
    );
    let mut req = client
        .post(&url)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    if let Some(extra) = &options.headers {
        for (k, v) in extra {
            req = req.header(k.as_str(), v.as_str());
        }
    }

    let req = req.json(&body);
    let resp = match crate::utils::retry::send_with_retry(&options, req).await {
        Ok(r) => r,
        Err(e) => {
            if e.is_aborted() {
                abort_utils::push_aborted(&mut sender, &model);
            } else {
                push_error(&mut sender, &model, format!("http error: {e}"));
            }
            return;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = match abort_utils::response_text_or_abort(resp, options.abort.as_ref()).await {
            Ok(txt) => txt,
            Err(AbortErrorOrReqwest::Aborted) => {
                abort_utils::push_aborted(&mut sender, &model);
                return;
            }
            Err(AbortErrorOrReqwest::Reqwest(_)) => String::new(),
        };
        push_error(
            &mut sender,
            &model,
            format!("Vertex API error ({status}): {txt}"),
        );
        return;
    }

    consume_gemini_sse(resp, &model, sender, options.abort.as_ref()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_regional_or_global() {
        assert_eq!(
            vertex_host("us-central1"),
            "https://us-central1-aiplatform.googleapis.com"
        );
        assert_eq!(vertex_host("global"), "https://aiplatform.googleapis.com");
    }
}
