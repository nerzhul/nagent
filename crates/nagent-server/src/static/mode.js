// nagent — mode toggle (Transcript / Discussion / Settings).
//
// Three views, only one visible at a time. The active mode is
// persisted in `localStorage` so a reload lands the user where
// they left off. Adding the Settings tab (plan: settings-tab-rework)
// only required extending `VALID_MODES` and the button / view
// pair lookups; `applyMode` still flips the three DOM nodes
// by-id without any per-mode branching.
//
// The Settings view itself is a 2-column shell (llama.cpp style)
// with a left rail of section tabs and a right pane that shows one
// section at a time. `applySettingsSection` owns that sub-nav;
// the active section is persisted in `localStorage` so leaving
// and re-entering the Settings tab lands the user on the same
// section.
//
// `modechange` is dispatched on `document` whenever the active
// mode flips; `chat.js` listens for it to lazy-load the model
// list / hydrate history on first entry to Discussion mode.

(function () {
  "use strict";

  const STORAGE_KEY = "nagent.mode";
  const VALID_MODES = new Set(["transcript", "discussion", "settings"]);
  const DEFAULT_MODE = "transcript";

  // Tab-button ↔ view-id mapping. Kept as a single object so the
  // list of modes is the only source of truth — `applyMode` walks
  // it and flips `hidden` / `aria-selected` per pair.
  const PAIRS = {
    transcript: { btn: "mode-transcript-btn", view: "view-transcript" },
    discussion: { btn: "mode-discussion-btn", view: "view-discussion" },
    settings:   { btn: "mode-settings-btn",   view: "view-settings"   },
  };

  // Settings sub-navigation: rail-button ↔ pane-page mapping. Mirrors
  // `data-section` on `.settings-nav-item` and `data-section-page`
  // on `.settings-section` in `index.html`. Section change is a
  // pure DOM toggle (one `aria-selected="true"`, one pane unhidden)
  // — no `<details>` collapses or scroll bookkeeping involved.
  const SECTION_DEFAULT = "privacy";
  const SECTION_STORAGE_KEY = "nagent.settings.section";

  function readStoredMode() {
    try {
      const v = localStorage.getItem(STORAGE_KEY);
      if (v && VALID_MODES.has(v)) return v;
    } catch (_e) {
      // localStorage can throw in private mode / file:// origins; fall
      // back to the default rather than failing the whole UI.
    }
    return DEFAULT_MODE;
  }

  function writeStoredMode(mode) {
    try {
      localStorage.setItem(STORAGE_KEY, mode);
    } catch (_e) {
      // Same as above: ignore quota / availability errors. The in-page
      // toggle still works, only persistence is lost.
    }
  }

  function readStoredSection() {
    try {
      const v = localStorage.getItem(SECTION_STORAGE_KEY);
      if (v) return v;
    } catch (_e) { /* same fallback rationale as readStoredMode */ }
    return SECTION_DEFAULT;
  }

  function writeStoredSection(section) {
    try {
      localStorage.setItem(SECTION_STORAGE_KEY, section);
    } catch (_e) { /* same fallback rationale as writeStoredMode */ }
  }

  // Show the requested section in the right pane and mark the
  // matching rail button as selected. Unknown section ids fall back
  // to `SECTION_DEFAULT` so a stale localStorage value from an older
  // build cannot strand the user on an empty pane.
  function applySettingsSection(section, { dispatch = true } = {}) {
    const target = document.querySelector(
      `.settings-section[data-section-page="${section}"]`,
    ) ? section : SECTION_DEFAULT;

    for (const btn of document.querySelectorAll(".settings-nav-item")) {
      const isActive = btn.dataset.section === target;
      btn.setAttribute("aria-selected", String(isActive));
    }
    for (const pane of document.querySelectorAll(".settings-section")) {
      pane.hidden = pane.dataset.sectionPage !== target;
    }
    if (dispatch) {
      document.dispatchEvent(
        new CustomEvent("settingssectionchange", {
          detail: { section: target },
        }),
      );
    }
  }

  function activeSettingsSection() {
    const btn = document.querySelector(
      '.settings-nav-item[aria-selected="true"]',
    );
    return btn ? btn.dataset.section : SECTION_DEFAULT;
  }

  function applyMode(mode, { dispatch = true } = {}) {
    const active = VALID_MODES.has(mode) ? mode : DEFAULT_MODE;
    for (const [m, pair] of Object.entries(PAIRS)) {
      const btn = document.getElementById(pair.btn);
      const view = document.getElementById(pair.view);
      const isActive = m === active;
      if (view) view.hidden = !isActive;
      if (view) view.dataset.active = String(isActive);
      if (btn) btn.setAttribute("aria-selected", String(isActive));
    }
    if (dispatch) {
      document.dispatchEvent(
        new CustomEvent("modechange", { detail: { mode: active } }),
      );
    }
  }

  function onClick(mode) {
    return () => {
      const current = activeMode();
      if (mode === current) return;
      writeStoredMode(mode);
      applyMode(mode);
    };
  }

  // Compute the currently-active mode by walking `PAIRS` and finding
  // the view whose `dataset.active === "true"`. Replaces the old
  // two-way ternary now that there are three modes.
  function activeMode() {
    for (const [m, pair] of Object.entries(PAIRS)) {
      const view = document.getElementById(pair.view);
      if (view && view.dataset.active === "true") return m;
    }
    return DEFAULT_MODE;
  }

  // Re-hydrate the active view from localStorage *before* wiring the
  // listeners so the first paint already reflects the persisted mode
  // (no flash of the wrong tab being selected).
  const initial = readStoredMode();
  applyMode(initial, { dispatch: false });
  applySettingsSection(readStoredSection(), { dispatch: false });

  for (const [mode, pair] of Object.entries(PAIRS)) {
    const btn = document.getElementById(pair.btn);
    if (btn) btn.addEventListener("click", onClick(mode));
  }

  // Settings sub-nav: clicking a rail button persists the new
  // section and dispatches `settingssectionchange` for any
  // subscriber that wants to react (e.g. re-hydrating the
  // Integrations list once it becomes visible).
  for (const btn of document.querySelectorAll(".settings-nav-item")) {
    const section = btn.dataset.section;
    if (!section) continue;
    btn.addEventListener("click", () => {
      writeStoredSection(section);
      applySettingsSection(section);
    });
  }

  // Export a tiny read-only handle for chat.js to detect re-entries
  // (the `modechange` event already covers this, but the helper is
  // useful in tests / debugging).
  globalThis.__nagentMode = {
    current: () => activeMode(),
    settingsSection: () => activeSettingsSection(),
    showSettingsSection: (section) => {
      writeStoredSection(section);
      applySettingsSection(section);
    },
  };
})();