// nagent STT — Transcript mode frontend.
//
// The actual audio capture pipeline (VAD, WebSocket, voice graph,
// wire-protocol codec) lives in `audio.js` and is shared with
// Discussion mode. This file is only responsible for wiring the
// Transcript-mode DOM (`#record-btn`, `#lang-select`, `#translate-check`,
// `#download-btn`, `#status`, `#backend-info`, `#transcript-list`,
// `#voice-graph-canvas`, `#voice-graph-level`) on top of an
// `AudioCapture` instance.

import { AudioCapture } from "/static/audio.js";

// ---- DOM --------------------------------------------------------------------

const $ = (id) => document.getElementById(id);
const recordBtn   = $("record-btn");
const langSelect  = $("lang-select");
const translateCk = $("translate-check");
const downloadBtn = $("download-btn");
const statusEl    = $("status");
const backendEl   = $("backend-info");
const listEl      = $("transcript-list");
const scopeCanvas = $("voice-graph-canvas");
const scopeLevel  = $("voice-graph-level");
const backendVersionEl    = $("backend-version");
const frontendVersionSelf = $("frontend-version-self");
const updateBanner        = $("update-banner");
const updateBannerReload  = $("update-banner-reload");
const updateBannerDismiss = $("update-banner-dismiss");

const transcript = []; // { text, lang, ts, latencyMs? }

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}

function appendLine(text, lang, latencyMs) {
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
  transcript.push({ text, lang, ts, latencyMs });
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

// ---- Audio pipeline --------------------------------------------------------

new AudioCapture({
  buttonEl: recordBtn,
  statusEl,
  canvasEl: scopeCanvas,
  levelEl: scopeLevel,
  graphEl: document.querySelector("#view-transcript .voice-graph"),
  containerEl: document.getElementById("view-transcript"),
  langSelectEl: langSelect,
  translateCheckEl: translateCk,
  backendInfoEl: backendEl,
  onFinalTranscript: (text, lang, latencyMs) => {
    if (text) appendLine(text, lang, latencyMs);
  },
  onError: (code, message) => {
    appendLine(`[error ${code}] ${message}`, "err");
  },
});

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
})();

// ---- Download button --------------------------------------------------------

downloadBtn.addEventListener("click", () => {
  const blob = new Blob(
    [
      transcript
        .map((l) => {
          const lat = typeof l.latencyMs === "number" ? ` (${formatLatency(l.latencyMs)})` : "";
          return `${l.ts} [${l.lang || "-"}]${lat} ${l.text}`;
        })
        .join("\n"),
    ],
    { type: "text/plain" },
  );
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = "transcript.txt";
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
});
