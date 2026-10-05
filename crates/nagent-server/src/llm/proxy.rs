//! `llm::proxy` — the top-level HTTP handlers for `/v1/*`.
//!
//! - [`chat_completions`] is the streaming OpenAI-compatible proxy;
//! it owns the SSE response body and hands the upstream byte
//! stream to [`crate::llm::tool_loop::run_tool_loop`].
//! - [`models_list`] proxies the upstream's `/v1/models` list (or
//! returns a single fallback when the upstream is unreachable).
//! - [`agents_list`] / [`agent_invoke`] are the direct agent HTTP
//! surface. Both moved into [`crate::agents::routes`] as part of
//!  the thin wrappers here re-export them under the
//! historical `llm::agents_list` / `llm::agent_invoke` names so
//! integration tests keep compiling unchanged.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use futures_util::stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{debug, warn};

use crate::agents::{AgentRegistry, AgentRegistryNewtype, ResolverSecretSource};
use crate::config::LlmConfig;
use crate::llm::client::{parse_chat_session_header, LlmError};
use crate::llm::permission::run_permission_intercept;
use crate::llm::privacy::{
    strip_user_location_if_disabled, strip_user_reply_language_if_disabled,
    strip_user_timezone_if_disabled,
};
use crate::llm::prompt::{
    build_reply_language_block, inject_default_system_prompt, inject_reply_language_block,
};
use crate::llm::tool_loop::run_tool_loop;
use crate::state::ArcPermissionStore;
use crate::state::{
    ArcAgentsConfig, ArcLlmState, ArcServices, OptArcAgentRegistry, OptArcAuthState,
};

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
/// Merge the admin-configured `num_predict` into the
/// `options.num_predict` slot of the upstream body so reasoning
/// models (deepseek-r1, qwen3.5 with thinking on, o1/o3) get a
/// per-response generation budget that lets them finish a
/// reasoning + tool-call round in one upstream request. The
/// Ollama default of `128` tokens otherwise cuts them off mid-
/// thought and trips the `llm_max_auto_continues` heuristic
/// into a loop. A no-op when the operator has not opted in
/// (`num_predict = None`) so the upstream keeps its own default;
/// the proxy only injects the field when explicitly told to.
/// Preserves any pre-existing `options.num_predict` the client
/// set on the body — the server knob wins on conflict so a
/// misbehaving client cannot force a tiny `num_predict` on a
/// reasoning model and re-create the loop.
fn inject_num_predict(forward_body: &mut Value, num_predict: Option<u32>) {
    let Some(np) = num_predict else { return };
    let Some(obj) = forward_body.as_object_mut() else {
        return;
    };
    let options = if let Some(existing) = obj.get_mut("options") {
        existing
    } else {
        obj.insert("options".into(), json!({}));
        obj.get_mut("options")
            .expect("just inserted `options` as object")
    };
    let Some(options_obj) = options.as_object_mut() else {
        // `options` was set by the client to a non-object
        // (e.g. a string). Don't overwrite — let the upstream
        // surface the mismatch. Logging here is enough; the
        // request is malformed regardless.
        tracing::warn!("forward body `options` is not an object; skipping num_predict injection");
        return;
    };
    options_obj.insert("num_predict".into(), json!(np));
}

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
    State(_agents_cfg): State<ArcAgentsConfig>,
    State(services): State<ArcServices>,
    State(auth_state): State<OptArcAuthState>,
    State(permission_store): State<ArcPermissionStore>,
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
    // Forward the admin-configured `num_predict` (env `LLM_NUM_PREDICT`
    // or TOML `[llm].num_predict`) as `options.num_predict`. The merge
    // is done before any tool-loop round so every round (auto-continues
    // included) carries the same per-response generation cap. A no-op
    // when the operator has not opted in.
    inject_num_predict(&mut forward_body, llm.cfg.num_predict);
    // Inject the per-user reply-language block (after the admin
    // prompt so the admin's instructions stay authoritative at
    // `messages[0]`). The block is server-prepended because the
    // user's preference lives in the `user_preferences` row and the
    // browser intentionally does NOT send it on every request —
    // sending it server-side keeps the wire shape minimal and lets
    // the admin kill-switch (`LLM_ALLOW_USER_REPLY_LANGUAGE`)
    // decide whether to keep or drop the hint before it reaches the
    // upstream model. Skipped on the anonymous / `auth.enabled =
    // false` path (no AuthUser, no AuthState) — falls back to the
    // default "match the user's input language" behaviour.
    if let (Some(axum::Extension(user)), Some(auth_arc)) =
        (auth_user.as_ref(), auth_state.0.as_ref())
    {
        match auth_arc.store.for_user(user.id).preferences().get().await {
            Ok(prefs) => {
                if let Some(block) = build_reply_language_block(prefs.reply_language.as_deref()) {
                    inject_reply_language_block(&mut forward_body, &block);
                }
            }
            Err(e) => {
                // A read failure on the per-user preferences row must
                // never break the chat: log and continue without the
                // hint. The default prompt-tail covers the case where the
                // user hasn't set a preference, so the LLM still gets
                // sensible guidance.
                warn!(
                    user_id = %user.id,
                    error = %e,
                    "failed to read user_preferences for reply-language injection; skipping"
                );
            }
        }
    }
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
    // Same kill-switch treatment for the per-user reply-language
    // block: when `LLM_ALLOW_USER_REPLY_LANGUAGE=false`, strip the
    // server-injected block matched on the exact `The user's
    // preferred reply language is` prefix. The three kill-switches
    // are independent so operators can forbid one without touching
    // the others.
    strip_user_reply_language_if_disabled(&mut forward_body, llm.cfg.allow_user_reply_language);

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
        let upstream_status = first_upstream.status();
        let status =
            StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = first_upstream.text().await.unwrap_or_default();
        warn!(%status, "upstream chat completion failed");

        // Upstream rejected the request because the requested model
        // is unknown to it. Surface a structured, friendly error with
        // the upstream's model list so the chat UI can render a
        // picker instead of a wall of JSON. We also fan out a
        // follow-up `/v1/models` so the response includes the
        // available alternatives even when the upstream hides them
        // behind a 404. If that follow-up also fails (degraded
        // upstream), the response is still actionable — `available`
        // is `None` and the UI falls back to the raw message.
        if let Some((name, upstream_message)) =
            crate::llm::client::detect_model_not_found(status, &body)
        {
            let models_url = format!("{}/v1/models", llm.cfg.base_url.trim_end_matches('/'));
            let available = match llm.http.get(&models_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<serde_json::Value>().await {
                        Ok(v) => v.get("data").and_then(|d| d.as_array()).map(|arr| {
                            arr.iter()
                                .filter_map(|m| {
                                    m.get("id").and_then(|id| id.as_str()).map(str::to_string)
                                })
                                .collect::<Vec<_>>()
                        }),
                        Err(_) => None,
                    }
                }
                _ => None,
            };
            return Err(LlmError::ModelNotFound {
                name,
                upstream_message,
                available,
            });
        }

        return Err(LlmError::Upstream { status, body });
    }

    let timeout = llm.cfg.request_timeout;
    let max_rounds = llm_state.cfg.llm_max_tool_rounds;
    // Plan R8 follow-up: max number of auto-continue rounds when the
    // reasoning stream hits `finish_reason: "length"` mid-reasoning.
    // See `tool_loop::run_tool_loop` for the heuristic and
    // `docs/llm-configuration.md` for the full contract.
    let max_auto_continues = llm_state.cfg.llm_max_auto_continues;
    let agents = agents.0.clone().map(|arc| (*arc).clone());

    // Channel that drives the response body. The tool-loop coroutine
    // pushes either upstream `data:` chunks (verbatim) or locally
    // synthesised `event: tool_call` / `event: tool_result` chunks
    // into this channel; axum streams them to the client.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);

    let http = llm.http.clone();
    let headers_clone = fwd.clone();
    let url_clone = upstream_url.clone();
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
    // Wire the per-user credential resolver into the tool loop
    // so agents that read `ctx.secret(...)` (currently
    // `caldav_list_events` / `caldav_get_event` /
    // `caldav_create_event` and `x_timeline`) actually hit the
    // vault. The resolver lives on `AuthState` (built at boot
    // when `[auth.credentials].key` is configured); `None` on
    // the `auth.enabled = false` trust boundary and on the
    // integration tests that skip the credentials subsystem. The
    // adapter is `ResolverSecretSource`, the server-side
    // implementation of the agents crate's `SecretSource` trait.
    let resolver: Option<std::sync::Arc<dyn crate::agents::SecretSource>> = auth_state
        .0
        .as_ref()
        .and_then(|auth| auth.credential_resolver.clone())
        .map(|r| std::sync::Arc::new(ResolverSecretSource::new(r)) as _);
    // Plan 4.C: the tool loop is now generic over
    // `Agent::requires_confirmation`. The cross-agent rule (e.g.
    // "`read_document` → `web_fetch` needs confirmation") lives
    // entirely in the `web_fetch` agent's own impl + reads
    // `cfg.web_fetch.allowlist` directly via its captured
    // `WebFetchConfig`. No allowlist needs to be threaded through
    // the tool loop here.

    // Permission intercept: drive any decision sentinel at the
    // start of the latest user message BEFORE the upstream round
    // begins. The helper pushes synthetic `tool_call` /
    // `tool_result` SSE frames into the response channel (so the
    // chat UI sees the approval outcome immediately) and returns
    // a new body with the sentinel stripped and the synthetic
    // tool round appended; `Some((n, sender))` here is the
    // fallback when no sentinel was present.
    let (body_for_loop, perm_pre_frames) = match run_permission_intercept(
        &forward_body,
        agents.as_ref(),
        chat_session_id,
        user_id,
        resolver.clone(),
        permission_store.0.clone(),
    )
    .await
    {
        Some((new_body, frames)) => (new_body, frames),
        None => (forward_body, Vec::new()),
    };

    let (tx_perm, rx_perm) =
        tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(perm_pre_frames.len().max(1));
    for frame in perm_pre_frames {
        let _ = tx_perm.send(Ok(frame)).await;
    }
    drop(tx_perm);

    tokio::spawn(async move {
        run_tool_loop(
            http,
            headers_clone,
            url_clone,
            body_for_loop,
            agents,
            max_rounds,
            max_auto_continues,
            timeout,
            tx,
            first_stream,
            chat_session_id,
            user_id,
            resolver,
            permission_store.0,
        )
        .await;
    });

    // Drain the pre-frames (if any) BEFORE the tool-loop output
    // so the chat UI sees the approval outcome first, then the LLM
    // summary. We model this with a small `Either` style channel
    // wrapper below.
    let rx = merge_pre_then_loop(rx_perm, rx);

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

/// Fetch the upstream `/v1/models` and return the parsed list of
/// model ids. Used by [`crate::http::features::Features::from_state`]
/// to populate the `llm_models` field of `GET /api/features` so the
/// frontend can paint the Discussion-mode model dropdown without a
/// dedicated `/v1/models` round-trip.
///
/// The fetch is bounded by [`UPSTREAM_MODELS_TIMEOUT`] — `/api/features`
/// is on every page-load hot path, so we refuse to block the response
/// on a slow / unreachable upstream. On timeout, network error, non-2xx
/// upstream status, or any other parse failure we fall back to a
/// single-entry list built around the operator-configured
/// `[llm].default_model`. The fallback keeps the dropdown usable; a
/// user who wants more models just fixes the upstream.
///
/// Returning `Vec<String>` (not the raw OpenAI JSON envelope) lets the
/// features endpoint serialise it directly without re-shaping.

/// Merge a permission-decision pre-frame stream with the main
/// tool-loop stream by draining the pre-frames first, then the
/// tool-loop frames. The pre-stream always closes before main is
/// touched (the caller drops its sender after pushing the
/// pre-frames), so a simple sequential drain suffices. The
/// spawned task ends when both channels close.
fn merge_pre_then_loop(
    pre: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
    main: tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>>,
) -> tokio::sync::mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        let mut pre = pre;
        let mut main = main;
        // Drain the pre-stream first.
        while let Some(item) = pre.recv().await {
            if tx.send(item).await.is_err() {
                return;
            }
        }
        // Then drain the main stream.
        while let Some(item) = main.recv().await {
            if tx.send(item).await.is_err() {
                return;
            }
        }
    });
    rx
}

pub(crate) async fn fetch_upstream_model_list(
    client: &crate::llm::client::LlmClient,
) -> Vec<String> {
    let url = format!("{}/v1/models", client.cfg().base_url.trim_end_matches('/'));
    debug!(%url, "fetching upstream models for /api/features");

    let mut req = client.http.get(&url);
    if let Some((name, value)) = client.auth_header() {
        req = req.header(name, value);
    }

    let send_result = req.send().await;
    let parse = async {
        let resp = send_result.map_err(|e| format!("upstream send failed: {e}"))?;
        if !resp.status().is_success() {
            warn!(status = %resp.status(), "upstream models failed");
            return Ok::<_, String>(None);
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| format!("upstream read failed: {e}"))?;
        let arr = body.get("data").and_then(|d| d.as_array());
        let models = arr
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Some(models))
    };

    match tokio::time::timeout(UPSTREAM_MODELS_TIMEOUT, parse).await {
        Ok(Ok(Some(models))) if !models.is_empty() => models,
        Ok(Ok(Some(_))) => {
            // Upstream reachable but returned an empty list — still
            // fall back to the default so the dropdown has at least
            // one entry rather than rendering empty.
            warn!("upstream models list empty; falling back to default_model");
            vec![client.cfg().default_model.clone()]
        }
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
            warn!("upstream models fetch failed; falling back to default_model");
            vec![client.cfg().default_model.clone()]
        }
    }
}

/// Hard ceiling on the upstream `/v1/models` fetch from inside
/// `GET /api/features`. The endpoint is fetched on every page load
/// (and on every `visibilitychange` from `chat.js`), so an unbounded
/// upstream connect timeout would degrade page-load latency for any
/// operator whose Ollama host happens to be down. 3 seconds is short
/// enough to fail fast on a typical LAN miss while still tolerating
/// a slow first-byte from a healthy Ollama under load.
pub(crate) const UPSTREAM_MODELS_TIMEOUT: Duration = Duration::from_secs(3);

/// `GET /v1/models` — proxies the upstream's model list verbatim.
///
/// When the upstream is unreachable we still return a JSON body with
/// `[{ "id": "<OLLAMA_MODEL>" }]` so the UI dropdown always has at
/// least one entry.
///
/// **Note**: this handler is kept for direct callers (`curl`, SDKs,
/// out-of-tree integrations). The browser UI no longer hits it — the
/// chat dropdown is populated from `GET /api/features`'s `llm_models`
/// field instead. The body shape is unchanged so the legacy callers
/// keep working without modification.
pub async fn models_list(State(llm_state): State<ArcLlmState>) -> Result<Response, LlmError> {
    let llm = &llm_state.client;
    let models = fetch_upstream_model_list(llm).await;
    let body = json!({
        "object": "list",
        "data": models
            .into_iter()
            .map(|id| json!({ "id": id, "object": "model" }))
            .collect::<Vec<_>>()
    })
    .to_string();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .expect("static response builder is valid"))
}

// ---------------------------------------------------------------------------
// Agents HTTP surface — moved to `crate::agents::routes` . The
// `llm::agents_list` and `llm::agent_invoke` names are kept here as
// thin re-exports so existing imports keep compiling.
// ---------------------------------------------------------------------------

/// `GET /v1/agents` — list every agent registered on this server.
///
/// Returns an empty array when agents are disabled (so the browser
/// can render the "no agents" hint without special-casing 404).
pub async fn agents_list(State(agents): State<AgentRegistryNewtype>) -> Result<Response, LlmError> {
    crate::agents::routes::agents_list(State(agents)).await
}

/// `POST /v1/agents/:name/invoke` — direct agent invocation, used by
/// tests and `curl`. The chat UI goes through `/v1/chat/completions`
/// instead so the SSE stream stays consistent.
pub async fn agent_invoke(
    State(agents): State<AgentRegistryNewtype>,
    State(services): State<ArcServices>,
    State(auth_state): State<OptArcAuthState>,
    auth_user: Option<axum::Extension<crate::auth::session::AuthUser>>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, LlmError> {
    crate::agents::routes::agent_invoke(
        State(agents),
        State(services),
        State(auth_state),
        auth_user,
        Path(name),
        body,
    )
    .await
}

// Silence the unused-import lint on `LlmConfig` / `AgentRegistry` if a
// future refactor removes the only reference inside this file: both
// are part of the public surface even when only used through trait
// methods.
#[allow(dead_code)]
fn _phantom(_: &LlmConfig, _: &AgentRegistry) {}
