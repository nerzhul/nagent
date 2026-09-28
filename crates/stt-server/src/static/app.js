// nagent STT — Transcript mode frontend.
//
// The actual audio capture pipeline (VAD, WebSocket, voice graph,
// wire-protocol codec) lives in `audio.js` and is shared with
// Discussion mode. This file is only responsible for wiring the
// Transcript-mode DOM (`#record-btn`, `#lang-select`, `#translate-check`,
// `#download-btn`, `#download-menu`, `#status`, `#backend-info`,
// `#transcript-list`) on top of an `AudioCapture` instance.
//
// Export (SRT / VTT / JSON / TXT) is owned by this file because the
// per-line `segments[]` carrying `t0_ms` / `t1_ms` is captured here
// when `appendLine` runs; downstream modules never see the raw wire
// payload.
//
// The voice oscilloscope is a shared widget (createScope +
// AudioCapture), but each mode mounts its own DOM instance — clicking
// Record in either view drives its own oscilloscope, scoped to the
// active view's layout. The Transcript-mode instance lives at the top
// of the transcript view (`#voice-graph-transcript`); the
// Discussion-mode instance is appended inline as a voice bubble
// inside `#chat-messages` (see `chat.js`).

import { AudioCapture } from "/static/audio.js";
import { preselectFromBrowser } from "/static/lang-preselect.js";

// ---- DOM --------------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const recordBtn   = $("record-btn");
const langSelect  = $("lang-select");
const translateCk = $("translate-check");
const inactivityCk = $("inactivity-check");
const downloadBtn = $("download-btn");
const downloadMenu = $("download-menu");
const clearBtn    = $("clear-btn");
const statusEl    = $("status");
const backendEl   = $("backend-info");
const listEl      = $("transcript-list");
const scopeCanvas = $("voice-graph-transcript-canvas");
const scopeLevel  = $("voice-graph-transcript-level");
const backendVersionEl    = $("backend-version");
const frontendVersionSelf = $("frontend-version-self");
const updateBanner        = $("update-banner");
const updateBannerReload  = $("update-banner-reload");
const updateBannerDismiss = $("update-banner-dismiss");

const transcript = []; // { text, lang, ts, latencyMs?, segments: [{ text, t0_ms, t1_ms }] }
//
// Cumulative audio-offset tracker for SRT/VTT export. Each FinalTranscript
// carries `segments[]` whose `t0_ms`/`t1_ms` are relative to the *start
// of the audio chunk that produced them*, not the start of the whole
// recording session. SRT/VTT captions need a global offset, so we bump
// this counter by the longest segment in the previous chunk on every
// arrival. Reset to 0 on `clearTranscript` (a fresh recording session).
let cumulativeOffsetMs = 0;

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}

function appendLine(text, lang, latencyMs, segments) {
  const empty = listEl.querySelector(".empty-state");
  if (empty) empty.remove();
  const li = document.createElement("li");
  const ts = new Date().toLocaleTimeString();
  if (lang) {
    const langSpan = document.createElement("span");
    langSpan.className = "lang";
    langSpan.textContent = `[${lang}]`;
    li.appendChild(langSpan);
  }
  const tsSpan = document.createElement("span");
  tsSpan.className = "ts";
  tsSpan.textContent = ts;
  li.appendChild(tsSpan);
  if (typeof latencyMs === "number" && Number.isFinite(latencyMs) && latencyMs >= 0) {
    const latSpan = document.createElement("span");
    latSpan.className = "latency";
    latSpan.title = "Audio → FinalTranscript round-trip latency";
    latSpan.textContent = formatLatency(latencyMs);
    li.appendChild(latSpan);
  }
  li.appendChild(document.createTextNode(text));
  listEl.appendChild(li);
  listEl.scrollTop = listEl.scrollHeight;
  downloadBtn.disabled = false;
  downloadBtn.hidden = false;
  if (downloadMenu) downloadMenu.removeAttribute("disabled");
  clearBtn.disabled = false;

  // Promote segments to absolute timestamps by adding the running
  // offset. We keep the original relative timings on `rel_*` so the
  // JSON export can show both views if it ever needs to.
  const segs = Array.isArray(segments) ? segments : [];
  const absoluteSegments = segs.map((s) => ({
    text: s.text,
    t0_ms: s.t0_ms + cumulativeOffsetMs,
    t1_ms: s.t1_ms + cumulativeOffsetMs,
    rel_t0_ms: s.t0_ms,
    rel_t1_ms: s.t1_ms,
  }));
  transcript.push({
    text,
    lang,
    ts,
    latencyMs,
    segments: absoluteSegments,
  });

  // Bump the cumulative offset by the chunk's last segment end so the
  // next chunk's captions start where this one finished. Empty chunks
  // (transcript from a tool error or no_speech frame) keep the
  // counter untouched.
  if (segs.length > 0) {
    const chunkEndMs = segs.reduce(
      (acc, s) => (typeof s.t1_ms === "number" && s.t1_ms > acc ? s.t1_ms : acc),
      0,
    );
    cumulativeOffsetMs += chunkEndMs;
  }
}

function clearTranscript() {
  // Confirmation guards against accidental clicks — clearing is
  // destructive and the transcript is not persisted to disk.
  if (!confirm("Clear the transcript?")) return;
  transcript.length = 0;
  cumulativeOffsetMs = 0;
  listEl.innerHTML = "";
  downloadBtn.disabled = true;
  downloadBtn.hidden = true;
  if (downloadMenu) downloadMenu.setAttribute("disabled", "");
  clearBtn.disabled = true;
  emptyState();
}

function formatLatency(ms) {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function emptyState() {
  if (transcript.length === 0) {
    const li = document.createElement("li");
    li.className = "empty-state";
    li.textContent = "No transcript yet. Click Record to start.";
    listEl.appendChild(li);
  }
}

emptyState();

// ---- Locale-aware defaults --------------------------------------------------

// Preselect the transcript-mode language from the browser locale if it
// matches one of the options; otherwise keep "Auto-detect" (empty value).
preselectFromBrowser(langSelect);

// ---- Toolbar wiring ---------------------------------------------------------

// Restore the inactivity-watchdog toggle from `localStorage` so the
// preference survives reloads; persist on every change.
const INACTIVITY_PREF_KEY = "nagent.audio.inactivityEnabled";
(function initInactivityPref() {
  try {
    const stored = localStorage.getItem(INACTIVITY_PREF_KEY);
    if (stored === "false") inactivityCk.checked = false;
    else if (stored === "true") inactivityCk.checked = true;
  } catch (_e) { /* localStorage may be unavailable; default to checked */ }
  inactivityCk.addEventListener("change", () => {
    try { localStorage.setItem(INACTIVITY_PREF_KEY, inactivityCk.checked ? "true" : "false"); }
    catch (_e) {}
  });
})();

clearBtn.addEventListener("click", clearTranscript);

// ---- Audio pipeline --------------------------------------------------------

new AudioCapture({
  buttonEl: recordBtn,
  statusEl,
  canvasEl: scopeCanvas,
  levelEl: scopeLevel,
  graphEl: $("voice-graph-transcript"),
  containerEl: document.getElementById("view-transcript"),
  langSelectEl: langSelect,
  translateCheckEl: translateCk,
  inactivityCheckEl: inactivityCk,
  backendInfoEl: backendEl,
  onFinalTranscript: (text, lang, latencyMs, segments) => {
    if (text) appendLine(text, lang, latencyMs, segments);
  },
  onError: (code, message) => {
    appendLine(`[error ${code}] ${message}`, "err");
  },
});

// ---- Authentication --------------------------------------------------------
//
// The server exposes `/api/me` (returns 200 with the AuthUser JSON
// when authenticated, 401 when auth is enabled and the user is
// anonymous, 404 when the auth subsystem is not configured at all)
// and `POST /api/auth/login/password`.
//
// The boot-time probe, the login form, and the modal control logic
// all live in `auth.js` (loaded directly in the body, outside the
// app-shell template). By the time this module is executed the
// template has already been cloned, the modal handlers are wired,
// and the user is either authenticated or the shell was mounted
// because the server returned 404 (no auth configured).
//
// Here we only render the auth pill based on the state exposed by
// `window.nagentAuth` and dispatch a `nagent:logout` event when
// the user clicks "Sign out" — `auth.js` listens for it and tears
// down the shell + opens the forced modal again.

const authState = {
  /** @type {null | {id:string,email:string,display_name:string,provider:string,csrf_token:string,session_expires_at:string}} */
  user: null,
  /** True after `auth.js` has made the first /api/me probe. */
  probed: false,
};

const authPill = document.getElementById("auth-pill");
const authPillText = document.getElementById("auth-pill-text");
const authPillLoginBtn = document.getElementById("auth-pill-action");
const authPillLogoutBtn = document.getElementById("auth-pill-logout");

/** Read the current user + probed flag from `auth.js` and re-render
 *  the pill. The shell only mounts after a 200 or 404, so the pill
 *  is always visible at this point. */
function refreshAuthState() {
  const auth = window.nagentAuth;
  if (!auth) {
    // Should not happen — `auth.js` is always loaded first.
    authPill.setAttribute("hidden", "");
    return;
  }
  authState.user = auth.getUser();
  authState.probed = auth.isProbed();
  renderAuthPill();
}

/** Hide or show the auth pill depending on the probe result. */
function renderAuthPill() {
  if (!authState.probed) {
    // 404 from /api/me — auth subsystem not configured, hide the pill
    // entirely. Pre-PR1 single-user trust boundary.
    authPill.setAttribute("hidden", "");
    return;
  }
  authPill.removeAttribute("hidden");
  if (authState.user) {
    authPillText.textContent = `Logged in as ${authState.user.email}`;
    authPillLoginBtn.setAttribute("hidden", "");
    authPillLogoutBtn.removeAttribute("hidden");
  } else {
    // Auth is enabled but the server returned 200 with no user
    // row — an edge case (e.g. session was revoked server-side
    // between the initial probe and the pill render). Offer the
    // Sign in button so the user can recover.
    authPillText.textContent = "Sign in to access the chat view";
    authPillLoginBtn.textContent = "Sign in";
    authPillLoginBtn.removeAttribute("hidden");
    authPillLogoutBtn.setAttribute("hidden", "");
  }
}

if (authPillLoginBtn) {
  authPillLoginBtn.addEventListener("click", () => {
    window.nagentAuth?.showLoginModal(false);
  });
}

if (authPillLogoutBtn) {
  authPillLogoutBtn.addEventListener("click", async () => {
    if (!authState.user) return;
    const csrf = authState.user.csrf_token;
    authPillLogoutBtn.disabled = true;
    try {
      await fetch("/api/auth/logout", {
        method: "POST",
        headers: { "x-csrf-token": csrf },
        credentials: "same-origin",
      });
    } catch (e) {
      console.warn("logout request failed:", e);
    }
    // Hand control back to auth.js: it will unmount the app shell
    // (so the chat/voice UI is no longer in the DOM) and pop the
    // forced login modal back up. We don't need to re-render the
    // pill — the entire shell is about to be removed.
    authPillLogoutBtn.disabled = false;
    window.dispatchEvent(new CustomEvent("nagent:logout"));
  });
}

// ---- Version drift detection ------------------------------------------------

const VERSION_POLL_MS = 30_000;
let versionPollId = 0;

async function fetchSelfVersion() {
  try {
    const r = await fetch("/static/version.txt", { cache: "no-store" });
    if (!r.ok) throw new Error(`status ${r.status}`);
    const text = (await r.text()).trim();
    if (text && frontendVersionSelf) frontendVersionSelf.textContent = text;
  } catch (e) {
    console.warn("could not read self frontend version:", e);
  }
}

async function fetchServerVersion() {
  try {
    const r = await fetch("/api/version", { cache: "no-store" });
    if (!r.ok) throw new Error(`status ${r.status}`);
    const info = await r.json();
    if (typeof info.backend === "string" && backendVersionEl) {
      backendVersionEl.textContent = info.backend;
    }
    if (typeof info.frontend === "string") {
      maybeShowUpdateBanner(info.frontend);
    }
  } catch (e) {
    console.warn("could not read server version:", e);
  }
}

function maybeShowUpdateBanner(serverFrontend) {
  if (!frontendVersionSelf || !frontendVersionSelf.textContent) return;
  if (frontendVersionSelf.textContent === serverFrontend) {
    updateBanner?.setAttribute("hidden", "");
    return;
  }
  updateBanner?.removeAttribute("hidden");
}

if (updateBannerReload) {
  updateBannerReload.addEventListener("click", () => location.reload());
}
if (updateBannerDismiss) {
  updateBannerDismiss.addEventListener("click", () => {
    updateBanner?.setAttribute("hidden", "");
  });
}

(async () => {
  await fetchSelfVersion();
  await fetchServerVersion();
  if (!versionPollId) {
    versionPollId = setInterval(fetchServerVersion, VERSION_POLL_MS);
  }
  // The `/api/me` probe, login form, and shell mount/unmount are
  // owned by `auth.js` (which ran before this module was cloned
  // into the DOM). Read its exposed state and render the pill.
  refreshAuthState();
})();

// ---- Export (SRT / VTT / JSON / TXT) ----------------------------------------
//
// Caption exports reuse the per-chunk `segments[]` carrying absolute
// `t0_ms`/`t1_ms` (see `cumulativeOffsetMs` above) so a player can
// jump directly to any line. `.txt` is the historical flat log used
// by `Download .txt` and stays untouched for backwards compatibility.
//
// The button is a `<details>`-based dropdown so we don't ship a
// popover library; clicking outside the menu closes it via the
// `toggle` event the browser fires when the `<summary>` is re-
// activated.

function pad2(n) {
  return n < 10 ? `0${n}` : `${n}`;
}

/// Format `ms` as a SRT/VTT-friendly `HH:MM:SS,mmm` (SRT) or
/// `HH:MM:SS.mmm` (VTT) timestamp. Hours are always two digits
/// because all three formats require it.
function formatSrtTimestamp(ms) {
  const totalMs = Math.max(0, Math.round(ms));
  const hours = Math.floor(totalMs / 3_600_000);
  const minutes = Math.floor((totalMs % 3_600_000) / 60_000);
  const seconds = Math.floor((totalMs % 60_000) / 1000);
  const millis = totalMs % 1000;
  return `${pad2(hours)}:${pad2(minutes)}:${pad2(seconds)},${pad2(millis)}`;
}

function formatVttTimestamp(ms) {
  // VTT uses a dot instead of a comma for the millisecond separator
  // (WebVTT spec). Everything else is identical to SRT.
  return formatSrtTimestamp(ms).replace(",", ".");
}

/// Build the SRT body. Iterates over every segment in every line so
/// captions line up with the actual audio timestamps — collapsing
/// to a single cue per `FinalTranscript` would lose intra-chunk
/// structure (rare but legal: whisper can emit multiple segments
/// from one chunk).
function buildSrt() {
  const cues = [];
  let index = 1;
  for (const line of transcript) {
    if (!line.segments || line.segments.length === 0) continue;
    for (const seg of line.segments) {
      cues.push(
        `${index}\n${formatSrtTimestamp(seg.t0_ms)} --> ${formatSrtTimestamp(seg.t1_ms)}\n${seg.text}\n`,
      );
      index += 1;
    }
  }
  // SRT spec mandates a trailing newline; players misbehave on
  // truncated files.
  return cues.join("\n") + (cues.length > 0 ? "\n" : "");
}

function buildVtt() {
  const cues = [];
  for (const line of transcript) {
    if (!line.segments || line.segments.length === 0) continue;
    for (const seg of line.segments) {
      cues.push(
        `${formatVttTimestamp(seg.t0_ms)} --> ${formatVttTimestamp(seg.t1_ms)}\n${seg.text}\n`,
      );
    }
  }
  // WebVTT requires the `WEBVTT` magic on the first line. A blank
  // line separates the header from the cues.
  const body = cues.join("\n");
  return body.length > 0 ? `WEBVTT\n\n${body}\n` : "WEBVTT\n\n";
}

function buildJson() {
  // Plain JSON serialization of the in-memory transcript. The shape
  // mirrors what `app.js` keeps internally so a future CLI importer
  // can round-trip the file without surprises.
  const payload = {
    version: 1,
    segments: transcript.flatMap((line) =>
      (line.segments || []).map((seg) => ({
        text: seg.text,
        t0_ms: seg.t0_ms,
        t1_ms: seg.t1_ms,
        lang: line.lang || null,
        wall_clock_ts: line.ts,
        latency_ms: typeof line.latencyMs === "number" ? line.latencyMs : null,
      })),
    ),
    lines: transcript.map((line) => ({
      text: line.text,
      lang: line.lang || null,
      wall_clock_ts: line.ts,
      latency_ms: typeof line.latencyMs === "number" ? line.latencyMs : null,
      segments: line.segments || [],
    })),
  };
  return JSON.stringify(payload, null, 2) + "\n";
}

function buildTxt() {
  // Historical flat format kept byte-for-byte compatible with the
  // pre-SRT/VTT download button: `<wall-clock> [<lang>] (<latency>)
  // <text>`, one line per `FinalTranscript`.
  return (
    transcript
      .map((l) => {
        const lat = typeof l.latencyMs === "number" ? ` (${formatLatency(l.latencyMs)})` : "";
        return `${l.ts} [${l.lang || "-"}]${lat} ${l.text}`;
      })
      .join("\n") + "\n"
  );
}

const exporters = {
  txt: { build: buildTxt, mime: "text/plain", ext: "txt", label: "Plain text (.txt)" },
  srt: { build: buildSrt, mime: "application/x-subrip", ext: "srt", label: "SubRip (.srt)" },
  vtt: { build: buildVtt, mime: "text/vtt", ext: "vtt", label: "WebVTT (.vtt)" },
  json: { build: buildJson, mime: "application/json", ext: "json", label: "JSON (.json)" },
};

function exportAs(format) {
  const exp = exporters[format];
  if (!exp) return;
  const blob = new Blob([exp.build()], { type: exp.mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = `transcript.${exp.ext}`;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

if (downloadMenu) {
  downloadMenu.addEventListener("click", (e) => {
    const target = e.target.closest("[data-format]");
    if (!target) return;
    e.preventDefault();
    exportAs(target.dataset.format);
    // Collapse the <details> after the click so the menu behaves
    // like a normal dropdown (one-shot pick → close).
    downloadMenu.removeAttribute("open");
  });
}

// Keep the historical `Download .txt` button (now hidden in the UI but
// still bound for back-compat with any third-party clicker that
// remembers the old id). Calls into the dropdown exporter.
downloadBtn.addEventListener("click", () => exportAs("txt"));

/// Programmatically trigger the `.txt` export. Used by the
/// `Ctrl+S` keyboard shortcut and any future export affordance.
function exportCurrentTranscript() {
  if (transcript.length === 0) return;
  exportAs("txt");
}

// Expose for other modules (e.g. keyboard-shortcut wiring in the
// future, or integration tests that simulate the shortcut).
globalThis.__nagentExportTranscript = exportCurrentTranscript;
