//! `GET /api/integrations/shortcut/stories` + `GET /api/integrations/shortcut/stories/:id`
//! — the **pure-API** backing endpoints for the "Stories" dashboard tab.
//!
//! Unlike the `shortcut_*` chat tools, these endpoints never go
//! through the LLM: the handler reads the caller's API token from
//! the per-user vault (decrypting the `shortcut.api_token` row),
//! and calls the Shortcut REST API through the shared
//! `nagent_shortcut::ShortcutClient` (the same wire client the
//! chat agents use, extracted into its own crate for exactly this
//! purpose).
//!
//! ## Endpoints
//!
//! - `GET /api/integrations/shortcut/stories?query=&limit=&include_done=`
//!   — `GET /search/stories` projected for the list view. The query
//!   is composed **server-side** and always scoped to the caller:
//!   `owner:<mention_name>` (from the saved `username`) unless the
//!   `query` text already pins an `owner:`, plus `!is:archived`
//!   always, plus `!is:done` unless `include_done=true`. With no
//!   saved `username`, the configured `[agents.shortcut].backlog_query`
//!   is the base. `limit` defaults to 30 and is clamped to
//!   `[1, max_items]`.
//! - `GET /api/integrations/shortcut/stories/:id` —
//!   `GET /stories/{id}` + `/comments`, with the **full**
//!   description (no LLM body cap) projected for the detail view.
//!
//! ## Hardening
//!
//! - Same egress SSRF pre-flight + `[agents.shortcut].allowlist`
//!   as the chat agents (enforced inside `ShortcutClient::get`).
//! - The token never lands in an audit row, a log line, or an
//!   error response (AGENTS.md §6): `ShortcutError` variants carry
//!   only upstream statuses / bodies, and the audit rows use the
//!   `shortcut_api_*` kind prefix.
//! - One `auth_events` row per call (kind `shortcut_api_list_*`
//!   / `shortcut_api_story_*`), same plumbing as the probes.
//! - Both endpoints are GET (read-only), so no CSRF token is
//!   required; they sit behind `RequireAuth` like every
//!   `/api/integrations*` route.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use nagent_db::NewAuthEvent;
use nagent_shortcut::{ShortcutClient, ShortcutError};
use secrecy::SecretString;

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::credentials::crypto::{decrypt, EncryptedSecret};
use crate::credentials::key::CredentialsKey;
use crate::probe::ProbeOutcome;

/// Per-route state. The `ShortcutClient` is built once at router
/// construction so the reqwest connection pool stays warm across
/// dashboard polls.
#[derive(Clone)]
pub struct ShortcutApiState {
    pub store: nagent_db::Db,
    pub key: Arc<CredentialsKey>,
    pub client: ShortcutClient,
}

impl std::fmt::Debug for ShortcutApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShortcutApiState")
            .field("store", &self.store)
            .field("key", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

/// Query string for the story list.
#[derive(Debug, Deserialize)]
pub struct ListStoriesQuery {
    /// Free-form Shortcut search text from the dashboard's search
    /// box. It is ALWAYS scoped to the caller (`owner:<mention_name>`
    /// is prepended unless the text already pins an `owner:`), so a
    /// generic filter like `is:started` can never list other people's
    /// stories.
    #[serde(default)]
    pub query: String,
    /// Page size; default 30, clamped to `[1, max_items]`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Include done (closed) stories. Default false — the dashboard
    /// lists open stories only unless the user explicitly ticks the
    /// "Include done" checkbox. Archived stories are always excluded.
    #[serde(default)]
    pub include_done: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct ApiErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ApiErrorBody { error: message })).into_response()
}

/// One `auth_events` row per dashboard call. Kind prefix
/// `shortcut_api` (distinct from `shortcut_probe`) so operators
/// can tell setup probes from dashboard reads; the `{call}_{outcome}`
/// shape mirrors the probe schema. Never carries the token.
fn write_api_audit(store: &nagent_db::Db, user_id: Uuid, call: &str, outcome: ProbeOutcome) {
    store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: format!("shortcut_api_{call}_{}", outcome.as_str()),
        provider: "shortcut".to_string(),
        target_service: Some("shortcut".to_string()),
        ip: None,
        user_agent: None,
    });
}

/// Read + decrypt the caller's `shortcut.api_token` vault row.
/// The plaintext only lives in the returned `SecretString` and is
/// handed to `ShortcutClient::get`, which puts it in the
/// `Shortcut-Token` header — never in an error path (AGENTS.md §6).
// `Response` is axum's error carrier and is large by design; boxing would
// ripple through every handler's `Ok(resp)` arm for no runtime gain on a
// cold error path.
#[allow(clippy::result_large_err)]
async fn vault_token(state: &ShortcutApiState, user_id: Uuid) -> Result<SecretString, Response> {
    let row = state
        .store
        .for_user(user_id)
        .credentials()
        .fetch("shortcut", "api_token")
        .await
        .map_err(|e| {
            tracing::warn!(user = %user_id, error = %e, "shortcut api: vault read failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read the Shortcut credentials from the vault".into(),
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "the Shortcut integration is not configured; save an API token in \
                 Settings → Integrations → Shortcut first"
                    .into(),
            )
        })?;
    decrypt(
        &state.key,
        &EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "shortcut api: vault decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decrypt the Shortcut API token (the credentials key may have been \
             rotated); re-save the token in Settings → Integrations"
                .into(),
        )
    })
}

/// Read the saved `shortcut.username` (mention name) — used to
/// ground the default `owner:` query. Empty when not saved.
async fn vault_username(state: &ShortcutApiState, user_id: Uuid) -> String {
    let Ok(Some(row)) = state
        .store
        .for_user(user_id)
        .credentials()
        .fetch("shortcut", "username")
        .await
    else {
        return String::new();
    };
    decrypt(
        &state.key,
        &EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .ok()
    .map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string())
    .unwrap_or_default()
}

/// Map a wire-client error onto an HTTP response. The messages
/// never contain the token — `ShortcutError` variants only carry
/// upstream statuses / (truncated) reply bodies.
fn map_shortcut_error(e: ShortcutError) -> Response {
    match e {
        ShortcutError::Unauthorized(msg) => error_response(StatusCode::UNAUTHORIZED, msg),
        ShortcutError::RateLimited { hint } => error_response(StatusCode::TOO_MANY_REQUESTS, hint),
        ShortcutError::Upstream { status, body } => {
            if status == 404 {
                error_response(StatusCode::NOT_FOUND, format!("story not found: {body}"))
            } else {
                error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("Shortcut returned HTTP {status}: {body}"),
                )
            }
        }
        ShortcutError::SandboxDenied(msg) => error_response(StatusCode::FORBIDDEN, msg),
        ShortcutError::InvalidArguments(msg) => error_response(StatusCode::BAD_REQUEST, msg),
        ShortcutError::ResponseExceeded { budget } => error_response(
            StatusCode::BAD_GATEWAY,
            format!("Shortcut response exceeded the {budget}-byte budget"),
        ),
        ShortcutError::Transport(msg) => error_response(StatusCode::BAD_GATEWAY, msg),
    }
}

/// `GET /api/integrations/shortcut/stories?query=&limit=`
pub async fn list_stories(
    State(state): State<ShortcutApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Query(q): Query<ListStoriesQuery>,
) -> Result<Response, AuthError> {
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "list", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    // Compose the final search query server-side so the dashboard can
    // never list other people's stories:
    // - `owner:<saved mention_name>` is always added unless the user
    //   text already pins an `owner:` (power-user escape hatch);
    // - `!is:archived` is always applied;
    // - `!is:done` is applied unless `include_done=true` (the "Include
    //   done" checkbox in the UI);
    // - the user's free-text filter is appended verbatim (ANDed).
    // With no saved username at all, fall back to the configured
    // backlog query as the base.
    let user_query = q.query.trim();
    let username = vault_username(&state, user.id).await;
    let include_done = q.include_done.unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if !username.is_empty() && !user_query.contains("owner:") {
        parts.push(format!("owner:{username}"));
    } else if username.is_empty() {
        parts.push(state.client.cfg.backlog_query.clone());
    }
    parts.push("!is:archived".to_string());
    if !include_done {
        parts.push("!is:done".to_string());
    }
    if !user_query.is_empty() {
        parts.push(user_query.to_string());
    }
    let query = parts.join(" ");
    let limit = q.limit.unwrap_or(30);
    match nagent_shortcut::list_stories(&state.client, &token, user.id, &query, limit).await {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "list", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = match &e {
                ShortcutError::Unauthorized(_) => ProbeOutcome::AuthRejected,
                ShortcutError::RateLimited { .. } => ProbeOutcome::UpstreamError,
                ShortcutError::SandboxDenied(_) => ProbeOutcome::ValidationFailed,
                _ => ProbeOutcome::UpstreamError,
            };
            write_api_audit(&state.store, user.id, "list", outcome);
            Ok(map_shortcut_error(e))
        }
    }
}

/// `GET /api/integrations/shortcut/me` — resolve the vault token's
/// member (`GET /members`, matched by the saved `username`) and
/// return the display identity `{id, name, mention_name}`. Lets the
/// integrations UI show "Connected as @jane" and pre-fill the
/// mention-name field when the service is already configured — the
/// token itself is never echoed (AGENTS.md §6); the mention name is
/// the same non-secret the setup probe already returns. Pure API,
/// never the LLM.
pub async fn whoami(
    State(state): State<ShortcutApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "me", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    let username = vault_username(&state, user.id).await;
    let result = async {
        let raw = state.client.get(&token, "/members", &[]).await?;
        let handle = if username.is_empty() {
            None
        } else {
            Some(username.as_str())
        };
        let member =
            nagent_shortcut::find_member(&raw, handle).ok_or_else(|| ShortcutError::Upstream {
                status: 404,
                body: "no member matches the saved mention name".into(),
            })?;
        Ok::<_, ShortcutError>(serde_json::json!({
            "id": member.get("id"),
            "name": nagent_shortcut::name_of_member(member),
            "mention_name": nagent_shortcut::mention_name_of(member),
        }))
    }
    .await;
    match result {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "me", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = match &e {
                ShortcutError::Unauthorized(_) => ProbeOutcome::AuthRejected,
                ShortcutError::SandboxDenied(_) => ProbeOutcome::ValidationFailed,
                _ => ProbeOutcome::UpstreamError,
            };
            write_api_audit(&state.store, user.id, "me", outcome);
            Ok(map_shortcut_error(e))
        }
    }
}

/// `GET /api/integrations/shortcut/stories/:id`
pub async fn get_story(
    State(state): State<ShortcutApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(story_id): Path<u64>,
) -> Result<Response, AuthError> {
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "story", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    match nagent_shortcut::get_story_detail(&state.client, &token, user.id, story_id).await {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "story", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = match &e {
                ShortcutError::Unauthorized(_) => ProbeOutcome::AuthRejected,
                ShortcutError::InvalidArguments(_) => ProbeOutcome::BadRequest,
                ShortcutError::SandboxDenied(_) => ProbeOutcome::ValidationFailed,
                _ => ProbeOutcome::UpstreamError,
            };
            write_api_audit(&state.store, user.id, "story", outcome);
            Ok(map_shortcut_error(e))
        }
    }
}

/// Build the Shortcut dashboard router subtree. Feature-gated on
/// `shortcut-agent` at the call site, mounted inside the
/// auth-protected subtree next to the setup probe.
pub fn build_shortcut_api_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    // Same guard as `build_protected_credentials_router`: with no
    // auth / credential resolver (e.g. test apps that mount the full
    // router without wiring credentials) there is no vault to read
    // from, so mount nothing instead of panicking on the key lookup.
    let Some(auth) = state.auth.as_ref() else {
        return axum::Router::new();
    };
    if auth.credential_resolver.is_none() {
        return axum::Router::new();
    }
    let store = auth.store.clone();
    let cfg: nagent_shortcut::ShortcutConfig = state.config.agents.shortcut.clone().into();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .build()
        .expect("shortcut api client must build");
    let api_state = ShortcutApiState {
        store,
        key: state.credential_encryption_key(),
        client: ShortcutClient::new(cfg, http),
    };
    axum::Router::new()
        .route("/api/integrations/shortcut/me", axum::routing::get(whoami))
        .route(
            "/api/integrations/shortcut/stories",
            axum::routing::get(list_stories),
        )
        .route(
            "/api/integrations/shortcut/stories/:id",
            axum::routing::get(get_story),
        )
        .with_state(api_state)
}
