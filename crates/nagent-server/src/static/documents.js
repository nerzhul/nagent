// nagent — Discussion-mode Documents panel.
//
// Owns the `/v1/documents*` sidebar panel:
// - lists uploaded documents for the active chat session
// - uploads new files via `+ Upload`, drag/drop, or `Ctrl+V`
// - deletes documents via the row `×` button
// - refreshes the list on session switch (callers re-invoke
//   `documents.refresh()` after `switchToSession()` / `newSession()`)
//
// Every request to `/v1/documents*` carries the
// `X-Chat-Session-Id` header so the server can scope rows to the
// active session. The same header is also sent on
// `/v1/chat/completions` so the LLM's `read_document` tool can
// find the right file.
//
// The header value is **server-bound** (SEV 2 fix): the browser
// calls `POST /v1/chat/session` on boot to mint a UUID + bind it
// to the authenticated user, then reuses it on every request. A
// previously-client-minted UUID would let one user forge the
// header to reach another user's docs. The mint / refresh
// helpers live in `chat.js` (general-purpose module); we import
// `getServerSessionId()` here so every upload / list / delete
// reads the current value lazily.
//
// When `[documents].enabled = false` at boot, the server injects
// `window.nagentConfig = { documentsEnabled: false }` into
// `index.html`. `initDocumentsPanel()` then bails out and removes
// the panel from the DOM so the user does not see an upload UI
// that 404s on the first request.

import { getServerSessionId, refreshServerSessionId } from "/static/chat.js";

const DOCUMENTS_PATH = "/v1/documents";
const CHAT_SESSION_HEADER = "x-chat-session-id";
// Named exports so `composer-attachments.js` can reuse the same caps
// without re-declaring. The values intentionally mirror
// `[documents].max_file_size_bytes` and the `extract.rs` allow-list
// on the Rust side; keep in sync if either is ever made operator-
// configurable beyond a build-time default.
export const MAX_FILE_BYTES = 20 * 1024 * 1024;
export const ACCEPTED_EXTENSIONS = ["txt", "pdf", "md", "log"];

// Documents module — every call site queries the DOM
// directly via `document.getElementById(id)`. A module-level
// `const el = …` was tried first but tripped a TDZ hazard
// under the cyclic import (chat.js ↔ documents.js): chat.js's
// eager `rehydrateAfterMount()` reaches this module via the
// exported `refresh` BEFORE the `const el = …` line has run.
// The bare `document.getElementById` call is a single
// identifier (`document`) which is part of the global object
// and is therefore *always* initialised — no TDZ possible.
// The `document.getElementById` lookup is also naturally
// post-mount-remount-friendly (a fresh `#documents-panel`
// after a logout → login cycle is found without any extra
// wiring).

// Read the documents-enabled flag from the page-level feature
// registry (populated by `chat.js` after fetching `/api/features`).
// We keep the legacy `<script type="application/json"
// id="nagent-config">` block as a fallback for builds that ship
// without the endpoint (e.g. a third-party embedding that
// injects its own HTML); the JS still falls back to that block
// when `window.__nagentFeatures` is undefined.
function readNagentConfig() {
  // Prefer the live feature registry when present. We don't
  // import the helper from chat.js (the cyclic import is
  // already ugly enough) — chat.js exports `feature` /
  // `subscribeFeatures` via `window.__nagentFeatures` so the
  // documents module just reads it.
  if (typeof window !== "undefined") {
    const live = window.__nagentFeatures;
    if (live && typeof live === "object") {
      return { documentsEnabled: Boolean(live.documents) };
    }
  }
  // Legacy fallback: a `<script type="application/json"
  // id="nagent-config">` block injected by older server
  // builds. We do NOT actively request this — the JSON block
  // is only there for back-compat with builds that pre-date
  // the `/api/features` endpoint.
  if (typeof document !== "undefined") {
    const el = document.getElementById("nagent-config");
    if (el) {
      try {
        const parsed = JSON.parse(el.textContent || "{}");
        if (parsed && typeof parsed === "object") return parsed;
      } catch (e) {
        // Malformed payload — fall through.
      }
    }
  }
  return { documentsEnabled: false };
}

export function initDocumentsPanel() {
  // The `<details id="documents-panel">` lives inside
  // `<main id="view-discussion">` which itself is inside
  // `<template id="app-shell-template">` — auth.js clones the
  // template into `#app-root` after the `/api/me` probe and
  // dispatches an `app-shell-mounted` event. Bail out if the
  // panel is not yet in the DOM; the listener below retries on
  // mount.
  const panel = document.getElementById("documents-panel");
  if (!panel) {
    // Panel not in the template — silently bail.
    return;
  }
  // Respect the server's runtime flag (`/api/features`
  // returns `documents: true` when `[documents].enabled = true`
  // AND the auth DB is reachable). When the flag flips false, we
  // hide the panel via CSS rather than removing it from the
  // DOM — keeping the element in the tree lets the panel be
  // re-shown without a DOM rebuild if the flag ever flips back
  // to true (the panel already has all its descendants wired).
  if (!isDocumentsEnabled()) {
    panel.hidden = true;
    return;
  }
  panel.hidden = false;
  wireDocumentsPanelEvents();
}

// One-time wiring of the panel's listeners. Called every time
// the panel transitions from hidden to visible. The duplicate-
// listener guard ensures we never attach the same `click` /
// `drop` handler twice if `initDocumentsPanel` runs again.
let _panelWired = false;
function wireDocumentsPanelEvents() {
  if (_panelWired) return;
  const panel = document.getElementById("documents-panel");
  const input = document.getElementById("documents-upload-input");
  const btn = document.getElementById("documents-upload-btn");
  if (!panel || !input || !btn) return;
  _panelWired = true;
  btn.addEventListener("click", () => input.click());
  input.addEventListener("change", () => {
    const files = Array.from(input.files || []);
    files.forEach((f) => uploadFile(f));
    input.value = "";
  });
  // Drag-and-drop on the panel itself. The sidebar is the
  // natural drop target — uploading by clicking the button is
  // the secondary path.
  panel.addEventListener("dragover", (e) => {
    e.preventDefault();
    panel.classList.add("is-dragover");
  });
  panel.addEventListener("dragleave", () => {
    panel.classList.remove("is-dragover");
  });
  panel.addEventListener("drop", (e) => {
    e.preventDefault();
    panel.classList.remove("is-dragover");
    const files = Array.from(e.dataTransfer?.files || []);
    files.forEach((f) => uploadFile(f));
  });

  // Paste handler on `window`. Skips when the focus is on an
  // editable element so we don't hijack text paste in the chat
  // input.
  window.addEventListener("paste", (e) => {
    const target = e.target;
    if (target && (target.tagName === "TEXTAREA" || target.tagName === "INPUT")) {
      // User is pasting text into the chat input — let the
      // default behaviour happen.
      return;
    }
    const items = Array.from(e.clipboardData?.files || []);
    if (items.length === 0) return;
    items.forEach((f) => uploadFile(f));
  });
}

/// Read the documents-enabled flag from the page-level feature
/// registry. Returns false when the registry is unreachable
/// (the endpoint 401'd, 404'd, or timed out) so the panel is
/// hidden by default — a safer default than "show" because
/// uploads against a 404'd endpoint would surface a confusing
/// toast.
function isDocumentsEnabled() {
  if (typeof window === "undefined") return false;
  // Prefer the function-form reader (`__nagentFeature`): it's a
  // closure over the live `nagentFeatures` binding in `chat.js`,
  // so it always returns the current flag value. The object form
  // (`__nagentFeatures`) is also live (chat.js installs it as a
  // getter) but the function form is the canonical reader and
  // also avoids one DOM-equivalent lookup per call.
  if (typeof window.__nagentFeature === "function") {
    return Boolean(window.__nagentFeature("documents"));
  }
  const live = window.__nagentFeatures;
  if (live && typeof live === "object") {
    return Boolean(live.documents);
  }
  // Legacy fallback for builds that pre-date `/api/features`:
  // a `<script type="application/json" id="nagent-config">`
  // block injected by older server versions. We do NOT actively
  // request it — the JSON block is only there for back-compat.
  const cfg = readNagentConfig();
  return cfg ? cfg.documentsEnabled !== false : false;
}

// The first `initDocumentsPanel()` call (from chat.js's boot
// sequence) runs BEFORE `auth.js` has mounted the app shell —
// the `#documents-panel` element lives inside the shell
// template and is therefore null. Retry once the shell mounts
// (and after each subsequent mount, e.g. after a logout →
// login cycle).
//
// We also listen for feature-flag changes: when chat.js
// completes the `/api/features` fetch and the `documents` flag
// flips from `false` to `true`, we unhide the panel and (re)-
// wire the listeners. The `hidden` attribute is toggled by
// `initDocumentsPanel` so we never duplicate work.
window.addEventListener("app-shell-mounted", () => {
  initDocumentsPanel();
});
if (typeof window !== "undefined" && window.__nagentSubscribeFeatures) {
  window.__nagentSubscribeFeatures((features) => {
    // After the feature flag flip, re-evaluate the panel state.
    // The listener fires both for the initial fetch result and
    // for any future mutation; `initDocumentsPanel` is
    // idempotent (the `_panelWired` guard ensures we don't
    // double-bind listeners).
    initDocumentsPanel();
  });
}

// The first `initDocumentsPanel()` call (from chat.js's boot
// sequence) runs BEFORE `auth.js` has mounted the app shell —
// the `#documents-panel` element lives inside the shell
// template and is therefore null. Retry once the shell mounts
// (and after each subsequent mount, e.g. after a logout →
// login cycle). The early bail-out above guarantees the
// listener wiring happens at most once per mount.
//
// Re-querying the DOM dodges a TDZ hazard when chat.js's eager
// `rehydrateAfterMount` fires before documents.js's
// module-level `let`s would have been initialised under
// cyclic-import ordering. See `refresh()` for details.
//
// (The mount listener above also handles this case — when the
// shell template is cloned and `documents-panel` appears, the
// listener fires `initDocumentsPanel()` which wires the panel.)

// `id` is the chat-tab id (used by the UI sidebar). We don't
// need it directly any more — the server-bound session id is
// fetched lazily from `getServerSessionId()` on every request.
// We keep this setter as a hook so chat.js can call it on tab
// switch / new-session and we trigger a fresh list.
export function setSessionId(_id) {
  refresh();
}

// Refresh the doc list for the active session. Called after
// every successful upload / delete + on session switch.
//
// We re-query the DOM on every `refresh()` instead of caching
// the elements in a `let`. The module-level `let … = null;`
// declarations can sit in TDZ for a brief window when chat.js's
// eager `rehydrateAfterMount` fires before documents.js's
// module body has finished evaluating (chat.js imports
// documents.js, so the module is loaded before chat.js's body
// runs — but ES modules have subtle initialization ordering
// under cyclic imports). Re-querying the DOM is cheap, removes
// the TDZ hazard, and naturally handles post-mount re-mounts.
export async function refresh() {
  // Gate on the feature flag FIRST. The panel element may be
  // hidden in the DOM but still present (we keep it in the
  // tree so a flag flip from false→true unhides without a DOM
  // rebuild), so checking `panel` alone is not enough — we
  // must check the server-side flag too. Skipping the fetch
  // here is what avoids the spurious `/v1/documents` round-trip
  // when `[documents].enabled = false`.
  if (!isDocumentsEnabled()) return;
  const panel = document.getElementById("documents-panel");
  if (!panel) return;
  const sid = await getServerSessionId();
  if (!sid) {
    renderEmpty("Waiting for chat session binding…");
    return;
  }
  try {
    const resp = await fetch(DOCUMENTS_PATH, {
      headers: { [CHAT_SESSION_HEADER]: sid },
      cache: "no-store",
    });
    if (resp.status === 403 || resp.status === 503) {
      // SEV 2 fix: binding lost — re-mint and retry once.
      await refreshServerSessionId();
      return refresh();
    }
    if (!resp.ok) {
      renderEmpty("List failed (server error).");
      return;
    }
    const body = await resp.json();
    const docs = Array.isArray(body?.data) ? body.data : [];
    renderList(docs);
  } catch (e) {
    console.warn("documents list failed:", e);
    renderEmpty("List failed (network error).");
  }
}

// Send one file to the server. Accepts txt/pdf/md/log. Anything
// else is rejected client-side so a wrong type never wastes a
// round trip.
//
// Implementation note: the actual `POST /v1/documents` fetch lives
// in the exported `uploadFileInBackground` helper below, so the
// composer's drag-and-drop path shares the same code path. CSRF
// is sent via `window.nagentAuth?.csrfHeaders()` (bearer-auth
// callers don't have it, the spread silently drops the absent
// key) — without it the route's `check_csrf` gate returns 403
// under cookie auth.
async function uploadFile(file) {
  if (!isDocumentsEnabled()) {
    showToast("Documents feature is disabled on this server.", "error");
    return;
  }
  const ext = (file.name.split(".").pop() || "").toLowerCase();
  if (!ACCEPTED_EXTENSIONS.includes(ext)) {
    showToast(`Unsupported file type: .${ext}`, "error");
    return;
  }
  if (file.size > MAX_FILE_BYTES) {
    showToast(`File too large (max ${Math.round(MAX_FILE_BYTES / 1024 / 1024)} MB).`, "error");
    return;
  }
  try {
    const created = await uploadFileInBackground(file);
    prependRow(created);
    showToast(`Uploaded ${file.name}.`, "info");
  } catch (e) {
    showToast(`Upload failed: ${e.message}`, "error");
  }
}

/// POST one file to `/v1/documents` and return the parsed
/// response body. Throws on any non-2xx response or network
/// failure. Used by both the sidebar `uploadFile` path and the
/// composer drag-and-drop module.
///
/// `sessionId` is optional; when omitted, the helper resolves
/// the server-bound chat session id lazily via
/// `getServerSessionId()`. The two call-sites both go through
/// the same `/v1/chat/session` mint, so the binding check on
/// the server side is identical.
export async function uploadFileInBackground(file, { sessionId } = {}) {
  const sid = sessionId ?? (await getServerSessionId());
  if (!sid) throw new Error("No chat session bound");
  const form = new FormData();
  form.append("file", file, file.name);
  const headers = {
    [CHAT_SESSION_HEADER]: sid,
    ...(window.nagentAuth?.csrfHeaders?.() ?? {}),
  };
  const doFetch = () => fetch(DOCUMENTS_PATH, {
    method: "POST",
    headers,
    body: form,
  });
  let resp = await doFetch();
  if (resp.status === 403 || resp.status === 503) {
    // SEV 2 fix: binding lost — re-mint and retry once.
    await refreshServerSessionId();
    resp = await doFetch();
  }
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(friendlyUploadError(resp, text));
  }
  return await resp.json();
}

/// Translate an upload failure into a message the user can act
/// on. The default fallback (`upload failed: <status> <body>`)
/// is honest but rarely useful — for the two common size-rejection
/// paths we recognise the verbatim server body and surface the actual
/// upload cap instead. Detected patterns:
///
/// - 400 + axum's `MultipartError` body — fires when
///   `DefaultBodyLimit` chops the multipart stream. The handler
///   never saw the bytes; the actionable advice is the upload
///   cap, not the parser message.
/// - 413 + `file too large` — fires when the handler enforces
///   `[documents].max_file_size_bytes` and the user-facing
///   `FileTooLarge { got, max }` text is echoed verbatim.
export function friendlyUploadError(resp, rawText) {
  const text = rawText || "";
  const limitMb = Math.round(MAX_FILE_BYTES / 1024 / 1024);
  if (
    resp.status === 400 &&
    /Error parsing `multipart\/form-data` request/.test(text)
  ) {
    return `file exceeds the server upload limit (max ${limitMb} MB).`;
  }
  if (resp.status === 413 && /file too large/i.test(text)) {
    return `file too large (max ${limitMb} MB).`;
  }
  return `upload failed: ${resp.status} ${text.slice(0, 200)}`;
}

async function deleteDoc(id) {
  // Same gate as `refresh()` / `uploadFile()`. A user could in
  // theory hold a stale row in the panel from a previous
  // session where `documents` was enabled; reject silently
  // rather than hitting the 404.
  if (!isDocumentsEnabled()) return;
  const sid = await getServerSessionId();
  if (!sid) return;
  try {
    const resp = await fetch(`${DOCUMENTS_PATH}/${encodeURIComponent(id)}`, {
      method: "DELETE",
      headers: {
        [CHAT_SESSION_HEADER]: sid,
        // The DELETE route is a state-changing request and is
        // gated by `check_csrf` in `auth/middleware.rs:107-125`.
        // The browser SPA must carry the per-session token;
        // bearer-auth callers are unaffected (the spread below
        // is a no-op when `csrfHeaders()` returns `undefined`).
        // Without this header, every DELETE 403s on a session
        // authenticated via cookie and the UI's retry loop
        // spams the route.
        ...(window.nagentAuth?.csrfHeaders?.() ?? {}),
      },
    });
    if (resp.status === 403 || resp.status === 503) {
      await refreshServerSessionId();
      return deleteDoc(id);
    }
    if (!resp.ok && resp.status !== 204) {
      showToast(`Delete failed: ${resp.status}`, "error");
      return;
    }
    // Drop the row from the DOM.
    const row = document.getElementById("documents-list")?.querySelector(
      `[data-doc-id="${CSS.escape(id)}"]`,
    );
    if (row) {
      row.classList.add("is-removing");
      setTimeout(() => row.remove(), 150);
    }
    refreshEmpty();
  } catch (e) {
    showToast(`Delete failed: ${e.message}`, "error");
  }
}

function renderList(docs) {
  const list = document.getElementById("documents-list");
  if (!list) return;
  list.innerHTML = "";
  for (const doc of docs) {
    list.appendChild(buildRow(doc));
  }
  refreshEmpty();
}

function prependRow(doc) {
  const list = document.getElementById("documents-list");
  if (!list) return;
  // If we were in the empty state, clear it.
  const empty = document.getElementById("documents-empty");
  empty?.remove();
  list.prepend(buildRow(doc));
  refreshEmpty();
}

function buildRow(doc) {
  const li = document.createElement("li");
  li.className = "documents-item";
  li.dataset.docId = doc.id;

  // Filename with a middle-ellipsis so the extension is
  // always visible on long names. The full name stays in the
  // `title` attribute for hover, and the underlying text node
  // is also the original `doc.name` so screen-readers and
  // copy-paste see the real filename.
  const name = document.createElement("span");
  name.className = "documents-item-name";
  name.textContent = doc.name;
  name.title = doc.name;

  const badge = document.createElement("span");
  badge.className = `documents-item-mime documents-item-mime--${(doc.mime || "").split("/").pop() || "bin"}`;
  badge.textContent = badgeFromMime(doc.mime);

  const meta = document.createElement("span");
  meta.className = "documents-item-meta";
  const sizeStr = formatSize(doc.size_bytes);
  if (doc.page_count) {
    meta.textContent = `${sizeStr} · ${doc.page_count} pages`;
  } else {
    meta.textContent = sizeStr;
  }

  const del = document.createElement("button");
  del.type = "button";
  del.className = "documents-item-delete";
  del.setAttribute("aria-label", `Delete document ${doc.name}`);
  del.title = "Delete";
  del.textContent = "×";
  del.addEventListener("click", () => {
    if (!confirm(`Delete “${doc.name}”?`)) return;
    deleteDoc(doc.id);
  });

  li.appendChild(name);
  li.appendChild(badge);
  li.appendChild(meta);
  li.appendChild(del);
  // After the row is in the DOM, swap the visible label for a
  // middle-ellipsised variant if the layout container is too
  // narrow to fit the full name + extension. The visible
  // string becomes "head…tail" with the tail still ending in
  // the original extension; the underlying textContent +
  // `title` attribute stay as the real `doc.name` so a
  // full-string copy (right-click → "Copy" in DevTools) still
  // resolves to the original.
  requestAnimationFrame(() => truncateNameIfOverflowing(name));
  return li;
}

/// Replace the visible text of `.documents-item-name` with a
/// middle-ellipsised variant when its scrollWidth exceeds the
/// available clientWidth. The extension (everything after the
/// last `.` in the basename) is preserved at the tail. No-op
/// when the name fits; the underlying `textContent` keeps the
/// original filename so DevTools copy / screen-reader / title
/// hover are unaffected.
function truncateNameIfOverflowing(nameEl) {
  if (!nameEl || !nameEl.isConnected) return;
  const full = nameEl.textContent;
  if (!full) return;
  if (nameEl.scrollWidth <= nameEl.clientWidth + 1) return;
  const dot = full.lastIndexOf(".");
  // Files without a `.` (e.g. `Makefile`) just get a plain
  // tail-ellipsis from the browser; we don't synthesise a fake
  // extension.
  const ext = dot > 0 ? full.slice(dot) : "";
  const base = dot > 0 ? full.slice(0, dot) : full;
  // Iteratively shrink the head until the rendered string
  // fits. A binary search would be cheaper; 4 iterations are
  // plenty for any realistic filename and keep the code
  // obviously correct.
  let headLen = Math.max(1, Math.floor(base.length / 2));
  for (let i = 0; i < 8; i++) {
    const candidate = `${base.slice(0, headLen)}…${ext}`;
    nameEl.textContent = candidate;
    if (nameEl.scrollWidth <= nameEl.clientWidth + 1) return;
    headLen = Math.max(1, Math.floor(headLen * 0.7));
  }
}

function renderEmpty(message) {
  const list = document.getElementById("documents-list");
  if (!list) return;
  list.innerHTML = "";
  renderEmptyHint(message || "Drag a PDF or text file here, or paste one.");
}

function renderEmptyHint(text) {
  const empty = document.getElementById("documents-empty");
  const list = document.getElementById("documents-list");
  if (!empty) return;
  empty.textContent = text;
  if (!empty.isConnected) {
    list?.after(empty);
  }
}

function refreshEmpty() {
  const list = document.getElementById("documents-list");
  const empty = document.getElementById("documents-empty");
  if (!list || !empty) return;
  const hasRows = list.children.length > 0;
  if (hasRows) {
    empty.remove();
  } else if (!empty.isConnected) {
    list.after(empty);
  }
}

function badgeFromMime(mime) {
  if (!mime) return "?";
  if (mime === "text/plain") return "TXT";
  if (mime === "application/pdf") return "PDF";
  return mime.split("/").pop().toUpperCase();
}

function formatSize(bytes) {
  const n = Number(bytes) || 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

function showToast(message, level) {
  // Minimal toast: log to console for now. A proper toast system
  // is out of scope for v1 (the chat-sessions sidebar already
  // owns a transient message area that could be reused in a
  // follow-up PR).
  if (level === "error") {
    console.warn("[documents]", message);
  } else {
    console.info("[documents]", message);
  }
  // Best-effort: also surface in the chat-status pill (when it
  // exists) so the user actually sees the message.
  const status = document.getElementById("chat-status");
  if (status && level === "error") {
    const prev = status.textContent;
    status.textContent = message;
    setTimeout(() => {
      if (status.textContent === message) status.textContent = prev;
    }, 4000);
  }
}