//! `llm::client` — the upstream HTTP client + the proxy-wide error type.
//!
//! `LlmClient` is the cheaply-cloneable handle shared across every
//! `/v1/*` request. `LlmError` is mapped to HTTP responses by the
//! `IntoResponse` impl below so each handler stays free of status-code
//! arithmetic.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::config::LlmConfig;

/// Header the browser uses to propagate the active chat-session id
/// into per-session agents (`read_document` reads it through
/// `UserContext::chat_session_id`). Lowercased per the SSE/header
/// spec; case-insensitive lookup via `HeaderMap::get` makes this
/// safe regardless.
pub const CHAT_SESSION_HEADER: &str = "x-chat-session-id";

/// Shared, cheaply-clonable HTTP client.
///
/// `reqwest::Client` wraps an `Arc` internally, so cloning it for every
/// request keeps the connection pool hot across consecutive messages in
/// the same chat session.
#[derive(Clone, Debug)]
pub struct LlmClient {
    pub(crate) http: reqwest::Client,
    pub(crate) cfg: Arc<LlmConfig>,
}

impl LlmClient {
    /// Build a client. The `request_timeout` is used as the per-byte
    /// idle timeout on the streaming body; we deliberately do **not**
    /// apply a global request timeout because long generations would
    /// otherwise be cut short.
    pub fn new(cfg: Arc<LlmConfig>) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            // Long-running streams need this off; the per-chunk idle
            // timeout below is the only thing that should ever close
            // a generation.
            .timeout(Duration::from_secs(86_400))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| LlmError::BadRequest(format!("reqwest build failed: {e}")))?;
        Ok(Self { http, cfg })
    }

    /// Borrow the live config (CORS allow-list, upstream URL, etc.).
    /// Used by the router builder to wire the CORS middleware without
    /// keeping a duplicate `Arc<LlmConfig>` in [`crate::AppState`].
    pub fn cfg(&self) -> &LlmConfig {
        &self.cfg
    }

    pub(crate) fn auth_header(&self) -> Option<(HeaderName, HeaderValue)> {
        let key = self.cfg.api_key.as_deref()?;
        let value = format!("Bearer {key}");
        // `Bearer ` is ASCII so this never fails in practice.
        let hv = HeaderValue::from_str(&value).ok()?;
        Some((header::AUTHORIZATION, hv))
    }
}

/// Errors surfaced by the LLM proxy.
///
/// `IntoResponse` maps each variant to the most informative status code
/// without leaking internal details to the browser.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("chat proxy is disabled on this server")]
    Disabled,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("upstream returned {status}: {body}")]
    Upstream { status: StatusCode, body: String },
    /// The upstream rejected the request because the requested model
    /// is not known to it (Ollama `"model 'X' not found"`, OpenAI
    /// `"model_not_found"`). The chat UI surfaces the friendly form
    /// with a list of models the upstream *does* serve, so the user
    /// can pick one and retry instead of staring at a 404 + raw JSON.
    #[error("model `{name}` not found")]
    ModelNotFound {
        /// The model name the caller requested.
        name: String,
        /// Optional plain upstream message (Ollama `"model 'llama3.1'
        /// not found"`, …) — preserved for transparency.
        upstream_message: Option<String>,
        /// Models the upstream actually serves, as best we could
        /// fetch. `None` when the follow-up `/v1/models` call also
        /// failed (the upstream is degraded but the not-found signal
        /// is still actionable).
        available: Option<Vec<String>>,
    },
    #[error("agents disabled on this server")]
    AgentsDisabled,
    #[error("agent `{0}` not found")]
    AgentNotFound(String),
}

impl IntoResponse for LlmError {
    fn into_response(self) -> Response {
        match self {
            // 404 makes the chat view render its "disabled" notice
            // without the user having to read a stack trace.
            LlmError::Disabled => (StatusCode::NOT_FOUND, "llm disabled").into_response(),
            LlmError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg).into_response(),
            LlmError::Upstream { status, body } => {
                let mut resp = (status, body).into_response();
                // Make sure the browser does not cache an upstream error.
                resp.headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                resp
            }
            LlmError::ModelNotFound {
                name,
                upstream_message,
                available,
            } => {
                // Stable, machine-friendly shape: the chat UI checks
                // `error.type === "model_not_found"` and renders a
                // picker over `error.available` rather than the raw
                // upstream body. `param` carries the offending name
                // (matches OpenAI's convention for invalid_request_error
                // payloads so JS code doesn't need a special case for
                // this single error type).
                let message = upstream_message
                    .clone()
                    .unwrap_or_else(|| format!("model `{name}` not found"));
                let mut payload = serde_json::json!({
                    "error": {
                        "type": "model_not_found",
                        "message": message,
                        "param": name,
                    }
                });
                if let Some(models) = available {
                    payload["error"]["available"] = serde_json::Value::Array(
                        models.into_iter().map(serde_json::Value::String).collect(),
                    );
                }
                let body = payload.to_string();
                let mut resp = Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .header(header::CACHE_CONTROL, "no-store")
                    .body(axum::body::Body::from(body))
                    .expect("static response builder is valid");
                // Surface the friendly type to client-side log
                // aggregators via the response extension header.
                resp.headers_mut()
                    .entry(HeaderName::from_static("x-error-type"))
                    .or_insert(HeaderValue::from_static("model_not_found"));
                resp
            }
            LlmError::AgentsDisabled => (StatusCode::NOT_FOUND, "agents disabled").into_response(),
            LlmError::AgentNotFound(name) => {
                (StatusCode::NOT_FOUND, format!("agent `{name}` not found")).into_response()
            }
        }
    }
}

/// Detect a "requested model does not exist" upstream response and
/// pull the offending model name out of the body. Returns `None`
/// when the response is something else (a generic 5xx, a JSON
/// parse error, a model name that happens to contain the substring
/// "not found", …).
///
/// We support two shapes:
///
/// 1. Ollama (and most OpenAI-compatible forges that mirror the
/// Ollama error envelope):
/// ```json
/// {"error":{"message":"model 'llama3.1' not found","type":"not_found_error",...}}
/// ```
/// Matched by `type == "not_found_error"` AND a `model '…' not
/// found` substring inside the message — either alone is not
/// strict enough (the message substring could appear in a tool
/// prompt; `not_found_error` alone covers other 404s the upstream
/// emits, e.g. "model not loaded yet").
///
/// 2. OpenAI:
/// ```json
/// {"error":{"message":"The model `foo` does not exist.","type":"invalid_request_error","code":"model_not_found"}}
/// ```
/// Matched by `code == "model_not_found"`. Some OpenAI-compatible
/// forges flip the location of the code (`param` instead of
/// `code`) so we also accept `param == "model"` with a
/// "does not exist" / "not found" substring.
///
/// The function is pure (no I/O) so it can be unit-tested against
/// canned bodies.
pub(crate) fn detect_model_not_found(
    status: StatusCode,
    body: &str,
) -> Option<(String, Option<String>)> {
    if status != StatusCode::NOT_FOUND && status != StatusCode::BAD_REQUEST {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let err = parsed.get("error")?;
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .map(String::from);
    let kind = err.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");

    // Case 1: Ollama — `type == "not_found_error"` AND message
    // contains `model '<name>' not found`. Extract the quoted name.
    if kind == "not_found_error" {
        if let Some(name) = message
            .as_deref()
            .and_then(extract_single_quoted_model_name)
        {
            return Some((name, message));
        }
        // Some Ollama versions use double quotes.
        if let Some(name) = message
            .as_deref()
            .and_then(extract_double_quoted_model_name)
        {
            return Some((name, message));
        }
    }

    // Case 2: OpenAI — `code == "model_not_found"`, or `param ==
    // "model"` with a "does not exist" / "not found" message.
    if code == "model_not_found" {
        let name = message
            .as_deref()
            .and_then(extract_single_quoted_model_name)
            .or_else(|| {
                message
                    .as_deref()
                    .and_then(extract_double_quoted_model_name)
            })
            .unwrap_or_default();
        return Some((name, message));
    }
    if err.get("param").and_then(|v| v.as_str()) == Some("model") {
        if let Some(msg) = message.as_deref() {
            let lower = msg.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("does not exist") {
                let name = extract_single_quoted_model_name(msg)
                    .or_else(|| extract_double_quoted_model_name(msg))
                    .unwrap_or_default();
                return Some((name, message));
            }
        }
    }

    None
}

/// Pull the model name out of `model 'foo' not found` (single-quoted
/// Ollama format). Returns `None` for any other shape.
fn extract_single_quoted_model_name(s: &str) -> Option<String> {
    let start = s.find("model '")? + "model '".len();
    let rest = &s[start..];
    let end = rest.find('\'')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Pull the model name out of `model "foo" not found` (double-quoted
/// variant some Ollama builds use).
fn extract_double_quoted_model_name(s: &str) -> Option<String> {
    let start = s.find("model \"")? + "model \"".len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests_detect {
    use super::*;

    #[test]
    fn detects_ollama_not_found_error() {
        let body = r#"{"error":{"message":"model 'llama3.1' not found","type":"not_found_error","param":null,"code":null}}"#;
        let (name, msg) = detect_model_not_found(StatusCode::NOT_FOUND, body).unwrap();
        assert_eq!(name, "llama3.1");
        assert!(msg.unwrap().contains("llama3.1"));
    }

    #[test]
    fn detects_ollama_double_quoted_name() {
        let body =
            r#"{"error":{"message":"model \"qwen2.5:14b\" not found","type":"not_found_error"}}"#;
        let (name, _) = detect_model_not_found(StatusCode::NOT_FOUND, body).unwrap();
        assert_eq!(name, "qwen2.5:14b");
    }

    #[test]
    fn detects_openai_code_model_not_found() {
        let body = r#"{"error":{"message":"The model `foo` does not exist.","type":"invalid_request_error","code":"model_not_found"}}"#;
        let (name, _) = detect_model_not_found(StatusCode::NOT_FOUND, body).unwrap();
        // No single-quote present, so name defaults to ""; the chat
        // UI uses the upstream message in that case.
        assert_eq!(name, "");
    }

    #[test]
    fn detects_param_model_with_not_found_message() {
        let body = r#"{"error":{"message":"model 'gpt-x' not found","type":"invalid_request_error","param":"model"}}"#;
        let (name, _) = detect_model_not_found(StatusCode::BAD_REQUEST, body).unwrap();
        assert_eq!(name, "gpt-x");
    }

    #[test]
    fn ignores_unrelated_404() {
        // A 404 with no model-not-found signal — must NOT match.
        let body = r#"{"error":{"message":"endpoint not found","type":"not_found_error"}}"#;
        assert!(detect_model_not_found(StatusCode::NOT_FOUND, body).is_none());
    }

    #[test]
    fn ignores_non_404_status() {
        let body = r#"{"error":{"message":"model 'foo' not found","type":"not_found_error"}}"#;
        assert!(detect_model_not_found(StatusCode::INTERNAL_SERVER_ERROR, body).is_none());
        assert!(detect_model_not_found(StatusCode::OK, body).is_none());
    }

    #[test]
    fn ignores_malformed_body() {
        assert!(detect_model_not_found(StatusCode::NOT_FOUND, "not json").is_none());
        assert!(detect_model_not_found(StatusCode::NOT_FOUND, "").is_none());
        assert!(detect_model_not_found(StatusCode::NOT_FOUND, "{}").is_none());
    }
}
/// as a UUID. Returns `None` for a missing header AND for a
/// malformed value (the chat-completions handler treats both as
/// "no session scoping"; the agent that needs a session id is
/// responsible for surfacing a clear error).
pub fn parse_chat_session_header(headers: &axum::http::HeaderMap) -> Option<uuid::Uuid> {
    let raw = headers.get(CHAT_SESSION_HEADER)?;
    let s = raw.to_str().ok()?.trim();
    uuid::Uuid::parse_str(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn parse_chat_session_header_returns_uuid_when_valid() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            CHAT_SESSION_HEADER,
            HeaderValue::from_static("01234567-89ab-cdef-0123-456789abcdef"),
        );
        let id = parse_chat_session_header(&h).expect("valid uuid must parse");
        assert_eq!(
            id,
            uuid::Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap()
        );
    }

    #[test]
    fn parse_chat_session_header_returns_none_for_missing_header() {
        let h = axum::http::HeaderMap::new();
        assert_eq!(parse_chat_session_header(&h), None);
    }

    #[test]
    fn parse_chat_session_header_returns_none_for_malformed_value() {
        // Garbage in the header must not crash the LLM proxy —
        // direct callers (curl, SDKs) that forget the header still
        // get a streaming response. The session-scoped agents
        // surface a clear tool error when the header is missing.
        let mut h = axum::http::HeaderMap::new();
        h.insert(CHAT_SESSION_HEADER, HeaderValue::from_static("not-a-uuid"));
        assert_eq!(parse_chat_session_header(&h), None);
    }
}
