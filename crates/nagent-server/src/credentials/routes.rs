//! HTTP handlers for the `/api/integrations*` family.
//!
//! All routes are mounted under the existing `RequireAuth` middleware
//! (see `auth/router.rs::build_protected_auth_router`). The
//! credential owner is always the session user — there is no
//! `?as=…` parameter and no admin override. Operators can manage
//! users via the existing `stt-server auth` CLI; the credentials
//! themselves are not administrative.
//!
//! Endpoints:
//! - `GET    /api/integrations`              — list every service
//! with `configured` per service for the caller.
//! - `GET    /api/integrations/:id`           — same, single service.
//! - `PUT    /api/integrations/:id/credentials` — atomic replace of
//! all fields. CSRF-protected.
//! - `DELETE /api/integrations/:id/credentials` — clear all fields.
//! CSRF-protected.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::error::{require_auth_store, AuthError};
use crate::auth::middleware::check_csrf;
use nagent_agents::{FieldKind, ServiceDef, ServiceRegistry};
// Plan 4.A: the `AuthStore` shim is on its way out. The
// `CredentialState` exposes the shared `nagent_db::Db` directly
// (its `store: nagent_db::Db` field) and route handlers reach
// per-user scoped views through it.
use crate::auth::AuthUser;
use crate::credentials::crypto::{decrypt, encrypt};
use crate::credentials::key::CredentialsKey;

/// Per-route state. `store` is the shared `nagent_db::Db`; the
/// handlers reach the per-user `credentials` repository through
/// `state.store.credentials.for_user(user.id)`.
#[derive(Clone)]
pub struct CredentialState {
    pub store: nagent_db::Db,
    pub services: Arc<ServiceRegistry>,
    pub key: Arc<CredentialsKey>,
}

impl std::fmt::Debug for CredentialState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialState")
            .field("store", &self.store)
            .field("services", &self.services)
            .field("key", &"<redacted>")
            .finish()
    }
}

/// Build the router subtree. Wired in `lib::build_router` after the
/// auth subtree so every request hits `RequireAuth` first.
pub fn build_protected_credentials_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let cred_state = state
        .auth
        .as_ref()
        .and_then(|a| a.credential_resolver.as_ref())
        .map(|_resolver| CredentialState {
            // The resolver holds the store; for routes we re-resolve via
            // `AppState` so we can also write rows (the resolver only
            // exposes reads). Pulling it off the resolver is safe — the
            // `AuthStore` is `Clone` and shares the same pool.
            store: auth_db(state.clone()),
            services: state
                .auth
                .as_ref()
                .map(|a| a.services.clone())
                .unwrap_or_else(|| nagent_agents::ServiceRegistry::empty().into_arc()),
            // The resolver owns the `CredentialsKey`; reach in via the
            // AppState's separate stash because the routes need to
            // encrypt on PUT (the resolver only decrypts).
            key: state.credential_encryption_key(),
        });
    let Some(cred_state) = cred_state else {
        // No resolver → no credentials routes. Return an empty router
        // so the merge in `build_protected_auth_router` is a no-op.
        return axum::Router::new();
    };
    axum::Router::new()
        .route("/api/integrations", axum::routing::get(list_integrations))
        .route("/api/integrations/:id", axum::routing::get(get_integration))
        .route(
            "/api/integrations/:id/credentials",
            axum::routing::put(put_credentials).delete(delete_credentials),
        )
        .with_state(cred_state)
}

/// Walk the resolver back to its `AuthStore`. Avoids a second field
/// on `AppState`; the resolver already carries it.
fn auth_db(state: Arc<crate::AppState>) -> nagent_db::Db {
    require_auth_store(&state)
        .expect("auth store wired in main")
        .clone()
}

/// Public re-export of [`auth_db`] for sibling modules that need
/// the same `nagent_db::Db` (the CalDAV probe handler reads it
/// to write `auth_events` rows).
pub fn credential_state_db(state: &Arc<crate::AppState>) -> nagent_db::Db {
    auth_db(state.clone())
}

/// Build the `field_key → plaintext` map for every **non-Password**
/// filled field of one service. Used by the GET routes so the
/// integrations UI can echo the saved URL / username / host back
/// to the form on edit. `Password` fields are never read here —
/// the type-level filter is the safety boundary; a future
/// `FieldKind` that requires the same treatment can sit in
/// the same `match` arm.
///
/// A decryption failure on a single field is logged (and skipped)
/// so a single corrupted ciphertext does not 500 the whole
/// `GET /api/integrations` response. The user can re-type the
/// value through the form; the upsert path will overwrite the
/// broken row.
async fn build_plaintext_values(
    svc: &ServiceDef,
    filled: &[String],
    user_id: uuid::Uuid,
    state: &CredentialState,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for f in svc.fields {
        if !matches!(f.kind, FieldKind::Text | FieldKind::Url) {
            continue;
        }
        if !filled.iter().any(|k| k == f.key) {
            continue;
        }
        let Some(row) = state
            .store
            .for_user(user_id)
            .credentials()
            .fetch(svc.id, f.key)
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        let sealed = crate::credentials::crypto::EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        match decrypt(&state.key, &sealed) {
            Ok(secret) => {
                use secrecy::ExposeSecret;
                out.insert(f.key.to_string(), secret.expose_secret().to_string());
            }
            Err(e) => {
                tracing::warn!(
                    service = svc.id,
                    field = f.key,
                    user = %user_id,
                    error = %e,
                    "failed to decrypt non-secret field for GET /api/integrations; \
                     the form will leave the field empty and the user can re-type it"
                );
            }
        }
    }
    out
}

/// `GET /api/integrations` — list every service with `configured`
/// flags per service for the caller.
///
/// The list endpoint intentionally does **not** decrypt any
/// field. The browser only needs the `configured` boolean and
/// the per-field `filled` flag to render the settings page; the
/// saved plaintexts (URL, username, host) are surfaced by
/// [`get_integration`] when the user opens the edit modal for
/// one connector. Decrypting every connector on every list
/// poll would burn CPU on values the UI is not going to read.
pub async fn list_integrations(
    State(state): State<CredentialState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let mut data = Vec::with_capacity(state.services.len());
    for svc in state.services.list() {
        let filled = state
            .store
            .for_user(user.id)
            .credentials()
            .list_field_keys(svc.id)
            .await?;
        data.push(svc.to_summary(&filled, &std::collections::HashMap::new()));
    }
    Ok(Json(serde_json::json!({ "data": data })).into_response())
}

/// `GET /api/integrations/:id` — single service summary. 404 on
/// unknown id.
pub async fn get_integration(
    State(state): State<CredentialState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> Result<Response, AuthError> {
    let Some(svc) = state.services.get(&id) else {
        return Err(AuthError::BadRequest(format!("unknown service: {id}")));
    };
    let filled = state
        .store
        .for_user(user.id)
        .credentials()
        .list_field_keys(svc.id)
        .await?;
    let plaintext = build_plaintext_values(svc, &filled, user.id, &state).await;
    Ok(Json(svc.to_summary(&filled, &plaintext)).into_response())
}

/// Body shape for `PUT /api/integrations/:id/credentials`.
#[derive(Debug, Deserialize)]
pub struct PutCredentialsBody {
    /// Map of `field_key → plaintext`. Unknown keys are rejected
    /// (400). Empty map is a no-op (returns 204 without touching
    /// the DB).
    #[serde(default)]
    pub fields: std::collections::HashMap<String, String>,
}

/// `PUT /api/integrations/:id/credentials` — atomic replace of all
/// fields. CSRF-protected (the middleware enforces it for PUT).
pub async fn put_credentials(
    State(state): State<CredentialState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PutCredentialsBody>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    let Some(svc) = state.services.get(&id) else {
        return Err(AuthError::BadRequest(format!("unknown service: {id}")));
    };
    // Reject any field_key not declared in the registry — protects
    // against typos in the client (a PUT with `pasword` would
    // otherwise silently leave `password` empty forever).
    let declared: std::collections::HashSet<&'static str> =
        svc.fields.iter().map(|f| f.key).collect();
    for key in body.fields.keys() {
        if !declared.contains(key.as_str()) {
            return Err(AuthError::BadRequest(format!(
                "unknown field {key:?} for service {:?}; declared: {:?}",
                svc.id, declared
            )));
        }
    }
    // Encrypt every field up-front so a single key mismatch
    // surfaces before any DB write.
    let mut rows: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::with_capacity(body.fields.len());
    for (field_key, plaintext) in &body.fields {
        let sealed = encrypt(&state.key, plaintext).map_err(|e| {
            AuthError::Internal(format!(
                "encrypt failed for service={} field={}: {e}",
                svc.id, field_key
            ))
        })?;
        rows.push((field_key.clone(), sealed.nonce, sealed.ciphertext));
    }
    state
        .store
        .for_user(user.id)
        .credentials()
        .upsert(svc.id, &rows)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/integrations/:id/credentials` — clear every
/// configured field for the service. CSRF-protected.
pub async fn delete_credentials(
    State(state): State<CredentialState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    if state.services.get(&id).is_none() {
        return Err(AuthError::BadRequest(format!("unknown service: {id}")));
    }
    state
        .store
        .for_user(user.id)
        .credentials()
        .delete_service(&id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[allow(dead_code)]
fn _resolve_user_id(user: &AuthUser) -> Uuid {
    user.id
}
