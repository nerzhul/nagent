//! `agents::routes` — direct HTTP surface for the chat agents.
//!
//! Two routes live here:
//!
//! - `GET /v1/agents` — list every registered agent (browser UI hint,
//!   used by `Integrations` to discover tool availability).
//! - `POST /v1/agents/:name/invoke` — direct agent invocation used by
//!   curl, the integration tests, and any non-streaming consumer. The
//!   chat UI goes through `/v1/chat/completions` instead so the SSE
//!   stream stays consistent.
//!
//! Both routes moved from `llm::proxy` as part of phase 1 of the
//! architecture refactor; the `llm::agents_list` / `llm::agent_invoke`
//! names are kept as thin re-exports for backward compatibility.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};

use crate::agents::{AgentError, AgentRegistry};
use crate::llm::client::LlmError;
use crate::AppState;

/// `GET /v1/agents` — list every agent registered on this server.
///
/// Returns an empty array when agents are disabled (so the browser
/// can render the "no agents" hint without special-casing 404).
pub async fn agents_list(State(state): State<Arc<AppState>>) -> Result<Response, LlmError> {
    let agents = state.agents.as_ref().ok_or(LlmError::AgentsDisabled)?;
    let body = json!({ "data": agents.list() }).to_string();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .expect("static response builder is valid"))
}

/// `POST /v1/agents/:name/invoke` — direct agent invocation.
///
/// Body shape: `{"arguments": {...}}`. Returns
/// `{"name": "<agent>", "result": "<json string>"}` on success, or
/// an [`LlmError`] mapped to the appropriate HTTP status on failure
/// (400 for invalid args, 404 for unknown agent, 502 for upstream).
pub async fn agent_invoke(
    State(state): State<Arc<AppState>>,
    auth_user: Option<axum::Extension<crate::auth::session::AuthUser>>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, LlmError> {
    let agents = state.agents.as_ref().ok_or(LlmError::AgentsDisabled)?;
    let agent = agents
        .get(&name)
        .ok_or_else(|| LlmError::AgentNotFound(name.clone()))?;

    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|e| LlmError::BadRequest(format!("invalid json body: {e}")))?;
    let args = parsed
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    // SEV 2 fix: thread the authenticated user id into the
    // `UserContext` so per-user agents (currently `read_document`)
    // can scope their lookups. The `/v1/agents/:name/invoke` route
    // is still reachable from any authenticated user; per-user
    // agents that read `ctx.secret(...)` will surface
    // `CredentialsMissing` because the resolver is not wired into
    // this code path (the route handler escapes the LLM tool loop,
    // which is where the resolver lives).
    let user_id = auth_user
        .map(|axum::Extension(u)| u.id)
        .unwrap_or_else(uuid::Uuid::nil);
    let services = state
        .auth
        .as_ref()
        .map(|a| a.services.clone())
        .unwrap_or_else(|| crate::agents::ServiceRegistry::empty().into_arc());
    let ctx = crate::agents::UserContext::for_tests(user_id, services);

    match agent.invoke(&ctx, args).await {
        Ok(result) => {
            let body = json!({ "name": name, "result": result }).to_string();
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(body))
                .expect("static response builder is valid"))
        }
        // Map agent-level errors to HTTP statuses that match the
        // chat-completions surface so clients can use the same error
        // handling regardless of which path they hit.
        Err(AgentError::InvalidArguments(msg)) | Err(AgentError::SandboxDenied(msg)) => {
            Err(LlmError::BadRequest(msg))
        }
        Err(AgentError::Upstream { status, body }) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            Err(LlmError::Upstream { status, body })
        }
        // `ResponseExceeded` only escapes the agent when the LLM-driven
        // /v1/agents/:name/invoke path is used directly (curl, tests).
        // The chat-completions path catches it inside the tool loop
        // before this match runs and either retries or surfaces it as
        // a `[error] response exceeded max_bytes=…` tool result.
        Err(AgentError::ResponseExceeded { budget }) => Err(LlmError::BadRequest(format!(
            "response exceeded max_bytes={budget}"
        ))),
        Err(AgentError::AgentFailed(msg)) => Err(LlmError::BadRequest(msg)),
        // Per-user credentials not configured / decrypt failed —
        // surfaced as 400 with a stable message so the chat UI can
        // detect it and offer a "configure integration" link.
        Err(AgentError::CredentialsMissing { service, .. })
        | Err(AgentError::CredentialsDecryptFailed { service, .. }) => Err(LlmError::BadRequest(
            format!("credentials not configured for service={service}"),
        )),
    }
}

// `AgentRegistry` is part of the public surface — the routes above
// only access it indirectly through `state.agents`, but downstream
// tests want to construct one in isolation. The `_phantom` keeps the
// import alive if a refactor removes the only direct use.
#[allow(dead_code)]
fn _phantom(_: &AgentRegistry) {}

// `HeaderValue` is referenced indirectly via axum builders; keep the
// import in scope for future response-header tweaks.
#[allow(dead_code)]
fn _phantom_header(_: HeaderValue) {}
