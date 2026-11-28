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
    /// `account` selector (plan 1795793328177): a configured account
    /// name or `"default"` (the default — the saved
    /// `default_account` when set); `"all"` merges the lists across
    /// every configured account, deduped.
    #[serde(default)]
    pub account: Option<String>,
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
    /// `account` selector (plan 1795793328177): a configured account
    /// name or `"default"`; `"all"` (the default when absent)
    /// aggregates across every configured account, each repo
    /// resolved to the account that can read it.
    #[serde(default)]
    pub account: Option<String>,
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

/// Read + decrypt one raw `github` vault field. Shared by every
/// helper below.
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn vault_field(
    state: &GithubApiState,
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
        tracing::warn!(user = %user_id, field, error = %e, "github api: vault decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to decrypt the GitHub `{field}` field; re-save it in Settings → Integrations"),
        )
    })?;
    Ok(Some(plaintext))
}

/// Every configured account as `(name, token)`, default first
/// (plan 1795793328177): `personal_access_token` as `"default"`,
/// then the parsed `github.accounts` blob (cap `max_accounts`).
/// Errors when neither exists (400 — integration not configured).
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn vault_accounts(
    state: &GithubApiState,
    user_id: Uuid,
) -> Result<Vec<(String, SecretString)>, Response> {
    use secrecy::ExposeSecret;
    let mut out: Vec<(String, SecretString)> = Vec::new();
    if let Some(pat) = vault_field(state, user_id, "personal_access_token").await? {
        out.push((nagent_github::DEFAULT_ACCOUNT_NAME.to_string(), pat));
    }
    if let Some(blob) = vault_field(state, user_id, "accounts").await? {
        let defs =
            nagent_github::parse_accounts(blob.expose_secret(), state.client.cfg.max_accounts)
                .map_err(|e| error_response(StatusCode::BAD_REQUEST, e.to_string()))?;
        for def in defs {
            out.push((def.name, def.token));
        }
    }
    if out.is_empty() {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "the GitHub integration is not configured; save a personal access token in \
             Settings → Integrations → GitHub first"
                .into(),
        ));
    }
    Ok(out)
}

/// The caller's `github.default_account` vault field, defaulting to
/// `"default"`. Names only — never a token.
async fn vault_default_account_name(state: &GithubApiState, user_id: Uuid) -> String {
    use secrecy::ExposeSecret;
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

/// Resolve one account by name (or `"default"`); 400 when unknown.
/// The error lists the configured account **names** only — never
/// tokens (AGENTS.md §6).
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn vault_account_by_name(
    state: &GithubApiState,
    user_id: Uuid,
    name: &str,
) -> Result<(String, SecretString), Response> {
    let accounts = vault_accounts(state, user_id).await?;
    let known = accounts
        .iter()
        .map(|(n, _)| n.clone())
        .collect::<Vec<_>>()
        .join(", ");
    accounts
        .into_iter()
        .find(|(n, _)| n == name)
        .ok_or_else(|| {
            error_response(
                StatusCode::BAD_REQUEST,
                format!("unknown GitHub account `{name}` (configured accounts: {known})"),
            )
        })
}

/// The persisted `github.repo_accounts` map
/// (`"owner/repo" → account name`), parsed leniently (a corrupt blob
/// yields an empty map — live resolution re-derives it).
async fn vault_repo_accounts_map(
    state: &GithubApiState,
    user_id: Uuid,
) -> std::collections::HashMap<String, String> {
    use secrecy::ExposeSecret;
    match vault_field(state, user_id, "repo_accounts").await {
        Ok(Some(blob)) => {
            serde_json::from_str::<std::collections::HashMap<String, String>>(blob.expose_secret())
                .unwrap_or_default()
        }
        _ => std::collections::HashMap::new(),
    }
}

/// Read + decrypt + parse the caller's `github.dashboard_repos`
/// vault field: a JSON array of `"owner/repo"` strings. Entries
/// are validated with the shared
/// [`nagent_github::valid_repo_full_name`], deduplicated, and
/// capped at [`nagent_github::MAX_DASHBOARD_REPOS`]. A missing or
/// unparsable field yields an empty list (the dashboard then shows
/// an empty table, not an error).
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
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

/// `GET /api/integrations/github/prs?repo=&state=open&limit=&enrich=&refresh=&account=`
///
/// `?account=` (plan 1795793328177): absent or `all` aggregates
/// across every configured account (each `dashboard_repos` entry
/// resolved to the account that can read it via the persisted
/// `repo_accounts` map, else live resolution; rows tagged
/// `"account"`); a name or `"default"` restricts the list to that
/// one account.
pub async fn list_prs(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Query(q): Query<ListPrsQuery>,
) -> Result<Response, AuthError> {
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
    let account_param = q
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match account_param {
        // `all` (or absent) — cross-account aggregation.
        None | Some(nagent_github::ALL_ACCOUNTS) => {
            let accounts = match vault_accounts(&state, user.id).await {
                Ok(a) => a,
                Err(resp) => {
                    write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
                    return Ok(resp);
                }
            };
            let repo_accounts = vault_repo_accounts_map(&state, user.id).await;
            let refs: Vec<nagent_github::AccountToken<'_>> = accounts
                .iter()
                .map(|(name, token)| nagent_github::AccountToken { name, token })
                .collect();
            let query = nagent_github::MultiDashboardPrsQuery {
                repos: &repos,
                filter,
                state: &state_param,
                limit: q.limit.unwrap_or(50),
                enrich: q.enrich.unwrap_or(false),
                refresh: q.refresh.unwrap_or(false),
                repo_accounts: Some(&repo_accounts),
            };
            match nagent_github::list_dashboard_prs_multi(&state.client, &refs, user.id, &query)
                .await
            {
                Ok(mut data) => {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert(
                            "account".into(),
                            serde_json::json!(nagent_github::ALL_ACCOUNTS),
                        );
                    }
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
        // One named account.
        Some(name) => {
            let (account_name, token) = match vault_account_by_name(&state, user.id, name).await {
                Ok(a) => a,
                Err(resp) => {
                    write_api_audit(&state.store, user.id, "prs", ProbeOutcome::BadRequest);
                    return Ok(resp);
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
            match nagent_github::list_dashboard_prs(
                &state.client,
                &token,
                user.id,
                &account_name,
                &query,
            )
            .await
            {
                Ok(mut data) => {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("account".into(), serde_json::json!(account_name));
                    }
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
    }
}

/// `GET /api/integrations/github/prs/:owner/:repo/:number`
///
/// The repo is resolved to the account that can read it (persisted
/// `repo_accounts` map, else live probing across accounts — plan
/// 1795793328177), and the response carries the resolved
/// `"account"` name.
pub async fn get_pr(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Response, AuthError> {
    let full = format!("{owner}/{repo}");
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
    // Resolve repo → account across every configured account.
    let accounts = match vault_accounts(&state, user.id).await {
        Ok(a) => a,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "pr", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    let persisted = vault_repo_accounts_map(&state, user.id).await;
    let refs: Vec<nagent_github::AccountToken<'_>> = accounts
        .iter()
        .map(|(name, token)| nagent_github::AccountToken { name, token })
        .collect();
    let (account_name, token) = match nagent_github::resolve_repo_account(
        &state.client,
        &refs,
        user.id,
        &full,
        Some(&persisted),
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "pr", outcome);
            return Ok(map_github_error(e));
        }
    };
    match nagent_github::get_pr_detail(
        &state.client,
        &token,
        user.id,
        &account_name,
        &owner,
        &repo,
        number,
    )
    .await
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
            if let Some(obj) = data.as_object_mut() {
                obj.insert("account".into(), serde_json::json!(account_name));
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

/// `GET /api/integrations/github/owners?account=` — user login +
/// orgs, backing the repo picker's owner list. `?account=` (plan
/// 1795793328177): a name or `"default"` (the default — the saved
/// `default_account` when set); `"all"` merges the owner lists
/// across every configured account, deduped by login.
pub async fn list_owners(
    State(state): State<GithubApiState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Query(q): Query<OwnerReposQuery>,
) -> Result<Response, AuthError> {
    let account_param = q
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let accounts = match vault_accounts(&state, user.id).await {
        Ok(a) => a,
        Err(resp) => {
            write_api_audit(&state.store, user.id, "owners", ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    // Which accounts to query.
    let selected: Vec<(String, SecretString)> = match account_param {
        Some(nagent_github::ALL_ACCOUNTS) => accounts,
        Some(name) => match accounts.iter().find(|(n, _)| n.as_str() == name).cloned() {
            Some(a) => vec![a],
            None => {
                let known = accounts
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write_api_audit(&state.store, user.id, "owners", ProbeOutcome::BadRequest);
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    format!("unknown GitHub account `{name}` (configured accounts: {known})"),
                ));
            }
        },
        None => {
            // Default: the saved `default_account`, else `"default"`.
            let name = vault_default_account_name(&state, user.id).await;
            match accounts.iter().find(|(n, _)| n == &name).cloned() {
                Some(a) => vec![a],
                None => {
                    write_api_audit(&state.store, user.id, "owners", ProbeOutcome::BadRequest);
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        format!("the default GitHub account `{name}` is not configured"),
                    ));
                }
            }
        }
    };
    if selected.len() == 1 {
        let (name, token) = &selected[0];
        match nagent_github::list_owners(&state.client, token, user.id, name).await {
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
    } else {
        // `all`: fan out and merge, deduped by login.
        let mut payloads = Vec::new();
        let mut first_err: Option<GithubError> = None;
        for (name, token) in &selected {
            match nagent_github::list_owners(&state.client, token, user.id, name).await {
                Ok(v) => payloads.push(v),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if payloads.is_empty() {
            let e = first_err.expect("at least one account was selected");
            let outcome = audit_outcome(&e);
            write_api_audit(&state.store, user.id, "owners", outcome);
            return Ok(map_github_error(e));
        }
        let data = nagent_github::merge_owners(payloads);
        write_api_audit(&state.store, user.id, "owners", ProbeOutcome::Ok);
        Ok(Json(serde_json::json!({ "data": data })).into_response())
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
    // `?account=`: name / `"default"` (saved `default_account` when
    // set) / `"all"` (merge across accounts, deduped by full_name).
    let account_param = q
        .account
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let accounts = match vault_accounts(&state, user.id).await {
        Ok(a) => a,
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
    let selected: Vec<(String, SecretString)> = match account_param {
        Some(nagent_github::ALL_ACCOUNTS) => accounts,
        Some(name) => match accounts.iter().find(|(n, _)| n.as_str() == name).cloned() {
            Some(a) => vec![a],
            None => {
                let known = accounts
                    .iter()
                    .map(|(n, _)| n.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                write_api_audit(
                    &state.store,
                    user.id,
                    "owner_repos",
                    ProbeOutcome::BadRequest,
                );
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    format!("unknown GitHub account `{name}` (configured accounts: {known})"),
                ));
            }
        },
        None => {
            let name = vault_default_account_name(&state, user.id).await;
            match accounts.iter().find(|(n, _)| n == &name).cloned() {
                Some(a) => vec![a],
                None => {
                    write_api_audit(
                        &state.store,
                        user.id,
                        "owner_repos",
                        ProbeOutcome::BadRequest,
                    );
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        format!("the default GitHub account `{name}` is not configured"),
                    ));
                }
            }
        }
    };
    // Single access-scoped source (`/user/repos?visibility=all&affiliation=…`,
    // paginated up to 10 pages, filtered by owner) — covers own repos,
    // org-member repos and
    // outside-collaborator private repos of orgs the user is not a
    // visible member of. An owner the PAT cannot see yields an empty
    // list (GitHub enforces visibility upstream).
    if selected.len() == 1 {
        let (name, token) = &selected[0];
        return match nagent_github::list_owner_repos(
            &state.client,
            token,
            user.id,
            name,
            &owner,
            include_archived,
        )
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
        };
    }
    // `all`: fan out and merge, deduped by full_name.
    let mut payloads = Vec::new();
    let mut first_err: Option<GithubError> = None;
    for (name, token) in &selected {
        match nagent_github::list_owner_repos(
            &state.client,
            token,
            user.id,
            name,
            &owner,
            include_archived,
        )
        .await
        {
            Ok(v) => payloads.push(v),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if payloads.is_empty() {
        let e = first_err.expect("at least one account was selected");
        let outcome = audit_outcome(&e);
        write_api_audit(&state.store, user.id, "owner_repos", outcome);
        return Ok(map_github_error(e));
    }
    let data = nagent_github::merge_owner_repos(payloads, &owner);
    write_api_audit(&state.store, user.id, "owner_repos", ProbeOutcome::Ok);
    Ok(Json(serde_json::json!({ "data": data })).into_response())
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
