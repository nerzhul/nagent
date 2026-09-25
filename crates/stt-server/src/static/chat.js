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
//   As soon as a transcript has been routed into a request, the audio
//   session is torn down (`audioCapture.stop()`). There is no point
//   keeping the mic open while the LLM responds — the user has to
//   click Record again to send a follow-up voice turn, by design.
//   This differs from Transcript mode, where the recording stays
//   open across many utterances until the inactivity watchdog
//   (toggleable via `#inactivity-check`) decides the user is done.
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

// ---- Markdown rendering ----------------------------------------------------
//
// The LLM replies arrive as a token stream, but the user expects nicely
// formatted output (lists, code blocks, bold, etc.) by the time the
// reply is done. We parse the accumulated text with `marked` and
// sanitize the resulting HTML with `DOMPurify` before assigning it to
// the bubble's `innerHTML`. Both libraries are vendored under
// `static/vendor/` and exposed as globals by index.html, so we use the
// globals directly instead of importing (they ship as classic scripts,
// not ES modules).
//
// Streaming UX: re-parsing and re-sanitizing the full accumulated text
// on every token would be wasteful and could cause visible flicker if
// partial markdown is re-rendered into slightly different shapes. We
// coalesce updates via `requestAnimationFrame` so each animation frame
// sees at most one render, and we schedule the next frame lazily —
// tokens arriving faster than 60 fps only produce one render per frame
// anyway.
//
// User bubbles and error bubbles stay as plain `textContent`: we never
// render user input as markdown (to avoid confusing the user about what
// is "trusted"), and error messages are developer-facing text that
// shouldn't be parsed as markdown.

// Probe the vendor scripts once at module load. They are classic
// `<script>` tags in `<head>` that run before any `<body>` content, so
// this check is normally true — but a partial deploy / CDN failure /
// strict CSP could leave them undefined. `renderMarkdown` then degrades
// to a manually-escaped plain-text render instead of crashing the
// chat on the first reply.
const MARKDOWN_AVAILABLE =
  typeof window.marked?.parse === "function"
  && typeof window.DOMPurify?.sanitize === "function";

// KaTeX auto-render is loaded as a `defer` script in `<head>`. The
// `renderMathInElement` helper walks a DOM subtree and rewrites every
// `$...$`, `$$...$$`, `\(...\)`, `\[...\]` block into a KaTeX span.
// We probe at module load the same way we do for marked/DOMPurify —
// when the math bundle fails to load (or while it is still in flight
// on a slow connection), the markdown source falls through verbatim
// instead of throwing.
const KATEX_AVAILABLE =
  typeof window.renderMathInElement === "function";

// Delimiters matched by `renderMathInElement`. We keep the default
// set: `$$…$$` and `\[…\]` for display math, `$…$` and `\(…\)` for
// inline. `left: true` lets `\left( … \right)` auto-size braces.
const KATEX_RENDER_OPTIONS = {
  delimiters: [
    { left: "$$", right: "$$", display: true },
    { left: "$",  right: "$",  display: false },
    { left: "\\(", right: "\\)", display: false },
    { left: "\\[", right: "\\]", display: true },
  ],
  throwOnError: false, // bad LaTeX renders as red source, doesn't break the bubble
};

function renderMarkdown(text) {
  if (!text) return "";
  if (!MARKDOWN_AVAILABLE) {
    // Vendor scripts failed to load (404, blocked, parse error). Fall
    // back to a manually-escaped, plain-text render rather than
    // throwing on the first reply. The user still sees the reply, just
    // without markdown formatting.
    return escapeHtml(text).replace(/\n/g, "<br>");
  }
  // marked.parse returns an HTML string when called with a string input.
  // `breaks: true` makes single newlines become `<br>` (LLMs often
  // break lines without blank lines). `gfm: true` (default) enables
  // GitHub-flavored features: tables, task lists, fenced code blocks.
  const raw = window.marked.parse(text, { breaks: true, gfm: true });
  return window.DOMPurify.sanitize(raw, {
    // Anchor tags get forced-open in a new tab so a chat reply can't
    // navigate the nagent UI away. DOMPurify honors `ADD_ATTR` for the
    // `target` and `rel` we add below; everything else stays at the
    // default-deny baseline.
    ADD_ATTR: ["target", "rel"],
  });
}

// Minimal HTML escaper used only when the vendor libraries fail to
// load. We intentionally never interpolate user-controlled strings
// into the DOM via `innerHTML` outside of `renderMarkdown`, so this
// only ever sees the LLM's own output — but escaping it anyway keeps
// the degraded path safe by construction.
function escapeHtml(text) {
  return String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

// `\[ \]` → `\[\]` shorthand used by `normalizeMathDelimiters` below.
// Kept in one place so both regexes stay in sync if we ever extend
// the rule.
const LATEX_CMD = /\\[a-zA-Z]+/;

// Rewrite `[ ... ]` blocks that contain LaTeX commands into `\[...\]`,
// the delimiter KaTeX's auto-render recognizes for display math.
//
// The chat model sometimes wraps display math in single brackets
// (probably confusing the LaTeX bracket pair `\[...\]` with markdown
// link syntax `[text](url)`) and produces output like:
//
//     [
//     \text{foo} = \frac{a}{b}
//     ]
//
// Marked has no math support, so it leaves the brackets in place —
// KaTeX then has nothing to match against and the LaTeX source
// renders as plain text. This pre-pass detects `[ ... ]` whose inner
// content contains at least one `\command` and rewrites it to
// `\[...\]`. The check on the inner content keeps real markdown links
// like `[label](url)` untouched (their inner content has no
// backslash) and avoids eating arbitrary bracketed prose.
function normalizeMathDelimiters(text) {
  // Both regexes below include a `(?!\s*\()` lookahead on the closing
  // `]` so we don't eat the bracket pair of a real markdown link like
  // `[\frac{a}{b}](https://example.com)` — the lookahead sees the `(`
  // right after the `]` and refuses to rewrite.
  //
  // The single-line regex also carries a `(?<!\\)` lookbehind so it
  // does not re-process the output of the multi-line regex (the
  // multi-line pass leaves `\[...\]` blocks behind; the lookbehind
  // sees the leading `\` and skips them).
  //
  // Multi-line: `[\n ... \n]` (with optional surrounding whitespace)
  // — the format the LLM in this thread actually emits.
  text = text.replace(
    /\[\s*\n([\s\S]*?)\n\s*\](?!\s*\()/g,
    (m, inner) => {
      if (!LATEX_CMD.test(inner)) return m;
      return `\\[\n${inner}\n\\]`;
    },
  );
  // Single-line: `[\frac{a}{b}]` — defensive, in case a future model
  // drops the newlines. Only matches when the inner starts with a
  // backslash command so plain markdown links `[label](url)` are left
  // alone (their inner never starts with `\`). The lookbehind skips
  // blocks already wrapped by the multi-line pass above.
  text = text.replace(/(?<!\\)\[(\s*\\[a-zA-Z][\s\S]*?)\](?!\s*\()/g, (m, inner) =>
    `\\[${inner}\\]`,
  );
  return text;
}

function renderMarkdown(text) {
  if (!text) return "";
  if (!MARKDOWN_AVAILABLE) {
    // Vendor scripts failed to load (404, blocked, parse error). Fall
    // back to a manually-escaped, plain-text render rather than
    // throwing on the first reply. The user still sees the reply, just
    // without markdown formatting. Math normalization still runs so a
    // future vendor reload benefits from a clean source — the result
    // is just plain text but consistent.
    const normalized = normalizeMathDelimiters(text);
    return escapeHtml(normalized).replace(/\n/g, "<br>");
  }
  // Rewrite `[ ... ]`-wrapped LaTeX into `\[...\]` so KaTeX has
  // delimiters to match. See `normalizeMathDelimiters` for the
  // exact rules. The marked call below runs after this rewrite.
  const normalized = normalizeMathDelimiters(text);
  // marked.parse returns an HTML string when called with a string input.
  // `breaks: true` makes single newlines become `<br>` (LLMs often
  // break lines without blank lines). `gfm: true` (default) enables
  // GitHub-flavored features: tables, task lists, fenced code blocks.
  const raw = window.marked.parse(normalized, { breaks: true, gfm: true });
  return window.DOMPurify.sanitize(raw, {
    // Anchor tags get forced-open in a new tab so a chat reply can't
    // navigate the nagent UI away. DOMPurify honors `ADD_ATTR` for the
    // `target` and `rel` we add below; everything else stays at the
    // default-deny baseline.
    ADD_ATTR: ["target", "rel"],
  });
}

// Patch every <a> inside the bubble so it opens in a new tab with
// `rel="noopener noreferrer"`. We do this after DOMPurify because
// DOMPurify would strip `target`/`rel` from otherwise unsafe URLs —
// the post-pass keeps the sanitization guarantee but adds safe-link
// behavior for the ones DOMPurify kept.
function decorateSafeLinks(root) {
  const links = root.querySelectorAll("a[href]");
  for (const a of links) {
    a.target = "_blank";
    a.rel = "noopener noreferrer";
  }
}

// Run KaTeX's auto-render over the bubble to convert `$…$`, `$$…$$`,
// `\(…\)`, `\[…\]` blocks into rendered math. KaTeX edits the DOM
// in-place (replacing the source text with a `<span class="katex">`)
// so we don't need to track the previous render — calling it again
// on the same subtree is a no-op because the math delimiters are
// gone after the first pass.
function renderMath(root) {
  if (!KATEX_AVAILABLE) return;
  try {
    window.renderMathInElement(root, KATEX_RENDER_OPTIONS);
  } catch (e) {
    // `throwOnError: false` already swallows LaTeX syntax errors, so
    // anything that lands here is a real bug in our call (or in the
    // library). Log it once and keep the bubble functional rather
    // than tearing down the stream on a transient failure.
    console.warn("KaTeX render failed:", e);
  }
}

function applyMarkdown(bubbleEl, text) {
  bubbleEl.innerHTML = renderMarkdown(text);
  decorateSafeLinks(bubbleEl);
  renderMath(bubbleEl);
}

// Schedule a markdown re-render of `bubbleEl` for the next animation
// frame. If a render is already pending, the call is a no-op — the
// pending render will use the latest `accumulated` text by closure.
// This keeps the streaming path at most one render per frame even if
// tokens arrive in bursts.
function scheduleMarkdownRender(bubbleEl, getText) {
  if (bubbleEl.dataset.mdPending === "1") return;
  bubbleEl.dataset.mdPending = "1";
  requestAnimationFrame(() => {
    bubbleEl.dataset.mdPending = "0";
    applyMarkdown(bubbleEl, getText());
  });
}

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
// and `AudioCapture` (low priority). The streaming state carries both
// a text and a class so we can distinguish "model is loading" (spinner)
// from "tokens are streaming in" (spinner) and from idle / error.
//
// While a reply is in flight we also hold the audio watchdog open —
// the user is silent (waiting for Ollama to finish a cold start or
// stream tokens), and the inactivity watchdog would otherwise close
// the audio session mid-response.

let lastAudioStatus = { text: "idle", cls: "idle" };
let lastStreamState = null; // null when no reply in flight, else { text, cls }
function renderStatus() {
  const { text, cls } = lastStreamState ?? lastAudioStatus;
  statusEl.textContent = text;
  statusEl.className = "status " + cls;
}
function setAudioStatus(text, cls) {
  lastAudioStatus = { text, cls };
  renderStatus();
}
function setStreamState(state) {
  lastStreamState = state;
  renderStatus();
}

function setStreamingUi(streaming) {
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
    // Re-render assistant history as markdown so a reload (or a
    // client that joined the session late) sees formatted replies,
    // not the raw `**bold**` source. User history stays plain text.
    const markdown = msg.role === "assistant";
    appendBubble(msg.role, msg.content, {
      persist: false,
      model: msg.model,
      markdown,
    });
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function appendBubble(role, text, { persist = true, model = null, markdown = false } = {}) {
  const div = document.createElement("div");
  const classes = [`chat-message`, `chat-${role}`];
  // The `--markdown` modifier unlocks `white-space: normal` and the
  // markdown element styling in style.css. Without it the bubble
  // would inherit `white-space: pre-wrap` and show `## Heading`
  // literally instead of as a heading.
  if (markdown) classes.push("chat-message--markdown");
  div.className = classes.join(" ");
  if (model && role === "assistant") div.dataset.model = model;
  if (markdown) {
    // Sanitized HTML render of the assistant reply. The text source is
    // persisted (below) — only the live bubble uses innerHTML so a
    // prompt-injection reply cannot smuggle scripts into the chat UI.
    applyMarkdown(div, text);
  } else {
    // User bubbles always render as plain text — we trust user input
    // but never interpret it as markdown so the user sees exactly
    // what they typed and we never try to render a hostile prompt as
    // a link.
    div.textContent = text;
  }
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
  const assistantEl = appendBubble("assistant", "", { persist: false, model, markdown: true });
  // Render an inline bouncing-dots loader inside the assistant bubble
  // while we wait for the LLM's first token. Without this, the bubble
  // sits empty during the Ollama cold start (sometimes 30s+ on a
  // freshly-pulled model) and looks like a frozen UI. The first delta
  // below clears the loader before any text is written.
  assistantEl.innerHTML =
    '<span class="chat-loader" role="status" aria-label="Loading response">'
    + '<span class="dot"></span>'
    + '<span class="dot"></span>'
    + '<span class="dot"></span>'
    + '</span>';

  const controller = new AbortController();
  inflight = { controller, assistantEl, model };
  setStreamingUi(true);
  setStreamState({ text: "Loading model…", cls: "loading" });
  // In chat mode we don't keep listening while the LLM responds:
  // the user has nothing to add until the reply is done, and an open
  // mic just burns CPU and risks accidental utterances being sent as
  // a follow-up turn. They can click Record again afterwards. We call
  // this after `setStreamState` so the chat pill (priority) doesn't
  // briefly flash the audio idle status during the teardown.
  audioCapture.stop();

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
  // `finalSource` is the text persisted into history at the end of
  // the turn. On a clean stream it equals `accumulated` (raw markdown
  // source, so reloads re-render with the same vendor libraries). On
  // an abort with no tokens, or a non-abort error, it is overwritten
  // below to match whatever the live bubble ended up showing — that
  // way a page reload reproduces what the user just saw, instead of
  // surfacing a stale "here's the partial markdown source" entry.
  let finalSource = "";
  // First-token arrival switches the pill from "Loading model…" to
  // "Streaming…"; the same `connecting` class keeps the spinner
  // animated so the user keeps getting live feedback.
  let streamingStarted = false;
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
              if (!streamingStarted) {
                streamingStarted = true;
                setStreamState({ text: "Streaming…", cls: "connecting" });
                // Strip the inline loader before writing real text so
                // the bubble transitions cleanly into the reply.
                assistantEl.innerHTML = "";
              }
              accumulated += delta;
              // Re-render the accumulated text as sanitized markdown.
              // We coalesce updates via requestAnimationFrame so a
              // burst of small tokens only triggers one parse per
              // animation frame, keeping the streaming path cheap.
              scheduleMarkdownRender(assistantEl, () => accumulated);
              messagesEl.scrollTop = messagesEl.scrollHeight;
            }
          } catch (_e) { /* skip malformed line */ }
        }
      }
    }
    finalSource = accumulated;
  } catch (e) {
    if (e?.name === "AbortError") {
      // Render whatever we got as markdown so the user sees the
      // partial reply formatted the same way as a full one. If we
      // never received any tokens, fall back to a plain "(stopped)"
      // marker so the bubble isn't empty.
      if (accumulated) {
        applyMarkdown(assistantEl, accumulated);
        finalSource = accumulated;
      } else {
        assistantEl.textContent = "(stopped)";
        finalSource = "(stopped)";
      }
    } else {
      // Error markers stay plain text — they are developer-facing
      // diagnostics, not part of the LLM's markdown output.
      const errText = `[error] ${e?.message || e}`;
      assistantEl.textContent = errText;
      finalSource = errText;
      appendError(e?.message || String(e));
    }
  } finally {
    // Persist the raw markdown source (or the error/stopped marker)
    // rather than the rendered HTML, so the history stays small,
    // portable, and re-renderable on clients that load the page
    // later. The live bubble keeps its sanitized innerHTML.
    const h = loadHistory();
    h.push({ role: "assistant", content: finalSource, ts: Date.now(), model });
    saveHistory(h);
    inflight = null;
    // Clear the streaming status so the pill falls back to the audio
    // state (typically "idle") before we drop the input-disable.
    setStreamState(null);
    setStreamingUi(false);
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
