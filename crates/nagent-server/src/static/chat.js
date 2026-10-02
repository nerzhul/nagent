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
import { NagentTts } from "/static/tts.js";
import { preselectFromBrowser } from "/static/lang-preselect.js";
import * as Documents from "/static/documents.js";
import {
  DEFAULT_TITLE,
  TIMEZONE_ENABLED_KEY,
  createSessionObj,
  deleteSession as storeDeleteSession,
  deriveTitle,
  getActiveId,
  historyKey,
  loadHistory,
  loadSelectedModel,
  loadSessions,
  migrateLegacy,
  persistNewSession,
  renameSession,
  saveHistory,
  saveSelectedModel,
  setActiveId,
  sortedSessions,
  touchSession,
} from "/static/chat-sessions.js";
import {
  clearCachedLocation,
  formatLocationMessage,
  formatRelativeTime,
  getLocation,
  loadCachedLocation,
  loadLocationEnabled,
  saveCachedLocation,
  setLocationEnabled,
} from "/static/geolocation.js";
import {
  currentFlags as acquireCurrentPreferenceFlags,
  getReplyLanguage,
  getSystem as acquireSystemPreference,
  getTemperature as acquireTemperaturePreference,
  loadFromServer as loadPreferencesFromServer,
  saveToServer as savePreferencesToServer,
  defaults as preferenceDefaults,
} from "/static/preferences.js";

// ---- Chat session id (server-minted) ---------------------------------------
//
// The `X-Chat-Session-Id` header sent on every `/v1/*` request
// was previously client-controlled (the browser minted a UUID
// v4 in `chat-sessions.js`). The audit flagged that: a
// logged-in user could send any UUID and reach another user's
// docs if the value was leaked. The server now mints the id via
// `POST /v1/chat/session`; we cache the returned value in
// localStorage and re-mint on a 403 (binding lost — e.g. logout
// from another tab).
const SERVER_SESSION_STORAGE_KEY = "nagent.chat.serverSessionId";
let serverSessionId = "";
let serverSessionPromise = null;

async function ensureServerSessionId() {
  if (serverSessionId) return serverSessionId;
  if (serverSessionPromise) return serverSessionPromise;
  serverSessionPromise = (async () => {
    try {
      const rawCached = localStorage.getItem(SERVER_SESSION_STORAGE_KEY);
      if (rawCached) serverSessionId = rawCached;
      if (serverSessionId) return serverSessionId;
      const resp = await fetch("/v1/chat/session", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          ...window.nagentAuth?.csrfHeaders(),
        },
        body: "{}",
      });
      if (!resp.ok) {
        // 404 / 405 = endpoint not mounted (e.g. a build with
        // `chat_sessions` disabled). Treat silently — the chat
        // session id is only useful for documents scoping, so a
        // missing endpoint just means the scoping is off. We
        // still fall back to "" which makes `X-Chat-Session-Id:
        // ` (empty header) harmless on the server side.
        if (resp.status !== 404 && resp.status !== 405) {
          console.warn(
            "chat.js: POST /v1/chat/session failed",
            resp.status,
            resp.statusText
          );
        }
        return "";
      }
      const body = await resp.json();
      if (!body || typeof body.id !== "string") {
        console.warn("chat.js: POST /v1/chat/session returned no id");
        return "";
      }
      serverSessionId = body.id;
      try {
        localStorage.setItem(SERVER_SESSION_STORAGE_KEY, serverSessionId);
      } catch (e) {
        // localStorage can throw under quota pressure; the
        // session id is still cached in-memory for this page
        // load so we keep working.
        console.warn("chat.js: localStorage.setItem failed", e);
      }
      return serverSessionId;
    } catch (e) {
      console.warn("chat.js: POST /v1/chat/session network error", e);
      return "";
    } finally {
      serverSessionPromise = null;
    }
  })();
  return serverSessionPromise;
}

async function refreshServerSessionId() {
  serverSessionId = "";
  try {
    localStorage.removeItem(SERVER_SESSION_STORAGE_KEY);
  } catch (e) {
    /* ignore */
  }
  return ensureServerSessionId();
}

async function getServerSessionId() {
  if (serverSessionId) return serverSessionId;
  return ensureServerSessionId();
}

export {
  ensureServerSessionId,
  refreshServerSessionId,
  getServerSessionId,
};

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

// ---- Weather widget helpers ------------------------------------------------
//
// `get_weather` returns a rich JSON blob (see weather_agent.rs:349-440) that
// the chat would otherwise render as a long prose paragraph. We build a
// compact "hero + 3-day strip" card out of that JSON instead. Helpers live
// near the other rendering primitives so the live stream (`resolveToolBubble`)
// and the session-rehydration path (`renderHistory`) can share them.

// Map the upstream WeatherAPI `condition.code` integers to a small emoji
// set. Unknown codes fall back to the thermometer glyph so the card still
// renders cleanly for codes the upstream adds later. Pure function, no
// side effects, no DOM access — candidate first target for a JS test
// harness if the repo ever adopts one.
function conditionEmoji(code) {
  const map = {
    1000: "\u2600",  // ☀️ Clear / Sunny
    1003: "\u{1F324}", // 🌤️ Partly cloudy
    1006: "\u2601",  // ☁️ Cloudy
    1009: "\u2601",  // ☁️ Overcast
    1030: "\u{1F32B}", // 🌫 Mist
    1063: "\u{1F326}", // 🌦 Patchy rain
    1066: "\u{1F328}", // 🌨 Patchy snow
    1069: "\u{1F328}", // 🌨 Patchy sleet
    1074: "\u{1F328}", // 🌨 Patchy freezing drizzle
    1087: "\u26C8",  // ⛈ Thundery outbreaks
    1114: "\u{1F328}", // 🌨 Blowing snow
    1117: "\u2744",  // ❄️ Blizzard
    1135: "\u{1F32B}", // 🌫 Fog
    1147: "\u{1F32B}", // 🌫 Freezing fog
    1150: "\u{1F326}", // 🌦 Patchy light drizzle
    1153: "\u{1F326}", // 🌦 Light drizzle
    1168: "\u{1F327}", // 🌧 Freezing drizzle
    1171: "\u{1F327}", // 🌧 Heavy freezing drizzle
    1180: "\u{1F326}", // 🌦 Patchy light rain
    1183: "\u{1F327}", // 🌧 Light rain
    1186: "\u{1F327}", // 🌧 Moderate rain at times
    1189: "\u{1F327}", // 🌧 Moderate rain
    1192: "\u{1F327}", // 🌧 Heavy rain at times
    1195: "\u{1F327}", // 🌧 Heavy rain
    1198: "\u{1F327}", // 🌧 Light freezing rain
    1201: "\u{1F327}", // 🌧 Moderate / heavy freezing rain
    1204: "\u{1F328}", // 🌨 Light sleet
    1207: "\u{1F328}", // 🌨 Moderate / heavy sleet
    1208: "\u{1F328}", // 🌨 Light freezing drizzle
    1210: "\u2744",  // ❄️ Patchy light snow
    1213: "\u2744",  // ❄️ Light snow
    1216: "\u{1F328}", // 🌨 Patchy moderate snow
    1219: "\u{1F328}", // 🌨 Moderate snow
    1222: "\u{1F328}", // 🌨 Patchy heavy snow
    1225: "\u2744",  // ❄️ Heavy snow
    1237: "\u{1F328}", // 🌨 Ice pellets
    1240: "\u{1F326}", // 🌦 Light rain shower
    1243: "\u{1F327}", // 🌧 Rain shower
    1246: "\u{1F327}", // 🌧 Torrential rain shower
    1249: "\u{1F328}", // 🌨 Light sleet showers
    1252: "\u2744",  // ❄️ Light snow showers
    1255: "\u{1F328}", // 🌨 Snow showers
    1258: "\u{1F328}", // 🌨 Heavy snow showers
    1261: "\u{1F328}", // 🌨 Light ice pellet showers
    1264: "\u2744",  // ❄️ Moderate / heavy ice pellet showers
    1273: "\u26C8",  // ⛈ Light rain with thunder
    1276: "\u26C8",  // ⛈ Rain with thunder
    1279: "\u26C8",  // ⛈ Snow with thunder
    1282: "\u26C8",  // ⛈ Heavy thunderstorms
  };
  return map[code] || "\u{1F321}"; // 🌡 fallback for unknown codes
}

// Format a YYYY-MM-DD date string as a short weekday label ("Wed") in the
// location's timezone. Returns the input unchanged when parsing fails so
// the rest of the widget keeps rendering even on a malformed date.
function formatDayShort(dateStr, tz) {
  if (!dateStr) return "";
  // `YYYY-MM-DD HH:MM` parses as local-time; appending `T00:00:00` and a
  // timezone would let `Intl.DateTimeFormat` give us the right weekday
  // even when the user's browser is in a different zone. Strip any time
  // component first.
  const datePart = String(dateStr).slice(0, 10);
  const parts = datePart.split("-");
  if (parts.length !== 3) return datePart;
  const [y, m, d] = parts;
  const isoUtc = `${y}-${m}-${d}T12:00:00Z`; // noon avoids DST flips
  const dt = new Date(isoUtc);
  if (isNaN(dt.getTime())) return datePart;
  try {
    return new Intl.DateTimeFormat(undefined, {
      weekday: "short",
      timeZone: tz || undefined,
    }).format(dt);
  } catch (_e) {
    return new Intl.DateTimeFormat(undefined, { weekday: "short" }).format(dt);
  }
}

// Format `localtime` ("YYYY-MM-DD HH:MM" in the location's tz) as
// "Tue 26 Sep 14:00" — a compact header that fits both on desktop and on
// narrow viewports.
function formatLocalTimestamp(localtime) {
  if (!localtime) return "";
  const trimmed = String(localtime).trim();
  const dayLabel = formatDayShort(trimmed);
  // Keep just the HH:MM portion of the time-of-day field for the as-of line.
  const timePart = trimmed.slice(11, 16);
  return timePart ? `${dayLabel} ${timePart}` : dayLabel;
}

// Read a string field off an arbitrary object/JSON value. `data.current`
// and `data.forecast[].date` are wrapped defensively so a partial or
// schema-drift payload doesn't throw the stream into a crash loop.
function safeStr(value, fallback = "") {
  return typeof value === "string" ? value : fallback;
}
function safeNum(value, fallback = 0) {
  const n = typeof value === "number" ? value : Number(value);
  return Number.isFinite(n) ? n : fallback;
}

// Pick the three forecast days to display. Current mode = today's slice
// plus the next two (the agent already returns forecast[0..N] in source
// order). Forecast mode with an explicit `requested_date` finds the
// matching slice and returns it plus the next two (or fewer if the
// forecast is shorter). Historical mode with `requested_date` returns
// just the matching single day so the card header can read "On …".
function pickWeatherDays(data, mode) {
  const list = Array.isArray(data?.forecast) ? data.forecast : [];
  if (list.length === 0) return [];
  if (mode === "historical") {
    const target = safeStr(data.requested_date);
    return target ? list.filter((d) => safeStr(d.date) === target).slice(0, 1) : list.slice(0, 1);
  }
  if (mode === "forecast") {
    const target = safeStr(data.requested_date);
    if (target) {
      const idx = list.findIndex((d) => safeStr(d.date) === target);
      if (idx >= 0) return list.slice(idx, idx + 3);
    }
    return list.slice(0, 3);
  }
  // current mode — first three entries (today, tomorrow, day after).
  return list.slice(0, 3);
}

// Inspect `requested_date` / `location.localtime` to decide which mode
// the query fell into. The agent only emits `requested_date` for the
// SingleDate arm of `Mode` (weather_agent.rs:432-437), so absence implies
// a current-snapshot query.
function detectWeatherMode(data) {
  const requested = safeStr(data?.requested_date);
  if (!requested) return "current";
  const localtime = safeStr(data?.location?.localtime);
  const today = localtime.slice(0, 10);
  if (requested === today) return "current";
  // Distinguishing past from future is cheaper than sorting: compare
  // the YYYY-MM-DD strings lexicographically (ISO format sorts cleanly).
  if (requested < today) return "historical";
  return "forecast";
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
  // `innerHTML = ...` above wiped every child including the per-bubble
  // replay button (added by `appendBubble`) and any inlined tool
  // traces (added by `appendToolBubble`). Re-attach both so they
  // survive every streaming markdown re-render.
  if (bubbleEl._replayBtn || bubbleEl.classList.contains("chat-message--markdown")) {
    ensureReplayButton(bubbleEl);
    refreshReplayButtonVisibility();
  }
  if (bubbleEl._toolUsageEls && bubbleEl._toolUsageEls.length > 0) {
    // `_toolUsageEls` is in source order (the order each tool_call
    // landed). Insert them as the last children of the bubble —
    // just before the replay button — so the visible flow stays
    // (prose, then tool traces, then 🔊 button).
    const replayBtn = bubbleEl._replayBtn;
    for (const details of bubbleEl._toolUsageEls) {
      bubbleEl.insertBefore(details, replayBtn);
    }
  }
  if (bubbleEl._weatherCards && bubbleEl._weatherCards.length > 0) {
    // Weather cards live as direct children of the bubble (NOT
    // inside any tool-trace `<details>` — the user closes the tool
    // summary but the answer must stay readable). Re-insert each
    // tracked card before the replay button so it stays the last
    // visible element. Order matches insertion order (one card per
    // `get_weather` call).
    const replayBtn = bubbleEl._replayBtn;
    for (const card of bubbleEl._weatherCards) {
      bubbleEl.insertBefore(card, replayBtn);
    }
  }
}

/**
 * Create (or recreate) the per-bubble replay button and attach it
 * to `bubbleEl`. Used after any operation that overwrites
 * `bubbleEl.innerHTML` (live-stream loader, applyMarkdown,
 * finalSource write) so the button survives.
 *
 * Returns the button instance; idempotent — calling twice on the
 * same bubble returns the existing button instead of stacking
 * duplicates.
 */
function ensureReplayButton(bubbleEl) {
  let btn = bubbleEl._replayBtn;
  if (btn && btn.isConnected) return btn;
  btn = document.createElement("button");
  btn.type = "button";
  btn.className = "chat-message-replay";
  btn.setAttribute("aria-label", "Replay this message aloud");
  btn.title = "Replay this message aloud";
  // Always visible on assistant bubbles -- the click handler in
  // `replayMessage` is what gates actual playback. Showing the
  // button unconditionally doubles as a discoverability cue.
  btn.hidden = false;
  btn.innerHTML = `
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
      <path d="M3 9v6h4l5 5V4L7 9H3zm13.5 3c0-1.77-1.02-3.29-2.5-4.03v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77 0-4.28-2.99-7.86-7-8.77z" fill="currentColor"/>
    </svg>`;
  btn.addEventListener("click", () => replayMessage(bubbleEl, btn));
  bubbleEl.appendChild(btn);
  bubbleEl._replayBtn = btn;
  return btn;
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
// The chat UI lives inside `<template id="app-shell-template">`
// and only enters the DOM after `auth.js` mounts the shell
// (typically after a `/api/me` probe). A plain module-level
// `document.getElementById` returns `null` for that reason — so
// every access site that does `formEl.hidden = true` would
// crash on the pre-mount null. We solve this with a `Proxy`
// that re-queries the DOM on every property access; reads + writes
// both forward to the live element (which may still be null, in
// which case they no-op or return undefined). This keeps every
// existing call site (`formEl.addEventListener`,
// `stopBtn.hidden = false`, `inputEl.focus()`, …) unchanged.
function lazyEl(id) {
  // Pre-mount stub. Built fresh per access so the iterable
  // markers (`Symbol.iterator`, `Symbol.toPrimitive`, …) point at
  // the proxy itself, not at a shared frozen object that some
  // other pre-mount code might mutate.
  const iterator = function* () {};
  // Single canonical "pre-mount DOM stub" used as the value for
  // every property access. The same stub object backs:
  // - `messagesEl.children`            → iterable (empty)
  // - `messagesEl.querySelectorAll(…)` → iterable (empty)
  // - `messagesEl.querySelector(…)`    → itself (callers usually
  //                                     check for null; the stub
  //                                     is truthy so `if (el)`
  //                                     branches still take the
  //                                     "exists" path)
  // - `messagesEl.appendChild(…)`      → no-op (function call)
  // - `messagesEl.innerHTML = ""`      → setter no-op
  // - `messagesEl.value`               → empty string
  // - `messagesEl.focus()`             → no-op
  //
  // The get trap returns `makeStub()` for ANY property so chained
  // access (`messagesEl.children.querySelectorAll(...)`) terminates
  // in the same iterable stub — `[...stub]` and
  // `for (const x of stub)` both yield zero iterations.
  const makeStub = () => {
    const stub = function () {};
    return new Proxy(stub, {
      get(_t, prop) {
        if (prop === Symbol.iterator) return iterator;
        if (prop === "length") return 0;
        if (prop === "value"
            || prop === "innerHTML"
            || prop === "textContent"
            || prop === "outerHTML") return "";
        return makeStub();
      },
      has(_t, prop) {
        return prop === Symbol.iterator
          || prop === "length"
          || prop === "value"
          || prop === "innerHTML"
          || prop === "textContent";
      },
      set() { return true; },
      // Calling the stub as a function (`messagesEl.querySelectorAll(".x")`,
      // `messagesEl.appendChild(node)`, `messagesEl.focus()`, …) must
      // return an iterable stub, NOT `undefined`, so that the caller
      // can spread / chain without throwing. Returning the stub
      // itself makes every chain terminate in the same iterable
      // no-op. Bind to a fresh `this` so callers that read the
      // returned object's properties still see the stub markers.
      apply() { return makeStub(); },
      // `Symbol.iterator` is set via the `get` trap above; the
      // constructor trap (`construct`) is needed so `new
      // messagesEl.SomeCtor()` (rare, but a few libs do this on
      // collections) also returns an iterable stub instead of
      // throwing "is not a constructor".
      construct() { return makeStub(); },
    });
  };
  return new Proxy(
    { _id: id },
    {
      get(_t, prop) {
        if (prop === "_id") return id;
        const el = document.getElementById(id);
        if (el == null) {
          // Pre-mount access (e.g. an event fires while the chat UI
          // is still inside `<template id="app-shell-template">`).
          // Returns a safe stub so chained property reads terminate
          // without throwing; the real render runs after
          // `app-shell-mounted`. Callers that need guaranteed
          // mount-time wiring should attach their listeners inside
          // an `app-shell-mounted` handler — see
          // `wireFormOnce` / `wireLocationControlsOnce` /
          // `wireChatSidebarOnce`.
          return makeStub();
        }
        const v = el[prop];
        return typeof v === "function" ? v.bind(el) : v;
      },
      set(_t, prop, value) {
        const el = document.getElementById(id);
        if (el == null) return true;
        el[prop] = value;
        return true;
      },
      has(_t, prop) {
        const el = document.getElementById(id);
        return el != null && prop in el;
      },
    },
  );
}
const messagesEl   = lazyEl("chat-messages");
const formEl       = lazyEl("chat-form");
const inputEl      = lazyEl("chat-input");
const sendBtn      = lazyEl("chat-send");
const stopBtn      = lazyEl("chat-stop");
const clearBtn     = lazyEl("chat-clear");
const modelEl      = lazyEl("chat-model");
// `systemEl.value` is an *extension* appended after the server's
// default system prompt (env `LLM_SYSTEM_PROMPT` / TOML
// `[llm].system_prompt`); the proxy prepends the admin's prompt as
// `messages[0]`, our value goes second so the admin's intent stays
// authoritative.
const systemEl     = lazyEl("chat-system");
const tempEl       = lazyEl("chat-temperature");
const statusEl     = lazyEl("chat-status");
const disabledNoticeEl = lazyEl("chat-disabled-notice");
const sessionsListEl = lazyEl("chat-sessions");
const agentsBannerEl = $("chat-agents-banner");
const agentsBannerNamesEl = $("chat-agents-banner-names");
// Geolocation UI handles. The initial opt-in lives in the form footer;
// the status pill in the header and the advanced-panel controls show
// up only after a position has been cached. Stored as `lazyEl` proxies
// (rather than plain `document.getElementById`) because the chat UI
// lives inside `<template id="app-shell-template">` and is only
// cloned into the DOM after `auth.js` mounts the shell — a plain
// `getElementById` at module top-level would return `null` and every
// later `el.click` / `el.hidden = …` would crash or no-op.
const locationShareBtn  = lazyEl("chat-location-share");
const locationPill      = lazyEl("chat-location-pill");
const locationPillText  = lazyEl("chat-location-pill-text");
const locationAdvBox    = lazyEl("chat-location-advanced");
const locationToggleEl  = lazyEl("chat-location-toggle");
const locationRefresh   = lazyEl("chat-location-refresh");
const locationForget    = lazyEl("chat-location-forget");
const locationStatusEl  = lazyEl("chat-location-status");

// Browser-timezone UI handles. Always visible (no permission, no
// cache); the toggle mirrors the LOCATION_ENABLED_KEY pattern so a
// fresh page reload picks up the user's choice. Same `lazyEl`
// rationale as the geolocation handles — the elements are inside the
// app-shell template and only resolve once the shell mounts.
const timezoneToggleEl  = lazyEl("chat-timezone-toggle");
const timezoneStatusEl  = lazyEl("chat-timezone-status");

// The active session id is read at every operation rather than
// cached, so a same-tab mutation (delete, new chat, switch from the
// sidebar) immediately takes effect without bookkeeping. `loadSessions`
// is the source of truth — if the cached `currentSessionId` is no
// longer valid, we fall back to the most-recently-updated session or
// create a fresh one. This keeps the invariant "there is always
// exactly one active session" without any explicit init step.
let currentSessionId = "";

// Idempotency guard for the `<select id="chat-model">` change
// listener. `loadModels()` runs more than once across the page
// lifetime (boot, visibility refresh, retry after a transient
// error), so the listener attachment must happen exactly once.
let _modelChangeWired = false;
let modelsLoaded = false;
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
  // Preserve the inline voice-graph element (Discussion-mode voice
  // oscilloscope, see docs/ui_features.md §4.10) across the wipe
  // below. The element lives as a direct child of `#chat-messages`,
  // so `messagesEl.innerHTML = ""` would otherwise orphan it and
  // silently dangle the `AudioCapture`'s `graphEl` reference — the
  // recording UI would never appear and any later `_setGraphVisible`
  // toggle would hit a detached element. Save the reference, wipe,
  // re-append after the rehydrated bubbles are in place.
  const inlineVoiceGraph = messagesEl.querySelector(".voice-graph--inline");
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
        const name = msg.name || "tool";
        // The matching assistant `tool_calls[]` entry (processed just
        // above) already created a running `<details>` for this id with
        // the tool's args wired into the bubble body. Don't add a second
        // one — only resolve the existing bubble. Without this guard
        // each tool call ends up with two traces after a refresh: the
        // first one flips to `ok` via `resolveToolBubble`, the second
        // stays in `--running` and the user sees a stuck
        // "tool running…" pill that never settles.
        const existing = id
          ? messagesEl.querySelector(
              `details.chat-message__tool-usage[data-tool-id="${CSS.escape(id)}"]`,
            )
          : null;
        if (!existing) {
          appendToolBubble(
            null,
            { id, name, args: null, index: 0 },
            toolAnchorEl,
          );
        }
        // Prefer the server-curated `summary` (matches the live pill
        // byte-for-byte, ≤ 80 chars on success / ≤ 160 on error). Fall
        // back to a content-derived truncation for history written by
        // older builds that did not persist `summary`.
        const summary = msg.summary
          || (msg.content ? truncateSummary(msg.content) : "");
        resolveToolBubble(null, {
          id,
          name,
          ok: true,
          summary,
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
    continue;
  }
  // Historical assistant bubbles: collapse the inlined tool traces
  // by default so the rehydrated view matches the "retracted after
  // the reply settles" rule. Live streams drop the same traces in
  // `streamReply`'s `finally` block.
  for (const bubble of messagesEl.querySelectorAll(".chat-assistant")) {
    const traces = bubble.querySelectorAll(
      "details.chat-message__tool-usage[open]",
    );
    traces.forEach((d) => { d.open = false; });
  }
  // Re-append the inline voice-graph at the very end so it stays
  // the last child of `#chat-messages` per §4.10 ("appended to
  // `#chat-messages`"). The wipe above removed it; the AudioCapture
  // still holds a JS reference to the element so the canvas and
  // level bindings survive the round-trip — only the DOM placement
  // needs restoring.
  if (inlineVoiceGraph) {
    messagesEl.appendChild(inlineVoiceGraph);
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
  if (sid) div.dataset.sessionId = sid;
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
  // Replay button: appended AFTER the markdown render so it sits
  // outside the sanitized HTML and can't be stripped by DOMPurify.
  // Only on assistant messages (user / error bubbles don't speak).
  // Always visible -- the click handler in `replayMessage`
  // implicitly enables the master `#chat-tts-check` if it's off,
  // and `probeTtsVoices` already filtered out bubbles on servers
  // without TTS support (no `GET /v1/audio/voices` response). We
  // use `ensureReplayButton` (rather than building the button
  // inline) so the same helper can re-attach the button after
  // every subsequent `innerHTML =` call on this bubble (live-
  // stream loader, applyMarkdown, finalSource write). Those calls
  // would otherwise destroy the button.
  if (role === "assistant") {
    ensureReplayButton(div);
  }
  messagesEl.appendChild(div);
  // Re-pin the inline voice-graph to the end so the
  // `position: sticky; bottom: 0` CSS keeps it at the bottom of the
  // chat-messages scroll container (see
  // docs/ui_features.md §4.10). Without this the new bubble would
  // push the voice-graph above the visible bottom edge.
  ensureInlineVoiceGraphAtEnd();
  messagesEl.scrollTop = messagesEl.scrollHeight;
  // Refresh replay-button visibility AFTER the bubble is in the DOM
  // tree. `document.querySelectorAll(".chat-message-replay")` skips
  // elements in detached sub-trees, so calling this before
  // `messagesEl.appendChild(div)` would leave the new button at its
  // initial `hidden = true` state until the next master-toggle event.
  if (role === "assistant") {
    refreshReplayButtonVisibility();
  }
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
  // Re-pin the inline voice-graph to the end (see `appendBubble`).
  ensureInlineVoiceGraphAtEnd();
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

/// Try to parse `err` as the proxy's `model_not_found` envelope and,
/// on success, render a clickable picker over `assistantEl` that
/// swaps the model and re-runs the user's last turn. Returns `true`
/// when the picker was rendered so the caller's catch block can skip
/// its plain-text fallback.
///
/// Recognised shape:
/// ```json
/// {"error":{"type":"model_not_found","message":"...","param":"<name>","available":["a","b","c"]}}
/// ```
/// `err` is whatever the catch block in `streamReply` saw — usually
/// `Error("HTTP 404: {...body...}")` produced upstream, so we have
/// to mine the JSON out of the message.
async function renderModelNotFoundIfApplicable(assistantEl, err) {
  const body = extractErrorBodyJson(err);
  if (!body) return false;
  const errorObj = body.error;
  if (!errorObj || errorObj.type !== "model_not_found") return false;
  const requested = errorObj.param || "";
  const upstreamMessage = errorObj.message || "model not found";
  const available = Array.isArray(errorObj.available) ? errorObj.available : [];
  const sessionId = assistantEl?.dataset?.sessionId || activeSessionId();

  // Wipe the loader / partial reply / replay button so the picker
  // sits on a clean bubble. The loader was the only thing in the
  // bubble when this fires (LLM returned before any token streamed),
  // so the cost is one DOM mutation.
  assistantEl.classList.remove("chat-message--streaming");
  assistantEl.innerHTML = "";

  const wrap = document.createElement("div");
  wrap.className = "chat-model-not-found";
  const heading = document.createElement("p");
  heading.className = "chat-model-not-found-heading";
  heading.textContent = requested
    ? `Model “${requested}” is not available on the upstream.`
    : `Model not available on the upstream.`;
  wrap.appendChild(heading);
  const detail = document.createElement("p");
  detail.className = "chat-model-not-found-detail";
  detail.textContent = upstreamMessage;
  wrap.appendChild(detail);

  if (available.length > 0) {
    const list = document.createElement("div");
    list.className = "chat-model-not-found-list";
    list.setAttribute("role", "group");
    list.setAttribute("aria-label", "Available models");
    for (const id of available) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "chat-model-not-found-pick";
      btn.textContent = id;
      btn.dataset.modelId = id;
      btn.addEventListener("click", () =>
        pickModelAndRetry(btn, sessionId, requested),
      );
      list.appendChild(btn);
    }
    wrap.appendChild(list);
    const hint = document.createElement("p");
    hint.className = "chat-model-not-found-hint";
    hint.textContent =
      "Pick a model to swap the dropdown and re-send the last turn.";
    wrap.appendChild(hint);
  } else {
    // The follow-up upstream `/v1/models` query (issued by the LLM
    // proxy itself on a `model_not_found` upstream response) also
    // failed — point the user at the operator docs so they can fix
    // the upstream or set `[llm].default_model` to something the
    // backend serves.
    const hint = document.createElement("p");
    hint.className = "chat-model-not-found-hint";
    hint.textContent =
      "Run `ollama pull <name>` on the upstream, then re-send. " +
      "Or set `OLLAMA_MODEL` / `[llm].default_model` on this server.";
    wrap.appendChild(hint);
  }

  assistantEl.appendChild(wrap);
  // `finalSource` is read by the `streamReply` caller (and by the
  // replay button) to decide what to persist into history. The
  // picker is transient UI, not part of the conversation, so we
  // blank it out — the user's last turn is preserved as the most
  // recent entry and a fresh assistant bubble will replace this
  // one when they pick a model.
  return true;
}

/// Walk the `Error.message` we built upstream
/// (``HTTP 404: {"error":...}``) and pull out the JSON object. Some
/// upstream bodies are plain text (e.g. ``llm disabled``); those
/// return `null`.
function extractErrorBodyJson(err) {
  const msg = err && err.message ? String(err.message) : String(err || "");
  const idx = msg.indexOf("{");
  if (idx < 0) return null;
  // Try to parse from the first `{` to the matching `}`. We can't
  // do real bracket matching in linear time without a stack, so
  // attempt progressively-longer tails until one parses.
  for (let end = msg.length; end > idx; end--) {
    if (msg[end - 1] !== "}") continue;
    try {
      const v = JSON.parse(msg.slice(idx, end));
      if (v && typeof v === "object") return v;
    } catch (_e) {
      // keep trying
    }
  }
  return null;
}

/// Apply a model pick from the picker: update the dropdown, persist
/// the choice, and re-run the user's last user turn against the new
/// model. The previous assistant bubble (the picker) is removed
/// because it has no conversation value.
async function pickModelAndRetry(buttonEl, sessionId, requestedModelId) {
  const newId = buttonEl.dataset.modelId;
  if (!newId) return;
  // Reflect the choice in the dropdown + persisted prefs before
  // re-sending so the next reload lands on the same model.
  if (Array.from(modelEl.options).some((o) => o.value === newId)) {
    modelEl.value = newId;
  } else {
    // Upstream model list might have grown between the failed
    // request and the click; inject the option so the dropdown
    // stays in sync.
    const opt = document.createElement("option");
    opt.value = newId;
    opt.textContent = newId;
    modelEl.appendChild(opt);
    modelEl.value = newId;
  }
  saveSelectedModel(modelEl.value);
  // Disable the picker while the retry runs so a second click does
  // not fire two parallel requests.
  const picker = buttonEl.closest(".chat-model-not-found");
  if (picker) {
    picker
      .querySelectorAll("button.chat-model-not-found-pick")
      .forEach((b) => {
        b.disabled = true;
      });
    buttonEl.classList.add("chat-model-not-found-pick--loading");
    buttonEl.textContent = `${newId}…`;
  }
  // Find the last user turn we need to re-send.
  const history = loadHistory(sessionId);
  const lastUserIdx = (() => {
    for (let i = history.length - 1; i >= 0; i--) {
      if (history[i]?.role === "user") return i;
    }
    return -1;
  })();
  if (lastUserIdx < 0) return;
  const userText = history[lastUserIdx].content || "";
  // Drop the picker bubble — its content was diagnostic only.
  const bubble = buttonEl.closest(".chat-message");
  if (bubble && bubble.parentElement === messagesEl) {
    bubble.remove();
  }
  // Re-issue the same user text against the freshly-picked model.
  await streamReply(sessionId, userText);
}

// The Discussion-mode voice oscilloscope (§4.10) lives as a direct
// child of `#chat-messages` and uses `position: sticky; bottom: 0` to
// stay pinned to the bottom of the conversation column. Every
// `messagesEl.appendChild` above pushes the bubble to the end and
// therefore pushes the voice-graph above it; without re-appending the
// graph, sticky bottom-of-the-DOM no longer matches sticky
// bottom-of-the-viewport and the widget can scroll out of view. This
// helper moves the inline voice-graph back to the last-child position
// in O(N) over `#chat-messages`'s direct children (typically < 50
// bubbles in a normal session). Safe to call when the graph is absent.
function ensureInlineVoiceGraphAtEnd() {
  for (const child of messagesEl.children) {
    if (child.classList?.contains("voice-graph--inline")) {
      messagesEl.appendChild(child);
      return;
    }
  }
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
  web_fetch: "\u{1F50E}",       // 🔎
  get_weather: "\u{1F324}",      // 🌤️ — matches the card's condition icon family
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

  // Wrap the tool bubble in a collapsible `<details>` element so the
  // user can hide the tool trace by default once the reply settles.
  // The `<summary>` carries a compact "🔧 wikipedia …" line; the body
  // is the full tool bubble. During the tool run the `<details>` is
  // open so the user sees progress; once the reply completes we
  // remove the `open` attribute (see `finalizeAssistantBubble`).
  //
  // The wrapper is inserted INSIDE the assistant bubble (before the
  // replay button), so the tool trace visually hangs off the message
  // that produced it. Rehydration on history reload reproduces the
  // same DOM via the `renderHistory` path, which calls
  // `appendToolBubble` with `sessionId = null` and the assistant
  // bubble as anchor.
  const details = document.createElement("details");
  details.className = "chat-message__tool-usage";
  details.dataset.toolId = id;
  details.dataset.toolName = name;
  details.open = true;
  const summary = document.createElement("summary");
  // `summary` must contain the same label so the user sees a stable
  // caption in both states (collapsed: this line; expanded: this line
  // + the full body). The icon + name are duplicated into the body
  // by the existing `.chat-tool-bubble` markup below.
  const summaryIcon = document.createElement("span");
  summaryIcon.className = "chat-message__tool-summary-icon";
  summaryIcon.textContent = toolIcon(name);
  const summaryName = document.createElement("span");
  summaryName.className = "chat-message__tool-summary-name";
  summaryName.textContent = name;
  const summaryStatus = document.createElement("span");
  summaryStatus.className = "chat-message__tool-summary-status";
  summaryStatus.textContent = "running…";
  summary.appendChild(summaryIcon);
  summary.appendChild(summaryName);
  summary.appendChild(summaryStatus);
  details.appendChild(summary);
  details.appendChild(div);

  if (assistantEl && messagesEl.contains(assistantEl)) {
    // Insert before the replay button if it's already attached
    // (`appendBubble` adds it before any tool_call lands). Falls
    // back to plain appendChild for callers that pass a detached
    // assistant bubble (tests).
    //
    // `messagesEl` is a `lazyEl` proxy — `contains()` resolves to
    // the real DOM element's `contains` (via the get-trap binding
    // on every access), so the ancestry check works across the
    // proxy. The previous `assistantEl.parentNode === messagesEl`
    // form failed silently after the proxy migration because the
    // strict identity compare treats the proxy object and the
    // real `<div id="chat-messages">` as different objects, so
    // the condition was always false and every tool trace landed
    // as a direct child of the messages container. That made
    // `renderWeatherWidget`'s `details.parentElement.closest
    // (".chat-assistant")` walk bail (no enclosing bubble), which
    // is exactly the symptom that surfaced as "le widget météo ne
    // s'affiche plus".
    const replayBtn = assistantEl.querySelector(".chat-message-replay");
    if (replayBtn) {
      assistantEl.insertBefore(details, replayBtn);
    } else {
      assistantEl.appendChild(details);
    }
  } else {
    messagesEl.appendChild(details);
  }
  // Track the wrapper on the bubble so `applyMarkdown` (which wipes
  // the bubble's `innerHTML` on every streaming tick) can re-insert
  // it after the wipe. Without this the tool trace would vanish
  // mid-stream.
  if (assistantEl && !assistantEl._toolUsageEls) {
    assistantEl._toolUsageEls = [];
  }
  if (assistantEl) {
    assistantEl._toolUsageEls.push(details);
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;

  // While the tool runs, hide the assistant's accumulating prose so
  // the "let me check…" / "fetching…" preamble doesn't steal focus
  // from the eventual widget. Only the live-stream path triggers
  // this — rehydration (`appendToolBubble(null, ...)`) plays back
  // already-finished tool calls and would otherwise flash a
  // "Préparation…" placeholder on a settled reply.
  if (sessionId && assistantEl?.classList?.contains("chat-assistant")
      && inflight && inflight.sessionId === sessionId) {
    setAssistantToolPending(assistantEl, true);
  }

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
  // The tool trace is now wrapped in a `<details>` collapsible
  // (`appendToolBubble`); the inner `.chat-tool-bubble` div is the
  // historical node whose status / class flips on result.
  const details = messagesEl.querySelector(
    `details.chat-message__tool-usage[data-tool-id="${CSS.escape(id)}"]`,
  );
  const div = details?.querySelector(".chat-tool-bubble");
  if (div) {
    div.classList.remove("chat-tool-bubble--running");
    div.classList.add(ok ? "chat-tool-bubble--ok" : "chat-tool-bubble--error");
    const statusEl = div.querySelector(".chat-tool-status");
    if (statusEl) {
      statusEl.textContent = ok ? `✓ ${truncateSummary(summary)}` : `⚠ ${truncateSummary(summary)}`;
    }
    // Mirror the result on the `<summary>` so the user sees the
    // status line even when the tool trace is collapsed (which is
    // the default once the reply settles).
    const summaryStatus = details.querySelector(".chat-message__tool-summary-status");
    if (summaryStatus) {
      summaryStatus.textContent = ok
        ? `✓ ${truncateSummary(summary)}`
        : `⚠ ${truncateSummary(summary)}`;
    }
    messagesEl.scrollTop = messagesEl.scrollHeight;
    // On tool error, no prose is going to stream after the tool
    // result, so collapse the wrapper now rather than waiting for
    // stream end: the `<details>` open state would otherwise show
    // the error body until the assistant turn finalises, which can
    // be much later for multi-tool replies. We also unhide the
    // assistant's prose at this point — if the LLM had already
    // streamed a preamble before the tool call, the user wants to
    // see it now (the LLM may also stream an error-acknowledging
    // reply that the prose placeholder is hiding otherwise).
    if (!ok && details) {
      details.open = false;
      const assistantEl = findAssistantBubble(details);
      if (assistantEl?.classList?.contains("chat-message--tool-pending")) {
        setAssistantToolPending(assistantEl, false);
      }
    }
    // `get_weather` carries a structured JSON payload already — build
    // the compact card out of it. Wrapped in try/catch so a malformed
    // payload degrades gracefully (summary line is still rendered) and
    // never tears down the live stream.
    if (ok && name === "get_weather") {
      try {
        const data = parseWeatherPayload(content);
        if (data) {
          const mode = detectWeatherMode(data);
          renderWeatherWidget(div, data, mode);
          // Mark the weather bubble so the stream-end finalizer
          // collapses the assistant's prose down to a one-liner
          // (the widget is the visible answer). Defer to stream end
          // via `inflight.weatherFinalizeEl`; on rehydration
          // (`inflight` is null) we suppress immediately.
          if (inflight && inflight.sessionId === sessionId) {
            inflight.weatherFinalizeEl = div;
          } else if (details?.parentElement) {
            const assistantEl = details.parentElement.closest(".chat-assistant");
            if (assistantEl) finalizeAssistantForToolResult(details, assistantEl);
          }
        }
      } catch (e) {
        console.warn("weather widget render failed:", e);
      }
    }
  }
  if (sessionId) {
    const h = loadHistory(sessionId);
    h.push({
      role: "tool",
      tool_call_id: id,
      // Persist `name` and `summary` so a page refresh reproduces the
      // same pill text as the live stream byte-for-byte. Without
      // `summary` the rehydration path had to recompute it from
      // `content` (≤ 117 chars + "…") which is slightly longer than
      // the server's live cap (≤ 80 chars on success) and visually
      // diverges from what the user just saw.
      name: name || "",
      content: content == null ? "" : String(content),
      summary: summary || "",
      ts: Date.now(),
    });
    saveHistory(sessionId, h);
  }
}

// Build the structured weather card and inject it as a sibling of
// the tool trace `<details>`, NOT as a child of it. The card is the
// visible answer for `get_weather`, so it must stay readable even
// when the user collapses the tool summary; tucking it inside the
// `<details>` made it disappear with the summary (caught in a UX
// review — the user was losing the structured card whenever they
// closed the tool trace). Built with `createElement` + `textContent`
// only — no `innerHTML` / DOMPurify pass, mirroring the existing
// tool-bubble detail line at chat.js:634-642.
//
// The card is tracked on `assistantEl._weatherCards` so
// `applyMarkdown` can re-insert it after the per-tick `innerHTML = ""`
// reset (the card lives as a direct child of the bubble, not inside
// any container that `applyMarkdown` re-mounts).
function renderWeatherWidget(parentEl, data, mode) {
  if (!parentEl || !messagesEl.contains(parentEl)) return null;
  const details = parentEl.closest("details.chat-message__tool-usage");
  const assistantEl = details?.parentElement?.closest(".chat-assistant");
  if (!assistantEl) {
    // No enclosing assistant bubble — bail. Shouldn't happen for a
    // live tool bubble, but a test fixture could pass a detached one.
    return null;
  }
  // Idempotency: if a previous render already attached a card to
  // this assistant bubble (e.g. a session rehydration racing the
  // live stream), replace it in place rather than stacking
  // duplicates. The card is no longer the next sibling of the
  // tool-bubble div, so we look it up by tracked reference instead.
  const tracked = (assistantEl._weatherCards || []).find((c) => c && c.isConnected);
  if (tracked) tracked.remove();
  const location = data?.location || {};
  const current = data?.current || null;
  const days = pickWeatherDays(data, mode);

  const card = document.createElement("div");
  card.className = `chat-weather-card chat-weather-card--${mode}`;

  // Header line: city · as-of · date. Always present so the card is
  // scannable even when the forecast is empty (e.g. an unexpected
  // empty `forecast[]` from the upstream).
  const header = document.createElement("div");
  header.className = "chat-weather-card__header";
  const cityEl = document.createElement("span");
  cityEl.className = "chat-weather-card__city";
  cityEl.textContent = safeStr(location.name) || "—";
  header.appendChild(cityEl);
  const asOf = document.createElement("span");
  asOf.className = "chat-weather-card__asof";
  asOf.textContent = `· ${formatLocalTimestamp(safeStr(location.localtime))}`;
  header.appendChild(asOf);
  if (mode !== "current" && safeStr(location.localtime)) {
    const dateLabel = document.createElement("span");
    dateLabel.className = "chat-weather-card__date";
    const requested = safeStr(data.requested_date);
    dateLabel.textContent = requested
      || formatDayShort(safeStr(location.localtime), safeStr(location.timezone));
    header.appendChild(dateLabel);
  }
  card.appendChild(header);

  // Hero block: only in current mode. Skipped for historical / pure
  // forecast queries where we don't have a now-cast — the days strip
  // tells the whole story there.
  if (mode === "current" && current) {
    const hero = document.createElement("div");
    hero.className = "chat-weather-card__hero";
    const iconEl = document.createElement("span");
    iconEl.className = "chat-weather-card__icon";
    iconEl.textContent = conditionEmoji(safeNum(current.condition_code));
    hero.appendChild(iconEl);
    const body = document.createElement("div");
    body.className = "chat-weather-card__hero-body";
    const tempEl = document.createElement("div");
    tempEl.className = "chat-weather-card__temp";
    tempEl.textContent = `${safeNum(current.temp_c)} °C`;
    body.appendChild(tempEl);
    const condEl = document.createElement("div");
    condEl.className = "chat-weather-card__condition";
    condEl.textContent = safeStr(current.condition) || "—";
    body.appendChild(condEl);
    hero.appendChild(body);
    // Meta row beneath the hero: feels-like, humidity, UV.
    // Wind gets its own dedicated row below — it's the second-most-
    // asked field after the temperature and visually combining it
    // with humidity/UV made it easy to miss.
    const metaEl = document.createElement("div");
    metaEl.className = "chat-weather-card__meta";
    const feelsItem = makeMetaItem("Ressenti", `${safeNum(current.feels_like_c)} °C`);
    if (feelsItem) metaEl.appendChild(feelsItem);
    if (current.humidity) {
      metaEl.appendChild(makeMetaItem("Humidité", `${safeNum(current.humidity)} %`));
    }
    if (current.uv) {
      metaEl.appendChild(makeMetaItem("UV", String(safeNum(current.uv))));
    }
    if (current.pressure_mb) {
      metaEl.appendChild(makeMetaItem("Pression", `${safeNum(current.pressure_mb)} hPa`));
    }
    card.appendChild(hero);
    if (metaEl.childElementCount > 0) card.appendChild(metaEl);

    // Dedicated wind row — visually prominent so a glance tells the
    // user the conditions include wind speed and direction. The
    // arrow rotates to match the compass heading.
    if (current.wind_kmh) {
      const windEl = document.createElement("div");
      windEl.className = "chat-weather-card__wind";
      const windIcon = document.createElement("span");
      windIcon.className = "chat-weather-card__wind-icon";
      windIcon.textContent = "\u{1F32C}"; // 🌬
      windEl.appendChild(windIcon);
      const dir = safeStr(current.wind_dir);
      if (dir && cardinalToDegrees(dir) != null) {
        const arrow = document.createElement("span");
        arrow.className = "chat-weather-card__wind-dir";
        arrow.textContent = "\u2191"; // ↑
        arrow.setAttribute("style", `transform: rotate(${cardinalToDegrees(dir)}deg); display:inline-block;`);
        windEl.appendChild(arrow);
      }
      const valEl = document.createElement("span");
      valEl.className = "chat-weather-card__wind-value";
      valEl.textContent = `${safeNum(current.wind_kmh)} km/h`;
      windEl.appendChild(valEl);
      if (dir) {
        const cardinal = document.createElement("span");
        cardinal.className = "chat-weather-card__wind-cardinal";
        cardinal.textContent = dir;
        windEl.appendChild(cardinal);
      }
      card.appendChild(windEl);
    }
  }

  // Day strip: always shown (up to 3 days) when forecast[] is non-empty.
  // Even in historical mode the strip renders the single matching day so
  // the layout doesn't shift between modes.
  if (days.length > 0) {
    const strip = document.createElement("div");
    strip.className = "chat-weather-card__days";
    for (const day of days) {
      const cell = document.createElement("div");
      cell.className = "chat-weather-card__day";
      const label = document.createElement("div");
      label.className = "chat-weather-card__day-label";
      label.textContent = formatDayShort(safeStr(day.date), safeStr(location.timezone));
      cell.appendChild(label);
      const dayIcon = document.createElement("div");
      dayIcon.className = "chat-weather-card__day-icon";
      dayIcon.textContent = conditionEmoji(safeNum(day.condition_code));
      cell.appendChild(dayIcon);
      const hi = document.createElement("div");
      hi.className = "chat-weather-card__day-hi";
      hi.textContent = `${safeNum(day.t_max_c)}° / ${safeNum(day.t_min_c)}°`;
      cell.appendChild(hi);
      const precip = document.createElement("div");
      const rainPct = safeNum(day.chance_of_rain);
      precip.className = "chat-weather-card__day-precip" + (rainPct >= 30 ? " chat-weather-card__day-precip--wet" : "");
      precip.textContent = rainPct > 0 ? `pluie ${rainPct}%` : "—";
      cell.appendChild(precip);
      strip.appendChild(cell);
    }
    card.appendChild(strip);
  }

  // Insert as a sibling of the tool trace <details>, before the
  // replay button so the visual flow reads: [prose/hint] → [tool
  // trace] → [weather card] → [🔊]. The card stays visible when the
  // tool trace is collapsed because it sits OUTSIDE the `<details>`.
  // Fall back to a plain appendChild if the replay button isn't
  // mounted yet (e.g. when called from a renderHistory pass before
  // `ensureReplayButton` has run on a freshly-rehydrated bubble).
  const replayBtn = assistantEl.querySelector(".chat-message-replay");
  if (replayBtn) {
    assistantEl.insertBefore(card, replayBtn);
  } else {
    assistantEl.appendChild(card);
  }
  // Track the card so `applyMarkdown`'s `innerHTML = ""` reset can
  // re-mount it after every streaming tick. The array grows with
  // each new tool call that has a widget; multiple cards in a
  // single assistant bubble (e.g. weather + calculate) stay in the
  // order they were rendered.
  if (!assistantEl._weatherCards) assistantEl._weatherCards = [];
  assistantEl._weatherCards.push(card);
  messagesEl.scrollTop = messagesEl.scrollHeight;
  return card;
}

// Build a single label + value pair for the meta row. Caller is
// responsible for not invoking this with a falsy underlying number —
// the truthy guards at the call site (`if (current.humidity) ...`)
// hide the row entirely when the field is missing or zero.
function makeMetaItem(label, value) {
  const wrap = document.createElement("span");
  wrap.className = "chat-weather-card__meta-item";
  const lbl = document.createElement("span");
  lbl.className = "chat-weather-card__meta-label";
  lbl.textContent = label;
  const val = document.createElement("span");
  val.className = "chat-weather-card__meta-value";
  val.textContent = value;
  wrap.appendChild(lbl);
  wrap.appendChild(val);
  return wrap;
}

// Map the WeatherAPI compass heading string ("N", "WSW", …) to degrees
// so the wind arrow can be rotated to face the actual wind direction.
// Returns null for unrecognised inputs (the arrow is omitted in that
// case, the speed still shows).
const CARDINAL_TO_DEG = {
  N: 0, NNE: 22.5, NE: 45, ENE: 67.5,
  E: 90, ESE: 112.5, SE: 135, SSE: 157.5,
  S: 180, SSW: 202.5, SW: 225, WSW: 247.5,
  W: 270, WNW: 292.5, NW: 315, NNW: 337.5,
};
function cardinalToDegrees(s) {
  return Object.prototype.hasOwnProperty.call(CARDINAL_TO_DEG, s)
    ? CARDINAL_TO_DEG[s]
    : null;
}

// Hide the assistant bubble that just emitted a tool call while the
// tool is running, so the user doesn't see the LLM's "let me check…"
// preamble stealing focus from the eventual widget. Original prose
// stays in the DOM (CSS only hides non-placeholder children) so a
// tool failure can unhide it without any state to roll back.
//
// Multiple chained tool calls are idempotent — calling this twice is
// a no-op once the placeholder exists. Removing it via `loading=false`
// restores visibility to whatever the LLM streamed in.
function setAssistantToolPending(assistantEl, loading) {
  if (!assistantEl) return;
  const placeholder = assistantEl.querySelector(".chat-message__tool-loading");
  if (loading) {
    if (placeholder) return;
    assistantEl.classList.add("chat-message--tool-pending");
    const el = document.createElement("div");
    el.className = "chat-message__tool-loading";
    el.textContent = "Préparation de la réponse\u2026";
    assistantEl.appendChild(el);
    return;
  }
  assistantEl.classList.remove("chat-message--tool-pending");
  if (placeholder) placeholder.remove();
}

// Walk up the DOM tree to find the assistant bubble that owns a
// tool bubble. With the tool trace now inlined inside the assistant
// bubble (see `appendToolBubble`), the assistant is the closest
// `.chat-assistant` ancestor.
function findAssistantBubble(el) {
  let cur = el?.parentElement;
  while (cur && !cur.classList?.contains("chat-assistant")) {
    cur = cur.parentElement;
  }
  return cur || null;
}

// Finalize the assistant bubble once its tool call has produced a
// weather widget:
//
//   1. Always unhide the children that `setAssistantToolPending` was
//      hiding during the tool run, so the prose streams back into
//      view once the tool settles.
//   2. If the prose is a short, single-line acknowledgment (typically
//      "Voici les informations demandées."), keep it as the answer —
//      the model already followed the prompt's instruction.
//   3. Otherwise replace the content with a small italic hint
//      pointing at the card. The full prose still lives in
//      localStorage so the LLM context and reloads aren't affected.
//
// Called from `resolveToolBubble` (rehydration / immediate) and from
// `streamReply`'s finally block (live stream — deferred to avoid
// clobbering tokens still in flight when the tool result lands).
function finalizeAssistantForToolResult(toolBubble, assistantEl) {
  // When called from `resolveToolBubble`, the tool trace is a
  // `<details>` wrapper; `assistantEl` is its closest `.chat-assistant`
  // ancestor. When called from `streamReply`'s finally block with the
  // raw `.chat-tool-bubble` div from a previous layout, fall back to
  // the upward walk.
  if (!assistantEl) {
    assistantEl = findAssistantBubble(toolBubble);
  }
  if (!assistantEl) return;
  setAssistantToolPending(assistantEl, false);
  const text = (assistantEl.textContent || "").trim();
  // "Short" = non-empty, single line, ≤ 120 chars. The threshold is
  // intentionally generous — anything that fits comfortably on one
  // line is kept so the user sees a real acknowledgment.
  const isShort = text.length > 0 && text.length <= 120 && !text.includes("\n");
  if (isShort) {
    messagesEl.scrollTop = messagesEl.scrollHeight;
    return;
  }
  assistantEl.classList.add("chat-message--weather-replaced");
  // The weather card IS the answer, so the assistant's prose
  // (preamble + ack) has to go. We can't blindly wipe every child
  // any more — the bubble now hosts the inlined tool trace
  // `<details>`, the weather card itself, and the replay button.
  // Collect the children to keep, then drop the rest.
  const keep = new Set();
  for (const child of Array.from(assistantEl.children)) {
    if (child.classList?.contains("chat-message__tool-usage")
        || child.classList?.contains("chat-weather-card")
        || child.classList?.contains("chat-message-replay")) {
      keep.add(child);
    }
  }
  for (const child of Array.from(assistantEl.children)) {
    if (!keep.has(child)) assistantEl.removeChild(child);
  }
  const hint = document.createElement("span");
  hint.className = "chat-message__weather-hint";
  hint.textContent = "\u{1F324} Détails ci-dessous.";
  // Insert the hint directly before the weather card so the visible
  // flow reads: [tool trace] → ["Détails ci-dessous."] → [weather
  // card] → [🔊]. The user explicitly asked for the card to come
  // after the hint — appending at the end would push the card above
  // the hint, which inverts the contract. Falls back to appendChild
  // when no card is present (the hint then sits just above the 🔊).
  const card = assistantEl.querySelector(".chat-weather-card");
  if (card) {
    assistantEl.insertBefore(hint, card);
  } else {
    assistantEl.appendChild(hint);
  }
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

// Best-effort parse of a `tool_result` content payload into a structured
// weather object. Returns `null` when the payload is missing, malformed,
// or doesn't look like the `get_weather` shape — callers fall back to the
// summary-only rendering in that case. Wrapped in try/catch because the
// content string is server-generated JSON and we never want a parse
// failure to throw out of `resolveToolBubble`.
//
// Wire shape: the weather agent wraps its payload as
//   {"ok": true, "data": {"location": {...}, "current": {...}, "forecast": [...]}, "source": "...", "fetched_at": "..."}
// We accept both the wrapped (current SSE `tool_result` content) and the
// unwrapped (caller-side / test fixture) shapes so the parser stays
// robust against future refactors that drop the envelope.
function parseWeatherPayload(content) {
  if (!content) return null;
  let parsed;
  try {
    parsed = JSON.parse(String(content));
  } catch (_e) {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const inner = (parsed.data && typeof parsed.data === "object")
    ? parsed.data
    : parsed;
  if (!inner.location || typeof inner.location !== "object") return null;
  return inner;
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

// `loadModels()` is no longer a network call. The model list now
// arrives inside the `GET /api/features` response (as the
// `llm_models` field — see `http/features.rs`); the chat dropdown
// reads it from the feature registry. Two consequences:
//
//   1. This function is synchronous and safe to call from a feature
//      subscriber — no `await` round-trips, no race against
//      `app-shell-mounted`.
//   2. A slow / unreachable Ollama no longer blocks the dropdown
//      paint: `/api/features` bounds the upstream fetch with
//      `UPSTREAM_MODELS_TIMEOUT` (3s) and collapses to
//      `[default_model]` on any failure, so the dropdown still has
//      one usable entry when features resolve.
//
// The `disabled` toggle (hide form, show "disabled" notice when
// LLM is off) is still owned by `loadModels` — that path doesn't
// depend on the upstream at all, it just keys on `feature("llm")`.
function loadModels() {
  // Toggle the visible / hidden state of the form based on whether
  // the LLM proxy is wired. This is the only "feature disabled"
  // gate in the chat UI: when `llm: false` the form goes away and
  // the user sees the `#chat-disabled-notice`. When `llm: true`
  // the form comes back regardless of how many models the upstream
  // actually served (the server-side fallback always returns at
  // least `[default_model]`).
  const chatHeaderEl = document.querySelector(".chat-header");
  const chatAudioEl = document.querySelector(".chat-audio");
  const voiceGraphEl = document.querySelector(".voice-graph");
  const chatAdvancedEl = document.querySelector(".chat-advanced");
  const inDiscussionView = chatHeaderEl !== null;
  const llmOn = feature("llm");
  if (!llmOn) {
    disabledNoticeEl.hidden = false;
    formEl.hidden = true;
    if (inDiscussionView) {
      chatHeaderEl.hidden = true;
      chatAudioEl.hidden = true;
      voiceGraphEl.hidden = true;
      chatAdvancedEl.hidden = true;
    }
    statusEl.textContent = "disabled";
    statusEl.className = "status idle";
    modelsLoaded = true;
    return;
  }
  disabledNoticeEl.hidden = true;
  formEl.hidden = false;
  if (inDiscussionView) {
    chatHeaderEl.hidden = false;
    chatAudioEl.hidden = false;
    voiceGraphEl.hidden = false;
    chatAdvancedEl.hidden = false;
  }
  // Refresh the pill so a previous "disabled" state disappears.
  renderStatus();

  // Read the model list from the feature registry. The server
  // populates `feature("llm_models")` from the upstream `/v1/models`
  // at `/api/features` time and collapses to `[default_model]` on
  // any failure (see `UPSTREAM_MODELS_TIMEOUT` in `llm/proxy.rs`).
  // We never have to defend against a missing field here — the
  // `Object.freeze` `DEFAULT_FEATURES` keeps the default at `[]`,
  // and the merge in `refreshFeatures` overwrites it with whatever
  // the server sent.
  const items = Array.isArray(nagentFeatures.llm_models)
    ? nagentFeatures.llm_models
    : [];
  modelEl.innerHTML = "";
  if (items.length === 0) {
    // The server contract guarantees `llm_models` is non-empty when
    // `llm: true`; if it slipped through empty we still render a
    // placeholder rather than an invisible dropdown.
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "(no models)";
    modelEl.appendChild(opt);
    modelsLoaded = true;
    return;
  }
  for (const id of items) {
    const opt = document.createElement("option");
    opt.value = id || "";
    opt.textContent = id || "(unnamed)";
    modelEl.appendChild(opt);
  }
  // Restore the user's last selection so a fresh page load lands
  // on the model they were already using, instead of jumping back
  // to the first entry of the list. A stale id (saved model no
  // longer served by the backend) is silently ignored — the
  // browser falls back to the first appended <option>, and the
  // next `change` event will overwrite the stale entry with
  // whatever the user picks.
  const saved = loadSelectedModel();
  if (saved && Array.from(modelEl.options).some((o) => o.value === saved)) {
    modelEl.value = saved;
  }
  // First successful load wires the change listener so every
  // future pick is persisted. The `_wired` guard makes the wire
  // idempotent across `loadModels` re-runs (visibility refresh,
  // error retry, etc.).
  if (!_modelChangeWired && modelEl.addEventListener) {
    _modelChangeWired = true;
    modelEl.addEventListener("change", () => {
      saveSelectedModel(modelEl.value);
    });
  }
  modelsLoaded = true;
}

// ---- Streaming reply -------------------------------------------------------
//
// ---- User geolocation ------------------------------------------------------
//
// The user opts in to sharing their approximate location once via the
// `Share location` button in the form footer. After that, every LLM
// turn prepends an ephemeral system message carrying the cached
// `lat,lon` + accuracy + capture time. The block is rebuilt on every
// request (timestamp, age) but only re-acquired from the browser when
// the cache is stale (> 7 days) or the user clicks Refresh / Forget.
//
// The block is never persisted to `localStorage` session history — see
// the comment at the `messages.unshift(locBlock)` call site.

/// Build the ephemeral location system message for this turn, or
/// `null` when sharing is disabled or no position is cached.
///
/// The cache is refreshed once per UI load by `refreshLocationOnBoot`
/// (silent — browser permission was granted on the original opt-in),
/// so we trust whatever is in `localStorage` at message-build time.
/// Long sessions where the user has not moved the page still rely on
/// the boot-time fix; the "captured X ago" phrase in the message body
/// lets the LLM flag obvious staleness.
function maybeBuildLocationBlock() {
  if (!loadLocationEnabled()) return null;
  const cached = loadCachedLocation();
  if (!cached) return null;
  return {
    role: "system",
    content: formatLocationMessage(cached),
  };
}

// ---- Browser timezone ------------------------------------------------------
//
// The user can opt in to forwarding their IANA timezone to the LLM
// from the Advanced drawer. The block is ephemeral (sent on each
// request, never persisted into the session history) so a flight
// across timezones picks up the new zone on the very next turn
// without any bookkeeping on our side.
//
// The IANA name is recomputed at message-build time from
// `Intl.DateTimeFormat().resolvedOptions().timeZone` rather than
// cached, because that value reflects the *system* timezone — which
// the user can change (travel, daylight saving, manual override)
// between page loads. We do cache the "current local time" snapshot
// in the message body so the LLM has a usable timestamp without
// having to call `get_datetime` first; the marker prefix matches
// `USER_TIMEZONE_MARKER` on the Rust side so the admin kill-switch
// (`LLM_ALLOW_USER_TIMEZONE=false`) can drop the block before it
// reaches the upstream model.

/// Detect the browser's IANA timezone, or `null` when unavailable.
///
/// `Intl.DateTimeFormat` is available in every modern browser; on
/// exotic runtimes (very old Safari, some embedded WebViews) the
/// resolved timezone can come back as `undefined` or an empty string.
/// We treat both as "no timezone" so the toggle quietly does nothing
/// rather than sending `"UTC"` to a user who actually has a real
/// zone but whose browser refused to disclose it.
function detectBrowserTimezone() {
  try {
    const tz = Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (typeof tz !== "string" || !tz) return null;
    return tz;
  } catch (_e) {
    return null;
  }
}

function loadTimezoneEnabled() {
  // Same boolean string convention as `loadLocationEnabled` — "true"
  // means on, anything else (including absence) means off.
  try { return lsGet(TIMEZONE_ENABLED_KEY) === "true"; }
  catch (_) { return false; }
}

function setTimezoneEnabled(enabled) {
  if (enabled) lsSet(TIMEZONE_ENABLED_KEY, "true");
  else lsSet(TIMEZONE_ENABLED_KEY, "false");
}

/// Render the cached IANA name into the body of an ephemeral system
/// message. The marker prefix MUST stay at the start so the server's
/// defensive strip can match it. The `now` parameter is overridable
/// for unit tests.
function formatTimezoneMessage(tz, now = new Date()) {
  // `Intl.DateTimeFormat` with `timeZoneName: "shortOffset"` yields
  // strings like "GMT+1" or "GMT-05:00"; we want the colon-bearing
  // form so the LLM has a deterministic shape to parse. Falling back
  // to the long name (e.g. "Central European Summer Time") keeps the
  // message informative even on browsers that do not implement
  // `shortOffset`.
  let offsetLabel;
  try {
    const parts = new Intl.DateTimeFormat("en-US", {
      timeZone: tz,
      timeZoneName: "shortOffset",
    }).formatToParts(now);
    offsetLabel = parts.find((p) => p.type === "timeZoneName")?.value
      || tz;
  } catch (_e) {
    offsetLabel = tz;
  }
  // Snapshot the local time in the user's zone so the LLM has an
  // instant to anchor on without calling `get_datetime`. The marker
  // paragraph below tells it to prefer the tool when it needs an
  // authoritative answer.
  let localTime;
  try {
    localTime = new Intl.DateTimeFormat("sv-SE", {
      timeZone: tz,
      year: "numeric", month: "2-digit", day: "2-digit",
      hour: "2-digit", minute: "2-digit", second: "2-digit",
      weekday: "long",
    }).format(now);
  } catch (_e) {
    localTime = now.toISOString();
  }
  return (
    `The user's local timezone is "${tz}" (${offsetLabel}, current local `
    + `time on the user's device: ${localTime}). Always answer time-related `
    + `questions ("what time is it", "today", "this week", "tonight", ...) in `
    + `this timezone unless the user explicitly names another one. When you `
    + `need an authoritative current time, call get_datetime with `
    + `timezone="${tz}" so the tool's answer matches what the user sees on `
    + `their device. The snapshot above is captured at message-build time `
    + `and may be a few seconds stale by the time you see it.`
  );
}

/// Build the ephemeral timezone system message for this turn, or
/// `null` when sharing is disabled or the browser refused to disclose
/// a zone. Mirrors `maybeBuildLocationBlock` so the two opt-ins are
/// symmetric.
function maybeBuildTimezoneBlock() {
  if (!loadTimezoneEnabled()) return null;
  const tz = detectBrowserTimezone();
  if (!tz) return null;
  return {
    role: "system",
    content: formatTimezoneMessage(tz),
  };
}

/// Update the Advanced-panel timezone control from the current
/// toggle + detected zone. Defensive about missing elements so an
/// older `index.html` does not crash the rest of the chat boot path.
function renderTimezoneUi() {
  if (timezoneToggleEl) {
    timezoneToggleEl.checked = loadTimezoneEnabled();
  }
  if (timezoneStatusEl) {
    const tz = detectBrowserTimezone();
    if (tz) {
      timezoneStatusEl.textContent = `Detected: ${tz}`;
    } else {
      timezoneStatusEl.textContent =
        "Browser did not disclose a timezone — toggle has no effect.";
    }
  }
}

/// Click handler for `#chat-timezone-toggle`. The persisted flag is
/// read at message-build time so a reload picks up the latest choice
/// without any further bookkeeping.
///
/// The server-side preference row is atomic, so the PUT body
/// always carries the *current* location flag alongside the
/// freshly-clicked timezone flag — `acquireCurrentPreferenceFlags()`
/// returns the cached state populated by `loadPreferencesFromServer`
/// at boot, falling back to localStorage when the boot fetch has
/// not resolved yet.
function handleTimezoneToggleChange() {
  const flags = acquireCurrentPreferenceFlags();
  savePreferencesToServer(
    flags.location,
    !!timezoneToggleEl?.checked,
    flags.reply_language,
    flags.additional_instructions,
    flags.temperature,
  );
}

/// Update every geolocation control from the current cache + toggle.
/// Called on boot, after every successful fetch, and on toggle /
/// refresh / forget clicks. Kept defensive about missing elements so
/// an older `index.html` doesn't crash the rest of the chat.
function renderLocationUi() {
  const cached = loadCachedLocation();
  const enabled = loadLocationEnabled();
  const inSecureContext = (typeof window !== "undefined"
    && window.isSecureContext !== false);

  if (locationShareBtn) {
    if (!inSecureContext) {
      // Insecure context (plain-HTTP LAN): the browser would refuse
      // the geolocation call. Hide the opt-in entirely instead of
      // looking broken; the user can still enable HTTPS / localhost.
      locationShareBtn.hidden = true;
      locationShareBtn.title =
        "Geolocation requires HTTPS or localhost";
    } else if (cached) {
      // A position is already cached — the pill in the header takes
      // over as the entry point.
      locationShareBtn.hidden = true;
    } else {
      locationShareBtn.hidden = false;
      locationShareBtn.title =
        "Share your approximate location with the LLM";
    }
  }

  const hasLoc = !!cached;
  if (locationPill) locationPill.hidden = !hasLoc;
  if (locationAdvBox) locationAdvBox.hidden = !hasLoc;

  if (hasLoc) {
    const acc = Math.max(0, Math.round(cached.accuracy));
    const ageText = formatRelativeTime(Date.now() - cached.timestamp);
    if (locationPillText) {
      locationPillText.textContent = `±${acc} m · ${ageText} ago`;
    }
    if (locationStatusEl) {
      const captured = new Date(cached.timestamp)
        .toISOString().replace(/\.\d{3}Z$/, "Z");
      locationStatusEl.textContent =
        `Last updated: ${captured} · ±${acc} m · ${ageText} ago`;
    }
  }

  if (locationToggleEl) {
    locationToggleEl.checked = enabled;
    locationToggleEl.disabled = !hasLoc;
  }
}

/// Click handler for `#chat-location-share`. Asks the browser for a
/// fresh position, caches it, flips the enabled flag, and refreshes
/// the UI. Surfaces errors inline via the advanced status text so
/// the user knows why nothing happened.
///
/// Persists the location flag through the server-side preferences
/// row (the localStorage mirror is updated by `savePreferencesToServer`
/// so a slow PUT never blocks the toggle paint).
async function handleShareLocationClick() {
  try {
    const loc = await getLocation();
    saveCachedLocation(loc);
    const flags = acquireCurrentPreferenceFlags();
    savePreferencesToServer(
      true, flags.timezone, flags.reply_language,
      flags.additional_instructions, flags.temperature,
    );
    renderLocationUi();
  } catch (err) {
    // GeolocationPositionError codes map cleanly to user-facing text;
    // the message field is set by us in the API-unavailable branch.
    const code = err?.code;
    let msg = err?.message || String(err);
    if (code === 1) msg = "Permission denied";
    else if (code === 2) msg = "Position unavailable";
    else if (code === 3) msg = "Geolocation timed out";
    if (locationStatusEl) {
      locationStatusEl.textContent = `Could not share location: ${msg}`;
    }
  }
}

/// Click handler for the advanced-panel toggle. Disabling the toggle
/// preserves the cache so the user can re-enable without re-granting
/// browser permission; "Forget my location" is the destructive path.
///
/// Persists through the server-side preferences row so the choice
/// survives a browser switch / private-browsing session / profile
/// reset (the previous localStorage-only storage lost all three).
function handleLocationToggleChange() {
  const flags = acquireCurrentPreferenceFlags();
  savePreferencesToServer(
    !!locationToggleEl?.checked,
    flags.timezone,
    flags.reply_language,
    flags.additional_instructions,
    flags.temperature,
  );
  renderLocationUi();
}

/// Click handler for `Refresh my location`. Same as the initial opt-in
/// but bypasses the cache check and updates the UI eagerly.
async function handleLocationRefreshClick() {
  try {
    const loc = await getLocation();
    saveCachedLocation(loc);
    const flags = acquireCurrentPreferenceFlags();
    savePreferencesToServer(
      true, flags.timezone, flags.reply_language,
      flags.additional_instructions, flags.temperature,
    );
    if (locationToggleEl) locationToggleEl.checked = true;
    renderLocationUi();
  } catch (err) {
    if (locationStatusEl) {
      locationStatusEl.textContent =
        `Refresh failed: ${err?.message || err}`;
    }
  }
}

/// Click handler for `Forget my location`. Wipes the cache and the
/// enabled flag; the next turn does not get a location block.
function handleLocationForgetClick() {
  clearCachedLocation();
  const flags = acquireCurrentPreferenceFlags();
  savePreferencesToServer(
    false, flags.timezone, flags.reply_language,
    flags.additional_instructions, flags.temperature,
  );
  if (locationToggleEl) locationToggleEl.checked = false;
  renderLocationUi();
}

/// Page-load refresh: if the user opted in previously, ask the
/// browser for a fresh position (no UI prompt — permission was
/// granted on the original opt-in). The result overwrites the cache
/// so a different day / different place reload always starts from
/// the user's current location rather than last week's. Permission
/// revoked / denied in the meantime clears the toggle and the cache
/// so the UI matches reality on the next render. The server-side
/// preferences row is left untouched — a permission change on this
/// device does NOT echo back to the server, only the user's explicit
/// toggle click does.
async function refreshLocationOnBoot() {
  if (!loadLocationEnabled()) return;
  try {
    const loc = await getLocation();
    saveCachedLocation(loc);
  } catch (_err) {
    // `getLocation` rejects with PERMISSION_DENIED (1) if the user
    // revoked the grant in browser settings, or with
    // POSITION_UNAVAILABLE / TIMEOUT if the device cannot produce a
    // fix. In every case the safe behaviour is to stop pretending
    // we have location: clear the toggle and the cache, then let
    // `renderLocationUi` hide the controls.
    clearCachedLocation();
    const flags = acquireCurrentPreferenceFlags();
    savePreferencesToServer(
      false, flags.timezone, flags.reply_language,
      flags.additional_instructions, flags.temperature,
    );
  }
  renderLocationUi();
}

/// Navigate from the header `#chat-location-pill` to the Settings tab's
/// "Privacy & context" section. Used to be an `Advanced`-disclosure
/// open + scroll; since the Settings tab rework, the location controls
/// live under `#settings-privacy` (plan: settings-tab-rework), so the
/// pill becomes a shortcut to that section. We dispatch a synthetic
/// `modechange` so any listener that gates on Discussion-tab-only
/// behaviour (e.g. the `AudioCapture` re-bind) tears down cleanly
/// when the user jumps out of Discussion.
function openAdvancedForLocation() {
  const settingsBtn = document.getElementById("mode-settings-btn");
  if (settingsBtn) settingsBtn.click();
  const privacy = document.getElementById("settings-privacy");
  if (privacy) privacy.scrollIntoView({ block: "start", behavior: "smooth" });
}

// `streamReply(sessionId, userText)` runs the LLM request bound to a
// specific session. The session id is captured at call time and used
// for every read/write — never re-read from `activeSessionId()` — so a
// mid-flight session switch can't route the assistant bubble or its
// persisted entry into the wrong conversation.

async function streamReply(sessionId, userText) {
  // SEV 2 fix: mint (or reuse) the server-bound chat session id.
  // The first request on every page load triggers the POST. The
  // returned id is cached in localStorage so subsequent requests
  // skip the round trip. A 403 from any documents /
  // chat-completions endpoint triggers a re-mint via
  // `refreshServerSessionId()`.
  const serverSid = await getServerSessionId();
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
  // Mark the bubble as actively streaming so the per-bubble replay
  // button (and any other "finished reply" affordance) stays hidden
  // until the reply is fully rendered. The flag is dropped in the
  // `finally` block of `streamReply` once the SSE stream ends.
  assistantEl.classList.add("chat-message--streaming");
  // Render an inline bouncing-dots loader inside the assistant bubble
  // while we wait for the LLM's first token. Without this, the bubble
  // sits empty during the Ollama cold start (sometimes 30s+ on a
  // freshly-pulled model) and looks like a frozen UI. The first delta
  // below clears the loader before any text is written.
  //
  // We use `appendChild` (NOT `innerHTML =`) so the replay button
  // `appendBubble` just attached isn't destroyed by overwriting the
  // bubble's innerHTML.
  const loader = document.createElement("span");
  loader.className = "chat-loader";
  loader.setAttribute("role", "status");
  loader.setAttribute("aria-label", "Loading response");
  loader.innerHTML = '<span class="dot"></span>'
    + '<span class="dot"></span>'
    + '<span class="dot"></span>';
  assistantEl.appendChild(loader);

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
  // `audioCapture` may be null if the user is in Transcript mode
  // (the Discussion shell is unmounted) — guard so a mode switch
  // during an in-flight reply does not crash.
  audioCapture?.stop();

  // TTS: lazily create the player on the first turn so the
  // AudioContext is created from a user-gesture path (we cannot
  // construct it here because `streamReply` may be called as a
  // queued continuation that started before any user input — e.g.
  // a voice transcript from `Ctrl+Shift+D`). The first toggle /
  // first Test-button click has already created the context by the
  // time the user lands here. If TTS is disabled at boot, this is
  // a no-op and `feed` never gets called.
  const ttsSettings = getTtsSettings();
  const tts = ttsSettings.enabled ? getOrCreateTtsPlayer() : null;
  // Honor the user's `stopOnSend` preference: when a new turn
  // starts while audio from the previous reply is still playing,
  // cut it off so the new reply isn't drowned out by the old one.
  if (tts && ttsSettings.stopOnSend) tts.stopAll();

  const messages = [
    ...(systemEl.value.trim()
      ? [{ role: "system", content: systemEl.value.trim() }]
      : []),
    ...earlier
      .filter((m) => m.role !== "system")
      .map((m) => ({ role: m.role, content: m.content })),
    { role: "user", content: userText != null ? userText : last.content },
  ];
  // The location block is *ephemeral*: it travels only in the
  // request payload for this turn and is never written back to the
  // session history (`h.push(...)` below intentionally omits it).
  // This keeps the visible chat log free of system noise and means a
  // follow-up turn in a future session starts with no location
  // unless the user re-consented.
  const locBlock = maybeBuildLocationBlock();
  if (locBlock) messages.unshift(locBlock);
  // Same ephemerality rule as the location block: the timezone is
  // recomputed on every turn (system clock may have changed since
  // the last request) and never persisted into the session history.
  // Inserted AFTER the location block so the admin system prompt and
  // the location block both stay ahead of it; `unshift` in the same
  // order keeps the most-recently-added block at index 0.
  const tzBlock = maybeBuildTimezoneBlock();
  if (tzBlock) messages.unshift(tzBlock);
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
      // `RequireAuth` rejects every state-changing request that
      // does not carry the per-session CSRF token. `csrfHeaders()`
      // returns `undefined` when the session is gone, which the
      // HeadersInit spread below silently drops (rather than
      // setting the header to the string "undefined").
// `X-Chat-Session-Id` propagates the **server-bound** chat
        // session id (SEV 2 fix). The browser mints this via
        // `POST /v1/chat/session` and caches the returned UUID
        // in localStorage; the server verifies the binding
        // against `chat_sessions.user_id` on every request so
        // a user cannot reach another user's docs by forging
        // the header. The id is captured at request start
        // (see `inflight.sessionId`) so a session switch
        // mid-stream does not confuse the server.
        headers: {
          "Content-Type": "application/json",
          "X-Chat-Session-Id": serverSid,
          ...window.nagentAuth?.csrfHeaders(),
        },
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
    // Reasoning vs content (R8 follow-up): the bubble displays the
    // model's *answer* in `delta.content` and the model's *thinking*
    // in `delta.reasoning` (qwen3.5 with reasoning, DeepSeek-R1, etc.).
    // Mixing the two streams produces the "stuck on reasoning" bug:
    // when every chunk has `content: ""` and only `reasoning` is
    // populated, the bubble fills with internal monologue and the
    // user never sees the actual answer (which may arrive later or
    // may have been truncated by a length cap).
    //
    // Reasoning goes into a collapsible `<details>` block above the
    // main answer; the visible reply is `delta.content` only. TTS
    // also reads only the answer so the user is not subjected to
    // out-loud chain-of-thought.
    let accumulatedReasoning = "";
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
          // Reasoning vs content (R8 follow-up):
          // - `delta.content` is the visible answer the user wants
          //   to read.
          // - `delta.reasoning` is the model's chain-of-thought,
          //   surfaced by qwen3.5 (with reasoning on) and DeepSeek-R1.
          //
          // We deliberately do NOT treat reasoning as a fallback
          // for content: doing so displayed the model's internal
          // monologue as the reply and, when the answer was
          // truncated by a length cap, left the user staring at a
          // half-rendered bubble with no visible answer. Reasoning
          // is now routed to a collapsible `<details>` block above
          // the main bubble content.
          const deltaObj = evt?.choices?.[0]?.delta || {};
          const contentDelta = typeof deltaObj.content === "string"
            ? deltaObj.content
            : "";
          const reasoningDelta = typeof deltaObj.reasoning === "string"
            ? deltaObj.reasoning
            : "";
          if (reasoningDelta.length > 0) {
            accumulatedReasoning += reasoningDelta;
            // Lazily create the reasoning `<details>` block on the
            // first reasoning token. Collapsed by default so the
            // user sees only what the model actually answered; one
            // click expands it for the curious / for debugging
            // tool-call traces.
            if (!assistantEl.querySelector(".chat-message__reasoning")) {
              const details = document.createElement("details");
              details.className = "chat-message__reasoning";
              const summary = document.createElement("summary");
              summary.textContent = "Reasoning";
              details.appendChild(summary);
              const pre = document.createElement("pre");
              pre.className = "chat-message__reasoning-body";
              details.appendChild(pre);
              // Insert before the existing markdown body so the
              // user reads the answer, not the trace, by default.
              const md = assistantEl.querySelector(".chat-message--markdown") || null;
              assistantEl.insertBefore(details, md);
            }
            const reasoningEl = assistantEl.querySelector(".chat-message__reasoning-body");
            if (reasoningEl) reasoningEl.textContent = accumulatedReasoning;
            // While only reasoning is flowing, show "Reasoning…"
            // so the user gets live feedback. The pill switches to
            // "Streaming…" the first time a content token arrives.
            if (!streamingStarted) {
              streamingStarted = true;
              setStreamState({ text: "Reasoning…", cls: "connecting" });
              const loaderEl = assistantEl.querySelector(".chat-loader");
              if (loaderEl) loaderEl.remove();
              ensureReplayButton(assistantEl);
            }
            messagesEl.scrollTop = messagesEl.scrollHeight;
          }
          if (contentDelta.length > 0) {
            // First content token: switch the pill from
            // "Reasoning…" to "Streaming…" if it was already on.
            // The reasoning block is already in place by this point
            // so we only mutate the pill text here.
            if (!streamingStarted) {
              streamingStarted = true;
              setStreamState({ text: "Streaming…", cls: "connecting" });
              // Strip the inline loader before writing real text so
              // the bubble transitions cleanly into the reply.
              //
              // The `innerHTML = ""` would normally also wipe the
              // replay button attached by `appendBubble`, but we
              // re-attach it below via `ensureReplayButton`.
              const loaderEl = assistantEl.querySelector(".chat-loader");
              if (loaderEl) loaderEl.remove();
              ensureReplayButton(assistantEl);
            } else if (reasoningDelta.length === 0) {
              // Reasoning arrived earlier; first content token now.
              setStreamState({ text: "Streaming…", cls: "connecting" });
            }
            accumulated += contentDelta;
            // Re-render the accumulated text as sanitized markdown.
            // We coalesce updates via requestAnimationFrame so a
            // burst of small tokens only triggers one parse per
            // animation frame, keeping the streaming path cheap.
            scheduleMarkdownRender(assistantEl, () => accumulated);
            messagesEl.scrollTop = messagesEl.scrollHeight;
            // Feed the raw `contentDelta` (NOT the markdown-rendered
            // HTML) to TTS so it doesn't read out `**bold**`, code
            // fences, etc. Reasoning is intentionally NOT fed —
            // TTS would otherwise speak the model's internal
            // monologue out loud.
            if (tts) tts.feed(sanitizeForTts(contentDelta));
          }
        } catch (_e) { /* skip malformed line */ }
      }
    }
    finalSource = accumulated;
    // Reasoning-without-content fallback: the model produced only
    // chain-of-thought and never emitted a `content` reply (typical
    // for a reasoning model truncated by a length cap). Without
    // this the user would see an empty bubble above the collapsible
    // reasoning block, which looks broken. Drop a short note so the
    // bubble is never silently empty.
    if (finalSource.length === 0 && accumulatedReasoning.length > 0) {
      finalSource = "[reasoning only — no answer received]";
      const note = document.createElement("p");
      note.className = "chat-message__reasoning-only-note";
      note.textContent = finalSource;
      // Insert before the reasoning `<details>` so the note reads
      // as the assistant's "answer" placeholder and the reasoning
      // stays below as supporting material.
      const reasoningEl = assistantEl.querySelector(".chat-message__reasoning");
      assistantEl.insertBefore(note, reasoningEl);
    }
    // Flush the TTS sentence buffer so the trailing partial sentence
    // (no terminator) is also synthesised and played. No-op when TTS
    // is disabled.
    if (tts) tts.flush();
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
      // Stop mid-reply TTS playback on user-initiated abort (the
      // Stop button). We do NOT stop on stopOnSend because that
      // case is handled at the top of streamReply.
      if (tts) tts.stopAll();
    } else {
      // Surface the upstream's model-not-found envelope as a
      // picker so the user can swap to a model their Ollama
      // actually serves with one click. Anything else stays a
      // plain `[error] ...` marker — error paths stay
      // developer-facing diagnostics unless we can recover.
      const handled = await renderModelNotFoundIfApplicable(assistantEl, e);
      if (!handled) {
        const errText = `[error] ${e?.message || e}`;
        assistantEl.textContent = errText;
        finalSource = errText;
        appendError(e?.message || String(e));
      }
      // On a hard error the audio would keep talking about a stale
      // half-answer. Stop it.
      if (tts) tts.stopAll();
    }
  } finally {
    // Always clear the `chat-message--tool-pending` class and its
    // placeholder at stream end so the assistant's prose is visible.
    // The class was added by `appendToolBubble` to hide the prose
    // while a tool runs (so the placeholder + <details> trace
    // dominated); without this clearing, the prose stays hidden
    // forever — the LLM might stream a perfect answer (audible via
    // TTS) but the user never sees it. The <details> wrapper is
    // now the visible tool indicator; we don't need to also hide
    // the prose.
    if (assistantEl?.classList?.contains("chat-message--tool-pending")) {
      setAssistantToolPending(assistantEl, false);
    }
    // Drop the streaming class so the per-bubble replay button
    // (hidden via CSS while the class is present) becomes visible
    // now that the reply is fully rendered. The user only sees the
    // 🔊 icon once the assistant turn is complete, never mid-stream.
    if (assistantEl) {
      assistantEl.classList.remove("chat-message--streaming");
      // Collapse every tool trace inside the bubble so the default
      // state matches the "retracted after the reply settles" rule.
      // The user can still expand a trace by clicking the `<summary>`.
      const traces = assistantEl.querySelectorAll(
        "details.chat-message__tool-usage[open]",
      );
      traces.forEach((d) => { d.open = false; });
    }
    // If a `get_weather` tool result came back successfully during
    // this reply, run the assistant finalizer (collapse long prose
    // down to a one-liner under the card; keep short
    // acknowledgments visible). Done here, not in
    // resolveToolBubble, so we never judge the assistant's prose while
    // the LLM is still streaming tokens after the tool result.
    if (inflight?.weatherFinalizeEl && assistantEl) {
      try {
        finalizeAssistantForToolResult(
          inflight.weatherFinalizeEl,
          assistantEl,
        );
      } catch (e) {
        console.warn("weather finalizer failed:", e);
      }
    }
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
//
// `formEl`, `stopBtn`, etc. are `lazyEl` Proxies — every access
// (read OR write) re-queries the DOM. Pre-mount accesses no-op
// (write to null returns true; read returns undefined), so the
// crash that surfaced with the inline `<script>` block change
// (which moved `index.html` re-emission into the `app-shell-mount`
// path) is gone. auth.js dispatches `app-shell-mounted` after
// cloning the template into `#app-root`; every access after
// that event sees the live element and the wire-up happens
// automatically.

let _formWired = false;
function wireFormOnce() {
  if (_formWired) return;
  const form = document.getElementById("chat-form");
  if (!form) return;
  _formWired = true;
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    sendTyped();
  });
  const stopBtnEl = document.getElementById("chat-stop");
  const clearBtnEl = document.getElementById("chat-clear");
  const inputElEl = document.getElementById("chat-input");
  if (stopBtnEl) stopBtnEl.addEventListener("click", () => stop());
  if (clearBtnEl) clearBtnEl.addEventListener("click", () => clearChat());
  if (inputElEl) {
    inputElEl.addEventListener("keydown", (e) => {
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
  }
}
window.addEventListener("app-shell-mounted", wireFormOnce);
// In the unlikely event the shell is already mounted (a cached
// page reload with `appRoot.firstChild` already populated),
// wire up immediately.
wireFormOnce();

// Post-mount rehydrate: every render that touched DOM elements
// pre-mount was a no-op (the `lazyEl` Proxy swallows null-element
// writes). After the shell mounts, re-run the relevant renderers
// so the UI actually reflects the persisted state.
//
// `currentSessionId` was set during the pre-mount boot via
// `activeSessionId()` (which falls back to the most-recent
// session in localStorage or mints a fresh one). Re-rendering
// here means the user immediately sees the session sidebar +
// chat history on first load, without waiting for a tab click.
// Server-side feature flags. The endpoint is fetched once per
// page load (after `/api/me` succeeds) and the result lives on
// `window.__nagentFeatures`. UI modules read `feature(name)`
// before rendering — every feature defaults to `false` so a
// missing endpoint / a 401 / a network failure hides the
// corresponding UI rather than failing open. The cache is
// rebuilt on every page load (no `localStorage`) so an operator
// who flips a config flag and rebuilds the server sees the new
// state on the next refresh.
const DEFAULT_FEATURES = Object.freeze({
  documents: false,
  llm: false,
  // `llm_models` is the list the server returned in its
  // `/api/features` response — populated only when `llm: true`.
  // Default is `[]` so a missing / failed feature fetch leaves the
  // dropdown empty (the loadModels fallback then renders the
  // "(no models)" placeholder).
  llm_models: Object.freeze([]),
  tts: false,
  agents: false,
  agent_names: Object.freeze([]),
  chat_sessions: false,
  tools: Object.freeze([]),
});
let nagentFeatures = { ...DEFAULT_FEATURES };
const featureListeners = new Set();
function feature(name) {
  return Boolean(nagentFeatures[name]);
}
function subscribeFeatures(listener) {
  featureListeners.add(listener);
  return () => featureListeners.delete(listener);
}
function emitFeatures() {
  for (const l of featureListeners) {
    try { l(nagentFeatures); } catch (e) { console.warn("feature listener threw", e); }
  }
}
async function refreshFeatures() {
  try {
    const resp = await fetch("/api/features", {
      credentials: "same-origin",
      cache: "no-store",
      headers: { Accept: "application/json" },
    });
    if (!resp.ok) {
      // 401 (anonymous), 404 (auth disabled), or 5xx — keep
      // defaults (every UI section hidden). The user sees a
      // minimal but functional shell.
      nagentFeatures = { ...DEFAULT_FEATURES };
      emitFeatures();
      return;
    }
    const body = await resp.json();
    // Merge defensively: server may be ahead of client (new
    // field added) — fall back to the default for missing keys.
    nagentFeatures = { ...DEFAULT_FEATURES, ...body };
    emitFeatures();
  } catch (e) {
    // Network failure: keep defaults. The UI is degraded
    // (panels hidden) but the page still loads.
    console.warn("features: /api/features fetch failed", e);
    nagentFeatures = { ...DEFAULT_FEATURES };
    emitFeatures();
  }
}
// Back-compat alias for `readNagentConfig()` — the documents
// module used to read `#nagent-config` from the HTML; that
// block is gone now and the panel reads `feature("documents")`.
export function readNagentConfig() {
  return { documentsEnabled: feature("documents") };
}

// Expose the feature registry on `window.__nagentFeatures` so
// the `documents.js` module (which lives in a separate file
// and avoids the chat.js ↔ documents.js cyclic import) can read
// it without going through `import`. Every reader also subscribes
// via `subscribeFeatures` so a future feature toggle (e.g.
// after a settings panel mutation) propagates without a page
// reload.
window.__nagentFeatures = nagentFeatures;
window.__nagentFeature = feature;
window.__nagentSubscribeFeatures = subscribeFeatures;

function rehydrateAfterMount() {
  // Make sure `currentSessionId` is bound before we render —
  // `activeSessionId()` is idempotent (no-op when the persisted
  // id is current).
  const sid = activeSessionId();
  renderSessionList();
  renderHistory(sid);
  // Documents panel: `initDocumentsPanel` already wired the
  // listeners; refresh now that the shell is mounted.
  Documents.setSessionId(sid);
  // Subscribe to feature-flag updates BEFORE the fetch fires so we
  // do not race against the response: the first `emitFeatures()`
  // inside `refreshFeatures` already calls our subscriber, which
  // calls `loadModels()` synchronously and paints the dropdown as
  // soon as `/api/features` resolves. The model list now lives on
  // `feature("llm_models")` (server fetches the upstream `/v1/models`
  // when the request comes in and bakes the list into the response),
  // so the chat UI does not need a separate `/v1/models` round-trip.
  subscribeFeatures(loadModels);
  // Refresh feature flags once the shell is mounted so the
  // UI can hide disabled sections. The fetch is async; the
  // renderers that gated themselves on `feature(name)` will
  // re-render via the subscriber callback.
  refreshFeatures();
  // Hide UI controls that are not applicable to the current
  // server build. We subscribe to feature changes so a future
  // toggle re-evaluates without a page reload.
  applyFeatureGates();
}

function applyFeatureGates() {
  // Hide the Discussion-mode tab when the LLM proxy is not
  // wired. The mode toggle then collapses to a single Transcript
  // button; the `modechange` listener (registered below) never
  // sees a `discussion` switch in that case, so the rest of
  // the chat code doesn't need a parallel check.
  const discussionBtn = document.getElementById("mode-discussion-btn");
  if (discussionBtn) {
    discussionBtn.hidden = !feature("llm");
  }
  // Future: gate the TTS settings drawer on `feature("tts")`,
  // etc. Each addition is one branch — the loop over a small
  // map keeps the boot tidy.
}

// Subscribe ONCE at module load so every `emitFeatures` triggers
// exactly one re-apply. Subscribing inside `applyFeatureGates`
// (the earlier iteration) caused an exponential listener
// explosion: each emit fired the listener, which subscribed a
// new listener, which fired on the next emit, doubling the
// count — the page froze within seconds of `/api/features`
// returning. The one-shot `subscribeFeatures` call below
// matches the pattern used by `documents.js` and keeps the
// listener set bounded at one entry per consumer.
subscribeFeatures(applyFeatureGates);
window.addEventListener("app-shell-mounted", rehydrateAfterMount);
// Eager rehydrate when the shell is already mounted (a cached
// page reload).
rehydrateAfterMount();

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
  // `audioCapture` is created on `app-shell-mounted`; null pre-mount.
  audioCapture?.toggle();
});

// ---- Audio capture (Discussion mode) ---------------------------------------
//
// `AudioCapture` accesses DOM elements (`buttonEl`,
// `canvasEl`, …) in its constructor. Those elements live inside
// `<template id="app-shell-template">` so they're null at module
// load time — instantiating `AudioCapture` at the top of chat.js
// would crash on `cfg.buttonEl.addEventListener(...)`. Defer the
// construction to `app-shell-mounted` (dispatched by `auth.js`
// after the template is cloned into `#app-root`) so every
// element handle resolves.
//
// `audioCapture` is a `let` (not `const`) and starts as `null`;
// the keyboard shortcut below no-ops when it's null. After mount,
// the global Ctrl/Cmd+Shift+D listener delegates to
// `audioCapture.toggle()` which now exists.
let audioCapture = null;
function initAudioCaptureOnce() {
  if (audioCapture) return;
  const buttonEl = document.getElementById("chat-record-btn");
  if (!buttonEl) return; // shell not mounted yet
  audioCapture = new AudioCapture({
    buttonEl,
    statusEl: null, // merged into #chat-status via the onStatusChange callback
    // Discussion-mode instance of the shared voice oscilloscope widget.
    // The DOM element lives inline as a voice bubble inside
    // #chat-messages (see §4.10 of docs/ui_features.md), distinct from
    // the Transcript-mode instance mounted at the top of the transcript
    // view. Each `AudioCapture` owns its own canvas/level; the only
    // shared resource is the underlying MicVAD singleton.
    canvasEl: document.getElementById("voice-graph-discussion-canvas"),
    levelEl:  document.getElementById("voice-graph-discussion-level"),
    graphEl:  document.getElementById("voice-graph-discussion"),
    containerEl: document.getElementById("view-discussion"),
    langSelectEl: document.getElementById("chat-lang-select"),
    translateCheckEl: document.getElementById("chat-translate-check"),
    backendInfoEl: document.getElementById("chat-backend-info"),
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
}
window.addEventListener("app-shell-mounted", initAudioCaptureOnce);
// Best-effort eager init: if the shell was already mounted (a
// cached page reload), wire it up without waiting for the event.
initAudioCaptureOnce();

// ---- Text-to-Speech (Piper, local) -----------------------------------------
//
// Settings are persisted to `localStorage` (with try/catch wrapping to
// tolerate private-mode browsers) so the user's voice + speed choice
// survives a page reload. Defaults are conservative: TTS off, neutral
// speed, "stop on new message" on (matches typical chat UX where each
// user turn is a fresh interaction).

// localStorage helpers — try/catch wrapping because Safari private
// mode throws on `setItem`. We never throw from these helpers; failed
// writes silently fall back to in-memory state for the session.
function lsGet(key) {
  try { return localStorage.getItem(key); } catch (_) { return null; }
}
function lsSet(key, value) {
  try { localStorage.setItem(key, value); } catch (_) { /* ignored */ }
}
function lsGetBool(key, fallback) {
  const v = lsGet(key);
  if (v === null) return fallback;
  return v === "1" || v === "true";
}
function lsGetNum(key, fallback) {
  const v = lsGet(key);
  if (v === null || v === "") return fallback;
  const n = parseFloat(v);
  return Number.isFinite(n) ? n : fallback;
}

/**
 * Strip markdown syntax from `text` so espeak-ng doesn't phonemise
 * characters that are meaningful in markdown but meaningless to a
 * TTS engine:
 *
 *   `*`, `**`, `_`, `__`, `` ` ``, `#`, `>`, `[]()`, `![]()`, etc.
 *
 * espeak-ng's tokenizer falls back to spelling out isolated
 * punctuation marks when it can't classify them as a known symbol,
 * so `**bold**` becomes "astérisque astérisque bold astérisque
 * astérisque" -- annoying and breaks the prose rhythm.
 *
 * The sanitisation is regex-based (not a full markdown parser) and
 * applied at DELTA granularity, so a multi-delta construct like
 * `**bo` + `ld**` may briefly match an extra `*` mid-stream. In
 * practice LLM tokens are short enough that this rarely happens,
 * and a stray single `*` is far less audible than the double form.
 *
 * The visible bubble still renders the original markdown via
 * `marked.parse` + `DOMPurify` -- only the TTS path gets the
 * plain-text variant.
 */
function sanitizeForTts(text) {
  if (!text) return "";
  return text
    // Fenced code blocks: drop entirely. Piper would otherwise try
    // to read identifiers and punctuation from code, which sounds
    // awful. The markdown renderer keeps the original block for
    // visual users.
    .replace(/```[\s\S]*?```/g, " ")
    // Inline code: keep content, drop backticks.
    .replace(/`+([^`]+?)`+/g, "$1")
    // Images: keep alt text, drop `![]()` boilerplate.
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1")
    // Links: keep visible text, drop URL.
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    // Reference-style short links (`[text][ref]`).
    .replace(/\[([^\]]+)\]\[[^\]]*\]/g, "$1")
    // Reference-style link definitions (`[1]: https://...`).
    .replace(/^\s{0,3}\[[^\]]+\]:\s+\S+.*$/gm, "")
    // Bold then italic (longer match first so `**` wins over `*`).
    .replace(/\*\*([^*\n]+?)\*\*/g, "$1")
    .replace(/__([^_\n]+?)__/g, "$1")
    .replace(/(^|[^*])\*([^*\n]+?)\*(?!\*)/g, "$1$2")
    .replace(/(^|\s)_([^_\n]+?)_(?!\w)/g, "$1$2")
    // Strikethrough.
    .replace(/~~([^~\n]+?)~~/g, "$1")
    // Orphan asterisks (NOT between digits): `A single * is here.`
    // and stray bullet markers mid-line. We explicitly keep `*`
    // between digits so arithmetic like `2*3=6` reads as
    // "deux multiplié trois égalent six" rather than collapsing to
    // "23=6". Piper's espeak-ng reads a bare `*` as "astérisque",
    // which is the whole point of this rule.
    .replace(/([^\d\s]|^)\*(?=[^\d\s]|$)/g, "$1")
    .replace(/(?<=[^\d\s]|^)\*(?=[^\d\s\s]|$)/g, "")
    // Heading markers at line start.
    .replace(/^\s{0,3}#{1,6}\s+/gm, "")
    // Blockquote markers at line start (only when at start; leave
    // comparison `>` mid-sentence alone so espeak-ng can read
    // "x supérieur 5" rather than collapsing to "x5").
    .replace(/^\s{0,3}>\s?/gm, "")
    // Unordered list markers at line start (`-`, `*`, `+`). The
    // hyphen case is tricky: a real em-dash `—` is wider, so this
    // matches ASCII `-` only and leaves prose hyphens alone.
    .replace(/^\s{0,3}[-*+]\s+/gm, "")
    // Ordered list markers (`1.`, `2.`, ...) at line start.
    .replace(/^\s{0,3}\d+\.\s+/gm, "")
    // Horizontal rules (3+ dashes/asterisks/underscores alone).
    .replace(/^\s{0,3}[-*_]{3,}\s*$/gm, "")
    // HTML-ish tags.
    .replace(/<\/?[a-zA-Z][^>]*>/g, "")
    // Stray HTML entities (`&`, `<`, `>`, `&#NNN;`).
    // Keep named entities that may carry semantics the user typed
    // literally (e.g. `&` in prose) but strip the common ones
    // espeak-ng would otherwise read as "et commercial", "inférieur
    // strict", etc.
    .replace(/&(?:amp|lt|gt|quot|apos|nbsp);/g, " ")
    // Trailing whitespace per line (keeps sentence boundaries clean).
    .replace(/[ \t]+$/gm, "");
}

const TTS_LS_ENABLED  = "nagent.chat.ttsEnabled";
const TTS_LS_VOICE_EN = "nagent.chat.tts.voiceEn";
const TTS_LS_VOICE_FR = "nagent.chat.tts.voiceFr";
const TTS_LS_SPEED    = "nagent.chat.tts.speed";
const TTS_LS_AUTOPLAY = "nagent.chat.tts.autoplay";
const TTS_LS_STOPSEND = "nagent.chat.tts.stopOnSend";

/**
 * Resolve the current TTS settings from the DOM controls (which are
 * the source of truth at runtime — `localStorage` only seeds them on
 * boot). Called by `streamReply` once per turn so a user changing the
 * speed mid-conversation takes effect on the very next request.
 */
function getTtsSettings() {
  return {
    enabled: !!$("chat-tts-check")?.checked,
    voiceEn: $("chat-tts-voice-en")?.value || "",
    voiceFr: $("chat-tts-voice-fr")?.value || "",
    // Speed slider: 0.5…1.5x where smaller = faster. Piper's
    // `length_scale` is inverted: >1 = slower. We send the slider
    // value directly (the server applies the mapping). 1.0x is the
    // neutral value.
    speed: parseFloat($("chat-tts-speed")?.value || "1.0") || 1.0,
    autoplay: !!$("chat-tts-autoplay")?.checked,
    stopOnSend: !!$("chat-tts-stop-on-send")?.checked,
  };
}

/**
 * Voice resolver used by `tts.js`. Reads the per-user *reply*
 * language preference (NOT the STT input language picker — the
 * two are decoupled so a user transcribing in one language but
 * asking for replies in another gets replies spoken in the right
 * voice). Returns the corresponding voice id from the saved
 * settings. Defaults to English when the hint is empty / unknown
 * — mirrors `TtsEngine::default_voice_for` server-side.
 */
function resolveTtsVoice() {
  const s = getTtsSettings();
  const lang = (getReplyLanguage() || "").toLowerCase();
  if (lang === "fr" || lang.startsWith("fr-")) return s.voiceFr;
  return s.voiceEn;
}

/**
 * Show / hide the per-bubble replay button. The button is visible
 * on every assistant bubble when the server reports TTS support
 * (`probeTtsVoices` sets `window.__ttsAvailable = true` on success).
 * The master `#chat-tts-check` is independent — clicking the button
 * toggles the master on if needed, so a first-time user can hear
 * replies without having to find the Advanced drawer first.
 *
 * When the probe hasn't completed yet (still in flight at boot),
 * `__ttsAvailable` is undefined and we default to showing the
 * button optimistically; if the probe fails, the click handler's
 * 503 / network error surfaces a toast to the user.
 */
function refreshReplayButtonVisibility() {
  const available = window.__ttsAvailable !== false;
  document.querySelectorAll(".chat-message-replay").forEach((btn) => {
    btn.hidden = !available;
  });
}

/**
 * Replay a single assistant message aloud. Unlike the streaming
 * autoplay (`tts.feed` per SSE delta), this sends the FULL
 * sanitised text in one HTTP round-trip, which is what the user
 * asked for: one chunk per click, no per-sentence latency.
 *
 * Click semantics:
 *   - Idle         -> speak full text
 *   - Playing this -> stop playback
 *   - Playing other-> stop the other, speak this one
 *
 * Visual state is tracked via a `playing` class on the button
 * (icon swap is in CSS) and a global `currentReplayButton`
 * pointer so we can clear the icon when the audio ends.
 */
let currentReplayButton = null;
async function replayMessage(div, btn) {
  // Implicit enable: if the master `#chat-tts-check` is off (the
  // user found this button on a bubble they didn't know was
  // gated), flip it on so the click actually does something. The
  // change handler refreshes button visibility (no-op under the new
  // always-visible behaviour) and persists the new state.
  const check = $("chat-tts-check");
  if (check && !check.checked) {
    check.checked = true;
    check.dispatchEvent(new Event("change", { bubbles: true }));
  }
  // Build the plain-text version from the bubble's textContent so
  // we don't ship innerHTML to the TTS server. We deliberately do
  // NOT strip markdown here -- the button reads the bubble after
  // sanitisation, but since DOMPurify already threw away scripts,
  // textContent is safe.
  const raw = (div.textContent || "").trim();
  if (!raw) return;
  // Strip markdown markers a second time so image alt text and
  // bullet markers rendered by `marked.parse` don't get read
  // either (e.g. a `<ul>` becomes empty lines that the TTS buffer
  // would otherwise split on).
  const text = sanitizeForTts(raw);
  if (!text.trim()) return;

  // Toggle: if this same button is already playing, stop.
  if (btn.classList.contains("chat-message-replay--playing")) {
    _ttsPlayer?.stopAll();
    return;
  }

  // Clear any other playing button's state.
  if (currentReplayButton && currentReplayButton !== btn) {
    currentReplayButton.classList.remove("chat-message-replay--playing");
  }
  // Cancel whatever the autoplay or another replay was doing.
  _ttsPlayer?.stopAll();
  // Now wait one microtask so the previous fetch's AbortController
  // settles, then start the new playback.
  await new Promise((r) => setTimeout(r, 0));
  _ttsPlayer = getOrCreateTtsPlayer();
  btn.classList.add("chat-message-replay--playing");
  currentReplayButton = btn;
  // Use the single-shot `speak` path: one fetch, one decode, one
  // play. We do NOT chain into the autoplay feed/flush -- this is
  // an isolated, user-triggered utterance.
  try {
    await _ttsPlayer.speak(text, {
      voice: resolveTtsVoice(),
      speed: getTtsSettings().speed,
      onEnd: () => {
        if (currentReplayButton === btn) {
          btn.classList.remove("chat-message-replay--playing");
          currentReplayButton = null;
        }
      },
    });
  } catch (e) {
    btn.classList.remove("chat-message-replay--playing");
    if (currentReplayButton === btn) currentReplayButton = null;
    appendError(`TTS replay failed: ${e?.message || e}`);
  }
}

// Wire DOM events on the TTS controls. Done once at boot; the values
// are persisted on every change so a reload restores them.
function wireTtsControls() {
  const check  = $("chat-tts-check");
  const settings = $("chat-tts-settings");
  const voiceEnEl = $("chat-tts-voice-en");
  const voiceFrEl = $("chat-tts-voice-fr");
  const speedEl   = $("chat-tts-speed");
  const speedDisp = $("chat-tts-speed-display");
  const autoEl    = $("chat-tts-autoplay");
  const stopEl    = $("chat-tts-stop-on-send");
  const testBtn   = $("chat-tts-test");
  // `check` may be absent if the server didn't render the controls
  // (TTS disabled, or older HTML). `settings` may be absent if the
  // HTML still uses the pre-refactor `<details>` wrapper. Either
  // being missing is fatal for TTS wiring -- bail out cleanly so the
  // rest of the discussion view still works.
  if (!check) return;
  if (!settings) return;

  // Seed from localStorage. We always read the saved value first so
  // the UI is immediately consistent with the user's last choice.
  check.checked = lsGetBool(TTS_LS_ENABLED, false);
  // Default voices mirror the server defaults so the selectors are
  // meaningful even before /v1/audio/voices responds.
  if (voiceEnEl && !voiceEnEl.value) voiceEnEl.value = lsGet(TTS_LS_VOICE_EN) || "en_US-lessac-medium";
  if (voiceFrEl && !voiceFrEl.value) voiceFrEl.value = lsGet(TTS_LS_VOICE_FR) || "fr_FR-upmc-medium";
  if (speedEl) {
    speedEl.value = String(lsGetNum(TTS_LS_SPEED, 1.0));
    if (speedDisp) speedDisp.textContent = `${parseFloat(speedEl.value).toFixed(2)}x`;
  }
  if (autoEl) autoEl.checked = lsGetBool(TTS_LS_AUTOPLAY, true);
  if (stopEl) stopEl.checked = lsGetBool(TTS_LS_STOPSEND, true);

  // Show / hide the TTS settings block (folded into the main
  // `.chat-advanced` drawer) based on the master toggle. The block
  // is hidden by default; we toggle the `[hidden]` attribute here.
  function syncAdvancedVisibility() {
    settings.hidden = !check.checked;
  }
  syncAdvancedVisibility();

  check.addEventListener("change", () => {
    lsSet(TTS_LS_ENABLED, check.checked ? "1" : "0");
    syncAdvancedVisibility();
    // Reveal/hide the per-message replay button on every existing
    // assistant bubble. New bubbles are wired in `appendMessage`
    // and inherit the current state automatically.
    refreshReplayButtonVisibility();
    // First time the user enables TTS we lazy-create the
    // AudioContext so the browser autoplay policy is satisfied.
    if (check.checked) ensureTtsAudioContext();
  });
  voiceEnEl?.addEventListener("change", () => lsSet(TTS_LS_VOICE_EN, voiceEnEl.value));
  voiceFrEl?.addEventListener("change", () => lsSet(TTS_LS_VOICE_FR, voiceFrEl.value));
  speedEl?.addEventListener("input", () => {
    if (speedDisp) speedDisp.textContent = `${parseFloat(speedEl.value).toFixed(2)}x`;
    lsSet(TTS_LS_SPEED, speedEl.value);
  });
  autoEl?.addEventListener("change", () => lsSet(TTS_LS_AUTOPLAY, autoEl.checked ? "1" : "0"));
  stopEl?.addEventListener("change", () => lsSet(TTS_LS_STOPSEND, stopEl.checked ? "1" : "0"));

  // Test button: synthesise a fixed phrase and play it. Uses the
  // same `tts` instance the LLM replies go through, so this is a
  // true end-to-end check (voice, network, decoding, playback).
  testBtn?.addEventListener("click", async () => {
    ensureTtsAudioContext();
    try {
      const tts = getOrCreateTtsPlayer();
      const resp = await fetch("/v1/audio/speech", {
        method: "POST",
        headers: { "Content-Type": "application/json", ...window.nagentAuth?.csrfHeaders() },
        body: JSON.stringify({
          input: "Hello, this is a voice test.",
          voice: resolveTtsVoice(),
          speed: getTtsSettings().speed,
        }),
      });
      if (!resp.ok) {
        appendError(`TTS test failed (HTTP ${resp.status})`);
        return;
      }
      const buf = await resp.arrayBuffer();
      const audioBuf = await tts.decodeExternal(buf);
      // Stop whatever else might be playing so the test phrase is
      // immediately audible.
      tts.stopAll();
      tts.schedule(audioBuf);
    } catch (e) {
      appendError(`TTS test failed: ${e?.message || e}`);
    }
  });
}

// Lazily create the AudioContext. The first call must be triggered
// by a user gesture handler (toggle / Test button) so the browser
// autoplay policy is satisfied. The returned context is reused for
// every subsequent `feed()` call in the same session.
let _ttsAudioCtx = null;
function ensureTtsAudioContext() {
  if (_ttsAudioCtx) return _ttsAudioCtx;
  if (typeof AudioContext === "undefined") {
    console.warn("Web Audio API not available; TTS will be silent.");
    return null;
  }
  try {
    _ttsAudioCtx = new AudioContext();
  } catch (e) {
    console.warn("AudioContext init failed:", e);
    return null;
  }
  return _ttsAudioCtx;
}

// Singleton TtsPlayer. We reuse it across turns so the Web Audio
// queue and the `AudioContext` are stable; otherwise every new reply
// would incur the cost of a fresh context.
let _ttsPlayer = null;
function getOrCreateTtsPlayer() {
  if (_ttsPlayer) return _ttsPlayer;
  _ttsPlayer = NagentTts.create({
    endpoint: "/v1/audio/speech",
    voiceResolver: resolveTtsVoice,
    // `speed` is read from settings on every `feed()` call via the
    // closure here — we don't snapshot it at construction time so
    // mid-conversation slider tweaks take effect immediately.
    speed: () => getTtsSettings().speed,
  });
  return _ttsPlayer;
}

/**
 * Probe `GET /v1/audio/voices`. On 200, populate the two voice
 * selectors and reveal the `#chat-tts-label` checkbox. On 503 or any
 * error, leave the controls hidden — the server has TTS disabled
 * (or has no voices installed) so the user has nothing to enable.
 */
async function probeTtsVoices() {
  try {
    const resp = await fetch("/v1/audio/voices");
    if (!resp.ok) return; // server has no TTS or no voices — leave hidden
    // Server has TTS: surface the per-message replay button on every
    // existing and future assistant bubble.
    window.__ttsAvailable = true;
    refreshReplayButtonVisibility();
    const body = await resp.json();
    const label = $("chat-tts-label");
    if (label) label.hidden = false;
    const voiceEnEl = $("chat-tts-voice-en");
    const voiceFrEl = $("chat-tts-voice-fr");
    if (voiceEnEl) {
      voiceEnEl.innerHTML = "";
      for (const v of body.voices || []) {
        const opt = document.createElement("option");
        opt.value = v.id;
        opt.textContent = `${v.id} (${v.language || "?"}, ${v.sample_rate} Hz)`;
        voiceEnEl.appendChild(opt);
      }
      // Restore the saved voice if it still exists; otherwise fall
      // back to the server-suggested default.
      const saved = lsGet(TTS_LS_VOICE_EN);
      const hasSaved = saved && Array.from(voiceEnEl.options).some((o) => o.value === saved);
      voiceEnEl.value = hasSaved ? saved : (body.default_voice_en || voiceEnEl.value);
    }
    if (voiceFrEl) {
      voiceFrEl.innerHTML = "";
      for (const v of body.voices || []) {
        const opt = document.createElement("option");
        opt.value = v.id;
        opt.textContent = `${v.id} (${v.language || "?"}, ${v.sample_rate} Hz)`;
        voiceFrEl.appendChild(opt);
      }
      const saved = lsGet(TTS_LS_VOICE_FR);
      const hasSaved = saved && Array.from(voiceFrEl.options).some((o) => o.value === saved);
      voiceFrEl.value = hasSaved ? saved : (body.default_voice_fr || voiceFrEl.value);
    }
  } catch (_) {
    // Network error / offline — leave controls hidden.
  }
}

// Boot the TTS UI: probe the server, wire DOM, restore persisted
// settings. Runs once after the AudioCapture is constructed.
probeTtsVoices();
wireTtsControls();

// Locale-aware defaults: preselect the discussion-mode STT input
// language from the browser locale if it matches one of the
// options; otherwise keep "Auto-detect" (empty value). The
// reply-language picker gets the same treatment inside
// `wireLocationControlsOnce` so the server-side state still wins
// after the preferences fetch resolves.
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
  // Refresh the documents panel so the list matches the now-active
  // session. `setSessionId` updates the header the upload / list /
  // delete endpoints use; `refresh()` re-fetches the rows.
  Documents.setSessionId(id);
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
  // New session = empty document list. `setSessionId` triggers
  // a refresh in `documents.js`.
  Documents.setSessionId(session.id);
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

// `#chat-new-session` lives inside `<template id="app-shell-template">`,
// so a module-top-level `document.getElementById` returns `null` and
// a `.addEventListener` would silently no-op — that's the bug behind
// the inert "New chat" button. Same pattern as `wireFormOnce` /
// `wireLocationControlsOnce`: wire inside an `app-shell-mounted`
// handler, with an idempotency guard so a duplicate dispatch from
// `auth.js` does not stack two click handlers on the same button.

let _chatSidebarWired = false;
function wireChatSidebarOnce() {
  if (_chatSidebarWired) return;
  const btn = document.getElementById("chat-new-session");
  if (!btn) return;
  _chatSidebarWired = true;
  btn.addEventListener("click", newSession);
}
window.addEventListener("app-shell-mounted", wireChatSidebarOnce);
// Cached page reload with the shell already in the DOM: wire up
// immediately (the module-top-level code runs before `auth.js` has a
// chance to dispatch the event in that case).
wireChatSidebarOnce();

// ---- Wire up the geolocation / timezone controls --------------------------
//
// The chat UI lives inside `<template id="app-shell-template">` and
// only enters the DOM after `auth.js` mounts the shell. Wiring at
// module top-level (the form pattern used to live there too) would
// see `null` for every element here — same trap that `wireFormOnce`
// already works around. We mirror that pattern: re-look the elements
// at wire-time (when the shell is in the DOM) and attach the
// handlers, then trigger the initial paint + boot-time position
// refresh so a cached fix is reflected without a tab click.
//
// The `_wired` guard makes the listener idempotent so a duplicate
// `app-shell-mounted` (e.g. if `auth.js` ever dispatches twice) does
// not stack two `click` listeners on the same button.

/**
 * Reply-language picker change. Persists through the server-side
 * preferences row (the localStorage mirror is updated by
 * `savePreferencesToServer` so a slow PUT never blocks the picker
 * paint). Mirrors the timezone handler — the PUT body always
 * carries the *current* location/timezone/additional_instructions/
 * temperature flags alongside the new language so the row stays in
 * lockstep (see `auth::routes::PutPreferencesBody`).
 */
function handleReplyLanguageChange() {
  const flags = acquireCurrentPreferenceFlags();
  const next = $("chat-reply-language")?.value || "";
  savePreferencesToServer(
    flags.location,
    flags.timezone,
    next || null,
    flags.additional_instructions,
    flags.temperature,
  );
}

/**
 * Settings-tab "Additional instructions" textarea input handler.
 * Persists the new value to `user_preferences.additional_instructions`
 * (debounced by the browser's input-event cadence, which is already
 * friendly for slow typers) and mirrors it to localStorage via
 * `savePreferencesToServer`. Sends the *full* quintuple so the
 * server-side atomic-replace contract is preserved.
 */
function handleSystemInput() {
  const flags = acquireCurrentPreferenceFlags();
  const next = (systemEl?.value ?? "").trim();
  savePreferencesToServer(
    flags.location,
    flags.timezone,
    flags.reply_language,
    next || null,
    flags.temperature,
  );
}

/**
 * Settings-tab "Temperature" number input handler. Reads the parsed
 * `parseFloat` so we don't store the raw `"0.8000000000000001"`
 * rounding artefact the browser can produce after edits, and so
 * an empty / non-finite value clears the preference (the
 * `additional_instructions: null` analogue for the number type).
 */
function handleTemperatureInput() {
  const flags = acquireCurrentPreferenceFlags();
  const raw = tempEl?.value ?? "";
  const n = Number.parseFloat(raw);
  const next = (raw.trim() !== "" && Number.isFinite(n)) ? n : null;
  savePreferencesToServer(
    flags.location,
    flags.timezone,
    flags.reply_language,
    flags.additional_instructions,
    next,
  );
}

/**
 * Settings-tab "Reset to defaults" button. Restores the two LLM
 * inputs to their UI baseline (`""` for instructions, `0.8` for
 * temperature) and writes `null` on the wire so the server keeps
 * SQL `NULL` and the LLM proxy falls back to the upstream model
 * default sampling. Mirrors the destructive-action pattern used
 * elsewhere (Forget my location, Remove integration): explicit
 * user gesture, no surprise wipe.
 */
function handleLlmResetClick() {
  const { system, temperature } = preferenceDefaults();
  if (systemEl) systemEl.value = system;
  if (tempEl) tempEl.value = String(temperature);
  const flags = acquireCurrentPreferenceFlags();
  savePreferencesToServer(
    flags.location,
    flags.timezone,
    flags.reply_language,
    null,
    temperature,
  );
}

let _locationWired = false;
function wireLocationControlsOnce() {
  if (_locationWired) return;
  // Re-query here instead of trusting the module-level `lazyEl`
  // proxies: the listener may fire while the shell is still being
  // cloned (defence in depth — today auth.js dispatches the event
  // AFTER `appendChild`, but the contract is "elements are
  // mountable" rather than "elements exist synchronously").
  const shareBtn = document.getElementById("chat-location-share");
  if (!shareBtn) return;
  _locationWired = true;
  shareBtn.addEventListener("click", handleShareLocationClick);
  document.getElementById("chat-location-pill")
    ?.addEventListener("click", openAdvancedForLocation);
  document.getElementById("chat-location-toggle")
    ?.addEventListener("change", handleLocationToggleChange);
  document.getElementById("chat-location-refresh")
    ?.addEventListener("click", handleLocationRefreshClick);
  document.getElementById("chat-location-forget")
    ?.addEventListener("click", handleLocationForgetClick);
  document.getElementById("chat-timezone-toggle")
    ?.addEventListener("change", handleTimezoneToggleChange);
  document.getElementById("chat-reply-language")
    ?.addEventListener("change", handleReplyLanguageChange);
  // Settings-tab LLM inputs. Use the `input` event (not `change`) so
  // every keystroke persists — the server-side row is the source of
  // truth for cross-device sync, and the user expects the value
  // they typed to survive a reload even if they never blur the
  // textarea. The PUT body always carries the full quintuple
  // (`savePreferencesToServer` reads the cached flags), so this
  // does not regress the other four fields.
  document.getElementById("chat-system")
    ?.addEventListener("input", handleSystemInput);
  document.getElementById("chat-temperature")
    ?.addEventListener("input", handleTemperatureInput);
  document.getElementById("settings-llm-reset")
    ?.addEventListener("click", handleLlmResetClick);
  // Elements are now live — paint the cached state and kick off the
  // boot-time position refresh. Both are no-ops on a fresh visit (no
  // cached fix, toggle off).
  //
  // `loadPreferencesFromServer` runs in parallel: it fetches
  // `/api/me/preferences` and, on success, rewrites the
  // localStorage keys the render functions below read from. When
  // it resolves we re-render the toggles so the user sees the
  // server-side choice immediately, not the (possibly stale)
  // localStorage copy from a different device / browser. The
  // reply-language picker is synced the same way so a fresh tab on
  // a device that never visited this account picks up the
  // server-side choice instead of falling back to "Auto".
  loadPreferencesFromServer().then(() => {
    renderLocationUi();
    renderTimezoneUi();
    renderReplyLanguageUi();
    renderLlmSettingsUi();
  });
  // Reply-language picker bootstrap: seed from the localStorage
  // mirror first (synchronous, runs before the server fetch so
  // there is no "Auto" flash for a returning user on this device),
  // then let `preselectFromBrowser` set it from the browser locale
  // on a fresh visit. `preselectFromBrowser` is a no-op when the
  // picker already has a non-empty value, so the localStorage seed
  // wins on returning visits and the locale sniff wins on first
  // visits. The server fetch (which may differ from either) wins
  // when it resolves — see the `.then()` handler above.
  renderReplyLanguageUi();
  renderLlmSettingsUi();
  preselectFromBrowser($("chat-reply-language"));
  renderLocationUi();
  renderTimezoneUi();
  refreshLocationOnBoot();
}

/**
 * Paint the reply-language picker from the cached preference.
 * Called on boot and after every successful fetch; mirrors
 * `renderLocationUi` / `renderTimezoneUi`. The picker keeps its
 * locale-preselected default until the server fetch (or a user
 * click) overwrites it.
 */
function renderReplyLanguageUi() {
  const sel = $("chat-reply-language");
  if (!sel) return;
  const cached = getReplyLanguage();
  // Set the select only if the cached value matches an option —
  // an unknown / unsupported code is left untouched so the user
  // sees their last valid choice and a future code addition
  // re-uses it without a forced migration.
  if (cached && Array.from(sel.options).some((o) => o.value === cached)) {
    sel.value = cached;
  }
}

/// Hydrate the two Settings-tab LLM inputs from the cached
/// preference. Mirrors `renderReplyLanguageUi`'s contract: seed
/// from the localStorage mirror synchronously on boot so the user
/// never sees an empty textarea / a 0.8 default after they've
/// already typed something, then let the boot-time
/// `loadPreferencesFromServer()` fetch overwrite the values from
/// the server-side row (the source of truth for cross-device
/// sync). The request-build path (`streamReply` at lines
/// 2538–2540 / 2563–2564) reads `systemEl.value` and
/// `tempEl.value` synchronously on every turn, so a missing
/// hydration would silently drop the user's instructions and
/// reset temperature to the `<input>` HTML default.
function renderLlmSettingsUi() {
  if (systemEl) {
    const sys = acquireSystemPreference();
    // Only overwrite the textarea if it is still at its HTML
    // default (empty), so a hydration race never wipes a value
    // the user just typed between `renderLlmSettingsUi` being
    // scheduled and the server fetch resolving.
    if (sys && !systemEl.value) systemEl.value = sys;
  }
  if (tempEl) {
    const temp = acquireTemperaturePreference();
    if (temp !== null && !tempEl.value) tempEl.value = String(temp);
  }
}

window.addEventListener("app-shell-mounted", wireLocationControlsOnce);
// Cached page reload with the shell already in the DOM:
wireLocationControlsOnce();

// ---- Boot ------------------------------------------------------------------

// One-time upgrade from the old single-history layout, then resolve
// the active session. `activeSessionId()` is self-healing — if there
// are zero sessions after migration, it creates one — so the rest of
// the boot path can assume a valid id.
migrateLegacy();
currentSessionId = getActiveId();
activeSessionId(); // validates / falls back / creates, updates sidebar
// `renderSessionList` / `renderHistory` (sidebar + chat bubbles) and
// the geolocation / timezone UI paints all touch DOM elements that
// live inside `<template id="app-shell-template">` and only enter
// `#app-root` once `auth.js` clones the template. Running them here
// would be a pre-mount no-op (and previously triggered a noisy
// `lazyEl` debug). The post-mount paint is owned by
// `rehydrateAfterMount` / `wireLocationControlsOnce`, both wired on
// the `app-shell-mounted` event below. Calling these at top level
// would render against `null` elements — that's the bug behind the
// inert "Refresh my location" / "New chat" buttons.

// Warm the agents banner once on page boot. The Discussion-mode
// model dropdown is warmed by `rehydrateAfterMount` via the
// `subscribeFeatures(loadModels)` subscriber — when `/api/features`
// resolves, `loadModels()` paints the `<select id="chat-model">`
// from `feature("llm_models")` (no separate `/v1/models` request
// from the browser).
loadAgentsBanner();
// Wire the Documents panel (sidebar upload + drag-drop + paste
// handlers). The module is a no-op when the server reports the
// documents feature off — see `documents.js:initDocumentsPanel`.
Documents.initDocumentsPanel();
Documents.setSessionId(currentSessionId);

document.addEventListener("visibilitychange", () => {
  // Re-fetch `/api/features` on tab return in case the server's
  // model list changed while the tab was backgrounded (e.g.
  // another `ollama pull` while the user was away). The
  // `subscribeFeatures(loadModels)` subscriber paints the dropdown
  // when the response resolves; this path is just a nudge to make
  // that happen. The agents banner still has its own fetch
  // (`/v1/agents` is not part of `/api/features`).
  if (!document.hidden) {
    refreshFeatures();
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
  // Switching INTO Discussion: the dropdown is already populated by
  // the `subscribeFeatures(loadModels)` subscriber wired in
  // `rehydrateAfterMount`. Switching modes does not change the model
  // list, so no work to do here — we only guard against the (rare)
  // case of a features fetch that has not yet resolved by the time
  // the user toggles modes.
  if (e?.detail?.mode === "discussion") {
    if (!modelsLoaded) loadModels();
    return;
  }
  if (inflight) inflight.controller.abort();
  resetTurnQueue();
});
