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

/// Request body for the Shortcut probe.
#[derive(Debug, Deserialize)]
pub struct ProbeShortcutBody {
    /// The API token to validate. Never logged, never echoed
    /// back, never written to the audit row.
    pub api_token: String,
    /// Optional mention name (the form's `username` field) used to
    /// pick the token's member out of the `/members` list.
    #[serde(default)]
    pub username: Option<String>,
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
    if body.api_token.trim().is_empty() {
        write_probe_audit(&state, user.id, "", ProbeOutcome::BadRequest);
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "api_token is required".into(),
        ));
    }
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
    // The token is only ever placed in the Shortcut-Token header;
    // no error path below formats it (AGENTS.md §6).
    let resp = http
        .get(validated.url.as_str())
        .header("Shortcut-Token", body.api_token.trim())
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
                "Shortcut rejected the token (HTTP {status}); generate a new token at \
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
    let Some(member) =
        nagent_agents::agents::shortcut::find_member(&parsed, body.username.as_deref())
    else {
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
