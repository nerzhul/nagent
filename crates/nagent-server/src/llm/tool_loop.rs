//! `llm::tool_loop` — the per-chat-completion tool loop.
//!
//! Each user message triggers a sequence of upstream rounds:
//!
//! 1. Open a fresh upstream `/v1/chat/completions` connection.
//! 2. Drain the SSE byte stream into the client channel (see
//! [`sse::drain_upstream_round`]), buffering any `tool_calls` deltas.
//! 3. If the LLM emitted one or more tool calls, dispatch each one
//! against the [`crate::agents::AgentRegistry`], append a
//! `role: "tool"` message with the result, and loop.
//! 4. When the LLM emits a final `finish_reason: "stop"` (no tool
//! calls), close the channel with `data: [DONE]\n\n`.
//!
//! Also owns the security plan #10 helper that enforces the
//! "`read_document` → `web_fetch` requires user confirmation" rule.

use std::time::Duration;

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::agents::{AgentRegistry, UserContext};
use crate::llm::sse::{
    drain_upstream_round, sse_error_event, sse_tool_call_event, sse_tool_result_event,
    UpstreamByteStream,
};

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
///
/// `chat_session_id` is the browser-supplied session id (read from
/// `X-Chat-Session-Id`). Threaded into the per-round `UserContext`
/// so session-scoped agents (`read_document`) can find their rows.
/// `None` for direct curl callers and for sessions that did not
/// send the header.
///
/// `user_id` is the authenticated user id from `AuthUser`
/// (SEV 2 fix). Threaded into the per-round `UserContext` so
/// per-user agents (`read_document`) can scope their queries.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_loop(
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    url: String,
    initial_body: Value,
    agents: Option<AgentRegistry>,
    max_rounds: u32,
    idle_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_stream: UpstreamByteStream,
    chat_session_id: Option<uuid::Uuid>,
    user_id: uuid::Uuid,
    web_fetch_allowlist: Vec<String>,
) {
    let agents = agents.unwrap_or_else(AgentRegistry::empty);
    let mut body = initial_body;
    let mut round: u32 = 0;
    let mut current_stream: Option<UpstreamByteStream> = Some(first_stream);
    // Security plan #10: track the set of agents invoked earlier
    // in this chat-completions turn so we can enforce the
    // "read_document → web_fetch requires user confirmation"
    // rule. The set resets every chat-completions call (each
    // call is a separate user message → separate "turn").
    let mut agents_invoked_this_turn: std::collections::HashSet<String> =
        std::collections::HashSet::new();
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
            tools = %crate::llm::sse::tool_call_names_pub(&outcome.tool_calls),
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
            // Security plan #10: enforce "read_document → web_fetch
            // requires user confirmation" at the tool-dispatch
            // site. The LLM cannot bypass this by ignoring the
            // system prompt — when the predicate fires, we replace
            // the tool result with an explanatory error so the
            // model reformulates ("please confirm") and the user
            // sees the request in chat. The next turn restarts with
            // `agents_invoked_this_turn` empty, so a user-confirmed
            // URL fetches normally.
            if name == "web_fetch"
                && agents_invoked_this_turn.contains("read_document")
                && web_fetch_needs_confirmation(&args_value, &web_fetch_allowlist)
            {
                let payload = "[error] `web_fetch` was called for a URL whose host is not in \
                     WEB_FETCH_ALLOWLIST, after `read_document` was invoked earlier in \
                     this turn. This is the indirect prompt-injection rule: the \
                     operator has not pre-authorised this host, and the user has not \
                     explicitly confirmed the fetch in chat. Ask the user to confirm \
                     by typing the URL in plain text and rephrasing the request; once \
                     they confirm, call web_fetch again on the next turn.";
                warn!(
                    agent = %name,
                    id = %tc.id,
                    url = %args_value.get("url").and_then(|v| v.as_str()).unwrap_or("?"),
                    "tool loop: refusing web_fetch after read_document without allowlist match"
                );
                let _ = tx
                    .send(Ok(Bytes::from(sse_tool_result_event(
                        &tc.id,
                        &name,
                        false,
                        // clippy: explicit `&` for clarity; `sse_tool_result_event`
                        // takes `&str` and String auto-derefs.
                        #[allow(clippy::needless_borrow)]
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
            let result = match agents.get(&name) {
                Some(agent) => {
                    // Per-tool-round `UserContext`. The chat-completions
                    // handler does not (yet) read the authenticated
                    // user off the request, so per-user agents that
                    // call `ctx.secret(...)` will surface
                    // `CredentialsMissing` from this anonymous
                    // pathway — wired-up production deployments must
                    // thread the session user through to this ctx in
                    // a follow-up. The plumbing here (resolver,
                    // services, cache) is already in place; only the
                    // user-id source needs wiring.
                    //
                    // When `chat_session_id` is set (browser sent
                    // `X-Chat-Session-Id`), use `for_chat_session`
                    // so agents like `read_document` can scope their
                    // queries to the right session. Otherwise fall
                    // back to `for_tests` and let the session-scoped
                    // agents surface a clear tool error.
                    //
                    // SEV 2 fix: `user_id` is the authenticated
                    // user id from `AuthUser`. The chat-completions
                    // middleware extracts it from the session
                    // cookie / bearer header and threads it through.
                    // Per-user agents (currently `read_document`)
                    // scope every DB query by `(user_id, session_id)`
                    // so a user cannot read another user's docs.
                    let services = crate::agents::ServiceRegistry::empty().into_arc();
                    let ctx = match chat_session_id {
                        Some(sid) => UserContext::for_chat_session(user_id, services, None, sid),
                        None => UserContext::for_tests(user_id, services),
                    };
                    match agent.invoke(&ctx, args_value).await {
                        Ok(s) => Ok(s),
                        Err(e) => Err(e.to_string()),
                    }
                }
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
            // Security plan #10: record this successful agent
            // invocation so subsequent web_fetch calls can be
            // checked against the read_document rule. Skipped on
            // the explicit-deny path above (we never ran the
            // agent, so it would be wrong to mark it as invoked).
            if ok {
                agents_invoked_this_turn.insert(name.clone());
            }
        }
    }
}

/// Security plan #10 helper. Returns `true` when `host` matches an
/// entry in `allowlist` (suffix match for `*.foo` entries, exact
/// match otherwise; bare `*` matches every host). Mirrors
/// `agents::web_fetch::host_matches_allowlist` but lives here so
/// the LLM tool loop does not need a cross-module import (the
/// web_fetch module keeps the same logic private for its own
/// invoke path).
fn host_matches_allowlist_simple(host: &str, allowlist: &[String]) -> bool {
    let host_lc = host.to_ascii_lowercase();
    for entry in allowlist {
        let entry = entry.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        if let Some(suffix) = entry.strip_prefix("*.") {
            if host_lc == suffix || host_lc.ends_with(&format!(".{suffix}")) {
                return true;
            }
        } else if host_lc == entry {
            return true;
        }
    }
    false
}

/// Security plan #10 helper. Decides whether a `web_fetch` call
/// needs explicit user confirmation based on (a) the
/// `WEB_FETCH_REQUIRE_CONFIRMATION` escape hatch and (b) whether
/// the URL's host is already in the operator-configured
/// `WEB_FETCH_ALLOWLIST`.
///
/// Returns `true` when the fetch should be refused (i.e. the LLM
/// must ask the user to confirm before re-invoking). `true` is
/// the conservative answer — any malformed URL, unknown scheme,
/// or empty host defaults to "needs confirmation" because we
/// cannot prove the host is already pre-authorised.
fn web_fetch_needs_confirmation(args: &Value, allowlist: &[String]) -> bool {
    // Escape hatch for self-hosted single-user deployments
    // where the operator trusts the model entirely.
    if let Ok(v) = std::env::var("WEB_FETCH_REQUIRE_CONFIRMATION") {
        if v == "false" || v == "0" {
            return false;
        }
    }
    let url = match args.get("url").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return true,
    };
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return true,
    };
    // Only web URLs (http / https) are eligible for the
    // allowlist shortcut — anything else (file://, data:, ftp)
    // was already rejected by the web_fetch agent itself, but
    // refuse it here too as defence in depth.
    if !matches!(parsed.scheme(), "http" | "https") {
        return true;
    }
    let host = match parsed.host_str() {
        Some(h) if !h.is_empty() => h.to_ascii_lowercase(),
        _ => return true,
    };
    !host_matches_allowlist_simple(&host, allowlist)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_matches_allowlist_simple_bare_star() {
        assert!(host_matches_allowlist_simple(
            "anywhere.example",
            &["*".into()]
        ));
        assert!(host_matches_allowlist_simple("127.0.0.1", &["*".into()]));
    }

    #[test]
    fn host_matches_allowlist_simple_exact() {
        let allow = vec!["example.com".into()];
        assert!(host_matches_allowlist_simple("example.com", &allow));
        assert!(!host_matches_allowlist_simple("foo.example.com", &allow));
        assert!(!host_matches_allowlist_simple("evil.com", &allow));
    }

    #[test]
    fn host_matches_allowlist_simple_wildcard_suffix() {
        let allow = vec!["*.wikipedia.org".into()];
        assert!(host_matches_allowlist_simple("wikipedia.org", &allow));
        assert!(host_matches_allowlist_simple("en.wikipedia.org", &allow));
        assert!(host_matches_allowlist_simple("a.b.wikipedia.org", &allow));
        assert!(!host_matches_allowlist_simple("wikipedia.com", &allow));
        assert!(!host_matches_allowlist_simple("evil.org", &allow));
    }

    #[test]
    fn host_matches_allowlist_simple_is_case_insensitive() {
        let allow = vec!["Example.COM".into()];
        assert!(host_matches_allowlist_simple("EXAMPLE.com", &allow));
        assert!(host_matches_allowlist_simple("example.com", &allow));
    }

    #[test]
    fn web_fetch_needs_confirmation_no_allowlist() {
        // Empty allowlist → every URL needs confirmation.
        let args = json!({"url": "https://example.com/x"});
        assert!(web_fetch_needs_confirmation(&args, &[]));
    }

    #[test]
    fn web_fetch_needs_confirmation_match_in_allowlist() {
        // Allowlist contains the host → no confirmation needed.
        let allow = vec!["*.wikipedia.org".into()];
        let args = json!({"url": "https://en.wikipedia.org/wiki/Foo"});
        assert!(!web_fetch_needs_confirmation(&args, &allow));
    }

    #[test]
    fn web_fetch_needs_confirmation_miss_in_allowlist() {
        // Allowlist does NOT contain the host → confirmation needed.
        let allow = vec!["*.wikipedia.org".into()];
        let args = json!({"url": "https://attacker.example/?d=x"});
        assert!(web_fetch_needs_confirmation(&args, &allow));
    }

    #[test]
    fn web_fetch_needs_confirmation_malformed_url_is_conservative() {
        // Any malformed URL defaults to "needs confirmation".
        let args = json!({"url": "not-a-url"});
        assert!(web_fetch_needs_confirmation(&args, &[]));
        let args = json!({"url": ""});
        assert!(web_fetch_needs_confirmation(&args, &[]));
    }

    #[test]
    fn web_fetch_needs_confirmation_non_http_scheme_is_conservative() {
        let allow = vec!["*".into()];
        let args = json!({"url": "file:///etc/passwd"});
        assert!(web_fetch_needs_confirmation(&args, &allow));
    }

    #[test]
    fn web_fetch_needs_confirmation_missing_url_is_conservative() {
        let args = json!({});
        assert!(web_fetch_needs_confirmation(&args, &[]));
    }

    #[test]
    fn web_fetch_needs_confirmation_escape_hatch_disables_check() {
        // WEB_FETCH_REQUIRE_CONFIRMATION=false → always allow,
        // even for untrusted URLs. Operators opt out explicitly
        // via the env var.
        // SAFETY: env-mutating test, unique var name.
        unsafe {
            std::env::set_var("WEB_FETCH_REQUIRE_CONFIRMATION", "false");
        }
        let args = json!({"url": "https://attacker.example/?d=x"});
        let allow = vec!["*.wikipedia.org".into()];
        assert!(!web_fetch_needs_confirmation(&args, &allow));
        unsafe {
            std::env::remove_var("WEB_FETCH_REQUIRE_CONFIRMATION");
        }
    }
}
