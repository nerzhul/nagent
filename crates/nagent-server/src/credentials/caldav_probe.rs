//! `POST /api/integrations/caldav/probe-calendars` — setup-only
//! helper used by the integrations UI to discover the user's
//! calendar collection before saving it to the per-user vault.
//!
//! The endpoint is **not** exposed as an LLM tool (the
//! `caldav_list_calendars` agent is not registered in
//! `AGENT_DESCRIPTORS`); the chat tool loop returns
//! `"unknown tool"` if it tries. The handler runs on the
//! server with credentials supplied in the request body —
//! it does **not** read the vault (the user is choosing what
//! to save into the vault).
//!
//! ## Hardening
//!
//! The principal URL is validated against the same
//! `EgressConfig` the chat agents use (`CalDavConfig::allowlist`).
//! The default `allowlist` is empty, so the endpoint refuses
//! every call until the operator sets `CALDAV_ALLOWLIST` —
//! fail-closed.
//!
//! ## Audit
//!
//! Every call writes one `auth_events` row of kind
//! `caldav_probe_<outcome>` (plus a `_host:<host>` companion
//! row) so an operator can correlate probe attempts with
//! the per-user vault. The password never lands on the
//! audit row.
//!
//! ## Design
//!
//! The handler is wired through the generic
//! `crate::probe::ProbeState<CalDavConfig>` newtype so
//! adding a second probe (e.g. CardDAV) is one new
//! `impl ProbeConfig for CardDavConfig` + one new
//! `FromRef<Arc<AppState>> for ProbeState<CardDavConfig>` —
//! no changes to this file.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde::{Deserialize, Serialize};
use url::Url;

use nagent_agents::egress::host_matches_allowlist;
use nagent_agents::egress::EgressClient;
use nagent_agents::egress::EgressConfig;

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::config::agents::CalDavConfig;
use crate::probe::{write_probe_audit, ProbeConfig, ProbeOutcome, ProbeState};

/// Request body shape.
#[derive(Debug, Deserialize)]
pub struct ProbeCalendarsBody {
    pub principal_url: String,
    pub username: String,
    pub password: String,
}

/// One calendar entry in the response.
#[derive(Debug, Serialize)]
pub struct CalendarEntry {
    pub href: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctag: Option<String>,
}

/// JSON shape returned by the probe endpoint.
#[derive(Debug, Serialize)]
pub struct ProbeResponse {
    pub data: Vec<CalendarEntry>,
}

#[derive(Debug, Serialize)]
struct ProbeErrorBody {
    error: String,
}

/// Handler: probe the CalDAV server with the supplied
/// credentials and return the discovered calendars.
pub async fn probe_calendars(
    State(state): State<ProbeState<CalDavConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeCalendarsBody>,
) -> Result<Response, AuthError> {
    // Parse the principal URL first (cheap, no HTTP).
    let principal_url = match Url::parse(&body.principal_url) {
        Ok(u) => u,
        Err(e) => {
            write_probe_audit(
                &state,
                user.id,
                &body.principal_url,
                ProbeOutcome::InvalidUrl,
            );
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid principal_url: {e}"),
            ));
        }
    };
    if !matches!(principal_url.scheme(), "http" | "https") {
        write_probe_audit(
            &state,
            user.id,
            &body.principal_url,
            ProbeOutcome::UnsupportedScheme,
        );
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported scheme `{}` (only http and https are accepted)",
                principal_url.scheme()
            ),
        ));
    }
    let host = principal_url
        .host_str()
        .ok_or_else(|| AuthError::BadRequest("principal_url has no host".into()))?
        .to_string();
    if state.cfg.allowlist.is_empty() {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::AllowlistEmpty);
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "CALDAV_ALLOWLIST is empty; the operator must configure the CalDAV allowlist \
             before the probe endpoint will accept calls"
                .into(),
        ));
    }
    if !host_matches_allowlist(&host, state.cfg.allowlist()) {
        write_probe_audit(&state, user.id, &host, ProbeOutcome::NotInAllowlist);
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            format!("host `{host}` is not in the CalDAV allowlist"),
        ));
    }

    let egress = EgressClient::new(EgressConfig {
        timeout_ms: state.cfg.timeout_ms(),
        allow_public: true,
        allowlist: state.cfg.allowlist().to_vec(),
        max_bytes: state.cfg.max_body_bytes(),
        user_agent: format!(
            "nagent-caldav-probe/{} (+https://github.com/nagent/nagent)",
            env!("CARGO_PKG_VERSION")
        ),
    });
    let auth_header = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", body.username, body.password).as_bytes())
    );
    let body_xml = nagent_agents::agents::caldav::build_propfind_body();

    // Step 1 of the discovery chain (RFC 4791 §5.2, RFC 5397):
    // ask the user-supplied URL for its own properties,
    // including `<C:calendar-home-set>`. If the resource is a
    // CalDAV principal, that property points to the user's
    // calendar home — the URL we actually want to PROPFIND
    // with `Depth: 1` to enumerate calendars. If the
    // property is absent, the resource is already a calendar
    // home (or similar) and we list calendars directly.
    let initial = match propfind(&egress, &principal_url, 0, &auth_header, &body_xml).await {
        Ok(r) => r,
        Err(ProbeError::Validation(e)) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::ValidationFailed);
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                format!("validate: {e}"),
            ));
        }
        Err(ProbeError::Transport(e)) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::TransportError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("upstream: {e}"),
            ));
        }
        Err(ProbeError::AuthRejected(body)) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::AuthRejected);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("CalDAV server rejected the credentials: {body}"),
            ));
        }
        Err(ProbeError::Upstream(status, body)) => {
            write_probe_audit(&state, user.id, &host, ProbeOutcome::UpstreamError);
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("CalDAV returned {status}: {body}"),
            ));
        }
    };

    let calendar_home_hrefs =
        nagent_agents::agents::caldav::extract_calendar_home_hrefs(&initial, &principal_url);

    // Decide which URLs to PROPFIND for the actual calendar
    // list. If the user pasted a principal URL, the
    // `calendar-home-set` property tells us where the user's
    // calendar home is; otherwise we treat the user URL as
    // a calendar home and list its children directly.
    let listing_urls: Vec<Url> = if calendar_home_hrefs.is_empty() {
        vec![principal_url.clone()]
    } else {
        // Validate each chain URL against the allowlist so the
        // upstream cannot redirect us to an unrelated host
        // through a malicious `calendar-home-set` response.
        let mut parsed = Vec::with_capacity(calendar_home_hrefs.len());
        for href in calendar_home_hrefs {
            let url = match Url::parse(&href) {
                Ok(u) => u,
                Err(e) => {
                    write_probe_audit(&state, user.id, &host, ProbeOutcome::InvalidUrl);
                    return Ok(error_response(
                        StatusCode::BAD_GATEWAY,
                        format!("upstream returned an invalid calendar-home URL `{href}`: {e}"),
                    ));
                }
            };
            if let Err(e) = egress.validate(url.as_str()).await {
                write_probe_audit(
                    &state,
                    user.id,
                    url.host_str().unwrap_or(""),
                    ProbeOutcome::NotInAllowlist,
                );
                return Ok(error_response(
                    StatusCode::FORBIDDEN,
                    format!(
                        "calendar-home host `{}` is not in the CalDAV allowlist: {e}",
                        url.host_str().unwrap_or("")
                    ),
                ));
            }
            parsed.push(url);
        }
        parsed
    };

    // Step 2: PROPFIND each calendar home with `Depth: 1` to
    // enumerate the user's calendars. Concatenate the parsed
    // responses and dedupe by href.
    let mut all_responses: Vec<nagent_agents::agents::caldav::PropfindResponse> = Vec::new();
    for url in &listing_urls {
        let xml = match propfind(&egress, url, 1, &auth_header, &body_xml).await {
            Ok(r) => r,
            Err(ProbeError::Validation(e)) => {
                write_probe_audit(
                    &state,
                    user.id,
                    url.host_str().unwrap_or(""),
                    ProbeOutcome::ValidationFailed,
                );
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    format!("validate: {e}"),
                ));
            }
            Err(ProbeError::Transport(e)) => {
                write_probe_audit(
                    &state,
                    user.id,
                    url.host_str().unwrap_or(""),
                    ProbeOutcome::TransportError,
                );
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("upstream: {e}"),
                ));
            }
            Err(ProbeError::AuthRejected(body)) => {
                write_probe_audit(
                    &state,
                    user.id,
                    url.host_str().unwrap_or(""),
                    ProbeOutcome::AuthRejected,
                );
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("CalDAV server rejected the credentials: {body}"),
                ));
            }
            Err(ProbeError::Upstream(status, body)) => {
                write_probe_audit(
                    &state,
                    user.id,
                    url.host_str().unwrap_or(""),
                    ProbeOutcome::UpstreamError,
                );
                return Ok(error_response(
                    StatusCode::BAD_GATEWAY,
                    format!("CalDAV returned {status}: {body}"),
                ));
            }
        };
        let responses = nagent_agents::agents::caldav::extract_propfind_responses(&xml, url);
        for r in responses {
            if r.is_calendar && !all_responses.iter().any(|prev| prev.href == r.href) {
                all_responses.push(r);
            }
        }
    }

    let mut data: Vec<CalendarEntry> = all_responses
        .into_iter()
        .map(|r| CalendarEntry {
            href: r.href,
            display_name: r.display_name,
            ctag: r.ctag,
        })
        .collect();
    data.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then(a.href.cmp(&b.href))
    });
    write_probe_audit(&state, user.id, &host, ProbeOutcome::Ok);
    Ok(Json(ProbeResponse { data }).into_response())
}

/// One PROPFIND request against the CalDAV upstream, with
/// the allowlist enforced, the status checked, and the body
/// decoded as UTF-8. Pulled out of `probe_calendars` so the
/// two-step discovery chain (principal, then calendar home)
/// shares the same error-handling.
async fn propfind(
    egress: &EgressClient,
    url: &Url,
    depth: u8,
    auth_header: &str,
    body: &str,
) -> Result<String, ProbeError> {
    use nagent_agents::egress::EgressError;
    let validated = egress.validate(url.as_str()).await.map_err(|e| match e {
        EgressError::NotInAllowlist { host } => {
            ProbeError::Validation(format!("host `{host}` is not in the CalDAV allowlist"))
        }
        other => ProbeError::Validation(other.to_string()),
    })?;
    let resp = egress
        .inner()
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").expect("PROPFIND"),
            validated.url.as_str(),
        )
        .header(reqwest::header::AUTHORIZATION, auth_header)
        .header("Depth", depth.to_string())
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| ProbeError::Transport(format!("connect: {e}")))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ProbeError::Transport(format!("read: {e}")))?;
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(ProbeError::AuthRejected(
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    if !status.is_success() && status.as_u16() != 207 {
        return Err(ProbeError::Upstream(
            status,
            String::from_utf8_lossy(&bytes).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Internal error type for the `propfind` helper so the
/// discovery chain can share the same status / body handling
/// without `probe_calendars` growing a forest of `match`
/// branches per URL.
enum ProbeError {
    Validation(String),
    Transport(String),
    AuthRejected(String),
    Upstream(StatusCode, String),
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ProbeErrorBody { error: message })).into_response()
}

/// Build the probe router subtree. The function is gated
/// on the `caldav-agent` cargo feature — a build without
/// the feature does not link the route, and the integrations
/// UI sees the connector as `configured: false`.
pub fn build_caldav_probe_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    // Mirror the credentials router pattern: the handler's
    // `State<ProbeState<CalDavConfig>>` extractor is wired
    // through `with_state` so the returned router is
    // `Router<Arc<AppState>>` (phantom-state, same shape as
    // every other mount_* helper in `http/mod.rs`).
    let probe_state = ProbeState::<CalDavConfig> {
        cfg: Arc::new(state.config.agents.caldav.clone()),
        store: state
            .auth
            .as_ref()
            .expect("caldav probe requires auth to be enabled")
            .store
            .clone(),
    };
    axum::Router::new()
        .route(
            "/api/integrations/caldav/probe-calendars",
            axum::routing::post(probe_calendars),
        )
        .with_state(probe_state)
}

// Re-export the audit / outcome helpers so the integration
// tests in `tests/caldav_e2e.rs` can build a fully-typed
// assertion without depending on the private `crate::probe`
// re-exports.
pub use crate::probe::{ProbeOutcome as PublicProbeOutcome, ProbeState as PublicProbeState};
