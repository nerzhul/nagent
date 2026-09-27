//! HTTP routes for the auth subsystem.
//!
//! - `GET  /api/me` — returns the [`AuthUser`] resolved by the
//!   `RequireAuth` middleware. The browser SPA reads this on boot
//!   to decide whether to render the login panel or the chat view.
//! - `POST /api/auth/logout` — deletes the current session and
//!   clears the cookie. Requires both the session cookie AND the
//!   matching CSRF token (constant-time compared).
//!
//! The `/api/auth/login/*` and `/api/auth/password/register`
//! routes live in [`crate::auth::password`], [`crate::auth::oidc`]
//! and [`crate::auth::passkey`]; the router wires them all up.

#[cfg(feature = "auth")]
use axum::extract::State;
#[cfg(feature = "auth")]
use axum::http::{header, StatusCode};
#[cfg(feature = "auth")]
use axum::response::{IntoResponse, Response};
#[cfg(feature = "auth")]
use axum::Json;

#[cfg(feature = "auth")]
use crate::auth::error::AuthError;
#[cfg(feature = "auth")]
use crate::auth::middleware::check_csrf;
#[cfg(feature = "auth")]
use crate::auth::session;
#[cfg(feature = "auth")]
use crate::auth::AuthUser;

/// `GET /api/me` — returns the [`AuthUser`] resolved by the
/// `RequireAuth` middleware.
#[cfg(feature = "auth")]
pub async fn me_handler(axum::Extension(user): axum::Extension<AuthUser>) -> Json<AuthUser> {
    Json(user)
}

/// `POST /api/auth/logout`
#[cfg(feature = "auth")]
pub async fn logout_handler(
    State(state): State<crate::auth::middleware::AuthState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AuthError> {
    // The middleware would normally have rejected anonymous
    // requests before we get here, but the handler is also called
    // from the per-route test helpers — be defensive.
    let user = crate::auth::middleware::extract_auth_user(
        &headers,
        &crate::auth::middleware::AuthState::new(state.store.clone(), state.cfg.clone()),
    )
    .await?
    .ok_or(AuthError::Unauthenticated)?;
    check_csrf(&headers, &user)?;
    state.store.delete_session(user.id).await?;
    let cookie =
        session::build_clear_cookie(state.cfg.auth.cookie_name(), state.cfg.auth.cookie_secure());
    state.store.record_event(crate::auth::store::NewAuthEvent {
        user_id: Some(user.id),
        kind: "logout".into(),
        provider: user.provider.clone(),
        ip: None,
        user_agent: None,
    });
    let mut response = (StatusCode::NO_CONTENT, "").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie
            .parse()
            .map_err(|e: axum::http::header::InvalidHeaderValue| {
                AuthError::Internal(format!("set-cookie parse: {e}"))
            })?,
    );
    Ok(response)
}
