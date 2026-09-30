//! `llm::proxy` — the top-level HTTP handlers for `/v1/*`.
//!
//! - [`chat_completions`] is the streaming OpenAI-compatible proxy;
//!   it owns the SSE response body and hands the upstream byte
//!   stream to [`crate::llm::tool_loop::run_tool_loop`].
//! - [`models_list`] proxies the upstream's `/v1/models` list (or
//!   returns a single fallback when the upstream is unreachable).
//! - [`agents_list`] / [`agent_invoke`] are the direct agent HTTP
//!   surface. Both moved into [`crate::agents::routes`] as part of
//!   phase 1; the thin wrappers here re-export them under the
//!   historical `llm::agents_list` / `llm::agent_invoke` names so
//!   integration tests keep compiling unchanged.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::agents::AgentRegistry;
use crate::config::LlmConfig;
use crate::llm::client::{parse_chat_session_header, LlmError};
use crate::llm::privacy::{strip_user_location_if_disabled, strip_user_timezone_if_disabled};
use crate::llm::prompt::inject_default_system_prompt;
use crate::llm::tool_loop::run_tool_loop;
use crate::state::{ArcAgentsConfig, ArcLlmState, ArcServices, OptArcAgentRegistry};

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
///
/// `auth_user` is `Some` when `RequireAuth` middleware injected
/// an `AuthUser` into the request extensions (production path
/// when `auth.enabled = true`). `None` when the route is reached
/// without auth (the `auth.enabled = false` trust boundary, or
/// direct integration tests). The per-round `UserContext` uses
/// `Uuid::nil()` when the extension is missing — see
/// `run_tool_loop` for the user-scoping consequences (per-user
/// agents surface a clear tool error).
pub async fn chat_completions(
    State(llm_state): State<ArcLlmState>,
    State(agents): State<OptArcAgentRegistry>,
    State(agents_cfg): State<ArcAgentsConfig>,
    State(services): State<ArcServices>,
    auth_user: Option<axum::Extension<crate::auth::session::AuthUser>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, LlmError> {
    let llm = &llm_state.client;

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

    // Read the chat-session id from the dedicated header (set by
    // `chat.js` on every `/v1/*` request). When present, thread
    // it into the per-tool-round `UserContext` so agents like
    // `read_document` can scope their queries to the right
    // session. A missing / malformed header is non-fatal — the
    // session-scoped agents will then return a tool error and
    // the LLM can recover.
    let chat_session_id = parse_chat_session_header(&headers);

    // Rebuild the body so we can override `model` with our fallback
    // without disturbing the rest of the user's payload (messages,
    // temperature, etc.).
    let mut forward_body = serde_json::from_slice::<Value>(&body)
        .map_err(|e| LlmError::BadRequest(format!("invalid json body: {e}")))?;
    if let Some(obj) = forward_body.as_object_mut() {
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(true));
        // Merge the agent tools array into the request body. We do
        // this once, up front, so every round of the tool-loop carries
        // the same `tools` definition (Ollama expects the field on
        // every call). Client-supplied `tools` are preserved.
        let server_tools = agents
            .0
            .as_ref()
            .map(|a| a.tools_schema())
            .unwrap_or_default();
        // Drop the `services` shadow so the parameter is used.
        let _ = services;
        if !server_tools.is_empty() {
            let client_tools = obj
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut merged = server_tools;
            // Append client tools verbatim — server tools take
            // precedence on name collisions to keep the wire-format
            // consistent for the LLM.
            merged.extend(client_tools);
            obj.insert("tools".into(), Value::Array(merged));
        }
    }

    // Prepend the admin-configured system prompt (env `LLM_SYSTEM_PROMPT`
    // or TOML `[llm].system_prompt`) as `messages[0]`. The browser's
    // "Additional instructions" textarea is appended *after* this by
    // `chat.js`, so the admin's prompt stays authoritative. Done once
    // here; the tool loop below reuses the same `forward_body` for
    // every round, so the prepend propagates automatically.
    inject_default_system_prompt(&mut forward_body, llm.cfg.system_prompt.as_deref());
    // Admin kill-switch: when the operator has set
    // `LLM_ALLOW_USER_LOCATION=false`, strip the browser-injected
    // location block (matched on the exact `User's approximate
    // location:` prefix) before it ever reaches the upstream model.
    // Runs after the admin prompt injection so the admin block
    // always survives; the tool loop reuses `forward_body` so the
    // strip persists across rounds.
    strip_user_location_if_disabled(&mut forward_body, llm.cfg.allow_user_location);
    // Same kill-switch treatment for the timezone block: when
    // `LLM_ALLOW_USER_TIMEZONE=false`, strip the browser-injected
    // block matched on the exact `The user's local timezone is`
    // prefix. The two kill-switches are independent so operators can
    // forbid one without touching the other.
    strip_user_timezone_if_disabled(&mut forward_body, llm.cfg.allow_user_timezone);

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

    let upstream_url = format!(
        "{}/v1/chat/completions",
        llm.cfg.base_url.trim_end_matches('/')
    );

    // Open the upstream connection eagerly so non-2xx responses
    // surface as their original HTTP status (the same way the
    // non-streaming proxy did). The streaming loop then owns the
    // body. We must NOT call `.error_for_status()` because a 4xx/5xx
    // body is still a valid SSE stream we want to forward verbatim.
    let first_upstream = llm
        .http
        .post(&upstream_url)
        .headers(fwd.clone())
        .body(forward_body.to_string())
        .send()
        .await
        .map_err(|e| LlmError::BadRequest(format!("upstream connect failed: {e}")))?;
    if !first_upstream.status().is_success() {
        let status = StatusCode::from_u16(first_upstream.status().as_u16())
            .unwrap_or(StatusCode::BAD_GATEWAY);
        let body = first_upstream.text().await.unwrap_or_default();
        warn!(%status, "upstream chat completion failed");
        return Err(LlmError::Upstream { status, body });
    }

    let timeout = llm.cfg.request_timeout;
    let max_rounds = agents_cfg.llm_max_tool_rounds;
    let agents = agents.0.clone().map(|arc| (*arc).clone());

    // Channel that drives the response body. The tool-loop coroutine
    // pushes either upstream `data:` chunks (verbatim) or locally
    // synthesised `event: tool_call` / `event: tool_result` chunks
    // into this channel; axum streams them to the client.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);

    let http = llm.http.clone();
    let headers_clone = fwd.clone();
    let url_clone = upstream_url.clone();
    let body_for_loop = forward_body.clone();
    let first_stream: crate::llm::sse::UpstreamByteStream = Box::pin(first_upstream.bytes_stream());
    // SEV 2 fix: thread the authenticated user id into the tool
    // loop. `auth_user` is `None` on the `auth.enabled = false`
    // path (and the integration tests that don't mount the
    // RequireAuth middleware); fall back to `Uuid::nil()` so the
    // per-user agents surface a clear tool error instead of
    // panicking.
    let user_id = auth_user
        .map(|axum::Extension(u)| u.id)
        .unwrap_or_else(uuid::Uuid::nil);
    // Security plan #10: pass the operator-configured web_fetch
    // allowlist down to the tool loop so the read_document →
    // web_fetch check can decide whether the URL host is already
    // pre-authorised.
    let web_fetch_allowlist = agents_cfg.web_fetch.allowlist.clone();
    tokio::spawn(async move {
        run_tool_loop(
            http,
            headers_clone,
            url_clone,
            body_for_loop,
            agents,
            max_rounds,
            timeout,
            tx,
            first_stream,
            chat_session_id,
            user_id,
            web_fetch_allowlist,
        )
        .await;
    });

    let body_stream = stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(Ok(chunk)) => Some((
                Ok::<Bytes, Box<dyn std::error::Error + Send + Sync>>(chunk),
                rx,
            )),
            Some(Err(e)) => Some((
                Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
                rx,
            )),
            None => None,
        }
    });

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
pub async fn models_list(State(llm_state): State<ArcLlmState>) -> Result<Response, LlmError> {
    let llm = &llm_state.client;

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

// ---------------------------------------------------------------------------
// Agents HTTP surface — moved to `crate::agents::routes` (phase 1). The
// `llm::agents_list` and `llm::agent_invoke` names are kept here as
// thin re-exports so existing imports keep compiling.
// ---------------------------------------------------------------------------

/// `GET /v1/agents` — list every agent registered on this server.
///
/// Returns an empty array when agents are disabled (so the browser
/// can render the "no agents" hint without special-casing 404).
pub async fn agents_list(State(agents): State<AgentRegistry>) -> Result<Response, LlmError> {
    crate::agents::routes::agents_list(State(agents)).await
}

/// `POST /v1/agents/:name/invoke` — direct agent invocation, used by
/// tests and `curl`. The chat UI goes through `/v1/chat/completions`
/// instead so the SSE stream stays consistent.
pub async fn agent_invoke(
    State(agents): State<AgentRegistry>,
    State(services): State<ArcServices>,
    auth_user: Option<axum::Extension<crate::auth::session::AuthUser>>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, LlmError> {
    crate::agents::routes::agent_invoke(State(agents), State(services), auth_user, Path(name), body)
        .await
}

// Silence the unused-import lint on `LlmConfig` / `AgentRegistry` if a
// future refactor removes the only reference inside this file: both
// are part of the public surface even when only used through trait
// methods.
#[allow(dead_code)]
fn _phantom(_: &LlmConfig, _: &AgentRegistry) {}
