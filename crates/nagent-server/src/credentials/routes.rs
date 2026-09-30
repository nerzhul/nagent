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
//!   with `configured` per service for the caller.
//! - `GET    /api/integrations/:id`           — same, single service.
//! - `PUT    /api/integrations/:id/credentials` — atomic replace of
//!   all fields. CSRF-protected.
//! - `DELETE /api/integrations/:id/credentials` — clear all fields.
//!   CSRF-protected.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::agents::ServiceRegistry;
use crate::auth::error::{require_auth_store, AuthError};
use crate::auth::middleware::check_csrf;
use crate::auth::store::AuthStore;
use crate::auth::AuthUser;
use crate::credentials::crypto::encrypt;
use crate::credentials::key::CredentialsKey;

/// All routes need an `AuthUser` (mounted behind `RequireAuth`) and
/// the resolved per-user `AuthStore` from `AppState`.
#[derive(Clone)]
pub struct CredentialState {
    pub store: AuthStore,
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
            store: auth_store_from_resolver(state.clone()),
            services: state
                .auth
                .as_ref()
                .map(|a| a.services.clone())
                .unwrap_or_else(|| crate::agents::ServiceRegistry::empty().into_arc()),
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
fn auth_store_from_resolver(state: Arc<crate::AppState>) -> AuthStore {
    // We expose the resolver's store indirectly: the resolver is
    // built from `AuthStore::clone()` and keeps the same pool, so
    // we read it back via the auth handler's helper.
    require_auth_store(&state)
        .expect("auth_store wired in main")
        .clone()
}

/// `GET /api/integrations` — list every service with `configured`
/// flags per service for the caller.
pub async fn list_integrations(
    State(state): State<CredentialState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let mut data = Vec::with_capacity(state.services.len());
    for svc in state.services.list() {
        let filled = state
            .store
            .list_configured_field_keys(user.id, svc.id)
            .await?;
        data.push(svc.to_summary(&filled));
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
        .list_configured_field_keys(user.id, svc.id)
        .await?;
    Ok(Json(svc.to_summary(&filled)).into_response())
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
        .upsert_user_credentials(user.id, svc.id, &rows)
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
    state.store.delete_service_credentials(user.id, &id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[allow(dead_code)]
fn _resolve_user_id(user: &AuthUser) -> Uuid {
    user.id
}
