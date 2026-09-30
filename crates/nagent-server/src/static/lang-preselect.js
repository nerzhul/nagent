// Locale-aware preselection for `<select>` language pickers.
//
// Both Transcript mode (`#lang-select` in app.js) and Discussion mode
// (`#chat-lang-select` in chat.js) carry the same fixed list of
// Whisper-supported languages. This helper inspects the browser's
// preferred language(s) and preselects the matching option on first
// load, so users whose browser locale already maps to a supported
// language do not have to dig through the picker.
//
// Kept in its own module so the matching logic is small, isolated,
// and reusable from both call sites.

/**
 * Pick the first supported language in `selectEl` whose `value`
 * matches any of `navigator.languages` (preferred) or `navigator.language`
 * (fallback).
 *
 * Matching rules:
 * - Case-insensitive.
 * - Each BCP-47 tag is reduced to its primary language subtag before
 *   comparison, so `fr-FR` matches an option `fr`, `en-US` matches
 *   `en`, and `zh-Hans-CN` matches `zh`.
 * - If the select already has a non-empty value, it is left untouched
 *   so explicit persistence always wins over locale sniffing.
 * - A `change` event is dispatched after mutation so any listener
 *   (e.g. the `audio.js` config sync) re-reads the new value.
 *
 * @param {HTMLSelectElement} selectEl
 * @returns {string|null} the value that was selected, or null if the
 *                         select was missing, already set, or no
 *                         browser language matched.
 */
export function preselectFromBrowser(selectEl) {
  if (!selectEl || typeof HTMLSelectElement === "undefined"
      || !(selectEl instanceof HTMLSelectElement)) {
    return null;
  }
  if (selectEl.value) return null;

  const tags = [];
  if (Array.isArray(navigator.languages)) {
    for (const l of navigator.languages) {
      if (l) tags.push(l);
    }
  }
  if (navigator.language && !tags.includes(navigator.language)) {
    tags.push(navigator.language);
  }
  if (tags.length === 0) return null;

  const supported = new Set();
  for (const opt of selectEl.options) {
    if (opt.value) supported.add(opt.value.toLowerCase());
  }

  for (const tag of tags) {
    const primary = String(tag).split(/[-_]/)[0].toLowerCase();
    if (primary && supported.has(primary)) {
      selectEl.value = primary;
      selectEl.dispatchEvent(new Event("change", { bubbles: true }));
      return primary;
    }
  }
  return null;
}
