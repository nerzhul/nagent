// nagent — Keyboard shortcuts + `?` help modal.
//
// Centralises every global shortcut declared in the
// `improvements-product-perf-security.md` plan so the help modal and
// the wiring stay in one place. Per-element shortcuts (e.g. the
// `Enter`/`Ctrl+Enter` inside the chat textarea, `Ctrl+Shift+D` for
// the voice capture) are owned by `chat.js` because they only fire
// while the discussion view is active; this module focuses on the
// shortcuts that must be reachable from anywhere in the page.
//
// Mapping (mirrored by the `#shortcuts-modal` body):
//   ?                  -> open the help modal
//   Esc                -> close the help modal (or stop the in-flight
//                         chat generation, owned by chat.js)
//   Ctrl/Cmd+Shift+R   -> toggle Record in Transcript mode
//   Ctrl/Cmd+Shift+D   -> toggle Record in Discussion mode (chat.js)
//   Ctrl/Cmd+Enter     -> send the current chat message (chat.js)
//   Ctrl/Cmd+S         -> export the current transcript as `.txt`
//
// We register at the `document` level and gate on the active mode
// for mode-specific shortcuts. `preventDefault` is called whenever a
// browser default would otherwise steal the gesture (Ctrl+S save,
// Ctrl+R reload, Ctrl+D bookmark, etc.).

(function () {
  "use strict";

  const $ = (id) => document.getElementById(id);

  const modal       = $("shortcuts-modal");
  const modalClose  = $("shortcuts-modal-close");
  const modalBody   = modal ? modal.querySelector(".shortcuts-modal-body") : null;
  // `audioCapture` is exposed by chat.js for the discussion mode;
  // `recordBtn` lives in index.html and is shared by both modes. We
  // call `recordBtn.click()` rather than poking the AudioCapture
  // directly because the click already runs the container-hidden
  // guard + state-machine validation in `_onButtonClick`.
  const recordBtn   = $("record-btn");

  function isMacLike() {
    // `navigator.platform` is deprecated but still the cheapest
    // signal for "should we render ⌘ or Ctrl?". Treat iPad/iPhone
    // as Mac for the same reason.
    const p = (navigator.platform || "").toLowerCase();
    return p.includes("mac") || p.includes("iphone") || p.includes("ipad");
  }
  const mod = isMacLike() ? "⌘" : "Ctrl";

  function openModal() {
    if (!modal) return;
    modal.removeAttribute("hidden");
    modal.setAttribute("aria-hidden", "false");
    // Move focus inside the dialog so screen readers announce it
    // and Tab/Shift+Tab cycle through the close button.
    (modalClose || modalBody)?.focus?.();
  }

  function closeModal() {
    if (!modal) return;
    modal.setAttribute("hidden", "");
    modal.setAttribute("aria-hidden", "true");
  }

  /// Replace the `<kbd>Ctrl</kbd>` markers in the static HTML with
  /// ⌘ on macOS-like platforms. Done once on boot so the modal
  /// itself stays a static asset (no template strings in the
  /// bundle). Skipped when `isMacLike()` returns false.
  function rewriteMacModifiers() {
    if (!modal || !isMacLike()) return;
    modal.querySelectorAll(".shortcuts-modal-mod").forEach((el) => {
      el.textContent = "\u2318"; // ⌘
    });
  }

  /// True when the keydown target is somewhere a user can type free
  /// text (`<input>`, `<textarea>`, or an element with
  /// `contenteditable`). We do not want to steal printable shortcuts
  /// — especially `?` — from these fields, otherwise typing a
  /// question mark in the chat box pops the help modal over the
  /// message the user is composing. Match is permissive on purpose:
  /// `contenteditable="false"` is the rare case where the field
  /// looks editable but is not, and excluding it would risk
  /// swallowing legitimate keystrokes in future rich-text widgets.
  function isTypingTarget(target) {
    if (!target) return false;
    const tag = target.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return true;
    if (target.isContentEditable) return true;
    return false;
  }

  document.addEventListener("keydown", (e) => {
    // The `?` key is Shift+/ on every common layout. We deliberately
    // accept the bare `?` symbol (it arrives with `shiftKey = true`)
    // so a user with a non-US layout that produces `?` through a
    // different chord still gets the modal. The IME guard makes
    // sure we do not fire while a CJK candidate picker is open.
    //
    // The typing-target guard is the fix for the regression where
    // typing `?` inside the chat textarea popped the help modal
    // over the half-typed message — every printable shortcut here
    // has to yield to a focused editable field.
    if (e.key === "?" && !e.ctrlKey && !e.metaKey && !e.altKey
        && !e.isComposing && !isTypingTarget(e.target)) {
      e.preventDefault();
      openModal();
      return;
    }
    // `Esc` closes the modal first; if it isn't, chat.js already
    // handles stopping an in-flight generation. We do not steal the
    // key from chat.js so both behaviours stay active.
    if (e.key === "Escape" && modal && !modal.hasAttribute("hidden")) {
      // Only act if the chat textarea isn't currently focused: we
      // do not want to swallow the chat-side Esc when the user is
      // typing a message and hits Escape to abort a generation.
      const active = document.activeElement;
      const inTextarea = active && active.tagName === "TEXTAREA";
      if (!inTextarea) {
        e.preventDefault();
        closeModal();
        return;
      }
    }
    // Ctrl/Cmd+S: export current transcript. We call the helper
    // exposed by `app.js` (`__nagentExportTranscript`) instead of
    // dispatching a click on `#download-btn` so the behaviour stays
    // stable even if the dropdown is re-skinned. Skipped while the
    // user is typing in a field — `Cmd+S` inside the chat textarea
    // would otherwise save the transcript and yank focus from the
    // message the user is composing.
    if ((e.ctrlKey || e.metaKey) && !e.shiftKey && !e.altKey
        && (e.key === "s" || e.key === "S")
        && !isTypingTarget(e.target)) {
      e.preventDefault();
      if (typeof globalThis.__nagentExportTranscript === "function") {
        globalThis.__nagentExportTranscript();
      }
      return;
    }
    // Ctrl/Cmd+Shift+R: toggle Record in Transcript mode. We
    // intentionally scope this to the Transcript tab so the
    // shortcut never starts a capture in the wrong view. The
    // existing `Ctrl+Shift+D` does the same for Discussion mode
    // and is wired inside chat.js. Also skipped while typing so
    // the browser's reload gesture stays usable from the chat
    // textarea (where `Cmd+R` would otherwise trigger a
    // Transcript-mode capture from inside a Discussion session).
    if ((e.ctrlKey || e.metaKey) && e.shiftKey && !e.altKey
        && (e.key === "R" || e.key === "r")
        && !isTypingTarget(e.target)) {
      const current = globalThis.__nagentMode?.current?.();
      if (current !== "transcript") return;
      e.preventDefault();
      recordBtn?.click();
      return;
    }
  });

  // Close-on-click on the dimmed backdrop and on the close button.
  // The inner card stops propagation so clicks inside the card do
  // not bubble back up to the backdrop handler.
  if (modal) {
    modal.addEventListener("click", (e) => {
      if (e.target === modal) closeModal();
    });
    modalClose?.addEventListener("click", closeModal);
  }

  // Expose the open/close pair so other modules (e.g. a future
  // header help button) can drive the modal without duplicating
  // the markup knowledge.
  globalThis.__nagentShortcuts = { open: openModal, close: closeModal };

  rewriteMacModifiers();
})();