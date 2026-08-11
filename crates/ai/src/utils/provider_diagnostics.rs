//! Shared provider diagnostics for wire-level details that should be preserved without changing
//! normalized cross-provider behavior.

use reqwest::StatusCode;
use serde_json::{Map, Value, json};

use crate::types::AssistantMessage;
use crate::utils::diagnostics::AssistantMessageDiagnostic;

const MAX_ERROR_BODY_CHARS: usize = 4096;
const MAX_ERROR_FIELD_CHARS: usize = 512;
const REDACTED: &str = "[redacted]";

#[derive(Clone, Debug)]
pub struct NormalizedProviderHttpError {
    pub message: String,
    pub diagnostic: Value,
}

pub fn add_diagnostic(message: &mut AssistantMessage, diagnostic: Value) {
    message
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}

pub fn raw_stop_reason_diagnostic(provider: &str, raw_stop_reason: &str) -> Value {
    diagnostic_value(AssistantMessageDiagnostic {
        kind: "provider_raw_stop_reason".into(),
        message: format!("{provider} raw stop reason: {raw_stop_reason}"),
        data: Some(json!({
            "provider": provider,
            "rawStopReason": truncate_chars(raw_stop_reason, MAX_ERROR_FIELD_CHARS).0,
        })),
    })
}

pub fn normalize_provider_http_error(
    provider: &str,
    status: StatusCode,
    body: &str,
) -> NormalizedProviderHttpError {
    let redacted_body = redact_text(body);
    let (body_preview, body_truncated) = truncate_chars(redacted_body.trim(), MAX_ERROR_BODY_CHARS);
    let parsed = serde_json::from_str::<Value>(&body_preview).ok();

    let mut data = Map::new();
    data.insert("provider".into(), json!(provider));
    data.insert("status".into(), json!(status.as_u16()));
    data.insert("bodyTruncated".into(), json!(body_truncated));

    let extracted_message;
    match parsed {
        Some(Value::Object(map)) => {
            let body = Value::Object(redact_json_object(map));
            extracted_message = extract_message(&body);
            data.insert("body".into(), body);
            data.insert("bodyType".into(), json!("object"));
        }
        Some(value) => {
            extracted_message = extract_message(&value);
            data.insert("bodyPreview".into(), json!(body_preview));
            data.insert("bodyType".into(), json!(json_type(&value)));
        }
        None if !body_preview.is_empty() => {
            extracted_message = Some(body_preview.clone());
            data.insert("bodyPreview".into(), json!(body_preview));
            data.insert("bodyType".into(), json!("text"));
        }
        None => {
            extracted_message = None;
            data.insert("bodyType".into(), json!("empty"));
        }
    }

    let safe_message = extracted_message.unwrap_or_else(|| "provider request failed".into());
    let message = format!("HTTP {status}: {safe_message}");
    let diagnostic = diagnostic_value(AssistantMessageDiagnostic {
        kind: "provider_http_error".into(),
        message: message.clone(),
        data: Some(Value::Object(data)),
    });

    NormalizedProviderHttpError {
        message,
        diagnostic,
    }
}

fn diagnostic_value(diagnostic: AssistantMessageDiagnostic) -> Value {
    serde_json::to_value(diagnostic).expect("assistant diagnostics serialize")
}

fn redact_json_object(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .map(|(key, value)| {
            let value = if is_secret_key(&key) {
                Value::String(REDACTED.into())
            } else {
                redact_json_value(value)
            };
            (key, value)
        })
        .collect()
}

fn redact_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(redact_json_object(map)),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_json_value).collect()),
        Value::String(s) => {
            Value::String(truncate_chars(&redact_text(&s), MAX_ERROR_FIELD_CHARS).0)
        }
        other => other,
    }
}

fn is_secret_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "apikey"
            | "authorization"
            | "bearer"
            | "clientsecret"
            | "credential"
            | "credentials"
            | "password"
            | "refreshtoken"
            | "secret"
            | "token"
    ) || normalized.ends_with("token")
        || normalized.ends_with("secret")
}

fn extract_message(value: &Value) -> Option<String> {
    let candidates = [
        value.pointer("/error/message"),
        value.pointer("/message"),
        value.pointer("/error"),
        value.pointer("/detail"),
    ];
    for candidate in candidates.into_iter().flatten() {
        match candidate {
            Value::String(s) if !s.is_empty() => {
                return Some(truncate_chars(&redact_text(s), MAX_ERROR_FIELD_CHARS).0);
            }
            Value::Object(_) => {
                if let Ok(s) = serde_json::to_string(candidate) {
                    return Some(truncate_chars(&redact_text(&s), MAX_ERROR_FIELD_CHARS).0);
                }
            }
            _ => {}
        }
    }
    None
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn redact_text(value: &str) -> String {
    let mut redacted = value.to_string();
    for marker in ["Bearer ", "bearer "] {
        let mut start_at = 0;
        while let Some(pos) = redacted[start_at..].find(marker) {
            let token_start = start_at + pos + marker.len();
            let token_end = redacted[token_start..]
                .find(|ch: char| {
                    ch.is_whitespace() || ch == '"' || ch == '\'' || ch == ',' || ch == '}'
                })
                .map(|offset| token_start + offset)
                .unwrap_or(redacted.len());
            if token_end > token_start {
                redacted.replace_range(token_start..token_end, REDACTED);
            }
            start_at = token_start + REDACTED.len();
        }
    }
    redacted
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    let mut chars = value.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return (out, false);
        };
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push_str("...");
        (out, true)
    } else {
        (out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_object_body_is_preserved_and_redacted() {
        let normalized = normalize_provider_http_error(
            "anthropic",
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"bad request","token":"secret"},"api_key":"sk-test","safe":"ok"}"#,
        );

        assert_eq!(normalized.message, "HTTP 400 Bad Request: bad request");
        let data = normalized.diagnostic.get("data").unwrap();
        assert_eq!(data["body"]["api_key"], REDACTED);
        assert_eq!(data["body"]["error"]["token"], REDACTED);
        assert_eq!(data["body"]["safe"], "ok");
        assert_eq!(data["bodyType"], "object");
    }

    #[test]
    fn arrays_and_scalars_are_previews_not_error_bodies() {
        let array = normalize_provider_http_error(
            "provider",
            StatusCode::BAD_GATEWAY,
            r#"[{"message":"x"}]"#,
        );
        let array_data = array.diagnostic.get("data").unwrap();
        assert!(array_data.get("body").is_none());
        assert_eq!(array_data["bodyType"], "array");

        let scalar = normalize_provider_http_error("provider", StatusCode::BAD_GATEWAY, r#""bad""#);
        let scalar_data = scalar.diagnostic.get("data").unwrap();
        assert!(scalar_data.get("body").is_none());
        assert_eq!(scalar_data["bodyType"], "string");
    }

    #[test]
    fn huge_text_body_is_capped() {
        let huge = "x".repeat(MAX_ERROR_BODY_CHARS + 10);
        let normalized = normalize_provider_http_error("provider", StatusCode::BAD_GATEWAY, &huge);
        let data = normalized.diagnostic.get("data").unwrap();
        assert_eq!(data["bodyTruncated"], true);
        assert!(
            data["bodyPreview"]
                .as_str()
                .expect("preview")
                .ends_with("...")
        );
    }
}
