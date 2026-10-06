# Plan — A4: Drag-and-drop file upload in the chat composer

Implements proposal **A4** from `1791234616248-ux-feature-agent-catalog.md`:
a file-picker button + drag-and-drop target on `#chat-form` that uploads
PDF/text/log/markdown files via the existing `POST /v1/documents` endpoint
and surfaces them as inline attachment chips on the user bubble.

The Documents backend (storage, `read_document` agent, quota + size + MIME
guards) is already complete. This plan only adds the **frontend affordance**
and the **LLM-side hint** that tells the model "the user just uploaded X".

## 0. Goal & non-goals

**Goal** — when `[documents].enabled = true` (read from `/api/features`),
the Discussion chat composer offers:
1. A paperclip button next to `#chat-record-btn` that opens the OS file
   picker (accepting `.pdf,.txt,.md,.log`, multi-select).
2. `#chat-form` (and the area immediately above it) as a drop target with
   a `--drag-over` highlight, identical look to the existing Documents
   sidebar panel.
3. Inline chip(s) under the textarea showing each staged file, with an `×`
   to remove a staged file before send.
4. Upload happens on add (background), so the user can keep typing and
   the chips re-render with the server-returned id + size once the upload
   completes.
5. On send, attachments ride along: a hidden `[Attached: …]` hint block
   goes into the LLM request (mirroring the existing
   `maybeBuildLocationBlock()` pattern at `chat.js:2869`), the chips
   render as `.chat-message-attachments` rows inside the user bubble,
   and the chip list is cleared.
6. The Documents panel and the new composer attachment UI share the same
   per-session document store, so docs uploaded from either surface
   appear in both.

**Non-goals** —

- No new server route, no new agent, no schema migration.
- No `documents` agent-side auto-injection outside the turn the user
  explicitly attached the file to (i.e. previous turns don't carry
  attachment hints forward — the LLM still has `read_document` available
  for "re-look at doc X" cases).
- No paste-to-upload (`Ctrl+V` inside the textarea). The Documents
  sidebar already supports paste; deferring composer paste avoids clashing
  with browser-native textarea paste and matches the original catalog
  scope (drag-and-drop + button only). Marked as a follow-up in the
  catalog note.
- No new icon SVG library — reuse the existing paperclip inline SVG
  pattern (no `<img>`, no external font).
- No light-theme token additions beyond what `.documents-item-mime--*`
  already declares.

## 1. Architecture & data flow

```
                ┌──────────────────────────────────────────────┐
                │  composer-attachments.js (new module)        │
                │                                              │
 user drops /  │  - isEnabled() ──→ feature("documents")       │
 picks file ───▶│  - stageFile(file) → pending[]               │
                │  - uploadPending() → POST /v1/documents      │
                │      headers: x-csrf-token, x-chat-session-id│
                │      body: multipart, field "file"           │
                │  - renderChip(summary) → DOM <li>            │
                │  - removePending(idx)                        │
                │                                              │
                │  exposes:                                    │
                │    window.__nagentAttachments                 │
                │      .stage(fileList)                         │
                │      .consumeOnSend() → hint + summaries      │
                │      .renderChips(containerEl)               │
                │      .subscribe(cb)                          │
                └──────────────────────────────────────────────┘
                              │ ▲                                       │
                              │ │                                     │
       ┌──────────────────────┘ └─────────────────────────┐           │
       │                                                    │           │
   chat.js                                            documents.js         │
   - submitUserTurn(text) → asks attachments for          - owns the same  │
     consumeOnSend() → builds ephemeral "Attached: …"     POST flow       │
     hint, passes to streamReply(sessionId, text, hint) - re-exports     │
   - appendBubble("user", text, { attachments }) →         uploadFile()   │
     renders chips inside the bubble                       for parity    │
   - hydrates history rows that carry `attachments`     - isDocuments    │
     → calls attachments.renderChips(bubbleEl)            Enabled()      │
```

The chip tray is a sibling of `#chat-form` (above it, below
`#chat-messages`). The drop zone is `#chat-form` itself (per
`catalog.md:90`). `#chat-input` does **not** become the drop target —
dropping on the textarea would lose the cursor position behaviour users
expect from text inputs.

## 2. Feature gating (the "toggle switch" rule)

`applyFeatureGates()` in `chat.js:3481-3494` is extended to:

```js
const showAttach = feature("documents");
toggleHidden(chat-attach-btn, !showAttach);        // paperclip button
toggleHidden(chat-attach-chips, !showAttach);      // tray wrapper
chat-form.classList.toggle("is-disabled", !showAttach && !hasPendingUploads);
```

Behaviour:
- **Toggle off (`documents.enabled = false`)** — the paperclip button is
  `hidden`, the chip tray is `hidden`, the drop-zone handlers bail at
  the top (`if (!isEnabled()) return;`). No regression for users on
  builds without the documents subsystem.
- **Toggle on after page load** — `subscribeFeatures(cb)` listener
  re-applies `applyFeatureGates()`; the button appears, drop zone
  becomes active, no reload needed.
- **Default-deny on `/api/features` failure** — `feature("documents")`
  returns `false` (per existing `DEFAULT_FEATURES` freeze at
  `chat.js:3367-3372`). Same fail-closed posture as the Documents panel.
- **Operator config sanity** — the server-side route is only mounted
  when `state.documents.is_some()` (`http/mod.rs:128-132`), so a request
  against a server with the toggle off 404s and the client surfaces a
  toast exactly like the sidebar's `uploadFile` does today
  (`documents.js:295-340`).

## 3. Files to change

| File | Change |
|---|---|
| `index.html:402-443` | Add chip tray `<ul id="chat-attach-chips" hidden>`, paperclip button `<button id="chat-attach-btn" type="button" hidden>`, hidden file input `<input id="chat-attach-input" type="file" accept=".pdf,.txt,.md,.log" multiple hidden>`. All three are siblings of `#chat-form`, inside `#app-shell-template` so they re-clone with the rest. |
| `static/composer-attachments.js` (new, ~200 lines) | Module described in §4. Loaded via `<script type="module" src="/static/composer-attachments.js" defer>` after `chat.js` in `index.html:629-725`. |
| `static/chat.js` | (a) `applyFeatureGates()` (line 3481) toggles the new button + tray. (b) `submitUserTurn()` (line 3237) accepts an optional `hint` and attachments, passes to `appendBubble` and `streamReply`. (c) `appendBubble("user", …)` (line 1017) renders `.chat-message-attachments` when given attachments. (d) `loadHistory` / `renderHistory` (line ~987) carries the `attachments` array through hydration. (e) New `chat.js` exports a tiny `onAttachmentsChanged(cb)` so the new module can re-render the tray when stage state changes. |
| `static/documents.js` | Export `uploadFileInBackground(file, onSuccess)` (refactor `uploadFile` so the existing sidebar upload still calls the new shared core). Add the same constant export surface (`MAX_FILE_BYTES`, `ACCEPTED_EXTENSIONS`) so `composer-attachments.js` imports them instead of re-declaring. |
| `static/style.css` | New section near `.chat-location-share` (line 2082): `.chat-attach-btn`, `.chat-attach-chips`, `.chat-attach-chip`, `.chat-attach-chip-mime--{pdf,txt,md,log}`, `.chat-form.is-dragover`, `.chat-message-attachments`. Reuse existing CSS variables (`--border`, `--bg-elev`, `--fg-mute`, `--accent`). Reuse `.documents-item-mime--*` colour tokens (declared `style.css:3045-3055`) for the badge colours. |
| `docs/ui_features.md` | Add §3.x sub-section "Discussion chat: file attachments" — mirrors the existing Documents sidebar description. (AGENTS.md §5 sync rule.) |
| `README.md:1047-1148` | Update the "Documents" paragraph to mention the new composer attach affordance ("drag onto the composer, the 📎 button, or via the sidebar"). (AGENTS.md §5 sync rule.) |

No Rust crate changes. No `Cargo.toml` changes.

## 4. Module spec — `static/composer-attachments.js`

### Public surface

```js
window.__nagentAttachments = {
  isEnabled(): bool,                         // feature("documents") && isMounted
  stage(fileList | File[]): Promise<void>,   // upload each, render chips
  consumeOnSend(): { hint: string, summaries: Array<DocSummary> },
  renderChips(containerEl): void,            // re-render after consume
  subscribe(cb: () => void): () => void,     // tray re-render listener
  reset(): void,                             // called on session switch
};
```

### Internal state

```js
const pending = [];  // Array<{ id?, file, status: "uploading"|"done"|"error", name, size, mime, error? }>
let mounted = false;
let trayEl = null;
const listeners = new Set();
```

### Wiring (mirrors `wireFormOnce` pattern at `chat.js:3311-3356`)

```js
let _wired = false;
function wireOnce() {
  if (_wired) return;
  const form = document.getElementById("chat-form");
  const btn  = document.getElementById("chat-attach-btn");
  const input = document.getElementById("chat-attach-input");
  const tray = document.getElementById("chat-attach-chips");
  if (!form || !btn || !input || !tray) return;       // not mounted yet
  _wired = true;
  trayEl = tray;
  mounted = true;

  btn.addEventListener("click", () => input.click());
  input.addEventListener("change", () => {
    const files = Array.from(input.files || []);
    files.forEach(stageFile);
    input.value = "";                                  // re-pick same file
  });

  // Drop zone: #chat-form + the tray itself.
  for (const region of [form, tray]) {
    region.addEventListener("dragover", (e) => {
      if (!isEnabled() || !e.dataTransfer?.types?.includes("Files")) return;
      e.preventDefault();
      region.classList.add("is-dragover");
    });
    region.addEventListener("dragleave", (e) => {
      // Only remove when leaving the region itself, not children.
      if (e.target === region) region.classList.remove("is-dragover");
    });
    region.addEventListener("drop", (e) => {
      region.classList.remove("is-dragover");
      if (!isEnabled()) return;
      const files = Array.from(e.dataTransfer?.files || []);
      if (!files.length) return;
      e.preventDefault();
      files.forEach(stageFile);
    });
  }

  // Disable while a turn is inflight (chat.js setStreamingUi).
  window.addEventListener("chat-stream-state", (e) => {
    const streaming = e.detail?.streaming === true;
    btn.toggleAttribute("disabled", streaming);
    tray.classList.toggle("is-locked", streaming);
  });
}

window.addEventListener("app-shell-mounted", () => {
  wireOnce();
  applyGate();
});
window.addEventListener("nagent-features-changed", applyGate);

function applyGate() {
  const btn = document.getElementById("chat-attach-btn");
  const tray = document.getElementById("chat-attach-chips");
  if (!btn || !tray) return;
  const on = feature("documents");
  btn.toggleAttribute("hidden", !on);
  tray.toggleAttribute("hidden", !on || pending.length === 0);
}
```

### `stageFile(file)` (core of the upload)

Mirrors `documents.js:295-340`, deduplicated via the shared
`uploadFileInBackground` helper that this plan adds to `documents.js`:

```js
async function stageFile(file) {
  if (!isEnabled()) { toast("Documents disabled", "error"); return; }
  const ext = (file.name.split(".").pop() || "").toLowerCase();
  if (!ACCEPTED_EXTENSIONS.includes(ext)) { toast("Unsupported file type", "error"); return; }
  if (file.size > MAX_FILE_BYTES)           { toast("File too large", "error"); return; }
  const slot = { file, status: "uploading", name: file.name, size: file.size, mime: file.type || guess(ext) };
  pending.push(slot);
  notify();
  try {
    const sid = await getServerSessionId();
    if (!sid) throw new Error("No chat session bound");
    const doc = await uploadFileInBackground(file, { sessionId: sid });
    slot.id = doc.id; slot.status = "done"; slot.serverName = doc.name;
    // Update Documents panel cache so the sidebar reflects the new file.
    window.dispatchEvent(new CustomEvent("documents-changed"));
  } catch (e) {
    slot.status = "error"; slot.error = e.message || "Upload failed";
    toast(slot.error, "error");
  }
  notify();
}
```

### `consumeOnSend()`

Called from `submitUserTurn` after `appendBubble` but before
`streamReply`. Returns the LLM-side hint + the array to pass to
`appendBubble` for rendering.

```js
function consumeOnSend() {
  const ready = pending.filter((p) => p.status === "done" && p.id);
  const failed = pending.filter((p) => p.status === "error");
  // Keep `ready` cleared; keep `failed` so the user can retry by re-dropping.
  pending.length = 0;
  pending.push(...failed);
  notify();
  if (!ready.length) return { hint: "", summaries: [] };
  const lines = ready.map((p) =>
    `- ${p.serverName || p.name} (id=${p.id}, size=${p.size}B)`
  );
  const hint = [
    "[Attachments uploaded by the user this turn. Call `read_document` with `name=<id>` to read any you need.]",
    ...lines,
  ].join("\n");
  return { hint, summaries: ready.map((p) => ({
    id: p.id, name: p.serverName || p.name, mime: p.mime, size_bytes: p.size,
  })) };
}
```

### `renderChips(containerEl)`

Used by `chat.js` to render the chips inside a freshly-created user
bubble (and by `renderHistory` for hydration). Pure DOM, no event
listeners other than the per-chip `×` button.

```js
function renderChips(containerEl, summaries) {
  containerEl.innerHTML = "";
  if (!summaries?.length) return;
  const frag = document.createDocumentFragment();
  for (const s of summaries) {
    const li = document.createElement("li");
    li.className = "chat-attach-chip";
    li.dataset.docId = s.id;
    const mime = (s.mime || "").includes("pdf") ? "pdf"
              : (s.mime || "").includes("markdown") ? "md"
              : "txt";
    const badge = document.createElement("span");
    badge.className = `chat-attach-chip-mime chat-attach-chip-mime--${mime}`;
    badge.textContent = mime.toUpperCase();
    const label = document.createElement("span");
    label.className = "chat-attach-chip-name";
    label.textContent = s.name;
    const meta = document.createElement("span");
    meta.className = "chat-attach-chip-meta";
    meta.textContent = humanSize(s.size_bytes);
    li.append(badge, label, meta);
    frag.append(li);
  }
  containerEl.append(frag);
}
```

## 5. chat.js changes — minimal touch points

**`applyFeatureGates()` (line 3481-3494)** — append three lines:

```js
const attachBtn  = document.getElementById("chat-attach-btn");
const attachTray = document.getElementById("chat-attach-chips");
if (attachBtn)  attachBtn.toggleAttribute("hidden", !feature("documents"));
if (attachTray) attachTray.toggleAttribute("hidden", !feature("documents"));
```

**`submitUserTurn()` (line 3237-3242)** — change to:

```js
async function submitUserTurn(sessionId, text) {
  const trimmed = (text || "").trim();
  if (!trimmed) return;
  const { hint, summaries } = window.__nagentAttachments?.consumeOnSend() ?? { hint: "", summaries: [] };
  appendBubble("user", trimmed, { sessionId, attachments: summaries });
  await streamReply(sessionId, trimmed, { attachmentsHint: hint });
}
```

`streamReply` extends the LLM request the same way `maybeBuildLocationBlock()`
already injects location — a hidden system-side block prepended to the
user message, **not** persisted to `loadHistory` (the hint is ephemeral;
the attachments array in history is what drives bubble re-render on
hydration). The helper that builds the prompt array gains a second
optional argument and concatenates the hint block identically to the
location one (`chat.js:2869`).

**`appendBubble("user", …)` (line 1017-1103)** — when `opts.attachments`
is non-empty:

```js
if (opts.attachments?.length) {
  div.classList.add("chat-message--has-attachments");
  const tray = document.createElement("ul");
  tray.className = "chat-message-attachments";
  div.appendChild(tray);
  window.__nagentAttachments.renderChips(tray, opts.attachments);
}
```

(`div.textContent = text` at line ~1048 still sets the user's text first;
chips are appended after, so the text remains pure plaintext per the
"trust user input" invariant at `chat.js:1046-1050`.)

**History hydration (`renderHistory` line ~987)** — when loading a
historical message that carries `attachments`, recreate the chip tray
the same way:

```js
if (msg.attachments?.length) {
  div.classList.add("chat-message--has-attachments");
  const tray = document.createElement("ul");
  tray.className = "chat-message-attachments";
  div.appendChild(tray);
  window.__nagentAttachments.renderChips(tray, msg.attachments);
}
```

**History write (`loadHistory` / `saveHistory` line ~990)** — extend the
pushed record:

```js
history.push({
  role: "user",
  content: text,
  ts: Date.now(),
  model,
  attachments: opts.attachments?.length ? opts.attachments : undefined,
});
```

**`chat-stream-state` event** — wherever `setStreamingUi(true|false)`
fires today (around `chat.js:3231`), emit:

```js
window.dispatchEvent(new CustomEvent("chat-stream-state", {
  detail: { streaming: streaming === true }
}));
```

so the new module can grey the attach button during generation (prevents
a race where the user drops a file mid-stream and `consumeOnSend` is
called against a stale turn).

## 6. documents.js changes — extract the shared upload

Refactor `uploadFile(file)` at `documents.js:295-340` so the inner
`fetch(POST /v1/documents)` block becomes a reusable helper:

```js
export async function uploadFileInBackground(file, { sessionId, csrfHeaders }) {
  const form = new FormData();
  form.append("file", file, file.name);
  const headers = {
    [CHAT_SESSION_HEADER]: sessionId,
    ...(csrfHeaders ?? window.nagentAuth?.csrfHeaders() ?? {}),
  };
  const doFetch = () => fetch(DOCUMENTS_PATH, { method: "POST", headers, body: form });
  let resp = await doFetch();
  if (resp.status === 403 || resp.status === 503) {
    await refreshServerSessionId();
    resp = await doFetch();
  }
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`upload failed: ${resp.status} ${text.slice(0, 200)}`);
  }
  return await resp.json();
}
```

The existing `uploadFile` (sidebar path) is rewritten as a 6-line
wrapper that calls this helper, catches errors, and toasts — same
behaviour as today. `composer-attachments.js` imports
`uploadFileInBackground` and the `MAX_FILE_BYTES` / `ACCEPTED_EXTENSIONS`
constants (currently declared at `documents.js:35-36`); both move to
named `export`s.

## 7. CSS spec — `static/style.css` (additions only)

```css
/* Document attachment button — paperclip, mirrors .chat-location-share */
.chat-attach-btn {
  flex: 0 0 auto; height: 2.6rem; width: 2.6rem;
  border-radius: 10px; border: 1px solid var(--border);
  background: transparent; color: var(--fg-mute);
  display: inline-flex; align-items: center; justify-content: center;
  cursor: pointer; transition: background 120ms, color 120ms;
}
.chat-attach-btn:hover { background: var(--bg-elev); color: var(--fg); }
.chat-attach-btn[disabled] { opacity: 0.4; cursor: not-allowed; }

/* Chip tray (between #chat-messages and #chat-form) */
.chat-attach-chips {
  list-style: none; padding: 0.4rem 0; margin: 0 0 0.4rem;
  display: flex; flex-wrap: wrap; gap: 0.4rem;
}
.chat-attach-chips[hidden] { display: none; }
.chat-attach-chips.is-locked { opacity: 0.5; pointer-events: none; }

/* Per-file chip */
.chat-attach-chip {
  display: inline-flex; align-items: center; gap: 0.5rem;
  padding: 0.35rem 0.6rem; border: 1px solid var(--border);
  border-radius: 999px; background: var(--bg-elev);
  font-size: 0.85rem; color: var(--fg);
}
.chat-attach-chip-mime {
  font-size: 0.7rem; padding: 0.1rem 0.4rem;
  border-radius: 4px; font-weight: 600; letter-spacing: 0.05em;
}
.chat-attach-chip-mime--pdf { background: #b91c1c; color: #fff; }
.chat-attach-chip-mime--txt { background: #1d4ed8; color: #fff; }
.chat-attach-chip-mime--md  { background: #047857; color: #fff; }
.chat-attach-chip-mime--log { background: #6b7280; color: #fff; }
.chat-attach-chip-name { color: var(--fg); }
.chat-attach-chip-meta { color: var(--fg-mute); font-size: 0.75rem; }
.chat-attach-chip-remove {
  border: none; background: transparent; color: var(--fg-mute);
  cursor: pointer; font-size: 1rem; line-height: 1;
}
.chat-attach-chip-remove:hover { color: var(--accent); }

/* Drop-zone highlight — mirrors .documents-panel .is-dragover */
#chat-form.is-dragover,
.chat-attach-chips.is-dragover {
  outline: 2px dashed var(--accent); outline-offset: 4px;
  background: color-mix(in srgb, var(--accent) 6%, transparent);
}

/* User bubble with attachments */
.chat-message--has-attachments .chat-message-attachments {
  list-style: none; padding: 0.4rem 0 0; margin: 0;
  display: flex; flex-wrap: wrap; gap: 0.4rem;
}
```

Reuses `.documents-item-mime--pdf|txt` colour tokens that already exist
at `style.css:3045-3055`. No new colour variables.

## 8. Edge cases & failure modes

| Scenario | Behaviour |
|---|---|
| `[documents].enabled = false` | Paperclip hidden, drop handlers bail at `if (!isEnabled()) return;`. Verified at `applyFeatureGates()` + `isEnabled()` runtime check. |
| `/api/features` fetch fails | Default-deny (per `DEFAULT_FEATURES` freeze); button stays hidden. |
| Drop while streaming (turn inflight) | Button is `disabled`, tray has `.is-locked`; drop zone still accepts drops but they queue and only upload after the turn finishes. (We accept the file in the slot but skip the actual `uploadFileInBackground` call until the `chat-stream-state` event with `streaming:false` fires.) |
| Drop unsupported file | Toast "Unsupported file type"; no chip added. |
| Drop oversize file | Toast "File too large"; no chip added. |
| Per-session quota exceeded (`429`) | Toast message returned by server; chip stays in `error` state and shows the server's message; user can `×` remove. |
| Upload 5xx mid-session | Same as 429 — chip turns red, user retries by re-dropping. |
| User sends before all uploads finish | `submitUserTurn` waits for `consumeOnSend` to be called; `consumeOnSend` filters by `status === "done"`. Pending uploads remain on screen with a spinner-style `chat-attach-chip--uploading` class so the user can either wait, `×` cancel, or send the partial set. (Documented behaviour: partial sends are allowed; the LLM just doesn't see the not-yet-uploaded file.) |
| Session switch with pending uploads | `reset()` clears the pending array (uploads in flight are aborted via `AbortController`); chips disappear. |
| History hydration after reload | Messages with `attachments` re-render chips via `renderHistory` path. Chips on historical bubbles are display-only (no `×` remove, no click handlers). |
| Browser without File API (`<IE11`) | Module guards `if (!window.File || !window.FormData) return;` at the top of `wireOnce`. |
| Drag image preview / drag text content | Drop handler checks `e.dataTransfer.types.includes("Files")` — text drops fall through to the browser default. |

## 9. Rollout

Single change, behind the existing `[documents]` server-side toggle.
No migration, no data backfill. Users on `[documents].enabled = false`
see no UI change.

`verify-chat-new-session.ts` smoke test (`scripts/`) references the
Documents panel + `/api/features` flow at lines 69-134 — page-load
sequence stays valid because the new module is purely additive (hidden
until enabled).

## 10. Validation (acceptance)

A reviewer or follow-up implementation agent should be able to check
these without re-deriving them:

1. **Toggle off** — `DOCS_ENABLED=false`, paperclip button is `hidden`,
   `#chat-attach-chips` is `hidden`, drag-drop on `#chat-form` is a no-op,
   no console errors.
2. **Toggle on** — `DOCS_ENABLED=true`, paperclip visible, button opens
   picker accepting `.pdf,.txt,.md,.log`. Dragging a `report.pdf` over
   `#chat-form` shows the dashed accent outline. Dropping it stages a
   chip with red `PDF` badge and `12.3 KB` meta.
3. **Quota** — with `max_docs_per_session = 2`, after 2 successful uploads
   a 3rd upload surfaces the server's quota toast; chip stays in error
   state.
4. **Size cap** — `max_file_size_bytes = 1000` server-side + a 2 KiB file
   drop surfaces "File too large" without a server round-trip (client
   guard at `MAX_FILE_BYTES`).
5. **Send flow** — with one staged `.pdf`, send "summarize". User bubble
   shows the chip *and* the typed text. The LLM request payload (visible
   in the browser devtools network tab) contains a hidden `[Attachments
   uploaded by the user this turn …]` block listing the doc id. After
   the assistant replies, the chips tray is empty.
6. **Streaming guard** — start a turn, then drop a file. Paperclip button
   has `[disabled]`, tray has `.is-locked`. Upload starts only after the
   turn ends (`chat-stream-state: streaming=false`).
7. **History hydration** — reload the page in the same browser context
   (same `localStorage`). The user message re-renders with the chip
   visible; the chip has no remove button (it's a historical record).
8. **Session switch** — drop a file on session A, switch to session B
   before sending. Pending array clears; chip tray disappears. Switch
   back to A — tray still empty (intentional; pending uploads are
   per-session).
9. **Cross-surface** — upload `a.pdf` via the sidebar Documents panel,
   then send a message that mentions "the pdf". `read_document` agent
   finds it. Then upload `b.pdf` via the new composer paperclip, send,
   same outcome. Documents panel reflects both rows.
10. **Lints** — `cargo fmt`, `cargo build --all-targets`, `cargo clippy`,
    `cargo audit` (no Rust changes, but the rule applies). No new JS
    lint pipeline today; reviewer should eyeball the new module against
    the existing `documents.js` patterns.
11. **Docs sync** — `docs/ui_features.md` and `README.md` Documents
    section updated per AGENTS.md §5.

## 11. Out of scope (explicit follow-ups)

- **Composer paste-to-upload** (`Ctrl+V` into `#chat-input`). Deferred —
  the existing Documents sidebar already supports paste, and paste into
  a textarea is a high-conflict surface (clipboard text vs clipboard
  image vs clipboard file). Recommend revisiting once a unified clipboard
  surface ships (the catalog at `1791234616248-ux-feature-agent-catalog.md`
  lists drag-and-drop + button as the A4 scope; paste is implicit in the
  sidebar only).
- **Image preview thumbnails**. Out of scope — only PDF/text/log/md are
  accepted; no `<img>` rendering needed.
- **Drag-from-OS-folder-tree into the bubble body** (vs the form). Out
  of scope — the form drop zone covers the typical UX.
- **Attachment chip click → preview pane**. Out of scope — clicking the
  chip is a no-op for now; the user reads via the LLM.
- **Auto-injection of "previous turn attachments" into later turns**.
  Out of scope — `read_document` already covers re-reads.
- **Markdown attachment syntax** (`[file](id)` parsed from user text).
  Out of scope — chips + LLM hint cover the catalog's UX gap (3).