# Chat export (A1), per-bubble copy (A9), message edit & regenerate (A3)

Combined plan for catalog items **A1**, **A3**, and **A9** from
`.kilo/plans/1791234616248-ux-feature-agent-catalog.md`.

A1 and A9 are pure-frontend; A3 needs server-side persistence (a
`chat_messages` table, two new routes, ownership scoping). The three
items ship together because A3 is the dependency that opens the door
to future cross-device chat features and a much cleaner A1 export
shape (server-side `chat_messages` is the canonical source of truth;
localStorage becomes a hydration cache, not the only copy).

## 0. Decisions resolved

- **A3 storage engine**: SQLite or Postgres — both compile today, the
  existing `nagent-db` crate already has a dual-backend pattern
  (`passkeys`, `memories`, `preferences`, …). The new module follows
  the same pattern. No engine is picked here; the code targets both.
- **A1 export formats**: **Markdown + JSON + HTML**, in a single
  `Download ▾` menu next to `#chat-clear` in the Discussion header.
- **A3 message schema**: `id, role, content, ts, model, session_id` +
  a `version int` column for optimistic concurrency. **Attachments
  are not persisted** server-side in this plan — they remain a
  localStorage-only field for now; documents stay reachable via
  the existing `/v1/documents` API and the chips re-hydrate from
  the local copy. A follow-up plan can extend the schema with a
  `attachments JSONB` column once we know if attachments need to
  follow a chat across devices.
- **A3 edit / regenerate scope**: edit a single user message
  (truncate the history at that point + re-send), regenerate the
  last assistant reply. Branching (C5) and per-tool retry (C6) are
  not in scope.
- **A9 placement**: a small clipboard icon appended to every
  assistant bubble, sibling of the existing 🔊 replay button.
  Hidden during streaming (same gate as 🔊) and on widget-only
  bubbles (same `chat-message--widget-only` class).

## 1. Server-side: `chat_messages` store (A3 foundation)

### 1.1 Migration

File: `crates/nagent-db/src/migrations/0012_chat_messages.{up,down}.sql`

```
CREATE TABLE chat_messages (
    id            UUID        NOT NULL PRIMARY KEY,
    session_id    UUID        NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    user_id       UUID        NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    role          TEXT        NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    content       TEXT        NOT NULL,
    model         TEXT        NULL,
    ts            TIMESTAMPTZ NOT NULL DEFAULT now(),
    ordinal       BIGINT      NOT NULL,    -- monotonically increasing per session
    version       INTEGER     NOT NULL DEFAULT 1   -- optimistic concurrency
);
CREATE INDEX chat_messages_session_ordinal
    ON chat_messages (session_id, ordinal);
CREATE INDEX chat_messages_user_ts
    ON chat_messages (user_id, ts DESC);
```

`down.sql`: `DROP TABLE chat_messages;` (the two indexes drop with
it). Migration tracked by `_sqlx_migrations` so re-running is a
no-op.

The `chat_sessions(id)` reference is what gives us per-user
ownership: a route can verify `(user_id, session_id)` is bound
through the existing `ChatSessions::touch_and_verify` and then
only ever touches rows scoped to that binding.

### 1.2 `nagent-db` module

New file: `crates/nagent-db/src/chat_messages.rs`. Public surface
mirrors `chat_sessions.rs`:

- `pub struct ChatMessages(DbInner)` with `new(pool)`, `for_user(user_id)`.
- `pub struct ScopedChatMessages { user_id, inner }` with
  `list(session_id)`, `get(session_id, message_id)`,
  `append(session_id, row)`, `edit(session_id, message_id, new_content, expected_version)`,
  `truncate_after(session_id, message_id)`, `delete(session_id, message_id)`.
- Errors: `MessageError { NotFound, NotOwned, VersionMismatch, BadRole }`.
- A `MessageRecord { id, session_id, role, content, model, ts, ordinal, version }`
  public type.
- Dual-backend: `Inner::Sqlite(sqlx::SqlitePool) / Postgres(sqlx::PgPool)`,
  same shape as `chat_sessions`. SQL is the only backend-divergent
  bit (`?` placeholders vs `$1`); wrap in a small `query!`-style
  helper that picks the right flavour per backend.

Hook it up in `crates/nagent-db/src/lib.rs`:
- `pub mod chat_messages;`
- `UserDb::chat_messages() -> ScopedChatMessages` (next to
  `chat_sessions()` at line 390).
- `Db::chat_messages` field on the top-level `Db` handle.

### 1.3 HTTP routes

New module: `crates/nagent-server/src/chat/messages.rs` (sibling
of `sessions.rs`).

Mount on the same router as the existing
`POST /v1/chat/session` mint endpoint. Add to
`build_chat_session_router` in
`crates/nagent-server/src/documents/routes.rs:69-82` so the auth
envelope (RequireAuth + CSRF) is shared without duplicating
middleware.

New routes (all `RequireAuth` + CSRF on state-changing methods,
JSON, `X-Chat-Session-Id` header required where applicable):

| Method | Path                                                        | Action |
|-------:|-------------------------------------------------------------|--------|
| GET    | `/v1/chat/session/:sid/messages`                            | List messages, oldest first |
| POST   | `/v1/chat/session/:sid/messages`                            | Append `{role, content, model?}` |
| PATCH  | `/v1/chat/session/:sid/messages/:mid`                       | Edit content, requires `version` |
| DELETE | `/v1/chat/session/:sid/messages/:mid`                       | Delete one message (truncate tail) |
| POST   | `/v1/chat/session/:sid/regenerate`                          | Delete last assistant message; client re-streams |

All five use `AuthUser` from the `RequireAuth` extension, then
`ChatSessions::for_user(user.id).touch_and_verify(sid)` (the
existing helper) to enforce ownership. Any `MessageError::NotOwned`
maps to `403`; `NotFound` to `404`; `VersionMismatch` to `409`.

Reuse the route-error / `IntoResponse` pattern from
`chat::sessions::RouteError` so the wire shape stays
status+plaintext.

The chat UI keeps using `POST /v1/chat/completions` for the
*streaming* path — the new `/messages` routes only own the
*persistence* of message rows. The browser:
1. Appends the user message via `POST /messages`.
2. Streams the reply through the existing `/v1/chat/completions`.
3. Appends the finalised assistant message via `POST /messages`.

Edit / regenerate use the same `/messages` + `/v1/chat/completions`
sequence; no LLM-side change is required because the browser
already builds the `messages: [...]` array per turn.

### 1.4 LocalStorage → server-side migration on first authenticated load

`chat-sessions.js` already migrates the legacy
`nagent.chat.history` blob into a per-session key on first boot.
Extend that shim:

- When `auth.enabled` is true AND the user just signed in
  (`window.nagentAuth.getUser()` is set), walk every
  `nagent.chat.session.<id>` entry and `POST` each message to
  `/v1/chat/session/:sid/messages`. On 2xx mark the session
  migrated by setting `localStorage["nagent.chat.migrated.<id>"] = "1"`.
- On failure: log + continue (don't block boot). A re-run on next
  load retries. Sessions that never migrate keep working from
  localStorage only (the UI reads server first, falls back to
  localStorage).

The reverse path (server-side is the source of truth, localStorage
is a cache) becomes the default on subsequent loads. The
`renderHistory` path checks the `__nagentSessionBacked` flag on
the session metadata; if true, fetch from the server first.

## 2. Frontend: chat bubbles get a `data-message-id`

Right now `appendBubble` does not assign a per-message id; the
`history` array is positional. A3 needs stable ids.

Changes in `chat.js`:
- On `appendBubble` with `persist: true`, `crypto.randomUUID()` →
  set `div.dataset.messageId = id`, include `id` in the persisted
  history record.
- On `renderHistory`, copy `data-message-id` from the history
  record. Records without an `id` (legacy) get one minted on
  render so edit/regenerate works on old data too.
- `streamReply` already keeps a handle to the assistant bubble
  (`assistantEl`); after the final `appendBubble` for the
  assistant reply, stamp the same `data-message-id`.

The two history consumers (`submitUserTurn`, `streamReply`) and
the two hydration paths (`renderHistory`, `pickModelAndRetry` after
edit) all pass through the same `appendBubble` / `loadHistory` /
`saveHistory` trio, so a single edit in those three functions
covers the new behaviour.

## 3. Frontend: A1 — chat export (Markdown / JSON / HTML)

New file: `crates/nagent-server/src/static/chat-export.js` (≈120
LoC). Pure module, no global side effects. Imported by `chat.js`.

Public surface:
- `exportSession(sessionId, format, history)` where
  `format ∈ {"md", "json", "html"}`.
- `formatMarkdown(history)`: H1 title = session title (from
  `chat-sessions.js::loadSessions().find(s.id === sessionId).title`),
  then `### <role> (<ts>)\n\n<content>\n\n` per message. Tool
  traces are folded into a single fenced block per assistant
  message (collapsed-by-default in viewers that respect
  `<details>`): `- tool: <name> — <caption>\n  args: …\n  result: …`.
- `formatJson(history)`: `JSON.stringify({ id, title, exportedAt,
  messages: history }, null, 2)`. Same shape as `loadHistory` plus
  the session metadata.
- `formatHtml(history)`: full `<!doctype html>` page. Each message
  rendered through the **same** `marked` + `DOMPurify` pipeline as
  the live bubble (`renderMarkdown` + `decorateSafeLinks`), then
  wrapped in a `class="chat-message chat-<role>"` div. Inline
  `<style>` mirrors the bubble CSS (one screenful of rules,
  vendored with the rest of `style.css` for consistency).

Each format is then `Blob`ed and saved via `URL.createObjectURL` +
a programmatic `<a download>` click — same pattern as the existing
Transcript-mode `Download ▾` menu (`index.html:184-192`).

### 3.1 UI: `Download ▾` menu

Insert next to `#chat-clear` in the Discussion header
(`index.html:255`):

```html
<details id="chat-export-menu" class="download-menu" disabled>
  <summary class="download-menu-summary">Export ▾</summary>
  <ul class="download-menu-list" role="menu">
    <li role="menuitem"><a href="#" data-format="md">Markdown (.md)</a></li>
    <li role="menuitem"><a href="#" data-format="json">JSON (.json)</a></li>
    <li role="menuitem"><a href="#" data-format="html">HTML (.html)</a></li>
  </ul>
</details>
```

`disabled` follows the existing Transcript-menu contract: lifted
once the session has at least one message, mirrored by `chat.js`
on every `appendBubble` / `renderHistory`. Same `.download-menu`
CSS so it picks up the dark-theme tokens for free.

Filename: `<sanitised-title>-<yyyymmdd-hhmm>.{md,json,html}`.
Title sanitisation = `replace(/[^a-z0-9-_]+/gi, "_").slice(0, 40)`.

The menu is hidden on `auth.enabled = true` builds until the
authenticated user is set (we don't export a session someone else
could be looking at). Actually no — sessions are local, not
multi-user, so the menu is always available.

## 4. Frontend: A9 — per-bubble copy-as-Markdown

`appendBubble` is the single insertion point. After the
`ensureReplayButton` call (chat.js:1103-1105), also call
`ensureCopyButton(div)` for assistant bubbles.

- `ensureCopyButton(bubbleEl)`: identical cached-rebuild pattern
  to `ensureReplayButton` (chat.js:594+). The button is a small
  📋 / clipboard glyph (Unicode `U+2398` works in dark themes;
  fall back to SVG if the font lacks the glyph).
- Wire the click: `navigator.clipboard.writeText(bubbleEl._rawMarkdown)`,
  where `_rawMarkdown` is set whenever the bubble's prose is
  rendered (start with the text passed to `appendBubble`, overwrite
  on every `applyMarkdown` call with the latest accumulated
  markdown).
- User bubbles: a smaller "Copy text" button with the same
  clipboard icon, copying `bubbleEl.textContent` (plain text).
- Visibility: same two-tier gate as 🔊 (style.css:1682-1684):
  - CSS: `.chat-message--streaming .chat-message-copy { display: none; }`
  - JS: `refreshReplayButtonVisibility` (chat.js:2331) gets a
    sibling `refreshCopyButtonVisibility` that runs on every
    state change, hides the button on `chat-message--widget-only`
    bubbles the same way the replay button is hidden.

Persistence: no server-side change. Copy is a transient action.

## 5. Frontend: A3 — edit & regenerate

### 5.1 Edit a user message

- On hover, the user bubble shows a small ✎ icon next to the
  existing bubble. Click → the bubble's `textContent` is swapped
  for a `<textarea>` with the current value, plus Save / Cancel
  buttons. Save posts to the server:
  1. `PATCH /v1/chat/session/:sid/messages/:mid`
     body `{ content, version }`. On 409 (version mismatch) show
     a small inline error and refresh from server.
  2. `DELETE /v1/chat/session/:sid/messages/<everything-after-mid>`
     — server-side `truncate_after(mid)` deletes the tail in one
     transaction so the next turn sees a clean state.
  3. Re-render the DOM: the edited bubble stays, everything from
     the *old* next message onward is removed from the
     `#chat-messages` list (and the local cache).
  4. Auto-submit the user turn through the existing `streamReply`
     path so the user gets a fresh reply without re-pressing
     Enter. The model picker, temperature, and the location /
     timezone blocks are read from their current UI state.

- The pencil is hidden while the bubble is being edited, while a
  stream is in flight (`chat-message--streaming` set on the
  bubble that owns the editing), and for messages older than
  the user’s most recent turn (we don’t expose editing of
  historical turns in v1 — that is a follow-up).

### 5.2 Regenerate the last assistant message

- A 🔁 icon next to 🔊 on the last assistant bubble (and only the
  last one — older assistant bubbles do not get a regenerate
  button, that is what edit-then-resend is for).
- Click → `POST /v1/chat/session/:sid/regenerate` (the route
  deletes the last assistant row), then re-streams through
  `streamReply` using the same user message as the previous
  turn. The `AbortController` + the `chat-message--streaming`
  CSS gate are reused as-is; the only new wiring is
  re-invoking the LLM call.

### 5.3 Conflict semantics

- Optimistic concurrency on `version` prevents a stale edit from
  silently overwriting a turn that was appended by another tab.
  On `409 VersionMismatch` the UI shows "This message changed
  on the server — reload to see the latest" and offers a
  Reload action that re-fetches `GET /messages` and repaints
  the bubble. (The Reload action does *not* trigger an auto
  resubmit — the user explicitly re-sends after a reload.)
- On `403 NotOwned`: same wire shape as the existing
  `chat_sessions::RouteError::Chat(NotBound)` so a probing
  caller can't tell the two cases apart.

## 6. CSS additions (style.css)

All under the existing dark-theme tokens; no new colours.

- `.chat-message-copy` — sibling of `.chat-message-replay`; small
  icon button, hover-only visibility.
- `.chat-message--streaming .chat-message-copy` —
  `display: none`, mirrors the existing replay gate.
- `.chat-message--widget-only .chat-message-copy` — same hide.
- `.chat-message-edit-pencil` — hover-only on user bubbles;
  reuses the existing `.chat-message-replay` colour tokens.
- `.chat-message-edit-form` — inline textarea + Save/Cancel
  buttons; min-height 1.5em, max-height 12em, autosize.
- `.download-menu-list .chat-export-error` — small red label
  shown if the export throws (e.g. clipboard blocked).

## 7. Documentation sync

Per `AGENTS.md` §5, every surface that mentions the chat export
shape or the message lifecycle must be updated in the same commit:

- `docs/ui_features.md` §4 — add a §4.12 for the export menu, a
  §4.13 for the per-bubble copy button, and a §4.14 for the
  edit/regenerate affordance (matching the existing section
  numbering).
- `docs/ui_features.md` §9 (out of scope) — remove the
  "no message edit/regenerate" bullet.
- `docs/architecture.md` §2.6 / §2.7 — document the new
  `chat_messages` table, the five new routes, the
  dual-backend pattern, the localStorage → server migration
  shim.
- `README.md` "API" section — list the five new
  `/v1/chat/session/:sid/messages*` routes.
- `docs/integrations/caldav.md` (and any other agent doc that
  reads history) — note that the server is the source of truth
  for chat history; the in-memory agent context is still built
  from the streamed turn, not the table.

## 8. Tests

Server-side (`crates/nagent-server`):
- `chat_messages::repo` unit tests: append, list, edit (success
  + VersionMismatch), delete, truncate_after. Mirror the
  `chat_sessions` test style (in-memory sqlite, optional
  postgres-gated tests).
- HTTP integration test (`crates/nagent-server/tests/`):
  mint a session, post two messages, GET them back, PATCH one,
  POST regenerate, DELETE one, assert 401/403/404/409 on the
  negative paths. Use the existing test harness that builds an
  authenticated `AuthUser` extension.

Frontend:
- `scripts/verify-chat-export.ts` — pure-Node script that
  imports `chat-export.js` and asserts:
  - Markdown round-trips a known history to the expected
    string.
  - JSON includes `id`, `title`, `exportedAt`, `messages`.
  - HTML escapes a `<script>` injection inside a user message
    (DOMPurify contract).
- Extend `scripts/verify-chat-sessions.ts` (or its sibling
  harness) to cover the per-message-id assignment in
  `appendBubble`.

Static-assets test (`tests/static_assets.rs`): add a guard
asserting `#chat-export-menu` is inside the app-shell template
(not in the body root) so the menu can't leak to anonymous
visitors on `auth.enabled = true` builds.

## 9. Out of scope (deferred)

These come up while planning but are not in this plan:

- **Cross-device chat sync**: enabling same-session pickup from
  a phone is a follow-up that uses the same `chat_messages`
  table. The shim in §1.4 already covers the offline
  localStorage copy.
- **Attachment persistence server-side**: skipped per §0. The
  `chat_messages.attachments JSONB` column is a one-line
  follow-up.
- **Branching (C5)**: edit only truncates the tail; we don't
  create a sibling branch session.
- **Per-tool retry (C6)**: stays a separate plan item.
- **Conversation export of all sessions as a zip**: only the
  active session ships in v1. Multi-session export is a
  follow-up.
- **Server-side conversation search (`history-agent`, B7)**:
  depends on the `chat_messages` table from this plan but
  lives separately.

## 10. Risk register

- **localStorage → server migration races with live edits**:
  the shim in §1.4 only migrates sessions without
  `nagent.chat.migrated.<id>` set. A user who creates a session
  *during* the first migration pass would race. Mitigation:
  the shim runs once per page load after `auth.js` confirms
  the user; subsequent reloads are no-ops because the flag
  is set. Worst case: that one session lives on the
  localStorage copy until the next reload, where it migrates.
- **Optimistic concurrency on edit**: a user who edits the
  same turn from two tabs will hit `409` on the second one.
  Documented in the §5.3 conflict semantics; UI shows a clear
  "reload to see latest" prompt.
- **Export HTML sanitization surface**: the HTML format is the
  widest attack surface (a downloaded file is read in many
  contexts). `DOMPurify` is reused with the same config the
  live bubble uses; the test in §8 covers a `<script>`
  injection.
- **Regenerate cost**: regenerating re-invokes the LLM and any
  tool round-trips. The existing rate-limit + tool-loop caps
  apply; no new abuse vector is opened.
- **Schema migration is irreversible on a populated DB only
  by hand** (down.sql drops the table). Standard sqlx flow; no
  production data on this server today, but the down migration
  is tested in `crates/nagent-db/src/migrate.rs::undo_tests`.

## 11. Validation

- `cargo fmt --all`
- `cargo build --all-targets --all-features` — zero warnings
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test --all` (server + db crates, including the new
  integration test)
- `cargo audit`
- Frontend test: `node scripts/verify-chat-export.ts` +
  `node scripts/verify-chat-sessions.ts`
- Manual smoke:
  1. New chat, send a turn, open `Export ▾`, download
     Markdown/JSON/HTML, open all three in the matching viewer.
  2. Hover an assistant bubble → click 📋, paste in another
     app, confirm the markdown round-trips.
  3. Edit a user message (mid-session), confirm the tail is
     truncated server-side and the new reply streams.
  4. Click 🔁 on the last assistant bubble, confirm a new
     reply streams without re-pressing Enter.
  5. Open a second tab, edit the same turn → 409 with
     "reload to see latest".
  6. Log out, log back in on a fresh browser/device, confirm
     the session list is empty (no cross-device sync in v1)
     but a pre-existing session still migrates its messages on
     the first load.
