// nagent — Discussion-mode user geolocation helpers.
//
// Pure-ish helpers around `navigator.geolocation` and `localStorage`
// so `chat.js` can ask "should I prepend a location block to this
// LLM request?" without re-implementing the cache/format logic in
// two places. The module is side-effect-free on import: nothing
// reads `localStorage` or touches `navigator.geolocation` until a
// function is called.
//
// Storage layout (under `localStorage`):
//
//   nagent.chat.location         -> JSON { lat, lon, accuracy, timestamp }
//   nagent.chat.locationEnabled  -> "true" / "false"
//
// There is deliberately **no TTL on the cached position**. The
// browser's permission grant covers every `getCurrentPosition` call
// after the user opts in, so `chat.js` re-fetches the position on
// every UI load (see `refreshLocationOnBoot`) and silently overwrites
// the cache. Within a single session the cache is trusted at face
// value; the manual "Refresh my location" button in Advanced is the
// only mid-session re-acquisition path.

import {
  LOCATION_KEY,
  LOCATION_ENABLED_KEY,
} from "/static/chat-sessions.js";

// Marker prefix for the ephemeral system message that `chat.js`
// prepends to the LLM request body. Mirrored on the Rust side as
// `crate::llm_prompt::USER_LOCATION_MARKER` so the admin kill-switch
// (`LLM_ALLOW_USER_LOCATION=false`) can strip the block before it
// reaches the upstream model. Keep the two strings in sync.
export const LOCATION_MESSAGE_MARKER = "User's approximate location:";

// `accuracy` is in metres (`GeolocationPosition.coords.accuracy`).
// `timestamp` is `Date.now()` at capture time, never serialised as an
// ISO string so the in-memory and on-disk representations match.
function safeStorage() {
  return (typeof globalThis !== "undefined" && globalThis.localStorage)
    ? globalThis.localStorage
    : null;
}

export function loadCachedLocation() {
  const ls = safeStorage();
  if (!ls) return null;
  try {
    const raw = ls.getItem(LOCATION_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    const { lat, lon, accuracy, timestamp } = parsed;
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) return null;
    if (!Number.isFinite(accuracy) || !Number.isFinite(timestamp)) return null;
    return { lat, lon, accuracy, timestamp };
  } catch (_e) {
    return null;
  }
}

export function saveCachedLocation(loc) {
  const ls = safeStorage();
  if (!ls) return;
  try { ls.setItem(LOCATION_KEY, JSON.stringify(loc)); } catch (_e) {}
}

export function clearCachedLocation() {
  const ls = safeStorage();
  if (!ls) return;
  try { ls.removeItem(LOCATION_KEY); } catch (_e) {}
}

export function loadLocationEnabled() {
  const ls = safeStorage();
  if (!ls) return false;
  try {
    return ls.getItem(LOCATION_ENABLED_KEY) === "true";
  } catch (_e) {
    return false;
  }
}

export function setLocationEnabled(enabled) {
  const ls = safeStorage();
  if (!ls) return;
  try {
    if (enabled) ls.setItem(LOCATION_ENABLED_KEY, "true");
    else ls.removeItem(LOCATION_ENABLED_KEY);
  } catch (_e) {}
}

// Render the cached position as the body of an ephemeral system
// message. The marker prefix MUST stay at the start so the server's
// defensive strip can match it. "Captured at HH:MM" stays in the
// message so the LLM can flag staleness during long sessions where
// the user has not moved the page since the boot refresh.
export function formatLocationMessage(loc, now = Date.now()) {
  const captured = new Date(loc.timestamp).toISOString().replace(/\.\d{3}Z$/, "Z");
  const ageMs = Math.max(0, now - loc.timestamp);
  const ageText = formatRelativeTime(ageMs);
  // Round lat/lon to 4 decimals (~11 m precision) so the message
  // stays short; full precision is in the cache for any future use.
  const lat = loc.lat.toFixed(4);
  const lon = loc.lon.toFixed(4);
  const acc = Math.max(0, Math.round(loc.accuracy));
  return (
    `${LOCATION_MESSAGE_MARKER} lat=${lat}, lon=${lon} (±${acc} m, `
    + `captured ${captured}, ${ageText} ago). `
    + `Treat location-relative queries ("here", "weather", "time", `
    + `"today", "tonight", "near me", ...) as referring to this place `
    + `unless the user explicitly names another location. When calling `
    + `get_weather without a named place, pass the coordinates as `
    + `location='lat,lon' directly.`
  );
}

// Compact relative-time formatter for the advanced-panel status row
// and the location message. Caps at "1d+" so a multi-day-stale value
// does not produce an unreadable "X days" wall of text.
export function formatRelativeTime(deltaMs) {
  const s = Math.round(deltaMs / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h`;
  return "1d+";
}

// Ask the browser for a fresh position. Resolves with the captured
// record; rejects with the original `GeolocationPositionError` so the
// caller can branch on `PERMISSION_DENIED` vs `POSITION_UNAVAILABLE`
// vs `TIMEOUT`. Used both for the initial opt-in (cache miss) and for
// the page-load silent refresh (cache hit + permission still granted).
//
// `enableHighAccuracy: false` keeps the call snappy (cellular/Wi-Fi
// triangulation is enough for "what's the weather?"). `maximumAge:
// 60_000` lets the browser return a sub-minute-old fix without a
// fresh GPS lock; `timeout: 10_000` matches the user's patience on a
// first opt-in click.
export function getLocation() {
  return new Promise((resolve, reject) => {
    if (typeof navigator === "undefined" || !navigator.geolocation) {
      reject(new Error("geolocation API unavailable"));
      return;
    }
    navigator.geolocation.getCurrentPosition(
      (pos) => {
        const { latitude, longitude, accuracy } = pos.coords;
        resolve({
          lat: latitude,
          lon: longitude,
          accuracy: Number.isFinite(accuracy) ? accuracy : 0,
          timestamp: Date.now(),
        });
      },
      (err) => reject(err),
      { enableHighAccuracy: false, timeout: 10_000, maximumAge: 60_000 },
    );
  });
}
