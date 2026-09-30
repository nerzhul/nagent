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
const MAX_FILE_BYTES = 20 * 1024 * 1024; // mirror [documents].max_file_size_bytes default
const ACCEPTED_EXTENSIONS = ["txt", "pdf", "md", "log"];

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

// Read the runtime config block injected by `serve_index_with_config`.
// We use a `<script type="application/json" id="nagent-config">`
// block (not an inline `<script>`) so the page's
// Content-Security-Policy (`script-src 'self' 'wasm-unsafe-eval'`)
// does not block the injection. JSON-typed `<script>` blocks are
// not executed by the browser — they're read via the DOM.
// `window.nagentConfig` is still read as a fallback so a build
// that ships without the config block (e.g. a third-party
// embedding) keeps working.
function readNagentConfig() {
  const el =
    typeof document !== "undefined"
      ? document.getElementById("nagent-config")
      : null;
  if (el) {
    try {
      const parsed = JSON.parse(el.textContent || "{}");
      if (parsed && typeof parsed === "object") return parsed;
    } catch (e) {
      // Malformed payload — fall through to the window fallback.
    }
  }
  return typeof window !== "undefined" ? window.nagentConfig : undefined;
}

export function initDocumentsPanel() {
  // SEV: respect the server's runtime flag. The server injects
  // a `<script type="application/json" id="nagent-config">`
  // block on every `/` request, so a build that ships with the
  // documents routes NOT mounted (because `cfg.documents.enabled
  // = false`) does not show a panel that would fail every upload.
  // The block is missing on very old cached HTML; treat absence
  // as "enabled" so we don't regress a build whose server was
  // already upgraded. The server is the source of truth and will
  // 404 any unhandled request anyway.
  const cfg = readNagentConfig();
  const documentsEnabled = cfg ? cfg.documentsEnabled !== false : true;
  // The `<details id="documents-panel">` lives inside
  // `<main id="view-discussion">` which itself is inside
  // `<template id="app-shell-template">` — auth.js clones the
  // template into `#app-root` after the `/api/me` probe and
  // dispatches an `app-shell-mounted` event. Bail out if the
  // panel is not yet in the DOM; `app.js` will retry via the
  // event listener below.
  const panel = document.getElementById("documents-panel");
  if (!panel) {
    // Panel not in the template — silently bail.
    return;
  }
  if (!documentsEnabled) {
    // Remove the panel AND its list/empty children so drag-over
    // and paste handlers cannot find the element via a stale
    // reference.
    panel.remove();
    return;
  }
  const list = document.getElementById("documents-list");
  const empty = document.getElementById("documents-empty");
  const input = document.getElementById("documents-upload-input");
  const btn = document.getElementById("documents-upload-btn");

  btn?.addEventListener("click", () => {
    input?.click();
  });
  input?.addEventListener("change", () => {
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
window.addEventListener("app-shell-mounted", () => {
  if (!document.getElementById("documents-panel")) {
    initDocumentsPanel();
  }
});

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
async function uploadFile(file) {
  const ext = (file.name.split(".").pop() || "").toLowerCase();
  if (!ACCEPTED_EXTENSIONS.includes(ext)) {
    showToast(`Unsupported file type: .${ext}`, "error");
    return;
  }
  if (file.size > MAX_FILE_BYTES) {
    showToast(`File too large (max ${Math.round(MAX_FILE_BYTES / 1024 / 1024)} MB).`, "error");
    return;
  }
  const sid = await getServerSessionId();
  if (!sid) {
    showToast("No chat session bound — try refreshing the page.", "error");
    return;
  }
  const form = new FormData();
  form.append("file", file, file.name);
  try {
    const resp = await fetch(DOCUMENTS_PATH, {
      method: "POST",
      headers: { [CHAT_SESSION_HEADER]: sid },
      body: form,
    });
    if (resp.status === 403 || resp.status === 503) {
      await refreshServerSessionId();
      return uploadFile(file);
    }
    if (!resp.ok) {
      const text = await resp.text().catch(() => "");
      showToast(`Upload failed: ${resp.status} ${text || resp.statusText}`, "error");
      return;
    }
    const created = await resp.json();
    prependRow(created);
    showToast(`Uploaded ${file.name}.`, "info");
  } catch (e) {
    showToast(`Upload failed: ${e.message}`, "error");
  }
}

async function deleteDoc(id) {
  const sid = await getServerSessionId();
  if (!sid) return;
  try {
    const resp = await fetch(`${DOCUMENTS_PATH}/${encodeURIComponent(id)}`, {
      method: "DELETE",
      headers: { [CHAT_SESSION_HEADER]: sid },
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
  return li;
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