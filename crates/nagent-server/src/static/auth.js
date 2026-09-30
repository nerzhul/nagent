// ---- Authentication portal + app-shell mount lifecycle ---------------------
//
// This module is the only script in the document body that lives
// OUTSIDE the `#app-shell-template`. It runs first, probes
// `/api/me`, and decides what the user sees:
//
//   - 200 (authenticated): clone `#app-shell-template` into
//     `#app-root` so the chat/voice UI is mounted. The cloned
//     `app.js` then renders the auth pill and owns session state
//     for the rest of the page's lifetime.
//   - 404 (auth subsystem disabled on the server): clone the
//     template. The cloned `app.js` hides the auth pill (because
//     `authState.probed === false`) and the pre-PR1 trust
//     boundary holds.
//   - 401 (auth enabled, anonymous): do NOT clone the template.
//     Open the login modal in "forced" mode (Cancel hidden, ESC
//     re-opens). The chat/voice controls are genuinely absent
//     from the DOM until the user authenticates — a logged-out
//     visitor cannot inspect them in DevTools, cannot click
//     them, and cannot call any protected HTTP endpoint because
//     the server returns 401 for everything outside the public
//     carve-out (`/`, `/static/*`, `/healthz`, `/api/version`,
//     `/api/auth/login/*`).
//
// After the template is cloned, `app.js` takes over the
// in-page UX. The two modules stay in sync via the global
// `window.nagentAuth` object (read by `app.js` for the auth
// pill) and the `nagent:logout` custom event (dispatched by
// `app.js` when the user clicks "Sign out", causing this
// module to unmount the template and pop the modal back up).

const state = {
  /**
   * @type {null | {id:string,email:string,display_name:string,provider:string,csrf_token:string,session_expires_at:string}}
   */
  user: null,
  /** True after the first `/api/me` probe completed (200/401). */
  probed: false,
};

const loginModal = document.getElementById("login-modal");
const loginForm = document.getElementById("login-form");
const loginEmailInput = document.getElementById("login-email");
const loginPasswordInput = document.getElementById("login-password");
const loginError = document.getElementById("login-error");
const loginSubmit = document.getElementById("login-submit");
const loginCancel = document.getElementById("login-cancel");
const appRoot = document.getElementById("app-root");
const appShellTemplate = document.getElementById("app-shell-template");

/** Probe `/api/me` and route to mount or show-modal accordingly. */
async function probe() {
  let r;
  try {
    r = await fetch("/api/me", {
      cache: "no-store",
      credentials: "same-origin",
    });
  } catch (e) {
    // Network error: leave the app shell unmounted. The user sees
    // an empty page until the next reload. The server's `/healthz`
    // endpoint would still be reachable from ops tooling without
    // the session cookie.
    console.warn("auth probe failed:", e);
    return;
  }
  if (r.status === 200) {
    state.user = await r.json();
    state.probed = true;
    mountShell();
  } else if (r.status === 404) {
    // Auth subsystem not configured on the server; show the UI
    // with the auth pill hidden (pre-PR1 trust boundary).
    state.user = null;
    state.probed = false;
    mountShell();
  } else if (r.status === 401) {
    // Auth enabled and anonymous. Do NOT mount the shell — the
    // server would 401 every protected call anyway. Show the
    // forced modal so the user has to log in to see the UI.
    state.user = null;
    state.probed = true;
    showLoginModal(true);
  } else {
    console.warn("unexpected /api/me status:", r.status);
  }
}

/** Clone `#app-shell-template` into `#app-root` (idempotent). */
function mountShell() {
  if (!appRoot || !appShellTemplate) return;
  if (appRoot.firstChild) return; // already mounted
  appRoot.appendChild(appShellTemplate.content.cloneNode(true));
  // Notify the other modules (`chat.js`, `app.js`,
  // `documents.js`) that the app shell is now in the DOM. They
  // use this to defer DOM lookups that previously failed at
  // module top-level because the form / sidebar / chat input
  // lived inside `<template id="app-shell-template">` and were
  // only cloned into `#app-root` once `/api/me` returned.
  window.dispatchEvent(new CustomEvent("app-shell-mounted"));
}

/** Remove the mounted shell. Triggers a fresh login flow. */
function unmountShell() {
  if (!appRoot) return;
  // Replace the mount point with an empty fragment so the next
  // `mountShell()` call re-runs the cloned scripts cleanly.
  appRoot.replaceChildren();
  state.user = null;
  state.probed = true;
}

/**
 * Open the login modal.
 * @param {boolean} force  When true: portal mode (Cancel hidden,
 *   ESC re-opens). Used for the initial anonymous load and after
 *   logout. When false: the user actively clicked "Sign in" and
 *   is allowed to dismiss the modal.
 */
function showLoginModal(force) {
  if (!loginModal) return;
  loginError.setAttribute("hidden", "");
  loginError.textContent = "";
  loginEmailInput.value = state.user?.email ?? "";
  loginPasswordInput.value = "";
  loginSubmit.disabled = false;
  if (force) {
    loginModal.classList.add("login-modal--forced");
    if (loginCancel) loginCancel.hidden = true;
  } else {
    loginModal.classList.remove("login-modal--forced");
    if (loginCancel) loginCancel.hidden = false;
  }
  loginModal.showModal();
  loginEmailInput.focus();
}

/** Close the login modal and clear the forced flag. */
function hideLoginModal() {
  if (!loginModal) return;
  loginModal.classList.remove("login-modal--forced");
  if (loginCancel) loginCancel.hidden = false;
  loginModal.close();
}

if (loginCancel) {
  loginCancel.addEventListener("click", hideLoginModal);
}

// In portal mode, block ESC / backdrop-click dismissals: the
// dialog's `cancel` event fires before the close, so we
// preventDefault() and re-open it.
if (loginModal) {
  loginModal.addEventListener("cancel", (e) => {
    if (loginModal.classList.contains("login-modal--forced") && !state.user) {
      e.preventDefault();
      loginModal.showModal();
      loginEmailInput.focus();
    }
  });
}

if (loginForm) {
  loginForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    loginSubmit.disabled = true;
    loginError.setAttribute("hidden", "");
    loginError.textContent = "";
    try {
      const r = await fetch("/api/auth/login/password", {
        method: "POST",
        headers: { "content-type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({
          email: loginEmailInput.value,
          password: loginPasswordInput.value,
        }),
      });
      if (!r.ok) {
        let msg = `login failed (${r.status})`;
        try {
          const body = await r.json();
          if (body && body.error) msg = body.error;
        } catch (_) {}
        loginError.textContent = msg;
        loginError.removeAttribute("hidden");
        loginSubmit.disabled = false;
        return;
      }
      // Server set the cookie automatically; pull the user payload.
      const body = await r.json();
      state.user = body.user;
      state.probed = true;
      hideLoginModal();
      mountShell();
    } catch (err) {
      loginError.textContent = `network error: ${err}`;
      loginError.removeAttribute("hidden");
      loginSubmit.disabled = false;
    }
  });
}

// `app.js` (inside the template) dispatches this when the user
// clicks "Sign out" on the auth pill. We unmount the shell and
// pop the forced modal so the UI disappears from the DOM again.
window.addEventListener("nagent:logout", () => {
  unmountShell();
  showLoginModal(true);
});

// Expose the state + helpers so `app.js` (loaded by the cloned
// template) can read the current user for the auth pill and
// open the login modal in non-forced mode when the user clicks
// "Sign in" on the pill (an edge case where the shell is mounted
// but the user is anonymous — e.g. a fresh 200 response with
// no user row, or a session that was revoked server-side).
window.nagentAuth = Object.freeze({
  getUser: () => state.user,
  isProbed: () => state.probed,
  showLoginModal: (force = false) => showLoginModal(force),
  hideLoginModal: () => hideLoginModal(),
  /**
   * Headers for a state-changing `fetch()` call (POST/PUT/PATCH/
   * DELETE) to a protected route. The server's `RequireAuth`
   * middleware rejects every mutating request that does not carry
   * the per-session CSRF token (constant-time compared against the
   * `sessions.csrf_token` row). The header name matches the default
   * `auth.csrf_header = "x-csrf-token"` — operators who customise
   * the server-side header name will need to mirror the change in
   * the frontend (the same constant is used in `app.js`'s logout
   * handler).
   *
   * Returns `undefined` when no session is in `state.user`; callers
   * should spread (`{...nagentAuth.csrfHeaders()}`) so the absent
   * case degrades gracefully to "no header added" rather than
   * `undefined` landing in the HeadersInit object.
   */
  csrfHeaders: () => {
    if (!state.user || !state.user.csrf_token) return undefined;
    return { "x-csrf-token": state.user.csrf_token };
  },
});

// Kick off the probe on module load.
probe();
