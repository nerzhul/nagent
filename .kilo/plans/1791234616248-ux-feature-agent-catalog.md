# nagent — UX-focused feature & agent catalog

A prioritized menu of UX improvements for the nagent web app.
The proposal covers both **frontend features** (UI-only) and **new chat
agents** that the LLM can call on the user's behalf. Nothing here is
implemented in this turn — this document is a planning menu the user
can pick from.

## 0. Scope and methodology

- **Audience**: the end user sitting in front of the browser — the
  STT/Discussion/Auth views described in `docs/ui_features.md`.
- **Out of scope for this document**: backend hardening, new infra,
  LLM provider plumbing, multi-tenant SaaS concerns. These proposals
  assume the existing single-binary `nagent-server` topology.
- **Prioritization axes**:
  - **UX gap** — how visibly annoying is the missing feature for a
    typical user today? (1 = invisible, 5 = constant papercut.)
  - **Effort** — relative implementation size in this codebase. (S ≈
    one file / one route, M = one new subsystem surface, L = new
    binary component.)
  - The catalog is grouped by **category** (UX gaps vs new agents vs
    cross-cutting), then sorted within each category by descending
    UX gap.

Existing reference points:

- Agent trait (`crates/nagent-agents/src/agents.rs`).
- Per-user credentials vault + setup-only HTTP probes
  (`docs/architecture.md` §2.6, §2.7; `docs/integrations/caldav.md`).
- `UserContext::secret` / `UserContext::update_secret` (read + write).
- `chat.js` (~2,800 lines), `app.js`, `audio.js`, `tts.js`,
  `geolocation.js`, `auth.js`, `chat-sessions.js` (localStorage).
- Out-of-scope list today (`docs/ui_features.md` §9):
  - No chat export, no drag-and-drop, no streaming partials, no
    system-prompt presets, no light theme, no message
    edit/regenerate, no cross-device session sync.

## A. End-user UX gaps (UI / frontend-only)

### A1. Chat session export (Markdown / JSON / PDF)

- **Problem**: only Transcript mode offers a `.txt` download. A
  multi-turn chat the user spent 20 min curating disappears if the
  browser storage is cleared.
- **Proposal**: a `Download ▾` menu next to `#chat-clear` offering
  Markdown (with reasoning + tool traces collapsed by default), raw
  JSON (machine-readable, same shape as localStorage), and HTML
  (sanitised, printable). All three flows are pure-frontend — the
  client renders and `URL.createObjectURL` triggers the save.
- **UX gap**: 5. **Effort**: S. **Dependencies**: none.

### A2. Streaming partial transcripts in Transcript view

- **Problem**: `PartialTranscript` tags are received but discarded
  (`docs/ui_features.md` §9). The user sees nothing while they speak
  — only a finalised line appears. For long utterances this feels
  broken ("is it recording at all?").
- **Proposal**: append a `<li class="partial">` for the current
  utterance, update its text in place on every partial, and replace
  it (not append) when the final arrives. Color/style via existing
  CSS variables; use the same `chat-loader` accessibility hook so
  screen readers don't double-announce.
- **UX gap**: 5. **Effort**: S. **Dependencies**: `app.js`
  `appendLine` only.

### A3. Message edit & regenerate

- **Problem**: there's no way to fix a typo in a sent user message
  other than deleting the rest of the session, and no way to ask the
  LLM to retry a bad answer without rewriting the prompt.
- **Proposal**:
  - User bubble gets an inline pencil on hover → opens a textarea
    that submits `PATCH /v1/chat/session/:id/messages/:mid` (new
    route, owner-scoped). The edited message + every message after
    it is re-fed into the next LLM request.
  - Assistant bubble gets a "regenerate" icon → POSTs the same user
    turn again. The existing `AbortController` + tools-footer
    machinery already covers most of the wiring.
- **UX gap**: 4. **Effort**: M. **Dependencies**: server-side
  per-session message store; `UserContext` already handles history
  injection.

### A4. Drag-and-drop file upload (Discussion)

- **Problem**: `docs/ui_features.md` §9 lists drag-and-drop as
  missing. The `documents/` subsystem + `read_document` agent exist,
  so the backend is ready; only the input affordance is missing.
- **Proposal**: a `<button>` next to `#chat-send` opens the OS file
  picker; `#chat-input` is also a drop target with a
  `--drag-over` highlight. Both paths call the existing
  `POST /v1/documents` and surface the file name as an inline
  attachment chip.
- **UX gap**: 3. **Effort**: S. **Dependencies**: existing
  `documents/` mount + `read_document` agent.

### A5. System prompt presets library

- **Problem**: `#chat-system` textarea starts empty. Users who want a
  "concise translator" persona or a "SRE copilot" persona have no
  shared starting point.
- **Proposal**: a sidebar of named presets (Concise translator /
  SRE copilot / Code reviewer / Multilingual tutor / Coach / …)
  that materialise the textarea when clicked. Presets ship in the
  binary (`crates/nagent-server/src/static/prompts/*.md`); operators
  can add their own via `[llm].prompt_presets_dir` in TOML.
- **UX gap**: 3. **Effort**: S–M. **Dependencies**: static assets
  loader; no new server route.

### A6. In-session search (Ctrl+F-aware)

- **Problem**: in a 200-message session the user can't find the
  earlier quote. Browser Ctrl+F hits code blocks / hidden reasoning.
- **Proposal**: a custom filter bar at the top of `#chat-messages`:
  keystroke `/` focuses it, matching messages stay visible,
  non-matching ones dim (no destruction of the DOM tree — important
  for re-streaming). Buttons for next / previous match.
- **UX gap**: 3. **Effort**: S. **Dependencies**: none.

### A7. Light theme switcher

- **Problem**: `docs/ui_features.md` §7.4: dark theme only. Bright
  rooms make the UI hard to read.
- **Proposal**: a theme pill in the header (sun/moon icon),
  preference persisted to `user_preferences.theme` (new column) so
  the auth path stays canonical. CSS exposes `--bg-light` /
  `--bg-dark` token pairs; `prefers-color-scheme` auto-selects on
  first visit (no flicker via `data-theme` set in `<head>` before
  paint).
- **UX gap**: 3. **Effort**: S. **Dependencies**: a new
  `user_preferences.theme` column + migration `0009`.

### A8. Pinned / starred messages

- **Problem**: in a long chat the user has no way to mark "this is
  the answer I wanted". Search helps but the user wants the
  answer-at-a-glance.
- **Proposal**: a star icon on every bubble. Starred messages move
  to a "Pinned" section at the top of `#chat-messages` (above the
  inline voice widget if active). Per-session; persists in
  `nagent.chat.pins.<session_id>` localStorage now, moves to the
  server table when message storage lands (A3).
- **UX gap**: 2. **Effort**: S. **Dependencies**: none today;
  later folds into the per-session store from A3.

### A9. Per-bubble "copy as Markdown" button

- **Problem**: copying a code block currently strips the backticks.
  Math, tables, lists lose formatting when pasted elsewhere.
- **Proposal**: a small clipboard icon on assistant bubbles. Uses
  `navigator.clipboard.writeText` with the raw markdown (the same
  string `applyMarkdown` consumed). For user bubbles it copies
  plaintext.
- **UX gap**: 2. **Effort**: S. **Dependencies**: none.

### A10. Mobile / tablet responsive layout

- **Problem**: the chat sidebar and the header are desktop-first;
  on a phone the sidebar overlays the conversation when expanded,
  and `#chat-input` fights the on-screen keyboard.
- **Proposal**: a hamburger-style header on `< 720 px` width; the
  sidebar slides in as a drawer. `#chat-input` uses
  `visualViewport` API to keep the composer above the keyboard on
  iOS Safari.
- **UX gap**: 3. **Effort**: M. **Dependencies**: none.

### A11. PWA / install-to-home-screen

- **Problem**: users on iPad / Android who treat the page as an app
  have to re-open the URL each session, and the browser may evict
  the page.
- **Proposal**: ship a `manifest.webmanifest` + a no-op service
  worker that precaches the static assets. Add an "Install" prompt
  that fires once per session and respects
  `beforeinstallprompt`. Standalone mode hides the browser chrome.
- **UX gap**: 2. **Effort**: M. **Dependencies**: none (assets are
  already served by the Rust static handler).

### A12. Approval card: deny with reason

- **Problem**: `requires_confirmation` denial today only emits
  `"Utilisateur refusé"`; the LLM cannot learn *why*.
- **Proposal**: in the inline approval card (per `docs/ui_features.md`
  §3.4.1) the Deny button opens a tiny popover with three
  radio choices (sensitive data / wrong moment / ask differently)
  plus a free-text field. The chosen reason is appended to the
  denial string the tool emits.
- **UX gap**: 2. **Effort**: S. **Dependencies**: extend
  `[APPROVE_DENY_REASON:...]` parsing in
  `parse_decision_prefix`.

## B. New chat agents (LLM-callable)

### B1. Reminders / tasks (`reminder-agent`)

- **Problem**: a voice-first assistant that cannot remember "in
  30 minutes, remind me to call Alice" feels incomplete. Today the
  LLM has no durable store of *future* state.
- **Proposal**: a `reminder_set` / `reminder_list` / `reminder_cancel`
  agent trio backed by a new `reminders` table (sqlite or postgres,
  same `nagent-db` crate). A background tokio task polls the table
  every 30 s and dispatches due reminders via the WebSocket: the
  chat UI pops a system bubble ("⏰ reminder: call Alice") that
  triggers TTS if enabled. Browser `Notification` permission is
  requested on first use (gated by existing settings-tab UX).
- **UX gap**: 5. **Effort**: M. **Dependencies**: `nagent-db`
  migration `0009_reminders.sql`; per-session WebSocket channel
  (already exists).

### B2. Long-term memory (`memory-agent`)

- **Problem**: the LLM forgets across `POST /v1/chat/session` calls.
  The user has to re-explain "my doctor is Dr Martin" every session.
- **Proposal**: a `memory_recall` / `memory_store` pair. `store`
  writes a structured triple (`subject`, `predicate`, `value`,
  `confidence`, `source_session_id`) to a new `memories` table;
  `recall` does a tiny vector-less keyword+tag match. The system
  prompt in `llm::prompt` auto-injects the top-N matches on every
  turn (gated by a `user_preferences.memory_enabled` flag, off by
  default). Audit log already covers the write path.
- **UX gap**: 4. **Effort**: M. **Dependencies**: migration
  `0010_memories.sql`; flag in `user_preferences`.

### B3. Calendar update / delete (`caldav-agent` extension)

- **Problem**: the CalDAV agents ship read + add only by design
  (`docs/architecture.md` §2.4). The user can create an event but
  not move or cancel it.
- **Proposal**: add `caldav_update_event` and `caldav_delete_event`
  agents behind the same `confirm-on-write` policy as
  `caldav_create_event`. Requires opening up
  `CalDavClient::update` / `delete` (gated by a feature flag in
  v1.1). Audit row per write.
- **UX gap**: 3. **Effort**: S–M. **Dependencies**: `caldav-agent`
  feature already on; `CalDavClient` plumbing.

### B4. Email (IMAP/SMTP) — first concrete follow-up after CalDAV

- **Problem**: `docs/architecture.md` §"Per-user credentials" lists
  IMAP/SMTP as a follow-up PR. A voice-first assistant that can't
  read or send email is a paperweight for productivity users.
- **Proposal**: add an `email_imap` / `email_smtp` `ServiceDef`
  (fields: host, username, password, port, tls). Agents:
  `email_list_inbox(folder, since, limit)`,
  `email_get_message(uid)`,
  `email_send(to, subject, body, in_reply_to)`. Hardening: TLS
  required by default; `confirm-on-send`; new `[email]`
  allow-list for outbound recipients (operator-set).
- **UX gap**: 4. **Effort**: L. **Dependencies**: new crate
  (lettre / async-imap) or hand-rolled; credentials vault already
  wired.

### B5. Home Assistant (`home-assistant-agent`)

- **Problem**: same shape as email — already named in the README as
  a follow-up. Users on the "voice assistant in the kitchen wall"
  pitch expect "turn off the living room lights".
- **Proposal**: `ServiceDef` `home_assistant` with `url` +
  `access_token`. Agents: `ha_list_entities(domain?)`,
  `ha_get_state(entity_id)`,
  `ha_call_service(domain, service, data)`. SSRF policy reuses
  the `WEB_FETCH_ALLOW_PUBLIC` knob but defaults to deny. The
  setup probe (`POST /api/integrations/home_assistant/probe`)
  validates the URL + token and lists the entity registry so the
  UI can offer a dropdown.
- **UX gap**: 3. **Effort**: M. **Dependencies**: Home Assistant
  REST API; credentials vault.

### B6. GitHub (`github-agent`)

- **Problem**: README §"Per-user credentials" lists it explicitly.
  Devs will ask the assistant "what's open in my team's repo?".
- **Proposal**: `ServiceDef` `github` with `personal_access_token`.
  Agents: `github_search_code`, `github_list_issues`,
  `github_get_issue`, `github_create_issue`,
  `github_list_prs`. All write paths `confirm-on-write`. Uses the
  REST API (`api.github.com`); hostname allow-list mandatory.
- **UX gap**: 3. **Effort**: M. **Dependencies**: GitHub REST API;
  credentials vault.

### B7. Personal transcript / chat search (`history-agent`)

- **Problem**: with per-session chat storage (A3) and possibly
  server-side persistence, the user wants "what did I tell the
  assistant last week about the kitchen remodel?". Today: no
  — `chat.js` keeps each session in its own localStorage blob.
- **Proposal**: `history_search(query, since, until, session_id?)`
  agent. Once per-session storage lands on the server, this becomes
  a small SQL query. Before that, it reads `localStorage` from
  the browser via a new `DocumentSource`-like capability.
- **UX gap**: 3. **Effort**: M (server-backed) / S (local-only).
  **Dependencies**: A3; permission scope ("may read every
  session").

### B8. URL preview / unfurl (`link_preview-agent`)

- **Problem**: the user pastes a URL in chat; the LLM has to call
  `web_fetch` (a full GET + clean text) just to render a one-line
  preview.
- **Proposal**: a lightweight `link_preview(url)` agent that returns
  just `{ title, description, image_url, site_name }` (Open Graph
  + Twitter card meta). Same egress hardening as `web_fetch`
  (allow-list, SSRF guard). The chat UI auto-detects bare URLs in
  the user message and emits a one-line card before sending.
- **UX gap**: 2. **Effort**: S. **Dependencies**: reuses
  `web_fetch` egress client.

### B9. Timer / stopwatch (`timer-agent`)

- **Problem**: "set a timer for 12 minutes" is the canonical
  smart-speaker request; nagent cannot answer it.
- **Proposal**: `timer_set(duration_secs, label)`,
  `timer_list()`, `timer_cancel(id)`. In-memory on the server is
  fine for v1 (the user is on the same browser). Notifications
  surface through the existing WebSocket → "system bubble" path
  (same plumbing as B1).
- **UX gap**: 4. **Effort**: S. **Dependencies**: WebSocket
  outbound; no DB needed.

### B10. Translation (`translate-agent`)

- **Problem**: STT already translates to English (checkbox); the
  user can't ask "translate this sentence to Japanese" in chat
  without the LLM doing it poorly.
- **Proposal**: a local translation agent (LibreTranslate public
  API or `translate` crate). Allow-list operator-configured. The
  per-bubble TTS voice already auto-resolves from the user's
  reply-language picker — translation plugs into the same flow.
- **UX gap**: 3. **Effort**: S. **Dependencies**: optional HTTP
  client.

### B11. Image generation / vision

- **Problem**: the user wants "draw a logo for my bakery" or "what
  does this screenshot say?".
- **Proposal**: vision side first (cheaper): `vision_describe(image_url | upload_id)` that forwards to Ollama's vision-capable model (llava, llama3.2-vision). Generation: `image_generate(prompt, size, steps)` against a local Stable Diffusion backend is a much larger lift and out of scope for v1.
- **UX gap**: 3. **Effort**: M (vision) / L (generation). **Dependencies**: image attachment path (A4); Ollama vision model.

### B12. Map / directions (`maps-agent`)

- **Problem**: "where is the nearest pharmacy" — chat has no sense
  of place beyond the optional location block (which the LLM has
  to call `web_fetch` for, badly).
- **Proposal**: `maps_search(query, near?)` / `maps_directions(from, to, mode)` against OpenStreetMap (Nominatim + OSRM). No key required; polite rate limit (`User-Agent`). Optional static-map image returned in `untrusted_output` fenced form for the chat widget layer.
- **UX gap**: 3. **Effort**: M. **Dependencies**: egress allow-list; Nominatim ToS compliance.

## C. Cross-cutting UX improvements

### C1. Per-session tool approval persistence

- **Problem**: today the `PermissionStore` override set is in-memory
  and dies on server restart (`docs/architecture.md` §3.4.1). The
  user who clicks "Always allow `web_fetch` to wikipedia.org" loses
  the preference on every reboot.
- **Proposal**: persist `(user_id, chat_session_id, tool_name,
  pattern)` overrides to `nagent-db`. UI exposes a clearable list
  in Settings → Integrations → "Always-allow rules".
- **UX gap**: 3. **Effort**: M. **Dependencies**: `nagent-db`
  migration `0011_permission_overrides.sql`.

### C2. Voice command grammar

- **Problem**: there's no way to say "new chat" or "stop" while the
  mic is open — only the keyboard shortcut works.
- **Proposal**: a tiny client-side intent classifier (regex on the
  partial transcript, not a separate LLM round-trip) that watches
  for short commands: "stop", "nouvelle conversation", "efface",
  "envoie". They bypass the chat submit path and call the same
  handlers as the keyboard shortcuts.
- **UX gap**: 3. **Effort**: S. **Dependencies**: A2 (partial
  transcript streaming).

### C3. Inline citations for `web_fetch`

- **Problem**: the LLM cites a Wikipedia URL in prose, but the user
  can't jump to the source — there's no numbered reference list
  at the end of the bubble.
- **Proposal**: when the LLM emits a tool call with `web_fetch`,
  the chat UI records the URL in the bubble's `_toolCitations`
  array and appends a compact list `[1] [2]` at the bottom of the
  prose. Pure-frontend: just walks the same `tool_result` SSE
  frames already in scope.
- **UX gap**: 3. **Effort**: S. **Dependencies**: none.

### C4. Push-to-talk toggle vs hold-to-talk

- **Problem**: the mic is click-to-toggle. Power users want
  push-to-talk (hold Space).
- **Proposal**: a `#chat-ptt-mode` toggle in Settings → Audio:
  `click` (current) vs `push-to-talk` (Space bar). PTT mode uses
  `keydown` / `keyup` and `e.preventDefault()` on the textarea so
  focus stays in the input.
- **UX gap**: 2. **Effort**: S. **Dependencies**: none.

### C5. Conversation forking

- **Problem**: the user wants to ask "what if I'd asked this
  differently?" without losing the current thread.
- **Proposal**: a "Fork from here" button on every assistant
  bubble. Creates a new chat session whose history is the
  truncated parent. Backed by per-session storage from A3.
- **UX gap**: 2. **Effort**: M. **Dependencies**: A3.

### C6. Tool call retry vs whole-turn retry

- **Problem**: today if a tool fails, the LLM retries on the next
  round (capped by `LLM_MAX_TOOL_ROUNDS = 4`). The user has no way
  to say "retry just that one call with different args".
- **Proposal**: per-tool-entry "Retry" button in the tools footer
  (`docs/ui_features.md` §4.6.2). Posts a sentinel
  `[RETRY:tool_call_id]` that the chat route intercepts and
  re-dispatches, mirroring the approval-decision plumbing in
  §3.4.1.
- **UX gap**: 2. **Effort**: M. **Dependencies**: extend
  `parse_decision_prefix`.

### C7. Settings export / import

- **Problem**: an operator who fine-tunes a system prompt,
  presets, and theme across many machines re-does the work
  every time.
- **Proposal**: `Export settings` / `Import settings` buttons on
  the Settings tab. JSON file with every `user_preferences`
  field + the localStorage mirror keys the user has set. Import
  is gated by a confirm dialog.
- **UX gap**: 2. **Effort**: S. **Dependencies**: A7 (theme
  field exists in `user_preferences`).

### C8. Inline code-block copy + syntax highlighting

- **Problem**: code blocks copy as raw text and lack syntax
  highlighting.
- **Proposal**: highlight.js vendored under
  `static/vendor/highlight/`, applied to `<pre><code>` after
  `DOMPurify`. A copy button is prepended via `decorateCodeBlocks`
  post-processor (parallel to `decorateSafeLinks`).
- **UX gap**: 2. **Effort**: S. **Dependencies**: vendor bump.

## Suggested top 5 quick wins (UX gap ≥ 4, Effort ≤ S/M)

If the user wants a "where do we start" list:

1. **A1 — Chat export (Markdown / JSON)** — pure frontend, biggest
   UX delta for power users, zero backend change.
2. **A2 — Streaming partial transcripts** — small daily papercut;
   `PartialTranscript` is already on the wire, just unused.
3. **B1 — Reminders** — fills the canonical voice-assistant
   expectation; WebSocket plumbing already exists.
4. **A7 — Light theme** — accessibility win, low risk, follows the
   existing `user_preferences` pattern.
5. **B9 — Timers** — 30-line agent + WebSocket reuse; rides on
   the same notification path as B1.

## Open questions

- **B2 (memory)**: should the LLM *auto-extract* memories from
  chat ("it sounds like your doctor's name is Martin — remember?"),
  or only on explicit user request? Both have privacy implications
  and need a UX choice.
- **B4 (email)**: do we want a single IMAP/SMTP agent pair, or two
  separate `ServiceDef`s with provider-specific UIs (Gmail OAuth vs
  generic IMAP)?
- **A3 / C5 (edit + fork)**: needs a server-side per-session store.
  SQLite or Postgres — both compile today (`docs/architecture.md`
  §"Storage engine"). Confirm direction before A3 lands.
- **B11 (image gen)**: out of v1 scope? Or fold the vision side
  in early and revisit generation later?
- **A11 (PWA)**: is the operator OK with shipping a service
  worker, or do they want a strict "no SW" footprint?