//! Generic "setup-only probe" abstraction for per-user
//! integrations (plan 1790963194218, §2.7 "Setup-only helpers").
//!
//! A *probe* is a setup-only HTTP endpoint the integrations
//! UI uses to discover remote resources (e.g. a CalDAV
//! calendar collection) **before** the user has saved the
//! resource URL into the per-user vault. Probes share three
//! properties:
//!
//! 1. They run on the server with credentials supplied in
//!    the request body (not from the vault — the user is
//!    choosing what to save into the vault).
//! 2. They validate the principal URL against an
//!    operator-set hostname allowlist. The default is
//!    fail-closed (empty allowlist refuses every call).
//! 3. They write one `auth_events` audit row per call
//!    with a kind derived from the service id.
//!
//! The first concrete probe is CalDAV. CardDAV will reuse
//! the same plumbing via a different `ProbeConfig` impl; the
//! `ProbeState<C>` newtype is the single shared state.

use std::sync::Arc;

use axum::extract::FromRef;
use nagent_db::NewAuthEvent;

use crate::config::agents::CalDavConfig;
use crate::state::AppState;

/// Trait implemented by every per-integration config that
/// exposes a setup-only HTTP probe.
///
/// The trait deliberately exposes **only** the fields a probe
/// needs: the service id (for the audit row), the audit
/// kind prefix, the hostname allowlist, the upstream
/// timeout, and the body-size cap. Adding a new probe
/// (e.g. CardDAV) means writing a new `impl ProbeConfig
/// for CardDavConfig` and a new `FromRef<Arc<AppState>>
/// for ProbeState<CardDavConfig>` — no other code changes.
pub trait ProbeConfig: Send + Sync + 'static {
    /// Stable service id (e.g. `"caldav"`). Echoed on the
    /// `auth_events.target_service` column so an operator
    /// can filter audit rows by service.
    fn service_id(&self) -> &'static str;

    /// Audit kind prefix (e.g. `"caldav_probe"`). The full
    /// kind written to `auth_events` is
    /// `{prefix}_{outcome}` where `outcome` is one of
    /// `ok`, `invalid_url`, `unsupported_scheme`,
    /// `allowlist_empty`, `not_in_allowlist`, ….
    fn audit_kind_prefix(&self) -> &'static str;

    /// Hostname allow-list applied by the SSRF guard. When
    /// non-empty, only the listed hosts (or their
    /// subdomains, for `*.foo` entries) may be reached.
    /// When empty, the probe refuses every call.
    fn allowlist(&self) -> &[String];

    /// Per-request connect+read timeout, in milliseconds.
    fn timeout_ms(&self) -> u64;

    /// Cap on the iCalendar / PROPFIND response body the
    /// probe will accept, in bytes.
    fn max_body_bytes(&self) -> usize;
}

impl ProbeConfig for CalDavConfig {
    fn service_id(&self) -> &'static str {
        "caldav"
    }
    fn audit_kind_prefix(&self) -> &'static str {
        "caldav_probe"
    }
    fn allowlist(&self) -> &[String] {
        &self.allowlist
    }
    fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
    fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }
}

/// Per-route state for any setup-only probe handler. The
/// `cfg` field is the agent's per-integration config (so
/// the SSRF allowlist is identical to the chat agents'
/// guard); `store` is the shared `nagent_db::Db` for audit
/// writes.
#[derive(Clone)]
pub struct ProbeState<C: ProbeConfig> {
    /// The agent's per-integration config.
    pub cfg: Arc<C>,
    /// The shared `nagent_db::Db` for the audit row.
    pub store: nagent_db::Db,
}

impl<C: ProbeConfig> std::fmt::Debug for ProbeState<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeState")
            .field("cfg", &"<config>")
            .field("store", &"<nagent_db::Db>")
            .finish()
    }
}

impl FromRef<Arc<AppState>> for ProbeState<CalDavConfig> {
    fn from_ref(state: &Arc<AppState>) -> Self {
        // The probe handler is mounted only when auth is
        // enabled (the auth subtree is the parent). A
        // missing auth store is a wiring bug — panic in
        // debug builds; in production the auth subtree
        // would never have been mounted.
        let store = state
            .auth
            .as_ref()
            .expect("caldav probe requires auth to be enabled")
            .store
            .clone();
        Self {
            cfg: Arc::new(state.config.agents.caldav.clone()),
            store,
        }
    }
}

/// Outcome kind suffixes for the `auth_events` row. The
/// caller picks one to compose the full kind
/// `{prefix}_{suffix}` (e.g. `caldav_probe_ok`,
/// `caldav_probe_not_in_allowlist`). Centralised so the
/// audit-row schema is grep-friendly.
#[derive(Debug, Clone, Copy)]
pub enum ProbeOutcome {
    /// PROPFIND / REPORT returned a 2xx (or 207) and the
    /// JSON was rendered.
    Ok,
    /// The supplied URL did not parse.
    InvalidUrl,
    /// The scheme was not `http` or `https`.
    UnsupportedScheme,
    /// The operator's allowlist is empty — every call is
    /// refused until the operator sets at least one entry.
    AllowlistEmpty,
    /// The principal host is not in the operator's allowlist.
    NotInAllowlist,
    /// DNS / SSRF pre-flight failed.
    ValidationFailed,
    /// Upstream transport error (connect / TLS / read).
    TransportError,
    /// Upstream returned 401 or 403 (auth rejected).
    AuthRejected,
    /// Upstream returned another non-2xx status.
    UpstreamError,
    /// The supplied credentials were missing the `url` field
    /// in the request body.
    BadRequest,
}

impl ProbeOutcome {
    /// Snake-case identifier used as the audit kind suffix.
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeOutcome::Ok => "ok",
            ProbeOutcome::InvalidUrl => "invalid_url",
            ProbeOutcome::UnsupportedScheme => "unsupported_scheme",
            ProbeOutcome::AllowlistEmpty => "allowlist_empty",
            ProbeOutcome::NotInAllowlist => "not_in_allowlist",
            ProbeOutcome::ValidationFailed => "validation_failed",
            ProbeOutcome::TransportError => "transport_error",
            ProbeOutcome::AuthRejected => "auth_rejected",
            ProbeOutcome::UpstreamError => "upstream_error",
            ProbeOutcome::BadRequest => "bad_request",
        }
    }
}

/// Write the audit row for a probe call. Centralised so the
/// schema (`{prefix}_{outcome}` + `_host:<host>` companion)
/// is identical across every probe integration.
pub fn write_probe_audit<C: ProbeConfig>(
    state: &ProbeState<C>,
    user_id: uuid::Uuid,
    host: &str,
    outcome: ProbeOutcome,
) {
    let full_kind = format!("{}_{}", state.cfg.audit_kind_prefix(), outcome.as_str());
    state.store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: full_kind,
        provider: state.cfg.service_id().to_string(),
        target_service: Some(state.cfg.service_id().to_string()),
        ip: None,
        user_agent: None,
    });
    if !host.is_empty() {
        state.store.admin().events.record(NewAuthEvent {
            user_id: Some(user_id),
            kind: format!("{}_host:{host}", state.cfg.audit_kind_prefix()),
            provider: state.cfg.service_id().to_string(),
            target_service: Some(state.cfg.service_id().to_string()),
            ip: None,
            user_agent: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::agents::CalDavConfig;

    #[test]
    fn caldav_probe_config_returns_stable_ids() {
        let cfg = CalDavConfig::default();
        assert_eq!(cfg.service_id(), "caldav");
        assert_eq!(cfg.audit_kind_prefix(), "caldav_probe");
    }

    #[test]
    fn empty_allowlist_is_exposed() {
        // The probe handler refuses calls when the
        // allowlist is empty. The test pins the contract.
        let cfg = CalDavConfig::default();
        assert!(cfg.allowlist().is_empty());
    }

    #[test]
    fn probe_outcome_suffixes_are_snake_case() {
        // The audit kind is grep-friendly: every suffix
        // is lowercase + underscore, no spaces.
        for s in [
            ProbeOutcome::Ok,
            ProbeOutcome::InvalidUrl,
            ProbeOutcome::UnsupportedScheme,
            ProbeOutcome::AllowlistEmpty,
            ProbeOutcome::NotInAllowlist,
            ProbeOutcome::ValidationFailed,
            ProbeOutcome::TransportError,
            ProbeOutcome::AuthRejected,
            ProbeOutcome::UpstreamError,
            ProbeOutcome::BadRequest,
        ] {
            let s = s.as_str();
            assert!(!s.is_empty());
            assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }
}
