//! Server-side proxy to an OpenAI-compatible chat server (typically Ollama).
//!
//! The proxy exists for two reasons:
//! 1. Hide the upstream base URL / API key behind the same origin as the
//!    static frontend, so the browser never sees them.
//! 2. Keep the wire protocol identical to OpenAI's `/v1/chat/completions`
//!    so any other OpenAI-compatible client (curl, the OpenAI Python
//!    SDK, etc.) can also point at `stt-server` once `LLM_ENABLED=true`.
//!
//! SSE is forwarded verbatim: we do not parse or re-encode chunks.
//! Re-encoding would risk breaking clients that depend on the exact
//! framing, and there is nothing the server gains by inspecting the
//! tokens.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::config::LlmConfig;
use crate::AppState;

/// Shared, cheaply-clonable HTTP client.
///
/// `reqwest::Client` wraps an `Arc` internally, so cloning it for every
/// request keeps the connection pool hot across consecutive messages in
/// the same chat session.
#[derive(Clone, Debug)]
pub struct LlmClient {
    http: reqwest::Client,
    cfg: Arc<LlmConfig>,
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

    fn auth_header(&self) -> Option<(HeaderName, HeaderValue)> {
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
        }
    }
}

/// Subset of the OpenAI chat request we care about.
///
/// We do **not** proxy the full schema; the few fields we don't
/// understand (e.g. `tools`, `functions`, `logit_bias`) are simply
/// ignored by the upstream and don't need validation here.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // `messages` is read implicitly via re-serialisation below.
struct ChatRequest {
    #[serde(default)]
    messages: Vec<serde_json::Value>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    model: Option<String>,
}

/// `POST /v1/chat/completions` — OpenAI-compatible streaming proxy.
///
/// Only `stream=true` is supported (the proxy exists to stream).
/// The `model` field is honoured when present; otherwise the
/// server-configured `OLLAMA_MODEL` is used.
pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, LlmError> {
    let llm = state.llm.as_ref().ok_or(LlmError::Disabled)?;

    let req: ChatRequest = serde_json::from_slice(&body)
        .map_err(|e| LlmError::BadRequest(format!("invalid json body: {e}")))?;

    // The proxy only exists to stream; refuse non-streaming requests so
    // a misconfigured client does not accidentally block on a full
    // generation.
    match req.stream {
        Some(true) => {}
        Some(false) => {
            return Err(LlmError::BadRequest(
                "stream must be true; this proxy only supports streaming responses".into(),
            ));
        }
        None => {
            return Err(LlmError::BadRequest(
                "missing `stream` field; this proxy only supports streaming responses".into(),
            ));
        }
    }

    let model = req.model.unwrap_or_else(|| llm.cfg.default_model.clone());

    // Rebuild the body so we can override `model` with our fallback
    // without disturbing the rest of the user's payload (messages,
    // temperature, etc.).
    let mut forward_body = serde_json::from_slice::<serde_json::Value>(&body)
        .map_err(|e| LlmError::BadRequest(format!("invalid json body: {e}")))?;
    if let Some(obj) = forward_body.as_object_mut() {
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(true));
    }

    // Forward a few well-known request headers. `Authorization` is
    // handled separately so we never leak the server-side key when it
    // is unset.
    let mut fwd = reqwest::header::HeaderMap::new();
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        fwd.insert(header::CONTENT_TYPE, ct.clone());
    } else {
        fwd.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    if let Some((name, value)) = llm.auth_header() {
        fwd.insert(name, value);
    }

    let url = format!(
        "{}/v1/chat/completions",
        llm.cfg.base_url.trim_end_matches('/')
    );
    debug!(%url, "forwarding chat completion");

    let upstream = llm
        .http
        .post(&url)
        .headers(fwd)
        .body(forward_body.to_string())
        .send()
        .await
        .map_err(|e| LlmError::BadRequest(format!("upstream connect failed: {e}")))?;

    let status = upstream.status();
    if !status.is_success() {
        let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = upstream.text().await.unwrap_or_default();
        warn!(%status, "upstream chat completion failed");
        return Err(LlmError::Upstream { status, body });
    }

    // Wrap the upstream byte stream with an idle-chunk timeout so a
    // stalled generator cannot hold a connection forever. We measure
    // *idle* time, not total time — long generations are fine, but
    // "no bytes for N seconds" should cut the stream.
    let timeout = llm.cfg.request_timeout;
    let upstream_stream = upstream.bytes_stream();

    // Wrap the upstream byte stream with an idle-chunk timeout so a
    // stalled generator cannot hold a connection forever. We measure
    // *idle* time, not total time — long generations are fine, but
    // "no bytes for N seconds" should cut the stream.
    //
    // We use `stream::unfold` + `tokio::time::timeout` rather than
    // pulling in `tokio_stream` for the single `StreamExt::timeout`
    // method we need.
    let body_stream = stream::unfold(
        (upstream_stream, timeout),
        |(mut stream, timeout)| async move {
            match tokio::time::timeout(timeout, stream.next()).await {
                Ok(Some(Ok(chunk))) => Some((
                    Ok::<Bytes, Box<dyn std::error::Error + Send + Sync>>(chunk),
                    (stream, timeout),
                )),
                Ok(Some(Err(e))) => Some((
                    Err(Box::new(std::io::Error::other(e.to_string()))
                        as Box<dyn std::error::Error + Send + Sync>),
                    (stream, timeout),
                )),
                Ok(None) => None,
                Err(_elapsed) => Some((
                    Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream idle timeout",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>),
                    (stream, timeout),
                )),
            }
        },
    );

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        // Defeat nginx-style buffering even though no reverse proxy is
        // documented; cheap and protects future deployments.
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(body_stream))
        .expect("static response builder is valid"))
}

/// `GET /v1/models` — proxies the upstream's model list verbatim.
///
/// When the upstream is unreachable we still return a JSON body with
/// `[{ "id": "<OLLAMA_MODEL>" }]` so the UI dropdown always has at
/// least one entry.
pub async fn models_list(State(state): State<Arc<AppState>>) -> Result<Response, LlmError> {
    let llm = state.llm.as_ref().ok_or(LlmError::Disabled)?;

    let url = format!("{}/v1/models", llm.cfg.base_url.trim_end_matches('/'));
    debug!(%url, "fetching upstream models");

    let mut req = llm.http.get(&url);
    if let Some((name, value)) = llm.auth_header() {
        req = req.header(name, value);
    }

    match req.send().await {
        Ok(upstream) => {
            let status = upstream.status();
            if !status.is_success() {
                let status =
                    StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let body = upstream.text().await.unwrap_or_default();
                warn!(%status, "upstream models failed");
                return Err(LlmError::Upstream { status, body });
            }
            // Pass the body through verbatim; OpenAI's response shape is
            // a small JSON object we don't need to re-shape.
            let bytes = upstream
                .bytes()
                .await
                .map_err(|e| LlmError::BadRequest(format!("upstream read failed: {e}")))?;
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(bytes))
                .expect("static response builder is valid"))
        }
        Err(e) => {
            warn!(error = %e, "upstream unreachable, returning fallback model");
            // Offline fallback: a single-entry list built around the
            // server-configured default. The UI is still usable; the
            // user just can't pick another model without fixing the
            // upstream.
            let body = json!({
                "object": "list",
                "data": [
                    { "id": llm.cfg.default_model, "object": "model" }
                ]
            })
            .to_string();
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(body))
                .expect("static response builder is valid"))
        }
    }
}
