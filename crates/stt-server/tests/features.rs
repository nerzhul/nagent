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
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use stt_server::auth::store::AuthStore;
use stt_server::config::{AuthBackendKind, AuthConfig, AuthDbConfig, LlmConfig};
use stt_server::http::build_router;
use stt_server::testing::app_state;
use stt_server::AppState;
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

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).auth = auth_cfg.clone();
    Arc::make_mut(&mut builder.config).documents.enabled = documents_enabled;
    builder = builder.with_auth(auth_store.clone());

    let documents = if documents_enabled {
        Some(stt_server::documents::DocumentStore::new(
            auth_store.clone(),
            100_000,
            std::path::PathBuf::from("/tmp/features-test-cache"),
        ))
    } else {
        None
    };
    if let Some(store) = documents {
        builder = builder.with_documents(store);
    }
    builder = builder.with_chat_sessions(stt_server::chat::sessions::ChatSessions::new(
        auth_store.clone(),
    ));
    if llm_enabled {
        builder = builder.with_llm(
            stt_server::llm::LlmClient::new(Arc::new(LlmConfig::default())).expect("LLM stub"),
        );
    }
    let state = builder.build();
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
    format!(
        "nagent_session={}",
        session
            .plaintext_token
            .as_deref()
            .unwrap_or("missing-token")
    )
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
