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
//! Plan 4.C: the cross-agent confirmation rule (e.g.
//! "`read_document` → `web_fetch` requires user confirmation")
//! lives entirely in the agent's own
//! [`crate::agents::Agent::requires_confirmation`] impl; this
//! module never has to know about a specific agent by name.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use bytes::Bytes;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::agents::{AgentRegistry, SecretSource, UserContext};
use crate::llm::permission::{approval_prompt, approval_prompt_json, PermissionStore};
use crate::llm::sse::{
    drain_upstream_round, sse_error_event, sse_tool_call_event, sse_tool_result_event,
    sse_tool_result_needs_approval, UpstreamByteStream,
};
use nagent_agents::agents::MemorySource;

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
///
/// `resolver` is the per-user credential resolver wrapped as
/// `Arc<dyn SecretSource>` (the server's `ResolverSecretSource`
/// adapts `CredentialResolver`). Wired into the per-round
/// `UserContext` so per-user agents that read `ctx.secret(...)`
/// (`caldav_list_events`, `x_timeline`, …) actually hit the
/// vault. `None` on the anonymous / `auth.enabled = false`
/// path; per-user agents then surface `CredentialsMissing`
/// without touching the DB, which is the right behaviour on
/// that trust boundary.
///
/// `permission_store` carries the per-session pending approvals and
/// session-wide overrides; the tool loop pushes a `PendingApproval`
/// on every `NeedsConfirmation` and consults the override set
/// before calling `Agent::requires_confirmation` so the
/// `[APPROVE_ALWAYS:...]` sentinel can skip the confirmation card
/// for subsequent calls of the same tool in the same session.
///
/// `max_rounds` is the maximum number of tool-call rounds a
/// single user turn may trigger before the proxy bails out and
/// surfaces an error bubble. This is purely a runaway guard for
/// agent loops — it has nothing to do with the LLM's reasoning
/// length, which is bounded by `max_auto_continues`.
///
/// `max_auto_continues` is the maximum number of auto-continue
/// rounds the loop appends when the upstream emits
/// `finish_reason: "length"` while still in the reasoning
/// phase of a reasoning-capable model (qwen3.5 with reasoning
/// on, DeepSeek-R1, o1/o3, …). Each round asks the model to
/// "continue from where you left off and produce the visible
/// answer now" without re-emitting the prior reasoning
/// (already streamed verbatim to the client via SSE). The
/// default of `32` is generous — a long question rarely needs
/// more than a handful of truncations on upstreams with a
/// sane `num_ctx`; raise the cap for providers with a small
/// fixed context window. Set to `0` to disable the heuristic
/// (the loop then closes the stream on truncation). See
/// `docs/llm-configuration.md` for the full contract.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_loop(
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    url: String,
    initial_body: Value,
    agents: Option<AgentRegistry>,
    max_rounds: u32,
    max_auto_continues: u32,
    idle_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_stream: UpstreamByteStream,
    chat_session_id: Option<uuid::Uuid>,
    user_id: uuid::Uuid,
    resolver: Option<Arc<dyn SecretSource>>,
    memory_source: Option<Arc<dyn MemorySource>>,
    permission_store: PermissionStore,
) {
    let agents = agents.unwrap_or_else(AgentRegistry::empty);
    let mut body = initial_body;
    // `tool_round` counts only rounds that produced at least one
    // tool call. The reasoning path does not consume this
    // budget — a long thinking chain followed by a final answer
    // is a single round from the loop's perspective.
    let mut tool_round: u32 = 0;
    let mut auto_continue_count: u32 = 0;
    let mut current_stream: Option<UpstreamByteStream> = Some(first_stream);
    info!(
        "tool loop: starting (max_rounds={max_rounds}, \
         max_auto_continues={max_auto_continues})"
    );
    loop {
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
            // Diagnostic snapshot of the round's outcome. Helps
            // operators tell apart "model is actively reasoning and
            // hits the token cap" (large `reasoning_chars`, empty
            // `assistant_text_len`, `finish_reason=length`) from
            // "model finished cleanly" (`finish_reason=stop`,
            // visible text) and from "model produced some visible
            // text and ran out of tokens mid-sentence" (non-zero
            // `assistant_text_len`, `finish_reason=length`). The
            // first pattern is the auto-continue trigger; the others
            // are clean exits.
            let assistant_text_len = outcome.assistant_text.chars().count();
            let sample: String = outcome.assistant_text.chars().take(200).collect();
            let fr = outcome.finish_reason.as_deref().unwrap_or("none");
            info!(
                tool_round,
                auto_continue_count,
                finish_reason = fr,
                assistant_text_len,
                reasoning_chars = outcome.reasoning_chars,
                "tool loop: round outcome (no tool_calls)"
            );
            if !sample.is_empty() {
                tracing::debug!(
                    tool_round,
                    auto_continue_count,
                    assistant_text_sample = %sample,
                    "tool loop: round outcome assistant_text sample"
                );
            }
            // Reasoning-only truncation auto-continue. Reasoning
            // models (qwen3.5 with reasoning on, DeepSeek-R1,
            // o1/o3, …) stream `delta.reasoning` first and can
            // hit the upstream's per-request token cap before
            // emitting any `delta.content`. The upstream surfaces
            // this as `finish_reason: "length"` with an empty
            // `delta.content`. Without intervention, the user
            // would see an empty assistant turn despite the
            // model having thought for thousands of tokens. We
            // detect the pattern
            //   `finish_reason == "length"`
            //       && `assistant_text.is_empty()`
            // and append a "please continue" user message to the
            // conversation; the new round asks the model to
            // finish the answer without re-emitting the prior
            // reasoning (already streamed verbatim to the
            // client). The upstream's per-request cap is the
            // real bound on how much the model can produce in
            // one round — see `docs/llm-configuration.md` for
            // how to bump `num_ctx` (Ollama) or `-c` (llama-
            // server) so this fallback rarely fires in practice.
            let is_reasoning_truncation = outcome.finish_reason.as_deref() == Some("length")
                && outcome.assistant_text.is_empty();
            if is_reasoning_truncation && max_auto_continues > 0 {
                if auto_continue_count >= max_auto_continues {
                    // We tried to auto-continue up to the cap and the
                    // model still came back with reasoning-only
                    // truncation. The reasoning text already
                    // streamed to the client stays visible in the
                    // chat UI's `<details>` block; surface a clear
                    // SSE `error` event so the chat UI can render a
                    // friendly bubble instead of leaving the user
                    // waiting on an empty assistant turn.
                    warn!(
                        auto_continue_count,
                        max_auto_continues,
                        reasoning_chars = outcome.reasoning_chars,
                        "tool loop: max auto-continues reached on reasoning truncation; \
                         surfacing error to client"
                    );
                    let _ = tx
                        .send(Ok(Bytes::from(sse_error_event(
                            "thinking truncated",
                            &format!(
                                "the model kept reasoning after {max_auto_continues} \
                                 auto-continues without producing an answer; \
                                 try a shorter question or a different model"
                            ),
                        ))))
                        .await;
                    let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
                    return;
                }
                auto_continue_count += 1;
                info!(
                    auto_continue_count,
                    max_auto_continues,
                    reasoning_chars = outcome.reasoning_chars,
                    "tool loop: length-truncated with reasoning only; auto-continuing",
                );
                // Append the partial assistant turn + a "continue"
                // user message. We deliberately do NOT preserve the
                // reasoning text in the assistant message — most
                // models repeat it verbatim if we do, doubling the
                // token cost of the next round. The reasoning stays
                // visible to the user via the `<details>` block in
                // the chat UI; the LLM does not need it back.
                let assistant_partial = json!({
                    "role": "assistant",
                    "content": "",
                });
                let continue_user = json!({
                    "role": "user",
                    "content":
                        "Your previous response was cut off while you were still \
                         reasoning (no answer reached the user). Please continue \
                         from where you left off and produce the visible answer now. \
                         Do not repeat the reasoning you already did; just finish the \
                         response."
                });
                if let Some(messages) = body
                    .as_object_mut()
                    .and_then(|o| o.get_mut("messages"))
                    .and_then(|m| m.as_array_mut())
                {
                    messages.push(assistant_partial);
                    messages.push(continue_user);
                }
                // Force the next round to open a fresh upstream
                // connection (the current_stream slot is None
                // after the `take()` at the top of the loop body).
                current_stream = None;
                continue;
            }
            // Clean completion (`finish_reason: "stop"`) or
            // partial content + truncation (the model produced
            // visible text but the upstream ran out of tokens
            // mid-sentence — the user gets what they got).
            info!(
                tool_round,
                auto_continue_count,
                finish_reason = outcome.finish_reason.as_deref().unwrap_or("none"),
                assistant_text_len,
                reasoning_chars = outcome.reasoning_chars,
                "tool loop: conversation complete (no tool_calls)"
            );
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }
        // Tool calls: this round counts against `max_rounds`.
        tool_round += 1;
        if tool_round > max_rounds {
            let _ = tx
                .send(Ok(Bytes::from(sse_error_event(
                    "agent loop exceeded",
                    &format!("max tool rounds ({max_rounds}) reached"),
                ))))
                .await;
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }
        info!(
            tool_round,
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
                    tool_round,
                ))))
                .await;

            let args_value: Value = serde_json::from_str(&tc.arguments).unwrap_or(Value::Null);

            // Per-tool-round `UserContext`. Carries the
            // authenticated `user_id` (SEV 2 fix) AND the per-user
            // credential `resolver` so agents like
            // `caldav_list_events` / `x_timeline` that read
            // `ctx.secret(...)` actually hit the vault. Without
            // the resolver, every per-user secret lookup would
            // short-circuit to `CredentialsMissing` regardless of
            // whether the user has configured the integration.
            // `resolver` is `None` on the `auth.enabled = false`
            // trust boundary; in that mode per-user agents surface
            // a clear tool error instead of panicking.
            //
            // When `chat_session_id` is set (browser sent
            // `X-Chat-Session-Id`), use `for_chat_session` so
            // agents like `read_document` can scope their queries
            // to the right session. Otherwise fall back to
            // `for_tests` and let the session-scoped agents
            // surface a clear tool error.
            //
            // The `memory_source` (plan 1791267136806, §7.6) is
            // attached via `for_chat_session_with_memories` whenever
            // the operator compiled the `memory-agent` feature AND
            // `LLM_ALLOW_USER_MEMORY=true`. When the source is
            // absent, the four `memory_*` agents fail closed with
            // `AgentError::AgentFailed("memory: source not wired
            // in this context")` — the chat UI shows a clear
            // "memory subsystem disabled" error rather than a
            // confusing 500.
            let services = nagent_agents::ServiceRegistry::empty().into_arc();
            let resolver = resolver.clone();
            let memory_source = memory_source.clone();
            let mut ctx = match (chat_session_id, memory_source.clone()) {
                (Some(sid), Some(mem)) => UserContext::for_chat_session_with_memories(
                    user_id, services, resolver, None, mem, sid,
                ),
                (Some(sid), None) => {
                    UserContext::for_chat_session(user_id, services, resolver, None, sid)
                }
                (None, _) => UserContext::for_tests(user_id, services),
            };
            // Plan 4.C: record the invocation so the next tool
            // call in this turn can ask the agent's
            // `requires_confirmation` impl whether to gate (e.g.
            // `web_fetch` after `read_document`).
            ctx.record_invocation(&name);

            // Session-wide override short-circuit: if the user
            // clicked "Toujours pour cette session" earlier in the
            // session for this tool, skip `requires_confirmation`
            // entirely. The agent's `requires_confirmation` impl is
            // untouched — the bypass happens at this call site only,
            // which keeps the trait contract clean and the
            // direct-invoke route (`/v1/agents/:name/invoke`) honest.
            let session_override = chat_session_id
                .map(|sid| permission_store.has_override(sid, &name))
                .unwrap_or(false);
            let force_allow = session_override;

            if let Some(agent) = agents.get(&name) {
                if !force_allow {
                    let decision = agent.requires_confirmation(&ctx, &args_value);
                    if let nagent_agents::ConfirmationDecision::NeedsConfirmation { reason } =
                        decision
                    {
                        warn!(
                            agent = %name,
                            id = %tc.id,
                            "tool loop: refusing {} without confirmation",
                            name
                        );
                        // Record the pending entry so the
                        // [APPROVE:…] / [DENY:…] sentinel on the
                        // next user turn can resolve it; emit the
                        // enriched SSE frame so the chat UI
                        // renders the inline approval card.
                        let prompt = approval_prompt(&name, &args_value);
                        if let Some(sid) = chat_session_id {
                            permission_store.set_pending(
                                sid,
                                tc.id.clone(),
                                name.clone(),
                                args_value.clone(),
                            );
                        }
                        let _ = tx
                            .send(Ok(Bytes::from(sse_tool_result_needs_approval(
                                &tc.id,
                                &name,
                                &reason,
                                approval_prompt_json(&prompt),
                            ))))
                            .await;
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tc.id,
                            "content": reason,
                        }));
                        continue;
                    }
                }
                let result = match agent.invoke(&ctx, args_value).await {
                    Ok(s) => Ok(s),
                    Err(e) => Err(e.to_string()),
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
            } else {
                let payload = format!("unknown agent: `{name}`");
                let _ = tx
                    .send(Ok(Bytes::from(sse_tool_result_event(
                        &tc.id, &name, false, &payload,
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
}

// ---------------------------------------------------------------------------
// The "needs confirmation" logic + the allowlist host matcher used
// to live here as private helpers; both moved into
// `nagent-agents::agents::web_fetch::WebFetchAgent::requires_confirmation`
// during plan 4.C so the tool loop is generic and does not need to
// know any agent by name. The unit tests for those helpers are now
// colocated with the agent impl (see `crates/nagent-agents/src/agents/
// web_fetch.rs`).
