//! Local password backend (PR1).
//!
//! Implements `POST /api/auth/login/password` and
//! `POST /api/auth/password/register`. Hashes are argon2id with the
//! OWASP 2025 default parameters (m=19 456 KiB, t=2, p=1) as
//! recommended in plan D8. The store layer (sqlite / postgres) is
//! opaque here — we always go through the [`AuthStore`] enum so
//! the same code path runs against both engines.

#[cfg(feature = "auth")]
use argon2::password_hash::rand_core::OsRng;
#[cfg(feature = "auth")]
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
#[cfg(feature = "auth")]
use argon2::Argon2;
#[cfg(feature = "auth")]
use axum::extract::{ConnectInfo, State};
#[cfg(feature = "auth")]
use axum::http::{HeaderMap, StatusCode};
#[cfg(feature = "auth")]
use axum::response::{IntoResponse, Response};
#[cfg(feature = "auth")]
use axum::Json;
#[cfg(feature = "auth")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "auth")]
use std::net::SocketAddr;
#[cfg(feature = "auth")]
use uuid::Uuid;

#[cfg(feature = "auth")]
use crate::auth::error::AuthError;
#[cfg(feature = "auth")]
use crate::auth::rate_limit::LoginRateLimitDecision;
#[cfg(feature = "auth")]
use crate::auth::session;
#[cfg(feature = "auth")]
use crate::auth::AuthUser;

/// Hash a password with argon2id. Returns the encoded `phc-string`
/// (includes the salt + parameters in a single string), which is
/// what the store layer persists as a BLOB.
#[cfg(feature = "auth")]
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
#[cfg(feature = "auth")]
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
#[cfg(feature = "auth")]
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[cfg(feature = "auth")]
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub user: AuthUser,
    /// Echoed so API clients (no cookie jar) can store it.
    pub session_id: Uuid,
}

/// Shared state the password handlers need. The router exposes
/// the application-wide `Arc<AppState>` to every auth handler so
/// the merged router has a single state type.
#[cfg(feature = "auth")]
pub type PasswordState = crate::auth::middleware::AuthState;

#[cfg(feature = "auth")]
pub async fn login_handler(
    State(state): State<crate::auth::middleware::AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginRequest>,
) -> Result<Response, AuthError> {
    let ip = addr.ip();
    if let LoginRateLimitDecision::Deny { retry_after_secs } =
        state.rate_limiter.check(&body.email, ip)
    {
        state.store.record_event(crate::auth::store::NewAuthEvent {
            user_id: None,
            kind: "login_rate_limited".into(),
            provider: "local".into(),
            ip: Some(ip.to_string()),
            user_agent: None,
        });
        return Err(AuthError::RateLimited { retry_after_secs });
    }

    let user = match state.store.get_user_by_email(&body.email).await? {
        Some(u) if u.password_hash.is_some() => u,
        _ => {
            // Always run argon2 even on a miss, so an attacker
            // cannot distinguish "unknown email" from "wrong
            // password" by timing. Constant-time-ish on the lookup
            // itself (DB lookup dominates either way).
            let _ = verify_password(&body.password, b"$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
            state.store.record_event(crate::auth::store::NewAuthEvent {
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
        state.store.record_event(crate::auth::store::NewAuthEvent {
            user_id: Some(user.id),
            kind: "login_fail".into(),
            provider: "local".into(),
            ip: Some(ip.to_string()),
            user_agent: None,
        });
        return Err(AuthError::InvalidCredentials);
    }

    let ttl =
        std::time::Duration::from_secs((state.cfg.auth.session_ttl_days as u64) * 24 * 60 * 60);
    let session = state
        .store
        .create_session(user.id, ttl, Some(&ip.to_string()), None)
        .await?;

    state.rate_limiter.reset(&body.email, ip);

    state.store.record_event(crate::auth::store::NewAuthEvent {
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
        state.cfg.auth.cookie_name(),
        session.id,
        state.cfg.auth.cookie_secure(),
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
#[cfg(feature = "auth")]
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[cfg(feature = "auth")]
#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub user: AuthUser,
}

#[cfg(feature = "auth")]
pub async fn register_handler(
    State(state): State<crate::auth::middleware::AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<RegisterRequest>,
) -> Result<Response, AuthError> {
    let auth_state =
        crate::auth::middleware::AuthState::new(state.store.clone(), state.cfg.clone());
    let _existing = crate::auth::middleware::extract_auth_user(&headers, &auth_state)
        .await?
        .ok_or(AuthError::Unauthenticated)?;

    if !state.cfg.auth.password.allow_registration {
        return Err(AuthError::Forbidden);
    }
    if body.password.len() < state.cfg.auth.password.min_password_length {
        return Err(AuthError::BadRequest(format!(
            "password must be at least {} characters",
            state.cfg.auth.password.min_password_length
        )));
    }

    let hash = hash_password(
        &body.password,
        state.cfg.auth.password.argon2_memory_kib,
        state.cfg.auth.password.argon2_iterations,
        state.cfg.auth.password.argon2_parallelism,
    )?;

    let user_id = state
        .store
        .create_user(&body.email, &body.display_name, "local", Some(&hash))
        .await?;

    let ip = addr.ip();
    state.store.record_event(crate::auth::store::NewAuthEvent {
        user_id: Some(user_id),
        kind: "register_local".into(),
        provider: "local".into(),
        ip: Some(ip.to_string()),
        user_agent: None,
    });

    let user = state
        .store
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

#[cfg(all(test, feature = "auth"))]
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
