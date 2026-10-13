# Plan: User-driven tool permission (no LLM-mediated bypass)

## Goal

Make the user the only authority for executing gated tools. The LLM must never
be able to run a gated tool without an explicit user click (or an explicit
"always for this session" opt-in already recorded for that tool). The current
`was_invoked` self-approval shortcut is the exact vector the user wants
closed.

Scope is the in-app chat permission flow. Claude-Code/Cursor style: every
gated call needs a click, the user can opt to skip clicks for a tool for the
rest of the session.

## Design decisions (resolved with user)

- **Per-call strict.** Every gated call requires one user click. No
  "first-call refuses, second-call allows" trick.
- **Session override button kept.** "✓ Toujours pour cette session" remains
  the only click-skip path. `email_send` still ignores the session override
  (always asks).
- **Round halts on first gated call.** When a gated call is refused, the
  tool loop breaks, the SSE stream is finalized with the `needs_approval`
  frame, and the LLM does not get a continuation round. Other tool calls
  the LLM issued in the same round are dropped silently (the LLM sees only
  the gated refusal).
- **Explicit refusal message to LLM.** The synthetic `role: "tool"`
  message tells the LLM to wait, and not to re-call the tool or call any
  other tool. The LLM resumes only when the user clicks Approve.
- **`was_invoked` is dead.** The per-turn "called already" tracking is no
  longer consulted by any `requires_confirmation` override. We keep the
  trait method on `UserContext` for one release as no-op + remove on the
  next cleanup, or remove outright if no telemetry needs it.

Out of scope (call out, do not do here):

- DB persistence of session overrides / per-user tool policy
- New "approve for this turn" / "approve for this task" buttons
- Removing the inline approval card (it stays, it is the only UI surface)
- `untrusted_output` indirection (still defined, still not wired — separate
  work)

## Data flow after the change

```
browser  POST /v1/chat/completions (stream)
   │
   ▼
proxy::chat_completions
   ├── run_permission_intercept            (sentinel: [APPROVE|DENY|…])
   │      - strips sentinel from user msg
   │      - runs the tool directly, emits tool_call+tool_result SSE
   │      - appends synthetic assistant+tool round to body
   │
   ├── upstream POST to Ollama  (round R)
   │
   ▼
run_tool_loop
   for each tool_call in round R:
       force_allow = session_overrides.contains(tool_name)         # email_send ignores this
       if !force_allow:
           match agent.requires_confirmation(ctx, args) {
               Allow              → run invoke()
               NeedsConfirmation  → break_round_with_approval_card()
           }
       if break → emit final SSE event "round_paused", return from loop,
                  do NOT call Ollama again, do NOT execute other tool_calls
   else (no gated):
       call Ollama round R+1
       (existing flow)

browser  receives tool_call / tool_result (needs_approval) / round_paused
   - appendToolBubble + resolveToolBubble → renderApprovalCard
   - SSE stream closes
   - LLM is idle, awaiting user click

user clicks "Autoriser"
   - chat.js sends "[APPROVE:<tool_call_id>]" via the normal send path
   - next user turn re-enters proxy::chat_completions
   - run_permission_intercept runs the tool, emits frames, ends the round
   - LLM sees synthetic assistant(tool_calls) + tool(result) on round R+1
   - LLM continues from there
```

## Files to change

### 1. `crates/nagent-server/src/llm/tool_loop.rs`

- Inside the per-tool-call gate, when `decision == NeedsConfirmation`:
  - Replace the current `continue;` (which moves to the next tool_call)
    with a `break` that exits the tool_call loop and a flag that
    short-circuits the post-loop "call Ollama again" step.
  - Stop emitting further SSE frames after the `needs_approval` frame;
    finalize the stream (the existing axum SSE sink already handles
    stream-end on function return).
  - Update the synthetic `role: "tool"` message content. See template
    below.
- Do **not** call `ctx.record_invocation(name)` on the refused path. The
  invocation counter is no longer load-bearing. Keep the call on the
  `Allow` path if any other code path depends on it (search first;
  remove if not).
- Add a constant or helper for the new refusal message template.

Refusal message template (sent to the LLM in the synthetic tool message):

```
Tool "{name}" requires explicit user approval before it can run. The
current turn is paused. Do NOT re-call this tool and do NOT call any
other tool. Wait for the user to click "Approve" in the chat UI; the
tool will execute on their next message and you will see its result
then. Reason from the user: {reason}
```

### 2. `crates/nagent-agents/src/agents.rs`

- Audit every override of `requires_confirmation` for `was_invoked`
  short-circuits. Confirmed overrides in the registry today:
  - `crates/nagent-agents/src/agents/caldav/create_event.rs:135`
  - `crates/nagent-agents/src/agents/caldav/list_events.rs:157`
  - `crates/nagent-agents/src/agents/caldav/get_event.rs:118`
  - `crates/nagent-agents/src/agents/email/list_inbox.rs:98`
  - `crates/nagent-agents/src/agents/email/get_message.rs:78`
  - `crates/nagent-agents/src/agents/email/search.rs:86`
  - `crates/nagent-agents/src/agents/email/mark_read.rs:82`
  - `crates/nagent-agents/src/agents/memory.rs:472` (memory_forget)
- For every override that returns `Allow` when `ctx.was_invoked(name)`:
  drop that branch entirely. The default becomes "always NeedsConfirmation
  on the first call" which is now the only call we ever let through
  before user action.
- Keep the `was_invoked` and `record_invocation` methods on `UserContext`
  for this commit (no other consumer today, but the trait is public) and
  add a doc-comment "deprecated, no longer consulted by
  requires_confirmation". Removal is a follow-up cleanup.
- `email_send` keeps its hard-coded "always NeedsConfirmation"
  behavior; the `force_allow` bypass on the server side must continue
  to ignore `email_send` (already documented in `send.rs:113–121`,
  preserve as-is).

### 3. `crates/nagent-server/src/llm/permission.rs`

- No new permission logic. The existing `parse_decision_prefix`,
  `PermissionStore`, `set_pending`, and `run_permission_intercept` paths
  are reused unchanged:
  - The sentinel protocol (`[APPROVE:id]`, `[APPROVE_ALWAYS:tool]`,
    `[DENY:id]`) is the user-driven trigger.
  - The `session_overrides` set is the explicit "always for this
    session" opt-in.
- Optional: add a unit test that `run_permission_intercept` re-runs the
  tool after a stale `pending` entry's 5-min TTL has expired (defensive;
  not the focus of this change).

### 4. `crates/nagent-server/src/llm/sse.rs`

- `sse_tool_result_needs_approval` (`sse.rs:347`) keeps its shape; this
  is what the client card binds to.
- Optional new event `sse_round_paused` (id, tool_name, reason) emitted
  exactly once at the end of the paused round, after the
  `needs_approval` frame. The client can use it to:
  - Show a "⏸ Waiting for your approval" footer on the last user
    message,
  - Disable the input textarea until the user decides (re-enabled by
    the next user-message send).
  If we keep the SSE stream ending right after the
  `needs_approval` frame, the client already detects pause from
  "stream ended without `done`" — pick one source of truth, not both.
  Recommendation: add `sse_round_paused`, drop the input-disable logic
  for the legacy "stream ended" path.

### 5. `crates/nagent-server/src/static/chat.js`

- `appendToolBubble` (`chat.js:2282`) and `resolveToolBubble`
  (`chat.js:2421`) need no functional change — the approval card is
  already rendered when `needs_approval === true`.
- On receiving the new `round_paused` event:
  - Add a class `chat-message--paused` to the latest user message
    bubble (visual: dim + small "⏸ Waiting for approval" pill).
  - Disable the chat input textarea + send button until the user sends
    a message. The sentinel sentinels are sent through the existing
    `sendTyped` path, so the "user just typed" detection re-enables the
    input naturally.
- Rehydration on reload: history rows with `needs_approval: true` should
  restore the paused state for the most recent such row, so a refresh
  during a paused round still shows the card. The existing
  `chat.js:1591, 1588` logic already re-renders the card on reload;
  extend it to also re-apply `chat-message--paused` on the preceding
  user bubble.
- No new copy. The three approval buttons keep their current French
  labels: "✓ Autoriser", "✓ Toujours pour cette session", "✕ Refuser".

### 6. `crates/nagent-server/src/static/style.css`

- Add a `.chat-message--paused` modifier on the user bubble: subtle
  dashed left border + "⏸ Waiting for approval" pill (use the same
  `--danger-low` accent defined at `style.css:1693`).
- Add a `:disabled` style for the chat textarea + send button while
  `data-paused="true"` is set on the chat root.
- No changes to the approval-card CSS block.

### 7. `crates/nagent-server/src/llm/proxy.rs`

- `build_tools_for_round` (`proxy.rs:201`) and
  `rebuild_tools_for_next_round` (`tool_loop.rs:634`) are unchanged.
  Tools are still declared via the `tools=[…]` field to Ollama.
- `run_permission_intercept` (`permission.rs:675`) is unchanged.

### 8. `docs/architecture.md`

- Add a one-paragraph note under the tools section explaining the
  current permission contract: per-call strict, LLM round halts on
  refusal, session override is the only click-skip, `email_send` is
  pinned to always-ask. This is the "documentation sync" surface for
  the behavior change required by AGENTS.md §5.

## Tests

Add (in this order, smallest first):

1. **Unit test, `crates/nagent-agents/src/agents.rs`** (or a dedicated
   test module): a stub `Agent` whose `requires_confirmation` is the
   "two-step" pattern used today. Assert that removing the
   `was_invoked` branch makes the second call in the same
   `UserContext` also return `NeedsConfirmation`. This locks in the
   bypass-removed invariant at the trait level.

2. **Unit test, `crates/nagent-server/src/llm/tool_loop.rs`** (or a
   dedicated loop harness in `crates/nagent-server/src/testing.rs`):
   feed a round with `[call_gated_a, call_gated_b, call_idempotent_c]`
   to the tool loop. Assert:
   - `call_gated_a` produces a `needs_approval` SSE frame and the
     synthetic wait message is added to the messages,
   - `call_gated_b` and `call_idempotent_c` are NOT executed (no
     `tool_call` / `tool_result` frames for them),
   - The loop returns without scheduling a follow-up Ollama call,
   - The `PermissionStore::pending` contains exactly one entry for
     `call_gated_a`.

3. **Integration test (existing harness in
   `crates/nagent-server/src/testing.rs:390`):** scripted Ollama stub
   that emits two tool calls in one round, the first gated, the second
   not. Assert end-to-end:
   - Client receives the `needs_approval` frame, the stream closes,
   - Second tool call never runs,
   - The persisted history has exactly one `role: "tool"` row
     containing the explicit wait message,
   - On the next user message containing `[APPROVE:<id>]`, the tool
     runs, a `tool_call` + `tool_result` pair is emitted, and the LLM
     stub sees the synthetic `assistant(tool_calls)` + `tool` round.

4. **Integration test:** session override path. User sends
   `[APPROVE_ALWAYS:caldav_list_events]`. Subsequent calls to
   `caldav_list_events` in the same session run without an approval
   card. `email_send` is asserted to STILL require approval after
   `[APPROVE_ALWAYS:email_send]` (regression guard for the
   always-ask pin).

5. **UI test (manual smoke checklist, no headless browser here):**
   - Cold load, send "list my events" → inline approval card appears,
     input is dimmed, "⏸ Waiting for approval" pill shows on user
     bubble.
   - Click "Autoriser" → card resolves, next LLM turn produces a real
     result, the pill disappears.
   - Click "Toujours pour cette session" → next call runs without a
     card, "Toujours" pill stays for the rest of the session.
   - Reload mid-pause → approval card + pill reappear on the most
     recent gated row.
   - For an `email_send` call after a previous
     `[APPROVE_ALWAYS:email_send]`, the card still appears.

## Validation steps

After implementation:

- `cargo fmt --all`
- `cargo build --workspace --all-targets` (zero warnings)
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p nagent-agents`
- `cargo test -p nagent-server` (covers the integration tests in
  `testing.rs`)
- `cargo audit`
- Manual smoke checklist (item 5 above) with `make run-mock`

## Open questions (left for follow-up, not blocking)

- Do we want `[APPROVE_ALWAYS:<tool>:<host-or-pattern>]` to scope
  session overrides by argument (e.g. allow `web_fetch` only for
  `example.com`)? The current override key is just the tool name.
- Do we want a per-user "deny list" stored in DB so a user can
  permanently block a tool they never use, and so a server restart
  preserves session overrides? Not in scope here; that requires the
  `auth_db_abstraction_required` decision in
  `project.md_Decisions_auth_db_abstraction_required` to land first.
- Do we want a "deny" path that records the denial so the LLM cannot
  re-attempt the same call for the rest of the turn? Today `[DENY:id]`
  just emits a refusal `tool_result` and lets the LLM continue. Not in
  scope here; worth a follow-up.

## Rollout

- No DB migration, no feature flag, no CLI option. Behavior change is
  effective on next server start.
- This is a security-relevant UX change. Note in the commit body that
  gated tool calls now require a user click even within the same LLM
  turn (previously a same-turn re-call could run without one).
- The change is not user-visible for users who already click the
  approval card on every gated call. It is visible for users who
  relied on the LLM re-calling the tool after a refusal — that
  shortcut is gone.
