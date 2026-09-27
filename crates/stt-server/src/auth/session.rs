//! Session row + cookie / bearer parsing + CSRF tokens.
//!
//! The session is the durable source of truth for "who is this
//! request". The HTTP layer carries either:
//!
//! - `Cookie: nagent_session=<id>` for browser clients, OR
//! - `Authorization: Bearer <id>` for `curl` / API clients.
//!
//! In both cases the middleware looks up the `id` in the `sessions`
//! table, checks `expires_at > now()`, and on success injects an
//! [`AuthUser`] into the request extensions.
//!
//! CSRF: every state-changing browser request must also carry an
//! `x-csrf-token` header (configurable via `auth.csrf_header`) whose
//! value matches the per-session `csrf_token`. The token is minted
//! at session creation from 32 bytes of OS RNG. Bearer clients skip
//! the CSRF check because they cannot be tricked into submitting
//! cross-site requests.
//!
//! Per plan D6a, the session is **absolute**: `expires_at` is set
//! at creation (`now() + auth.session_ttl_days`) and never extended
//! by activity. Sliding / idle timeouts can be layered later if a
//! threat model requires them.

#[cfg(feature = "auth")]
use chrono::{DateTime, Utc};
#[cfg(feature = "auth")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "auth")]
use uuid::Uuid;

/// Public-facing identity attached to every authenticated request.
///
/// Lives in `req.extensions_mut()` after [`middleware::require_auth_middleware`].
/// Handlers extract it via the [`AuthUser`] axum extractor (see
/// [`crate::auth::routes::me_handler`]).
#[cfg(feature = "auth")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    /// One of `"local"`, `"oidc:<issuer>"`, `"passkey"`. The UI
    /// surfaces this in the `Logged in as … · via <provider>` pill
    /// so the user knows how they authenticated.
    pub provider: String,
    /// Roles — empty in PR1 (RBAC lands in PR2). Reserved here so
    /// the JSON shape does not have to change when PR2 ships.
    pub roles: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// Per-session CSRF token. Handlers that mutate state must
    /// compare the `x-csrf-token` request header to this value.
    pub csrf_token: String,
    /// Absolute session expiry. Surfaced in the UI so the user
    /// can see when they will be asked to log in again.
    pub session_expires_at: DateTime<Utc>,
}

/// A row from the `sessions` table. Internal — handlers never see
/// the raw row, they get an [`AuthUser`].
#[cfg(feature = "auth")]
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
}

/// Generate a new opaque session id (UUID v4).
#[cfg(feature = "auth")]
pub fn new_session_id() -> Uuid {
    Uuid::new_v4()
}

/// Generate a CSRF token: 32 bytes of OS RNG, hex-encoded.
#[cfg(feature = "auth")]
pub fn new_csrf_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Parse the session id out of a request. Tries the cookie first
/// (`Cookie: nagent_session=<id>`), then the `Authorization: Bearer
/// <id>` header. Returns `None` if neither is present or if both are
/// malformed.
#[cfg(feature = "auth")]
pub fn extract_session_id(headers: &axum::http::HeaderMap, cookie_name: &str) -> Option<Uuid> {
    // 1. Cookie path. We use the `cookie` crate directly rather
    //    than `Cookie::parse` on the raw header because the request
    //    may carry several cookies (CSRF cookie + the session cookie
    //    + analytics cookies on the same domain) and we only care
    //    about the one with our name.
    if let Some(raw) = headers.get(axum::http::header::COOKIE) {
        if let Ok(s) = raw.to_str() {
            for c in s.split(';') {
                let c = c.trim();
                if let Some(rest) = c
                    .strip_prefix(cookie_name)
                    .and_then(|s| s.strip_prefix('='))
                {
                    if let Ok(uuid) = Uuid::parse_str(rest.trim()) {
                        return Some(uuid);
                    }
                }
            }
        }
    }
    // 2. Bearer path.
    if let Some(raw) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = raw.to_str() {
            if let Some(rest) = s
                .strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
            {
                if let Ok(uuid) = Uuid::parse_str(rest.trim()) {
                    return Some(uuid);
                }
            }
        }
    }
    None
}

/// Build the `Set-Cookie` value for a freshly-minted session.
///
/// `secure = false` is the right answer when the operator is on
/// `http://localhost` (see `AuthConfig::cookie_secure`); the
/// browser drops `Secure` cookies set over plain HTTP and dev
/// would silently break. `HttpOnly` is unconditional — the cookie
/// is opaque to JS.
#[cfg(feature = "auth")]
pub fn build_set_cookie(
    cookie_name: &str,
    session_id: Uuid,
    secure: bool,
    max_age_secs: i64,
) -> String {
    // We hand-build the header rather than using the `cookie` crate
    // because (a) the crate's `Cookie` type does not have a stable
    // `Display` impl that matches what we want here, and (b) the
    // surface area is tiny.
    let mut out = String::with_capacity(128);
    out.push_str(cookie_name);
    out.push('=');
    out.push_str(&session_id.hyphenated().to_string());
    out.push_str("; Path=/; HttpOnly; SameSite=Lax");
    if max_age_secs > 0 {
        out.push_str("; Max-Age=");
        out.push_str(&max_age_secs.to_string());
    }
    if secure {
        out.push_str("; Secure");
    }
    out
}

/// Build the `Set-Cookie` value that clears the session cookie.
#[cfg(feature = "auth")]
pub fn build_clear_cookie(cookie_name: &str, secure: bool) -> String {
    let mut out = String::with_capacity(64);
    out.push_str(cookie_name);
    out.push_str("=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        out.push_str("; Secure");
    }
    out
}

/// HTTP methods that require a CSRF token when the request carries
/// a session cookie (bearer requests skip CSRF because they cannot
/// be tricked into cross-site submissions).
#[cfg(feature = "auth")]
pub fn requires_csrf_check(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    )
}

/// Constant-time string comparison for CSRF tokens. Avoids leaking
/// the token length / prefix via the early-exit path of `==`.
#[cfg(feature = "auth")]
pub fn constant_time_eq_str(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(all(test, feature = "auth"))]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_session_id_prefers_cookie_over_bearer() {
        // Both present → cookie wins (browsers may receive a Bearer
        // header in some proxy setups; the cookie is the canonical
        // identity for the browser).
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "nagent_session=11111111-1111-1111-1111-111111111111"
                .parse()
                .unwrap(),
        );
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer 22222222-2222-2222-2222-222222222222"
                .parse()
                .unwrap(),
        );
        let id = extract_session_id(&h, "nagent_session").unwrap();
        assert_eq!(id.to_string(), "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn extract_session_id_falls_back_to_bearer() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer 33333333-3333-3333-3333-333333333333"
                .parse()
                .unwrap(),
        );
        let id = extract_session_id(&h, "nagent_session").unwrap();
        assert_eq!(id.to_string(), "33333333-3333-3333-3333-333333333333");
    }

    #[test]
    fn extract_session_id_returns_none_when_missing() {
        let h = HeaderMap::new();
        assert!(extract_session_id(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_id_rejects_malformed_uuid() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "nagent_session=not-a-uuid".parse().unwrap(),
        );
        assert!(extract_session_id(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_id_ignores_other_cookies() {
        // Other cookies on the same domain (analytics, etc.) must
        // not pollute the session lookup.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "tracking=xyz; nagent_session=44444444-4444-4444-4444-444444444444; lang=en"
                .parse()
                .unwrap(),
        );
        let id = extract_session_id(&h, "nagent_session").unwrap();
        assert_eq!(id.to_string(), "44444444-4444-4444-4444-444444444444");
    }

    #[test]
    fn csrf_token_is_64_hex_chars() {
        let t = new_csrf_token();
        assert_eq!(t.len(), 64, "32 bytes hex-encoded = 64 chars");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Two tokens in a row must not collide.
        assert_ne!(t, new_csrf_token());
    }

    #[test]
    fn constant_time_eq_str_matches() {
        assert!(constant_time_eq_str("abc", "abc"));
        assert!(!constant_time_eq_str("abc", "abd"));
        assert!(!constant_time_eq_str("abc", "abcd"));
    }

    #[test]
    fn set_cookie_contains_required_attrs() {
        let id = Uuid::parse_str("55555555-5555-5555-5555-555555555555").unwrap();
        let s = build_set_cookie("nagent_session", id, true, 3600);
        assert!(s.contains("nagent_session=55555555-5555-5555-5555-555555555555"));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        assert!(s.contains("Max-Age=3600"));
        assert!(s.contains("Secure"));
        assert!(s.contains("Path=/"));
    }

    #[test]
    fn set_cookie_omits_secure_when_insecure() {
        let id = Uuid::nil();
        let s = build_set_cookie("nagent_session", id, false, 0);
        assert!(!s.contains("Secure"));
        assert!(!s.contains("Max-Age="), "Max-Age=0 must be omitted");
    }

    #[test]
    fn clear_cookie_uses_max_age_zero() {
        let s = build_clear_cookie("nagent_session", true);
        assert!(s.contains("Max-Age=0"));
        assert!(s.contains("HttpOnly"));
    }

    #[test]
    fn requires_csrf_check_covers_state_changers() {
        assert!(requires_csrf_check(&axum::http::Method::POST));
        assert!(requires_csrf_check(&axum::http::Method::PUT));
        assert!(requires_csrf_check(&axum::http::Method::PATCH));
        assert!(requires_csrf_check(&axum::http::Method::DELETE));
        assert!(!requires_csrf_check(&axum::http::Method::GET));
        assert!(!requires_csrf_check(&axum::http::Method::HEAD));
        assert!(!requires_csrf_check(&axum::http::Method::OPTIONS));
    }
}
