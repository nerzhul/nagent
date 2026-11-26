//! `GET /api/integrations/github/prs` + `GET /api/integrations/github/prs/:owner/:repo/:number`
//! — the **pure-API** backing endpoints for the "PRs" dashboard tab
//! (plan 1795686864347).
//!
//! Unlike the `github_*` chat tools, these endpoints never go
//! through the LLM: the handler reads the caller's PAT from the
//! per-user vault (decrypting the `github.personal_access_token`
//! row) and the configured repo list (`github.dashboard_repos`, a
//! JSON array of `"owner/repo"` strings), then calls the GitHub
//! REST API through the shared `nagent_github::GithubClient` (the
//! same wire client the chat agents use, extracted into its own
//! crate for exactly this purpose).
//!
//! ## Endpoints
//!
//! - `GET /api/integrations/github/prs?repo=&state=open&limit=&enrich=&refresh=` —
//!   aggregates the PRs of every configured repo (first page of
//!   100 per repo), merges them sorted by `updated_at` desc, and
//!   truncates to `limit` (default 50, clamped to
//!   `[1, max_items]`). `?repo=owner/repo` filters to one repo and
//!   must be one of the configured ones (400 otherwise).
//!   `?enrich=true` attaches a per-PR `ci` object (`state`/`detail`/
//!   `url`, from the head SHA's check-runs — one extra cached call
//!   per returned row, hence opt-in). `?refresh=true` skips the
//!   response-cache reads (fresh values still write through) so a
//!   just-merged PR disappears on Refresh; `state=open` also drops
//!   rows flagged `merged` as defense against a stale cached page.
//!   A repo that answers 404/403 lands in `repo_errors` instead of
//!   failing the whole list.
//! - `GET /api/integrations/github/prs/:owner/:repo/:number` —
//!   single PR detail (diff stats + capped body) for the repo /
//!   number pair; the repo must be configured.
//! - `GET /api/integrations/github/owners` — the picker's owner
//!   list: the user's login, the orgs from `GET /user/orgs`, plus
//!   every owner of an accessible repo (from
//!   `GET /user/repos?visibility=all&affiliation=owner,collaborator,organization_member`),
//!   which is what surfaces orgs with a private membership or
//!   outside-collaborator private repos.
//! - `GET /api/integrations/github/owners/:owner/repos` — one owner's
//!   repos (`pushed` desc, up to 10 pages of 100) from that same
//!   paginated `/user/repos?visibility=all` list, filtered by
//!   `owner.login`. Archived repos are excluded unless
//!   `?include_archived=true`.
//!   All calls are access-scoped upstream, so the picker can only
//!   offer repos the saved PAT can read.
//!
//! ## Hardening
//!
//! - Same egress SSRF pre-flight + `[agents.github].allowlist`
//!   as the chat agents (enforced inside `GithubClient::get`).
//! - The token never lands in an audit row, a log line, or an
//!   error response (AGENTS.md §6): `GithubError` variants carry
//!   only upstream statuses / bodies, and the audit rows use the
//!   `github_api_*` kind prefix (distinct from `github_probe_*`).
//! - One `auth_events` row per call (kind `github_api_prs_<outcome>`
//!   / `github_api_pr_<outcome>`), same plumbing as the probes.
//! - Both endpoints are GET (read-only), so no CSRF token is
//!   required; they sit behind `RequireAuth` like every
//!   `/api/integrations*` route.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use uuid::Uuid;

use nagent_db::NewAuthEvent;
use nagent_github::{GithubClient, GithubError};

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::credentials::crypto::{decrypt, EncryptedSecret};
use crate::credentials::key::CredentialsKey;
use crate::probe::ProbeOutcome;

/// Allowed `state` values for the PR list (clamped server-side).
const PR_STATES: &[&str] = &["open", "closed", "all"];

/// Per-route state. The `GithubClient` is built once at router
/// construction so the reqwest connection pool stays warm across
/// dashboard polls.
#[derive(Clone)]
pub struct GithubApiState {
    pub store: nagent_db::Db,
    pub key: Arc<CredentialsKey>,
    pub client: GithubClient,
}

impl std::fmt::Debug for GithubApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubApiState")
            .field("store", &self.store)
            .field("key", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

/// Query string for the per-owner repo list.
#[derive(Debug, Deserialize)]
pub struct OwnerReposQuery {
    /// Include archived repositories (default false — the picker
    /// hides them unless its "show archived" box is ticked).
    #[serde(default)]
    pub include_archived: Option<bool>,
}

/// Query string for the PR list.
#[derive(Debug, Deserialize)]
pub struct ListPrsQuery {
    /// Optional `"owner/repo"` filter; must be one of the
    /// configured `dashboard_repos` entries.
    #[serde(default)]
    pub repo: Option<String>,
    /// `open` (default) / `closed` / `all`.
    #[serde(default)]
    pub state: Option<String>,
    /// Page size after the merge; default 50, clamped to
    /// `[1, max_items]`.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Attach a per-PR `ci` object (`state`/`detail`/`url`) fetched
    /// from the head SHA's check-runs — costs up to one extra call
    /// per returned row (cached per SHA), so it is opt-in.
    #[serde(default)]
    pub enrich: Option<bool>,
    /// Skip the response-cache reads for this call (fresh values
    /// still write through) — the UI's Refresh button sets it so a
    /// just-merged PR disappears immediately.
    #[serde(default)]
    pub refresh: Option<bool>,
}

#[derive(Debug, serde::Serialize)]
struct ApiErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ApiErrorBody { error: message })).into_response()
}

/// One `auth_events` row per dashboard call. Kind prefix
/// `github_api` (distinct from `github_probe` / `github_probe_repo`)
/// so operators can tell setup probes from dashboard reads; the
/// `{call}_{outcome}` shape mirrors the probe schema. Never carries
/// the token.
fn write_api_audit(store: &nagent_db::Db, user_id: Uuid, call: &str, outcome: ProbeOutcome) {
    store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: format!("github_api_{call}_{}", outcome.as_str()),
        provider: "github".to_string(),
        target_service: Some("github".to_string()),
        ip: None,
        user_agent: None,
    });
}

/// Read + decrypt the caller's `github.personal_access_token`
/// vault row. The plaintext only lives in the returned
/// `SecretString` and is handed to `GithubClient::get`, which puts
/// it in the `Authorization` header — never in an error path
/// (AGENTS.md §6).
async fn vault_token(state: &GithubApiState, user_id: Uuid) -> Result<SecretString, Response> {
    let row = state
        .store
        .for_user(user_id)
        .credentials()
        .fetch("github", "personal_access_token")
        .await
        .map_err(|e| {
            tracing::warn!(user = %user_id, error = %e, "github api: vault read failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read the GitHub credentials from the vault".into(),
            )
        })?
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                "the GitHub integration is not configured; save a personal access token in \
                 Settings → Integrations → GitHub first"
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
        tracing::warn!(user = %user_id, error = %e, "github api: vault decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decrypt the GitHub token (the credentials key may have been \
             rotated); re-save the token in Settings → Integrations"
                .into(),
        )
    })
}

/// Read + decrypt + parse the caller's `github.dashboard_repos`
/// vault field: a JSON array of `"owner/repo"` strings. Entries
/// are validated with the shared
/// [`nagent_github::valid_repo_full_name`], deduplicated, and
/// capped at [`nagent_github::MAX_DASHBOARD_REPOS`]. A missing or
/// unparsable field yields an empty list (the dashboard then shows
/// an empty table, not an error).
async fn vault_repos(state: &GithubApiState, user_id: Uuid) -> Result<Vec<String>, Response> {
    let Ok(Some(row)) = state
        .store
        .for_user(user_id)
        .credentials()
        .fetch("github", "dashboard_repos")
        .await
    else {
        return Ok(Vec::new());
    };
    let plaintext = decrypt(
        &state.key,
        &EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github api: dashboard_repos decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decrypt the dashboard repo list; re-save it in Settings → Integrations"
                .into(),
        )
    })?;
    let text = plaintext.expose_secret();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
    let mut repos: Vec<String> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for entry in arr {
            let Some(s) = entry.as_str() else { continue };
            if nagent_github::valid_repo_full_name(s).is_none() {
                continue;
            }
            if !repos.iter().any(|r| r == s) {
                repos.push(s.to_string());
            }
            if repos.len() >= nagent_github::MAX_DASHBOARD_REPOS {
                break;
            }
        }
    }
    Ok(repos)
}

/// Map a wire-client error onto an HTTP response. The messages
/// never contain the token — `GithubError` variants only carry
/// upstream statuses / (truncated) reply bodies.
fn map_github_error(e: GithubError) -> Response {
    match e {
        GithubError::Unauthorized(msg) => error_response(StatusCode::UNAUTHORIZED, msg),
        GithubError::RateLimited { hint } => error_response(StatusCode::TOO_MANY_REQUESTS, hint),
        GithubError::Upstream { status, body } => {
            if status == 404 {
                error_response(StatusCode::NOT_FOUND, format!("not found: {body}"))
            } else {
                error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("GitHub returned HTTP {status}: {body}"),
                )
            }
        }
        GithubError::SandboxDenied(msg) => error_response(StatusCode::FORBIDDEN, msg),
        GithubError::InvalidArguments(msg) => error_response(StatusCode::BAD_REQUEST, msg),
        GithubError::ResponseExceeded { budget } => error_response(
            StatusCode::BAD_GATEWAY,
            format!("GitHub response exceeded the {budget}-byte budget"),
        ),
        GithubError::Transport(msg) => error_response(StatusCode::BAD_GATEWAY, msg),
    }
}

fn audit_outcome(e: &GithubError) -> ProbeOutcome {
    match e {
        GithubError::Unauthorized(_) => ProbeOutcome::AuthRejected,
        GithubError::RateLimited { .. } => ProbeOutcome::UpstreamError,
        GithubError::SandboxDenied(_) => ProbeOutcome::ValidationFailed,
        GithubError::InvalidArguments(_) => ProbeOutcome::BadRequest,
        _ => ProbeOutcome::UpstreamError,
    }
}

/// `GET /api/integrations/github/prs?repo=&state=open&limit=&enrich=&refresh=`
pub async fn list_prs(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Query(q): Query<ListPrsQuery>,
) -> Result<Response, AuthError> {
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    let repos = match vault_repos(&state, user.id).await {
        Ok(r) => r,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "prs", ProbeOutcome::UpstreamError);
            return Ok(resp);
        }
    };
    let state_param = match q.state.as_deref() {
        None | Some("") => "open".to_string(),
        Some(s) if PR_STATES.contains(&s) => s.to_string(),
        Some(other) => {
            write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "`state` must be one of {}, got `{other}`",
                    PR_STATES.join(", ")
                ),
            ));
        }
    };
    let filter = match q.repo.as_deref() {
        None | Some("") => None,
        Some(r) => {
            if nagent_github::valid_repo_full_name(r).is_none() {
                write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    format!("`repo` must be shaped `\"owner/repo\"`, got `{r}`"),
                ));
            }
            if !repos.iter().any(|c| c == r) {
                write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    format!("`{r}` is not in your configured dashboard repositories"),
                ));
            }
            Some(r)
        }
    };
    let query = nagent_github::DashboardPrsQuery {
        repos: &repos,
        filter,
        state: &state_param,
        limit: q.limit.unwrap_or(50),
        enrich: q.enrich.unwrap_or(false),
        refresh: q.refresh.unwrap_or(false),
    };
    match nagent_github::list_dashboard_prs(&state.client, &token, user.id, &query).await {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "prs", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "prs", outcome);
            Ok(map_github_error(e))
        }
    }
}

/// `GET /api/integrations/github/prs/:owner/:repo/:number`
pub async fn get_pr(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Response, AuthError> {
    let full = format!("{owner}/{repo}");
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "pr", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    if nagent_github::valid_repo_full_name(&full).is_none() {
        write_api_audit(&state.store, user.id, "pr", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid `owner/repo` path segment: `{full}`"),
        ));
    }
    let repos = match vault_repos(&state, user.id).await {
        Ok(r) => r,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "pr", ProbeOutcome::UpstreamError);
            return Ok(resp);
        }
    };
    if !repos.iter().any(|r| r == &full) {
        write_api_audit(&state.store, user.id, "pr", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!("`{full}` is not in your configured dashboard repositories"),
        ));
    }
    match nagent_github::get_pr_detail(&state.client, &token, user.id, &owner, &repo, number).await
    {
        Ok(mut data) => {
            // The dashboard detail is not LLM-facing, but cap the
            // body anyway so a huge PR body never lands in a
            // single JSON response unbounded.
            if let Some(body) = data.get("body").and_then(|b| b.as_str()) {
                let capped = nagent_github::truncate(body, state.client.cfg.max_body_bytes);
                if let Some(obj) = data.as_object_mut() {
                    obj.insert("body".into(), serde_json::Value::String(capped));
                }
            }
            write_api_audit(&state.store, user.id, "pr", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": { "pr": data } })).into_response())
        }
        Err(e) => {
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "pr", outcome);
            Ok(map_github_error(e))
        }
    }
}

/// `GET /api/integrations/github/owners` — user login + orgs,
/// backing the repo picker's owner list.
pub async fn list_owners(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "owners", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    match nagent_github::list_owners(&state.client, &token, user.id).await {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "owners", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "owners", outcome);
            Ok(map_github_error(e))
        }
    }
}

/// `GET /api/integrations/github/owners/:owner/repos` — one
/// owner's repos for the picker checkbox list.
pub async fn list_owner_repos(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(owner): Path<String>,
    Query(q): Query<OwnerReposQuery>,
) -> Result<Response, AuthError> {
    let include_archived = q.include_archived.unwrap_or(false);
    if !nagent_github::valid_owner_name(&owner) {
        write_api_audit(
            &state.store,
            user.id,
            "owner_repos",
            ProbeOutcome::BadRequest,
        );
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid owner name `{owner}`"),
        ));
    }
    let token = match vault_token(&state, user.id).await {
        Ok(t) => t,
        Err(resp) => {
            write_api_audit(
                &state.store,
                user.id,
                "owner_repos",
                ProbeOutcome::BadRequest,
            );
            return Ok(resp);
        }
    };
    // Single access-scoped source (`/user/repos?visibility=all&affiliation=…`,
    // paginated up to 10 pages, filtered by owner) — covers own repos,
    // org-member repos and
    // outside-collaborator private repos of orgs the user is not a
    // visible member of. An owner the PAT cannot see yields an empty
    // list (GitHub enforces visibility upstream).
    match nagent_github::list_owner_repos(&state.client, &token, user.id, &owner, include_archived)
        .await
    {
        Ok(data) => {
            write_api_audit(&state.store, user.id, "owner_repos", ProbeOutcome::Ok);
            Ok(Json(serde_json::json!({ "data": data })).into_response())
        }
        Err(e) => {
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "owner_repos", outcome);
            Ok(map_github_error(e))
        }
    }
}

/// Build the GitHub dashboard router subtree. Feature-gated on
/// `github-agent` at the call site, mounted inside the
/// auth-protected subtree next to the setup probe.
pub fn build_github_api_router(state: Arc<crate::AppState>) -> axum::Router<Arc<crate::AppState>> {
    // Same guard as `build_shortcut_api_router`: with no auth /
    // credential resolver (e.g. test apps that mount the full
    // router without wiring credentials) there is no vault to read
    // from, so mount nothing instead of panicking on the key lookup.
    let Some(auth) = state.auth.as_ref() else {
        return axum::Router::new();
    };
    if auth.credential_resolver.is_none() {
        return axum::Router::new();
    }
    let store = auth.store.clone();
    let cfg: nagent_github::GithubConfig = state.config.agents.github.clone().into();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .build()
        .expect("github api client must build");
    let api_state = GithubApiState {
        store,
        key: state.credential_encryption_key(),
        client: GithubClient::new(cfg, http),
    };
    axum::Router::new()
        .route("/api/integrations/github/prs", axum::routing::get(list_prs))
        .route(
            "/api/integrations/github/prs/:owner/:repo/:number",
            axum::routing::get(get_pr),
        )
        .route(
            "/api/integrations/github/owners",
            axum::routing::get(list_owners),
        )
        .route(
            "/api/integrations/github/owners/:owner/repos",
            axum::routing::get(list_owner_repos),
        )
        .with_state(api_state)
}
