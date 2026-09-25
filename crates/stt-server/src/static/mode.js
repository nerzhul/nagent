// nagent — mode toggle (Transcript / Discussion).
//
// Two views, only one visible at a time. The active mode is persisted
// in `localStorage` so a reload lands the user where they left off.
//
// `modechange` is dispatched on `document` whenever the active mode
// flips; `chat.js` listens for it to lazy-load the model list / hydrate
// history on first entry to Discussion mode.

(function () {
  "use strict";

  const STORAGE_KEY = "nagent.mode";
  const VALID_MODES = new Set(["transcript", "discussion"]);
  const DEFAULT_MODE = "transcript";

  const transcriptView = document.getElementById("view-transcript");
  const discussionView = document.getElementById("view-discussion");
  const transcriptBtn = document.getElementById("mode-transcript-btn");
  const discussionBtn = document.getElementById("mode-discussion-btn");

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

  function applyMode(mode, { dispatch = true } = {}) {
    const transcriptActive = mode === "transcript";
    transcriptView.hidden = !transcriptActive;
    discussionView.hidden = transcriptActive;
    transcriptView.dataset.active = String(transcriptActive);
    discussionView.dataset.active = String(!transcriptActive);
    transcriptBtn.setAttribute("aria-selected", String(transcriptActive));
    discussionBtn.setAttribute("aria-selected", String(!transcriptActive));
    if (dispatch) {
      document.dispatchEvent(
        new CustomEvent("modechange", { detail: { mode } }),
      );
    }
  }

  function onClick(mode) {
    return () => {
      const current = transcriptView.dataset.active === "true"
        ? "transcript"
        : "discussion";
      if (mode === current) return;
      writeStoredMode(mode);
      applyMode(mode);
    };
  }

  // Re-hydrate the active view from localStorage *before* wiring the
  // listeners so the first paint already reflects the persisted mode
  // (no flash of the wrong tab being selected).
  const initial = readStoredMode();
  applyMode(initial, { dispatch: false });

  transcriptBtn.addEventListener("click", onClick("transcript"));
  discussionBtn.addEventListener("click", onClick("discussion"));

  // Export a tiny read-only handle for chat.js to detect re-entries
  // (the `modechange` event already covers this, but the helper is
  // useful in tests / debugging).
  globalThis.__nagentMode = {
    current: () => (transcriptView.dataset.active === "true" ? "transcript" : "discussion"),
  };
})();
