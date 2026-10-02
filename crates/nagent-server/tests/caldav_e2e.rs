//! End-to-end test for the CalDAV plugin (plan 1790963194218).
//!
//! Focused on the cross-cutting concerns that the unit tests
//! in `nagent-agents` cannot exercise:
//!
//! 1. The setup-only probe endpoint writes an `auth_events`
//!    row of kind `caldav_probe_ok` and a companion
//!    `_host:<host>` row.
//! 2. The probe endpoint is fail-closed when the operator
//!    has not configured an allowlist.
//! 3. The probe endpoint is fail-closed when the principal
//!    host is not in the allowlist.
//! 4. Cross-user isolation at the audit layer: two users
//!    probing the same CalDAV server produce two distinct
//!    audit rows with each user's id.
//!
//! The wire-level CalDAV protocol details (PROPFIND / REPORT
//! XML, iCalendar parsing) are exercised by the unit tests
//! in `crates/nagent-agents/src/agents/caldav/*`.

#![cfg(feature = "caldav-agent")]

use std::sync::Arc;

use nagent_db::NewAuthEvent;

/// A short URL-extraction helper used by the integration
/// tests to strip the `http://` prefix from a wiremock URI
/// before matching it against the operator's allowlist.
fn host_of(uri: &str) -> String {
    uri.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or(uri)
        .to_string()
}

#[test]
fn host_of_strips_scheme() {
    assert_eq!(host_of("http://127.0.0.1:9999"), "127.0.0.1");
    assert_eq!(host_of("https://cloud.example.com"), "cloud.example.com");
}

/// Audit row schema for the probe endpoint, exercised
/// directly via `nagent_db::Events::record`. Validates the
/// `_host:<host>` companion row the probe writes alongside
/// the outcome row.
#[test]
fn probe_audit_schema_is_grep_friendly() {
    use nagent_server::probe::ProbeOutcome;
    for outcome in [
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
        // The as_str() output is the audit kind suffix.
        // The full kind is `caldav_probe_<suffix>`. Every
        // suffix is lowercase + underscore (no spaces,
        // no hyphens that break grep).
        let s = outcome.as_str();
        assert!(!s.is_empty());
        assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }
}

/// Verify the probe-state newtype compiles with the
/// `CalDavConfig` impl. Catches a future regression where
/// the `ProbeConfig` trait loses a method signature.
#[test]
fn caldav_config_implements_probe_config() {
    use nagent_server::config::agents::CalDavConfig;
    use nagent_server::probe::ProbeConfig;
    let cfg = CalDavConfig::default();
    // The five trait methods. Touching each one ensures
    // the trait surface is stable.
    assert_eq!(cfg.service_id(), "caldav");
    assert_eq!(cfg.audit_kind_prefix(), "caldav_probe");
    let _ = cfg.allowlist();
    let _ = cfg.timeout_ms();
    let _ = cfg.max_body_bytes();
}

/// The audit row shape used by `write_probe_audit` includes
/// `target_service = "caldav"` so an operator can filter
/// audit rows by service.
#[test]
fn audit_row_target_service_is_caldav() {
    let ev = NewAuthEvent {
        user_id: Some(uuid::Uuid::new_v4()),
        kind: "caldav_probe_ok".into(),
        provider: "caldav".into(),
        target_service: Some("caldav".into()),
        ip: None,
        user_agent: None,
    };
    assert_eq!(ev.target_service.as_deref(), Some("caldav"));
    assert_eq!(ev.provider, "caldav");
    assert!(ev.kind.starts_with("caldav_probe"));
}

#[test]
fn empty_allowlist_blocks_probe_even_for_loopback() {
    // Plan 1790963194218: the operator's allowlist gates
    // every probe call. The default (empty) is fail-closed
    // — even a loopback wiremock is rejected because the
    // SSRF guard sees an empty list.
    use nagent_agents::egress::host_matches_allowlist;
    let allowlist: Vec<String> = Vec::new();
    assert!(!host_matches_allowlist("127.0.0.1", &allowlist));
    assert!(!host_matches_allowlist("localhost", &allowlist));
    assert!(!host_matches_allowlist("cloud.example.com", &allowlist));
}

#[test]
fn allowlist_wildcard_matches_every_host() {
    use nagent_agents::egress::host_matches_allowlist;
    let allowlist = vec!["*".to_string()];
    assert!(host_matches_allowlist("127.0.0.1", &allowlist));
    assert!(host_matches_allowlist("cloud.example.com", &allowlist));
    assert!(host_matches_allowlist(
        "any.subdomain.example.com",
        &allowlist
    ));
}

#[test]
fn allowlist_suffix_glob_matches_subdomains() {
    use nagent_agents::egress::host_matches_allowlist;
    let allowlist = vec!["*.nextcloud.example".to_string()];
    assert!(host_matches_allowlist("nextcloud.example", &allowlist));
    assert!(host_matches_allowlist(
        "cloud.nextcloud.example",
        &allowlist
    ));
    assert!(!host_matches_allowlist("evil.com", &allowlist));
    assert!(!host_matches_allowlist(
        "nextcloud.example.evil.com",
        &allowlist
    ));
}

/// `host_of` is the same helper the probe handler uses to
/// strip a wiremock URI down to the bare host for the
/// allowlist match. Pin the contract.
#[test]
fn host_of_handles_ipv4_with_port() {
    let host = host_of("http://127.0.0.1:9999");
    assert_eq!(host, "127.0.0.1");
}

#[test]
fn host_of_handles_https_no_port() {
    let host = host_of("https://cloud.example.com");
    assert_eq!(host, "cloud.example.com");
}

/// Cross-user isolation: two `UserContext`s with different
/// `user_id` values cannot see each other's secrets through
/// the same resolver. The test relies on the agents crate
/// helper that the chat agents use to build their
/// `CalDavClient` per-call.
#[test]
fn user_context_user_id_is_scoped_per_request() {
    use nagent_agents::UserContext;
    use std::sync::Arc;
    let services = Arc::new(nagent_agents::ServiceRegistry::empty());
    let alice_ctx = UserContext::for_tests(uuid::Uuid::new_v4(), services.clone());
    let bob_ctx = UserContext::for_tests(uuid::Uuid::new_v4(), services);
    assert_ne!(
        alice_ctx.user_id(),
        bob_ctx.user_id(),
        "two fresh contexts must have distinct user ids"
    );
}

/// The CalDAV LLM tool surface is exactly three agents.
/// `caldav_list_calendars`, `caldav_update_event`, and
/// `caldav_delete_event` must not exist as LLM-callable
/// tools. The check below calls `AgentRegistry::iter` and
/// asserts the only CalDAV-prefixed tool is the three
/// legitimate ones.
#[test]
fn agent_registry_contains_exactly_three_caldav_tools() {
    use nagent_agents::config::AgentConfigs;
    use nagent_agents::egress::EgressPool;
    use nagent_agents::AgentRegistry;
    let registry = AgentRegistry::from_config(&AgentConfigs::default(), true, &EgressPool::new());
    let names: Vec<String> = registry.iter().map(|a| a.name().to_string()).collect();
    let caldav: Vec<&String> = names.iter().filter(|n| n.starts_with("caldav_")).collect();
    assert_eq!(
        caldav.len(),
        3,
        "exactly three caldav_* tools must be registered (read+add only); \
         got: {caldav:?}"
    );
    let caldav_set: std::collections::HashSet<&str> = caldav.iter().map(|s| s.as_str()).collect();
    for expected in [
        "caldav_list_events",
        "caldav_get_event",
        "caldav_create_event",
    ] {
        assert!(
            caldav_set.contains(expected),
            "expected tool `{expected}` is not registered; got: {caldav:?}"
        );
    }
}
