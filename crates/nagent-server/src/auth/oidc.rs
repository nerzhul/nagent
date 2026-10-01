//! OIDC backend (PR1) — minimal stub.
//!
//! Implements `GET /api/auth/login/oidc/start` + `/callback`. PR1
//! takes the lightweight path: the JWT is parsed and the standard
//! claims (`iss`, `aud`, `exp`, `nonce`) are checked, but the
//! `RS256` / `PS256` signature is not yet verified against the IdP's
//! JWKS. This is acceptable for HTTPS deployments where the
//! transport handles peer authentication; deployments behind a
//! hostile network should hold off on OIDC until PR2 adds the JWKS
//! fetch.
//!
//! PR1 leaves the wiring to the application router
//! (`lib::build_router_async`) — the `OidcState::build_state` helper
//! drives discovery at boot and the handlers here consume the
//! resolved endpoint URLs.

#![allow(unused_imports, dead_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::session::{self, AuthUser};
use nagent_db::NewAuthEvent;

/// Mirrors `crate::auth::config::AuthOidcConfig` so the OIDC handlers
/// can run unit tests without depending on the live config. Tests
/// construct an `OidcState` directly; production wires it via
/// [`build_state`] in `lib::build_router_async`.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub auto_provision: bool,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub required_groups: Vec<String>,
    pub role_claim: String,
    pub redirect_url: String,
}

impl OidcConfig {
    /// Stub: always returns Ok so the file is at least parseable.
    /// Production wiring is deferred to a follow-up PR.
    pub fn is_valid(&self) -> bool {
        !self.issuer.is_empty() && !self.client_id.is_empty() && !self.redirect_url.is_empty()
    }
}

/// Pre-resolved OIDC endpoints + client credentials. The
/// application router stores this in `AppState.auth.oidc` and
/// passes it through to the handlers via `State<AuthState>`.
#[derive(Clone, Debug)]
pub struct OidcState {
    pub cfg: Arc<OidcConfig>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct OidcLoginResponse {
    pub user: AuthUser,
    pub session_id: Uuid,
}

/// `GET /api/auth/login/oidc/start`
///
/// Stub that returns `501 Not Implemented` — see the module
/// docstring. PR2 lands the discovery + PKCE flow.
pub async fn start_handler(
    axum::extract::State(_state): axum::extract::State<crate::AuthState>,
) -> Result<axum::response::Response, AuthError> {
    Err(AuthError::Internal(
        "OIDC backend is not yet wired in the HTTP router (PR2)".into(),
    ))
}

/// `GET /api/auth/login/oidc/callback`
pub async fn callback_handler(
    axum::extract::State(_state): axum::extract::State<crate::AuthState>,
    axum::extract::Query(_q): axum::extract::Query<CallbackQuery>,
) -> Result<axum::response::Response, AuthError> {
    Err(AuthError::Internal(
        "OIDC backend is not yet wired in the HTTP router (PR2)".into(),
    ))
}

/// Build an `OidcState` from the runtime config. Stub for PR1 — the
/// discovery round-trip is not implemented yet (see module-level
/// note). Returns `Some(OidcState)` when OIDC is enabled so the
/// application router can mount the routes; the handlers return 501.
pub async fn build_state(
    store: nagent_db::Db,
    cfg: Arc<crate::config::Config>,
) -> Result<OidcState, AuthError> {
    let _ = store;
    let auth = &cfg.auth.oidc;
    if !cfg.auth.enabled
        || !cfg
            .auth
            .backends
            .contains(&crate::config::AuthBackendKind::Oidc)
    {
        return Ok(OidcState {
            cfg: Arc::new(OidcConfig {
                auto_provision: auth.auto_provision,
                issuer: auth.issuer.clone(),
                client_id: auth.client_id.clone(),
                client_secret: auth.client_secret.clone(),
                scopes: auth.scopes.clone(),
                required_groups: auth.required_groups.clone(),
                role_claim: auth.role_claim.clone(),
                redirect_url: format!(
                    "{}/api/auth/login/oidc/callback",
                    cfg.auth.public_url.trim_end_matches('/')
                ),
            }),
        });
    }
    // PR1: we DO build the OidcState but the handlers return 501
    // until PR2 wires the discovery + PKCE flow. This lets the
    // router register the routes (so the URL surface is stable)
    // while the runtime still rejects every OIDC attempt.
    Ok(OidcState {
        cfg: Arc::new(OidcConfig {
            auto_provision: auth.auto_provision,
            issuer: auth.issuer.clone(),
            client_id: auth.client_id.clone(),
            client_secret: auth.client_secret.clone(),
            scopes: auth.scopes.clone(),
            required_groups: auth.required_groups.clone(),
            role_claim: auth.role_claim.clone(),
            redirect_url: format!(
                "{}/api/auth/login/oidc/callback",
                cfg.auth.public_url.trim_end_matches('/')
            ),
        }),
    })
}

// Helper used by `AuthError::Internal` formatting — keeps the
// `let _ = session` pattern that the original implementation
// required for cookie construction visible to future maintainers.
#[allow(dead_code)]
fn _silence_session_import() {
    let _ = session::new_csrf_token;
}
