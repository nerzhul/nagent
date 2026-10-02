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
//   * On toggle, `saveToServer(loc, tz, reply_language)` PUTs the
//     new flags to `/api/me/preferences` (CSRF-protected) and
//     mirrors them to `localStorage`. The PUT is fire-and-forget so
//     a slow network does not block the UI: the localStorage write
//     happens first so the next request the UI builds already
//     reflects the user's choice, and the server catches up
//     asynchronously. A 401 / 404 / network failure is logged but
//     does NOT roll back the local change — the user explicitly
//     asked for the toggle to flip.
//
// The wire shape mirrors the auth_preferences migration: each
// boolean flag is a JSON bool, `reply_language` is a JSON string
// or `null`. The row is updated atomically by the server (all
// three fields required in the body). Sending every field on
// every PUT means the localStorage writes always match the
// server-side state for a single user — no drift between a laptop
// that has the location toggle ON and a phone that has the
// timezone toggle ON.

import {
  LOCATION_ENABLED_KEY,
  TIMEZONE_ENABLED_KEY,
} from "/static/chat-sessions.js";

/// `localStorage` key that mirrors the per-user reply-language
/// preference for the anonymous / pre-auth fallback path. Kept
/// namespaced under `nagent.chat.*` like the existing two keys so a
/// `localStorage.clear()` wipes everything together.
export const REPLY_LANGUAGE_LS_KEY = "nagent.chat.replyLanguage";

/// Cache of the latest server-side preferences. Lets the toggle
/// handlers PUT the *current* full state (location + timezone +
/// reply_language) without re-reading localStorage at toggle time —
/// keeping the three flags in lockstep so a PUT always reflects
/// the user's current intent for *every* preference, not just the
/// one that was just clicked.
///
/// `reply_language` is `null` (not `""`) for the "Auto" / unset
/// case so the wire payload and the cached state agree on a single
/// "no explicit preference" value — empty strings from the
/// `<select>` are normalised to `null` at read time.
let _cachedPrefs = { location: null, timezone: null, reply_language: null };

function csrfHeaders() {
  return window.nagentAuth?.csrfHeaders?.() || undefined;
}

/// Read the current preference flags from localStorage. Used as
/// the fallback when the server fetch fails AND as the source of
/// truth for the synchronous render path (`renderLocationUi`,
/// `renderTimezoneUi`, the reply-language picker, the per-request
/// `maybeBuildLocationBlock` / `maybeBuildTimezoneBlock`). The boot
/// path overwrites this with the server-side value when the fetch
/// succeeds.
function _readLocal() {
  const ls = (typeof globalThis !== "undefined" && globalThis.localStorage)
    ? globalThis.localStorage
    : null;
  if (!ls) return { location: false, timezone: false, reply_language: null };
  let location = false;
  let timezone = false;
  let reply_language = null;
  try { location = ls.getItem(LOCATION_ENABLED_KEY) === "true"; } catch (_) {}
  try { timezone = ls.getItem(TIMEZONE_ENABLED_KEY) === "true"; } catch (_) {}
  try {
    const v = ls.getItem(REPLY_LANGUAGE_LS_KEY);
    // Local mirror stores the raw `<select>` value: `""` for Auto,
    // a BCP-47 subtag otherwise. We normalise empty / whitespace
    // to `null` so the rest of the code only has to handle one
    // "no preference" representation.
    if (v != null && v.trim() !== "") reply_language = v;
  } catch (_) {}
  return { location, timezone, reply_language };
}

function _writeLocal(location, timezone, reply_language) {
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
  try {
    // Always store the *resolved* value: `""` for Auto (so a
    // fresh read on a different device picks up the same default),
    // the BCP-47 subtag otherwise. Round-trip is unambiguous:
    // null/empty/whitespace in → Auto on read; anything else is
    // treated as the user's explicit choice.
    if (reply_language) ls.setItem(REPLY_LANGUAGE_LS_KEY, reply_language);
    else ls.removeItem(REPLY_LANGUAGE_LS_KEY);
  } catch (_) { /* quota or disabled storage */ }
}

/// Fetch the server-side preferences and mirror them into
/// localStorage. Returns the resolved triple so callers (the boot
/// path) can re-render the toggles without a localStorage read.
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
    // Server returns `null` (or omits the field on an older build)
    // for "Auto"; anything else is the BCP-47 subtag the user
    // picked. We accept both representations so a server / client
    // skew never silently flips the picker.
    const reply_language = (typeof body?.reply_language === "string"
      && body.reply_language.trim() !== "")
      ? body.reply_language
      : null;
    _cachedPrefs = { location, timezone, reply_language };
    _writeLocal(location, timezone, reply_language);
    return { location, timezone, reply_language };
  } catch (e) {
    console.warn("preferences fetch error:", e);
    return _readLocal();
  }
}

/// Persist the supplied triple to the server and mirror them into
/// localStorage. The localStorage write happens *first* so a slow
/// PUT never blocks the UI; the PUT is fire-and-forget and any
/// server-side error is logged but does NOT roll back the local
/// state.
///
/// Pass the *full* triple (location + timezone + reply_language),
/// not just the one the user just clicked: the server treats a PUT
/// as an atomic replace of the row, so a partial body would
/// silently flip the other flags off.
export function saveToServer(location, timezone, reply_language) {
  // Normalise the reply_language argument: empty / whitespace /
  // explicit `null` all collapse to `null` (Auto) so every
  // downstream caller only ever sees one "no preference"
  // representation.
  const normalisedLang = (typeof reply_language === "string"
    && reply_language.trim() !== "")
    ? reply_language.trim()
    : null;
  _cachedPrefs = { location, timezone, reply_language: normalisedLang };
  _writeLocal(location, timezone, normalisedLang);
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
          // Wire-level Auto: JSON `null`. The server treats this
          // the same as a missing key (see the route handler
          // comment for the rationale).
          reply_language: normalisedLang,
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

/// Return the latest cached triple. Used by the toggle handlers
/// so a `PUT` body always reflects the *current* full state, never
/// just the flag the user just clicked.
export function currentFlags() {
  if (_cachedPrefs.location !== null
      && _cachedPrefs.timezone !== null
      && _cachedPrefs.reply_language !== undefined) {
    return { ..._cachedPrefs };
  }
  // First call before `loadFromServer` resolved — read from
  // localStorage so the toggle stays consistent with whatever the
  // user picked on this device.
  return _readLocal();
}

/// Return just the reply-language preference (or `""` for Auto).
/// Convenience helper for `chat.js::resolveTtsVoice` so the TTS
/// voice picker and the LLM reply-language picker share one source
/// of truth. The empty string is the Auto sentinel — the same
/// value the `<select>` uses for its "Auto" entry, so the caller
/// can drop the result straight back into a `<select>` without
/// further mapping.
export function getReplyLanguage() {
  return currentFlags().reply_language || "";
}

/// Return the user-facing label for the cached reply language (e.g.
/// "English", "Français") for use in the chat input placeholder
/// or the status pill. Returns `""` for Auto so the caller can
/// decide whether to render a label at all.
export function getReplyLanguageLabel() {
  const code = getReplyLanguage();
  if (!code) return "";
  const labels = {
    en: "English",
    fr: "Français",
    es: "Español",
    de: "Deutsch",
    it: "Italiano",
    pt: "Português",
    ja: "日本語",
    zh: "中文",
  };
  return labels[code] || code;
}