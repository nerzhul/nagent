//! Local password backend (PR1).
//!
//! Implements `POST /api/auth/login/password` and
//! `POST /api/auth/password/register`. Hashes are argon2id with the
//! OWASP 2025 default parameters (m=19 456 KiB, t=2, p=1) as
//! recommended in plan D8. The store layer (sqlite / postgres) is
//! opaque here — we always go through the [`AuthStore`] enum so
//! the same code path runs against both engines.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

use crate::auth::error::require_auth_store;
use crate::auth::error::AuthError;
use crate::auth::rate_limit::LoginRateLimitDecision;
use crate::auth::session;
use crate::auth::AuthUser;

/// Hash a password with argon2id. Returns the encoded `phc-string`
/// (includes the salt + parameters in a single string), which is
/// what the store layer persists as a BLOB.
pub fn hash_password(
    password: &str,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<Vec<u8>, AuthError> {
    use argon2::Params;
    let params = Params::new(
        memory_kib,
        iterations,
        parallelism,
        // 64-byte output is the OWASP recommendation. The argon2
        // crate's `Params::DEFAULT_OUTPUT_LEN` is also 32; we pick
        // 64 explicitly so an operator who tunes the cost params
        // cannot accidentally downgrade the output size below the
        // NIST-recommended 256-bit minimum.
        Some(64),
    )
    .map_err(|e| AuthError::Crypto(format!("invalid argon2 params: {e}")))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let salt = SaltString::generate(&mut OsRng);
    let hash = argon
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Crypto(format!("argon2 hash failed: {e}")))?;
    Ok(hash.to_string().into_bytes())
}

/// Verify a password against the stored encoded hash. Returns
/// `Ok(true)` on match, `Ok(false)` on mismatch.
pub fn verify_password(password: &str, encoded: &[u8]) -> Result<bool, AuthError> {
    let encoded_str = std::str::from_utf8(encoded)
        .map_err(|e| AuthError::Crypto(format!("stored hash is not utf-8: {e}")))?;
    let parsed = PasswordHash::new(encoded_str)
        .map_err(|e| AuthError::Crypto(format!("stored hash is malformed: {e}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

// ---- HTTP handlers ---------------------------------------------------------

/// `POST /api/auth/login/password` — body:
/// `{ "email": "...", "password": "..." }`.
///
/// Sets the `nagent_session` cookie on success. Bearer clients (no
/// cookie jar) can still POST with `Authorization` absent — the
/// cookie value is also returned in the JSON body so a CLI script
/// can store it and use `Authorization: Bearer <id>` for subsequent
/// calls.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub user: AuthUser,
    /// Echoed so API clients (no cookie jar) can store it.
    pub session_id: Uuid,
}

/// (Reserved) — the password handlers now take
/// `State<Arc<AppState>>` directly (same state type as the
/// LLM / agents / TTS handlers). This alias is kept for any
/// downstream caller that still references the type by name.
pub type PasswordState = std::sync::Arc<crate::AppState>;

pub async fn login_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AuthError> {
    let ip = addr.ip();
    if let LoginRateLimitDecision::Deny { retry_after_secs } =
        state.auth_rate_limiter.check(&body.email, ip)
    {
        tracing::warn!(
            event = "auth.password.login",
            outcome = "rate_limited",
            email = %body.email,
            ip = %ip,
            retry_after_secs,
            "password login rate-limited"
        );
        require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
            user_id: None,
            kind: "login_rate_limited".into(),
            provider: "local".into(),
            ip: Some(ip.to_string()),
            user_agent: None,
        });
        return Err(AuthError::RateLimited { retry_after_secs });
    }

    let user = match require_auth_store(&state)?
        .get_user_by_email(&body.email)
        .await?
    {
        Some(u) if u.password_hash.is_some() => u,
        _ => {
            // Always run argon2 even on a miss, so an attacker
            // cannot distinguish "unknown email" from "wrong
            // password" by timing. Constant-time-ish on the lookup
            // itself (DB lookup dominates either way).
            let _ = verify_password(&body.password, b"$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
            tracing::warn!(
                event = "auth.password.login",
                outcome = "fail",
                reason = "invalid_credentials",
                email = %body.email,
                ip = %ip,
                "password login failed (unknown email or no password hash)"
            );
            require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
                user_id: None,
                kind: "login_fail".into(),
                provider: "local".into(),
                ip: Some(ip.to_string()),
                user_agent: None,
            });
            return Err(AuthError::InvalidCredentials);
        }
    };

    let ok = verify_password(&body.password, user.password_hash.as_deref().unwrap_or(b""))?;
    if !ok {
        tracing::warn!(
            event = "auth.password.login",
            outcome = "fail",
            reason = "invalid_credentials",
            email = %user.email,
            user_id = %user.id,
            ip = %ip,
            "password login failed (wrong password)"
        );
        require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
            user_id: Some(user.id),
            kind: "login_fail".into(),
            provider: "local".into(),
            ip: Some(ip.to_string()),
            user_agent: None,
        });
        return Err(AuthError::InvalidCredentials);
    }

    let ttl =
        std::time::Duration::from_secs((state.config.auth.session_ttl_days as u64) * 24 * 60 * 60);
    let session = require_auth_store(&state)?
        .create_session(user.id, ttl, Some(&ip.to_string()), None)
        .await?;

    state.auth_rate_limiter.reset(&body.email, ip);

    tracing::info!(
        event = "auth.password.login",
        outcome = "ok",
        email = %user.email,
        user_id = %user.id,
        ip = %ip,
        session_id = %session.id,
        "password login ok"
    );
    require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
        user_id: Some(user.id),
        kind: "login_ok".into(),
        provider: "local".into(),
        ip: Some(ip.to_string()),
        user_agent: None,
    });

    let auth_user = AuthUser {
        id: user.id,
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        provider: user.provider.clone(),
        roles: Vec::new(),
        created_at: user.created_at,
        csrf_token: session.csrf_token.clone(),
        session_expires_at: session.expires_at,
    };
    let resp = LoginResponse {
        user: auth_user,
        session_id: session.id,
    };
    let cookie = session::build_set_cookie(
        state.config.auth.cookie_name(),
        session.id,
        state.config.auth.cookie_secure(),
        ttl.as_secs() as i64,
    );
    let mut response = (StatusCode::OK, Json(resp)).into_response();
    response.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        cookie
            .parse()
            .map_err(|e: axum::http::header::InvalidHeaderValue| {
                AuthError::Internal(format!("set-cookie parse: {e}"))
            })?,
    );
    Ok(response)
}

/// `POST /api/auth/password/register` — body:
/// `{ "email": "...", "display_name": "...", "password": "..." }`.
///
/// Requires both an existing session cookie AND
/// `auth.password.allow_registration = true` (PR2 will gate this
/// further with a role check). The minimum password length comes
/// from `auth.password.min_password_length`.
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub user: AuthUser,
}

pub async fn register_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<Response, AuthError> {
    let auth_state = crate::auth::middleware::AuthState::new(
        require_auth_store(&state)?.clone(),
        state.config.clone(),
    );
    let _existing = crate::auth::middleware::extract_auth_user(&headers, &auth_state)
        .await?
        .ok_or(AuthError::Unauthenticated)?;

    if !state.config.auth.password.allow_registration {
        tracing::warn!(
            event = "auth.password.register",
            outcome = "denied",
            reason = "registration_disabled",
            email = %body.email,
            "password register denied (allow_registration = false)"
        );
        return Err(AuthError::Forbidden);
    }
    if body.password.len() < state.config.auth.password.min_password_length {
        tracing::warn!(
            event = "auth.password.register",
            outcome = "fail",
            reason = "password_too_short",
            email = %body.email,
            "password register failed (password too short)"
        );
        return Err(AuthError::BadRequest(format!(
            "password must be at least {} characters",
            state.config.auth.password.min_password_length
        )));
    }

    let hash = hash_password(
        &body.password,
        state.config.auth.password.argon2_memory_kib,
        state.config.auth.password.argon2_iterations,
        state.config.auth.password.argon2_parallelism,
    )?;

    let user_id = require_auth_store(&state)?
        .create_user(&body.email, &body.display_name, "local", Some(&hash))
        .await
        .map_err(|e| {
            tracing::warn!(
                event = "auth.password.register",
                outcome = "fail",
                reason = "create_user_failed",
                email = %body.email,
                error = %e,
                "password register failed"
            );
            e
        })?;

    let ip = addr.ip();
    tracing::info!(
        event = "auth.password.register",
        outcome = "ok",
        email = %body.email,
        user_id = %user_id,
        ip = %ip,
        "password register ok"
    );
    require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent {
        user_id: Some(user_id),
        kind: "register_local".into(),
        provider: "local".into(),
        ip: Some(ip.to_string()),
        user_agent: None,
    });

    let user = require_auth_store(&state)?
        .get_user_by_id(user_id)
        .await?
        .ok_or_else(|| AuthError::Internal("just-created user disappeared".into()))?;

    let resp = RegisterResponse {
        user: AuthUser {
            id: user.id,
            email: user.email.clone(),
            display_name: user.display_name.clone(),
            provider: user.provider.clone(),
            roles: Vec::new(),
            created_at: user.created_at,
            csrf_token: String::new(),
            session_expires_at: chrono::Utc::now(),
        },
    };
    Ok((StatusCode::CREATED, Json(resp)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_roundtrip() {
        let hash = hash_password("hunter2", 19_456, 2, 1).expect("hash must succeed");
        assert!(verify_password("hunter2", &hash).expect("verify"));
        assert!(!verify_password("hunter3", &hash).expect("verify"));
        // Stored as bytes — verify_password accepts a slice.
        assert!(!verify_password("", &hash).expect("verify"));
    }

    #[test]
    fn argon2_two_hashes_of_same_password_differ() {
        // Salting: two hashes of the same password must NOT be
        // equal. This is what defeats rainbow tables.
        let a = hash_password("hunter2", 19_456, 2, 1).unwrap();
        let b = hash_password("hunter2", 19_456, 2, 1).unwrap();
        assert_ne!(a, b);
        let _ = (a, b);
    }

    #[test]
    fn argon2_verify_rejects_garbage_input() {
        // Garbage that does not parse as a PHC string.
        assert!(verify_password("hunter2", b"not-a-phc-string").is_err());
    }
}
