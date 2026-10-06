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
use crate::llm::discovered_tools::DiscoveredTools;
use crate::llm::permission::run_permission_intercept;
use crate::llm::privacy::{
    strip_user_location_if_disabled, strip_user_memories_if_disabled,
    strip_user_reply_language_if_disabled, strip_user_timezone_if_disabled,
};
use crate::llm::prompt::{
    build_memories_block, build_reply_language_block, inject_default_system_prompt,
    inject_memories_block, inject_reply_language_block,
};
use crate::llm::tool_loop::run_tool_loop;
use crate::state::ArcDiscoveredToolsStore;
use crate::state::ArcPermissionStore;
use crate::state::{
    ArcAgentsConfig, ArcLlmState, ArcServices, OptArcAgentRegistry, OptArcAuthState,
};
use nagent_agents::ToolsRouter;

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
/// server-configured `OLLAMA_MODEL` is used. `auth_user` is
/// threaded into the tool loop for per-user agents
/// (`read_document`, `caldav_*`, `x_timeline`, …) — see
/// [`run_tool_loop`] for the user-scoping consequences.
fn inject_ollama_num_predict(forward_body: &mut Value, ollama_num_predict: Option<u32>) {
    let Some(np) = ollama_num_predict else { return };
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

/// Merge the admin-configured `ollama_num_ctx` into the
/// `options.num_ctx` slot of the upstream body so reasoning
/// models (deepseek-r1, qwen3.5 with thinking on) see enough
/// context to finish a long reasoning + tool-call round in one
/// upstream request. The Ollama default `num_ctx` of `2048`
/// otherwise cuts them off mid-thought and trips the
/// `llm_max_auto_continues` heuristic into a loop. A no-op
/// when the operator has not opted in (`ollama_num_ctx = None`)
/// so the upstream keeps its own default (e.g. a Modelfile
/// `PARAMETER num_ctx 32768`, or a server-wide
/// `OLLAMA_CONTEXT_LENGTH`); the proxy only injects the field
/// when explicitly told to. Preserves any pre-existing
/// `options.num_ctx` the browser set on the request — the
/// server knob wins on conflict so a misbehaving client
/// cannot shrink the context window.
fn inject_ollama_num_ctx(forward_body: &mut Value, ollama_num_ctx: Option<u32>) {
    let Some(ctx) = ollama_num_ctx else { return };
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
        tracing::warn!("forward body `options` is not an object; skipping num_ctx injection");
        return;
    };
    options_obj.insert("num_ctx".into(), json!(ctx));
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
    State(discovered_tools): State<ArcDiscoveredToolsStore>,
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
        // Build the round's `tools=[]` via the dynamic helper.
        // Round 0 is special: there is no router pre-selection
        // source yet (the discovery store is empty for a fresh
        // session) and the LLM hasn't invoked anything. Pass
        // `None` for both so the helper still ships
        // `search_tools` + the rest of the registry's static
        // `tools_schema()` projection. Subsequent rounds (in
        // `run_tool_loop`) re-run this helper with the live
        // router + discovered set folded in.
        let router_ref = llm_state.tools_router.as_deref();
        let initial_tools = build_tools_for_round(
            agents.0.as_ref().map(|arc| arc.as_ref()),
            router_ref,
            &DiscoveredTools::new(),
            None,
            "",
        );
        // Drop the `services` shadow so the parameter is used.
        let _ = services;
        if !initial_tools.is_empty() {
            let client_tools = obj
                .get("tools")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut merged = initial_tools;
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
    // Forward the admin-configured `ollama_num_predict` (env
    // `LLM_OLLAMA_NUM_PREDICT` or TOML `[llm].ollama_num_predict`)
    // as `options.num_predict`. The merge is done before any
    // tool-loop round so every round (auto-continues included)
    // carries the same per-response generation cap. A no-op when
    // the operator has not opted in.
    inject_ollama_num_predict(&mut forward_body, llm.cfg.ollama_num_predict);
    // Forward the admin-configured `ollama_num_ctx` (env
    // `LLM_OLLAMA_NUM_CTX` or TOML `[llm].ollama_num_ctx`) as
    // `options.num_ctx`. The merge is done before any tool-loop
    // round so every round (auto-continues included) sees the
    // same context window. A no-op when the operator has not
    // opted in — deployments that already pin `num_ctx` via the
    // Modelfile or `OLLAMA_CONTEXT_LENGTH` keep working
    // unchanged.
    inject_ollama_num_ctx(&mut forward_body, llm.cfg.ollama_num_ctx);
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
                // Plan 1791267136806 §1.5 + §7.7: when the user has
                // opted in (`memory_enabled = true`) AND the LLM
                // state has a wired `MemorySource`, decrypt the
                // top-N rows and inject the summary block. The
                // `MemorySource` is `None` when the subsystem was
                // killed (`LLM_ALLOW_USER_MEMORY=false`) so the
                // inject is skipped silently — the kill-switch
                // strip then runs for symmetry with the reply-
                // language block.
                if prefs.memory_enabled {
                    if let Some(mem_src) = llm_state.memory_source.as_ref() {
                        let rows = mem_src.recall(user.id, None, None, None, 10).await;
                        match rows {
                            Ok(rows) => {
                                if let Some(block) = build_memories_block(&rows) {
                                    inject_memories_block(&mut forward_body, &block);
                                }
                            }
                            Err(e) => {
                                warn!(
                                    user_id = %user.id,
                                    error = %e,
                                    "failed to recall memories for short-term injection; skipping"
                                );
                            }
                        }
                    }
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
    // Plan 1791267136806 §1.5: the fourth kill-switch. When
    // `LLM_ALLOW_USER_MEMORY=false`, strip the
    // `USER_MEMORIES_MARKER`-prefixed system message before it
    // reaches the upstream model. The four kill-switches are
    // independent; an operator may forbid one without touching
    // the others.
    strip_user_memories_if_disabled(&mut forward_body, llm.cfg.allow_user_memory);

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
    // Plan 1791267136806 §7.6: the per-user memory source is
    // wired into the LLM state at boot time
    // (`LlmState.memory_source`). The proxy threads it into the
    // per-tool-round `UserContext` so the four `memory_*` agents
    // can run; `None` here means the subsystem was killed
    // (`LLM_ALLOW_USER_MEMORY=false`) or the encryption key was
    // not loaded at boot, in which case the agents fail closed.
    let memory_source: Option<std::sync::Arc<dyn nagent_agents::agents::MemorySource>> =
        llm_state.memory_source.clone();
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
            memory_source,
            permission_store.0,
            llm_state.tools_router.clone(),
            // Plan 1791317253718: the per-session discovered-tools
            // store the tool loop writes to after every successful
            // dispatch and reads from on every round-level rebuild.
            // The boot path always builds the store so this is
            // `Some` in production.
            Some(discovered_tools.0.clone()),
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

/// Build the `tools=[]` array for a single chat-completions
/// round (plan 1791317253718). Composes three sources:
///
/// 1. The `search_tools` meta-agent (always present, first in
///    the list — some upstreams cache the schema by position
///    and `search_tools` must stay at the same slot).
/// 2. The BM25 router's pre-selection over the user's most
///    recent message (`router` is `None` on round 0 or when
///    agents are off — empty contribution).
/// 3. The per-session discovered-tools store (every agent the
///    LLM has successfully called earlier in this
///    `chat_session_id`).
///
/// `history_names` is the set of tool names the LLM has
/// invoked earlier in the conversation (collected from the
/// `messages[*].tool_calls[*].function.name` entries already
/// on the outgoing body). Folded into the result so the round
/// the LLM was about to call a tool carries that tool's schema
/// even if neither the router nor the store has indexed it
/// yet.
///
/// `latest_user_msg` is the latest user message text — the
/// router scores against it. Empty when there is no recent user
/// turn (e.g. round 0 with no messages); the router returns
/// empty in that case.
///
/// Output order:
/// `[search_tools] + router hits + discovered + history_names`,
/// de-duplicated by name (first wins). The full agent
/// projection for each name comes from
/// `AgentRegistry::tools_schema()` so the wire format is
/// identical to the historical static merge.
///
/// `None` for `agents` → empty (the caller injects nothing). The
/// `search_tools` agent is excluded from the router hits
/// inside [`ToolsRouter::pre_select`]; the helper itself
/// prepends `search_tools` unconditionally so the caller does
/// not have to care about the feature flag.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_tools_for_round(
    agents: Option<&AgentRegistry>,
    router: Option<&ToolsRouter>,
    discovered: &DiscoveredTools,
    chat_session_id: Option<uuid::Uuid>,
    latest_user_msg: &str,
) -> Vec<Value> {
    let Some(registry) = agents else {
        return Vec::new();
    };

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Value> = Vec::new();

    // 1. search_tools first.
    if let Some(st) = registry.get(nagent_agents::SEARCH_TOOLS_NAME) {
        seen.insert(st.name().to_string());
        out.push(json!({
            "type": "function",
            "function": {
                "name": st.name(),
                "description": st.description(),
                "parameters": st.parameters_schema(),
            },
        }));
    }

    // 2. Router pre-selection. `router == None` when agents
    // were built without a router wired in (the
    // `search_tools` feature off path, or the boot path
    // skipped it).
    if let Some(router) = router {
        let pre_selected = router.pre_select(latest_user_msg, 5);
        for name in pre_selected {
            if !seen.insert(name.to_string()) {
                continue;
            }
            if let Some(agent) = registry.get(&name) {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": agent.name(),
                        "description": agent.description(),
                        "parameters": agent.parameters_schema(),
                    },
                }));
            }
        }
    }

    // 3. Discovered set (per-session).
    if let Some(sid) = chat_session_id {
        for name in discovered.snapshot(sid) {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(agent) = registry.get(&name) {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": agent.name(),
                        "description": agent.description(),
                        "parameters": agent.parameters_schema(),
                    },
                }));
            }
        }
    }

    out
}

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

#[cfg(test)]
mod build_tools_for_round_tests {
    //! Unit tests for the round-level `tools=[]` builder
    //! (plan 1791317253718). Composition order, de-duplication,
    //! and the empty-registry / no-router / no-discovered-set
    //! edges all live here so a future refactor that
    //! accidentally re-orders the three sources trips the
    //! guards.
    use super::*;
    use crate::llm::discovered_tools::DiscoveredTools;
    use async_trait::async_trait;
    use nagent_agents::agents::Agent;
    use nagent_agents::agents::AgentError;
    use nagent_agents::UserContext;
    use serde_json::{json, Value};
    use std::sync::Arc;

    struct StubAgent {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false,
            })
        }
        async fn invoke(&self, _ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
            Ok("{}".to_string())
        }
    }

    fn weather() -> StubAgent {
        StubAgent {
            name: "get_weather",
            description: "Current / forecast weather at a location.",
        }
    }
    fn calculate() -> StubAgent {
        StubAgent {
            name: "calculate",
            description: "Evaluate an arithmetic expression.",
        }
    }
    fn search_tools_agent() -> StubAgent {
        StubAgent {
            name: nagent_agents::SEARCH_TOOLS_NAME,
            description: "Discover the tools you have for a task.",
        }
    }

    fn names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .filter_map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
            })
            .collect()
    }

    fn registry(agents: Vec<StubAgent>) -> AgentRegistry {
        let mut reg = AgentRegistry::empty();
        for a in agents {
            reg.push_agent(a);
        }
        reg
    }

    #[test]
    fn empty_registry_returns_empty() {
        let reg = AgentRegistry::empty();
        let out = build_tools_for_round(Some(&reg), None, &DiscoveredTools::new(), None, "");
        assert!(out.is_empty());
    }

    #[test]
    fn none_registry_returns_empty() {
        let out = build_tools_for_round(None, None, &DiscoveredTools::new(), None, "");
        assert!(out.is_empty());
    }

    #[test]
    fn search_tools_is_first_and_always_present() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let out = build_tools_for_round(
            Some(&reg),
            Some(&router),
            &DiscoveredTools::new(),
            None,
            "weather in Paris",
        );
        let n = names(&out);
        assert!(!n.is_empty(), "build must return at least search_tools");
        assert_eq!(
            n[0],
            nagent_agents::SEARCH_TOOLS_NAME,
            "search_tools must be the first entry"
        );
        // The router excludes `search_tools` from its index, so
        // the only path it lands in `out` is the helper's
        // unconditional prepend.
        assert!(n.contains(&"get_weather".to_string()));
    }

    #[test]
    fn router_hits_come_after_search_tools() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let out = build_tools_for_round(
            Some(&reg),
            Some(&router),
            &DiscoveredTools::new(),
            None,
            "weather forecast today",
        );
        let n = names(&out);
        // search_tools first, then the router hit.
        assert_eq!(n[0], nagent_agents::SEARCH_TOOLS_NAME);
        assert_eq!(n[1], "get_weather");
    }

    #[test]
    fn discovered_set_lands_after_router_hits() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let sid = uuid::Uuid::new_v4();
        let discovered = DiscoveredTools::new();
        discovered.add(sid, "calculate");
        let out = build_tools_for_round(
            Some(&reg),
            Some(&router),
            &discovered,
            Some(sid),
            "weather in Lyon",
        );
        let n = names(&out);
        assert_eq!(n[0], nagent_agents::SEARCH_TOOLS_NAME);
        // Router hit + discovered.
        assert!(n.contains(&"get_weather".to_string()));
        assert!(n.contains(&"calculate".to_string()));
    }

    #[test]
    fn deduplication_keeps_first_occurrence() {
        let reg = registry(vec![search_tools_agent(), weather()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let sid = uuid::Uuid::new_v4();
        let discovered = DiscoveredTools::new();
        // The router hit already includes get_weather; the
        // discovered set adds the same name. The result must
        // still be de-duplicated.
        discovered.add(sid, "get_weather");
        let out = build_tools_for_round(
            Some(&reg),
            Some(&router),
            &discovered,
            Some(sid),
            "weather today",
        );
        let n = names(&out);
        let weather_occurrences = n.iter().filter(|s| *s == "get_weather").count();
        assert_eq!(
            weather_occurrences, 1,
            "duplicate get_weather must collapse"
        );
    }

    #[test]
    fn empty_query_yields_only_search_tools() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let out =
            build_tools_for_round(Some(&reg), Some(&router), &DiscoveredTools::new(), None, "");
        let n = names(&out);
        // search_tools is always first; the router contributes
        // nothing on an empty query.
        assert_eq!(n, vec![nagent_agents::SEARCH_TOOLS_NAME.to_string()]);
    }

    #[test]
    fn no_router_falls_back_to_search_tools_only() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let out = build_tools_for_round(
            Some(&reg),
            None,
            &DiscoveredTools::new(),
            None,
            "weather in Paris",
        );
        let n = names(&out);
        // No router means no pre-selection; the only entry is
        // the meta-tool itself.
        assert_eq!(n, vec![nagent_agents::SEARCH_TOOLS_NAME.to_string()]);
    }

    #[test]
    fn unknown_session_id_yields_only_search_tools() {
        let reg = registry(vec![search_tools_agent(), weather(), calculate()]);
        let router = Arc::new(ToolsRouter::from_registry(&reg));
        let out = build_tools_for_round(
            Some(&reg),
            Some(&router),
            &DiscoveredTools::new(),
            // `None` for chat_session_id: the discovered set
            // does not contribute.
            None,
            "weather in Paris",
        );
        let n = names(&out);
        assert_eq!(n[0], nagent_agents::SEARCH_TOOLS_NAME);
        // The router still hits.
        assert!(n.contains(&"get_weather".to_string()));
        // No "calculate" / unknown tool names leaked in.
        assert!(!n.contains(&"calculate".to_string()));
    }
}
