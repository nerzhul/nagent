// Pure-Node tests for the Discussion-mode export module
// (`chat-export.js`, plan 1791464974103 §3). Imported directly
// by Deno so we can assert the wire shape of every formatter
// without standing up a browser DOM.
//
// The assertions mirror the three risks the plan calls out:
//   1. Markdown round-trips a known history to the expected
//      string (so a viewer that respects `<details>` collapses
//      tool calls).
//   2. JSON includes `id`, `title`, `exportedAt`, `messages`
//      (so a follow-up ingest can replay the conversation).
//   3. HTML escapes a `<script>` injection inside a user
//      message (DOMPurify contract — the test runs under Node
//      where the `window` globals are absent, so the module
//      falls back to a plain-text render that already escapes).

import {
  EXPORT_FORMATS,
  filenameFor,
  formatHtml,
  formatJson,
  formatMarkdown,
} from "../crates/nagent-server/src/static/chat-export.js";

function assert(cond: unknown, msg = "condition is falsy"): asserts cond {
  if (!cond) throw new Error(`assert failed: ${msg}`);
}
function assertEq<T>(a: T, b: T, msg = "values differ"): void {
  if (a !== b) {
    throw new Error(`assertEq failed (${msg}): ${JSON.stringify(a)} !== ${JSON.stringify(b)}`);
  }
}
function assertContains(haystack: string, needle: string, msg = ""): void {
  if (!haystack.includes(needle)) {
    throw new Error(`assertContains failed (${msg}): missing ${JSON.stringify(needle)} in:\n${haystack}`);
  }
}

Deno.test("EXPORT_FORMATS lists md/json/html", () => {
  assertEq(EXPORT_FORMATS.length, 3);
  assert(EXPORT_FORMATS.includes("md"));
  assert(EXPORT_FORMATS.includes("json"));
  assert(EXPORT_FORMATS.includes("html"));
});

Deno.test("formatMarkdown emits H1 title + per-message H3 sections", () => {
  const out = formatMarkdown({
    title: "Trip planning",
    messages: [
      { id: "1", role: "user", content: "Where should I go?", ts: 1717000000000 },
      { id: "2", role: "assistant", content: "Try Kyoto.", ts: 1717000001000, model: "llama3.1" },
    ],
  });
  assertContains(out, "# Trip planning");
  assertContains(out, "### user (");
  assertContains(out, "Where should I go?");
  assertContains(out, "### assistant (");
  assertContains(out, "Try Kyoto.");
});

Deno.test("formatMarkdown folds tool_calls into a <details> block", () => {
  const out = formatMarkdown({
    title: "t",
    messages: [
      {
        id: "1",
        role: "assistant",
        content: "Let me check…",
        ts: 1,
        tool_calls: [{
          id: "call_x",
          type: "function",
          function: { name: "get_weather", arguments: '{"city":"Paris"}' },
        }],
      },
    ],
  });
  assertContains(out, "<details>");
  assertContains(out, "Tool calls");
  assertContains(out, "get_weather");
  assertContains(out, '{"city":"Paris"}');
});

Deno.test("formatJson returns the full envelope", () => {
  const out = formatJson({
    id: "sess-1",
    title: "demo",
    messages: [
      { id: "1", role: "user", content: "hi", ts: 1 },
      { id: "2", role: "assistant", content: "hello", ts: 2 },
    ],
  });
  const parsed = JSON.parse(out);
  assertEq(parsed.id, "sess-1");
  assertEq(parsed.title, "demo");
  assert(typeof parsed.exportedAt === "string", "exportedAt is an ISO string");
  assert(parsed.exportedAt.length > 0);
  assertEq(parsed.messages.length, 2);
  assertEq(parsed.messages[0].content, "hi");
  assertEq(parsed.messages[1].role, "assistant");
});

Deno.test("formatHtml escapes a <script> injection", () => {
  // The DOMPurify-backed `sanitizeMarkdown` only runs when
  // `window.marked` + `window.DOMPurify` are present (browser).
  // Under Deno the module falls back to a plain-text render
  // that already escapes — the test pins that fallback so a
  // future refactor that strips the fallback surfaces here
  // instead of in production.
  const out = formatHtml({
    title: "x",
    messages: [
      { id: "1", role: "user", content: "<script>alert(1)</script>hello", ts: 1 },
    ],
  });
  assert(!out.includes("<script>alert(1)</script>"),
    "raw <script> tag must never appear in the export");
  assertContains(out, "&lt;script&gt;");
  assertContains(out, "hello");
  // The page chrome still references the title and includes
  // the exported-at marker.
  assertContains(out, "<!doctype html>");
  assertContains(out, "<h1>x</h1>");
});

Deno.test("filenameFor sanitises the title and stamps a date", () => {
  const fname = filenameFor("Trip planning!! 2026?", "md", new Date("2026-10-09T14:25:00"));
  // `[a-z0-9-_]` only, capped at 40 chars, suffixed with
  // `-YYYYMMDD-HHMM.md`.
  assertEq(fname, "Trip_planning_2026-20261009-1425.md");
});

Deno.test("filenameFor handles empty / odd titles", () => {
  const a = filenameFor("", "json");
  assert(a.endsWith(".json"));
  assert(a.includes("chat-"));
  const b = filenameFor("!!!!", "html");
  assert(b.endsWith(".html"));
});
