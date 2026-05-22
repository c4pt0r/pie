//! Provider registry. 1:1 port of `packages/ai/src/api-registry.ts`.
//!
//! Per Q2:A the registry stores trait objects (`Box<dyn ApiProvider>`). Each provider declares
//! the `api` string it serves, and a mismatched call (`model.api != provider.api`) returns an
//! error stream (not a panic) — matching the TS `wrapStream` wrapper.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;

use crate::types::{Api, Context, Model, SimpleStreamOptions, StreamOptions};
use crate::utils::event_stream::AssistantMessageEventStream;

/// Provider trait. Each wire-protocol implementation is a `Box<dyn ApiProvider>` registered in
/// the global registry. `async_trait` lets us write async methods; the `Send + Sync` bounds
/// come from the `#[async_trait]` macro's expansion.
#[async_trait]
pub trait ApiProvider: Send + Sync {
    /// The wire-protocol identifier this provider serves (e.g. `"anthropic-messages"`).
    fn api(&self) -> &str;

    /// Provider-specific streaming entry point. Caller passes `StreamOptions` whose
    /// `provider_extras` map may include vendor-specific knobs.
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream;

    /// Universal streaming entry point. Each provider translates `SimpleStreamOptions` into
    /// whatever its own knobs are.
    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream;
}

struct RegisteredProvider {
    provider: Arc<dyn ApiProvider>,
    source_id: Option<String>,
}

struct Registry {
    entries: HashMap<String, RegisteredProvider>,
}

fn registry() -> &'static Mutex<Registry> {
    static CELL: OnceLock<Mutex<Registry>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(Registry {
            entries: HashMap::new(),
        })
    })
}

pub fn register_api_provider(provider: Box<dyn ApiProvider>, source_id: Option<String>) {
    let mut reg = registry().lock().expect("registry poisoned");
    let api = provider.api().to_string();
    let provider: Arc<dyn ApiProvider> = Arc::from(provider);
    reg.entries.insert(
        api,
        RegisteredProvider {
            provider,
            source_id,
        },
    );
}

/// Lookup. Returns a handle that delegates to the registered provider while holding no lock —
/// we clone-by-reference using a shim because trait objects in a `MutexGuard` can't outlive the
/// guard. The shim takes a function pointer that re-acquires the lock for each call. This is
/// the Rust equivalent of TS returning the function reference directly.
pub fn get_api_provider(api: &Api) -> Option<RegisteredHandle> {
    let reg = registry().lock().expect("registry poisoned");
    let entry = reg.entries.get(&api.0)?;
    Some(RegisteredHandle {
        provider: entry.provider.clone(),
    })
}

pub fn unregister_api_providers(source_id: &str) {
    let mut reg = registry().lock().expect("registry poisoned");
    reg.entries
        .retain(|_, entry| entry.source_id.as_deref() != Some(source_id));
}

pub fn clear_api_providers() {
    let mut reg = registry().lock().expect("registry poisoned");
    reg.entries.clear();
}

/// Snapshot of currently-registered api ids. The TS `getApiProviders()` returns the internal
/// shim objects; we return ids to keep the lock scope tight.
pub fn list_api_ids() -> Vec<String> {
    let reg = registry().lock().expect("registry poisoned");
    reg.entries.keys().cloned().collect()
}

/// Handle returned by [`get_api_provider`]. The handle captures the provider `Arc`, matching
/// TS semantics where unregister-while-streaming is allowed because the in-flight call keeps
/// working off the captured function reference.
pub struct RegisteredHandle {
    provider: Arc<dyn ApiProvider>,
}

impl RegisteredHandle {
    pub fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        // Mismatch guard: same as TS `wrapStream`.
        if model.api.0 != self.provider.api() {
            return error_stream(format!(
                "Mismatched api: {} expected {}",
                model.api.0,
                self.provider.api()
            ));
        }
        self.provider.stream(model, context, options)
    }

    pub fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        if model.api.0 != self.provider.api() {
            return error_stream(format!(
                "Mismatched api: {} expected {}",
                model.api.0,
                self.provider.api()
            ));
        }
        self.provider.stream_simple(model, context, options)
    }
}

/// Construct an instantly-errored stream. Per the TS contract, providers must encode failures
/// in the returned stream rather than throw — same applies to the registry-level guard.
pub(crate) fn error_stream(message: String) -> AssistantMessageEventStream {
    use crate::types::*;
    let (stream, mut sender) = AssistantMessageEventStream::new();
    let err = AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![],
        api: Api::from(""),
        provider: Provider::from(""),
        model: String::new(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error_message: Some(message),
        timestamp: chrono::Utc::now().timestamp_millis(),
    };
    sender.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error: err,
    });
    stream
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, AssistantMessageEvent, AssistantRole, Context, ErrorReason, Model,
        ModelCost, Provider, SimpleStreamOptions, StopReason, StreamOptions, Usage,
    };
    use futures::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex as TokioMutex;

    fn registry_test_lock() -> &'static TokioMutex<()> {
        static CELL: OnceLock<TokioMutex<()>> = OnceLock::new();
        CELL.get_or_init(|| TokioMutex::new(()))
    }

    #[derive(Default)]
    struct CountingProvider {
        stream_calls: AtomicUsize,
        simple_calls: AtomicUsize,
    }

    impl CountingProvider {
        fn event_stream(&self, label: &str) -> AssistantMessageEventStream {
            let (stream, mut sender) = AssistantMessageEventStream::new();
            let message = AssistantMessage {
                role: AssistantRole::Assistant,
                content: vec![],
                api: Api::from("race-api"),
                provider: Provider::from("race"),
                model: label.to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 0,
            };
            sender.push(AssistantMessageEvent::Done {
                reason: crate::types::DoneReason::Stop,
                message,
            });
            stream
        }
    }

    #[async_trait]
    impl ApiProvider for CountingProvider {
        fn api(&self) -> &str {
            "race-api"
        }

        fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: Option<&StreamOptions>,
        ) -> AssistantMessageEventStream {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            self.event_stream("stream")
        }

        fn stream_simple(
            &self,
            _model: &Model,
            _context: &Context,
            _options: Option<&SimpleStreamOptions>,
        ) -> AssistantMessageEventStream {
            self.simple_calls.fetch_add(1, Ordering::SeqCst);
            self.event_stream("simple")
        }
    }

    fn race_model(api: &str) -> Model {
        Model {
            id: "race-model".into(),
            name: "Race Model".into(),
            api: Api::from(api),
            provider: Provider::from("race"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![],
            cost: ModelCost::default(),
            context_window: 1,
            max_tokens: 1,
            headers: None,
            compat: None,
        }
    }

    fn empty_context() -> Context {
        Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        }
    }

    #[tokio::test]
    async fn handle_survives_unregister_after_lookup() {
        let _guard = registry_test_lock().lock().await;
        clear_api_providers();
        register_api_provider(
            Box::new(CountingProvider::default()),
            Some("race-source".into()),
        );
        let handle = get_api_provider(&Api::from("race-api")).expect("provider handle");

        unregister_api_providers("race-source");
        assert!(get_api_provider(&Api::from("race-api")).is_none());

        let mut stream = handle.stream(&race_model("race-api"), &empty_context(), None);
        let mut done_model = None;
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::Done { message, .. } = event {
                done_model = Some(message.model);
            }
        }
        assert_eq!(done_model.as_deref(), Some("stream"));
    }

    #[tokio::test]
    async fn handle_survives_clear_after_lookup_for_simple_stream() {
        let _guard = registry_test_lock().lock().await;
        clear_api_providers();
        register_api_provider(Box::new(CountingProvider::default()), None);
        let handle = get_api_provider(&Api::from("race-api")).expect("provider handle");

        clear_api_providers();
        assert!(get_api_provider(&Api::from("race-api")).is_none());

        let mut stream = handle.stream_simple(&race_model("race-api"), &empty_context(), None);
        let mut done_model = None;
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::Done { message, .. } = event {
                done_model = Some(message.model);
            }
        }
        assert_eq!(done_model.as_deref(), Some("simple"));
    }

    #[tokio::test]
    async fn captured_handle_still_returns_mismatch_error_stream() {
        let _guard = registry_test_lock().lock().await;
        clear_api_providers();
        register_api_provider(Box::new(CountingProvider::default()), None);
        let handle = get_api_provider(&Api::from("race-api")).expect("provider handle");
        clear_api_providers();

        let mut stream = handle.stream(&race_model("other-api"), &empty_context(), None);
        let mut error_message = None;
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::Error { reason, error } = event {
                assert_eq!(reason, ErrorReason::Error);
                error_message = error.error_message;
            }
        }
        assert_eq!(
            error_message.as_deref(),
            Some("Mismatched api: other-api expected race-api")
        );
    }
}
