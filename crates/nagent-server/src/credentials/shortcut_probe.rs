//! `POST /api/integrations/shortcut/probe` — setup-only helper
//! used by the integrations UI to validate a Shortcut API token
//! before saving it (plus the resolved `mention_name`) to the
//! per-user vault.
//!
//! The endpoint is **not** exposed as an LLM tool; the chat tool
//! loop returns "unknown tool" if it tries. The handler runs on
//! the server with the token supplied in the request body — it
//! does **not** read the vault (the user is choosing what to
//! save into the vault).
//!
//! ## What the probe does
//!
//! `GET {base_url}/members` with `Shortcut-Token: <token>`
//! (plus `Accept: application/json` and a `User-Agent`). The v3 API
//! has no `/members/me` route — that path falls through to
//! `/members/{member-public-id}`, whose spec requires a UUID, so the
//! server answers 400 `member-public-id … not instance? UUID "me"`.
//! The list route is the supported way to resolve the token's member:
//! when the request carries a `username` (the mention name the user
//! typed in the form), the handler matches it against
//! `profile.mention_name` / `profile.name` / `profile.email_address`
//! (case-insensitive, `@`-tolerant) and returns
//! `{id, name, mention_name}` for that member; the UI stores
//! `username = mention_name` next to the token through the existing
//! `PUT /api/integrations/shortcut/credentials` flow.
//!
//! ## Hardening
//!
//! - The API host is validated against the same
//!   `[agents.shortcut].allowlist` the chat agents use (default
//!   `["api.app.shortcut.com"]`). Fail-closed when empty.
//! - The SSRF pre-flight (`EgressClient::validate`) runs on
//!   the probe URL before the request.
//! - The token never lands on an audit row, in a log line, or
//!   in an error response (AGENTS.md §6): only the upstream
//!   status + its (truncated) reply body are surfaced.
//!
//! ## Audit
//!
//! Every call writes one `auth_events` row of kind
//! `shortcut_probe_<outcome>` (plus a `_host:<host>` companion)
//! via the shared [`crate::probe`] plumbing.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use url::Url;

use nagent_agents::egress::{host_matches_allowlist, EgressClient, EgressConfig, EgressError};

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::config::agents::ShortcutConfig;
use crate::probe::{write_probe_audit, ProbeConfig, ProbeOutcome, ProbeState};

/// Request body for the Shortcut probe. Accepts either auth mode:
/// a classic `api_token` (sent as `Shortcut-Token`) or an OAuth
/// `access_token` (sent as `Authorization: Bearer`) — exactly one
/// of the two must be non-empty.
#[derive(Debug, Deserialize)]
pub struct ProbeShortcutBody {
    /// The API token to validate. Never logged, never echoed
    /// back, never written to the audit row.
    #[serde(default)]
    pub api_token: Option<String>,
    /// An OAuth access token to validate instead (bearer mode).
    #[serde(default)]
    pub access_token: Option<String>,
    /// Optional mention name (the form's `username` field) used to
    /// pick the token's member out of the `/members` list.
    #[serde(default)]
    pub username: Option<String>,
}

impl ProbeShortcutBody {
    /// Build the wire credential from the body. `None` when both
    /// fields are empty or both are set.
    fn auth(&self) -> Option<nagent_shortcut::ShortcutAuth> {
        use secrecy::SecretString;
        let api = self
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let oauth = self
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (api, oauth) {
            (Some(a), None) => Some(nagent_shortcut::ShortcutAuth::ApiToken(SecretString::from(
                a.to_string(),
            ))),
            (None, Some(o)) => Some(nagent_shortcut::ShortcutAuth::Bearer(SecretString::from(
                o.to_string(),
            ))),
            _ => None,
        }
    }
}

/// JSON shape returned by the probe on success.
#[derive(Debug, Serialize)]
pub struct ProbeShortcutResponse {
    pub ok: bool,
    /// Shortcut member UUID.
    pub id: String,
    /// Display name from the member `profile`.
    pub name: Option<String>,
    /// Shortcut `mention_name` (e.g. `"jane"`) — the UI stores
    /// this as the `username` field of the `shortcut` service.
    pub mention_name: String,
}

#[derive(Debug, Serialize)]
struct ProbeErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ProbeErrorBody { error: message })).into_response()
}

/// Handler: validate the supplied token against
/// `GET {base_url}/members` and resolve the member from the list.
pub async fn probe_shortcut(
    State(state): State<ProbeState<ShortcutConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeShortcutBody>,
) -> Result<Response, AuthError> {
    let Some(auth) = body.auth() else {
        write_probe_audit(&state, user.id, "", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "exactly one of `api_token` or `access_token` is required".into(),
        ));
    };
    let base = state.cfg.base_url.trim_end_matches('/').to_string();
    let url_str = format!("{base}/members");
    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            write_probe_audit(&state, user.id, &url_str, ProbeOutcome::InvalidUrl);
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid base_url `{base}`: {e}"),
            ));
        }
    };
    if !matches!(url.scheme(), "http" | "https") {
        write_probe_audit(&state, user.id, &url_str, ProbeOutcome::UnsupportedScheme);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported scheme `{}` (only http and https are accepted)",
                url.scheme()
            ),
        ));
    }
    let host = url.host_str().unwrap_or("").to_string();
    if state.cfg.allowlist().is_empty() {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::AllowlistEmpty);
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "SHORTCUT_AGENT_ALLOWLIST is empty; the operator must configure the Shortcut \
             allowlist before the probe endpoint will accept calls"
                .into(),
        ));
    }
    if !host_matches_allowlist(&host, state.cfg.allowlist()) {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::NotInAllowlist);
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            format!("host `{host}` is not in the Shortcut allowlist"),
        ));
    }

    let egress = EgressClient::new(EgressConfig {
        timeout_ms: state.cfg.timeout_ms(),
        allow_public: true,
        allowlist: state.cfg.allowlist().to_vec(),
        max_bytes: state.cfg.max_body_bytes(),
        user_agent: format!(
            "nagent-shortcut-probe/{} (+https://github.com/nagent/nagent)",
            env!("CARGO_PKG_VERSION")
        ),
    });
    let validated = match egress.validate(url.as_str()).await {
        Ok(v) => v,
        Err(e) => {
            let outcome = match e {
                EgressError::NotInAllowlist { .. } | EgressError::DisallowedAddress { .. } => {
                    ProbeOutcome::NotInAllowlist
                }
                EgressError::Scheme(_) => ProbeOutcome::UnsupportedScheme,
                EgressError::UrlParse(_) | EgressError::NoHost => ProbeOutcome::InvalidUrl,
                _ => ProbeOutcome::ValidationFailed,
            };
            write_probe_audit(&state, user.id, &host, outcome);
            return Ok(error_response(
                StatusCode::FORBIDDEN,
                format!("probe URL failed validation: {e}"),
            ));
        }
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(state.cfg.timeout_ms()))
        .build()
        .map_err(|e| AuthError::Internal(format!("probe client: {e}")))?;
    // The credential is only ever placed in its auth header
    // (`Shortcut-Token` or `Authorization: Bearer`); no error path
    // below formats it (AGENTS.md §6).
    let (auth_name, auth_value) = auth.header();
    let resp = http
        .get(validated.url.as_str())
        .header(auth_name, auth_value)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::USER_AGENT,
            format!("nagent-shortcut-probe/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::TransportError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("Shortcut probe transport failed: {e}"),
            ));
        }
    };
    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::AuthRejected);
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            format!(
                "Shortcut rejected the credential (HTTP {status}); use \"Connect with \
                 Shortcut\" (OAuth) or generate a new API token at \
                 app.shortcut.com/settings/account/api-tokens"
            ),
        ));
    }
    if status.as_u16() == 429 {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        return Ok(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            "Shortcut rate limit hit during the probe (200 req/min); retry in a moment".into(),
        ));
    }
    if !status.is_success() {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        let snippet: String = body_text.chars().take(256).collect();
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            format!("Shortcut returned HTTP {status}: {snippet}"),
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("Shortcut /members returned invalid JSON: {e}"),
            ));
        }
    };
    let Some(member) = nagent_shortcut::find_member(&parsed, body.username.as_deref()) else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        let wanted = body
            .username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let msg = if wanted.is_empty() {
            "Shortcut /members returned several members and no `username` was provided; \
             fill the mention-name field so the probe can pick yours"
                .into()
        } else {
            format!(
                "no Shortcut workspace member matches `{wanted}` \
                 (checked mention_name, display name, email)"
            )
        };
        return Ok(error_response(StatusCode::BAD_GATEWAY, msg));
    };
    // The v3 member nests the profile under `profile`; accept a
    // top-level field as a fallback so the probe survives minor
    // API drift.
    let profile = member
        .get("profile")
        .cloned()
        .unwrap_or_else(|| member.clone());
    let Some(mention_name) = profile.get("mention_name").and_then(|v| v.as_str()) else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "the matched Shortcut member has no `mention_name` field".into(),
        ));
    };
    let Some(id) = member.get("id").and_then(|v| v.as_str()) else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "the matched Shortcut member has no `id` field".into(),
        ));
    };
    write_probe_audit(&state, user.id, &host, ProbeOutcome::Ok);
    Ok((
        StatusCode::OK,
        Json(ProbeShortcutResponse {
            ok: true,
            id: id.to_string(),
            name: profile
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mention_name: mention_name.to_string(),
        }),
    )
        .into_response())
}

/// Build the Shortcut probe router subtree. Mirrors
/// `build_github_probe_router` — feature-gated on
/// `shortcut-agent`, mounted inside the auth-protected subtree.
pub fn build_shortcut_probe_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let probe_state = ProbeState::<ShortcutConfig> {
        cfg: Arc::new(state.config.agents.shortcut.clone()),
        store: state
            .auth
            .as_ref()
            .expect("shortcut probe requires auth to be enabled")
            .store
            .clone(),
    };
    axum::Router::new()
        .route(
            "/api/integrations/shortcut/probe",
            axum::routing::post(probe_shortcut),
        )
        .with_state(probe_state)
}

// ---------------------------------------------------------------------------
// `POST /api/integrations/shortcut/probe-teams` — setup-only helper
// listing the workspace's groups (`GET /groups`) with a token
// supplied in the request body, so the
// integrations UI can populate the "Default group" picker BEFORE the
// token is saved to the vault (same pattern as the CalDAV
// `probe-calendars` picker). Once the token is saved, the pure-API
// `GET /api/integrations/shortcut/teams` (vault-token variant) takes
// over. Not an LLM tool.
// ---------------------------------------------------------------------------

/// Newtype so the teams probe gets its own audit prefix
/// `shortcut_probe_teams` while sharing the `[agents.shortcut]`
/// allow-list, timeout and service id with the token probe.
#[derive(Debug, Clone)]
pub struct ShortcutTeamsProbeConfig(pub ShortcutConfig);

impl ProbeConfig for ShortcutTeamsProbeConfig {
    fn service_id(&self) -> &'static str {
        "shortcut"
    }
    fn audit_kind_prefix(&self) -> &'static str {
        "shortcut_probe_teams"
    }
    fn allowlist(&self) -> &[String] {
        &self.0.allowlist
    }
    fn timeout_ms(&self) -> u64 {
        self.0.timeout_ms
    }
    fn max_body_bytes(&self) -> usize {
        // The `/groups` response is a JSON array of group objects
        // (a few KiB each); 256 KiB is a generous cap.
        256 * 1024
    }
}

/// Request body for the teams probe: the API token, supplied by
/// the user in the integrations form (never logged, never echoed,
/// never written to the audit row — AGENTS.md §6). Accepts either
/// `api_token` (Shortcut-Token header) or `access_token` (bearer).
#[derive(Debug, Deserialize)]
pub struct ProbeTeamsBody {
    #[serde(default)]
    pub api_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
}

impl ProbeTeamsBody {
    /// Exactly one of the two credentials must be supplied.
    fn auth(&self) -> Option<nagent_shortcut::ShortcutAuth> {
        use secrecy::SecretString;
        let api = self
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let oauth = self
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (api, oauth) {
            (Some(a), None) => Some(nagent_shortcut::ShortcutAuth::ApiToken(SecretString::from(
                a.to_string(),
            ))),
            (None, Some(o)) => Some(nagent_shortcut::ShortcutAuth::Bearer(SecretString::from(
                o.to_string(),
            ))),
            _ => None,
        }
    }
}

/// JSON shape returned by the teams probe on success.
#[derive(Debug, Serialize)]
pub struct ProbeTeamsResponse {
    pub ok: bool,
    pub count: usize,
    pub teams: Vec<serde_json::Value>,
}

/// Host of the configured `base_url`, for the `_host:<host>`
/// audit companion row.
fn base_url_host(cfg: &ShortcutConfig) -> String {
    Url::parse(&cfg.base_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// Handler: list `GET /groups` with the
/// supplied credential. The wire call goes through the shared
/// `nagent_shortcut::ShortcutClient` (same egress SSRF pre-flight and
/// `[agents.shortcut].allowlist` as the chat agents); the credential
/// only ever travels in its auth header (`Shortcut-Token` or
/// `Authorization: Bearer`).
pub async fn probe_teams(
    State(state): State<ProbeState<ShortcutTeamsProbeConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeTeamsBody>,
) -> Result<Response, AuthError> {
    let host = base_url_host(&state.cfg.0);
    let Some(auth) = body.auth() else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "exactly one of `api_token` or `access_token` is required".into(),
        ));
    };
    let wire_cfg: nagent_shortcut::ShortcutConfig = state.cfg.0.clone().into();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AuthError::Internal(format!("probe client: {e}")))?;
    let client = nagent_shortcut::ShortcutClient::new(wire_cfg, http);
    // `GET /groups` — the UUID-id entities usable as `group_id`.
    let result = nagent_shortcut::fetch_groups(&client, &auth).await;
    match result {
        Ok(raw) => {
            let teams: Vec<serde_json::Value> = raw
                .as_array()
                .map(|arr| arr.iter().map(nagent_shortcut::project_team).collect())
                .unwrap_or_default();
            write_probe_audit(&state, user.id, &host, ProbeOutcome::Ok);
            Ok((
                StatusCode::OK,
                Json(ProbeTeamsResponse {
                    ok: true,
                    count: teams.len(),
                    teams,
                }),
            )
                .into_response())
        }
        Err(e) => {
            let (outcome, resp) = match &e {
                nagent_shortcut::ShortcutError::Unauthorized(msg) => (
                    ProbeOutcome::AuthRejected,
                    error_response(StatusCode::BAD_GATEWAY, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::RateLimited { hint } => (
                    ProbeOutcome::UpstreamError,
                    error_response(StatusCode::TOO_MANY_REQUESTS, hint.clone()),
                ),
                nagent_shortcut::ShortcutError::SandboxDenied(msg) => (
                    ProbeOutcome::NotInAllowlist,
                    error_response(StatusCode::FORBIDDEN, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::InvalidArguments(msg) => (
                    ProbeOutcome::BadRequest,
                    error_response(StatusCode::BAD_REQUEST, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::Upstream { status, body } => (
                    ProbeOutcome::UpstreamError,
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("Shortcut returned HTTP {status}: {body}"),
                    ),
                ),
                nagent_shortcut::ShortcutError::ResponseExceeded { budget } => (
                    ProbeOutcome::UpstreamError,
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("Shortcut response exceeded the {budget}-byte budget"),
                    ),
                ),
                nagent_shortcut::ShortcutError::Transport(msg) => (
                    ProbeOutcome::TransportError,
                    error_response(StatusCode::BAD_GATEWAY, msg.clone()),
                ),
            };
            write_probe_audit(&state, user.id, &host, outcome);
            Ok(resp)
        }
    }
}

/// Build the teams-probe router subtree. Same feature gate and
/// mounting as `build_shortcut_probe_router`.
pub fn build_shortcut_probe_teams_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let probe_state = ProbeState::<ShortcutTeamsProbeConfig> {
        cfg: Arc::new(ShortcutTeamsProbeConfig(
            state.config.agents.shortcut.clone(),
        )),
        store: state
            .auth
            .as_ref()
            .expect("shortcut probe-teams requires auth to be enabled")
            .store
            .clone(),
    };
    axum::Router::new()
        .route(
            "/api/integrations/shortcut/probe-teams",
            axum::routing::post(probe_teams),
        )
        .with_state(probe_state)
}

// ---------------------------------------------------------------------------
// `POST /api/integrations/shortcut/probe-workflows` — setup-only helper
// listing the workspace's workflow states (`GET /workflows`, flattened)
// with a token supplied in the request body, so the integrations UI can
// populate the "Default workflow state" picker BEFORE the token is
// saved to the vault (same pattern as `probe-teams`). Once the token is
// saved, the pure-API `GET /api/integrations/shortcut/workflows`
// (vault-token variant) takes over. Not an LLM tool.
// ---------------------------------------------------------------------------

/// Newtype so the workflows probe gets its own audit prefix
/// `shortcut_probe_workflows` while sharing the `[agents.shortcut]`
/// allow-list, timeout and service id with the token probe.
#[derive(Debug, Clone)]
pub struct ShortcutWorkflowsProbeConfig(pub ShortcutConfig);

impl ProbeConfig for ShortcutWorkflowsProbeConfig {
    fn service_id(&self) -> &'static str {
        "shortcut"
    }
    fn audit_kind_prefix(&self) -> &'static str {
        "shortcut_probe_workflows"
    }
    fn allowlist(&self) -> &[String] {
        &self.0.allowlist
    }
    fn timeout_ms(&self) -> u64 {
        self.0.timeout_ms
    }
    fn max_body_bytes(&self) -> usize {
        // The `/workflows` response is a JSON array of workflow
        // objects with their states (a few KiB each); 256 KiB is a
        // generous cap.
        256 * 1024
    }
}

/// Request body for the workflows probe: the API token, supplied by
/// the user in the integrations form (never logged, never echoed,
/// never written to the audit row — AGENTS.md §6). Accepts either
/// `api_token` (Shortcut-Token header) or `access_token` (bearer).
#[derive(Debug, Deserialize)]
pub struct ProbeWorkflowsBody {
    #[serde(default)]
    pub api_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
}

impl ProbeWorkflowsBody {
    /// Exactly one of the two credentials must be supplied.
    fn auth(&self) -> Option<nagent_shortcut::ShortcutAuth> {
        use secrecy::SecretString;
        let api = self
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let oauth = self
            .access_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match (api, oauth) {
            (Some(a), None) => Some(nagent_shortcut::ShortcutAuth::ApiToken(SecretString::from(
                a.to_string(),
            ))),
            (None, Some(o)) => Some(nagent_shortcut::ShortcutAuth::Bearer(SecretString::from(
                o.to_string(),
            ))),
            _ => None,
        }
    }
}

/// JSON shape returned by the workflows probe on success.
#[derive(Debug, Serialize)]
pub struct ProbeWorkflowsResponse {
    pub ok: bool,
    pub count: usize,
    pub states: Vec<serde_json::Value>,
    pub workflows: Vec<serde_json::Value>,
}

/// Handler: list `GET /workflows` (flattened to states) with the
/// supplied credential. The wire call goes through the shared
/// `nagent_shortcut::ShortcutClient` (same egress SSRF pre-flight and
/// `[agents.shortcut].allowlist` as the chat agents); the credential
/// only ever travels in its auth header (`Shortcut-Token` or
/// `Authorization: Bearer`).
pub async fn probe_workflows(
    State(state): State<ProbeState<ShortcutWorkflowsProbeConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeWorkflowsBody>,
) -> Result<Response, AuthError> {
    let host = base_url_host(&state.cfg.0);
    let Some(auth) = body.auth() else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "exactly one of `api_token` or `access_token` is required".into(),
        ));
    };
    let wire_cfg: nagent_shortcut::ShortcutConfig = state.cfg.0.clone().into();
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AuthError::Internal(format!("probe client: {e}")))?;
    let client = nagent_shortcut::ShortcutClient::new(wire_cfg, http);
    // `GET /workflows` — flatten every workflow's states.
    let result = nagent_shortcut::fetch_workflows(&client, &auth).await;
    match result {
        Ok(raw) => {
            let workflows: Vec<serde_json::Value> = raw
                .as_array()
                .map(|arr| arr.iter().map(nagent_shortcut::project_workflow).collect())
                .unwrap_or_default();
            let states: Vec<serde_json::Value> = raw
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .flat_map(|w| {
                            w.get("states")
                                .and_then(|s| s.as_array())
                                .map(|states| {
                                    states
                                        .iter()
                                        .map(|s| nagent_shortcut::project_workflow_state(w, s))
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            write_probe_audit(&state, user.id, &host, ProbeOutcome::Ok);
            Ok((
                StatusCode::OK,
                Json(ProbeWorkflowsResponse {
                    ok: true,
                    count: states.len(),
                    states,
                    workflows,
                }),
            )
                .into_response())
        }
        Err(e) => {
            let (outcome, resp) = match &e {
                nagent_shortcut::ShortcutError::Unauthorized(msg) => (
                    ProbeOutcome::AuthRejected,
                    error_response(StatusCode::BAD_GATEWAY, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::RateLimited { hint } => (
                    ProbeOutcome::UpstreamError,
                    error_response(StatusCode::TOO_MANY_REQUESTS, hint.clone()),
                ),
                nagent_shortcut::ShortcutError::SandboxDenied(msg) => (
                    ProbeOutcome::NotInAllowlist,
                    error_response(StatusCode::FORBIDDEN, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::InvalidArguments(msg) => (
                    ProbeOutcome::BadRequest,
                    error_response(StatusCode::BAD_REQUEST, msg.clone()),
                ),
                nagent_shortcut::ShortcutError::Upstream { status, body } => (
                    ProbeOutcome::UpstreamError,
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("Shortcut returned HTTP {status}: {body}"),
                    ),
                ),
                nagent_shortcut::ShortcutError::ResponseExceeded { budget } => (
                    ProbeOutcome::UpstreamError,
                    error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("Shortcut response exceeded the {budget}-byte budget"),
                    ),
                ),
                nagent_shortcut::ShortcutError::Transport(msg) => (
                    ProbeOutcome::TransportError,
                    error_response(StatusCode::BAD_GATEWAY, msg.clone()),
                ),
            };
            write_probe_audit(&state, user.id, &host, outcome);
            Ok(resp)
        }
    }
}

/// Build the workflows-probe router subtree. Same feature gate and
/// mounting as `build_shortcut_probe_teams_router`.
pub fn build_shortcut_probe_workflows_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let probe_state = ProbeState::<ShortcutWorkflowsProbeConfig> {
        cfg: Arc::new(ShortcutWorkflowsProbeConfig(
            state.config.agents.shortcut.clone(),
        )),
        store: state
            .auth
            .as_ref()
            .expect("shortcut probe-workflows requires auth to be enabled")
            .store
            .clone(),
    };
    axum::Router::new()
        .route(
            "/api/integrations/shortcut/probe-workflows",
            axum::routing::post(probe_workflows),
        )
        .with_state(probe_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agents::ShortcutConfig;
    use crate::probe::ProbeConfig;

    #[test]
    fn shortcut_probe_config_returns_stable_ids() {
        let cfg = ShortcutConfig::default();
        assert_eq!(cfg.service_id(), "shortcut");
        assert_eq!(cfg.audit_kind_prefix(), "shortcut_probe");
        assert_eq!(cfg.allowlist(), &["api.app.shortcut.com".to_string()]);
    }

    #[test]
    fn shortcut_probe_workflows_config_returns_stable_ids() {
        let cfg = ShortcutWorkflowsProbeConfig(ShortcutConfig::default());
        assert_eq!(cfg.service_id(), "shortcut");
        assert_eq!(cfg.audit_kind_prefix(), "shortcut_probe_workflows");
        assert_eq!(cfg.allowlist(), &["api.app.shortcut.com".to_string()]);
    }

    #[test]
    fn probe_workflows_body_requires_exactly_one_credential() {
        let none = ProbeWorkflowsBody {
            api_token: None,
            access_token: None,
        };
        assert!(none.auth().is_none());
        let both = ProbeWorkflowsBody {
            api_token: Some("a".into()),
            access_token: Some("b".into()),
        };
        assert!(both.auth().is_none());
        let api = ProbeWorkflowsBody {
            api_token: Some("  tok  ".into()),
            access_token: None,
        };
        assert!(matches!(
            api.auth(),
            Some(nagent_shortcut::ShortcutAuth::ApiToken(_))
        ));
    }

    #[test]
    fn probe_error_body_never_carries_the_token() {
        // AGENTS.md §6: the error JSON shape has a single
        // `error` string; the handler only ever fills it from
        // the upstream status + reply body, never from the
        // request. This test pins the shape so a future edit
        // that adds a `debug` field trips review.
        let body = ProbeErrorBody {
            error: "Shortcut rejected the token (HTTP 401)".into(),
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("api_token"));
    }
}
