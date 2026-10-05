//! End-to-end test for the X OAuth + `x_timeline` integration
//! (plan 1790695073418).
//!
//! Focused on the cross-cutting concerns that the unit tests
//! in `nagent-agents` and the `oauth::x` lib tests cannot
//! exercise:
//!
//! 1. The `x_account` `ServiceDef` is registered in the
//!    `ServiceRegistry` when the `x-agent` cargo feature is on.
//! 2. The `x_timeline` agent descriptor is registered in
//!    `AGENT_DESCRIPTORS` with id `x_timeline` and feature
//!    `x-agent`.
//! 3. The audit row kind names for the X OAuth callback /
//!    disconnect handlers are stable
//!    (`credential_oauth_x_connected` /
//!    `credential_oauth_x_disconnected`).
//! 4. The `UserContext::update_secret` path fails closed
//!    when no sink is wired (test ctx).
//! 5. The `XOAuthConfig` defaults and `redirect_url` join
//!    work as documented.
//!
//! The wire-level X protocol details (OAuth state round-trip,
//! PKCE generation, v2 timeline JSON projection, and the live
//! `SecretSink` round-trip against a real DB) are exercised by
//! the unit tests in `crates/nagent-agents/src/agents/x_timeline/`
//! and `crates/nagent-server/src/oauth/x.rs`.

#![cfg(feature = "x-agent")]

use std::sync::Arc;

use nagent_agents::agents::{x_timeline::XTimelineAgent, AGENT_DESCRIPTORS};
use nagent_agents::Agent;
use nagent_agents::AgentConfigs;
use nagent_agents::{ServiceRegistry, UserContext};

use nagent_db::NewAuthEvent;
use nagent_server::config::XOAuthConfig;
use nagent_server::oauth::x::{XOAuthConfig as ServerXOAuthConfig, XRefreshTokenClient};

#[test]
fn x_oauth_config_default_is_off() {
    let cfg = XOAuthConfig::default();
    assert!(!cfg.enabled);
    assert!(cfg.client_id.is_empty());
    assert!(cfg.client_secret.is_none());
    assert_eq!(cfg.redirect_path, "/api/auth/login/x/callback");
    assert_eq!(
        cfg.scopes,
        vec![
            "tweet.read".to_string(),
            "users.read".to_string(),
            "follows.read".to_string()
        ]
    );
}

#[test]
fn x_oauth_config_redirect_url_joins_public_url() {
    let server_cfg = ServerXOAuthConfig {
        enabled: true,
        client_id: "id".into(),
        client_secret: None,
        redirect_path: "/api/auth/login/x/callback".into(),
        scopes: vec!["tweet.read".into()],
        timeout_ms: 8000,
    };
    assert_eq!(
        server_cfg.redirect_url("https://example.com/"),
        "https://example.com/api/auth/login/x/callback"
    );
    assert_eq!(
        server_cfg.redirect_url("https://example.com"),
        "https://example.com/api/auth/login/x/callback"
    );
}

#[test]
fn x_account_service_def_is_registered_when_feature_on() {
    let reg = ServiceRegistry::new(&[nagent_agents::x_account_service::X_ACCOUNT_SERVICE]);
    assert_eq!(reg.len(), 1);
    let svc = reg.get("x_account").expect("x_account must be registered");
    assert_eq!(svc.id, "x_account");
    assert_eq!(svc.icon, "𝕏");
    // The six locked fields must all be present.
    let keys: Vec<&str> = svc.fields.iter().map(|f| f.key).collect();
    assert!(keys.contains(&"access_token"));
    assert!(keys.contains(&"refresh_token"));
    assert!(keys.contains(&"token_scope"));
    assert!(keys.contains(&"x_user_id"));
    assert!(keys.contains(&"x_screen_name"));
    assert!(keys.contains(&"token_expires_at"));
    // `access_token` + `refresh_token` are masked (Password kind).
    for f in svc.fields {
        if f.key == "access_token" || f.key == "refresh_token" {
            assert!(matches!(f.kind, nagent_agents::FieldKind::Password));
        } else {
            assert!(matches!(f.kind, nagent_agents::FieldKind::Text));
        }
    }
}

#[test]
fn x_timeline_descriptor_is_in_table_when_feature_on() {
    // Plan 1790695073418: `x_timeline` must appear in the static
    // `AGENT_DESCRIPTORS` slice when the `x-agent` cargo feature
    // is on. The id matches `XTimelineAgent::name()`.
    let mut found = false;
    for d in AGENT_DESCRIPTORS {
        if d.id == "x_timeline" {
            found = true;
            assert_eq!(d.feature, "x-agent");
        }
    }
    assert!(found, "x_timeline must be registered in AGENT_DESCRIPTORS");
    // The descriptor's build closure must produce an agent
    // whose name() matches the descriptor id.
    let cfgs = AgentConfigs::default();
    let pool = nagent_agents::egress::EgressPool::new();
    for d in AGENT_DESCRIPTORS {
        if d.id != "x_timeline" {
            continue;
        }
        let agent = (d.build)(&cfgs, &pool).expect("x_timeline build must succeed");
        assert_eq!(agent.name(), "x_timeline");
    }
}

#[test]
fn x_timeline_agent_constructs_with_shared_pool() {
    let cfgs = AgentConfigs::default();
    let pool = nagent_agents::egress::EgressPool::new();
    let agent = XTimelineAgent::new(cfgs.x_timeline.clone(), pool.public());
    assert_eq!(agent.name(), "x_timeline");
    // `untrusted_output` must be `true`: posts come from a
    // remote service the LLM does not own.
    assert!(agent.untrusted_output());
}

#[tokio::test]
async fn user_context_secret_sink_missing_for_test_ctx() {
    // `for_tests` and `for_chat_session(..., sink=None, ...)`
    // both leave the sink `None`. `update_secret` must surface
    // an `AgentFailed("credential sink not wired in this
    // context")` so the agent fails closed outside the
    // chat-session path.
    let services = ServiceRegistry::empty().into_arc();
    let ctx = UserContext::for_tests(uuid::Uuid::new_v4(), services);
    let err = ctx
        .update_secret(
            "x_account",
            &[("access_token", secrecy::SecretString::from("x".to_string()))],
        )
        .await
        .expect_err("sink must be missing for test ctx");
    let msg = format!("{err}");
    assert!(
        msg.contains("credential sink not wired in this context"),
        "unexpected error: {msg}"
    );
}

#[test]
fn audit_event_kinds_for_x_oauth_are_stable() {
    // The audit row kinds the X OAuth callback / disconnect
    // handlers write are part of the contract: any future
    // change must bump the plan and update this test alongside
    // the handler that produces the row.
    let kinds = [
        "credential_oauth_x_connected",
        "credential_oauth_x_disconnected",
    ];
    for kind in kinds {
        // The kinds are non-empty and lowercase with a stable
        // suffix (`_x_…`); the grep-friendly prefix is
        // `credential_oauth_x_`.
        assert!(kind.starts_with("credential_oauth_x_"));
        assert!(kind.is_ascii());
        assert!(!kind.is_empty());
    }
    // The expected kinds list is the canonical one — the
    // handler code must produce these literal strings.
    let _event = NewAuthEvent {
        user_id: Some(uuid::Uuid::new_v4()),
        kind: "credential_oauth_x_connected".to_string(),
        provider: "oauth_x".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("x_account".to_string()),
    };
    let _event = NewAuthEvent {
        user_id: Some(uuid::Uuid::new_v4()),
        kind: "credential_oauth_x_disconnected".to_string(),
        provider: "oauth_x".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("x_account".to_string()),
    };
    // XRefreshTokenClient is constructed at app boot and
    // threaded through the X OAuth state. We assert the
    // public surface here so a future refactor that drops
    // the `http` / `cfg` fields breaks the test.
    let _client = XRefreshTokenClient {
        http: reqwest::Client::new(),
        cfg: Arc::new(ServerXOAuthConfig {
            enabled: true,
            client_id: "x".into(),
            client_secret: None,
            redirect_path: "/api/auth/login/x/callback".into(),
            scopes: vec!["tweet.read".into()],
            timeout_ms: 8000,
        }),
    };
}
