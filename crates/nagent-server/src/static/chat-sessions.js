// nagent — Discussion-mode chat session store.
//
// Pure helpers around `localStorage` that keep the per-session
// message lists and the sidebar metadata in sync. Extracted out of
// `chat.js` so it can be exercised by `scripts/verify-chat-sessions.ts`
// without standing up a browser DOM. The browser calls these via
// `chat.js`; the test harness installs a stub `localStorage` on
// `globalThis` before importing the module.
//
// Storage layout (all under `localStorage`):
//
//   nagent.chat.sessions          -> JSON [{ id, title, createdAt, updatedAt }]
//   nagent.chat.active            -> active session id (string)
//   nagent.chat.session.<id>      -> JSON message array for that session
//   nagent.chat.history           -> LEGACY single-history payload (read once,
//                                    migrated into a session, then removed)

export const SESSIONS_KEY = "nagent.chat.sessions";
export const ACTIVE_KEY = "nagent.chat.active";
export const HISTORY_PREFIX = "nagent.chat.session.";
export const LEGACY_HISTORY_KEY = "nagent.chat.history";
export const HISTORY_CAP = 200;
export const TITLE_MAX = 60;
export const DEFAULT_TITLE = "New chat";

// per-session flag marking the
// localStorage copy as already mirrored on the server. The
// first authenticated load POSTs every entry of
// `nagent.chat.session.<id>` and sets this flag on success;
// subsequent loads skip the migration. Sessions that never
// migrate (server missing the table, network failure) keep
// working from localStorage only.
export const MIGRATED_PREFIX = "nagent.chat.migrated.";

export function migratedKey(id) {
  return MIGRATED_PREFIX + id;
}

// User-geolocation preference keys. Shared with `geolocation.js` and
// `chat.js` so the names live in exactly one place. The TTL lives on
// the cached position only — the enabled flag is a free switch and
// does not expire.
export const LOCATION_KEY = "nagent.chat.location";
export const LOCATION_ENABLED_KEY = "nagent.chat.locationEnabled";

// User-timezone opt-in flag. The detected IANA name is *not* cached:
// `Intl.DateTimeFormat().resolvedOptions().timeZone` is cheap, always
// fresh, and reflects the system clock even when the user crosses
// timezones between page loads. Storing it would freeze yesterday's
// zone across an evening flight.
export const TIMEZONE_ENABLED_KEY = "nagent.chat.timezoneEnabled";

// Last model the user picked from the Discussion-mode `<select>`.
// Persisted locally so a fresh page load (or a session switch)
// pre-selects the same model instead of jumping back to the first
// entry of the model list. We deliberately keep this *local* (not in
// `user_preferences` on the server) because it is a UI state
// preference, not a privacy / behaviour toggle: an Ollama install
// reachable from a laptop is typically a different inventory than
// the one reachable from a phone, so cross-device sync would pick a
// model the user's current backend doesn't serve. The model list is
// fetched fresh on every boot and the UI falls back to the first
// option whenever the saved id is missing from that list.
export const CHAT_MODEL_KEY = "nagent.chat.model";

// Local-storage mirror keys for the two LLM preferences migrated
// to `user_preferences` in migration 0008 (plan:
// settings-tab-rework). The DB row is the source of truth for
// authenticated users; these mirrors are read on first paint so
// the Settings tab does not flash empty defaults, and on
// anonymous / auth-disabled servers where the DB row does not
// exist. `preferences.js` owns the read / write / save-to-server
// contract; the constants are exported here so the key names
// live in exactly one place (same convention as
// `LOCATION_ENABLED_KEY` / `TIMEZONE_ENABLED_KEY`).
export const SYSTEM_KEY = "nagent.chat.system";
export const TEMPERATURE_KEY = "nagent.chat.temperature";

export function historyKey(id) {
  return HISTORY_PREFIX + id;
}

// True when the localStorage copy of `id` has been mirrored on
// the server. Used by `chat.js` to skip the migration shim on
// subsequent loads and to decide whether to fetch the server
// copy (or stick to localStorage) on history render.
export function isMigrated(id) {
  try {
    return globalThis.localStorage.getItem(migratedKey(id)) === "1";
  } catch (_e) {
    return false;
  }
}

export function markMigrated(id) {
  try {
    globalThis.localStorage.setItem(migratedKey(id), "1");
  } catch (_e) {}
}

export function isValidSession(s) {
  return s && typeof s.id === "string" && typeof s.title === "string"
    && typeof s.createdAt === "number" && typeof s.updatedAt === "number";
}

export function loadSessions() {
  try {
    const raw = globalThis.localStorage.getItem(SESSIONS_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isValidSession) : [];
  } catch (_e) {
    return [];
  }
}

export function saveSessions(sessions) {
  try {
    globalThis.localStorage.setItem(SESSIONS_KEY, JSON.stringify(sessions));
  } catch (_e) {}
}

export function loadHistory(id) {
  try {
    const raw = globalThis.localStorage.getItem(historyKey(id));
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch (_e) {
    return [];
  }
}

export function saveHistory(id, history) {
  try {
    const trimmed = history.slice(-HISTORY_CAP);
    globalThis.localStorage.setItem(historyKey(id), JSON.stringify(trimmed));
  } catch (_e) {}
}

export function getActiveId() {
  try {
    return globalThis.localStorage.getItem(ACTIVE_KEY) || "";
  } catch (_e) {
    return "";
  }
}

export function setActiveId(id) {
  try {
    if (id) globalThis.localStorage.setItem(ACTIVE_KEY, id);
    else globalThis.localStorage.removeItem(ACTIVE_KEY);
  } catch (_e) {}
}

// Read the last model the user picked from the `<select id="chat-model">`
// dropdown. Returns the empty string when no preference is stored yet
// (first boot) or when localStorage is unavailable (private mode).
// The caller is responsible for verifying the saved id still matches
// an entry of the freshly-fetched model list before assigning it to
// `select.value` — the dropdown silently ignores unknown values, but
// a stale id would quietly stick around in storage and be re-applied
// on every reload.
export function loadSelectedModel() {
  try {
    return globalThis.localStorage.getItem(CHAT_MODEL_KEY) || "";
  } catch (_e) {
    return "";
  }
}

// Persist the currently-selected model. Stores under `CHAT_MODEL_KEY`
// so the next `loadModels()` pass can restore it. Mirrors the
// `setActiveId` shape (single key, empty value removes) and uses the
// same try/catch so a quota / disabled-storage environment never
// throws into the UI event loop.
export function saveSelectedModel(id) {
  try {
    if (id) globalThis.localStorage.setItem(CHAT_MODEL_KEY, id);
    else globalThis.localStorage.removeItem(CHAT_MODEL_KEY);
  } catch (_e) {}
}

// Sort the in-memory session list by `updatedAt` desc. We re-sort on
// every mutation rather than maintaining order on disk so concurrent
// edits from multiple tabs (rare, but possible) reconcile to the same
// view instead of fighting over the array's order.
export function sortedSessions(sessions) {
  return [...sessions].sort((a, b) => b.updatedAt - a.updatedAt);
}

// `crypto.randomUUID` is available in modern browsers and Node 19+.
// The fallback exists for environments where the global is missing
// (older test runners, non-browser embeds). It is not cryptographically
// strong — the id only needs to be unique within one user's
// localStorage, not unpredictable.
export function newId() {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

// Build a title out of the first user turn of a session. We collapse
// whitespace (newlines frequently split LLM-targeted prompts in the
// wild) and cap at `TITLE_MAX` so a chat with a long monologue
// doesn't blow up the sidebar width.
export function deriveTitle(content) {
  const flat = String(content || "").replace(/\s+/g, " ").trim();
  if (!flat) return DEFAULT_TITLE;
  return flat.length > TITLE_MAX ? flat.slice(0, TITLE_MAX - 1) + "…" : flat;
}

export function createSessionObj(title = DEFAULT_TITLE) {
  const now = Date.now();
  return {
    id: newId(),
    title,
    createdAt: now,
    updatedAt: now,
  };
}

export function persistNewSession(session) {
  const sessions = loadSessions();
  sessions.push(session);
  saveSessions(sessions);
}

// Migration shim: on first boot with the new layout, wrap any
// pre-existing `nagent.chat.history` payload into a single session
// under its own key and remove the legacy key. We deliberately only
// migrate when the new layout is empty so a user who upgrades, plays
// with sessions, and then somehow rolls back doesn't silently lose
// the new state.
//
// Returns the migrated session when migration happened, or `null`
// when there was nothing to migrate (or migration was skipped because
// the new layout is already populated). The caller can use the
// return value to decide whether to repaint the sidebar.
export function migrateLegacy() {
  if (loadSessions().length > 0) return null;
  let legacy;
  try {
    const raw = globalThis.localStorage.getItem(LEGACY_HISTORY_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed) || parsed.length === 0) {
      globalThis.localStorage.removeItem(LEGACY_HISTORY_KEY);
      return null;
    }
    legacy = parsed;
  } catch (_e) {
    // Malformed legacy payload: drop it so the migration runs cleanly
    // next time rather than blocking the boot path on a parse error.
    try { globalThis.localStorage.removeItem(LEGACY_HISTORY_KEY); } catch (_e2) {}
    return null;
  }
  // Derive the title from the first user message we can find; fall
  // back to the default if the legacy history was assistant-only.
  const firstUser = legacy.find((m) => m && m.role === "user");
  const session = createSessionObj(firstUser ? deriveTitle(firstUser.content) : DEFAULT_TITLE);
  saveHistory(session.id, legacy);
  persistNewSession(session);
  setActiveId(session.id);
  try { globalThis.localStorage.removeItem(LEGACY_HISTORY_KEY); } catch (_e) {}
  return session;
}

// Bump `updatedAt` on a session so it floats to the top of the
// sidebar. No-op if the session is unknown.
export function touchSession(id) {
  const sessions = loadSessions();
  const idx = sessions.findIndex((s) => s.id === id);
  if (idx === -1) return;
  sessions[idx].updatedAt = Date.now();
  saveSessions(sessions);
}

export function renameSession(id, title) {
  const sessions = loadSessions();
  const idx = sessions.findIndex((s) => s.id === id);
  if (idx === -1) return;
  sessions[idx].title = title;
  sessions[idx].updatedAt = Date.now();
  saveSessions(sessions);
}

// Wipe the message list and the descriptor for `id`. Does not touch
// the active-id pointer — the caller picks a replacement (we leave
// the policy in the UI layer where the fallback behaviour lives).
export function deleteSession(id) {
  try { globalThis.localStorage.removeItem(historyKey(id)); } catch (_e) {}
  const sessions = loadSessions().filter((s) => s.id !== id);
  saveSessions(sessions);
}
