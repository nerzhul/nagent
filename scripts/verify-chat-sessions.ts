// Unit tests for the chat session store.
//
// `chat-sessions.js` is a pure localStorage wrapper; the tests stub
// `globalThis.localStorage` with an in-memory map so the module runs
// under Deno without a browser. We test:
//
//   - migration from the legacy single-history key
//   - per-session isolation (a save in one session doesn't leak into
//     another, the sidebar descriptor stays in sync)
//   - rename / touch semantics
//   - deleteSession cleans up both the descriptor and the per-session
//     history key
//   - deriveTitle's whitespace / truncation rules
//
// Each test resets the stub before running so the order is irrelevant.

import {
  ACTIVE_KEY,
  DEFAULT_TITLE,
  HISTORY_CAP,
  HISTORY_PREFIX,
  LEGACY_HISTORY_KEY,
  SESSIONS_KEY,
  TITLE_MAX,
  createSessionObj,
  deleteSession,
  deriveTitle,
  getActiveId,
  historyKey,
  isValidSession,
  loadHistory,
  loadSessions,
  migrateLegacy,
  persistNewSession,
  renameSession,
  saveHistory,
  saveSessions,
  setActiveId,
  sortedSessions,
  touchSession,
} from "../crates/stt-server/src/static/chat-sessions.js";

// Minimal `Storage`-shaped object that satisfies the methods
// `chat-sessions.js` actually calls. We cast to `Storage` on install
// so the assignment to `globalThis.localStorage` lines up with the
// DOM lib type.
function makeStubStorage() {
  const map = new Map<string, string>();
  return {
    getItem: (k: string): string | null => (map.has(k) ? map.get(k)! : null),
    setItem: (k: string, v: string): void => { map.set(k, String(v)); },
    removeItem: (k: string): void => { map.delete(k); },
    clear: (): void => { map.clear(); },
    _map: map,
  };
}

function installStorage() {
  const stub = makeStubStorage();
  // Cast: the stub implements the subset of `Storage` the module
  // exercises (`getItem` / `setItem` / `removeItem`). The full DOM
  // type carries extra members we don't need.
  //
  // Deno exposes `localStorage` as a getter/setter pair on the global
  // object (it points at an empty in-memory store by default). Plain
  // assignment to `globalThis.localStorage` does NOT replace it
  // because the built-in setter is what writes the property; we have
  // to redefine the accessor so subsequent bareword lookups resolve
  // to our stub.
  Object.defineProperty(globalThis, "localStorage", {
    value: stub as unknown as Storage,
    writable: true,
    configurable: true,
  });
  return stub;
}

function assert(cond: unknown, msg = "condition is falsy"): asserts cond {
  if (!cond) throw new Error(`assert failed: ${msg}`);
}
function assertEq<T>(a: T, b: T, msg = "values differ"): void {
  if (a !== b) throw new Error(`assertEq failed (${msg}): ${JSON.stringify(a)} !== ${JSON.stringify(b)}`);
}
function assertDeepEq<T>(a: T, b: T, msg = "values differ"): void {
  const sa = JSON.stringify(a);
  const sb = JSON.stringify(b);
  if (sa !== sb) throw new Error(`assertDeepEq failed (${msg}): ${sa} !== ${sb}`);
}

function freshId() {
  // Deterministic ids make test failures easy to read.
  let n = 0;
  return () => `id-${++n}`;
}

Deno.test("deriveTitle collapses whitespace and trims", () => {
  assertEq(deriveTitle(""), DEFAULT_TITLE);
  assertEq(deriveTitle("   "), DEFAULT_TITLE);
  assertEq(deriveTitle("\n\t  hello  \n  world \t"), "hello world");
  assertEq(deriveTitle("single"), "single");
});

Deno.test("deriveTitle caps at TITLE_MAX with an ellipsis", () => {
  const long = "a".repeat(TITLE_MAX + 10);
  const t = deriveTitle(long);
  assert(t.endsWith("…"), "truncated title ends with ellipsis");
  assertEq(t.length, TITLE_MAX);
});

Deno.test("loadSessions returns [] when storage is empty", () => {
  installStorage();
  assertDeepEq(loadSessions(), []);
  assertEq(getActiveId(), "");
});

Deno.test("isValidSession rejects malformed descriptors", () => {
  assert(!isValidSession(null));
  assert(!isValidSession({}));
  assert(!isValidSession({ id: "x" }));
  assert(isValidSession({ id: "x", title: "t", createdAt: 1, updatedAt: 1 }));
});

Deno.test("persistNewSession then loadSessions round-trips", () => {
  installStorage();
  const s = { id: "abc", title: "T", createdAt: 1, updatedAt: 1 };
  persistNewSession(s);
  const sessions = loadSessions();
  assertEq(sessions.length, 1);
  assertEq(sessions[0].id, "abc");
});

Deno.test("sortedSessions orders by updatedAt desc", () => {
  installStorage();
  const sessions = [
    { id: "a", title: "A", createdAt: 1, updatedAt: 1 },
    { id: "b", title: "B", createdAt: 1, updatedAt: 5 },
    { id: "c", title: "C", createdAt: 1, updatedAt: 3 },
  ];
  assertDeepEq(sortedSessions(sessions).map((s) => s.id), ["b", "c", "a"]);
});

Deno.test("saveHistory / loadHistory stay isolated per session", () => {
  installStorage();
  saveHistory("s1", [{ role: "user", content: "hi", ts: 1 }]);
  saveHistory("s2", [{ role: "user", content: "there", ts: 2 }]);
  assertEq(loadHistory("s1")[0].content, "hi");
  assertEq(loadHistory("s2")[0].content, "there");
  assertEq(loadHistory("s3").length, 0);
});

Deno.test("saveHistory caps at HISTORY_CAP", () => {
  installStorage();
  const huge = Array.from({ length: HISTORY_CAP + 25 }, (_, i) => ({
    role: i % 2 === 0 ? "user" : "assistant",
    content: `m${i}`,
    ts: i,
  }));
  saveHistory("s1", huge);
  const loaded = loadHistory("s1");
  assertEq(loaded.length, HISTORY_CAP);
  // The cap is "keep the last N" so the first surviving entry is the
  // 26th message, not the first.
  assertEq(loaded[0].content, `m${25}`);
  assertEq(loaded[HISTORY_CAP - 1].content, `m${HISTORY_CAP + 24}`);
});

Deno.test("touchSession bumps updatedAt and is a no-op on unknown ids", () => {
  installStorage();
  persistNewSession({ id: "a", title: "A", createdAt: 1, updatedAt: 1 });
  touchSession("a");
  assertEq(loadSessions()[0].updatedAt >= 2, true);
  // Unknown id must not throw.
  touchSession("nope");
  assertEq(loadSessions().length, 1);
});

Deno.test("renameSession updates title and bumps updatedAt", () => {
  installStorage();
  persistNewSession({ id: "a", title: "old", createdAt: 1, updatedAt: 1 });
  renameSession("a", "new");
  const sessions = loadSessions();
  assertEq(sessions[0].title, "new");
  assertEq(sessions[0].updatedAt >= 2, true);
});

Deno.test("deleteSession removes both the descriptor and the history key", () => {
  installStorage();
  persistNewSession({ id: "a", title: "A", createdAt: 1, updatedAt: 1 });
  saveHistory("a", [{ role: "user", content: "x", ts: 1 }]);
  assertEq(loadHistory("a").length, 1);
  deleteSession("a");
  assertDeepEq(loadSessions(), []);
  assertEq(loadHistory("a").length, 0);
  // The descriptor for an unrelated session is untouched.
  persistNewSession({ id: "b", title: "B", createdAt: 1, updatedAt: 1 });
  deleteSession("a");
  assertEq(loadSessions().length, 1);
});

Deno.test("migrateLegacy wraps a legacy history list into one session", () => {
  const stub = installStorage();
  const legacy = [
    { role: "user", content: "first hello", ts: 1 },
    { role: "assistant", content: "hi", ts: 2 },
    { role: "user", content: "second", ts: 3 },
  ];
  stub.setItem(LEGACY_HISTORY_KEY, JSON.stringify(legacy));
  const migrated = migrateLegacy();
  assert(migrated !== null, "migrateLegacy returns the new session on success");
  assertEq(migrated.title, "first hello", "title is derived from the first user turn");
  assertDeepEq(loadHistory(migrated.id), legacy, "the full list is preserved under the new key");
  assertEq(getActiveId(), migrated.id, "the migrated session is set as active");
  assertEq(stub.getItem(LEGACY_HISTORY_KEY), null, "the legacy key is removed");
});

Deno.test("migrateLegacy drops an empty legacy payload and removes the key", () => {
  const stub = installStorage();
  stub.setItem(LEGACY_HISTORY_KEY, "[]");
  assertEq(migrateLegacy(), null);
  assertEq(stub.getItem(LEGACY_HISTORY_KEY), null);
});

Deno.test("migrateLegacy drops a malformed legacy payload instead of throwing", () => {
  const stub = installStorage();
  stub.setItem(LEGACY_HISTORY_KEY, "{not json");
  assertEq(migrateLegacy(), null);
  assertEq(stub.getItem(LEGACY_HISTORY_KEY), null);
});

Deno.test("migrateLegacy is a no-op when the new layout is already populated", () => {
  const stub = installStorage();
  stub.setItem(LEGACY_HISTORY_KEY, JSON.stringify([
    { role: "user", content: "should not migrate", ts: 1 },
  ]));
  // Pre-existing new layout.
  persistNewSession({ id: "pre", title: "Pre", createdAt: 1, updatedAt: 1 });
  setActiveId("pre");
  assertEq(migrateLegacy(), null);
  // The legacy payload is left alone so a rollback doesn't lose state.
  assert(stub.getItem(LEGACY_HISTORY_KEY) !== null);
  assertEq(getActiveId(), "pre");
});

Deno.test("createSessionObj assigns a unique id and current timestamps", () => {
  installStorage();
  const a = createSessionObj("hello");
  const b = createSessionObj("world");
  assert(a.id !== b.id, "ids are distinct");
  assert(a.createdAt > 0 && a.updatedAt > 0, "timestamps are positive");
  assertEq(a.title, "hello");
  assertEq(b.title, "world");
});

Deno.test("setActiveId round-trips and clears on empty", () => {
  installStorage();
  setActiveId("xyz");
  assertEq(getActiveId(), "xyz");
  setActiveId("");
  assertEq(getActiveId(), "");
  assertEq(globalThis.localStorage.getItem(ACTIVE_KEY), null);
});

Deno.test("historyKey uses the expected prefix", () => {
  assertEq(historyKey("abc"), `${HISTORY_PREFIX}abc`);
});

Deno.test("clear flow: deleting the active session leaves siblings intact", () => {
  // Simulates the storage-level half of `clearChat`: wipe the active
  // session's message list and descriptor, then verify the remaining
  // session is untouched and a fallback can be picked. The DOM-side
  // teardown (abort, queue reset, re-render) and the active-id clear
  // live in the UI layer — `chat.js`'s `deleteSession` wrapper clears
  // the pointer on top of the store call, see the wrapper for the
  // full policy. Here we only verify what the store promises.
  installStorage();
  // Two sessions, A is active.
  persistNewSession({ id: "A", title: "A", createdAt: 1, updatedAt: 1 });
  persistNewSession({ id: "B", title: "B", createdAt: 1, updatedAt: 1 });
  saveHistory("A", [{ role: "user", content: "keep me?", ts: 1 }]);
  saveHistory("B", [{ role: "user", content: "important", ts: 1 }]);
  setActiveId("A");

  deleteSession("A");
  // A's history key is gone, B's history is intact.
  assertEq(loadHistory("A").length, 0);
  assertEq(loadHistory("B").length, 1);
  // The store leaves the active pointer alone — the UI wrapper is
  // responsible for clearing it. sortedSessions still returns B, so
  // the wrapper can pick it as the fallback active session.
  assertDeepEq(sortedSessions(loadSessions()).map((s) => s.id), ["B"]);
});
