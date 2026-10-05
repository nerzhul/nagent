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
///
/// `max_auto_continues` is the maximum number of auto-continue
/// rounds appended when the upstream ends with `finish_reason:
/// "length"` and only reasoning (no visible answer) was emitted
/// before the cap. Counted on its own counter so the
/// auto-continue path does **not** eat into `max_rounds`: the
/// latter gates tool-call rounds, the former gates
/// reasoning-truncation rounds, and they can run side by side.
/// The default (5) leaves headroom for long reasoning chains
/// (DeepSeek-R1, Qwen3.5 with thinking, o1/o3) that may truncate
/// 2-3 times before producing the visible answer; raising the
/// cap further only protects against truly runaway models.
///
/// `max_thinking_chars` is the cumulative cap on
/// `delta.reasoning` characters across every auto-continue
/// round of one user turn. Pairs with `max_auto_continues` so a
/// runaway model cannot burn the upstream's token budget: the
/// count cap guards against unbounded rounds, the chars cap
/// guards against unbounded per-round reasoning length. When
/// the chars cap is hit, the loop emits an SSE `error` event
/// and exits cleanly so the chat UI can show a friendly bubble
/// instead of leaving the user waiting on an empty assistant
/// turn. The reasoning text already streamed to the client
/// stays visible in the chat UI's `<details>` block.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tool_loop(
    http: reqwest::Client,
    headers: reqwest::header::HeaderMap,
    url: String,
    initial_body: Value,
    agents: Option<AgentRegistry>,
    max_rounds: u32,
    max_auto_continues: u32,
    max_thinking_chars: u32,
    idle_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    first_stream: UpstreamByteStream,
    chat_session_id: Option<uuid::Uuid>,
    user_id: uuid::Uuid,
) {
    let agents = agents.unwrap_or_else(AgentRegistry::empty);
    let mut body = initial_body;
    // `tool_round` counts only rounds that produced at least one
    // tool call. Auto-continues (reasoning truncations that
    // append a "please continue" user prompt) live on their own
    // counter so a model that needs several reasoning rounds to
    // finish a long thought does not exhaust the tool-call
    // budget. See the comment on `max_auto_continues` above.
    let mut tool_round: u32 = 0;
    let mut auto_continue_count: u32 = 0;
    // Cumulative `delta.reasoning` characters emitted across
    // every round of the current user turn. Tracked so a model
    // that keeps emitting reasoning but never produces an answer
    // cannot burn the upstream's token budget indefinitely. The
    // chat UI streams the reasoning text to the user via the
    // `<details>` block as soon as it lands, so when the cap is
    // hit the user already sees what the model was thinking.
    let mut total_thinking_chars: u64 = 0;
    let mut current_stream: Option<UpstreamByteStream> = Some(first_stream);
    info!(
        "tool loop: starting (max_rounds={max_rounds}, \
         max_auto_continues={max_auto_continues}, \
         max_thinking_chars={max_thinking_chars})"
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
            // Reasoning-only truncation (plan R8 follow-up):
            // reasoning-capable models (`qwen3.5` with reasoning
            // on, DeepSeek-R1, …) stream `delta.reasoning` first
            // and may hit the upstream's token cap before ever emitting
            // a `delta.content` answer, leaving the user staring at
            // an empty bubble. We detect the pattern
            //   `finish_reason == "length"`
            //       && assistant_text.is_empty()
            //       && !reasoning_text.is_empty()
            // and append a follow-up round that asks the model to
            // continue. The continuation round runs without
            // re-emitting the reasoning (most models repeat it
            // verbatim if asked), so the user only sees the
            // `delta.content` they were waiting for. Two budgets
            // gate this path:
            //   - `max_auto_continues` on the count of continues
            //     already appended (default 5, plenty for o1 / R1
            //     chains);
            //   - `max_thinking_chars` on the cumulative
            //     `delta.reasoning` chars across every round of
            //     the user turn (default 32 KB, the real "how
            //     much is the model allowed to think" knob).
            // When either cap is hit, the loop surfaces a clear
            // SSE `error` event naming the budget so the chat UI
            // can render a friendly error bubble. The reasoning
            // already streamed to the client stays visible in the
            // chat UI's `<details>` block.
            let is_reasoning_truncation = outcome.finish_reason.as_deref() == Some("length")
                && outcome.assistant_text.is_empty()
                && !outcome.reasoning_text.is_empty();
            if is_reasoning_truncation {
                let round_chars = outcome.reasoning_text.chars().count() as u64;
                total_thinking_chars = total_thinking_chars.saturating_add(round_chars);
                let over_count_cap = auto_continue_count >= max_auto_continues;
                let over_chars_cap =
                    max_thinking_chars > 0 && total_thinking_chars > max_thinking_chars as u64;
                if over_count_cap || over_chars_cap {
                    let reason = if over_chars_cap {
                        format!(
                            "the model spent {total_thinking_chars} chars of reasoning \
                             (limit: {max_thinking_chars}) without producing an answer; \
                             try a shorter question or a different model"
                        )
                    } else {
                        format!(
                            "the model kept reasoning after {max_auto_continues} \
                             auto-continues ({total_thinking_chars} chars total) \
                             without producing an answer; try a shorter question \
                             or a different model"
                        )
                    };
                    warn!(
                        auto_continue_count,
                        total_thinking_chars,
                        max_auto_continues,
                        max_thinking_chars,
                        "tool loop: reasoning budget exhausted; surfacing error to client"
                    );
                    let _ = tx
                        .send(Ok(Bytes::from(sse_error_event(
                            "thinking truncated",
                            &reason,
                        ))))
                        .await;
                    let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
                    return;
                }
                auto_continue_count += 1;
                info!(
                    auto_continue_count,
                    max_auto_continues,
                    round_chars,
                    total_thinking_chars,
                    max_thinking_chars,
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
                // connection (the current_stream slot is None after
                // the `take()` at the top of the loop body).
                current_stream = None;
                continue;
            }
            // No tool calls, no reasoning-truncation → the
            // conversation is done. Always emit a single
            // `data: [DONE]` here because `drain_upstream_round`
            // swallows any upstream `[DONE]` (forwarding it would let
            // the browser cut the response mid-loop and drop our
            // subsequent `event: tool_call` / `event: tool_result`
            // frames — see the regression in `tests/agents.rs`). The
            // sentinel we send is the only one the client ever sees.
            info!(
                tool_round,
                auto_continue_count,
                total_thinking_chars,
                "tool loop: conversation complete (no tool_calls)"
            );
            let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
            return;
        }
        // Tool calls: this round counts against `max_rounds`.
        // Auto-continues never reach this branch (the empty-tool-
        // calls path returns or `continue`s above), so the two
        // budgets stay independent as documented on
        // `max_auto_continues`.
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
            // Security plan #10 (now generic via
            // `Agent::requires_confirmation`, plan 4.C). The tool
            // loop never knows a specific agent by name — the rule
            // lives in the agent's `requires_confirmation` impl,
            // which gets to inspect `ctx.invoked_this_turn` to
            // enforce cross-agent invariants (e.g. `web_fetch` after
            // `read_document`). When the predicate fires, we
            // replace the tool result with the agent's own reason
            // so the model reformulates ("please confirm") and the
            // user sees the request in chat. The next turn
            // restarts with `ctx.invoked_this_turn` empty so a
            // user-confirmed URL fetches normally.
            if let Some(decision) =
                agents
                    .get(&name)
                    .and_then(|agent| -> Option<nagent_agents::ConfirmationDecision> {
                        let services = nagent_agents::ServiceRegistry::empty().into_arc();
                        let ctx = match chat_session_id {
                            Some(sid) => {
                                UserContext::for_chat_session(user_id, services, None, None, sid)
                            }
                            None => UserContext::for_tests(user_id, services),
                        };
                        Some(agent.requires_confirmation(&ctx, &args_value))
                    })
            {
                if let nagent_agents::ConfirmationDecision::NeedsConfirmation { reason } = decision
                {
                    warn!(
                        agent = %name,
                        id = %tc.id,
                        "tool loop: refusing {} without confirmation",
                        name
                    );
                    let _ = tx
                        .send(Ok(Bytes::from(sse_tool_result_event(
                            &tc.id,
                            &name,
                            false,
                            #[allow(clippy::needless_borrow)]
                            &reason,
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
                    let services = nagent_agents::ServiceRegistry::empty().into_arc();
                    let mut ctx = match chat_session_id {
                        Some(sid) => {
                            UserContext::for_chat_session(user_id, services, None, None, sid)
                        }
                        None => UserContext::for_tests(user_id, services),
                    };
                    // Plan 4.C: record the invocation so the next
                    // tool call in this turn can ask the agent's
                    // `requires_confirmation` impl whether to gate
                    // (e.g. `web_fetch` after `read_document`).
                    ctx.record_invocation(&name);
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
