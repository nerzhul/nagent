//! `GET/POST/DELETE /api/integrations/github/accounts` — the
//! dedicated CRUD endpoints for the per-user **named GitHub
//! accounts** (plan 1795793328177 §3).
//!
//! The `github.accounts` vault blob holds several tokens, so it is
//! managed **only** through these endpoints — never through the
//! generic `PUT /api/integrations/github/credentials` round-trip
//! and never echoed back (the `ServiceDef` marks the field
//! `managed` + `echo_on_edit = false`; AGENTS.md §6).
//!
//! - `GET …/accounts` — list accounts (name, username, scopes,
//!   is_default) + the saved `default_account`. Tokens absent.
//! - `POST …/accounts` — body `{name, token}`; the name charset is
//!   `[a-z0-9][a-z0-9-_]{0,31}` (`"default"` reserved); the token is
//!   probed with `GET /user` and the returned `login` +
//!   `X-OAuth-Scopes` are captured; the entry is upserted into
//!   `github.accounts` (duplicate name = update; cap
//!   `[agents.github].max_accounts`). Audit kind
//!   `github_account_add_<outcome>` / `github_account_update_<outcome>`.
//! - `DELETE …/accounts/:name` — remove one entry. Refuses
//!   `"default"` (backed by `personal_access_token`) and refuses
//!   the current `default_account` (repoint first). Audit kind
//!   `github_account_delete_<outcome>`.
//!
//! ## Hardening
//!
//! - POST / DELETE are CSRF-protected (`check_csrf`); GET is
//!   read-only and sits behind `RequireAuth` like every
//!   `/api/integrations*` route.
//! - The token from the request body is only ever placed in the
//!   `Authorization` header of the probe call; error bodies and
//!   audit rows carry account **names** and upstream statuses
//!   only — never the token (AGENTS.md §6).
//! - The egress SSRF pre-flight + `[agents.github].allowlist`
//!   runs inside `GithubClient::get`.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use nagent_db::NewAuthEvent;
use nagent_github::{GithubClient, GithubError};

use crate::auth::error::AuthError;
use crate::auth::middleware::check_csrf;
use crate::auth::AuthUser;
use crate::credentials::crypto::{decrypt, encrypt, EncryptedSecret};
use crate::credentials::key::CredentialsKey;
use crate::probe::ProbeOutcome;

/// Per-route state: vault store + key, the shared wire client
/// (built once at router construction), and the accounts cap.
#[derive(Clone)]
pub struct GithubAccountsState {
    pub store: nagent_db::Db,
    pub key: Arc<CredentialsKey>,
    pub client: GithubClient,
}

impl std::fmt::Debug for GithubAccountsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubAccountsState")
            .field("store", &self.store)
            .field("key", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

/// Body of `POST …/accounts`.
#[derive(Debug, Deserialize)]
pub struct AddAccountBody {
    /// Account name — charset `[a-z0-9][a-z0-9-_]{0,31}`,
    /// `"default"` reserved. Never logged.
    pub name: String,
    /// The PAT to probe + store. Never logged, never echoed,
    /// never written to an audit row (AGENTS.md §6).
    pub token: String,
}

/// One account row of the `GET …/accounts` response — token-free.
#[derive(Debug, Serialize)]
pub struct AccountRow {
    pub name: String,
    pub username: Option<String>,
    pub scopes: Option<String>,
    pub is_default: bool,
}

/// `GET …/accounts` response.
#[derive(Debug, Serialize)]
pub struct AccountsResponse {
    pub accounts: Vec<AccountRow>,
    pub default_account: String,
}

/// `POST …/accounts` success response.
#[derive(Debug, Serialize)]
pub struct AddAccountResponse {
    pub ok: bool,
    pub account: AccountRow,
    /// `true` when an existing entry with the same name was
    /// replaced, `false` when a new one was added.
    pub updated: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ErrorBody { error: message })).into_response()
}

/// One `auth_events` row per accounts-endpoint call. Kind prefix
/// `github_account_{add,update,delete}` (distinct from
/// `github_probe_*` / `github_api_*`). Carries account **names**
/// only — never the token (AGENTS.md §6).
fn write_accounts_audit(store: &nagent_db::Db, user_id: Uuid, action: &str, outcome: ProbeOutcome) {
    store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: format!("github_account_{action}_{}", outcome.as_str()),
        provider: "github".to_string(),
        target_service: Some("github".to_string()),
        ip: None,
        user_agent: None,
    });
}

/// Read + decrypt one raw `github` vault field.
// `Response` is axum's error carrier and is large by design; boxing would
// ripple through every handler's `Ok(resp)` arm for no runtime gain on a
// cold error path (same rule as `vault_field` in github_api.rs).
#[allow(clippy::result_large_err)]
async fn vault_field(
    state: &GithubAccountsState,
    user_id: Uuid,
    field: &str,
) -> Result<Option<SecretString>, Response> {
    let Ok(row) = state
        .store
        .for_user(user_id)
        .credentials()
        .fetch("github", field)
        .await
    else {
        return Ok(None);
    };
    let Some(row) = row else {
        return Ok(None);
    };
    let plaintext = decrypt(
        &state.key,
        &EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .map_err(|e| {
        tracing::warn!(user = %user_id, field, error = %e, "github accounts: vault decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decrypt the GitHub credentials (the credentials key may have been \
             rotated); re-save the token in Settings → Integrations"
                .into(),
        )
    })?;
    Ok(Some(plaintext))
}

/// Parse the `github.accounts` blob (missing/empty → empty list).
// `Response` is axum's error carrier and is large by design; boxing
// would ripple through every handler's `Ok(resp)` arm for no runtime
// gain on a cold error path (same rule as `vault_field` above).
#[allow(clippy::result_large_err)]
async fn vault_account_defs(
    state: &GithubAccountsState,
    user_id: Uuid,
) -> Result<Vec<nagent_github::AccountDef>, Response> {
    match vault_field(state, user_id, "accounts").await? {
        None => Ok(Vec::new()),
        Some(blob) => {
            nagent_github::parse_accounts(blob.expose_secret(), state.client.cfg.max_accounts)
                .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))
        }
    }
}

/// The saved `default_account` name (`"default"` when unset).
async fn vault_default_account(state: &GithubAccountsState, user_id: Uuid) -> String {
    match vault_field(state, user_id, "default_account").await {
        Ok(Some(v)) => {
            let s = v.expose_secret().trim().to_string();
            if s.is_empty() {
                nagent_github::DEFAULT_ACCOUNT_NAME.to_string()
            } else {
                s
            }
        }
        _ => nagent_github::DEFAULT_ACCOUNT_NAME.to_string(),
    }
}

/// Persist the `accounts` blob, carrying every other `github`
/// field's ciphertext over verbatim (the DB upsert replaces all
/// rows of the service — same rule as the generic PUT).
// See `vault_field` above for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn write_accounts_blob(
    state: &GithubAccountsState,
    user_id: Uuid,
    blob: &str,
) -> Result<(), Response> {
    let creds = state.store.for_user(user_id).credentials();
    let keys = creds.list_field_keys("github").await.map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github accounts: list_field_keys failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read the GitHub credentials from the vault".into(),
        )
    })?;
    let mut rows: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::with_capacity(keys.len());
    for key in keys {
        if key == "accounts" {
            continue;
        }
        if let Ok(Some(row)) = creds.fetch("github", &key).await {
            rows.push((key, row.nonce, row.ciphertext));
        }
    }
    let sealed = encrypt(&state.key, blob).map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github accounts: encrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encrypt the accounts blob".into(),
        )
    })?;
    rows.push(("accounts".to_string(), sealed.nonce, sealed.ciphertext));
    creds.upsert("github", &rows).await.map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github accounts: upsert failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to persist the GitHub accounts".into(),
        )
    })?;
    Ok(())
}

/// `GET /api/integrations/github/accounts` — token-free account
/// list + the saved default account name.
pub async fn list_accounts(
    State(state): State<GithubAccountsState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let default_name = vault_default_account(&state, user.id).await;
    let username_field = match vault_field(&state, user.id, "username").await {
        Ok(Some(v)) => Some(v.expose_secret().to_string()),
        _ => None,
    };
    let mut accounts: Vec<AccountRow> = Vec::new();
    if vault_field(&state, user.id, "personal_access_token")
        .await
        .ok()
        .flatten()
        .is_some()
    {
        accounts.push(AccountRow {
            name: nagent_github::DEFAULT_ACCOUNT_NAME.to_string(),
            username: username_field,
            scopes: None,
            is_default: default_name == nagent_github::DEFAULT_ACCOUNT_NAME,
        });
    }
    for def in vault_account_defs(&state, user.id).await.map_err(|resp| {
        // Convert the Response into an AuthError-shaped 400 via the
        // handler's Ok(resp) path — simplest is to return it as the
        // response; do that by mapping to a marker. We instead just
        // surface a 400 here.
        let _ = resp;
        AuthError::BadRequest(
            "the saved github.accounts blob is invalid; re-add the accounts".into(),
        )
    })? {
        accounts.push(AccountRow {
            name: def.name.clone(),
            username: def.username.clone(),
            scopes: def.scopes.clone(),
            is_default: def.name == default_name,
        });
    }
    write_accounts_audit(&state.store, user.id, "list", ProbeOutcome::Ok);
    Ok(Json(AccountsResponse {
        accounts,
        default_account: default_name,
    })
    .into_response())
}

/// `POST /api/integrations/github/accounts` — add or update one
/// named account. CSRF-protected.
pub async fn add_account(
    State(state): State<GithubAccountsState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AddAccountBody>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    let name = body.name.trim().to_string();
    if !nagent_github::valid_account_name(&name) {
        write_accounts_audit(&state.store, user.id, "add", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "invalid account name `{name}` (charset [a-z0-9][a-z0-9-_]{{0,31}}; \
                 `\"default\"` is reserved)"
            ),
        ));
    }
    if body.token.trim().is_empty() {
        write_accounts_audit(&state.store, user.id, "add", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "`token` is required".into(),
        ));
    }
    // Probe the token with GET /user; capture login + scopes. The
    // token is only ever placed in the Authorization header.
    let token = SecretString::from(body.token.trim().to_string());
    let (profile, scopes) = match state.client.get_user(&token).await {
        Ok(v) => v,
        Err(e) => {
            let outcome = match &e {
                GithubError::Unauthorized(_) => ProbeOutcome::AuthRejected,
                GithubError::RateLimited { .. } | GithubError::Upstream { .. } => {
                    ProbeOutcome::UpstreamError
                }
                GithubError::SandboxDenied(_) | GithubError::InvalidArguments(_) => {
                    ProbeOutcome::ValidationFailed
                }
                _ => ProbeOutcome::TransportError,
            };
            write_accounts_audit(&state.store, user.id, "add", outcome);
            return Ok(match e {
                GithubError::Unauthorized(msg) => error_response(StatusCode::UNAUTHORIZED, msg),
                GithubError::RateLimited { hint } => {
                    error_response(StatusCode::TOO_MANY_REQUESTS, hint)
                }
                GithubError::Upstream { status, body } => error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("GitHub returned HTTP {status}: {body}"),
                ),
                other => error_response(StatusCode::BAD_GATEWAY, other.to_string()),
            });
        }
    };
    let login = profile
        .get("login")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if login.is_empty() {
        write_accounts_audit(&state.store, user.id, "add", ProbeOutcome::UpstreamError);
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "GitHub /user response has no `login` field".into(),
        ));
    }
    // Upsert into the accounts blob (duplicate name = update).
    let mut defs = vault_account_defs(&state, user.id).await.map_err(|resp| {
        let _ = resp;
        AuthError::BadRequest(
            "the saved github.accounts blob is invalid; re-add the accounts".into(),
        )
    })?;
    let updated = defs.iter().any(|d| d.name == name);
    if !updated && defs.len() >= state.client.cfg.max_accounts {
        write_accounts_audit(&state.store, user.id, "add", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "max_accounts ({}) reached; remove an account before adding another",
                state.client.cfg.max_accounts
            ),
        ));
    }
    let entry = nagent_github::AccountDef {
        name: name.clone(),
        token,
        username: Some(login),
        scopes,
    };
    defs.retain(|d| d.name != name);
    defs.push(entry);
    let blob = nagent_github::serialize_accounts(&defs);
    if let Err(resp) = write_accounts_blob(&state, user.id, &blob).await {
        write_accounts_audit(&state.store, user.id, "add", ProbeOutcome::UpstreamError);
        return Ok(resp);
    }
    let default_name = vault_default_account(&state, user.id).await;
    let action = if updated { "update" } else { "add" };
    write_accounts_audit(&state.store, user.id, action, ProbeOutcome::Ok);
    let is_default = default_name == name;
    let row = AccountRow {
        name,
        username: defs.last().and_then(|d| d.username.clone()),
        scopes: defs.last().and_then(|d| d.scopes.clone()),
        is_default,
    };
    Ok((
        StatusCode::OK,
        Json(AddAccountResponse {
            ok: true,
            account: row,
            updated,
        }),
    )
        .into_response())
}

/// `DELETE /api/integrations/github/accounts/:name` — remove one
/// named account. CSRF-protected. Refuses `"default"` and the
/// current `default_account`.
pub async fn delete_account(
    State(state): State<GithubAccountsState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: axum::http::HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    let name = name.trim().to_string();
    if name == nagent_github::DEFAULT_ACCOUNT_NAME {
        write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "the `\"default\"` account is the personal access token and cannot be deleted \
             here; remove it via the main integration form"
                .into(),
        ));
    }
    if !nagent_github::valid_account_name(&name) && name != nagent_github::DEFAULT_ACCOUNT_NAME {
        // A malformed name simply matches nothing below — still 400.
        write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid account name `{name}`"),
        ));
    }
    let default_name = vault_default_account(&state, user.id).await;
    if name == default_name {
        write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "`{name}` is the current default account; pick another default account \
                 before deleting it"
            ),
        ));
    }
    let mut defs = vault_account_defs(&state, user.id).await.map_err(|resp| {
        let _ = resp;
        AuthError::BadRequest(
            "the saved github.accounts blob is invalid; re-add the accounts".into(),
        )
    })?;
    let before = defs.len();
    defs.retain(|d| d.name != name);
    if defs.len() == before {
        write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::NOT_FOUND,
            format!("unknown GitHub account `{name}`"),
        ));
    }
    let blob = nagent_github::serialize_accounts(&defs);
    if let Err(resp) = write_accounts_blob(&state, user.id, &blob).await {
        write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::UpstreamError);
        return Ok(resp);
    }
    write_accounts_audit(&state.store, user.id, "delete", ProbeOutcome::Ok);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Build the accounts router subtree. Feature-gated on
/// `github-agent` at the call site, mounted inside the
/// auth-protected subtree next to the other GitHub routes.
pub fn build_github_accounts_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    // Same graceful guard as the other GitHub routers: without
    // auth / a credential resolver there is no vault to read.
    let Some(auth) = state.auth.as_ref() else {
        return axum::Router::new();
    };
    if auth.credential_resolver.is_none() {
        return axum::Router::new();
    }
    let cfg: nagent_github::GithubConfig = state.config.agents.github.clone().into();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .build()
        .expect("github accounts client must build");
    let accounts_state = GithubAccountsState {
        store: auth.store.clone(),
        key: state.credential_encryption_key(),
        client: GithubClient::new(cfg, http),
    };
    axum::Router::new()
        .route(
            "/api/integrations/github/accounts",
            axum::routing::get(list_accounts).post(add_account),
        )
        .route(
            "/api/integrations/github/accounts/:name",
            axum::routing::delete(delete_account),
        )
        .with_state(accounts_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_shape_has_no_token_field() {
        // AGENTS.md §6: the error JSON has a single `error`
        // string; nothing else may be added without review.
        let body = ErrorBody {
            error: "invalid account name".into(),
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("token"));
    }

    #[test]
    fn account_row_serializes_without_token() {
        let row = AccountRow {
            name: "client-a".into(),
            username: Some("octo".into()),
            scopes: Some("repo".into()),
            is_default: true,
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains("client-a"));
        assert!(!s.contains("ghp_"), "no token-shaped value may appear");
    }
}
