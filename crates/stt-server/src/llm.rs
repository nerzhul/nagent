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
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::agents::{AgentError, AgentRegistry};
use crate::config::LlmConfig;
use crate::llm_prompt::{inject_default_system_prompt, USER_LOCATION_MARKER};
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

    /// Borrow the live config (CORS allow-list, upstream URL, etc.).
    /// Used by the router builder to wire the CORS middleware without
    /// keeping a duplicate `Arc<LlmConfig>` in [`crate::AppState`].
    pub fn cfg(&self) -> &LlmConfig {
        &self.cfg
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

/// Drop the ephemeral `User's approximate location:` system message
/// the browser prepends when the admin has switched the feature off.
///
/// The browser only injects the block when the user has granted
/// consent, but the admin may still want to forbid the upstream model
/// from ever seeing it (compliance, sensitive deployment, …). The flag
/// defaults to `true` so the UI is the primary gate; this helper is a
/// defence-in-depth filter that runs after
/// [`inject_default_system_prompt`] so the admin's prompt block always
/// survives.
///
/// Matching is strict on the marker prefix to avoid clobbering an
/// unrelated system message the user happened to type. A no-op when
/// the flag is `true` or the body carries no location block.
fn strip_user_location_if_disabled(forward_body: &mut Value, allow: bool) {
    if allow {
        return;
    }
    let Some(messages) = forward_body
        .as_object_mut()
        .and_then(|o| o.get_mut("messages"))
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    messages.retain(|m| {
        let Some(role) = m.get("role").and_then(|v| v.as_str()) else {
            return true;
        };
        if role != "system" {
            return true;
        }
        // `content` may be a string OR a list of `{type, text}` parts
        // per the OpenAI multimodal schema. We only support the string
        // form (the browser always emits it); anything else is left
        // alone so a future multimodal prompt isn't accidentally
        // dropped by the kill-switch.
        match m.get("content").and_then(|v| v.as_str()) {
            Some(text) => !text.starts_with(USER_LOCATION_MARKER),
            None => true,
        }
    });
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
    let mut forward_body = serde_json::from_slice::<Value>(&body)
        .map_err(|e| LlmError::BadRequest(format!("invalid json body: {e}")))?;
    if let Some(obj) = forward_body.as_object_mut() {
        obj.insert("model".into(), json!(model));
        obj.insert("stream".into(), json!(true));
        // Merge the agent tools array into the request body. We do
        // this once, up front, so every round of the tool-loop carries
        // the same `tools` definition (Ollama expects the field on
        // every call). Client-supplied `tools` are preserved.
        let server_tools = state
            .agents
            .as_ref()
            .map(|a| a.tools_schema())
            .unwrap_or_default();
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
    let max_rounds = state.config.agents.llm_max_tool_rounds;
    let agents = state.agents.clone();

    // Channel that drives the response body. The tool-loop coroutine
    // pushes either upstream `data:` chunks (verbatim) or locally
    // synthesised `event: tool_call` / `event: tool_result` chunks
    // into this channel; axum streams them to the client.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(8);

    let http = llm.http.clone();
    let headers_clone = fwd.clone();
    let url_clone = upstream_url.clone();
    let body_for_loop = forward_body.clone();
    let first_stream: UpstreamByteStream = Box::pin(first_upstream.bytes_stream());
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

/// Per-round inner state for the upstream→client loop.
///
/// Each round we open a fresh upstream request and drain its SSE byte
/// stream into the client channel, buffering any `tool_calls` deltas
/// for the round we just finished.
struct ToolCallAccumulator {
    /// Buffered tool calls keyed by their `index` field in the SSE
    /// delta. Multiple `index` values are possible when the LLM emits
    /// parallel tool calls in one round (rare with the supported
    /// models but legal in the spec).
    by_index: std::collections::BTreeMap<u32, PendingToolCall>,
}

#[derive(Debug, Default, Clone)]
struct PendingToolCall {
    id: String,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    fn new() -> Self {
        Self {
            by_index: std::collections::BTreeMap::new(),
        }
    }
    fn apply_delta(&mut self, raw: &Value) {
        let Some(arr) = raw.as_array() else { return };
        for tc in arr {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let entry = self.by_index.entry(idx).or_default();
            if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                entry.id = id.to_string();
            }
            if let Some(func) = tc.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    entry.name = Some(name.to_string());
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    entry.arguments.push_str(args);
                }
            }
        }
    }
    fn into_sorted(self) -> Vec<PendingToolCall> {
        self.by_index.into_values().collect()
    }
}

/// Comma-separated list of tool names for log lines.
///
/// Names may be missing when the model emits the `arguments` delta
/// before the `name` (the SSE spec permits either order); we surface
/// `<unknown>` in that case so the count stays honest and the log
/// line is unambiguous. Arguments / payloads are deliberately omitted
/// — operators only need to know *which* agents ran, not *what they
/// were called with*.
fn tool_call_names(tool_calls: &[PendingToolCall]) -> String {
    tool_calls
        .iter()
        .map(|tc| tc.name.as_deref().unwrap_or("<unknown>"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Type-erased upstream byte stream we feed through the SSE parser.
/// `reqwest::Response::bytes_stream()` produces a concrete
/// `impl Stream<...>` we cannot name; round 0 hands us that stream
/// directly, round 1+ wrap a fresh `bytes_stream()` from a new
/// upstream response in the same boxed shape.
type UpstreamByteStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Run the tool loop until the LLM stops or we hit `max_rounds`.
///
/// All output is pushed into `tx` as either verbatim upstream SSE
/// bytes or locally synthesised `event: tool_call` /
/// `event: tool_result` frames. The function exits when the
/// conversation is complete (LLM emits `finish_reason: "stop"`) or
/// when an unrecoverable error occurs; either way, `tx` is dropped
/// before return so the axum body stream terminates cleanly.
///
/// `first_stream` is the body stream of the upstream response that
/// `chat_completions` already opened for round 0; round 1+ reuse the
/// shared `http` client and open their own connection.
#[allow(clippy::too_many_arguments)]
async fn run_tool_loop(
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    url: String,
    initial_body: Value,
    agents: Option<AgentRegistry>,
    max_rounds: u32,
    idle_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_stream: UpstreamByteStream,
) {
    let agents = agents.unwrap_or_else(AgentRegistry::empty);
    let mut body = initial_body;
    let mut round: u32 = 0;
    let mut current_stream: Option<UpstreamByteStream> = Some(first_stream);
    info!("tool loop: starting (max_rounds={max_rounds})");
    loop {
        round += 1;
        if round > max_rounds {
            let _ = tx
                .send(Ok(Bytes::from(sse_error_event(
                    "agent loop exceeded",
                    &format!("max tool rounds ({max_rounds}) reached"),
                ))))
                .await;
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }

        let stream = if let Some(s) = current_stream.take() {
            s
        } else {
            // Subsequent rounds: open a fresh upstream connection.
            let upstream = match http
                .post(&url)
                .headers(headers.clone())
                .body(body.to_string())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(Err(std::io::Error::other(format!(
                            "upstream connect failed: {e}"
                        ))))
                        .await;
                    return;
                }
            };
            if !upstream.status().is_success() {
                let status = StatusCode::from_u16(upstream.status().as_u16())
                    .unwrap_or(StatusCode::BAD_GATEWAY);
                let body = upstream.text().await.unwrap_or_default();
                warn!(%status, "upstream chat completion failed");
                let _ = tx
                    .send(Ok(Bytes::from(sse_error_event(
                        "upstream error",
                        &format!("status {status}: {body}"),
                    ))))
                    .await;
                let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
                return;
            }
            Box::pin(upstream.bytes_stream())
        };

        // Drain the upstream SSE stream. We parse each event so we can
        // buffer `tool_calls` deltas while forwarding everything else
        // verbatim.
        let outcome = drain_upstream_round(stream, idle_timeout, &tx).await;

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let _ = tx
                    .send(Err(std::io::Error::other(format!(
                        "upstream read failed: {e}"
                    ))))
                    .await;
                return;
            }
        };

        if outcome.tool_calls.is_empty() {
            // No tool calls — conversation is done. Always emit a
            // single `data: [DONE]` here because `drain_upstream_round`
            // swallows any upstream `[DONE]` (forwarding it would let
            // the browser cut the response mid-loop and drop our
            // subsequent `event: tool_call` / `event: tool_result`
            // frames — see the regression in `tests/agents.rs`). The
            // sentinel we send is the only one the client ever sees.
            info!(round, "tool loop: conversation complete (no tool_calls)");
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }
        info!(
            round,
            tools = %tool_call_names(&outcome.tool_calls),
            "tool loop: dispatching agents"
        );

        // There are tool calls to run. Append the assistant message
        // (with its tool_calls[]) and one `role: "tool"` message per
        // invocation to the body, then loop.
        let messages = match body
            .as_object_mut()
            .and_then(|o| o.get_mut("messages"))
            .and_then(|m| m.as_array_mut())
        {
            Some(m) => m,
            None => {
                let _ = tx
                    .send(Err(std::io::Error::other(
                        "internal: body has no messages array",
                    )))
                    .await;
                return;
            }
        };

        // Persist the assistant turn with the full `tool_calls[]`
        // array — this is what the LLM expects on the next round.
        let assistant_entry = json!({
            "role": "assistant",
            "content": outcome.assistant_text,
            "tool_calls": outcome.tool_calls.iter().map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments,
                    }
                })
            }).collect::<Vec<_>>(),
        });
        messages.push(assistant_entry);

        for tc in &outcome.tool_calls {
            // The model is required to set `function.name` before
            // emitting `finish_reason: "tool_calls"`. Defensively
            // fall back to a synthetic error if it didn't so the LLM
            // round can recover on the next iteration.
            let name = match &tc.name {
                Some(n) => n.clone(),
                None => {
                    let payload = "[error] tool call arrived without a function name".to_string();
                    let _ = tx
                        .send(Ok(Bytes::from(sse_tool_result_event(
                            &tc.id,
                            "<unknown>",
                            false,
                            &payload,
                        ))))
                        .await;
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "content": payload,
                    }));
                    continue;
                }
            };
            // Surface the call to the browser immediately so the
            // spinner bubble appears while we run the agent.
            let _ = tx
                .send(Ok(Bytes::from(sse_tool_call_event(
                    &tc.id,
                    &name,
                    &tc.arguments,
                    round,
                ))))
                .await;

            let args_value: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);
            let result = match agents.get(&name) {
                Some(agent) => match agent.invoke(args_value).await {
                    Ok(s) => Ok(s),
                    Err(e) => Err(e.to_string()),
                },
                None => Err(format!("unknown agent: `{name}`")),
            };
            let (ok, payload) = match &result {
                Ok(s) => (true, s.clone()),
                Err(e) => {
                    warn!(agent = %name, id = %tc.id, error = %e, "tool loop: agent invocation failed");
                    (false, format!("[error] {e}"))
                }
            };
            let _ = tx
                .send(Ok(Bytes::from(sse_tool_result_event(
                    &tc.id, &name, ok, &payload,
                ))))
                .await;
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tc.id,
                "content": payload,
            }));
        }
    }
}

/// Drain a single upstream SSE round and return what we learned.
///
/// `tx` receives the verbatim upstream SSE bytes — anything that
/// arrives from the upstream (including its final `[DONE]` marker) is
/// pushed straight through to the client. We additionally accumulate
/// `tool_calls` deltas internally and report them in the returned
/// `RoundOutcome` so the caller can decide whether to start a new
/// round.
async fn drain_upstream_round(
    upstream_bytes: impl futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
    idle_timeout: Duration,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<RoundOutcome, std::io::Error> {
    let mut acc = ToolCallAccumulator::new();
    let mut assistant_text = String::new();

    let mut sse = SseStream::new(idle_timeout, upstream_bytes);
    while let Some(event) = sse.next_event().await? {
        // The `[DONE]` sentinel is special: the client uses it to
        // close the response stream (`reader.cancel()` on the
        // browser). If we forward it here, the client cuts the
        // connection and any subsequent `event: tool_call` /
        // `event: tool_result` frames we emit on the same response
        // arrive after the cancel and are dropped — the tool bubble
        // never resolves. So we *swallow* `[DONE]` here; the
        // `run_tool_loop` outer function emits a single final
        // `[DONE]` after the last round has completed.
        if event.data.trim() == "[DONE]" {
            break;
        }
        // Forward the event verbatim, then parse it for tool_calls.
        // We *only* add an `event:` line when the upstream named the
        // event; this preserves the original SSE framing (no
        // synthetic `event: message`) so existing clients that expect
        // raw `data:` lines keep working byte-for-byte.
        let frame = match &event.name {
            Some(name) => format!("event: {name}\ndata: {d}\n\n", d = event.data),
            None => format!("data: {d}\n\n", d = event.data),
        };
        if tx.send(Ok(Bytes::from(frame))).await.is_err() {
            // Client disconnected mid-stream. Stop processing: the
            // outer loop will pick up on the dropped channel on the
            // next iteration.
            return Ok(RoundOutcome {
                assistant_text,
                tool_calls: acc.into_sorted(),
            });
        }
        let parsed: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array()) {
            for choice in choices {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                        assistant_text.push_str(text);
                    }
                    if let Some(tcs) = delta.get("tool_calls") {
                        acc.apply_delta(tcs);
                    }
                }
            }
        }
    }

    Ok(RoundOutcome {
        assistant_text,
        tool_calls: acc.into_sorted(),
    })
}

struct RoundOutcome {
    assistant_text: String,
    tool_calls: Vec<PendingToolCall>,
}

/// SSE event we have parsed out of the upstream byte stream.
struct SseEvent {
    name: Option<String>,
    data: String,
}

/// Line-oriented SSE parser wrapped around a byte stream with an
/// idle-timeout. The upstream sends events as
/// `event: name\ndata: …\n\n` (or just `data: …\n\n`); we accumulate
/// until we see a blank line, then yield the event. We tolerate CRLF
/// and tolerate comments / unknown fields.
struct SseStream<S> {
    src: S,
    timeout: Duration,
    buf: String,
}

impl<S> SseStream<S>
where
    S: futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin,
{
    fn new(timeout: Duration, src: S) -> Self {
        Self {
            src,
            timeout,
            buf: String::new(),
        }
    }

    async fn next_event(&mut self) -> Result<Option<SseEvent>, std::io::Error> {
        loop {
            // Split off the first complete event if we already have one
            // buffered.
            if let Some(idx) = self.buf.find("\n\n") {
                let raw = self.buf[..idx].to_string();
                self.buf.drain(..idx + 2);
                return Ok(Some(parse_sse_frame(&raw)));
            }
            // Otherwise wait for more bytes.
            let chunk = match tokio::time::timeout(self.timeout, self.src.next()).await {
                Ok(Some(Ok(b))) => b,
                Ok(Some(Err(e))) => {
                    return Err(std::io::Error::other(e.to_string()));
                }
                Ok(None) => {
                    if self.buf.trim().is_empty() {
                        return Ok(None);
                    }
                    // Flush whatever is left.
                    let raw = std::mem::take(&mut self.buf);
                    return Ok(Some(parse_sse_frame(&raw)));
                }
                Err(_elapsed) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "upstream idle timeout",
                    ));
                }
            };
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

fn parse_sse_frame(raw: &str) -> SseEvent {
    let mut name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
        // All other fields (id:, retry:, …) are intentionally
        // ignored — we forward what we know to the browser verbatim
        // and the LLM proxy doesn't depend on them.
    }
    SseEvent {
        name,
        data: data_lines.join("\n"),
    }
}

fn sse_tool_call_event(id: &str, name: &str, arguments: &str, index: u32) -> String {
    // `arguments` may be partial JSON if the model split the call
    // across SSE chunks — we forward what we have at the moment we
    // decide to dispatch. The browser is the only consumer of this
    // event for the live bubble; the LLM gets the full arguments
    // through the next round's request body.
    let args_value: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let payload = json!({
        "id": id,
        "name": name,
        "args": args_value,
        "arguments_raw": arguments,
        "index": index,
    });
    format!("event: tool_call\ndata: {payload}\n\n")
}

fn sse_tool_result_event(id: &str, name: &str, ok: bool, payload: &str) -> String {
    let summary = if ok {
        // Pull a short "summary" out of the JSON payload when
        // possible so the bubble can show something more useful than
        // the full text. Best-effort — falls back to the raw string
        // for non-JSON payloads.
        serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|v| v.get("summary").and_then(|s| s.as_str().map(String::from)))
            .unwrap_or_else(|| payload.chars().take(80).collect::<String>())
    } else {
        payload.chars().take(160).collect::<String>()
    };
    let payload_json = json!({
        "id": id,
        "name": name,
        "ok": ok,
        "summary": summary,
        "content": payload,
    });
    format!("event: tool_result\ndata: {payload_json}\n\n")
}

fn sse_error_event(reason: &str, detail: &str) -> String {
    let payload = json!({
        "error": reason,
        "detail": detail,
    });
    format!("event: error\ndata: {payload}\n\n")
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

/// `POST /v1/agents/:name/invoke` — direct agent invocation, used by
/// tests and `curl`. The chat UI goes through `/v1/chat/completions`
/// instead so the SSE stream stays consistent.
///
/// Body shape: `{"arguments": {...}}`. Returns
/// `{"name": "<agent>", "result": "<json string>"}` on success, or
/// an `LlmError` mapped to the appropriate HTTP status on failure
/// (400 for invalid args, 404 for unknown agent, 502 for upstream).
pub async fn agent_invoke(
    State(state): State<Arc<AppState>>,
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

    match agent.invoke(args).await {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(name: Option<&str>) -> PendingToolCall {
        PendingToolCall {
            id: "call_x".into(),
            name: name.map(str::to_owned),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn tool_call_names_joins_known_names() {
        let calls = vec![tc(Some("web_fetch")), tc(Some("web_search"))];
        assert_eq!(tool_call_names(&calls), "web_fetch, web_search");
    }

    #[test]
    fn tool_call_names_marks_missing_name_as_unknown() {
        // Parallel calls where the SSE delta order dropped the `name`
        // before `arguments`. The log must still be unambiguous.
        let calls = vec![tc(Some("web_fetch")), tc(None), tc(Some("weather"))];
        assert_eq!(tool_call_names(&calls), "web_fetch, <unknown>, weather");
    }

    #[test]
    fn tool_call_names_handles_empty_input() {
        assert_eq!(tool_call_names(&[]), "");
    }

    fn body_with_loc_marker() -> Value {
        // Hand-built payload matching what `chat.js` would send when
        // the user has shared their location: admin system prompt
        // (already prepended by `inject_default_system_prompt`),
        // followed by the browser-injected location block, then the
        // user's actual turn.
        json!({
            "messages": [
                { "role": "system", "content": "admin prompt" },
                {
                    "role": "system",
                    "content": format!("{USER_LOCATION_MARKER} lat=48.85, lon=2.35 (±65 m, captured 2026-09-26T14:05Z).")
                },
                { "role": "user", "content": "what's the weather?" },
            ]
        })
    }

    #[test]
    fn strip_user_location_keeps_block_when_allowed() {
        // Default-on path: the kill-switch flag is `true`, the
        // browser-injected block survives so the LLM can use it for
        // location-relative queries.
        let mut body = body_with_loc_marker();
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, true);
        assert_eq!(body, snapshot, "allow=true must be a pure no-op");
    }

    #[test]
    fn strip_user_location_drops_only_the_marker_block_when_disabled() {
        // Kill-switch path: the marker-prefixed system message is
        // dropped, but the admin prompt (different prefix) and the
        // user turn both survive untouched.
        let mut body = body_with_loc_marker();
        strip_user_location_if_disabled(&mut body, false);
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2, "location block must be removed");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "admin prompt");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn strip_user_location_is_a_noop_without_marker_block() {
        // The body never carried a location block (user hasn't shared
        // any). The helper must not mutate the body in either mode.
        let mut body = json!({
            "messages": [
                { "role": "system", "content": "admin" },
                { "role": "user", "content": "hi" },
            ]
        });
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, false);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn strip_user_location_does_not_touch_non_system_or_multimodal_messages() {
        // Defensive: a non-string `content` (OpenAI multimodal parts)
        // is left alone, and only `role: system` messages are
        // inspected.
        let mut body = json!({
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": format!("{USER_LOCATION_MARKER} multimodal") }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": format!("{USER_LOCATION_MARKER} user-side") }
                    ]
                }
            ]
        });
        let snapshot = body.clone();
        strip_user_location_if_disabled(&mut body, false);
        assert_eq!(
            body, snapshot,
            "non-string content must never be stripped by the kill-switch"
        );
    }
}
