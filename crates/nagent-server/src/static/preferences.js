// nagent — Discussion-mode user preference sync.
//
// Hoists the per-user "share my location with the LLM" / "forward
// my browser timezone to the LLM" toggles from `localStorage` to
// the server-side `user_preferences` row keyed by `user_id`. The
// flow is:
//
//   * On boot, `loadFromServer()` fetches `/api/me/preferences`
//     (CSRF-unprotected GET) and writes the result into the same
//     `localStorage` keys the rest of the chat reads from. A 401 /
//     404 / network failure falls back to whatever is already in
//     `localStorage` — an anonymous user on an `auth.enabled =
//     false` server gets the pre-existing localStorage behaviour
//     unchanged.
//   * On toggle, `saveToServer(flags)` PUTs the new flags to
//     `/api/me/preferences` (CSRF-protected) and mirrors them to
//     `localStorage`. The PUT is fire-and-forget so a slow
//     network does not block the UI: the localStorage write
//     happens first so the next request the UI builds already
//     reflects the user's choice, and the server catches up
//     asynchronously. A 401 / 404 / network failure is logged but
//     does NOT roll back the local change — the user explicitly
//     asked for the toggle to flip.
//
// The wire shape mirrors the auth_preferences migration: each flag
// is a JSON bool, the row is updated atomically by the server
// (both flags required in the body). Sending both flags on every
// PUT means the localStorage writes always match the server-side
// state for a single user — no drift between a laptop that has
// the location toggle ON and a phone that has the timezone toggle
// ON.

import {
  LOCATION_ENABLED_KEY,
  TIMEZONE_ENABLED_KEY,
} from "/static/chat-sessions.js";

/// Cache of the latest server-side preferences. Lets the toggle
/// handlers PUT the *current* full state (location + timezone)
/// without re-reading localStorage at toggle time — keeping the
/// two flags in lockstep so a PUT always reflects the user's
/// current intent for *both* opt-ins, not just the one that was
/// just clicked.
let _cachedFlags = { location: null, timezone: null };

function csrfHeaders() {
  return window.nagentAuth?.csrfHeaders?.() || undefined;
}

/// Read the current preference flags from localStorage. Used as
/// the fallback when the server fetch fails AND as the source of
/// truth for the synchronous render path (`renderLocationUi`,
/// `renderTimezoneUi`, `maybeBuildLocationBlock`,
/// `maybeBuildTimezoneBlock`). The boot path overwrites this with
/// the server-side value when the fetch succeeds.
function _readLocal() {
  const ls = (typeof globalThis !== "undefined" && globalThis.localStorage)
    ? globalThis.localStorage
    : null;
  if (!ls) return { location: false, timezone: false };
  let location = false;
  let timezone = false;
  try { location = ls.getItem(LOCATION_ENABLED_KEY) === "true"; } catch (_) {}
  try { timezone = ls.getItem(TIMEZONE_ENABLED_KEY) === "true"; } catch (_) {}
  return { location, timezone };
}

function _writeLocal(location, timezone) {
  const ls = (typeof globalThis !== "undefined" && globalThis.localStorage)
    ? globalThis.localStorage
    : null;
  if (!ls) return;
  try {
    if (location) ls.setItem(LOCATION_ENABLED_KEY, "true");
    else ls.removeItem(LOCATION_ENABLED_KEY);
  } catch (_) { /* quota or disabled storage */ }
  try {
    if (timezone) ls.setItem(TIMEZONE_ENABLED_KEY, "true");
    else ls.removeItem(TIMEZONE_ENABLED_KEY);
  } catch (_) { /* quota or disabled storage */ }
}

/// Fetch the server-side preferences and mirror them into
/// localStorage. Returns the resolved flag pair so callers (the
/// boot path) can re-render the toggles without a localStorage read.
/// `findLastPath` is intentionally silent on network failure: the
/// localStorage copy stays, and the user sees the last value they
/// picked on this device. Auth-related errors (401 = anonymous on
/// an auth-enabled server, 404 = `/api/me/preferences` not mounted)
/// are also silently swallowed for the same reason.
export async function loadFromServer() {
  try {
    const resp = await fetch("/api/me/preferences", {
      credentials: "same-origin",
      cache: "no-store",
    });
    if (!resp.ok) {
      // 401 (anonymous) and 404 (route not mounted) are expected
      // failure modes — both fall back to localStorage. Other
      // statuses are noisy but non-fatal.
      if (resp.status !== 401 && resp.status !== 404) {
        console.warn("preferences fetch failed:", resp.status);
      }
      return _readLocal();
    }
    const body = await resp.json();
    const location = !!body?.share_location_enabled;
    const timezone = !!body?.share_timezone_enabled;
    _cachedFlags = { location, timezone };
    _writeLocal(location, timezone);
    return { location, timezone };
  } catch (e) {
    console.warn("preferences fetch error:", e);
    return _readLocal();
  }
}

/// Persist the supplied flags to the server and mirror them into
/// localStorage. The localStorage write happens *first* so a slow
/// PUT never blocks the UI; the PUT is fire-and-forget and any
/// server-side error is logged but does NOT roll back the local
/// state.
///
/// Pass the *full* flag pair (location + timezone), not just the
/// one the user just clicked: the server treats a PUT as an
/// atomic replace of the row, so a partial body would silently
/// flip the other flag off.
export function saveToServer(location, timezone) {
  _cachedFlags = { location, timezone };
  _writeLocal(location, timezone);
  // Fire-and-forget: the localStorage mirror above already
  // satisfies the user's intent on this device, the PUT just
  // syncs to the server so a different device / browser picks up
  // the same flags on next login. We do not await it.
  (async () => {
    try {
      const resp = await fetch("/api/me/preferences", {
        method: "PUT",
        credentials: "same-origin",
        headers: {
          "Content-Type": "application/json",
          ...(csrfHeaders() || {}),
        },
        body: JSON.stringify({
          share_location_enabled: !!location,
          share_timezone_enabled: !!timezone,
        }),
      });
      if (!resp.ok) {
        if (resp.status !== 401 && resp.status !== 404) {
          console.warn("preferences save failed:", resp.status);
        }
      }
    } catch (e) {
      console.warn("preferences save error:", e);
    }
  })();
}

/// Return the latest cached flag pair. Used by the toggle
/// handlers so a `PUT` body always reflects the *current* full
/// state, never just the flag the user just clicked.
export function currentFlags() {
  if (_cachedFlags.location !== null && _cachedFlags.timezone !== null) {
    return { ..._cachedFlags };
  }
  // First call before `loadFromServer` resolved — read from
  // localStorage so the toggle stays consistent with whatever the
  // user picked on this device.
  return _readLocal();
}