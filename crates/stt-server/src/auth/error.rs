//! Error types for the auth subsystem.
//!
//! All fallible auth operations return `Result<T, AuthError>`. The
//! axum handlers translate the variants into the appropriate HTTP
//! status code via the `IntoResponse` impl in [`crate::auth::routes`].

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The supplied credentials did not match (wrong password,
    /// unknown email, expired challenge).
    #[error("invalid credentials")]
    InvalidCredentials,
    /// The caller is authenticated but not authorized for the
    /// requested action (e.g. registering a passkey on someone
    /// else's behalf when `self_registration = false`).
    #[error("forbidden")]
    Forbidden,
    /// The request violated a server-side invariant (e.g. csrf
    /// mismatch, missing header, malformed payload). Maps to 400.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The caller is unauthenticated. Maps to 401.
    #[error("unauthenticated")]
    Unauthenticated,
    /// The account has been administratively disabled.
    #[error("account disabled")]
    AccountDisabled,
    /// Rate limit tripped. Maps to 429 with a `Retry-After` header.
    #[error("rate limited; retry after {retry_after_secs}s")]
    RateLimited {
        /// Suggested minimum delay before the next attempt.
        retry_after_secs: u64,
    },
    /// The supplied value is already taken (duplicate email on
    /// registration). Maps to 409.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Underlying database error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Cryptographic / hashing error (argon2, base64, etc.).
    #[error("crypto error: {0}")]
    Crypto(String),
    /// Internal misconfiguration — the operator-facing message
    /// says "server misconfigured" so secrets do not leak.
    #[error("internal: {0}")]
    Internal(String),
}

/// Helper for auth handlers: pull the auth store off an
/// `Arc<AppState>`. Refuses with `AuthError::Internal` if the
/// store is not configured — which would be a bug since the auth
/// subtree is only mounted when `auth.enabled = true` (which
/// `auto_bootstrap` enforces by populating `state.auth_store`).
pub fn require_auth_store(
    state: &std::sync::Arc<crate::AppState>,
) -> Result<&crate::auth::AuthStore, AuthError> {
    state.auth_store.as_ref().ok_or_else(|| {
        AuthError::Internal(
            "auth_store is not configured; the auth subtree should not be mounted when auth is disabled".into(),
        )
    })
}

/// Helper for auth handlers: pull the passkey sub-state off an
/// `Arc<AppState>`. Returns `AuthError::Internal` when passkey is
/// not enabled — the passkey routes are only mounted when
/// `state.auth_passkey.is_some()`, so hitting this in a handler
/// indicates a wiring bug.
pub fn require_passkey_state(
    state: &std::sync::Arc<crate::AppState>,
) -> Result<&crate::auth::passkey::PasskeyState, AuthError> {
    state.auth_passkey.as_ref().ok_or_else(|| {
        AuthError::Internal(
            "passkey backend is not configured; the passkey routes should not be mounted when passkey is disabled".into(),
        )
    })
}

/// Helper for auth handlers: pull the OIDC sub-state off an
/// `Arc<AppState>`. Returns `AuthError::Internal` when OIDC is
/// not enabled.
pub fn require_oidc_state(
    state: &std::sync::Arc<crate::AppState>,
) -> Result<&crate::auth::oidc::OidcState, AuthError> {
    state.auth_oidc.as_ref().ok_or_else(|| {
        AuthError::Internal(
            "OIDC backend is not configured; the OIDC routes should not be mounted when OIDC is disabled".into(),
        )
    })
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        use axum::http::header::HeaderValue;
        use axum::Json;
        let (status, body) = match &self {
            AuthError::InvalidCredentials => (StatusCode::UNAUTHORIZED, self.to_string()),
            AuthError::Forbidden => (StatusCode::FORBIDDEN, self.to_string()),
            AuthError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AuthError::Unauthenticated => (StatusCode::UNAUTHORIZED, self.to_string()),
            AuthError::AccountDisabled => (StatusCode::FORBIDDEN, self.to_string()),
            AuthError::RateLimited { retry_after_secs } => {
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({ "error": self.to_string() })),
                )
                    .into_response();
                if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                    resp.headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, v);
                }
                return resp;
            }
            AuthError::Conflict(_) => (StatusCode::CONFLICT, self.to_string()),
            AuthError::Database(e) => {
                // Avoid leaking DB internals (table names, SQL
                // fragments) over the wire; log full detail and
                // surface a generic 500 message instead.
                tracing::error!(error = %e, "auth DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            AuthError::Crypto(_) | AuthError::Internal(_) => {
                tracing::error!(error = %self, "auth internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        if status.is_server_error() {
            (status, body).into_response()
        } else {
            (status, Json(serde_json::json!({ "error": body }))).into_response()
        }
    }
}
