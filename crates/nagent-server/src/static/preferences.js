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
//   * On toggle, `saveToServer(loc, tz, reply_language, sys, temp)`
//     PUTs the new flags to `/api/me/preferences` (CSRF-protected)
//     and mirrors them to `localStorage`. The PUT is
//     fire-and-forget so a slow network does not block the UI: the
//     localStorage write happens first so the next request the UI
//     builds already reflects the user's choice, and the server
//     catches up asynchronously. A 401 / 404 / network failure is
//     logged but does NOT roll back the local change — the user
//     explicitly asked for the toggle to flip.
//
// The wire shape mirrors the auth_preferences migration: each
// boolean flag is a JSON bool, `reply_language` is a JSON string
// or `null`, `additional_instructions` is a JSON string or
// `null`, `temperature` is a JSON number or `null`. The row is
// updated atomically by the server (all five fields required in
// the body). Sending every field on every PUT means the
// localStorage writes always match the server-side state for a
// single user — no drift between a laptop that has the location
// toggle ON and a phone that has the temperature slider at 0.3.

import {
  LOCATION_ENABLED_KEY,
  TIMEZONE_ENABLED_KEY,
  SYSTEM_KEY,
  TEMPERATURE_KEY,
} from "/static/chat-sessions.js";

/// `localStorage` key that mirrors the per-user reply-language
/// preference for the anonymous / pre-auth fallback path. Kept
/// namespaced under `nagent.chat.*` like the existing two keys so a
/// `localStorage.clear()` wipes everything together.
export const REPLY_LANGUAGE_LS_KEY = "nagent.chat.replyLanguage";

/// `localStorage` key that mirrors the per-user long-term memory
/// opt-in (plan 1791267136806, §7.10). Same `nagent.chat.*`
/// namespace as the other mirrors so a `localStorage.clear()` wipes
/// the lot together.
export const MEMORY_ENABLED_LS_KEY = "nagent.chat.memoryEnabled";

/// UI defaults for the two LLM preferences. Mirrored here (rather
/// than read from `chat.js`) so the localStorage fall-back path
/// uses the same baseline the server-side default (`0.8` for
/// temperature, `""` for additional instructions) — see the
/// discussion in the Settings tab reset-to-defaults code in
/// `chat.js`. Changing the defaults in one place without the
/// other would silently revert a user's last explicit choice.
const DEFAULT_SYSTEM = "";
const DEFAULT_TEMPERATURE = 0.8;

/// Cache of the latest server-side preferences. Lets the toggle
/// handlers PUT the *current* full state (location + timezone +
/// reply_language + additional_instructions + temperature +
/// memory_enabled) without re-reading localStorage at toggle time
/// — keeping all six flags in lockstep so a PUT always reflects
/// the user's current intent for *every* preference, not just the
/// one that was just clicked.
///
/// `reply_language`, `additional_instructions`, and `temperature`
/// use `null` (not `""` / `0`) for the "Auto" / unset case so the
/// wire payload and the cached state agree on a single "no
/// explicit preference" value — empty strings from the
/// `<select>` and the empty textarea are normalised to `null` at
/// read time.
let _cachedPrefs = {
  location: null,
  timezone: null,
  reply_language: null,
  additional_instructions: null,
  temperature: null,
  memory_enabled: null,
};

function csrfHeaders() {
  return window.nagentAuth?.csrfHeaders?.() || undefined;
}

/// Read the current preference flags from localStorage. Used as
/// the fallback when the server fetch fails AND as the source of
/// truth for the synchronous render path (`renderLocationUi`,
/// `renderTimezoneUi`, the reply-language picker, the per-request
/// `maybeBuildLocationBlock` / `maybeBuildTimezoneBlock`, the LLM
/// system + temperature inputs in the Settings tab). The boot
/// path overwrites this with the server-side value when the fetch
/// succeeds.
function _readLocal() {
  const ls = (typeof globalThis !== "undefined" && globalThis.localStorage)
    ? globalThis.localStorage
    : null;
  if (!ls) {
    return {
      location: false,
      timezone: false,
      reply_language: null,
      additional_instructions: null,
      temperature: null,
      memory_enabled: false,
    };
  }
  let location = false;
  let timezone = false;
  let reply_language = null;
  let additional_instructions = null;
  let temperature = null;
  let memory_enabled = false;
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
  try {
    const v = ls.getItem(SYSTEM_KEY);
    // Empty / unset = "no user-supplied instructions". The local
    // mirror stores the textarea verbatim, so an empty string and a
    // missing key both collapse to `null` (matches the wire shape).
    if (v != null && v.trim() !== "") additional_instructions = v;
  } catch (_) {}
  try {
    const v = ls.getItem(TEMPERATURE_KEY);
    // Stored as a string (localStorage can only hold strings); we
    // parse back to a number here. Anything non-finite collapses to
    // `null` so the request builder (`chat.js::streamReply`) sees a
    // single "use the proxy default" sentinel.
    if (v != null && v.trim() !== "") {
      const n = Number.parseFloat(v);
      if (Number.isFinite(n)) temperature = n;
    }
  } catch (_) {}
  try { memory_enabled = ls.getItem(MEMORY_ENABLED_LS_KEY) === "true"; } catch (_) {}
  return {
    location, timezone, reply_language, additional_instructions, temperature,
    memory_enabled,
  };
}

function _writeLocal(
  location, timezone, reply_language,
  additional_instructions, temperature,
  memory_enabled,
) {
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
  try {
    // Additional instructions: store the raw user text so the
    // textarea on next load shows exactly what they typed. `null`
    // / empty string collapses to "no mirror" so the LLM proxy
    // falls back to the admin-supplied default system prompt.
    if (additional_instructions != null) {
      ls.setItem(SYSTEM_KEY, additional_instructions);
    } else {
      ls.removeItem(SYSTEM_KEY);
    }
  } catch (_) { /* quota or disabled storage */ }
  try {
    // Temperature: store the canonical float as a string so the
    // next session hydrates to the same value. `null` / NaN
    // collapses to "no mirror" — the request builder then sees
    // `null` and the LLM proxy falls back to the upstream model
    // default sampling.
    if (temperature != null && Number.isFinite(temperature)) {
      ls.setItem(TEMPERATURE_KEY, String(temperature));
    } else {
      ls.removeItem(TEMPERATURE_KEY);
    }
  } catch (_) { /* quota or disabled storage */ }
  try {
    if (memory_enabled) ls.setItem(MEMORY_ENABLED_LS_KEY, "true");
    else ls.removeItem(MEMORY_ENABLED_LS_KEY);
  } catch (_) { /* quota or disabled storage */ }
}

/// Fetch the server-side preferences and mirror them into
/// localStorage. Returns the resolved quintuple so callers (the
/// boot path) can re-render the toggles without a localStorage
/// read. The function is intentionally silent on network failure:
/// the localStorage copy stays, and the user sees the last value
/// they picked on this device. Auth-related errors (401 =
/// anonymous on an auth-enabled server, 404 =
/// `/api/me/preferences` not mounted) are also silently swallowed
/// for the same reason.
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
    const additional_instructions = (typeof body?.additional_instructions === "string"
      && body.additional_instructions.trim() !== "")
      ? body.additional_instructions
      : null;
    // Server returns a finite `number` for an explicit temperature
    // choice and `null` for "use the proxy default". Anything else
    // (string, NaN, missing) collapses to `null` so the request
    // builder only sees two states.
    const temperature = (typeof body?.temperature === "number"
      && Number.isFinite(body.temperature))
      ? body.temperature
      : null;
    // Plan 1791267136806 §1.6: `memory_enabled` defaults to
    // `false` on the server side (the migration sets
    // `INTEGER NOT NULL DEFAULT 0`). Older server builds that
    // pre-date the column return `undefined` — we coerce to
    // `false` so the Settings-tab Memory section defaults to
    // "off" on a fresh boot.
    const memory_enabled = !!body?.memory_enabled;
    _cachedPrefs = {
      location, timezone, reply_language,
      additional_instructions, temperature,
      memory_enabled,
    };
    _writeLocal(
      location, timezone, reply_language,
      additional_instructions, temperature,
      memory_enabled,
    );
    return _cachedPrefs;
  } catch (e) {
    console.warn("preferences fetch error:", e);
    return _readLocal();
  }
}

/// Persist the supplied sextuple to the server and mirror them
/// into localStorage. The localStorage write happens *first* so a
/// slow PUT never blocks the UI; the PUT is fire-and-forget and
/// any server-side error is logged but does NOT roll back the
/// local state.
///
/// Pass the *full* sextuple (location + timezone + reply_language
/// + additional_instructions + temperature + memory_enabled), not
/// just the one the user just clicked: the server treats a PUT as
/// an atomic replace of the row, so a partial body would silently
/// flip the other flags off.
export function saveToServer(
  location, timezone, reply_language,
  additional_instructions, temperature,
  memory_enabled,
) {
  // Normalise the reply_language argument: empty / whitespace /
  // explicit `null` all collapse to `null` (Auto) so every
  // downstream caller only ever sees one "no preference"
  // representation.
  const normalisedLang = (typeof reply_language === "string"
    && reply_language.trim() !== "")
    ? reply_language.trim()
    : null;
  const normalisedInstructions = (typeof additional_instructions === "string"
    && additional_instructions.trim() !== "")
    ? additional_instructions
    : null;
  const normalisedTemp = (typeof temperature === "number"
    && Number.isFinite(temperature))
    ? temperature
    : null;
  _cachedPrefs = {
    location,
    timezone,
    reply_language: normalisedLang,
    additional_instructions: normalisedInstructions,
    temperature: normalisedTemp,
    memory_enabled: !!memory_enabled,
  };
  _writeLocal(
    location, timezone, normalisedLang,
    normalisedInstructions, normalisedTemp,
    !!memory_enabled,
  );
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
          additional_instructions: normalisedInstructions,
          temperature: normalisedTemp,
          memory_enabled: !!memory_enabled,
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

/// Return the latest cached sextuple. Used by the toggle handlers
/// so a `PUT` body always reflects the *current* full state, never
/// just the flag the user just clicked.
export function currentFlags() {
  if (_cachedPrefs.location !== null
      && _cachedPrefs.timezone !== null
      && _cachedPrefs.reply_language !== undefined
      && _cachedPrefs.additional_instructions !== undefined
      && _cachedPrefs.temperature !== undefined
      && _cachedPrefs.memory_enabled !== undefined) {
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

/// Return the cached additional-instructions string (the same
/// value `#chat-system` is hydrated from on boot), or `""` when
/// the user has not supplied any. Mirrors `getReplyLanguage()`'s
/// empty-string sentinel so `systemEl.value || ""` continues to
/// work without an explicit null check in the request builder.
export function getSystem() {
  return currentFlags().additional_instructions || "";
}

/// Return the cached temperature (a finite `number`) or `null`
/// when the user has not set one — matches the wire shape the LLM
/// proxy understands (`number | null`, never `""`). The request
/// builder checks `Number.isFinite(...)` so a `null` return value
/// simply omits the field from the request body and the proxy
/// falls back to the upstream model default sampling.
export function getTemperature() {
  const v = currentFlags().temperature;
  return (typeof v === "number" && Number.isFinite(v)) ? v : null;
}

/// Plan 1791267136806 §7.10: return the cached long-term memory
/// opt-in flag. The Settings-tab Memory card wires this to its
/// `<input type="checkbox">` and to the future `memory_list` /
/// `memory_forget` UI. `false` is the safe default — a user who
/// never opened the toggle gets nothing stored, nothing injected,
/// and the LLM is told to refuse `memory_store` calls.
export function getMemoryEnabled() {
  return !!currentFlags().memory_enabled;
}

/// UI defaults exposed so the Settings-tab "Reset to defaults"
/// button can restore the same baseline without duplicating the
/// magic numbers. The values are also used as the boot-time
/// fallback when neither the server nor localStorage have a value
/// (anonymous first load on an `auth.enabled = false` server).
export function defaults() {
  return {
    system: DEFAULT_SYSTEM,
    temperature: DEFAULT_TEMPERATURE,
  };
}