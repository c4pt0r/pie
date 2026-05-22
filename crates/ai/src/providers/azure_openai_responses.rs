//! Azure OpenAI Responses provider. Partial 1:1 port of
//! `packages/ai/src/providers/azure-openai-responses.ts` (~300 LOC).
//!
//! Azure speaks the same Responses wire protocol as OpenAI, so it reuses
//! [`openai_responses::consume_responses_sse`] and `build_request_body`. Azure-specific:
//! - URL: `{base}/openai/v1/responses?api-version={ver}` (resource-name host)
//! - Auth via `api-key` header instead of Bearer
//! - deployment-name resolution (option → AZURE_OPENAI_DEPLOYMENT_NAME_MAP → model id)
//!
//! TODO:
//! - deployment-name map env parsing edge cases
//! - reasoningSummary / serviceTier knobs

use async_trait::async_trait;
use serde_json::json;

use crate::api_registry::ApiProvider;
use crate::providers::openai_responses::{
    build_request_body, consume_responses_sse, push_error, resolve_compat,
};
use crate::types::*;
use crate::utils::abort::{self as abort_utils, AbortErrorOrReqwest};
use crate::utils::event_stream::{AssistantMessageEventSender, AssistantMessageEventStream};

const DEFAULT_AZURE_API_VERSION: &str = "v1";

#[derive(Default)]
pub struct AzureOpenAIResponsesProvider {}

#[async_trait]
impl ApiProvider for AzureOpenAIResponsesProvider {
    fn api(&self) -> &str {
        KnownApi::AzureOpenAIResponses.as_str()
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
        let translated = options
            .map(|o| {
                let mut base = o.base.clone();
                if let Some(level) = o.reasoning {
                    let effort = match level {
                        ThinkingLevel::Minimal => "minimal",
                        ThinkingLevel::Low => "low",
                        ThinkingLevel::Medium => "medium",
                        ThinkingLevel::High => "high",
                        ThinkingLevel::Xhigh => "xhigh",
                    };
                    base.provider_extras
                        .insert("reasoning_effort".to_string(), json!(effort));
                }
                base
            })
            .unwrap_or_default();
        self.stream(model, context, Some(&translated))
    }
}

/// Resolve the Azure deployment name: explicit option, then the env name-map, then the model id.
fn resolve_deployment_name(model: &Model, options: &StreamOptions) -> String {
    if let Some(name) = options
        .provider_extras
        .get("azure_deployment_name")
        .and_then(|v| v.as_str())
    {
        return name.to_string();
    }
    if let Ok(map) = std::env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP") {
        for entry in map.split(',') {
            let entry = entry.trim();
            if let Some((model_id, deployment)) = entry.split_once('=') {
                if model_id.trim() == model.id {
                    return deployment.trim().to_string();
                }
            }
        }
    }
    model.id.clone()
}

async fn run(
    model: Model,
    context: Context,
    options: StreamOptions,
    mut sender: AssistantMessageEventSender,
) {
    let api_key = match options
        .api_key
        .clone()
        .or_else(|| crate::env_api_keys::get_env_api_key("azure-openai-responses"))
        .or_else(|| std::env::var("AZURE_OPENAI_API_KEY").ok())
    {
        Some(k) => k,
        None => {
            push_error(
                &mut sender,
                &model,
                "AZURE_OPENAI_API_KEY is not set".into(),
            );
            return;
        }
    };

    let compat = resolve_compat(&model);
    let mut body = match build_request_body(&model, &context, &options, &compat) {
        Ok(b) => b,
        Err(e) => {
            push_error(&mut sender, &model, format!("build request body: {e}"));
            return;
        }
    };
    // Azure routes by deployment name; the body `model` field carries it.
    body["model"] = json!(resolve_deployment_name(&model, &options));

    let client = match crate::utils::node_http_proxy::build_client(options.timeout_ms) {
        Ok(c) => c,
        Err(e) => {
            push_error(&mut sender, &model, format!("http client: {e}"));
            return;
        }
    };

    let api_version = options
        .provider_extras
        .get("azure_api_version")
        .and_then(|v| v.as_str())
        .unwrap_or(DEFAULT_AZURE_API_VERSION);
    let base = model.base_url.trim_end_matches('/');
    let url = format!("{base}/openai/v1/responses?api-version={api_version}");

    let mut req = client
        .post(&url)
        .header("api-key", api_key)
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
            format!("Azure OpenAI API error ({status}): {txt}"),
        );
        return;
    }

    consume_responses_sse(resp, &model, &mut sender, options.abort.as_ref()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_model() -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: Api::known(KnownApi::AzureOpenAIResponses),
            provider: Provider::from("azure-openai-responses"),
            base_url: "https://my-resource.openai.azure.com".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 16_384,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn deployment_name_defaults_to_model_id() {
        let m = mk_model();
        assert_eq!(resolve_deployment_name(&m, &Default::default()), "gpt-5");
    }

    #[test]
    fn deployment_name_from_option() {
        let m = mk_model();
        let mut opts = StreamOptions::default();
        opts.provider_extras
            .insert("azure_deployment_name".into(), json!("my-deploy"));
        assert_eq!(resolve_deployment_name(&m, &opts), "my-deploy");
    }
}
