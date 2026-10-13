//! `POST /api/integrations/email_imap/probe` and
//! `POST /api/integrations/email_smtp/probe` — setup-only helpers
//! used by the integrations UI to validate the user's IMAP / SMTP
//! credentials before saving them into the per-user vault.
//!
//! The endpoints are **not** exposed as LLM tools; the chat tool
//! loop returns "unknown tool" if it tries. Each handler runs on
//! the server with credentials supplied in the request body —
//! it does **not** read the vault (the user is choosing what to
//! save into the vault).
//!
//! ## Hardening
//!
//! The host is validated against the same `EmailConfig::allowlist`
//! the chat agents use. The default `allowlist` is empty, so the
//! endpoints refuse every call until the operator sets
//! `EMAIL_ALLOWLIST` — fail-closed.
//!
//! ## Audit
//!
//! Every call writes one `auth_events` row of kind
//! `email_<service>_probe_<outcome>` (plus a `_host:<host>`
//! companion row) so an operator can correlate probe attempts
//! with the per-user vault. The password never lands on the audit
//! row.
//!
//! ## v1 status
//!
//! Plan 1791566269874 splits the email family into six phases; the
//! v1 commit ships the **endpoint wiring, audit rows, allowlist
//! validation, and the JSON shape** end-to-end. The actual
//! TCP+TLS+IMAP/SMTP wire layer for the probe is a follow-up
//! commit (Phases 2 and 3 of the plan). v1 behaviour: every call
//! to the probe returns `ProbeOutcome::WireLayerNotImplemented`
//! so the operator gets a clear "feature compiled in but wire
//! layer pending" error instead of a silent success or a network
//! failure. The two endpoints share the same JSON shape so a
//! client that switches from one to the other does not have to
//! re-encode the response.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use nagent_agents::agents::email::{EmailImap, EmailSmtp};
use nagent_agents::egress::host_matches_allowlist;

use crate::auth::error::AuthError;
use crate::auth::AuthUser;
use crate::config::agents::EmailConfig;
use crate::probe::{ProbeConfig, ProbeOutcome, ProbeState};

/// Request body for the IMAP probe.
#[derive(Debug, Deserialize)]
pub struct ProbeImapBody {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// One of `implicit`, `starttls`, `none`. The probe
    /// validates the value against the operator-set
    /// `EMAIL_ALLOW_CLEARTEXT_IMAP` knob.
    pub tls_mode: String,
}

/// Request body for the SMTP probe.
#[derive(Debug, Deserialize)]
pub struct ProbeSmtpBody {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub tls_mode: String,
    pub from_address: String,
}

/// JSON shape returned by the IMAP probe.
#[derive(Debug, Serialize)]
pub struct ProbeImapResponse {
    /// Mailboxes the IMAP server advertised on `LIST "" *"`
    /// (always empty in v1 — wire layer is a follow-up).
    pub mailboxes: Vec<MailboxEntry>,
    /// Server capabilities (`IMAP4rev1`, `STARTTLS`, `AUTH=PLAIN`,
    /// `CONDSTORE`, …). Empty in v1.
    pub capabilities: Vec<String>,
    /// `true` when the probe round-trip succeeded. `false` in
    /// v1 — the wire layer is a follow-up.
    pub imap_ok: bool,
}

/// One mailbox entry surfaced by the IMAP probe.
#[derive(Debug, Serialize)]
pub struct MailboxEntry {
    pub name: String,
    pub uidnext: Option<u32>,
    pub total: Option<u32>,
}

/// JSON shape returned by the SMTP probe.
#[derive(Debug, Serialize)]
pub struct ProbeSmtpResponse {
    /// Server capabilities (`SIZE 10485760`, `8BITMIME`,
    /// `STARTTLS`, `AUTH PLAIN LOGIN`, …). Empty in v1.
    pub server_capabilities: Vec<String>,
    /// `true` when the `MAIL FROM:<from_address>` round-trip
    /// succeeded. `false` in v1.
    pub smtp_ok: bool,
}

#[derive(Debug, Serialize)]
struct ProbeErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: String) -> Response {
    (status, Json(ProbeErrorBody { error: message })).into_response()
}

/// Validate the host / port / allowlist triplet shared by both
/// probe endpoints. Returns the validated host string on
/// success; the response is otherwise an `Err` carrying the
/// already-audited error response. Generic over the
/// `ProbeConfig` impl so the IMAP and SMTP probes share the
/// helper even though they reach it through different state
/// types — each handler's `C` impl points at the right
/// allow-list (`imap_allowlist` vs `smtp_allowlist`).
async fn validate_host<C: ProbeConfig>(
    state: &ProbeState<C>,
    user_id: uuid::Uuid,
    host: &str,
    port: u16,
    audit_kind: &str,
) -> Result<String, Response> {
    if host.is_empty() {
        write_probe_audit_with_kind(state, user_id, host, audit_kind, ProbeOutcome::InvalidUrl);
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "host is required".into(),
        ));
    }
    if !is_valid_host(host) {
        write_probe_audit_with_kind(state, user_id, host, audit_kind, ProbeOutcome::InvalidUrl);
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("host `{host}` is not a valid hostname or IP"),
        ));
    }
    if port == 0 {
        write_probe_audit_with_kind(state, user_id, host, audit_kind, ProbeOutcome::InvalidUrl);
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "port must be a positive integer".into(),
        ));
    }
    if state.cfg.allowlist().is_empty() {
        write_probe_audit_with_kind(
            state,
            user_id,
            host,
            audit_kind,
            ProbeOutcome::AllowlistEmpty,
        );
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "EMAIL_ALLOWLIST is empty; the operator must configure the email allowlist before \
             the probe endpoint will accept calls"
                .into(),
        ));
    }
    if !host_matches_allowlist(host, state.cfg.allowlist()) {
        write_probe_audit_with_kind(
            state,
            user_id,
            host,
            audit_kind,
            ProbeOutcome::NotInAllowlist,
        );
        return Err(error_response(
            StatusCode::FORBIDDEN,
            format!("host `{host}` is not in the email allowlist"),
        ));
    }
    Ok(host.to_string())
}

/// Lightweight host validator. Accepts:
/// - IPv4 (e.g. `192.0.2.1`)
/// - IPv6 (e.g. `2001:db8::1`)
/// - DNS hostnames (RFC 1123 §2.1: 1-253 chars, labels
///   separated by `.`, each label 1-63 chars, alphanumeric +
///   hyphens, no leading/trailing hyphen).
fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
    }
    true
}

/// Handler: probe the IMAP server with the supplied credentials.
/// Calls the real IMAP wire client (Phases 3-5 of
/// plan 1791566269874 landed in the same commit). On success
/// the response is a `ProbeImapResponse` with the server's
/// mailboxes + capabilities; on failure the response is a
/// JSON error body with the wire client's diagnosis.
pub async fn probe_imap(
    State(state): State<ProbeState<EmailConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeImapBody>,
) -> Result<Response, AuthError> {
    let audit_kind = "email_imap_probe";
    let _host = match validate_host(&state, user.id, &body.host, body.port, audit_kind).await {
        Ok(h) => h,
        Err(resp) => return Ok(resp),
    };
    // Reject the cleartext posture unless the operator has
    // explicitly opted in.
    let tls_mode = body.tls_mode.to_ascii_lowercase();
    if matches!(tls_mode.as_str(), "none" | "plain" | "cleartext")
        && !state.cfg.allow_cleartext_imap
    {
        write_probe_audit_with_kind(
            &state,
            user.id,
            &body.host,
            audit_kind,
            ProbeOutcome::ValidationFailed,
        );
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "clear-text IMAP is refused by default; set `[email].allow_cleartext_imap = true` \
             on the server to enable"
                .into(),
        ));
    }
    // Build a tiny `EmailAgentConfig` (the wire client
    // only reads `timeout_ms` + `max_body_bytes`) and
    // hand it to the IMAP capability. The `From` impl
    // projects the server's `EmailConfig` down to the
    // agents-crate shape — see the comment on the `From`
    // for the field-level rationale.
    let agent_cfg = Arc::new(nagent_agents::EmailAgentConfig::from(
        crate::config::agents::EmailConfig::from_env_with_toml(None)
            .map_err(|e| AuthError::Internal(e.to_string()))?,
    ));
    let imap_impl = nagent_agents::agents::email::EmailImapImpl::new(agent_cfg);
    let imap_cfg = nagent_agents::agents::email::ImapConnectConfig {
        host: body.host.clone(),
        port: body.port,
        tls_mode: nagent_agents::agents::email::ImapTlsMode::parse(&body.tls_mode)
            .map_err(|e| AuthError::Internal(e.to_string()))?,
        username: body.username.clone(),
        password: body.password.clone(),
    };
    let user_id = user.id;
    match imap_impl.list_mailboxes(user_id, &imap_cfg).await {
        Ok(mailboxes) => {
            write_probe_audit_with_kind(&state, user.id, &body.host, audit_kind, ProbeOutcome::Ok);
            Ok((
                StatusCode::OK,
                Json(ProbeImapResponse {
                    mailboxes: mailboxes
                        .into_iter()
                        .map(|m| MailboxEntry {
                            name: m.name,
                            uidnext: None,
                            total: if m.exists >= 0 {
                                Some(m.exists as u32)
                            } else {
                                None
                            },
                        })
                        .collect(),
                    capabilities: Vec::new(),
                    imap_ok: true,
                }),
            )
                .into_response())
        }
        Err(e) => {
            write_probe_audit_with_kind(
                &state,
                user.id,
                &body.host,
                audit_kind,
                ProbeOutcome::AuthRejected,
            );
            Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("IMAP probe failed: {e}"),
            ))
        }
    }
}

/// Handler: probe the SMTP server with the supplied credentials.
/// Mirrors `probe_imap` for the submission side; the wire layer
/// lands in plan Phase 5.
///
/// The handler's state type is `ProbeState<EmailSmtpConfig>` (not
/// the bare `EmailConfig` the IMAP handler uses) so the SMTP
/// allow-list is the one checked against the user's host. The
/// `FromRef` impl on `ProbeState<EmailSmtpConfig>` wraps the
/// same `EmailConfig` in a thin newtype; both `ProbeConfig`
/// impls share the audit-row shape so the operator can grep
/// `email_imap_probe_*` and `email_smtp_probe_*` rows
/// independently.
pub async fn probe_smtp(
    State(state): State<ProbeState<crate::probe::EmailSmtpConfig>>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ProbeSmtpBody>,
) -> Result<Response, AuthError> {
    let audit_kind = "email_smtp_probe";
    let _host = match validate_host(&state, user.id, &body.host, body.port, audit_kind).await {
        Ok(h) => h,
        Err(resp) => return Ok(resp),
    };
    let tls_mode = body.tls_mode.to_ascii_lowercase();
    if matches!(tls_mode.as_str(), "none" | "plain" | "cleartext")
        && !state.cfg.allow_cleartext_smtp
    {
        write_probe_audit_with_kind(
            &state,
            user.id,
            &body.host,
            audit_kind,
            ProbeOutcome::ValidationFailed,
        );
        return Ok(error_response(
            StatusCode::FORBIDDEN,
            "clear-text SMTP is refused by default; set `[email].allow_cleartext_smtp = true` \
             on the server to enable"
                .into(),
        ));
    }
    if body.from_address.is_empty() || !body.from_address.contains('@') {
        write_probe_audit_with_kind(
            &state,
            user.id,
            &body.host,
            audit_kind,
            ProbeOutcome::BadRequest,
        );
        return Ok(error_response(
            StatusCode::BAD_REQUEST,
            "from_address must be a non-empty RFC 5322 address".into(),
        ));
    }
    // Wire the SMTP capability through the real client
    // (Phase 5 of plan 1791566269874). The probe
    // round-trips `EHLO` / `STARTTLS` / `AUTH PLAIN` /
    // `MAIL FROM:<from>` so a misconfigured `from_address`
    // surfaces a 553-class error from the relay BEFORE the
    // user can ask the LLM to send.
    let agent_cfg = Arc::new(nagent_agents::EmailAgentConfig::from(
        crate::config::agents::EmailConfig::from_env_with_toml(None)
            .map_err(|e| AuthError::Internal(e.to_string()))?,
    ));
    let smtp_impl = nagent_agents::agents::email::EmailSmtpImpl::new(agent_cfg);
    let smtp_cfg = nagent_agents::agents::email::SmtpConnectConfig {
        host: body.host.clone(),
        port: body.port,
        tls_mode: nagent_agents::agents::email::SmtpTlsMode::parse(&body.tls_mode)
            .map_err(|e| AuthError::Internal(e.to_string()))?,
        username: body.username.clone(),
        password: body.password.clone(),
        from_address: body.from_address.clone(),
    };
    let user_id = user.id;
    // Open a session, run MAIL FROM, and immediately close
    // (no recipient is supplied on the probe path — the
    // server's response to MAIL FROM alone is enough to
    // know whether the relay will accept the envelope
    // sender).
    let client = nagent_agents::agents::email::SmtpClient::new(state.cfg.timeout_ms);
    let helo = state.cfg.default_probe_helo.clone();
    let mut session = match client
        .connect(
            &smtp_cfg.host,
            smtp_cfg.port,
            smtp_cfg.tls_mode,
            &smtp_cfg.username,
            &smtp_cfg.password,
            &helo,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            write_probe_audit_with_kind(
                &state,
                user.id,
                &body.host,
                audit_kind,
                ProbeOutcome::TransportError,
            );
            return Ok(error_response(
                StatusCode::BAD_GATEWAY,
                format!("SMTP probe failed: {e}"),
            ));
        }
    };
    if let Err(e) = nagent_agents::agents::email::client_smtp::mail_from_probe(
        &mut session,
        &smtp_cfg.from_address,
    )
    .await
    {
        write_probe_audit_with_kind(
            &state,
            user.id,
            &body.host,
            audit_kind,
            ProbeOutcome::AuthRejected,
        );
        return Ok(error_response(
            StatusCode::BAD_GATEWAY,
            format!("SMTP MAIL FROM rejected: {e}"),
        ));
    }
    write_probe_audit_with_kind(&state, user.id, &body.host, audit_kind, ProbeOutcome::Ok);
    Ok((
        StatusCode::OK,
        Json(ProbeSmtpResponse {
            server_capabilities: Vec::new(),
            smtp_ok: true,
        }),
    )
        .into_response())
}

/// Write an audit row with a non-default `audit_kind_prefix` —
/// the [`crate::probe::write_probe_audit`] helper bakes the
/// per-`ProbeConfig` `audit_kind_prefix()` value into the row,
/// so both probe endpoints override the kind explicitly
/// (`"email_imap_probe"` / `"email_smtp_probe"`). Centralised
/// so the audit row schema (`{prefix}_{outcome}` +
/// `_host:<host>` companion) is identical between the two
/// probe endpoints. Generic over the `ProbeConfig` impl so
/// the SMTP and IMAP handlers share the helper even though
/// they reach it through different state types.
fn write_probe_audit_with_kind<C: ProbeConfig>(
    state: &ProbeState<C>,
    user_id: uuid::Uuid,
    host: &str,
    kind_prefix: &str,
    outcome: ProbeOutcome,
) {
    let full_kind = format!("{}_{}", kind_prefix, outcome.as_str());
    state.store.admin().events.record(nagent_db::NewAuthEvent {
        user_id: Some(user_id),
        kind: full_kind,
        provider: state.cfg.service_id().to_string(),
        target_service: Some(state.cfg.service_id().to_string()),
        ip: None,
        user_agent: None,
    });
    if !host.is_empty() {
        state.store.admin().events.record(nagent_db::NewAuthEvent {
            user_id: Some(user_id),
            kind: format!("{kind_prefix}_host:{host}"),
            provider: state.cfg.service_id().to_string(),
            target_service: Some(state.cfg.service_id().to_string()),
            ip: None,
            user_agent: None,
        });
    }
}

// Suppress the unused-import warning for the generic helper
// (we hand-roll the audit row so the SMTP probe can override
// the kind prefix without duplicating the `ProbeConfig` impl).
#[allow(dead_code)]
fn _ensure_helper_used() {}

/// Build the email probe router subtree. Mirrors
/// `build_caldav_probe_router` so the two probe integrations
/// share the same shape (one subtree, feature-gated mount,
/// fail-closed when auth is off).
///
/// The IMAP and SMTP routes need **different** state types
/// (`ProbeState<EmailConfig>` vs `ProbeState<EmailSmtpConfig>`)
/// so the per-endpoint allow-list is honoured. The router is
/// built by mounting each route against its own sub-router
/// and merging them — the resulting `Router<Arc<AppState>>`
/// type matches the rest of the auth subtree.
pub fn build_email_probe_router(state: Arc<crate::AppState>) -> axum::Router<Arc<crate::AppState>> {
    let store = state
        .auth
        .as_ref()
        .expect("email probe requires auth to be enabled")
        .store
        .clone();
    // IMAP sub-router. The IMAP allow-list is the one
    // checked (the bare `EmailConfig`'s `allowlist()` method
    // returns `imap_allowlist`).
    let imap_router: axum::Router<Arc<crate::AppState>> = axum::Router::new()
        .route(
            "/api/integrations/email_imap/probe",
            axum::routing::post(probe_imap),
        )
        .with_state(ProbeState::<EmailConfig> {
            cfg: Arc::new(state.config.agents.email.clone()),
            store: store.clone(),
        });
    // SMTP sub-router. Uses the `EmailSmtpConfig` newtype so
    // its `allowlist()` points at `smtp_allowlist` instead.
    let smtp_router: axum::Router<Arc<crate::AppState>> = axum::Router::new()
        .route(
            "/api/integrations/email_smtp/probe",
            axum::routing::post(probe_smtp),
        )
        .with_state(ProbeState::<crate::probe::EmailSmtpConfig> {
            cfg: Arc::new(crate::probe::EmailSmtpConfig(
                state.config.agents.email.clone(),
            )),
            store,
        });
    imap_router.merge(smtp_router)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_validator_accepts_ipv4_ipv6_and_dns_names() {
        for h in [
            "imap.example.com",
            "fastmail.com",
            "127.0.0.1",
            "::1",
            "2001:db8::1",
            "x",
        ] {
            assert!(is_valid_host(h), "expected `{h}` to validate");
        }
    }

    #[test]
    fn host_validator_rejects_malformed_inputs() {
        for h in [
            "",
            "-leading-hyphen.example.com",
            "trailing-hyphen-.example.com",
            "with spaces.example.com",
            "very-long-label-over-sixty-three-chars-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.example.com",
        ] {
            assert!(!is_valid_host(h), "expected `{h}` to be rejected");
        }
    }

    /// Plan 1791566269874: IMAP and SMTP have **separate**
    /// hostname allow-lists. The IMAP probe must check the
    /// IMAP list; the SMTP probe must check the SMTP list.
    /// A config that allows the IMAP host but not the SMTP
    /// host (or vice versa) must reach the right probe
    /// endpoint.
    ///
    /// The test is run against the [`crate::probe::EmailSmtpConfig`]
    /// newtype so the SMTP `ProbeConfig` impl points at the
    /// SMTP allow-list. The IMAP probe uses the bare
    /// `EmailConfig` impl (defined right next to it).
    #[test]
    fn smtp_probe_allowlist_is_independent_from_imap() {
        use crate::config::agents::EmailConfig;
        use crate::probe::{EmailSmtpConfig, ProbeConfig};
        let cfg = EmailConfig {
            imap_allowlist: vec!["imap.example.com".to_string()],
            smtp_allowlist: vec!["smtp.example.com".to_string()],
            ..EmailConfig::default()
        };
        // The bare EmailConfig (IMAP path) reports the IMAP
        // allow-list.
        assert_eq!(cfg.allowlist(), &["imap.example.com".to_string()]);
        // The EmailSmtpConfig newtype reports the SMTP
        // allow-list.
        let smtp = EmailSmtpConfig(cfg);
        assert_eq!(smtp.allowlist(), &["smtp.example.com".to_string()]);
        // A host that's on the IMAP list but not the SMTP
        // list is a sandbox denial for the SMTP probe.
        assert!(nagent_agents::egress::host_matches_allowlist(
            "imap.example.com",
            &["imap.example.com".to_string()]
        ));
        assert!(!nagent_agents::egress::host_matches_allowlist(
            "imap.example.com",
            &["smtp.example.com".to_string()]
        ));
    }
}
