// nagent — Discussion mode chat UI.
//
// Streams tokens from `/v1/chat/completions` (proxied to Ollama or any
// OpenAI-compatible endpoint). History is kept in `localStorage` only;
// no server-side session is involved.
//
// Streaming: a single module-level `AbortController` is the source of
// truth for the in-flight request. A second `send()` while streaming
// aborts the previous one and starts fresh — keeping the chat UI
// responsive to user edits.

const HISTORY_KEY = "nagent.chat.history";
const HISTORY_CAP = 200;

// One stream at a time. If the user clicks Send again mid-stream, we
// abort the previous request before opening the new one. This is a
// module-level singleton by design (see AGENTS.md §4: pick a strategy
// and document it).
let inflight = null; // { controller, assistantEl }

const $ = (id) => document.getElementById(id);
const messagesEl = $("chat-messages");
const formEl = $("chat-form");
const inputEl = $("chat-input");
const sendBtn = $("chat-send");
const stopBtn = $("chat-stop");
const clearBtn = $("chat-clear");
const modelEl = $("chat-model");
const systemEl = $("chat-system");
const tempEl = $("chat-temperature");
const statusEl = $("chat-status");
const disabledNoticeEl = $("chat-disabled-notice");

function setStatus(text, cls) {
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}

function setStreaming(streaming) {
  // Toggle between the Send/Stop pair. While streaming, Send is hidden
  // and Stop is shown (and vice-versa).
  if (streaming) {
    sendBtn.hidden = true;
    stopBtn.hidden = false;
    inputEl.disabled = true;
  } else {
    sendBtn.hidden = false;
    stopBtn.hidden = true;
    inputEl.disabled = false;
    inputEl.focus();
  }
}

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
    // Cap to the most recent N messages to stay well under the typical
    // localStorage 5 MB quota even on long sessions.
    const trimmed = history.slice(-HISTORY_CAP);
    localStorage.setItem(HISTORY_KEY, JSON.stringify(trimmed));
  } catch (_e) {
    // Quota errors are non-fatal: the in-page log keeps working.
  }
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
  if (model && role === "assistant") {
    div.dataset.model = model;
  }
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

async function loadModels() {
  // Pull the model list once on first entry to Discussion mode, then
  // never again unless the user clears localStorage. The list is also
  // re-fetched when the server config changes (rare in practice).
  try {
    const r = await fetch("/v1/models", { cache: "no-store" });
    if (r.status === 404) {
      // Chat is disabled on the server; show the notice and hide the form.
      disabledNoticeEl.hidden = false;
      formEl.hidden = true;
      document.querySelector(".chat-header").hidden = true;
      document.querySelector(".chat-advanced").hidden = true;
      setStatus("disabled", "idle");
      return;
    }
    if (!r.ok) throw new Error(`status ${r.status}`);
    disabledNoticeEl.hidden = true;
    formEl.hidden = false;
    document.querySelector(".chat-header").hidden = false;
    document.querySelector(".chat-advanced").hidden = false;
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
    // Network error: leave the dropdown alone (it might already have
    // options from a previous load) and surface the failure softly.
    console.warn("loadModels failed:", e);
  }
}

function buildMessages() {
  const messages = [];
  const system = systemEl.value.trim();
  if (system) {
    messages.push({ role: "system", content: system });
  }
  const history = loadHistory();
  for (const m of history) {
    if (m.role !== "system") messages.push({ role: m.role, content: m.content });
  }
  return messages;
}

async function send() {
  const text = inputEl.value;
  if (!text.trim()) return;

  // If a stream is already running, abort it before starting a new one.
  // See the module-level `inflight` note for why this is the chosen
  // policy (replacement rather than refusal).
  if (inflight) {
    inflight.controller.abort();
    inflight = null;
  }

  // Push the user turn now so the UI reflects the pending message even
  // if the network fails before the assistant reply starts.
  appendBubble("user", text);
  inputEl.value = "";
  const model = modelEl.value || undefined;
  const assistantEl = appendBubble("assistant", "", {
    persist: false,
    model,
  });

  const controller = new AbortController();
  inflight = { controller, assistantEl };
  setStreaming(true);
  setStatus("streaming…", "connecting");

  const body = {
    messages: [
      ...buildMessages().slice(0, -1), // drop the just-pushed user turn (it's already in history)
      { role: "user", content: text },
    ],
    stream: true,
  };
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
      // SSE messages are separated by a blank line.
      let sep;
      while ((sep = buffer.indexOf("\n\n")) !== -1) {
        const raw = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        for (const line of raw.split("\n")) {
          if (!line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (payload === "[DONE]") {
            reader.cancel();
            break;
          }
          if (!payload) continue;
          try {
            const evt = JSON.parse(payload);
            const delta = evt?.choices?.[0]?.delta?.content;
            if (typeof delta === "string" && delta.length > 0) {
              accumulated += delta;
              assistantEl.textContent = accumulated;
              messagesEl.scrollTop = messagesEl.scrollHeight;
            }
          } catch (_e) {
            // Skip malformed lines rather than aborting the stream —
            // one bad payload from upstream should not ruin the rest.
          }
        }
      }
    }
  } catch (e) {
    if (e?.name === "AbortError") {
      // User stopped mid-generation: keep whatever was streamed so far.
      assistantEl.textContent = accumulated || "(stopped)";
    } else {
      assistantEl.textContent = `[error] ${e?.message || e}`;
      appendError(e?.message || String(e));
    }
  } finally {
    // Persist the final assistant turn (success, partial, or error).
    const history = loadHistory();
    history.push({
      role: "assistant",
      content: assistantEl.textContent,
      ts: Date.now(),
      model,
    });
    saveHistory(history);
    inflight = null;
    setStreaming(false);
    setStatus("idle", "idle");
  }
}

function stop() {
  if (inflight) {
    inflight.controller.abort();
  }
}

function clearChat() {
  if (!confirm("Clear the conversation?")) return;
  try {
    localStorage.removeItem(HISTORY_KEY);
  } catch (_e) {}
  messagesEl.innerHTML = "";
  inputEl.focus();
}

// ---- Wire up the form -------------------------------------------------------

formEl.addEventListener("submit", (e) => {
  e.preventDefault();
  send();
});
stopBtn.addEventListener("click", stop);
clearBtn.addEventListener("click", clearChat);

// Ctrl/Cmd+Enter submits from the textarea; Esc while streaming
// triggers Stop.
inputEl.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
    e.preventDefault();
    send();
  } else if (e.key === "Escape" && inflight) {
    e.preventDefault();
    stop();
  }
});

// Hydrate the conversation immediately so reloads restore context, even
// before the user enters Discussion mode for the first time.
renderHistory();

// Lazy-load the model list on first entry to Discussion mode (and
// re-load when the tab becomes visible again, e.g. after the server
// was restarted while the tab was idle).
let modelsLoaded = false;
async function ensureModelsLoaded() {
  if (modelsLoaded) return;
  await loadModels();
  modelsLoaded = true;
}
document.addEventListener("modechange", (e) => {
  if (e?.detail?.mode === "discussion") ensureModelsLoaded();
});
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && globalThis.__nagentMode?.current() === "discussion") {
    // Re-check models when the tab becomes visible; harmless if the
    // dropdown is already populated.
    modelsLoaded = false;
    ensureModelsLoaded();
  }
});
