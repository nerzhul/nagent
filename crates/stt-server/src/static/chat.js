// nagent — Discussion mode chat UI.
//
// Streams tokens from `/v1/chat/completions` (proxied to Ollama or any
// OpenAI-compatible endpoint). History is kept in `localStorage` only;
// no server-side session is involved.
//
// Sessions:
//   Multiple independent chat threads are supported, each with its own
//   message history. The persistence model is still localStorage — the
//   previous single-history layout (`nagent.chat.history`) is migrated
//   on first boot into a single legacy session, after which every
//   session lives under its own key. See the `SessionStore` block for
//   the exact layout and the `migrateLegacy()` routine for the upgrade
//   path. The wire format sent to the LLM is unchanged: only the
//   per-session message list is in scope at any given time.
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
//   waits for the current reply to finish. Switching sessions aborts
//   the in-flight reply and resets the queue so queued turns from a
//   previous session never bleed into the new one.

import { AudioCapture } from "/static/audio.js";
import { preselectFromBrowser } from "/static/lang-preselect.js";
import {
  DEFAULT_TITLE,
  createSessionObj,
  deleteSession as storeDeleteSession,
  deriveTitle,
  getActiveId,
  historyKey,
  loadHistory,
  loadSessions,
  migrateLegacy,
  persistNewSession,
  renameSession,
  saveHistory,
  setActiveId,
  sortedSessions,
  touchSession,
} from "/static/chat-sessions.js";

// ---- Session storage -------------------------------------------------------
//
// The localStorage layout and the per-session helpers live in
// `chat-sessions.js` (extracted so they can be unit-tested without a
// DOM). This file owns the UI side: the sidebar rendering, the
// session lifecycle (new / switch / delete), and the binding between
// streamed turns and the session they belong to.
//
// Three localStorage keys cooperate to keep the multi-session state:
//
//   nagent.chat.sessions          -> JSON array of session descriptors
//                                   [{ id, title, createdAt, updatedAt }]
//                                   sorted by `updatedAt` descending so
//                                   the sidebar can render in MRU order
//                                   without re-sorting.
//   nagent.chat.active            -> the active session id (string).
//   nagent.chat.session.<id>      -> JSON array of message records for
//                                   that session, the same shape the
//                                   single-history layout used
//                                   ({ role, content, ts, model }).
//
// The previous layout stored the whole message list under
// `nagent.chat.history`. `migrateLegacy()` wraps that list into a
// single session on first boot and removes the old key, so a reload
// from an older build keeps the user's conversation.

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
// `systemEl.value` is an *extension* appended after the server's
// default system prompt (env `LLM_SYSTEM_PROMPT` / TOML
// `[llm].system_prompt`); the proxy prepends the admin's prompt as
// `messages[0]`, our value goes second so the admin's intent stays
// authoritative.
const systemEl     = $("chat-system");
const tempEl       = $("chat-temperature");
const statusEl     = $("chat-status");
const disabledNoticeEl = $("chat-disabled-notice");
const sessionsListEl = $("chat-sessions");
const newSessionBtnEl = $("chat-new-session");
const agentsBannerEl = $("chat-agents-banner");
const agentsBannerNamesEl = $("chat-agents-banner-names");

// The active session id is read at every operation rather than
// cached, so a same-tab mutation (delete, new chat, switch from the
// sidebar) immediately takes effect without bookkeeping. `loadSessions`
// is the source of truth — if the cached `currentSessionId` is no
// longer valid, we fall back to the most-recently-updated session or
// create a fresh one. This keeps the invariant "there is always
// exactly one active session" without any explicit init step.
let currentSessionId = "";
function activeSessionId() {
  const sessions = loadSessions();
  if (sessions.some((s) => s.id === currentSessionId)) return currentSessionId;
  // Fallback: most-recent first, else create. `sortedSessions` is
  // safe on an empty array.
  const sorted = sortedSessions(sessions);
  const fallback = sorted[0] || null;
  if (fallback) {
    currentSessionId = fallback.id;
    setActiveId(currentSessionId);
    return currentSessionId;
  }
  const fresh = createSessionObj();
  persistNewSession(fresh);
  currentSessionId = fresh.id;
  setActiveId(currentSessionId);
  renderSessionList();
  return currentSessionId;
}

// One stream at a time. If a new turn arrives while a reply is
// streaming, the reply continues to completion and the new turn runs
// immediately after (no abort — the user can keep recording without
// cutting the current reply short). `inflight.sessionId` records which
// session the request belongs to so an abort persisted via the
// `finally` block writes its "(stopped)" / partial marker into the
// right place even after the user switched sessions.
let inflight = null; // { controller, assistantEl, model, sessionId }

// Serialize user turns so a reply never overlaps another reply. The
// queue also guards against cross-session bleed: when a session is
// switched, `resetTurnQueue()` reassigns the head promise so anything
// queued for the previous session is dropped before it can run.
let turnQueue = Promise.resolve();
function enqueueTurn(fn) {
  turnQueue = turnQueue.then(fn, fn);
  return turnQueue;
}
function resetTurnQueue() {
  // Replace the chain with a fresh resolved promise. Handlers already
  // attached to the old chain still resolve (each `then` is its own
  // microtask), but anything queued *after* this point lands on the
  // new chain and runs against the now-active session.
  turnQueue = Promise.resolve();
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
//
// Per-session message lists live under `nagent.chat.session.<id>`.
// `loadHistory` / `saveHistory` take a session id explicitly so the
// caller can never accidentally read from or write to the wrong
// session — every persist path passes `currentSessionId` (or
// `inflight.sessionId`, in the abort/finally branch) and never reads
// `currentSessionId` lazily. The list shape is unchanged from the
// previous single-history layout (`{ role, content, ts, model }`), so
// a render after a reload reproduces the same view as before.

function renderHistory(sessionId) {
  messagesEl.innerHTML = "";
  const history = loadHistory(sessionId);
  // Track the assistant bubble the *current* tool-bubble cluster
  // should hang off. When the history contains the pattern
  // `assistant(tool_calls) → tool → … → assistant(final)`, the
  // tool bubbles belong under the *first* assistant so the second
  // assistant renders after them (matching the live-streaming
  // layout). We anchor with `toolAnchorEl`, which only advances on
  // a non-tool_call-bearing assistant.
  let lastUserOrFinalAssistantEl = null;
  let toolAnchorEl = null;
  for (const msg of history) {
    if (msg.role === "tool") {
      // Render each tool bubble under the current tool anchor so
      // reloads reproduce the same DOM as the live stream.
      if (toolAnchorEl) {
        const id = msg.tool_call_id || "";
        appendToolBubble(
          null,
          { id, name: msg.name || "tool", args: null, index: 0 },
          toolAnchorEl,
        );
        resolveToolBubble(null, {
          id,
          name: msg.name || "tool",
          ok: true,
          summary: msg.content ? truncateSummary(msg.content) : "",
          content: msg.content,
        });
      }
      continue;
    }
    if (msg.role === "assistant") {
      const hasToolCalls = Array.isArray(msg.tool_calls) && msg.tool_calls.length > 0;
      const bubble = appendBubble("assistant", msg.content || "", {
        persist: false,
        model: msg.model,
        markdown: true,
        sessionId,
      });
      if (hasToolCalls) {
        // Anchor subsequent tool bubbles under this assistant; do
        // NOT update `lastUserOrFinalAssistantEl` so any later
        // non-tool user/system message still anchors correctly.
        toolAnchorEl = bubble;
        for (const tc of msg.tool_calls) {
          const args = (() => {
            try { return JSON.parse(tc.function?.arguments || "{}"); }
            catch (_e) { return {}; }
          })();
          appendToolBubble(
            null,
            {
              id: tc.id,
              name: tc.function?.name || "tool",
              args,
              index: 0,
            },
            bubble,
          );
        }
        // The assistant may also have rendered text alongside the
        // tool calls (the LLM often says "Let me check…"). Keep
        // the bubble as the anchor.
      } else {
        // Final reply (no tool_calls): a new anchor.
        lastUserOrFinalAssistantEl = bubble;
        toolAnchorEl = bubble;
      }
      continue;
    }
    // user / system
    const bubble = appendBubble(msg.role, msg.content || "", {
      persist: false,
      model: msg.model,
      markdown: false,
      sessionId,
    });
    lastUserOrFinalAssistantEl = bubble;
    toolAnchorEl = null;
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function appendBubble(role, text, {
  persist = true, model = null, markdown = false, sessionId = null,
} = {}) {
  // `sessionId` is required on every persist path: a missing id is a
  // programming error, and silently dropping into the active session
  // is exactly the cross-session bleed this refactor is meant to
  // prevent. Render-only callers (history hydration) pass `persist:
  // false` and don't need a sessionId.
  const sid = sessionId ?? (persist ? activeSessionId() : null);
  if (persist && !sid) {
    console.warn("appendBubble called without a sessionId; skipping persist");
    return null;
  }
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
    const history = loadHistory(sid);
    history.push({ role, content: text, ts: Date.now(), model });
    saveHistory(sid, history);
    // First user message of a session seeds its title. We only do
    // this for the very first user turn so a long-running session
    // doesn't keep rewriting its title on every subsequent message.
    if (role === "user") {
      const userCount = history.filter((m) => m.role === "user").length;
      if (userCount === 1) {
        renameSession(sid, deriveTitle(text));
      } else {
        touchSession(sid);
      }
    } else {
      touchSession(sid);
    }
    renderSessionList();
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

// ---- Tool bubbles -------------------------------------------------------
//
// The LLM proxy emits two named SSE events mid-stream whenever an
// agent runs:
//
//   event: tool_call    { id, name, args, ... }
//   event: tool_result  { id, name, ok, summary, content }
//
// Each `tool_call` is rendered as a live bubble below the in-flight
// assistant message; the corresponding `tool_result` swaps the
// spinner for a `✓ <summary>` line and (when the assistant had
// `tool_calls[]`) persists a `role: "assistant" + tool_calls` entry
// followed by a `role: "tool"` entry into the chat history so a
// reload or follow-up turn keeps the agent's result in the LLM's
// context.
//
// The DOM splits the assistant message for rendering only — the
// storage model is still OpenAI-flavoured: an assistant turn with
// `tool_calls[]`, then a `role: "tool"` turn for each result. That
// matches the wire format the LLM proxy already emits and what
// Ollama expects on the next round.

const TOOL_ICON = {
  web_fetch: "\u{1F50E}", // 🔎
};

function toolIcon(name) {
  return TOOL_ICON[name] || "\u{1F6E0}"; // 🔧 (default wrench)
}

/**
 * Render a tool bubble under `assistantEl`. Returns the bubble so
 * the matching `tool_result` event can swap its contents in place.
 *
 * `persist` defaults to true: the assistant's `tool_calls[]` entry
 * is written to history now (so reloads see it even before the agent
 * returns), and the matching `role: "tool"` entry is appended when
 * the result lands.
 */
function appendToolBubble(
  sessionId,
  { id, name, args, index },
  assistantEl,
) {
  const div = document.createElement("div");
  div.className = "chat-tool-bubble chat-tool-bubble--running";
  div.dataset.toolId = id;
  div.dataset.toolName = name;

  const icon = document.createElement("span");
  icon.className = "chat-tool-icon";
  icon.textContent = toolIcon(name);
  div.appendChild(icon);

  const nameEl = document.createElement("span");
  nameEl.className = "chat-tool-name";
  nameEl.textContent = name;
  div.appendChild(nameEl);

  // First-line context: URL for web_fetch, raw args otherwise.
  const detailEl = document.createElement("span");
  detailEl.className = "chat-tool-detail";
  if (name === "web_fetch" && args && typeof args === "object" && args.url) {
    detailEl.textContent = String(args.url);
  } else if (args && Object.keys(args).length > 0) {
    detailEl.textContent = JSON.stringify(args);
  } else {
    detailEl.textContent = "…";
  }
  div.appendChild(detailEl);

  const statusEl = document.createElement("span");
  statusEl.className = "chat-tool-status";
  statusEl.textContent = "running…";
  div.appendChild(statusEl);

  // Insert directly after the assistant bubble so the live tool
  // appears under the message that requested it, in source order.
  if (assistantEl && assistantEl.parentNode === messagesEl) {
    assistantEl.insertAdjacentElement("afterend", div);
  } else {
    messagesEl.appendChild(div);
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;

  // Persist a placeholder assistant turn with `tool_calls[]` so a
  // page reload / follow-up turn keeps the LLM context intact even
  // before the agent returns. We only do this the first time we see
  // the tool_call — the tool_result path appends the role:tool entry.
  if (sessionId && !div.dataset.persisted) {
    div.dataset.persisted = "1";
    const h = loadHistory(sessionId);
    const last = h[h.length - 1];
    // If the previous entry was already an assistant turn written
    // by the streaming layer with no content, fold the tool_calls[]
    // into it so we don't end up with two back-to-back assistant
    // messages. Otherwise append a fresh assistant turn.
    if (last && last.role === "assistant"
        && !last.tool_calls && (last.content == null || last.content === "")) {
      last.tool_calls = [{
        id,
        type: "function",
        function: { name, arguments: JSON.stringify(args || {}) },
      }];
      h[h.length - 1] = last;
    } else {
      h.push({
        role: "assistant",
        content: "",
        tool_calls: [{
          id,
          type: "function",
          function: { name, arguments: JSON.stringify(args || {}) },
        }],
        ts: Date.now(),
      });
    }
    saveHistory(sessionId, h);
  }
  return div;
}

/**
 * Update a tool bubble in place when the server emits the matching
 * `tool_result` event. Also appends the `role: "tool"` history
 * entry so the result survives reloads and rides along in future
 * LLM turns.
 */
function resolveToolBubble(sessionId, { id, name, ok, summary, content }) {
  const div = messagesEl.querySelector(`.chat-tool-bubble[data-tool-id="${CSS.escape(id)}"]`);
  if (div) {
    div.classList.remove("chat-tool-bubble--running");
    div.classList.add(ok ? "chat-tool-bubble--ok" : "chat-tool-bubble--error");
    const statusEl = div.querySelector(".chat-tool-status");
    if (statusEl) {
      statusEl.textContent = ok ? `✓ ${truncateSummary(summary)}` : `⚠ ${truncateSummary(summary)}`;
    }
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }
  if (sessionId) {
    const h = loadHistory(sessionId);
    h.push({
      role: "tool",
      tool_call_id: id,
      content: content == null ? "" : String(content),
      ts: Date.now(),
    });
    saveHistory(sessionId, h);
  }
}

function truncateSummary(s) {
  if (!s) return "";
  const str = String(s);
  return str.length > 120 ? str.slice(0, 117) + "…" : str;
}

// ---- Models ----------------------------------------------------------------

// Fetch /v1/agents on Discussion mount and surface a hint banner
// when at least one agent is wired up. Hidden when the list is empty
// or the server returns 404 (agents disabled). Failures are
// swallowed silently — the chat is the primary surface, the banner
// is cosmetic.
async function loadAgentsBanner() {
  if (!agentsBannerEl) return;
  try {
    const r = await fetch("/v1/agents", { cache: "no-store" });
    if (!r.ok) {
      agentsBannerEl.hidden = true;
      return;
    }
    const body = await r.json();
    const agents = Array.isArray(body?.data) ? body.data : [];
    if (agents.length === 0) {
      agentsBannerEl.hidden = true;
      return;
    }
    if (agentsBannerNamesEl) {
      agentsBannerNamesEl.textContent = agents.map((a) => a.name).join(", ");
    }
    agentsBannerEl.hidden = false;
  } catch (_e) {
    agentsBannerEl.hidden = true;
  }
}

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

// ---- Streaming reply -------------------------------------------------------
//
// `streamReply(sessionId, userText)` runs the LLM request bound to a
// specific session. The session id is captured at call time and used
// for every read/write — never re-read from `activeSessionId()` — so a
// mid-flight session switch can't route the assistant bubble or its
// persisted entry into the wrong conversation.

async function streamReply(sessionId, userText) {
  // Build the request from the captured history (the user turn was
  // already appended by `submitUserTurn`, so the last entry IS the
  // new user message — we re-include it explicitly so we don't depend
  // on history-load timing).
  const history = loadHistory(sessionId);
  const last = history[history.length - 1];
  if (!last || last.role !== "user") return; // nothing to reply to
  const earlier = history.slice(0, -1);

  const model = modelEl.value || undefined;
  const assistantEl = appendBubble("assistant", "", {
    persist: false, model, markdown: true, sessionId,
  });
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
  inflight = { controller, assistantEl, model, sessionId };
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
    { role: "user", content: userText != null ? userText : last.content },
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
    // Track the current SSE event name so we can route `event:
    // tool_call` / `event: tool_result` to the tool-bubble layer
    // instead of the `data:` parser. The SSE spec accumulates `data:`
    // lines until a blank line, then dispatches the event named by
    // the most recent `event:` line (or `message` if absent).
    let currentEventName = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let sep;
      while ((sep = buffer.indexOf("\n\n")) !== -1) {
        const raw = buffer.slice(0, sep);
        buffer = buffer.slice(sep + 2);
        currentEventName = "";
        let dataPayload = "";
        for (const line of raw.split("\n")) {
          if (!line) continue;
          if (line.startsWith(":")) continue; // SSE comment
          if (line.startsWith("event:")) {
            currentEventName = line.slice(6).trim();
            continue;
          }
          if (line.startsWith("data:")) {
            // Multi-line `data:` is concatenated with a single `\n`
            // per the SSE spec (lines arrive individually and the
            // parser joins them). We do NOT add a trailing newline
            // — the assembled string must be valid JSON for
            // `JSON.parse`, and trailing whitespace throws. JSON.parse
            // is called on `dataPayload.trim()` at the dispatch sites
            // below so multi-line payloads still round-trip cleanly.
            if (dataPayload.length > 0) dataPayload += "\n";
            dataPayload += line.slice(5).replace(/^ /, "");
            continue;
          }
          // Other fields (id:, retry:, …) are ignored.
        }
        if (currentEventName === "tool_call") {
          if (dataPayload) {
            try {
              const evt = JSON.parse(dataPayload.trim());
              appendToolBubble(sessionId, evt, assistantEl);
              setStreamState({ text: `Working…`, cls: "connecting" });
            } catch (e) {
              console.warn("tool_call parse failed:", e, dataPayload);
            }
          }
          continue;
        }
        if (currentEventName === "tool_result") {
          if (dataPayload) {
            try {
              const evt = JSON.parse(dataPayload.trim());
              resolveToolBubble(sessionId, evt);
            } catch (e) {
              console.warn("tool_result parse failed:", e, dataPayload);
            }
          }
          continue;
        }
        if (currentEventName === "error") {
          if (dataPayload) {
            try {
              const evt = JSON.parse(dataPayload.trim());
              appendError(evt?.detail || evt?.error || "agent loop error");
            } catch (_e) {
              appendError("agent loop error");
            }
          }
          continue;
        }
        if (!dataPayload) continue;
        const payload = dataPayload.trim();
        if (payload === "[DONE]") { reader.cancel(); break; }
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
    // later. The live bubble keeps its sanitized innerHTML. We use
    // `inflight.sessionId` (captured at request start) rather than
    // `activeSessionId()` so the abort branch still writes to the
    // correct session after a switch.
    const targetSessionId = inflight?.sessionId || sessionId;
    const h = loadHistory(targetSessionId);
    h.push({ role: "assistant", content: finalSource, ts: Date.now(), model });
    saveHistory(targetSessionId, h);
    touchSession(targetSessionId);
    renderSessionList();
    inflight = null;
    // Clear the streaming status so the pill falls back to the audio
    // state (typically "idle") before we drop the input-disable.
    setStreamState(null);
    setStreamingUi(false);
  }
}

// ---- User turn entry points ------------------------------------------------

async function submitUserTurn(sessionId, text) {
  const trimmed = (text || "").trim();
  if (!trimmed) return;
  appendBubble("user", trimmed, { sessionId });
  await streamReply(sessionId, trimmed);
}

function sendTyped() {
  const text = inputEl.value;
  inputEl.value = "";
  // Capture the session id at send time so the queued turn still
  // belongs to the session the user addressed. If the user switches
  // sessions before the queue drains, `resetTurnQueue()` in the
  // switch handler drops this turn — that's intentional, the user
  // explicitly moved away.
  const sessionId = activeSessionId();
  enqueueTurn(() => submitUserTurn(sessionId, text));
}

function receiveTranscript(text) {
  // No empty-text guard here: AudioCapture already filters empty
  // FinalTranscripts at the wire-protocol level.
  const sessionId = activeSessionId();
  enqueueTurn(() => submitUserTurn(sessionId, text));
}

function stop() {
  if (inflight) inflight.controller.abort();
}

function clearChat() {
  // "Clear" deletes the *active* session entirely: the message list,
  // the sidebar entry, and the active-id pointer all go away. This
  // matches the user's mental model — "clear" means the conversation
  // is gone, not that an empty shell is left in the sidebar with a
  // "New chat" placeholder. The per-session × button still exists for
  // users who want to remove a non-active session.
  //
  // We tear down any in-flight reply first so a streaming LLM doesn't
  // race the DOM clear (the abort path writes its marker into the
  // session that was active at request start, but `deleteSession`
  // below removes that session entirely — so the marker is persisted
  // briefly into a session that's about to be wiped, which is the
  // right behaviour: nothing survives).
  if (!confirm("Clear the conversation?")) return;
  const sid = activeSessionId();
  if (inflight) inflight.controller.abort();
  resetTurnQueue();
  // Wipe the session from the store. This removes the message key,
  // drops the descriptor, and clears the active-id pointer (handled
  // by `deleteSession`).
  deleteSession(sid);
  // Pick a replacement active session. If nothing is left we create a
  // fresh empty one so the chat input always has somewhere to land.
  const remaining = sortedSessions(loadSessions());
  if (remaining.length > 0) {
    switchToSession(remaining[0].id);
  } else {
    newSession();
  }
}

// ---- Wire up the form -------------------------------------------------------

formEl.addEventListener("submit", (e) => {
  e.preventDefault();
  sendTyped();
});
stopBtn.addEventListener("click", stop);
clearBtn.addEventListener("click", clearChat);

inputEl.addEventListener("keydown", (e) => {
  // Keyboard model:
  //   Enter           -> send
  //   Ctrl/Cmd+Enter  -> newline (fall through to textarea default)
  //   Shift+Enter     -> newline (fall through to textarea default)
  //   Escape          -> stop an in-flight reply
  //
  // We only `preventDefault` on plain Enter so the newline insertion
  // path stays the browser's default (no manual `value += "\n"` dance).
  // The `isComposing` guard avoids hijacking Enter while an IME is
  // open — pressing Enter to confirm a CJK candidate must not send
  // the half-typed composition as a message.
  if (e.key === "Enter"
      && !e.shiftKey && !e.ctrlKey && !e.metaKey
      && !e.isComposing) {
    e.preventDefault();
    sendTyped();
  } else if (e.key === "Escape" && inflight) {
    e.preventDefault();
    stop();
  }
});

// Global voice shortcut: Ctrl+Shift+D (or Cmd+Shift+D on macOS)
// toggles the voice recording session, regardless of which element
// currently has focus. Bound at the document level so the user can
// trigger it from the textarea, the sidebar, or anywhere else in
// the Discussion view without first clicking the mic button.
//
// We gate on `globalThis.__nagentMode?.current() === "discussion"`
// so the shortcut is a no-op while the Transcript view is active —
// no point starting a voice capture that would land in the wrong
// mode's transcript. `preventDefault` stops Chromium from
// interpreting Ctrl+D as "bookmark this page", which would steal
// focus and pop a dialog on some browsers.
document.addEventListener("keydown", (e) => {
  if (globalThis.__nagentMode?.current() !== "discussion") return;
  if (e.key !== "D" && e.key !== "d") return;
  if (!e.shiftKey) return;
  if (!e.ctrlKey && !e.metaKey) return;
  if (e.altKey) return;
  e.preventDefault();
  audioCapture.toggle();
});

// ---- Audio capture (Discussion mode) ---------------------------------------

const audioCapture = new AudioCapture({
  buttonEl: $("chat-record-btn"),
  statusEl: null, // merged into #chat-status via the onStatusChange callback
  // Shared oscilloscope: same DOM element as Transcript mode, so a
  // single waveform shows up regardless of which view the user is
  // in when they click Record.
  canvasEl: $("voice-graph-shared-canvas"),
  levelEl:  $("voice-graph-shared-level"),
  graphEl:  $("voice-graph-shared"),
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

// ---- Session management ---------------------------------------------------
//
// Sidebar primitives. `renderSessionList` is the single source of UI
// truth for the sidebar — every mutation (new / delete / switch /
// rename-on-first-turn) calls back into it so the DOM and the
// `loadSessions()` snapshot can't drift apart. The active item is
// styled via `aria-current="true"` and a dedicated class so screen
// readers and CSS both pick it up.

function deleteSession(id) {
  // The store handles the storage side; we additionally clear the
  // active-id pointer so a follow-up call to `activeSessionId()` picks
  // a replacement from whatever's left.
  storeDeleteSession(id);
  if (currentSessionId === id) {
    currentSessionId = "";
    setActiveId("");
  }
}

function switchToSession(id) {
  if (!id || id === currentSessionId) return;
  // Tear down the current reply (if any) and drop anything queued for
  // the previous session. The `finally` block in `streamReply` writes
  // the abort marker to the *previous* session via `inflight.sessionId`,
  // so the user sees the partial reply in the conversation they
  // actually addressed.
  if (inflight) inflight.controller.abort();
  resetTurnQueue();
  currentSessionId = id;
  setActiveId(id);
  // Re-render the message list from the new session's history. The
  // streaming UI is already torn down by the abort path; we just need
  // to swap the bubbles.
  renderHistory(id);
  renderSessionList();
  inputEl.focus();
}

function newSession() {
  const session = createSessionObj();
  persistNewSession(session);
  // Same teardown as a switch — the user is moving away from whatever
  // is on screen, so an in-flight reply is interrupted.
  if (inflight) inflight.controller.abort();
  resetTurnQueue();
  currentSessionId = session.id;
  setActiveId(session.id);
  renderSessionList();
  renderHistory(session.id);
  inputEl.focus();
}

function renderSessionList() {
  if (!sessionsListEl) return;
  sessionsListEl.innerHTML = "";
  const sessions = sortedSessions(loadSessions());
  for (const s of sessions) {
    const li = document.createElement("li");
    li.className = "chat-session-item";
    li.dataset.sessionId = s.id;
    if (s.id === currentSessionId) {
      li.classList.add("chat-session-item--active");
      li.setAttribute("aria-current", "true");
    }
    const titleBtn = document.createElement("button");
    titleBtn.type = "button";
    titleBtn.className = "chat-session-title";
    titleBtn.textContent = s.title || DEFAULT_TITLE;
    titleBtn.title = s.title || DEFAULT_TITLE;
    titleBtn.addEventListener("click", () => switchToSession(s.id));
    const delBtn = document.createElement("button");
    delBtn.type = "button";
    delBtn.className = "chat-session-delete";
    delBtn.setAttribute("aria-label", `Delete session “${s.title || DEFAULT_TITLE}”`);
    delBtn.title = "Delete session";
    delBtn.textContent = "×";
    delBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      if (!confirm(`Delete “${s.title || DEFAULT_TITLE}”?`)) return;
      const wasActive = s.id === currentSessionId;
      deleteSession(s.id);
      // Pick a replacement for the active session if we just deleted
      // it. Otherwise the next mutation would auto-create a new one
      // and the user would see a surprising empty entry appear.
      if (wasActive) {
        const remaining = sortedSessions(loadSessions());
        if (remaining.length > 0) {
          switchToSession(remaining[0].id);
        } else {
          newSession();
        }
      } else {
        renderSessionList();
      }
    });
    li.appendChild(titleBtn);
    li.appendChild(delBtn);
    sessionsListEl.appendChild(li);
  }
}

// ---- Wire up the sidebar --------------------------------------------------

newSessionBtnEl?.addEventListener("click", newSession);

// ---- Boot ------------------------------------------------------------------

// One-time upgrade from the old single-history layout, then resolve
// the active session. `activeSessionId()` is self-healing — if there
// are zero sessions after migration, it creates one — so the rest of
// the boot path can assume a valid id.
migrateLegacy();
currentSessionId = getActiveId();
activeSessionId(); // validates / falls back / creates, updates sidebar
renderSessionList();
renderHistory(currentSessionId);

// Load the model list once on page boot. The previous version only
// fired on `modechange`, which meant a Discussion-mode-persisted user
// saw an empty dropdown until they clicked away and back. Loading at
// boot keeps the dropdown warm regardless of the persisted mode, and
// the disabled-server notice still renders correctly on 404.
loadModels();
// Same for the agents banner: one fetch on boot. A reload on
// /v1/agents that came back empty just hides the banner.
loadAgentsBanner();

document.addEventListener("visibilitychange", () => {
  // Re-fetch on tab return in case the server's model list changed
  // while the tab was backgrounded (e.g. another `ollama pull`).
  if (!document.hidden && globalThis.__nagentMode?.current() === "discussion") {
    loadModels();
    loadAgentsBanner();
  }
});

// Mode-toggle abort: when the user clicks the Transcript tab while a
// chat reply is streaming (or a turn is queued behind it), tear the
// work down. The voice recording is already stopped by the
// `MutationObserver` inside `AudioCapture` — the view going `hidden`
// triggers `_stop()` on the Discussion instance, which sends
// `StopSession`, closes the WS, pauses the shared VAD, and resets the
// pill. The LLM streaming reply is owned by this module, though, so
// it has no equivalent hook. Without this listener, tokens keep
// arriving on the now-hidden Discussion view, and any user turn that
// was queued (typed and not yet sent through, or transcribed and
// waiting on the in-flight reply) wakes up against a session the
// user has just left.
//
// Symmetry: this mirrors what `switchToSession` / `newSession` /
// `clearChat` already do for intra-mode navigation. Same two-step
// (abort the controller, then `resetTurnQueue`), same rationale for
// writing the "(stopped)" marker via `inflight.sessionId` so the
// partial reply is preserved against the session that was active at
// request start.
//
// We only act on the way *out* of Discussion. Switching back to it
// is a no-op — we already cancelled the in-flight reply on the way
// out, and there is no new work to interrupt.
document.addEventListener("modechange", (e) => {
  if (e?.detail?.mode === "discussion") return;
  if (inflight) inflight.controller.abort();
  resetTurnQueue();
});
