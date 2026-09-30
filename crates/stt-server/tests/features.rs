//! `GET /api/features` — feature-discovery endpoint for the
//! frontend.
//!
//! Verifies the JSON shape, the gating logic (each flag tracks
//! the corresponding `AppState` field), and the auth posture
//! (the route lives under the protected subtree so
//! `RequireAuth` returns 401 for anonymous callers when
//! `auth.enabled = true`).
//!
//! Mirrors the harness used by `tests/auth_gating.rs` — in-memory
//! sqlite for the auth DB, real `AuthStore::migrate()`, real
//! `build_router` so we exercise the full axum stack via
//! `tower::ServiceExt::oneshot`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use std::time::Duration;
use stt_server::auth::store::AuthStore;
use stt_server::config::AuthConfig;
use stt_server::rate_limit::{RateLimitPolicy, RateLimiter};
use stt_server::session::SessionMap;
use stt_server::{
    agents::ServiceRegistry,
    build_router,
    config::{AuthBackendKind, AuthDbConfig, LlmConfig, RateLimitConfig},
    AppState,
};
use tower::ServiceExt;
use uuid::Uuid;

/// Build a minimal in-process `AppState`. The STT backend is
/// stubbed; we only exercise HTTP routing here.
async fn build_state_with_features(
    documents_enabled: bool,
    llm_enabled: bool,
) -> (Arc<AppState>, AuthStore) {
    let auth_cfg = AuthConfig {
        enabled: true,
        backends: vec![AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:features_test_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..AuthConfig::default()
    };
    let auth_store = AuthStore::connect(&auth_cfg)
        .await
        .expect("auth store must connect");
    auth_store.migrate().await.expect("migrations must apply");

    // Build a Config that enables/disables the optional subsystems.
    // `Config` has no Default impl — we go through from_env_with_toml
    // with a TOML overlay that sets the minimum required fields.
    let toml_text = format!(
        r#"
            [server]
            whisper_model_path = "/tmp/fake-model.bin"
            [auth]
            enabled = true
            backends = ["local"]
            public_url = "https://example.com"
            [auth.db]
            backend = "sqlite"
            url = "sqlite::memory:"
            [documents]
            enabled = {}
        "#,
        documents_enabled
    );
    let toml: stt_server::config_file::TomlConfig =
        toml::from_str(&toml_text).expect("TOML must parse");
    let mut cfg =
        stt_server::config::Config::from_env_with_toml(Some(&toml)).expect("config from env");
    cfg.auth.db = auth_cfg.db.clone();
    cfg.documents.enabled = documents_enabled;

    let backend: Arc<dyn stt_core::WhisperBackend> =
        Arc::new(stt_core::MockBackend::new("test-model"));
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let (job_tx_inner, _job_rx) = tokio::sync::mpsc::channel::<stt_core::InferenceJob>(16);
    let job_tx = stt_core::PoolDispatch::from_single_sender(job_tx_inner);
    let (_resp_tx, _resp_rx) = tokio::sync::mpsc::channel::<stt_core::InferResponse>(16);

    // Build optional subsystems based on flags.
    let documents = if documents_enabled {
        Some(stt_server::documents::DocumentStore::new(
            auth_store.clone(),
            100_000,
            std::path::PathBuf::from("/tmp/features-test-cache"),
        ))
    } else {
        None
    };
    let chat_sessions = Some(stt_server::chat_sessions::ChatSessions::new(
        auth_store.clone(),
    ));

    let state = Arc::new(AppState {
        backend,
        sessions,
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: Arc::new(cfg),
        llm: if llm_enabled {
            Some(stt_server::llm::LlmClient::new(Arc::new(LlmConfig::default())).expect("LLM stub"))
        } else {
            None
        },
        agents: None,
        tts: None,
        stt_rate_limiter: RateLimiter::new(RateLimitPolicy::stt(
            RateLimitConfig::default().stt_per_min,
        )),
        llm_rate_limiter: RateLimiter::new(RateLimitPolicy::llm(
            RateLimitConfig::default().llm_per_min,
        )),
        auth_store: Some(auth_store.clone()),
        auth_oidc: None,
        auth_passkey: None,
        auth_rate_limiter: stt_server::auth::rate_limit::LoginRateLimiter::new(),
        services: ServiceRegistry::empty().into_arc(),
        credential_resolver: None,
        credentials_key: None,
        documents,
        chat_sessions,
    });
    (state, auth_store)
}

/// Mint a session for `email` and return the cookie string the
/// browser would send.
async fn login_cookie(store: &AuthStore, email: &str) -> String {
    let user_id = store
        .create_user(email, "Alice", "local", Some(b"hash"))
        .await
        .expect("create_user");
    let session = store
        .create_session(user_id, Duration::from_secs(60), None, None)
        .await
        .expect("create_session");
    // The cookie name is the same as `auth.config.cookie_name()`;
    // we hard-code it for the test to avoid plumbing the config
    // back through here. The auth router reads
    // `COOKIE_NAME = "nagent_session"`.
    format!("nagent_session={}", session.id)
}

/// `GET /api/features` returns the right shape with most
/// subsystems disabled. CSRF is not required (GET).
#[tokio::test]
async fn features_returns_all_disabled_when_subsystems_off() {
    let (state, store) = build_state_with_features(false, false).await;
    let app = build_router(state);
    let cookie = login_cookie(&store, "alice@test.invalid").await;

    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/features")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /api/features must return 200 for an authenticated caller"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/json"),
        "/api/features must be JSON, got content-type {ct}"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).expect("body must parse as JSON");
    assert_eq!(
        body["documents"], false,
        "documents flag must be false (no documents subsystem wired)"
    );
    assert_eq!(
        body["llm"], false,
        "llm flag must be false (LLM proxy not wired)"
    );
    assert_eq!(body["tts"], false, "tts flag must be false (TTS not wired)");
    assert_eq!(
        body["agents"], false,
        "agents flag must be false (no agent registry)"
    );
    assert_eq!(
        body["chat_sessions"], true,
        "chat_sessions flag must be true (auth DB is reachable so the ChatSessions handle was built)"
    );
    assert_eq!(
        body["agent_names"],
        serde_json::json!([]),
        "agent_names must be an empty array"
    );
    assert_eq!(
        body["tools"],
        serde_json::json!([]),
        "tools must be an empty array"
    );
}

/// `GET /api/features` reflects runtime configuration when
/// subsystems are enabled.
#[tokio::test]
async fn features_reflects_runtime_configuration() {
    let (state, store) = build_state_with_features(true, true).await;
    let app = build_router(state);
    let cookie = login_cookie(&store, "bob@test.invalid").await;

    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/features")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["documents"], true);
    assert_eq!(body["llm"], true);
    assert_eq!(body["tts"], false);
    assert_eq!(body["agents"], false);
    assert_eq!(body["chat_sessions"], true);
}

/// Anonymous (no session) calls are 401 under auth gating.
#[tokio::test]
async fn features_anonymous_returns_401_when_auth_enabled() {
    let (state, _store) = build_state_with_features(false, false).await;
    let app = build_router(state);
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/features")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "anonymous /api/features must 401 when auth is enabled"
    );
}
