//! `POST /api/integrations/github/probe` — setup-only helper
//! used by the integrations UI to validate a GitHub personal
//! access token before saving it (plus the resolved `login`) to
//! the per-user vault.
//!
//! The endpoint is **not** exposed as an LLM tool; the chat tool
//! loop returns "unknown tool" if it tries. The handler runs on
//! the server with the token supplied in the request body — it
//! does **not** read the vault (the user is choosing what to
//! save into the vault).
//!
//! ## What the probe does
//!
//! `GET {base_url}/user` with `Authorization: Bearer
//! <token>` (plus `Accept: application/vnd.github+json`,
//! `X-GitHub-Api-Version: 2022-11-28`, and a `User-Agent` —
//! GitHub rejects requests without one). On 2xx the response
//! body carries the authenticated user's profile; the handler
//! returns `{login, name, avatar_url, plan}` and the UI stores
//! `username = login` next to the token through the existing
//! `PUT /api/integrations/github/credentials` flow.
//!
//! ## Hardening
//!
//! - The API host is validated against the same
//!   `[agents.github].allowlist` the chat agents use (default
//!   `["api.github.com"]`; GHES deployments override
//!   `base_url` + `allowlist`). Fail-closed when empty.
//! - The SSRF pre-flight (`EgressClient::validate`) runs on
//!   the probe URL before the request.
//! - The token never lands on an audit row, in a log line, or
//!   in an error response (AGENTS.md §6): only the upstream
//!   status + its (truncated) reply body are surfaced.
//!
//! ## Audit
//!
//! Every call writes one `auth_events` row of kind
//! `github_probe_<outcome>` (plus a `_host:<host>` companion
//! row) via the shared [`crate::probe`] plumbing.

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
use crate::config::agents::GithubConfig;
use crate::probe::{write_probe_audit, ProbeConfig, ProbeOutcome, ProbeState};

/// Request body for the GitHub probe.
#[derive(Debug, Deserialize)]
pub struct ProbeGithubBody {
    /// The personal access token to validate. Never logged,
    /// never echoed back, never written to the audit row.
    pub personal_access_token: String,
}

/// JSON shape returned by the probe on success.
#[derive(Debug, Serialize)]
pub struct ProbeGithubResponse {
    pub ok: bool,
    /// GitHub `login` (e.g. `"octocat"`) — the UI stores this
    /// as the `username` field of the `github` service.
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    /// Plan name (`"free"`, `"pro"`, …) when GitHub returned one.
    pub plan: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProbeErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ProbeErrorBody { error: message })).into_response()
}

/// Handler: validate the supplied PAT against `GET {base_url}/user`.
pub async fn probe_github(
    State(state): State<ProbeState<GithubConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeGithubBody>,
) -> Result<Response, AuthError> {
    if body.personal_access_token.trim().is_empty() {
        write_probe_audit(&state, user.id, "", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "personal_access_token is required".into(),
        ));
    }
    let base = state.cfg.base_url.trim_end_matches('/').to_string();
    let url_str = format!("{base}/user");
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
            "GITHUB_AGENT_ALLOWLIST is empty; the operator must configure the GitHub \
             allowlist before the probe endpoint will accept calls"
                .into(),
        ));
    }
    if !host_matches_allowlist(&host, state.cfg.allowlist()) {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::NotInAllowlist);
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            format!("host `{host}` is not in the GitHub allowlist"),
        ));
    }

    let egress = EgressClient::new(EgressConfig {
        timeout_ms: state.cfg.timeout_ms(),
        allow_public: true,
        allowlist: state.cfg.allowlist().to_vec(),
        max_bytes: state.cfg.max_body_bytes(),
        user_agent: format!(
            "nagent-github-probe/{} (+https://github.com/nagent/nagent)",
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
    // The token is only ever placed in the Authorization header;
    // no error path below formats it (AGENTS.md §6).
    let resp = http
        .get(validated.url.as_str())
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", body.personal_access_token.trim()),
        )
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(
            reqwest::header::USER_AGENT,
            format!("nagent-github-probe/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::TransportError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("GitHub probe transport failed: {e}"),
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
                "GitHub rejected the token (HTTP {status}); check the scopes \
                     (read:user + repo) and that the token is not expired"
            ),
        ));
    }
    if !status.is_success() {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        let snippet: String = body_text.chars().take(256).collect();
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            format!("GitHub returned HTTP {status}: {snippet}"),
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("GitHub /user returned invalid JSON: {e}"),
            ));
        }
    };
    let Some(login) = parsed.get("login").and_then(|v| v.as_str()) else {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            "GitHub /user response has no `login` field".into(),
        ));
    };
    write_probe_audit(&state, user.id, &host, ProbeOutcome::Ok);
    Ok((
        StatusCode::OK,
        Json(ProbeGithubResponse {
            ok: true,
            login: login.to_string(),
            name: parsed
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            avatar_url: parsed
                .get("avatar_url")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            html_url: parsed
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            plan: parsed
                .get("plan")
                .and_then(|p| p.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
    )
        .into_response())
}

/// Build the GitHub probe router subtree. Mirrors
/// `build_caldav_probe_router` — feature-gated on
/// `github-agent`, mounted inside the auth-protected subtree.
pub fn build_github_probe_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let probe_state = ProbeState::<GithubConfig> {
        cfg: Arc::new(state.config.agents.github.clone()),
        store: state
            .auth
            .as_ref()
            .expect("github probe requires auth to be enabled")
            .store
            .clone(),
    };
    axum::Router::new()
        .route(
            "/api/integrations/github/probe",
            axum::routing::post(probe_github),
        )
        .with_state(probe_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agents::GithubConfig;
    use crate::probe::ProbeConfig;

    #[test]
    fn github_probe_config_returns_stable_ids() {
        let cfg = GithubConfig::default();
        assert_eq!(cfg.service_id(), "github");
        assert_eq!(cfg.audit_kind_prefix(), "github_probe");
        assert_eq!(cfg.allowlist(), &["api.github.com".to_string()]);
    }

    #[test]
    fn probe_error_body_never_carries_the_token() {
        // AGENTS.md §6: the error JSON shape has a single
        // `error` string; the handler only ever fills it from
        // the upstream status + reply body, never from the
        // request. This test pins the shape so a future edit
        // that adds a `debug` field trips review.
        let body = ProbeErrorBody {
            error: "GitHub rejected the token (HTTP 401)".into(),
        };
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("error"));
        assert!(!s.contains("personal_access_token"));
    }
}
