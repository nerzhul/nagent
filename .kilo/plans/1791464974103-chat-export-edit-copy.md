# Chat export (A1), per-bubble copy (A9), message edit & regenerate (A3) — residual

Refresh of `.kilo/plans/1791464974103-chat-export-edit-copy.md` after the
three catalog items landed in the codebase. The original §1–§6, §8 are
all implemented:

- `crates/nagent-db/src/chat_messages.rs` + migration `0012_chat_messages`
  with `version int` optimistic-concurrency column.
- `crates/nagent-server/src/chat/messages.rs` exposing the five
  `/v1/chat/session/:sid/messages*` routes plus
  `/v1/chat/session/:sid/regenerate`, mounted on the same auth envelope
  as the session-mint endpoint (`documents/routes.rs:95-100`).
- `migrateAllSessionsToServer()` in `chat.js:1222` POSTs every
  not-yet-migrated localStorage session on the first authenticated load
  and sets `nagent.chat.migrated.<id> = "1"` on success.
- `chat-export.js` (Markdown / JSON / HTML) + `#chat-export-menu`
  dropdown next to `#chat-clear` in `index.html:267`.
- Per-bubble `chat-message-copy` (`chat.js:654`), `chat-message-edit-pencil`
  (`chat.js:765`), `chat-message-regenerate` (`chat.js:798`) with the
  two-tier visibility gate (streaming + widget-only) inherited from the
  existing 🔊 replay button.
- `chat-message-edit-form` (`chat.js:863`) with the GitHub-style hint
  row above the action buttons (Enter to save, Esc to cancel) — see
  `latest_session_digest: ses_ede53a749ffeeYdVz19exjbStX`.
- Tests: `crates/nagent-server/tests/chat_messages.rs` (HTTP integration,
  401/403/404/409 paths) + `scripts/verify-chat-export.ts` (Markdown /
  JSON / HTML / DOMPurify guard).

What is **not** done is documented below.

## 0. Open work, reprioritised

P0 ships next; P1 follows once P0 lands; P2 stays a menu until P0+P1
are stable.

### P0 — Documentation sync (the §7 of the original plan that never landed)

`AGENTS.md` §5 requires doc updates in the same commit as the feature.
The features shipped without them, so several surfaces still advertise
behaviour that no longer matches reality. Land this in **one commit**
before adding more chat-messages features.

- `docs/ui_features.md`
  - Add **§4.12 Chat export menu** (Download ▾ → Markdown / JSON / HTML,
    filename shape, `disabled` gate lifted at first message).
  - Add **§4.13 Per-bubble copy button** (assistant = raw markdown via
    `navigator.clipboard.writeText`, user = `textContent`).
  - Add **§4.14 Edit & regenerate affordance** (pencil on user bubbles,
    🔁 on the last assistant bubble, 409 reload flow, streaming/widget
    visibility gates).
  - In §9 (out of scope), drop the bullets for "no export", "no message
    edit/regenerate". Keep the other items (no light theme, no
    drag-and-drop, no partial streaming, etc.).
- `docs/architecture.md` §2.6/§2.7 — add a short paragraph on
  `chat_messages` (table shape, dual-backend, five new routes, the
  localStorage → server migration shim, `version` optimistic
  concurrency).
- `README.md` "API" section — list the five new routes
  (`/v1/chat/session/:sid/messages`,
  `/v1/chat/session/:sid/messages/:mid`,
  `/v1/chat/session/:sid/regenerate`) with one-line descriptions.
- `docs/integrations/caldav.md` and any other agent doc that reads
  history — add a "server-side history" note clarifying that the
  streamed turn still drives the in-memory agent context; the table is
  the source of truth for the chat surface only.

Validation: a grep over `docs/` + `README.md` for
`chat_messages|/v1/chat/session/:sid/messages|chat-export-menu` returns
hits in the expected files only.

### P1 — Layer on top of the existing foundation

These are the items that were deferred in §9 of the original plan and
that the new `chat_messages` table directly enables. Ordered by UX gap
descending, ties broken by effort.

1. **Attachment persistence server-side** — small. Add an
   `attachments JSONB/TEXT` column to `chat_messages` via migration
   `0013_chat_messages_attachments.{up,down}.sql`. Extend
   `POST /messages` to accept `attachments?: [{id, name, mime, size}]`.
   `chat.js` `composer-attachments.js` already builds the array; the
   only frontend work is to ship it through the persistence path. UX
   gap: 4 (today an attachment chip is lost on device switch).

2. **Server-side conversation search (`history-agent`, B7)** — small–M.
   New `history_search(query, limit, before_ts?)` agent reading from
   `chat_messages`. Frontend surfaces a `/`-keyed search bar at the top
   of `#chat-messages` (catalog A6 — merge the two). UX gap: 4 (the
   `chat_messages` table is already there, only the LLM-callable
   shape is missing).

3. **Cross-device chat sync** — M. The migration shim in
   `chat.js:1222` already uploads the existing localStorage copy on
   first authenticated load; the missing piece is the **read** path
   (`GET /v1/chat/session` returns the server's `chat_messages`
   newest-first so a fresh device sees the same history). Today
   `chat-sessions.js` is localStorage-only. UX gap: 4.

4. **Edit historical turns** — S. Today the pencil is hidden for any
   user message older than the most recent turn
   (`chat.js:820-826`). Lift that gate, reuse the existing edit-form +
   `PATCH + DELETE-after-mid` flow. Optimistic concurrency on
   `version` already covers the cross-tab race. UX gap: 3.

5. **Multi-session export as a zip** — S. Reuse `chat-export.js`'s
   per-session formatters, zip them with a tiny vendored `jszip`
   (already on the same vendor list — see `static/vendor/`). UI: a
   checkbox column on each sidebar row + an "Export selected ▾" entry
   in the existing export menu. UX gap: 3.

### P2 — UX improvements worth considering once P0 + P1 are stable

These came up while reviewing the implementation; none are in the
catalog yet. Listed for visibility, not for commitment.

- **Streaming partial assistant text in Discussion mode**: today the
  live bubble shows "…" / a typing indicator while the model emits
  tokens, then snaps to the final text once `streamReply` finalises.
  Wire `applyMarkdown` to be called on every token chunk (the server
  already emits partial chunks — verify on
  `/v1/chat/completions`). UX gap: 4, effort: S.
- **Undo toast after edit / regenerate**: store the previous message
  blob in a small `window.__nagentEditUndo` slot for 5 s and offer an
  inline "Undo" pill. Pure-frontend; rolls back DOM + posts the
  inverse `PATCH`. UX gap: 3, effort: S.
- **Keyboard shortcuts for the new affordances**: `C` to copy the
  focused bubble's markdown, `E` to enter edit mode on the focused
  user bubble, `R` to regenerate the last assistant turn. Visible in
  the `chat-shortcuts.js` overlay next to existing bindings. UX gap:
  2, effort: S.
- **"Regenerate with model…" picker**: the regenerate icon currently
  re-runs the previous model. Open a tiny popover (same widget family
  as the approval card) so the user can pick an alternative model for
  the retry without leaving the bubble. UX gap: 3, effort: S.
- **Right-click / long-press context menu**: the hover-only icon row
  is invisible on touch devices. Add a `contextmenu` handler that
  shows copy / edit / regenerate in a floating menu; same widget
  primitives as `mode.js`. UX gap: 3, effort: S.
- **Diff preview on edit**: when the edited text differs from the
  original, show a one-line "was: … / now: …" diff above the Save
  button so the user catches wrong-paste mistakes before submitting.
  Pure-frontend diff using `diff` (vendored). UX gap: 2, effort: S.
- **Reasoning visibility toggle per bubble**: some models emit
  `<think>` blocks. Today they either render inline or are dropped
  (`chat.js` rendering pipeline). A per-bubble "Show reasoning"
  toggle makes debugging retries much easier. UX gap: 3, effort: S–M.
- **Confirm-before-regenerate when the prior turn used paid tools**:
  if the last assistant message contains a `web_fetch` / `wikipedia`
  / `caldav_*` trace, show a small "Re-runs N external calls"
  confirmation before issuing `POST /regenerate`. UX gap: 2,
  effort: S.

### Deferred (still out of scope, with rationale)

- **Branching (C5)**: chat-edit currently truncates the tail; no
  sibling branch. Re-evaluate once edit-historical-turns (P1.4) is in
  use — the data shape needed for branching may overlap with the
  attachments column.
- **Per-tool retry (C6)**: separate plan item, no schema change
  needed — it is a frontend-only re-issue of the same tool call.
- **Conversation search across all sessions**: depends on
  `history-agent` (P1.2) plus a session-scoped facet; deferred until
  the per-session search lands.
- **Attachment OCR / preview in the export**: an HTML export should
  embed images / PDFs as `cid:` or as base64 so the downloaded file is
  self-contained. Large follow-up; not on the P0–P2 list.

## 1. Decisions to revisit

- **Attachment MIME in the export HTML**: if we add `attachments`
  server-side, the HTML export should either inline images or link to
  `/v1/documents/:id`. Pick one before P1.1 lands so the export
  shape is stable.
- **Edit-history visibility**: P1.4 (edit historical turns) opens the
  door to "restore this version" affordance. The `version` column
  already supports a stack; decide whether to keep N versions per
  message or only the latest.
- **Optimistic-concurrency UX**: today `409` shows a Reload action
  that re-fetches the row but does **not** resubmit. Consider an
  "Edit & resubmit anyway" path for power users once P1.4 is in.

## 2. Risks carried over

- **localStorage → server migration races with live edits** (original
  §10): unchanged. The flag-per-session makes it self-healing on the
  next load.
- **Cross-tab 409 on edit**: already surfaced in the UI as
  "reload to see latest". Once P1.3 (cross-device sync) lands, the
  same 409 also covers device-vs-device edits — the messaging needs
  to stay neutral about "another tab vs another device".
- **Export HTML sanitisation surface**: the live bubble's `marked` +
  `DOMPurify` pipeline is reused; `verify-chat-export.ts` already
  covers a `<script>` injection. Re-run on every `chat-export.js`
  change.

## 3. Validation

P0 (documentation sync only):

- `rg -n "chat_messages|/v1/chat/session/:sid/messages|chat-export-menu" docs/ README.md` — every hit lives in the expected surface.
- Manual: open `docs/ui_features.md` §9 — no "no export" / "no message
  edit/regenerate" bullets.

P1+ (any new code):

- `cargo fmt --all`
- `cargo build --all-targets --all-features` — zero warnings
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all` (incl. the `chat_messages.rs` integration test)
- `cargo audit`
- `node scripts/verify-chat-export.ts` + `verify-chat-sessions.ts`

## 4. Suggested commit order

1. `docs: document chat export, copy, edit & regenerate features` (P0)
2. `feat(chat): persist attachments server-side` (P1.1)
3. `feat(agents): history_search agent + /-search bar` (P1.2 + A6)
4. `feat(chat): server-side session list for cross-device sync` (P1.3)
5. `feat(chat): allow editing any user turn` (P1.4)
6. `feat(chat): multi-session export as a zip` (P1.5)
7. P2 polish items, one commit each.