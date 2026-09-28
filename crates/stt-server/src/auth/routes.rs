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

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::auth::error::require_auth_store;
use crate::auth::error::AuthError;
use crate::auth::middleware::check_csrf;
use crate::auth::session;
use crate::auth::AuthUser;

/// `GET /api/me` — returns the [`AuthUser`] resolved by the
/// `RequireAuth` middleware.
pub async fn me_handler(axum::Extension(user): axum::Extension<AuthUser>) -> Json<AuthUser> {
    Json(user)
}

/// `POST /api/auth/logout`
pub async fn logout_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AuthError> {
    // The middleware would normally have rejected anonymous
    // requests before we get here, but the handler is also called
    // from the per-route test helpers — be defensive.
    let user = crate::auth::middleware::extract_auth_user(
        &headers,
        &crate::auth::middleware::AuthState::new(
            require_auth_store(&state)?.clone(),
            state.config.clone(),
        ),
    )
    .await?
    .ok_or(AuthError::Unauthenticated)?;
    check_csrf(&headers, &user)?;
    require_auth_store(&state)?.delete_session(user.id).await?;
    let cookie = session::build_clear_cookie(
        state.config.auth.cookie_name(),
        state.config.auth.cookie_secure(),
    );
    tracing::info!(
        event = "auth.logout",
        outcome = "ok",
        email = %user.email,
        user_id = %user.id,
        provider = %user.provider,
        "auth logout ok"
    );
    require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
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
