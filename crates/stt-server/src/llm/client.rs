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
            LlmError::AgentsDisabled => (StatusCode::NOT_FOUND, "agents disabled").into_response(),
            LlmError::AgentNotFound(name) => {
                (StatusCode::NOT_FOUND, format!("agent `{name}` not found")).into_response()
            }
        }
    }
}

/// Read the `X-Chat-Session-Id` header from `headers` and parse it
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
