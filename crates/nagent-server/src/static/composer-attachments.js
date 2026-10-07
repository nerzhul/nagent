// nagent — Discussion-mode composer file attachments.
//
// Owns the paperclip / drag-and-drop affordance on `#chat-form`:
//   - opens the OS file picker when the paperclip is clicked;
//   - accepts `*.pdf`, `*.txt`, `*.md`, `*.log` dropped on the
//     form or on the chip tray itself;
//   - uploads each file in the background via the same
//     `POST /v1/documents` endpoint the sidebar Documents panel
//     uses, then renders a per-file chip in the tray;
//   - on submit, returns a `consumeOnSend()` payload carrying an
//     LLM-side system block (listing each staged document id) and
//     a per-bubble attachments array (for chip re-render on
//     history hydration).
//
// Per-session isolation: the module clears its pending slots on
// every `app-shell-mounted` event (a fresh login re-clones the
// shell template), and exposes a `reset()` hook so `chat.js` can
// drop pending uploads when the user switches sessions mid-stage.
//
// Cross-surface parity: a successful composer upload triggers
// `Documents.refresh()` so the sidebar list reflects the new row;
// the chip tray and the sidebar share the per-session document
// store on the server side.

import {
  uploadFileInBackground,
  MAX_FILE_BYTES,
  ACCEPTED_EXTENSIONS,
} from "/static/documents.js";
import * as Documents from "/static/documents.js";
import { getServerSessionId } from "/static/chat.js";

// `feature()` reads the page-level registry `chat.js` populates
// from `/api/features`. We prefer the function-form reader
// (`__nagentFeature`) because it is a closure over the live
// `nagentFeatures` binding — it always returns the current
// value. The `__nagentFeatures` object form is also live (see
// `chat.js:3657` which installs it as a getter) but the function
// form is the canonical reader and avoids one property lookup.
// Falling back to the object form keeps legacy builds working
// until they're upgraded.
const feature = () => {
  if (typeof window.__nagentFeature === "function") {
    return Boolean(window.__nagentFeature("documents"));
  }
  return Boolean(window.__nagentFeatures?.documents);
};

// `pending` is the source of truth for staged uploads. Each slot
// starts as `{ status: "uploading" }` and transitions to `"done"`
// (with `id` / `serverName` populated) or `"error"` (with
// `error` set). `consumeOnSend()` only ships the `done` slots;
// `reset()` / session switches drop everything in flight.
const pending = [];
let mounted = false;
let trayEl = null;
let streaming = false;

const isEnabled = () => feature() && mounted;

function humanSize(bytes) {
  if (!Number.isFinite(bytes)) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function guessMime(name) {
  const ext = (name.split(".").pop() || "").toLowerCase();
  if (ext === "pdf") return "application/pdf";
  if (ext === "md") return "text/markdown";
  if (ext === "log") return "text/plain";
  return "text/plain";
}

function showToast(message, level) {
  if (typeof window.showToast === "function") {
    window.showToast(message, level);
    return;
  }
  // Fallback: console-only when the page-level toast helper
  // isn't on `window` (e.g. unit tests that exercise the module
  // in isolation). The Documents panel provides a richer toast
  // for the sidebar path.
  if (level === "error") console.warn("[attachments]", message);
  else console.info("[attachments]", message);
}

function resolveTray() {
  // Re-resolve the tray element from the DOM on every read so a
  // shell re-mount (logout → login) cannot leave `trayEl`
  // pointing at an orphan node. `getElementById` is cheap and
  // the read is bounded to the single DOM lookup.
  return document.getElementById("chat-attach-chips");
}

function notify() {
  const tray = resolveTray();
  if (!tray) return;
  trayEl = tray;
  renderTray();
  // Reveal the tray the moment a slot lands; keep it hidden
  // when the user removes the last chip.
  tray.toggleAttribute("hidden", !feature() || pending.length === 0);
}

function renderTray() {
  if (!trayEl) return;
  trayEl.innerHTML = "";
  if (!pending.length) return;
  const frag = document.createDocumentFragment();
  for (const slot of pending) {
    frag.appendChild(buildTrayChip(slot));
  }
  trayEl.appendChild(frag);
}

function buildTrayChip(slot) {
  const li = document.createElement("li");
  li.className = "chat-attach-chip";
  if (slot.status === "uploading") li.classList.add("chat-attach-chip--uploading");
  if (slot.status === "error") li.classList.add("chat-attach-chip--error");
  if (slot.id) li.dataset.docId = slot.id;

  const badge = document.createElement("span");
  badge.className = mimeBadgeClass(slot.mime);
  badge.textContent = mimeLabel(slot.mime);

  const label = document.createElement("span");
  label.className = "chat-attach-chip-name";
  label.textContent = slot.name;

  const meta = document.createElement("span");
  meta.className = "chat-attach-chip-meta";
  meta.textContent = slot.status === "error"
    ? (slot.error || "Upload failed")
    : humanSize(slot.size);

  const remove = document.createElement("button");
  remove.type = "button";
  remove.className = "chat-attach-chip-remove";
  remove.setAttribute("aria-label", `Remove ${slot.name}`);
  remove.title = "Remove";
  remove.textContent = "\u00d7";
  remove.addEventListener("click", () => removePending(slot));

  li.append(badge, label, meta, remove);
  return li;
}

function mimeBadgeClass(mime) {
  const m = (mime || "").toLowerCase();
  if (m.includes("pdf")) return "chat-attach-chip-mime chat-attach-chip-mime--pdf";
  if (m.includes("markdown")) return "chat-attach-chip-mime chat-attach-chip-mime--md";
  if (m.includes("log")) return "chat-attach-chip-mime chat-attach-chip-mime--log";
  return "chat-attach-chip-mime chat-attach-chip-mime--txt";
}

function mimeLabel(mime) {
  const cls = mimeBadgeClass(mime);
  if (cls.endsWith("--pdf")) return "PDF";
  if (cls.endsWith("--md")) return "MD";
  if (cls.endsWith("--log")) return "LOG";
  return "TXT";
}

function removePending(slot) {
  const idx = pending.indexOf(slot);
  if (idx < 0) return;
  pending.splice(idx, 1);
  notify();
}

async function stageFile(file) {
  if (!isEnabled()) {
    showToast("Documents feature is disabled on this server.", "error");
    return;
  }
  const ext = (file.name.split(".").pop() || "").toLowerCase();
  if (!ACCEPTED_EXTENSIONS.includes(ext)) {
    showToast(`Unsupported file type: .${ext}`, "error");
    return;
  }
  if (file.size > MAX_FILE_BYTES) {
    showToast(
      `File too large (max ${Math.round(MAX_FILE_BYTES / 1024 / 1024)} MB).`,
      "error",
    );
    return;
  }
  const slot = {
    file,
    status: "uploading",
    name: file.name,
    size: file.size,
    mime: file.type || guessMime(file.name),
  };
  pending.push(slot);
  console.debug("[attachments] staged", slot.name, slot.size, "pending=", pending.length);
  notify();
  try {
    const sid = await getServerSessionId();
    if (!sid) throw new Error("No chat session bound");
    const doc = await uploadFileInBackground(file, { sessionId: sid });
    slot.id = doc.id;
    slot.status = "done";
    slot.serverName = doc.name;
    // Mirror the upload into the sidebar Documents panel so the
    // same file appears in both surfaces. `Documents.refresh`
    // short-circuits when the documents feature is off, so this
    // is a no-op in the disabled-while-staged edge case.
    Documents.refresh?.();
  } catch (e) {
    slot.status = "error";
    slot.error = e.message || "Upload failed";
    showToast(slot.error, "error");
  }
  notify();
}

function consumeOnSend() {
  const ready = pending.filter((p) => p.status === "done" && p.id);
  const inFlight = pending.filter((p) => p.status !== "done");
  // Keep in-flight + errored slots in the tray; the user can wait
  // for the upload to finish or `×` them. Only the slots that
  // actually made it to the server ride along on this turn.
  pending.length = 0;
  pending.push(...inFlight);
  notify();
  if (!ready.length) return { hint: "", summaries: [] };
  const lines = ready.map((p) =>
    `- ${p.serverName || p.name} (id=${p.id}, size=${p.size}B)`,
  );
  const hint = [
    "[Attachments uploaded by the user this turn. Call `read_document` with `name=<id>` to read any you need.]",
    ...lines,
  ].join("\n");
  const summaries = ready.map((p) => ({
    id: p.id,
    name: p.serverName || p.name,
    mime: p.mime,
    size_bytes: p.size,
  }));
  return { hint, summaries };
}

function renderChips(containerEl, summaries) {
  if (!containerEl) return;
  containerEl.innerHTML = "";
  if (!Array.isArray(summaries) || !summaries.length) return;
  const frag = document.createDocumentFragment();
  for (const s of summaries) {
    const li = document.createElement("li");
    li.className = "chat-attach-chip";
    if (s.id) li.dataset.docId = s.id;
    const badge = document.createElement("span");
    badge.className = mimeBadgeClass(s.mime);
    badge.textContent = mimeLabel(s.mime);
    const label = document.createElement("span");
    label.className = "chat-attach-chip-name";
    label.textContent = s.name;
    const meta = document.createElement("span");
    meta.className = "chat-attach-chip-meta";
    meta.textContent = humanSize(s.size_bytes);
    // Historical chips are display-only — no remove button.
    li.append(badge, label, meta);
    frag.append(li);
  }
  containerEl.append(frag);
}

function reset() {
  pending.length = 0;
  notify();
}

function applyGate() {
  const btn = document.getElementById("chat-attach-btn");
  const tray = resolveTray();
  if (!btn || !tray) return;
  const on = feature();
  btn.toggleAttribute("hidden", !on);
  // Keep the tray hidden when no file is staged even if the
  // feature is on, so the empty tray doesn't take layout space.
  tray.toggleAttribute("hidden", !on || pending.length === 0);
  trayEl = tray;
}

// `wireOnce` is called from every `app-shell-mounted` event
// (the shell re-clones on every logout → login). The
// `_wiredForm` reference is the form element we attached the
// drop-zone listeners to; if it's been replaced by a fresh
// mount, re-bind to the new form/tray. This avoids the
// "stale listener on an orphan node" failure mode that
// would otherwise drop drag-and-drop after the first
// logout/login cycle.
let _wiredForm = null;
function wireOnce() {
  if (typeof window.File === "undefined" || typeof window.FormData === "undefined") {
    // Pre-File-API browsers can't upload; bail silently. The
    // paperclip stays hidden via the feature gate.
    return;
  }
  const form = document.getElementById("chat-form");
  const btn = document.getElementById("chat-attach-btn");
  const input = document.getElementById("chat-attach-input");
  const tray = document.getElementById("chat-attach-chips");
  if (!form || !btn || !input || !tray) return;
  if (_wiredForm === form) {
    // Same form node, listeners are already bound.
    trayEl = tray;
    mounted = true;
    return;
  }
  _wiredForm = form;
  trayEl = tray;
  mounted = true;
  console.debug("[attachments] wiring drop zone on form", form);

  btn.addEventListener("click", () => {
    if (btn.hasAttribute("disabled")) return;
    input.click();
  });
  input.addEventListener("change", () => {
    const files = Array.from(input.files || []);
    if (!files.length) return;
    for (const f of files) stageFile(f);
    // Reset so re-picking the same file fires `change` again.
    input.value = "";
  });

  // Drop zone: `#chat-form` (the natural target) and the tray
  // itself (so a drop above the form still lands). Both
  // handlers bail when the feature is off or the drop carries
  // no file (e.g. a text drag).
  for (const region of [form, tray]) {
    region.addEventListener("dragover", (e) => {
      if (!isEnabled()) return;
      if (!e.dataTransfer?.types?.includes("Files")) return;
      e.preventDefault();
      region.classList.add("is-dragover");
    });
    region.addEventListener("dragleave", (e) => {
      // Only clear when the pointer leaves the region itself,
      // not its children — otherwise the highlight flickers as
      // the cursor crosses chip boundaries.
      if (e.target === region) region.classList.remove("is-dragover");
    });
    region.addEventListener("drop", (e) => {
      region.classList.remove("is-dragover");
      if (!isEnabled()) return;
      const files = Array.from(e.dataTransfer?.files || []);
      if (!files.length) return;
      e.preventDefault();
      for (const f of files) stageFile(f);
    });
  }

  // Grey the paperclip + tray while a turn is inflight, so the
  // user can't `×` a chip and re-add it during the same turn's
  // streaming phase. `chat.js` dispatches this `CustomEvent` from
  // every `setStreamingUi(true|false)` call site.
  window.addEventListener("chat-stream-state", (e) => {
    streaming = e.detail?.streaming === true;
    btn.toggleAttribute("disabled", streaming);
    tray.classList.toggle("is-locked", streaming);
  });
}

window.addEventListener("app-shell-mounted", () => {
  wireOnce();
  applyGate();
});
if (window.__nagentSubscribeFeatures) {
  window.__nagentSubscribeFeatures(applyGate);
}
// Eager run in case the features arrived before this module
// loaded (the same race the documents module handles at
// `documents.js:201-212`).
applyGate();

window.__nagentAttachments = {
  isEnabled,
  stage: (files) => {
    const list = Array.isArray(files) ? files : Array.from(files || []);
    return Promise.all(list.map((f) => stageFile(f))).then(() => {});
  },
  consumeOnSend,
  renderChips,
  reset,
};

export {
  isEnabled,
  consumeOnSend,
  renderChips,
  reset,
};
