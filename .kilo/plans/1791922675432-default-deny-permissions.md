# Plan: Default-deny tool permissions + llama.cpp-style approval modal

## Goal

Flip the chat-permission posture from **opt-in gating** to **default-deny on every
registered agent**, and replace the inline approval card with a real
`<dialog>`-based modal — the same affordance llama.cpp's server UI surfaces when
a model asks to call a tool. The session-wide "Toujours pour cette session"
override stays. `email_send` keeps its always-ask pin. The `web_fetch`
cross-tool rule (`only gates after read_document ran`) is removed; `web_fetch`
is gated on every call.

Reference: the previous permission rework lives at
`.kilo/plans/1791834996882-user-driven-tool-permission.md` and the current
implementation is locked behind commit `253b6ba` ("make user the only
authority for gated tool calls"). This plan extends that posture from
"every gated call" to "every call".

## Design decisions (resolved with user)

- **Default-deny on every agent.** The trait default of
  `ConfirmationDecision::Allow` is flipped to `NeedsConfirmation`. Every
  agent that does not explicitly opt out now requires a user click.
- **Session override preserved.** "✓ Toujours pour cette session" stays.
  `email_send` still opts out of the override.
- **Modal dialog, llama.cpp style.** Replace `.chat-tool-approval` inline
  card with a top-level `<dialog>` opened on every `needs_approval` event.
  ESC and backdrop click = Deny. Focus trap is the native dialog
  behaviour. Three buttons ("Autoriser" / "Toujours pour cette session"
  / "Refuser") keep the existing French labels and sentinel routing.
- **`web_fetch` cross-tool rule removed.** Always gated, regardless of
  whether `read_document` ran in the same turn. The `was_invoked` check
  inside `web_fetch::requires_confirmation` is gone; `UserContext::was_invoked`
  stays public (still used by any future cross-tool rule) but
  `UserContext::record_invocation` is only called on the dispatch path
  (already the case after plan 1791834996882 — no further move needed).
- **Per-tool `approval_prompt` filled in for every agent.** The generic
  fallback ("Autoriser cet outil ?") is reserved for genuinely unknown
  names; every registered agent now has a curated title + body so the
  modal explains *what* and *why*.
- **`requires_confirmation_by_default` field renamed.** Becomes
  `requires_confirmation` (since "by default" is now always true); the
  `GET /v1/agents` summary keeps the same wire field name for
  compatibility with the existing `chat-agents-banner` UI but the Rust
  field is renamed to match the trait.

## Data flow after the change

```
browser  POST /v1/chat/completions (stream)
   │
   ▼
proxy::chat_completions
   ├── run_permission_intercept            (sentinel: [APPROVE|DENY|…])
   │      - unchanged from plan 1791834996882
   │
   ├── upstream POST to Ollama  (round R)
   │
   ▼
run_tool_loop
   for each tool_call in round R:
       force_allow = session_overrides.contains(tool_name) AND agent.allow_session_override()
       if !force_allow:
           match agent.requires_confirmation(ctx, args) {
               // Default is now NeedsConfirmation. Allow is reserved
               // for future operator overrides (e.g. an "allow safe tools
               // without confirmation" config flag — out of scope here).
               Allow              → run invoke()
               NeedsConfirmation  → break_round_with_approval_card()
           }
       if break → emit final SSE event "round_paused", return from loop
   else (no gated):
       call Ollama round R+1

browser  receives tool_call / tool_result (needs_approval) / round_paused
   - chat.js opens <dialog id="chat-approval-modal"> with prompt info
   - The user bubble gets .chat-message--paused (visual only; no input
     disable because the modal blocks interaction)
   - SSE stream closes, LLM is idle
   - User clicks one of three buttons
   - chat.js sends [APPROVE:<id>] | [APPROVE_ALWAYS:<tool>] | [DENY:<id>]
   - Next user turn re-enters proxy::chat_completions, dispatch follows
```

## Files to change

### 1. `crates/nagent-agents/src/agents.rs`

- Flip the trait default:
  ```rust
  fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
      ConfirmationDecision::NeedsConfirmation {
          reason: "default-deny: every tool call requires explicit user approval".into(),
      }
  }
  ```
- Update the trait doc to state that the default is now gated, that
  agents only override to `Allow` when they want to opt out of the gate,
  and that the gate is the same regardless of how many times the tool
  was called in the turn (plan 1791834996882 invariant, still true).
- Rename `AgentSummary::requires_confirmation_by_default` →
  `requires_confirmation` (cleaner; the "by default" was meaningful
  when the default was `Allow`).
- Audit the existing per-agent unit test
  `requires_confirmation_is_strict_even_in_same_turn` in every
  `*_requires_confirmation` test — these still hold; add a new test on
  the trait level that an agent which does not override the method
  (e.g. a stub `NoOverrideAgent`) returns `NeedsConfirmation`.

### 2. Per-agent overrides

Walk every registered agent and act on its `requires_confirmation`
impl. The end state is the only place that documents which tools opt
out of the gate. Net changes:

- **`x_timeline`** (`crates/nagent-agents/src/agents/x_timeline/mod.rs:256`):
  remove the `Allow` override entirely. With the new trait default the
  tool is now gated; the comment ("Read-only tool: no extra user
  confirmation") is wrong on two counts (X is the user's account, and
  there is no "no extra confirmation" any more). Update the agent's
  top-level doc and the unit test file in the same module.
- **`web_fetch`** (`crates/nagent-agents/src/agents/web_fetch.rs:153`):
  drop the `if !ctx.was_invoked("read_document") { return Allow; }`
  branch. Keep the `host_matches_allowlist` check as a separate
  agent-level opt-out (the operator-configured allowlist is still
  honoured — `Allow` on allowlisted hosts is the documented behaviour).
  Update the unit tests that asserted the cross-tool rule.
- **`caldav_create_event`, `caldav_list_events`, `caldav_get_event`**,
  **`email_list_inbox`, `email_get_message`, `email_search`,
  `email_mark_read`, `email_send`**,
  **`memory_forget`**: no behaviour change. Their explicit
  `NeedsConfirmation` overrides stay (they document the per-tool
  reason). The `requires_confirmation_is_strict_even_in_same_turn`
  unit tests still hold.
- **`memory_store`, `memory_recall`, `memory_list`**: no current
  override. With the new default they are now gated automatically.
  Add per-agent `NeedsConfirmation` overrides with curated reasons
  ("`memory_store` adds a durable fact to your long-term memory",
  "`memory_recall` reads durable facts", "`memory_list` lists the
  stored facts") so the modal is informative. Add unit tests
  asserting the strict-same-turn invariant.
- **`tool_search`** (`crates/nagent-agents/src/agents/tool_search.rs:16`):
  no current override. With the new default `search_tools` is now
  gated. Add an override with the reason "the assistant wants to look
  up which tools are available for this question" and bump the
  unit test.
- **`read_document`, `get_weather`, `calculate`, `wikipedia`,
  `unit_convert_agent`, `datetime_agent`, `dictionary_agent`,
  `stock_agent`**: no current override. Gated automatically by the new
  default. No code change beyond the trait flip.

### 3. `crates/nagent-server/src/llm/permission.rs`

- Extend the `approval_prompt` match with a curated entry for every
  registered agent. The fallback at `_ =>` becomes a true fallback
  reserved for unknown agent names. Concrete entries to add (each with
  a `title`, `body`, and a sensible `Danger` level):
  - `email_list_inbox`, `email_get_message`, `email_search`,
    `email_send`, `email_mark_read` (all `Danger::High` for `email_send`,
    `Danger::Medium` for the rest).
  - `memory_store`, `memory_recall`, `memory_list`, `memory_forget`
    (all `Danger::Medium`).
  - `x_timeline` (`Danger::Medium`; body explains the user's X account
    is accessed).
  - `read_document` (`Danger::Low`; "the assistant wants to read your
    uploaded document X").
  - `get_weather`, `calculate`, `wikipedia`, `unit_convert`,
    `datetime`, `dictionary`, `stock` (all `Danger::Low` with a
    one-liner explaining the tool).
  - `search_tools` (`Danger::Low`; "the assistant wants to look up
    which tools fit this question").
  - `web_fetch` (entry already exists; body wording refresh so the
    reason matches the now-always-gated posture).
- Danger matrix documented inline in the match as a doc-comment so
  future agents pick the right level without grepping.

### 4. `crates/nagent-server/src/static/index.html`

- Add a single `<dialog id="chat-approval-modal" class="chat-approval-modal">`
  at the end of `<body>`. Structure:
  ```html
  <dialog id="chat-approval-modal" class="chat-approval-modal" aria-labelledby="chat-approval-modal-title">
    <form method="dialog" class="chat-approval-modal__form">
      <header class="chat-approval-modal__header">
        <span class="chat-approval-modal__danger" data-danger="low" aria-hidden="true"></span>
        <h2 id="chat-approval-modal-title" class="chat-approval-modal__title">…</h2>
        <button type="button" class="chat-approval-modal__close" aria-label="Refuser">✕</button>
      </header>
      <p class="chat-approval-modal__tool">
        <code class="chat-approval-modal__tool-name">…</code>
      </p>
      <p class="chat-approval-modal__body">…</p>
      <details class="chat-approval-modal__args">
        <summary>Arguments</summary>
        <pre class="chat-approval-modal__args-pre">…</pre>
      </details>
      <div class="chat-approval-modal__buttons">
        <button type="button" class="chat-approval-modal__button chat-approval-modal__button--allow"     data-decision="approve">✓ Autoriser</button>
        <button type="button" class="chat-approval-modal__button chat-approval-modal__button--always"    data-decision="approve-always">✓ Toujours pour cette session</button>
        <button type="button" class="chat-approval-modal__button chat-approval-modal__button--deny"      data-decision="deny">✕ Refuser</button>
      </div>
    </form>
  </dialog>
  ```
- The `<dialog>` element natively provides focus trap and ESC handling;
  the `close` button is a convenience for users who reach for it
  before ESC.

### 5. `crates/nagent-server/src/static/chat.js`

- **Remove** the `renderApprovalCard` function and the
  `.chat-tool-approval` DOM render path inside `resolveToolBubble`.
  The tool entry keeps its name / args / result preview but the
  inline card is gone.
- **Add** `openApprovalModal({ id, name, prompt, args })`:
  - Pulls the cached `<dialog>` and sets `title`, `body`, `tool-name`,
    `args-pre` text, and `data-danger` on the danger badge from the
    prompt payload.
  - Calls `dialog.showModal()`.
  - Wires the three button click handlers (and the close button) to
    `sendPermissionDecision(name, id, decision)`, the same helper
    used today. After dispatch, the modal closes
    (`dialog.close()`).
  - On ESC or backdrop click the dialog fires its own `cancel` event;
    intercept it, dispatch `[DENY:<id>]` and `dialog.close()` (default
    ESC behaviour is to close + fire `cancel`, so this is one handler).
  - Set `dataset.state = "pending"` on the button row before
    dispatching, then `dataset.state = "approved"` / `"denied"` on
    resolution to keep the muted/pending CSS working.
- **Hook** the new modal into the existing `needs_approval` tool
  result handler. Today that handler calls `renderApprovalCard`;
  replace it with `openApprovalModal`. Pass the same `prompt` payload
  the server already sends.
- **Rehydration**: `rehydratePausedState` already detects the most
  recent tool row with `needs_approval: true`. Extend it to call
  `openApprovalModal` with the persisted prompt + args so a page
  refresh mid-pause re-opens the modal. The `PermissionStore` is
  in-memory only, so a server restart also drops the pending
  entry — in that case the modal opens, the user clicks, the
  intercept path returns "no matching pending entry", and the
  chat gracefully falls back to a regular LLM turn. (The current
  behaviour; the test in `crates/nagent-server/src/llm/permission.rs`
  covers this on the server side.)
- **Keep** `applyRoundPaused` so the user bubble still shows the
  "⏸ Waiting for approval" pill — it is a useful visual indicator
  even with the modal blocking interaction. The chat textarea
  disabling is no longer needed (the modal already blocks
  interaction); drop the `data-paused` CSS hook from the input
  rules and the JS that sets it.

### 6. `crates/nagent-server/src/static/style.css`

- **Remove** the `.chat-tool-approval`, `.chat-tool-approval__*`,
  `.chat-tool-approval--danger-*` blocks. They are replaced by the
  modal.
- **Add** the `.chat-approval-modal` rules. Reference: native
  `<dialog>` provides the `::backdrop` pseudo-element. The CSS
  uses `--accent` for the danger-low border, `#d99a3c` for medium,
  `var(--error)` for high (same palette as the old inline card so
  the visual language is consistent).
- Layout: fixed centre modal, max-width 480px, padding 1.2rem,
  rounded corners 12px, drop shadow. Buttons stacked vertically
  on narrow viewports (`< 480px`) and horizontally above. The
  "Args" `<details>` is collapsed by default — most users will
  not need to inspect raw JSON.
- **Keep** `.chat-message--paused` and `.chat-message-paused-pill`
  blocks. They are still surfaced on the latest user bubble.
- **Remove** the `[data-paused="true"]` textarea/button disabled
  rules — the modal already prevents interaction.

### 7. `crates/nagent-server/src/llm/sse.rs`

- No wire change. `sse_tool_result_needs_approval` already carries
  the `prompt` payload the modal reads. `sse_round_paused` is
  unchanged.

### 8. `crates/nagent-server/src/llm/tool_loop.rs`

- The `force_allow` short-circuit is unchanged. The new default
  means every agent that does not override `allow_session_override() =
  false` honours the session override, which is the right default.
- The synthetic refusal message sent to the LLM on a gated call
  (`refusal_message_for_llm`) is updated to drop the "you can
  re-call this after the user decides" wording — the LLM is no
  longer in the loop until the user clicks. New wording:

  ```
  Tool "{name}" did not run: the user has not approved it yet.
  The current turn is paused. Do NOT re-call this tool and do
  NOT call any other tool — wait for the user's next message;
  the tool will run only if the user approves it then. Reason:
  {reason}
  ```

  Same shape as today, no longer a behavioural change.
- The `warn!(... "tool loop: refusing {} without confirmation")`
  log line stays — operators rely on it for the audit trail.

### 9. `crates/nagent-agents/src/agents/web_fetch.rs`

- Drop the `if !ctx.was_invoked("read_document") { return Allow; }`
  branch. Keep the `host_matches_allowlist(host, &self.cfg.allowlist)`
  branch (allowlisted hosts remain operator-trusted and run without a
  card — this is documented in the existing `web_fetch` config docs).
- Update the unit tests in the same module that asserted the
  cross-tool rule.

### 10. `docs/architecture.md`

- Update the "Tool call approval" paragraph to:
  - State the default-deny posture.
  - Reference the new modal in the chat UI section.
  - Note that `web_fetch`'s cross-tool rule is gone and the
    allowlist is the only host-level bypass.
  - Keep the "`email_send` is pinned to always-ask" note.
  - Reference this plan file.

### 11. `README.md` (UI section, if it describes the approval card)

- Replace any "inline approval card" wording with "approval modal".
  The screenshots subfolder at `docs/screenshots/` may want a
  refresh; mark as optional in the validation checklist.

## Tests

Add (smallest first):

1. **Trait default flip** — `crates/nagent-agents/src/agents.rs`:
   add a stub `NoOverrideAgent` and assert
   `agent.requires_confirmation(&ctx, &Value::Null)` returns
   `NeedsConfirmation`. Locks in the default-deny invariant at the
   trait level so a future agent added to the registry is forced to
   think about whether to opt out.

2. **`x_timeline` removal** — `crates/nagent-agents/src/agents/x_timeline`:
   update the unit test that asserted `Allow` to assert
   `NeedsConfirmation` (strict same-turn).

3. **`web_fetch` cross-tool rule removed** — `crates/nagent-agents/src/agents/web_fetch.rs`:
   replace the `was_invoked` test with a test that asserts gated
   behaviour regardless of prior turn history, and a test that
   allowlisted hosts still return `Allow`.

4. **`memory_store/recall/list` strict-per-call** — extend the
   existing `memory.rs` test pattern (`requires_confirmation_is_strict_even_in_same_turn`)
   to the three new agents.

5. **`tool_search` strict-per-call** — `crates/nagent-agents/src/agents/tool_search.rs`:
   add a `requires_confirmation_is_strict_even_in_same_turn` test.

6. **`approval_prompt` per-agent entries** —
   `crates/nagent-server/src/llm/permission.rs`: add a unit test for
   every new entry (`email_*`, `memory_*`, `x_timeline`, `read_document`,
   `get_weather`, `calculate`, `wikipedia`, `unit_convert`,
   `datetime`, `dictionary`, `stock`, `search_tools`) asserting
   `danger` level and that the body contains the tool name.

7. **Round-halt integration test** — extend the existing
   `crates/nagent-server/tests/round_halts.rs` with two scripted
   Ollama rounds that emit a `search_tools` call (no override ⇒
   gated) and assert the same end-to-end flow as the existing
   `caldav_create_event` case.

8. **Modal rehydration** — `crates/nagent-server/src/static/chat.js`:
   the JS unit test harness (if one exists) gets a small test that
   builds a history row with `needs_approval: true` and asserts
   `openApprovalModal` is called with the right payload. If there
   is no JS unit harness, fold this into the manual smoke
   checklist (manual-only).

## Validation steps

After implementation:

- `cargo fmt --all`
- `cargo build --workspace --all-targets` (zero warnings)
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p nagent-agents`
- `cargo test -p nagent-server` (covers the integration tests in
  `testing.rs` and the `round_halts.rs` cases)
- `cargo audit`
- Manual smoke checklist (`make run-mock`):
  1. Send a message that triggers `search_tools` → modal opens with
     the curated prompt and a Low danger badge.
  2. Send a message that triggers `get_weather` → modal opens with
     a Low danger badge and the city name in the body.
  3. Send a message that triggers `email_send` → modal opens with
     a High danger badge; click "Toujours pour cette session" and
     confirm the next `email_send` call STILL opens the modal
     (regression guard for the always-ask pin).
  4. Send a message that triggers `caldav_list_events` → modal
     opens with a Medium danger badge.
  5. Click "Autoriser" on any modal → tool runs, modal closes,
     round resumes.
  6. Click "Toujours pour cette session" → next call of the same
     tool runs without a modal.
  7. Press ESC on an open modal → `[DENY:…]` is dispatched, the
     tool does not run, the LLM round halts and surfaces a
     "Utilisateur refusé" tool result.
  8. Reload the page mid-modal → the modal re-opens on the
     persisted `needs_approval` history row.
  9. Web fetch with an allowlisted host runs without a modal;
     a non-allowlisted host always opens a modal (regression
     guard for the cross-tool rule removal).
  10. Capture a fresh screenshot of the modal for
      `docs/screenshots/` (optional).

## Open questions (left for follow-up, not blocking)

- **Per-arg session overrides** (e.g. "always allow
  `web_fetch` for `example.com` only"). Out of scope here. The
  current session override is per-tool; a future plan can key
  the override on `(tool, hash(args-pattern))` for fine-grained
  grants. The infrastructure is in place
  (`PermissionStore::session_overrides: HashSet<String>` —
  swap for a richer key type when needed).
- **Server-side opt-out for safe tools** (e.g. an operator
  flag `[permissions].allow_safe_tools_without_confirmation = true`
  that auto-allows `get_weather`, `calculate`, etc.). Out of
  scope here. If added, it lives in the tool loop's
  `force_allow` short-circuit as a per-tool whitelist consulted
  BEFORE the agent's `requires_confirmation` call. Tracked as
  a separate request.
- **DB persistence of session overrides** (per
  `auth_db_abstraction_required` decision in `project.md`).
  Out of scope here. Requires the auth DB abstraction to land
  first. The in-memory `PermissionStore` is the right shape to
  swap for a DB-backed implementation later.
- **i18n** of the modal copy. Today the labels are hard-coded
  French. The other UI strings follow the same posture. Not a
  regression; a separate i18n plan can pull every string at
  once.

## Rollout

- No DB migration, no feature flag, no new CLI option. The
  behaviour change is effective on next server start.
- This is a **security-relevant UX change**. Note in the commit
  body that:
  - Every tool call now requires a user click by default
    (previously only ~10 tools were gated).
  - The `web_fetch` cross-tool rule is removed; `web_fetch` is
    gated on every call regardless of `read_document` history.
  - The inline approval card is replaced by a `<dialog>` modal.
- User-visible regression: power users who relied on
  `get_weather` / `calculate` / `wikipedia` running without a
  click will now see a modal on the first call. The
  "Toujours pour cette session" button covers that case after
  one click. Document in the commit body.

## Plan tracking

This plan supersedes the working note in
`.kilo/plans/1791834996882-user-driven-tool-permission.md` for
behaviour, but the previous plan still documents the round-halt
and `was_invoked` removal mechanics — keep it as a reference for
the next reader.
