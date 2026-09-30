//! Passkey (WebAuthn) backend (PR1).
//!
//! Implements the two passkey ceremonies:
//!
//! - **Login** (`POST /api/auth/login/passkey/start` + `/finish`):
//!   the start handler returns a challenge JSON the browser turns
//!   into an assertion, the finish handler verifies it, looks up
//!   the matching `passkeys.credential_id`, increments the counter,
//!   and writes a session row.
//! - **Registration** (`POST /api/auth/login/passkey/register/start`
//!   + `/finish`): logged-in only (gated by `RequireAuth`); the
//!     start handler returns a challenge JSON, the finish handler
//!     validates the attestation and stores the new public key.
//!
//! Ceremony state between the two halves is held entirely
//! server-side: the start response carries an opaque `state_token`
//! the browser echoes back to the finish handler, which looks the
//! state up in an in-process `DashMap` (PR1 keeps the passkey
//! ceremony state out of the DB to avoid an extra table). PR2 may
//! move the state to the DB to survive process restarts.
//!
//! Per plan D11 the registration ceremony is open to any
//! logged-in user by default; the operator can disable it with
//! `auth.passkey.self_registration = false`.

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::auth::error::AuthError;
use crate::auth::error::{require_auth_store, require_passkey_state};
use crate::auth::middleware::{check_csrf, extract_auth_user};
use crate::auth::session::{self, AuthUser, SessionSource};
use crate::auth::store::{AuthStore, NewAuthEvent, NewPasskeyRecord};

const CEREMONY_TTL_SECS: i64 = 5 * 60;

/// Shared state for the passkey handlers.
#[derive(Clone)]
pub struct PasskeyState {
    pub store: AuthStore,
    pub cfg: std::sync::Arc<crate::config::Config>,
    pub webauthn: Arc<Webauthn>,
    /// In-memory ceremony state, keyed by the opaque state_token
    /// returned to the browser. Entries auto-expire after
    /// [`CEREMONY_TTL_SECS`] (lazy eviction on the read path).
    pub ceremonies: Arc<DashMap<String, CeremonyEntry>>,
}

impl std::fmt::Debug for PasskeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyState")
            .field("store", &"<store>")
            .field("cfg", &"<cfg>")
            .field("webauthn", &"<WebauthnServer>")
            .field("ceremonies", &self.ceremonies.len())
            .finish()
    }
}

#[derive(Debug)]
pub struct CeremonyEntry {
    pub kind: CeremonyKind,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// For registration: the user being enrolled.
    /// For login: empty (the assertion's credential_id resolves to
    /// the user).
    pub user_id: Option<Uuid>,
    /// Server-side state required to complete the ceremony.
    /// `PasskeyRegistration` for `CeremonyKind::Register`,
    /// `PasskeyAuthentication` for `CeremonyKind::Authenticate`.
    /// Stored as JSON so the `DashMap` value is `Send + Sync`
    /// without needing a `Mutex`.
    pub state_json: String,
}

#[derive(Debug, Clone, Copy)]
pub enum CeremonyKind {
    Register,
    Authenticate,
}

/// Build the [`PasskeyState`]. Runs once at boot. Returns
/// `AuthError::Internal` when the operator's `rp_id` / `origins`
/// configuration is invalid (see plan §"WebAuthn `rp_id` must
/// match the browser's effective domain").
pub fn build_state(
    store: AuthStore,
    cfg: std::sync::Arc<crate::config::Config>,
) -> Result<PasskeyState, AuthError> {
    if cfg.auth.passkey.origins.is_empty() {
        return Err(AuthError::Internal(
            "auth.passkey.origins is empty; cannot build WebAuthn server".into(),
        ));
    }
    // WebauthnBuilder takes a single rp_origin; we pick the first
    // origin. The browser will only attempt the ceremony against an
    // origin that matches one of the entries in
    // `webauthn.get_allowed_origins()` (which we set below).
    let origin_str = cfg.auth.passkey.origins[0].clone();
    let origin = Url::parse(&origin_str)
        .map_err(|e| AuthError::Internal(format!("invalid passkey origin: {e}")))?;
    let rp_id = if cfg.auth.passkey.rp_id.is_empty() {
        origin
            .host_str()
            .ok_or_else(|| AuthError::Internal("passkey rp_id is empty".into()))?
            .to_string()
    } else {
        cfg.auth.passkey.rp_id.clone()
    };
    let mut builder = WebauthnBuilder::new(&rp_id, &origin)
        .map_err(|e| AuthError::Internal(format!("invalid passkey rp_id/origin pair: {e}")))?;
    // The first origin is already configured above; the rest
    // (typically none) are appended one at a time.
    for extra in cfg.auth.passkey.origins.iter().skip(1) {
        if let Ok(u) = Url::parse(extra) {
            builder = builder.append_allowed_origin(&u);
        }
    }
    let webauthn = builder
        .build()
        .map_err(|e| AuthError::Internal(format!("webauthn builder failed: {e}")))?;
    Ok(PasskeyState {
        store,
        cfg,
        webauthn: Arc::new(webauthn),
        ceremonies: Arc::new(DashMap::new()),
    })
}

#[allow(dead_code)]
fn encode_base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---- Registration ceremony -------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StartRegisterResponse {
    pub state_token: String,
    pub challenge_json: serde_json::Value,
}

pub async fn register_start_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let inner = require_passkey_state(&state)?;
    if !state.config.auth.passkey.self_registration {
        return Err(AuthError::Forbidden);
    }
    let auth_state = state
        .auth
        .as_ref()
        .expect("auth must be enabled for passkey_register_start")
        .clone();
    let user = extract_auth_user(&headers, &auth_state)
        .await?
        .ok_or(AuthError::Unauthenticated)?;
    if session::requires_csrf_check(&axum::http::Method::POST) {
        check_csrf(&headers, &user)?;
    }
    let user_uuid: Uuid = user.id;
    let user_unique_id = user.id.to_string();
    let (ccr, skr) = inner
        .webauthn
        .start_passkey_registration(user_uuid, &user_unique_id, &user.display_name, None)
        .map_err(|e| AuthError::Internal(format!("passkey start_register: {e}")))?;
    let state_token = crate::auth::session::new_csrf_token();
    let state_json = serde_json::to_string(&skr)
        .map_err(|e| AuthError::Internal(format!("serialise PasskeyRegistration: {e}")))?;
    inner.ceremonies.insert(
        state_token.clone(),
        CeremonyEntry {
            kind: CeremonyKind::Register,
            created_at: chrono::Utc::now(),
            user_id: Some(user_uuid),
            state_json,
        },
    );
    let challenge_json = serde_json::to_value(&ccr)
        .map_err(|e| AuthError::Internal(format!("serialise ccr: {e}")))?;
    Ok((
        StatusCode::OK,
        Json(StartRegisterResponse {
            state_token,
            challenge_json,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct FinishRegisterRequest {
    pub state_token: String,
    /// The raw `PublicKeyCredential<...>` JSON the browser returned,
    /// base64url-decoded by the browser SDK on the client side and
    /// re-serialised to plain JSON here.
    pub response: serde_json::Value,
}

pub async fn register_finish_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    Json(req): Json<FinishRegisterRequest>,
) -> Result<Response, AuthError> {
    let inner = require_passkey_state(&state)?;
    let (_token, entry) = inner
        .ceremonies
        .remove(&req.state_token)
        .ok_or_else(|| AuthError::BadRequest("unknown or expired state_token".into()))?;
    if !matches!(entry.kind, CeremonyKind::Register) {
        return Err(AuthError::BadRequest(
            "state_token belongs to a different ceremony".into(),
        ));
    }
    if entry.created_at + chrono::Duration::seconds(CEREMONY_TTL_SECS) < chrono::Utc::now() {
        return Err(AuthError::BadRequest(
            "passkey ceremony state expired".into(),
        ));
    }
    let user_id = entry
        .user_id
        .ok_or_else(|| AuthError::Internal("register ceremony without user_id".into()))?;

    let credential: RegisterPublicKeyCredential = serde_json::from_value(req.response)
        .map_err(|e| AuthError::BadRequest(format!("invalid register response: {e}")))?;
    let skr: PasskeyRegistration = serde_json::from_str(&entry.state_json)
        .map_err(|e| AuthError::Internal(format!("deserialise PasskeyRegistration: {e}")))?;
    let passkey = inner
        .webauthn
        .finish_passkey_registration(&credential, &skr)
        .map_err(|e| AuthError::BadRequest(format!("finish_passkey_registration: {e}")))?;
    let cred_id = passkey.cred_id().clone();
    let pk_id = Uuid::new_v4();
    // We serialise BOTH the full `Passkey` and a `DiscoverableKey`
    // view (the same `cred` field wrapped differently) into the
    // `public_key` blob. The `passkey` half is kept for future
    // non-discoverable login flows; the `dk` half is what the
    // current `finish_discoverable_authentication` path consumes.
    // The `CredentialID` is also extracted separately so the
    // `credential_id UNIQUE` lookup at login time is a straight
    // byte comparison.
    let passkey_json = serde_json::to_vec(&passkey)
        .map_err(|e| AuthError::Internal(format!("serialise passkey: {e}")))?;
    let _ = passkey_json; // stored as separate column in a future PR
    require_auth_store(&state)?
        .insert_passkey(NewPasskeyRecord {
            id: pk_id,
            user_id,
            credential_id: cred_id,
            public_key: serde_json::to_vec(&passkey)
                .map_err(|e| AuthError::Internal(format!("serialise passkey: {e}")))?,
            counter: 0,
            transports: String::new(),
            aaguid: None,
        })
        .await?;
    tracing::info!(
        event = "auth.passkey.register",
        outcome = "ok",
        user_id = %user_id,
        passkey_id = %pk_id,
        "passkey register ok"
    );
    require_auth_store(&state)?.record_event(crate::auth::store::NewAuthEvent::auth(
        Some(user_id),
        "passkey_register",
        "passkey",
    ));
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({"passkey_id": pk_id})),
    )
        .into_response())
}

// ---- Login ceremony --------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StartAuthResponse {
    pub state_token: String,
    pub challenge_json: serde_json::Value,
}

pub async fn login_start_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Result<Response, AuthError> {
    let inner = require_passkey_state(&state)?;
    let _ = addr; // available for the audit row + rate-limit hook
    let (car, skr) = inner
        .webauthn
        .start_discoverable_authentication()
        .map_err(|e| {
            AuthError::Internal(format!("passkey start_discoverable_authentication: {e}"))
        })?;
    let state_token = crate::auth::session::new_csrf_token();
    let state_json = serde_json::to_string(&skr)
        .map_err(|e| AuthError::Internal(format!("serialise DiscoverableAuthentication: {e}")))?;
    inner.ceremonies.insert(
        state_token.clone(),
        CeremonyEntry {
            kind: CeremonyKind::Authenticate,
            created_at: chrono::Utc::now(),
            user_id: None,
            state_json,
        },
    );
    let challenge_json = serde_json::to_value(&car)
        .map_err(|e| AuthError::Internal(format!("serialise car: {e}")))?;
    Ok((
        StatusCode::OK,
        Json(StartAuthResponse {
            state_token,
            challenge_json,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
pub struct FinishAuthRequest {
    pub state_token: String,
    pub response: serde_json::Value,
}

pub async fn login_finish_handler(
    State(state): State<std::sync::Arc<crate::AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<FinishAuthRequest>,
) -> Result<Response, AuthError> {
    let ip = addr.ip();
    let inner = require_passkey_state(&state)?;
    let (_token, entry) = match inner.ceremonies.remove(&req.state_token) {
        Some(e) => e,
        None => {
            tracing::warn!(
                event = "auth.passkey.login",
                outcome = "fail",
                reason = "unknown_state_token",
                ip = %ip,
                "passkey login failed (unknown or expired state_token)"
            );
            return Err(AuthError::BadRequest(
                "unknown or expired state_token".into(),
            ));
        }
    };
    if !matches!(entry.kind, CeremonyKind::Authenticate) {
        tracing::warn!(
            event = "auth.passkey.login",
            outcome = "fail",
            reason = "wrong_ceremony_kind",
            ip = %ip,
            "passkey login failed (state_token belongs to a different ceremony)"
        );
        return Err(AuthError::BadRequest(
            "state_token belongs to a different ceremony".into(),
        ));
    }
    if entry.created_at + chrono::Duration::seconds(CEREMONY_TTL_SECS) < chrono::Utc::now() {
        tracing::warn!(
            event = "auth.passkey.login",
            outcome = "fail",
            reason = "ceremony_expired",
            ip = %ip,
            "passkey login failed (ceremony state expired)"
        );
        return Err(AuthError::BadRequest(
            "passkey ceremony state expired".into(),
        ));
    }

    let assertion: PublicKeyCredential = serde_json::from_value(req.response)
        .map_err(|e| AuthError::BadRequest(format!("invalid assertion: {e}")))?;
    let cred_id_bytes = assertion.raw_id.clone();
    let stored = match require_auth_store(&state)?
        .get_passkey_by_credential_id(&cred_id_bytes)
        .await?
    {
        Some(s) => s,
        None => {
            tracing::warn!(
                event = "auth.passkey.login",
                outcome = "fail",
                reason = "unknown_credential_id",
                ip = %ip,
                "passkey login failed (no passkey matches the credential_id)"
            );
            return Err(AuthError::InvalidCredentials);
        }
    };
    // `Passkey` and `DiscoverableKey` are both single-field
    // wrappers around the same private `Credential` struct. Both
    // derive `Serialize + Deserialize` so the JSON we stored at
    // registration time round-trips into a `DiscoverableKey` for
    // the `finish_discoverable_authentication` call without
    // poking at any crate-private fields.
    let dk: DiscoverableKey = serde_json::from_slice(&stored.public_key).map_err(|e| {
        AuthError::Internal(format!(
            "deserialise stored passkey as DiscoverableKey: {e}"
        ))
    })?;
    let sk_auth: DiscoverableAuthentication = serde_json::from_str(&entry.state_json)
        .map_err(|e| AuthError::Internal(format!("deserialise DiscoverableAuthentication: {e}")))?;
    let auth_result = inner
        .webauthn
        .finish_discoverable_authentication(&assertion, sk_auth, &[dk])
        .map_err(|e| {
            tracing::warn!(
                event = "auth.passkey.login",
                outcome = "fail",
                reason = "webauthn_finish_failed",
                user_id = %stored.user_id,
                ip = %ip,
                error = %e,
                "passkey login failed (webauthn finish_discoverable_authentication)"
            );
            AuthError::BadRequest(format!("finish_discoverable_authentication: {e}"))
        })?;
    require_auth_store(&state)?
        .bump_passkey_counter(stored.id, passkey_counter_u32(&auth_result))
        .await?;

    let user = require_auth_store(&state)?
        .get_user_by_id(stored.user_id)
        .await?
        .ok_or_else(|| AuthError::Internal("passkey user disappeared".into()))?;

    let ttl =
        std::time::Duration::from_secs((state.config.auth.session_ttl_days as u64) * 24 * 60 * 60);
    let mut session = require_auth_store(&state)?
        .create_session(user.id, ttl, Some(&ip.to_string()), None)
        .await?;
    // Security plan #7: pull the plaintext token out of the
    // SessionRecord exactly once. After this line the token is
    // either in the cookie, in the JSON body, or in the local
    // `session_token` binding; the DB only sees the hash.
    let session_token = session
        .take_plaintext_token()
        .ok_or_else(|| AuthError::Internal("create_session did not mint a token".into()))?;
    let token_hash_for_log = crate::auth::session::sha256_of(
        &crate::auth::session::decode_session_token(&session_token)
            .expect("token from create_session must round-trip"),
    );
    let token_prefix = token_hash_for_log[..3]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    tracing::info!(
        event = "auth.passkey.login",
        outcome = "ok",
        email = %user.email,
        user_id = %user.id,
        ip = %ip,
        session_hash_prefix = %token_prefix,
        "passkey login ok"
    );
    require_auth_store(&state)?.record_event(NewAuthEvent::auth(
        Some(user.id),
        "login_ok",
        "passkey",
    ));

    let auth_user = AuthUser {
        id: user.id,
        email: user.email.clone(),
        display_name: user.display_name.clone(),
        provider: user.provider.clone(),
        roles: Vec::new(),
        created_at: user.created_at,
        csrf_token: session.csrf_token.clone(),
        session_expires_at: session.expires_at,
        // Security plan #7: mirror the password login path —
        // `session_token_hash` carries the SHA-256 for the
        // logout round-trip, the JSON body carries the
        // plaintext token for the API client.
        session_token_hash: session.token_hash,
        // The login response carries the token in both a cookie
        // and the JSON body (mirrors the password path); a
        // browser session is a cookie source from the CSRF
        // point of view.
        session_source: SessionSource::Cookie,
    };
    let cookie = session::build_set_cookie(
        state.config.auth.cookie_name(),
        &session_token,
        state.config.auth.cookie_secure(),
        ttl.as_secs() as i64,
    );
    let mut response = (
        StatusCode::OK,
        Json(serde_json::json!({
            "user": auth_user,
            "session_token": session_token,
        })),
    )
        .into_response();
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

/// Extract the post-update counter from the auth result. The
/// `AuthenticatorData` carries a 32-bit counter; webauthn-rs
/// exposes it through `AuthenticationResult::counter()` which
/// returns a plain `u32` (alias for `Counter`).
fn passkey_counter_u32(r: &AuthenticationResult) -> u32 {
    r.counter()
}
