// nagent — Discussion mode chat UI.
//
// Streams tokens from `/v1/chat/completions` (proxied to Ollama or any
// OpenAI-compatible endpoint). History is kept in `localStorage` only;
// no server-side session is involved.
//
// Audio in Discussion mode:
//   A second `AudioCapture` instance is created alongside the chat UI.
//   When the user clicks Record, FinalTranscripts are routed as user
//   turns (same path as typed messages) and the assistant reply streams
//   in automatically. The audio pipeline is fully independent from
//   Transcript mode: each mode has its own `AudioCapture` (its own WS,
//   its own recording state). The underlying MicVAD instance is shared
//   so we do not load the Silero model twice.
//
// Concurrency:
//   User turns (typed or transcribed) go through a single Promise
//   queue so two replies can never stream at the same time. A new
//   `send()` or transcript arrival while a reply is in flight simply
//   waits for the current reply to finish.

import { AudioCapture } from "/static/audio.js";
import { preselectFromBrowser } from "/static/lang-preselect.js";

const HISTORY_KEY = "nagent.chat.history";
const HISTORY_CAP = 200;

const $ = (id) => document.getElementById(id);
const messagesEl   = $("chat-messages");
const formEl       = $("chat-form");
const inputEl      = $("chat-input");
const sendBtn      = $("chat-send");
const stopBtn      = $("chat-stop");
const clearBtn     = $("chat-clear");
const modelEl      = $("chat-model");
const systemEl     = $("chat-system");
const tempEl       = $("chat-temperature");
const statusEl     = $("chat-status");
const disabledNoticeEl = $("chat-disabled-notice");

// One stream at a time. If a new turn arrives while a reply is
// streaming, the reply continues to completion and the new turn runs
// immediately after (no abort — the user can keep recording without
// cutting the current reply short).
let inflight = null; // { controller, assistantEl, model }

// Serialize user turns so a reply never overlaps another reply.
let turnQueue = Promise.resolve();
function enqueueTurn(fn) {
  turnQueue = turnQueue.then(fn, fn);
  return turnQueue;
}

// ---- Status pill -----------------------------------------------------------
//
// Two sources feed the pill: the chat streaming layer (high priority)
// and `AudioCapture` (low priority). While a reply is streaming the
// pill shows the streaming state; otherwise it shows the most recent
// audio state (or "idle").

let lastAudioStatus = { text: "idle", cls: "idle" };
let lastStreamActive = false;
function renderStatus() {
  const { text, cls } = lastStreamActive
    ? { text: "streaming…", cls: "connecting" }
    : lastAudioStatus;
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}
function setAudioStatus(text, cls) {
  lastAudioStatus = { text, cls };
  renderStatus();
}
function setStreamActive(active) {
  lastStreamActive = active;
  renderStatus();
}

function setStreaming(streaming) {
  if (streaming) {
    sendBtn.hidden = true;
    stopBtn.hidden = false;
    inputEl.disabled = true;
    setStreamActive(true);
  } else {
    sendBtn.hidden = false;
    stopBtn.hidden = true;
    inputEl.disabled = false;
    inputEl.focus();
  }
}

// ---- History ---------------------------------------------------------------

function loadHistory() {
  try {
    const raw = localStorage.getItem(HISTORY_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch (_e) {
    return [];
  }
}

function saveHistory(history) {
  try {
    const trimmed = history.slice(-HISTORY_CAP);
    localStorage.setItem(HISTORY_KEY, JSON.stringify(trimmed));
  } catch (_e) {}
}

function renderHistory() {
  messagesEl.innerHTML = "";
  const history = loadHistory();
  for (const msg of history) {
    appendBubble(msg.role, msg.content, { persist: false, model: msg.model });
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function appendBubble(role, text, { persist = true, model = null } = {}) {
  const div = document.createElement("div");
  div.className = `chat-message chat-${role}`;
  if (model && role === "assistant") div.dataset.model = model;
  div.textContent = text;
  messagesEl.appendChild(div);
  messagesEl.scrollTop = messagesEl.scrollHeight;
  if (persist) {
    const history = loadHistory();
    history.push({ role, content: text, ts: Date.now(), model });
    saveHistory(history);
  }
  return div;
}

function appendError(text) {
  const div = document.createElement("div");
  div.className = "chat-message chat-error";
  div.textContent = `[error] ${text}`;
  messagesEl.appendChild(div);
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

// ---- Models ----------------------------------------------------------------

async function loadModels() {
  try {
    const r = await fetch("/v1/models", { cache: "no-store" });
    if (r.status === 404) {
      disabledNoticeEl.hidden = false;
      formEl.hidden = true;
      document.querySelector(".chat-header").hidden = true;
      document.querySelector(".chat-audio").hidden = true;
      document.querySelector(".voice-graph").hidden = true;
      document.querySelector(".chat-advanced").hidden = true;
      statusEl.textContent = "disabled";
      statusEl.className = "status idle";
      return;
    }
    if (!r.ok) throw new Error(`status ${r.status}`);
    disabledNoticeEl.hidden = true;
    formEl.hidden = false;
    document.querySelector(".chat-header").hidden = false;
    document.querySelector(".chat-audio").hidden = false;
    document.querySelector(".voice-graph").hidden = false;
    document.querySelector(".chat-advanced").hidden = false;
    // Refresh the pill so a previous "disabled" state disappears.
    renderStatus();
    const data = await r.json();
    const items = Array.isArray(data?.data) ? data.data : [];
    modelEl.innerHTML = "";
    if (items.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no models)";
      modelEl.appendChild(opt);
      return;
    }
    for (const item of items) {
      const opt = document.createElement("option");
      opt.value = item.id || item.name || "";
      opt.textContent = item.id || item.name || "(unnamed)";
      modelEl.appendChild(opt);
    }
  } catch (e) {
    // Surface failures in the status pill so an empty dropdown is
    // not silently confusing — the user can see *why* nothing loaded.
    console.warn("loadModels failed:", e);
    setAudioStatus(`models: ${e?.message || e}`, "error");
  }
}

function buildMessages() {
  const messages = [];
  const system = systemEl.value.trim();
  if (system) messages.push({ role: "system", content: system });
  for (const m of loadHistory()) {
    if (m.role !== "system") messages.push({ role: m.role, content: m.content });
  }
  return messages;
}

// ---- Streaming reply -------------------------------------------------------

async function streamReply() {
  // Build the request from history (minus the very last entry, which
  // is the user turn we just appended). The last entry is re-added
  // explicitly so we don't depend on history-load timing.
  const history = loadHistory();
  const last = history[history.length - 1];
  if (!last || last.role !== "user") return; // nothing to reply to
  const earlier = history.slice(0, -1);

  const model = modelEl.value || undefined;
  const assistantEl = appendBubble("assistant", "", { persist: false, model });

  const controller = new AbortController();
  inflight = { controller, assistantEl, model };
  setStreaming(true);

  const messages = [
    ...(systemEl.value.trim()
      ? [{ role: "system", content: systemEl.value.trim() }]
      : []),
    ...earlier
      .filter((m) => m.role !== "system")
      .map((m) => ({ role: m.role, content: m.content })),
    { role: "user", content: last.content },
  ];
  const body = { messages, stream: true };
  const temperature = parseFloat(tempEl.value);
  if (Number.isFinite(temperature)) body.temperature = temperature;
  if (model) body.model = model;

  let accumulated = "";
  try {
    const resp = await fetch("/v1/chat/completions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    if (!resp.ok) {
      const errBody = await resp.text().catch(() => "");
      throw new Error(`HTTP ${resp.status}: ${errBody || resp.statusText}`);
    }
    if (!resp.body) throw new Error("response has no body");

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let sep;
      while ((sep = buffer.indexOf("\n\n")) !== -1) {
        const raw = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        for (const line of raw.split("\n")) {
          if (!line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (payload === "[DONE]") { reader.cancel(); break; }
          if (!payload) continue;
          try {
            const evt = JSON.parse(payload);
            const delta = evt?.choices?.[0]?.delta?.content;
            if (typeof delta === "string" && delta.length > 0) {
              accumulated += delta;
              assistantEl.textContent = accumulated;
              messagesEl.scrollTop = messagesEl.scrollHeight;
            }
          } catch (_e) { /* skip malformed line */ }
        }
      }
    }
  } catch (e) {
    if (e?.name === "AbortError") {
      assistantEl.textContent = accumulated || "(stopped)";
    } else {
      assistantEl.textContent = `[error] ${e?.message || e}`;
      appendError(e?.message || String(e));
    }
  } finally {
    const h = loadHistory();
    h.push({ role: "assistant", content: assistantEl.textContent, ts: Date.now(), model });
    saveHistory(h);
    inflight = null;
    setStreaming(false);
  }
}

// ---- User turn entry points ------------------------------------------------

async function submitUserTurn(text) {
  const trimmed = (text || "").trim();
  if (!trimmed) return;
  appendBubble("user", trimmed);
  await streamReply();
}

function sendTyped() {
  const text = inputEl.value;
  inputEl.value = "";
  enqueueTurn(() => submitUserTurn(text));
}

function receiveTranscript(text) {
  // No empty-text guard here: AudioCapture already filters empty
  // FinalTranscripts at the wire-protocol level.
  enqueueTurn(() => submitUserTurn(text));
}

function stop() {
  if (inflight) inflight.controller.abort();
}

function clearChat() {
  if (!confirm("Clear the conversation?")) return;
  try { localStorage.removeItem(HISTORY_KEY); } catch (_e) {}
  messagesEl.innerHTML = "";
  inputEl.focus();
}

// ---- Wire up the form -------------------------------------------------------

formEl.addEventListener("submit", (e) => {
  e.preventDefault();
  sendTyped();
});
stopBtn.addEventListener("click", stop);
clearBtn.addEventListener("click", clearChat);

inputEl.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
    e.preventDefault();
    sendTyped();
  } else if (e.key === "Escape" && inflight) {
    e.preventDefault();
    stop();
  }
});

// ---- Audio capture (Discussion mode) ---------------------------------------

const audioCapture = new AudioCapture({
  buttonEl: $("chat-record-btn"),
  statusEl: null, // merged into #chat-status via the onStatusChange callback
  canvasEl: $("chat-voice-graph-canvas"),
  levelEl:  $("chat-voice-graph-level"),
  graphEl:  document.querySelector("#view-discussion .voice-graph"),
  containerEl: document.getElementById("view-discussion"),
  langSelectEl: $("chat-lang-select"),
  translateCheckEl: $("chat-translate-check"),
  backendInfoEl: $("chat-backend-info"),
  onFinalTranscript: (text) => {
    if (text && text.trim()) receiveTranscript(text);
  },
  onError: (code, message) => {
    appendError(`audio ${code}: ${message}`);
  },
  onStatusChange: (text, cls) => {
    setAudioStatus(text, cls);
  },
});

// Locale-aware defaults: preselect the discussion-mode language from
// the browser locale if it matches one of the options; otherwise keep
// "Auto-detect" (empty value).
preselectFromBrowser($("chat-lang-select"));

// ---- Boot ------------------------------------------------------------------

renderHistory();

// Load the model list once on page boot. The previous version only
// fired on `modechange`, which meant a Discussion-mode-persisted user
// saw an empty dropdown until they clicked away and back. Loading at
// boot keeps the dropdown warm regardless of the persisted mode, and
// the disabled-server notice still renders correctly on 404.
loadModels();

document.addEventListener("visibilitychange", () => {
  // Re-fetch on tab return in case the server's model list changed
  // while the tab was backgrounded (e.g. another `ollama pull`).
  if (!document.hidden && globalThis.__nagentMode?.current() === "discussion") {
    loadModels();
  }
});
