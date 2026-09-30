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

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Length (in bytes) of the random session token. 32 bytes = 256
/// bits of entropy, comfortably above the OWASP 2025 guidance
/// for session identifiers.
pub const SESSION_TOKEN_BYTES: usize = 32;

/// Length (in bytes) of the SHA-256 hash that is persisted in the
/// `sessions.token_hash` column. Always equal to the SHA-256
/// output size.
pub const SESSION_HASH_BYTES: usize = 32;

/// SHA-256 hash of a session token. Used as the primary key in
/// `sessions.token_hash`. Held in stack-allocated arrays so the
/// hot path (every authenticated request) never touches the heap.
pub type SessionTokenHash = [u8; SESSION_HASH_BYTES];

/// Public-facing identity attached to every authenticated request.
///
/// Lives in `req.extensions_mut()` after [`middleware::require_auth_middleware`].
/// Handlers extract it via the [`AuthUser`] axum extractor (see
/// [`crate::auth::routes::me_handler`]).
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
    /// SHA-256 hash of the opaque session token. The plaintext
    /// token is only held in the cookie + JSON body — it is
    /// never persisted, never logged, and never reflected back
    /// from a `Lookup` (security plan #7).
    ///
    /// Populated by [`crate::auth::middleware::extract_auth_user`].
    /// `logout_handler` uses this to call
    /// `delete_session_by_token_hash(user.session_token_hash)`
    /// and revoke exactly this session.
    #[serde(skip_serializing)]
    pub session_token_hash: SessionTokenHash,
    /// Which credential authenticated this request. Drives the
    /// CSRF check: state-changing requests authenticated by a
    /// cookie must also carry the matching `x-csrf-token` header;
    /// requests authenticated by a bearer token (no cookie jar,
    /// cannot be tricked into cross-site submissions) skip CSRF.
    /// The pre-fix bug (security plan #17) was that *any*
    /// `Authorization: Bearer …` header — even one that did NOT
    /// authenticate — silenced CSRF for a cookie-authenticated
    /// request, letting an attacker bypass the check with a junk
    /// header on a cross-origin form submission.
    pub session_source: SessionSource,
}

/// Origin of the credential that authenticated a request. Set by
/// [`crate::auth::middleware::extract_auth_user`] from the same
/// header walk that resolves the session id; consumed by
/// [`crate::auth::middleware::check_csrf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSource {
    /// Authenticated via the session cookie. CSRF must be enforced
    /// on state-changing verbs.
    Cookie,
    /// Authenticated via the `Authorization: Bearer …` header.
    /// CSRF does not apply — a bearer client cannot be tricked
    /// into a cross-site request by a third-party form submission.
    Bearer,
}

/// A row from the `sessions` table. Internal — handlers never see
/// the raw row, they get an [`AuthUser`].
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// SHA-256 hash of the opaque token (security plan #7).
    /// Stored in `sessions.token_hash` and used as the primary
    /// key. The plaintext token is never persisted and never
    /// reflected back from a `lookup_session` — it is only
    /// returned at creation time via the `plaintext_token` field,
    /// which the caller is expected to consume immediately (see
    /// [`SessionRecord::take_plaintext_token`]).
    pub token_hash: SessionTokenHash,
    pub user_id: Uuid,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// Plaintext session token — set ONLY by [`crate::auth::store::AuthStore::create_session`]
    /// and ONLY at the moment of creation. Reads (`lookup_*`)
    /// leave this as `None` so a leaked DB row or a stolen
    /// `SessionRecord` from a debug log cannot be used to hijack
    /// a session. The login handler calls
    /// [`SessionRecord::take_plaintext_token`] to move it out and
    /// then forgets it.
    pub plaintext_token: Option<String>,
}

impl SessionRecord {
    /// Move the plaintext session token out of the record. Returns
    /// `None` if the record was produced by a `lookup_*` call
    /// (which never sees the plaintext) or if the token was
    /// already taken.
    pub fn take_plaintext_token(&mut self) -> Option<String> {
        self.plaintext_token.take()
    }
}

/// Generate a new opaque session token.
///
/// Returns the 32-byte OS-RNG token (base64url-no-pad, ready to be
/// placed in a cookie or JSON body) plus the SHA-256 hash of the
/// raw token bytes (the value persisted in `sessions.token_hash`).
///
/// **Security plan #7.** The plaintext token is the only thing the
/// client sees; the hash is the only thing the database stores.
/// A database dump / log reader / log aggregator can no longer
/// recover a usable session id — they only see the SHA-256 of it,
/// which is a one-way function.
pub fn new_session_token() -> (String, SessionTokenHash) {
    use rand::RngCore;
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let hash = sha256_of(&bytes);
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    (encoded, hash)
}

/// SHA-256 of `bytes`. Exposed so callers that already have the
/// raw token (e.g. middleware reading a cookie or bearer header)
/// can hash it without going through `new_session_token`.
pub fn sha256_of(bytes: &[u8]) -> SessionTokenHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hash = [0u8; SESSION_HASH_BYTES];
    hash.copy_from_slice(&out);
    hash
}

/// Base64url-no-pad-decode the wire-format session token into the
/// raw 32 bytes the hash is computed over. Returns `None` if the
/// token is the wrong length or contains invalid characters; the
/// caller should then respond 401.
pub fn decode_session_token(token: &str) -> Option<[u8; SESSION_TOKEN_BYTES]> {
    let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
    if raw.len() != SESSION_TOKEN_BYTES {
        return None;
    }
    let mut out = [0u8; SESSION_TOKEN_BYTES];
    out.copy_from_slice(&raw);
    Some(out)
}

/// Generate a CSRF token: 32 bytes of OS RNG, hex-encoded.
pub fn new_csrf_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Outcome of [`extract_session_token`]. The token (when present)
/// is paired with the [`SessionSource`] so the auth middleware can
/// route the CSRF decision correctly. Security plan #17 fix: the
/// source is now per-resolved-credential, not "any
/// `Authorization: Bearer …` header present on the request".
///
/// Security plan #7: the field is the plaintext token (base64url
/// of the 32 random bytes), not a UUID. The middleware hashes it
/// before it hits the database.
#[derive(Debug)]
pub struct ExtractedSession {
    pub token: String,
    pub source: SessionSource,
}

/// Parse the session token out of a request. Tries the cookie first
/// (`Cookie: nagent_session=<token>`), then the `Authorization:
/// Bearer <token>` header. Returns `None` if neither is present,
/// if both are malformed, or if the token does not decode to the
/// expected 32-byte length.
///
/// The returned [`ExtractedSession`] includes the [`SessionSource`]
/// — the credential that *actually* authenticated the request, not
/// every header present on it. A cookie + a junk `Authorization:
/// Bearer not-a-token` header resolves as `SessionSource::Cookie`,
/// so the CSRF check stays enforced on state-changing verbs
/// (security plan #17).
pub fn extract_session_token(
    headers: &axum::http::HeaderMap,
    cookie_name: &str,
) -> Option<ExtractedSession> {
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
                    let candidate = rest.trim();
                    // Reject anything that isn't base64url-no-pad of
                    // the right length — refuses UUID-format cookies
                    // still in flight from the pre-#7 deployment as
                    // well as arbitrary junk.
                    if decode_session_token(candidate).is_some() {
                        return Some(ExtractedSession {
                            token: candidate.to_string(),
                            source: SessionSource::Cookie,
                        });
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
                let candidate = rest.trim();
                if decode_session_token(candidate).is_some() {
                    return Some(ExtractedSession {
                        token: candidate.to_string(),
                        source: SessionSource::Bearer,
                    });
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
pub fn build_set_cookie(cookie_name: &str, token: &str, secure: bool, max_age_secs: i64) -> String {
    // We hand-build the header rather than using the `cookie` crate
    // because (a) the crate's `Cookie` type does not have a stable
    // `Display` impl that matches what we want here, and (b) the
    // surface area is tiny.
    let mut out = String::with_capacity(128);
    out.push_str(cookie_name);
    out.push('=');
    out.push_str(token);
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn extract_session_token_prefers_cookie_over_bearer() {
        // Both present → cookie wins (browsers may receive a Bearer
        // header in some proxy setups; the cookie is the canonical
        // identity for the browser).
        let (cookie_token, _) = new_session_token();
        let (bearer_token, _) = new_session_token();
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            format!("nagent_session={}", cookie_token).parse().unwrap(),
        );
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", bearer_token).parse().unwrap(),
        );
        let ext = extract_session_token(&h, "nagent_session").unwrap();
        assert_eq!(ext.token, cookie_token);
        assert_eq!(ext.source, SessionSource::Cookie);
    }

    #[test]
    fn extract_session_token_falls_back_to_bearer() {
        let (bearer_token, _) = new_session_token();
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", bearer_token).parse().unwrap(),
        );
        let ext = extract_session_token(&h, "nagent_session").unwrap();
        assert_eq!(ext.token, bearer_token);
        assert_eq!(ext.source, SessionSource::Bearer);
    }

    #[test]
    fn extract_session_token_returns_none_when_missing() {
        let h = HeaderMap::new();
        assert!(extract_session_token(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_token_rejects_malformed_token() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "nagent_session=not-a-real-token!".parse().unwrap(),
        );
        assert!(extract_session_token(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_token_rejects_legacy_uuid_format() {
        // Security plan #7: cookies minted by the pre-#7 deployment
        // were UUIDs in plaintext. They must NOT resolve after the
        // deploy — that would re-enable the session-hijack vector
        // for any still-in-flight cookie.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "nagent_session=11111111-1111-1111-1111-111111111111"
                .parse()
                .unwrap(),
        );
        assert!(extract_session_token(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_token_rejects_wrong_length() {
        // A short base64url string that decodes to < 32 bytes must
        // not authenticate — it would never match a row in the
        // `sessions` table anyway, but the helper rejects it
        // before the DB lookup so a flood of bogus cookies cannot
        // consume bucket tokens.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "nagent_session=AAAA".parse().unwrap(),
        );
        assert!(extract_session_token(&h, "nagent_session").is_none());
    }

    #[test]
    fn extract_session_token_ignores_other_cookies() {
        // Other cookies on the same domain (analytics, etc.) must
        // not pollute the session lookup.
        let (cookie_token, _) = new_session_token();
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            format!("tracking=xyz; nagent_session={}; lang=en", cookie_token)
                .parse()
                .unwrap(),
        );
        let ext = extract_session_token(&h, "nagent_session").unwrap();
        assert_eq!(ext.token, cookie_token);
        assert_eq!(ext.source, SessionSource::Cookie);
    }

    #[test]
    fn extract_session_token_cookie_with_junk_bearer_is_cookie_source() {
        // Security plan #17: a junk `Authorization: Bearer …`
        // header on a cookie-authenticated request must NOT make
        // the session look like a bearer auth. The bearer lookup
        // returns `None` (the value is not a base64url token of
        // the right length), the cookie lookup resolves, and the
        // source is `Cookie` so CSRF stays on.
        let (cookie_token, _) = new_session_token();
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            format!("nagent_session={}", cookie_token).parse().unwrap(),
        );
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer not-a-token".parse().unwrap(),
        );
        let ext = extract_session_token(&h, "nagent_session").unwrap();
        assert_eq!(ext.token, cookie_token);
        assert_eq!(ext.source, SessionSource::Cookie);
    }

    #[test]
    fn new_session_token_is_random_and_verifiable() {
        // Two tokens must not collide and both must round-trip
        // through the decode + sha256 helpers.
        let (t1, h1) = new_session_token();
        let (t2, h2) = new_session_token();
        assert_ne!(t1, t2);
        assert_ne!(h1, h2);
        assert_eq!(t1.len(), 43); // base64url of 32 bytes, no padding
        let raw1 = decode_session_token(&t1).expect("must decode");
        assert_eq!(sha256_of(&raw1), h1);
        let raw2 = decode_session_token(&t2).expect("must decode");
        assert_eq!(sha256_of(&raw2), h2);
    }

    #[test]
    fn build_set_cookie_embeds_token() {
        let (token, _) = new_session_token();
        let header = build_set_cookie("nagent_session", &token, true, 3600);
        assert!(header.starts_with(&format!("nagent_session={};", token)));
        assert!(header.contains("HttpOnly"));
        assert!(header.contains("SameSite=Lax"));
        assert!(header.contains("Max-Age=3600"));
        assert!(header.contains("Secure"));
    }

    #[test]
    fn build_set_cookie_omits_secure_when_false() {
        let (token, _) = new_session_token();
        let header = build_set_cookie("nagent_session", &token, false, 3600);
        assert!(!header.contains("Secure"));
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
        let (token, _) = new_session_token();
        let s = build_set_cookie("nagent_session", &token, true, 3600);
        assert!(s.starts_with(&format!("nagent_session={};", token)));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("SameSite=Lax"));
        assert!(s.contains("Max-Age=3600"));
        assert!(s.contains("Secure"));
        assert!(s.contains("Path=/"));
    }

    #[test]
    fn set_cookie_omits_secure_when_insecure() {
        let (token, _) = new_session_token();
        let s = build_set_cookie("nagent_session", &token, false, 0);
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
