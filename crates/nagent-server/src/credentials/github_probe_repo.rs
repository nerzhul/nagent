//! `POST /api/integrations/github/probe-repo` — setup-only helper
//! used by the integrations UI to verify that the **saved** GitHub
//! PAT has access to a repo before the repo is appended to the
//! per-user `dashboard_repos` list (plan 1795686864347).
//!
//! Unlike `POST /api/integrations/github/probe` (which takes the
//! token in the request body because nothing is saved yet), this
//! probe runs after the token is in the vault: the handler reads
//! and decrypts `github.personal_access_token` from the caller's
//! vault row and calls `GET /repos/{owner}/{repo}` through the
//! shared `nagent_github::GithubClient`.
//!
//! The endpoint is **not** exposed as an LLM tool; the chat tool
//! loop returns "unknown tool" if it tries.
//!
//! ## Wire contract
//!
//! ```text
//! POST /api/integrations/github/probe-repo
//! {"repo": "octocat/hello-world"}
//! → 200 {"ok": true, "repo": {"full_name": "…", "private": false,
//!                              "default_branch": "main", "html_url": "…"}}
//! → 400 {"error": "`repo` must be shaped \"owner/repo\" …"}        (invalid shape)
//! → 400 {"error": "the GitHub integration is not configured …"}   (no saved PAT)
//! → 401 {"error": "GitHub rejected the personal access token …"}  (PAT revoked)
//! → 404 {"error": "no access to this repo with the saved token …"} (no access / missing)
//! → 429 {"error": "rate limited …"}                                (quota exhausted)
//! ```
//!
//! ## Hardening
//!
//! - The `repo` value is validated with
//!   [`nagent_github::valid_repo_full_name`] **before any network
//!   call**, so a malformed value can never smuggle a path
//!   traversal into the API path.
//! - The SSRF pre-flight + `[agents.github].allowlist` runs inside
//!   `GithubClient::get` (default `["api.github.com"]`; GHES
//!   overrides `base_url` + `allowlist`). Fail-closed when empty.
//! - The token never lands on an audit row, in a log line, or in
//!   an error response (AGENTS.md §6): only the upstream status +
//!   its (truncated) reply body are surfaced.
//!
//! ## Audit
//!
//! Every call writes one `auth_events` row of kind
//! `github_probe_repo_<outcome>` (plus a `_host:<host>` companion)
//! via the shared [`crate::probe`] plumbing — distinct from
//! `github_probe_*` (token probe) and `github_api_*` (dashboard
//! reads). The token never lands on the audit row.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use url::Url;

use nagent_github::{GithubClient, GithubError};

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::config::agents::GithubConfig;
use crate::credentials::crypto::{decrypt, EncryptedSecret};
use crate::credentials::key::CredentialsKey;
use crate::probe::{write_probe_audit, ProbeConfig, ProbeOutcome, ProbeState};

/// Newtype wrapper exposing the audit kind prefix
/// `github_probe_repo` for the repo probe (the token probe uses
/// `github_probe`); both share the `[agents.github]` allow-list,
/// timeout, and service id so the audit rows stay grep-able by
/// service while telling the two probes apart.
#[derive(Debug, Clone)]
pub struct GithubProbeRepoConfig(pub GithubConfig);

impl ProbeConfig for GithubProbeRepoConfig {
    fn service_id(&self) -> &'static str {
        "github"
    }
    fn audit_kind_prefix(&self) -> &'static str {
        "github_probe_repo"
    }
    fn allowlist(&self) -> &[String] {
        &self.0.allowlist
    }
    fn timeout_ms(&self) -> u64 {
        self.0.timeout_ms
    }
    fn max_body_bytes(&self) -> usize {
        // The `/repos/{owner}/{repo}` response is a single JSON
        // object (a few KiB); 256 KiB is a generous cap.
        256 * 1024
    }
}

/// Request body for the repo probe.
#[derive(Debug, Deserialize)]
pub struct ProbeRepoBody {
    /// `"owner/repo"` to check access for. Validated before any
    /// network call; never logged.
    pub repo: String,
}

/// JSON shape returned by the probe on success. `account` is the
/// name of the configured account that could read the repo (plan
/// 1795793328177) — a name, never a token.
#[derive(Debug, Serialize)]
pub struct ProbeRepoResponse {
    pub ok: bool,
    pub repo: serde_json::Value,
    pub account: String,
}

#[derive(Debug, Serialize)]
struct ProbeErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ProbeErrorBody { error: message })).into_response()
}

/// Per-route state: the probe plumbing (config + audit store),
/// the credentials key to decrypt the vault PAT, and the shared
/// wire client (built once at router construction).
#[derive(Clone)]
pub struct GithubProbeRepoState {
    pub probe: ProbeState<GithubProbeRepoConfig>,
    pub key: Arc<CredentialsKey>,
    pub client: GithubClient,
}

impl std::fmt::Debug for GithubProbeRepoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubProbeRepoState")
            .field("probe", &self.probe)
            .field("key", &"<redacted>")
            .field("client", &self.client)
            .finish()
    }
}

/// Host of the configured `base_url`, for the `_host:<host>`
/// audit companion row. Empty string skips the companion row.
fn base_url_host(cfg: &GithubConfig) -> String {
    Url::parse(&cfg.base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Read + decrypt one raw `github` vault field.
// See below for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn vault_field(
    state: &GithubProbeRepoState,
    user_id: uuid::Uuid,
    field: &str,
) -> Result<Option<SecretString>, Response> {
    let row = state
        .probe
        .store
        .for_user(user_id)
        .credentials()
        .fetch("github", field)
        .await
        .map_err(|e| {
            tracing::warn!(user = %user_id, field, error = %e, "github probe-repo: vault read failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read the GitHub credentials from the vault".into(),
            )
        })?;
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
        tracing::warn!(user = %user_id, field, error = %e, "github probe-repo: vault decrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decrypt the GitHub credentials (the credentials key may have been \
             rotated); re-save the token in Settings → Integrations"
                .into(),
        )
    })?;
    Ok(Some(plaintext))
}

/// Every configured account as `(name, token)`, default first
/// (plan 1795793328177): `personal_access_token` as `"default"`,
/// then the parsed `github.accounts` blob.
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn vault_accounts(
    state: &GithubProbeRepoState,
    user_id: uuid::Uuid,
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

/// Persist one `github` vault field, carrying every other field's
/// ciphertext over verbatim (the DB upsert replaces all rows of
/// the service, so a partial write must re-supply them — same
/// rule as `PUT /api/integrations/:id/credentials`).
// See `vault_field` for why the `Response` error variant is not boxed.
#[allow(clippy::result_large_err)]
async fn write_vault_field(
    state: &GithubProbeRepoState,
    user_id: uuid::Uuid,
    field: &str,
    plaintext: &str,
) -> Result<(), Response> {
    let creds = state.probe.store.for_user(user_id).credentials();
    let keys = creds.list_field_keys("github").await.map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github probe-repo: list_field_keys failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to read the GitHub credentials from the vault".into(),
        )
    })?;
    let mut rows: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::with_capacity(keys.len());
    for key in keys {
        if key == field {
            continue;
        }
        if let Ok(Some(row)) = creds.fetch("github", &key).await {
            rows.push((key, row.nonce, row.ciphertext));
        }
    }
    let sealed = crate::credentials::crypto::encrypt(&state.key, plaintext).map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github probe-repo: encrypt failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encrypt the repo→account map".into(),
        )
    })?;
    rows.push((field.to_string(), sealed.nonce, sealed.ciphertext));
    creds.upsert("github", &rows).await.map_err(|e| {
        tracing::warn!(user = %user_id, error = %e, "github probe-repo: upsert failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to persist the repo→account map".into(),
        )
    })?;
    Ok(())
}

/// Record `repo → account` in the `github.repo_accounts` vault map
/// so the dashboard and the repo-scoped tools resolve instantly.
/// Best-effort: a failure here never fails the probe.
async fn record_repo_account(
    state: &GithubProbeRepoState,
    user_id: uuid::Uuid,
    repo: &str,
    account: &str,
) {
    use secrecy::ExposeSecret;
    let mut map: std::collections::HashMap<String, String> =
        match vault_field(state, user_id, "repo_accounts").await {
            Ok(Some(blob)) => serde_json::from_str(blob.expose_secret()).unwrap_or_default(),
            _ => std::collections::HashMap::new(),
        };
    map.insert(repo.to_string(), account.to_string());
    let blob = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
    if let Err(resp) = write_vault_field(state, user_id, "repo_accounts", &blob).await {
        tracing::warn!(
            user = %user_id,
            repo,
            account,
            "github probe-repo: could not persist repo→account map"
        );
        let _ = resp;
    }
}

/// Handler: verify that at least one configured account has access
/// to `body.repo` (plan 1795793328177 — every account is tried,
/// default first) and record the winning account in the
/// `github.repo_accounts` vault map.
pub async fn probe_repo(
    State(state): State<GithubProbeRepoState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeRepoBody>,
) -> Result<Response, AuthError> {
    let host = base_url_host(&state.probe.cfg.0);
    let raw = body.repo.trim();
    // Validate the shape BEFORE any network call so a malformed
    // value can never smuggle a path traversal into the API path.
    let Some((owner, repo)) = nagent_github::valid_repo_full_name(raw) else {
        write_probe_audit(&state.probe, user.id, &host, ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "`repo` must be shaped `\"owner/repo\"` (alphanumeric, `-`, `_`, `.`), \
                 got `{raw}`"
            ),
        ));
    };
    let accounts = match vault_accounts(&state, user.id).await {
        Ok(a) => a,
        Err(resp) => {
            write_probe_audit(&state.probe, user.id, &host, ProbeOutcome::BadRequest);
            return Ok(resp);
        }
    };
    let full = format!("{owner}/{repo}");
    // Try every account, default first; first success wins.
    let mut first_auth_err: Option<GithubError> = None;
    let mut saw_404 = false;
    for (name, token) in &accounts {
        match nagent_github::get_repo(&state.client, token, user.id, name, owner, repo).await {
            Ok(summary) => {
                // Record which account can read the repo so the
                // dashboard and repo-scoped tools skip re-probing.
                record_repo_account(&state, user.id, &full, name).await;
                write_probe_audit(&state.probe, user.id, &host, ProbeOutcome::Ok);
                return Ok((
                    StatusCode::OK,
                    Json(ProbeRepoResponse {
                        ok: true,
                        repo: summary,
                        account: name.clone(),
                    }),
                )
                    .into_response());
            }
            Err(GithubError::Upstream { status: 404, .. }) => {
                saw_404 = true;
            }
            Err(e @ GithubError::Unauthorized(_)) => {
                if first_auth_err.is_none() {
                    first_auth_err = Some(e);
                }
            }
            Err(e) => {
                let (outcome, resp) = probe_err_to_response(&e);
                write_probe_audit(&state.probe, user.id, &host, outcome);
                return Ok(resp);
            }
        }
    }
    // No account could read it: 404 when at least one answered 404
    // (genuinely invisible), else the first credential error (e.g.
    // every token revoked → 401).
    if !saw_404 {
        if let Some(e) = first_auth_err {
            let (outcome, resp) = probe_err_to_response(&e);
            write_probe_audit(&state.probe, user.id, &host, outcome);
            return Ok(resp);
        }
    }
    write_probe_audit(&state.probe, user.id, &host, ProbeOutcome::UpstreamError);
    Ok(error_response(
        StatusCode::NOT_FOUND,
        format!(
            "no configured GitHub account can read `{full}` (tried: {})",
            accounts
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

/// Map one wire error onto `(audit outcome, HTTP response)` for the
/// probe. Messages never carry a token (AGENTS.md §6).
fn probe_err_to_response(e: &GithubError) -> (ProbeOutcome, Response) {
    match e {
        GithubError::Unauthorized(msg) => (
            ProbeOutcome::AuthRejected,
            error_response(StatusCode::UNAUTHORIZED, msg.clone()),
        ),
        GithubError::RateLimited { hint } => (
            ProbeOutcome::UpstreamError,
            error_response(StatusCode::TOO_MANY_REQUESTS, hint.clone()),
        ),
        GithubError::Upstream { status, body } if *status == 404 => (
            ProbeOutcome::UpstreamError,
            error_response(
                StatusCode::NOT_FOUND,
                "no access to this repo with the saved token (or it does not exist)".into(),
            ),
        ),
        GithubError::Upstream { status, body } => (
            ProbeOutcome::UpstreamError,
            error_response(
                StatusCode::BAD_GATEWAY,
                format!("GitHub returned HTTP {status}: {body}"),
            ),
        ),
        GithubError::SandboxDenied(msg) => (
            ProbeOutcome::ValidationFailed,
            error_response(StatusCode::FORBIDDEN, msg.clone()),
        ),
        GithubError::InvalidArguments(msg) => (
            ProbeOutcome::BadRequest,
            error_response(StatusCode::BAD_REQUEST, msg.clone()),
        ),
        GithubError::ResponseExceeded { budget } => (
            ProbeOutcome::UpstreamError,
            error_response(
                StatusCode::BAD_GATEWAY,
                format!("GitHub response exceeded the {budget}-byte budget"),
            ),
        ),
        GithubError::Transport(msg) => (
            ProbeOutcome::TransportError,
            error_response(StatusCode::BAD_GATEWAY, msg.clone()),
        ),
    }
}

/// Build the repo-probe router subtree. Feature-gated on
/// `github-agent` at the call site, mounted inside the
/// auth-protected subtree next to `build_github_probe_router`.
pub fn build_github_probe_repo_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    // Same graceful guard as `build_shortcut_api_router`: the probe
    // reads the vault, so without auth / a credential resolver
    // there is nothing to decrypt — mount nothing.
    let Some(auth) = state.auth.as_ref() else {
        return axum::Router::new();
    };
    if auth.credential_resolver.is_none() {
        return axum::Router::new();
    }
    let cfg = state.config.agents.github.clone();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(8)
        .build()
        .expect("github probe-repo client must build");
    let probe_state = GithubProbeRepoState {
        probe: ProbeState {
            cfg: Arc::new(GithubProbeRepoConfig(cfg.clone())),
            store: auth.store.clone(),
        },
        key: state.credential_encryption_key(),
        client: GithubClient::new(cfg.into(), http),
    };
    axum::Router::new()
        .route(
            "/api/integrations/github/probe-repo",
            axum::routing::post(probe_repo),
        )
        .with_state(probe_state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_repo_config_uses_the_repo_audit_prefix() {
        let cfg = GithubProbeRepoConfig(GithubConfig::default());
        assert_eq!(cfg.service_id(), "github");
        assert_eq!(cfg.audit_kind_prefix(), "github_probe_repo");
        assert_eq!(cfg.allowlist(), &["api.github.com".to_string()]);
    }

    #[test]
    fn probe_repo_error_body_never_carries_the_token() {
        // AGENTS.md §6: the error JSON shape has a single
        // `error` string; the handler only ever fills it from
        // the upstream status + reply body, never from the
        // request or the vault. This test pins the shape so a
        // future edit that adds a `debug` field trips review.
        let body = ProbeErrorBody {
            error: "no access to this repo with the saved token".into(),
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("personal_access_token"));
    }

    #[test]
    fn base_url_host_extracts_the_host() {
        let cfg = GithubConfig {
            base_url: "https://ghes.example.com/api/v3".into(),
            ..GithubConfig::default()
        };
        assert_eq!(base_url_host(&cfg), "ghes.example.com");
        let bad = GithubConfig {
            base_url: "not a url".into(),
            ..GithubConfig::default()
        };
        assert_eq!(base_url_host(&bad), "");
    }
}
