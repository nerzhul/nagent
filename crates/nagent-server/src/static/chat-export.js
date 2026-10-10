// nagent — Discussion-mode chat session export (A1, plan 1791464974103).
//
// Pure module: no DOM side effects, no global state. Imported by
// `chat.js`, called when the user picks a format from the
// `#chat-export-menu` dropdown. Each formatter returns a string;
// the caller wraps it in a `Blob` and triggers a download via
// `URL.createObjectURL` + a programmatic `<a download>` click —
// the same pattern the Transcript-mode exporter uses
// (`app.js::downloadTranscript`).
//
// The wire format of each exporter is stable across builds:
//   - Markdown: H1 title + H3 per-message section with role, ts
//     and content. Tool traces are folded into a single
//     `<details>` block per assistant message so the file renders
//     cleanly on every markdown viewer (no fenced code blocks for
//     non-CLI consumers).
//   - JSON: `{ id, title, exportedAt, messages }` — same shape
//     the LLM proxy consumes plus the session metadata so a
//     follow-up ingest tool can round-trip without losing
//     context.
//   - HTML: full `<!doctype html>` page. Each message rendered
//     through the same `marked` + `DOMPurify` pipeline the live
//     bubble uses (re-imported in the browser so a script
//     injection in a downloaded file is treated the same as a
//     live chat reply). The CSS is a single `<style>` block
//     vendored from the chat bubble's rules so an opened file
//     in a browser looks like the in-app bubble.

export const EXPORT_FORMATS = ["md", "json", "html"];

const ISO_TS_RE = /^\d{4}-\d{2}-\d{2}T/;

/**
 * Build a Markdown rendering of `history`. The title is the
 * session's `title` field (or the `id` when the title is the
 * default placeholder). Each message becomes a `### <role>
 * (<ts>)` heading followed by the content. Tool traces for an
 * assistant turn are folded into a single `<details>` block
 * under the assistant's section so a viewer that respects
 * `<details>` collapses them by default.
 */
export function formatMarkdown({ title, messages }) {
  const safeTitle = (title || "Chat export").trim() || "Chat export";
  const out = [`# ${safeTitle}`, ""];
  for (const msg of messages) {
    const role = msg.role || "unknown";
    const ts = formatTs(msg.ts);
    out.push(`### ${role} (${ts})`, "");
    if (role === "assistant" && Array.isArray(msg.tool_calls) && msg.tool_calls.length > 0) {
      // Render the prose first (often empty for a tool-only
      // turn, sometimes a "Let me check…" prefix from the
      // model), then the tool trace as a collapsible block.
      if (msg.content) {
        out.push(msg.content, "");
      }
      out.push("<details><summary>Tool calls</summary>", "");
      for (const tc of msg.tool_calls) {
        const name = tc.function?.name || "tool";
        const args = tc.function?.arguments || "";
        out.push(`- \`${name}\``);
        if (args) out.push(`  - args: \`${args}\``);
      }
      out.push("", "</details>", "");
      continue;
    }
    if (msg.content) {
      out.push(msg.content, "");
    }
  }
  return out.join("\n");
}

/**
 * JSON envelope: `{ id, title, exportedAt, messages }`. The
 * `messages` array is the same shape the LLM proxy consumes
 * (`{role, content}` for non-tool turns; the full `{role,
 * content, tool_calls, tool_call_id, ...}` for tool turns) so
 * a follow-up ingest can replay the conversation without
 * losing context.
 */
export function formatJson({ id, title, messages }) {
  return JSON.stringify({
    id: id || null,
    title: title || null,
    exportedAt: new Date().toISOString(),
    messages,
  }, null, 2);
}

/**
 * Standalone HTML page. Each message is rendered as a
 * `class="chat-message chat-<role>"` div with the same
 * `chat-message--markdown` / `chat-message--widget-only` rules
 * the live bubble uses (inlined via a single `<style>` block).
 * The `marked` + `DOMPurify` pipeline is re-imported in the
 * browser; when this module is loaded under Node (the
 * `verify-chat-export` test harness) the pipeline falls back to
 * a plain-text render.
 */
export function formatHtml({ title, messages }) {
  const safeTitle = escapeHtml((title || "Chat export").trim() || "Chat export");
  const body = messages.map((msg) => renderMessageHtml(msg)).join("\n");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>${safeTitle}</title>
<style>${EXPORT_HTML_CSS}</style>
</head>
<body>
<header>
  <h1>${safeTitle}</h1>
  <p class="export-meta">Exported ${escapeHtml(new Date().toISOString())}</p>
</header>
<main>
${body}
</main>
</body>
</html>
`;
}

function renderMessageHtml(msg) {
  const role = msg.role || "unknown";
  const ts = formatTs(msg.ts);
  const tsLine = `<p class="chat-message-meta">${escapeHtml(role)} · ${escapeHtml(ts)}</p>`;
  let body;
  if (role === "assistant" && Array.isArray(msg.tool_calls) && msg.tool_calls.length > 0) {
    const prose = msg.content ? sanitizeMarkdown(msg.content) : "";
    const tools = msg.tool_calls.map((tc) => {
      const name = escapeHtml(tc.function?.name || "tool");
      const args = escapeHtml(tc.function?.arguments || "");
      return `<li><code>${name}</code>${args ? ` — <code>${args}</code>` : ""}</li>`;
    }).join("");
    body = `${prose}<details><summary>Tool calls</summary><ul>${tools}</ul></details>`;
  } else {
    body = sanitizeMarkdown(msg.content || "");
  }
  return `<article class="chat-message chat-${escapeHtml(role)}">${tsLine}${body}</article>`;
}

/**
 * Best-effort sanitized render of `md` for the HTML export. We
 * use the same `marked` + `DOMPurify` pipeline the live bubble
 * uses when running in a browser; in Node (the verify harness)
 * we fall back to a plain-text render so a test can still
 * assert the `<script>` injection is escaped.
 */
function sanitizeMarkdown(md) {
  if (typeof window !== "undefined" && window.marked && window.DOMPurify) {
    const raw = window.marked.parse(md, { breaks: true, gfm: true });
    return window.DOMPurify.sanitize(raw, { ADD_ATTR: ["target", "rel"] });
  }
  return `<p>${escapeHtml(md).replace(/\n/g, "<br>")}</p>`;
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    "\"": "&quot;",
    "'": "&#39;",
  }[c]));
}

function formatTs(ts) {
  if (ts == null) return "unknown";
  // History records use `Date.now()` (millis since epoch).
  // Future records may carry an ISO string (the
  // server-mirror rows). Handle both.
  if (typeof ts === "number") return new Date(ts).toISOString();
  if (typeof ts === "string") {
    if (ISO_TS_RE.test(ts)) return ts;
    const n = Number(ts);
    if (Number.isFinite(n)) return new Date(n).toISOString();
    return ts;
  }
  return String(ts);
}

/**
 * Build a stable, filesystem-safe filename:
 * `<sanitised-title>-<yyyymmdd-hhmm>.<ext>`. The title is
 * squashed to `[a-z0-9-_]` and capped at 40 chars so a long
 * session name does not blow up the OS filename limit.
 */
export function filenameFor(title, ext, now = new Date()) {
  const safe = (title || "chat")
    .replace(/[^a-z0-9-_]+/gi, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 40) || "chat";
  const pad = (n) => String(n).padStart(2, "0");
  const stamp = `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}`;
  return `${safe}-${stamp}.${ext}`;
}

/**
 * Trigger a download of `content` as a `Blob` of the matching
 * MIME type. The same `URL.createObjectURL` + programmatic
 * `<a download>` click pattern the Transcript-mode exporter
 * uses (`app.js::downloadTranscript`).
 */
export function downloadBlob(content, mime, filename) {
  if (typeof document === "undefined") return; // Node test path
  const blob = new Blob([content], { type: `${mime};charset=utf-8` });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.rel = "noopener";
  // Append so Firefox honours the click in some quirks modes
  // (the click is a no-op on detached elements otherwise).
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  // Defer revoke so Safari has time to read the blob.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// ---- Inline CSS (mirrors the live bubble's style.css subset) -------------
//
// A single screenful of rules vendored from `style.css` so a
// downloaded file in a browser looks like the in-app bubble.
// Kept short on purpose: this is a static export, not a full
// app theme.
const EXPORT_HTML_CSS = `
:root {
  color-scheme: dark;
  --bg: #1a1a1a;
  --bg-elev: #232323;
  --fg: #e6e6e6;
  --fg-mute: #9aa0a6;
  --border: #3a3a3a;
  --accent: #4a90e2;
  --error: #d35a5a;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
}
body { background: var(--bg); color: var(--fg); margin: 0; padding: 0 0 4rem; }
header { padding: 1.5rem 2rem 1rem; border-bottom: 1px solid var(--border); }
header h1 { margin: 0; font-size: 1.4rem; }
.export-meta { color: var(--fg-mute); margin: 0.25rem 0 0; font-size: 0.85rem; }
main { max-width: 50rem; margin: 0 auto; padding: 1.5rem 1rem; }
.chat-message { background: var(--bg-elev); border: 1px solid var(--border); border-radius: 8px; padding: 0.85rem 1rem; margin: 0 0 0.75rem; }
.chat-message.chat-user { background: var(--bg-elev); }
.chat-message.chat-assistant { background: #1f2530; }
.chat-message-meta { color: var(--fg-mute); font-size: 0.78rem; margin: 0 0 0.4rem; text-transform: uppercase; letter-spacing: 0.04em; }
.chat-message pre { background: #0e0e0e; padding: 0.7rem; border-radius: 6px; overflow-x: auto; }
.chat-message code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.9em; }
.chat-message a { color: var(--accent); }
.chat-message details { margin-top: 0.5rem; }
.chat-message summary { cursor: pointer; color: var(--fg-mute); }
`;
