//! Builder for the auth subtree that `lib::build_router` merges
//! into the main router. See the module-level docs of
//! `crate::auth` for the route inventory.
//!
//! Every auth handler takes `State<Arc<AppState>>` so the merged
//! router has a single state type (matches the LLM / agents / TTS
//! subtrees). The auth-specific bits (`auth_store`, `auth_oidc`,
//! `auth_passkey`, `auth_rate_limiter`) are read directly from
//! `AppState` by each handler.

use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

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

/// Build the public (anonymous) half of the auth subtree. These
/// routes accept an unauthenticated request — the OIDC callback
/// arrives without a session cookie because the user just came back
/// from the IdP, the password form is sent from a logged-out browser.
///
/// The OIDC and passkey sub-states are pre-built in `main.rs` and
/// stashed on `AppState`; the OIDC `build_state` (which does
/// discovery) and the passkey builder are sync here so this function
/// can be called from `lib::build_router` without `await`.
pub fn build_public_auth_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    // Fail fast with a clear error if the auth subtree is mounted
    // without an auth store — `auto_bootstrap` ensures
    // `state.auth_store.is_some()` whenever `auth.enabled = true`.
    let _ = crate::auth::error::require_auth_store(&state)
        .ok()
        .cloned()
        .unwrap_or_else(|| unreachable!("auth enabled but auth_store missing"));
    let mut public: Router<Arc<AppState>> = Router::new()
        .route("/api/auth/login/password", post(login_handler))
        .route("/api/auth/login/passkey/start", post(passkey_login_start))
        .route("/api/auth/login/passkey/finish", post(passkey_login_finish))
        .route("/api/auth/password/register", post(register_handler));
    if state.auth_oidc.is_some() {
        public = public
            .route("/api/auth/login/oidc/start", get(oidc_start))
            .route("/api/auth/login/oidc/callback", get(oidc_callback));
    }
    public.with_state(state)
}

/// Build the protected half of the auth subtree. The caller is
/// expected to wrap this with the `RequireAuth` layer (which lives
/// in `lib::build_router`) so the handler can read `AuthUser` from
/// `axum::Extension`.
pub fn build_protected_auth_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
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
    if state.auth_passkey.is_some() {
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
    protected.with_state(state)
}
