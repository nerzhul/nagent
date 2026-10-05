//! Builder for the auth subtree that `lib::build_router` merges
//! into the main router.
//!
//! The subtree uses `Arc<AppState>` as its axum state (so it can
//! be merged into the protected subtree without `Router<S>` interop)
//! and each handler extracts only what it needs through
//! `FromRef<Arc<AppState>>`. Auth-specific bits are read off the
//! resolved `AuthState` sub-state.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::AppState;

use crate::auth::oidc::{callback_handler as oidc_callback, start_handler as oidc_start};
use crate::auth::passkey::{
    login_finish_handler as passkey_login_finish, login_start_handler as passkey_login_start,
    register_finish_handler as passkey_register_finish,
    register_start_handler as passkey_register_start,
};
use crate::auth::password::{login_handler, register_handler};
use crate::auth::routes::{
    get_preferences_handler, logout_handler, me_handler, put_preferences_handler,
};

#[cfg(feature = "x-agent")]
use crate::oauth::x::{
    callback_handler as x_callback, disconnect_handler as x_disconnect, start_handler as x_start,
};

/// Build the public (anonymous) half of the auth subtree. These
/// routes accept an unauthenticated request — the OIDC callback
/// arrives without a session cookie because the user just came back
/// from the IdP, the password form is sent from a logged-out browser.
///
/// The OIDC and passkey sub-states are pre-built in `app::build_app`
/// and stashed on `AppState.auth`; the OIDC `build_state` (which
/// does discovery) and the passkey builder are sync here so this
/// function can be called from `lib::build_router` without `await`.
pub fn build_public_auth_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let auth_state = state
        .auth
        .clone()
        .expect("auth must be Some when the public auth router is mounted");
    let mut public: Router<Arc<AppState>> = Router::new()
        .route("/api/auth/login/password", post(login_handler))
        .route("/api/auth/login/passkey/start", post(passkey_login_start))
        .route("/api/auth/login/passkey/finish", post(passkey_login_finish))
        .route("/api/auth/password/register", post(register_handler));
    if auth_state.oidc.as_ref().is_some() {
        public = public
            .route("/api/auth/login/oidc/start", get(oidc_start))
            .route("/api/auth/login/oidc/callback", get(oidc_callback));
    }
    // Plan 1790695073418: X OAuth flow. The handlers require an
    // authenticated session (the `start` handler reads the cookie
    // so the resulting tokens bind to the calling user), so the
    // routes are mounted under `RequireAuth` in the protected
    // subtree below. We only mount them when the X OAuth state is
    // `Some` (the operator supplied a `client_id`).
    public.with_state(state)
}

/// Build the protected half of the auth subtree. The caller is
/// expected to wrap this with the `RequireAuth` layer (which lives
/// in `lib::build_router`) so the handler can read `AuthUser` from
/// `axum::Extension`.
pub fn build_protected_auth_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let auth_state = state
        .auth
        .clone()
        .expect("auth must be Some when the protected auth router is mounted");
    let mut protected: Router<Arc<AppState>> = Router::new()
        .route("/api/me", get(me_handler))
        // Per-user UI preferences. Mounted under the same RequireAuth
        // gate as `/api/me` (so anonymous clients get a clean 401
        // rather than a partial state) but routed here rather than
        // in `build_protected_credentials_router` because the
        // credentials subtree is only mounted when the agent
        // framework is enabled — preferences exist regardless.
        .route(
            "/api/me/preferences",
            get(get_preferences_handler).put(put_preferences_handler),
        )
        .route("/api/auth/logout", post(logout_handler));
    if auth_state.passkey.as_ref().is_some() {
        protected = protected
            .route(
                "/api/auth/login/passkey/register/start",
                post(passkey_register_start),
            )
            .route(
                "/api/auth/login/passkey/register/finish",
                post(passkey_register_finish),
            );
    }
    // Plan 1790695073418: X OAuth flow. All three endpoints live
    // under `RequireAuth` so the start handler can read `AuthUser`
    // and bind the resulting tokens to the calling user. The URL
    // surface keeps the `/api/auth/login/x/...` prefix to mirror
    // the existing OIDC + passkey URLs.
    #[cfg(feature = "x-agent")]
    if auth_state.x.is_some() {
        protected = protected
            .route("/api/auth/login/x/start", get(x_start))
            .route("/api/auth/login/x/callback", get(x_callback))
            .route("/api/auth/login/x/disconnect", post(x_disconnect));
    }
    protected.with_state(state)
}
