//! End-to-end test for the auth-gating router contract.
//!
//! When `auth.enabled = true`, the `RequireAuth` middleware must
//! protect every endpoint **except** the public carve-out
//! (`/`, `/static/*`, `/healthz`, `/api/version`, the auth login
//! routes). This is the "global portal" promise: a logged-out
//! browser can fetch the login page and submit credentials, and
//! nothing else — every functional call (`/ws`, `/v1/*`,
//! `/api/me`, `/api/auth/logout`) returns `401` until a valid
//! session cookie is presented.
//!
//! The test uses the real `AuthStore` against an in-memory sqlite
//! database (so no port allocation is needed) and exercises the
//! full axum stack via `tower::ServiceExt::oneshot`, matching the
//! pattern used by `auth::middleware::tests`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use stt_server::auth::store::AuthStore;
use stt_server::{
    build_router,
    config::{
        AgentConfig, AuthBackendKind, AuthConfig, AuthDbConfig, LlmAuthMode, LlmConfig,
        RateLimitConfig, TtsConfig,
    },
    rate_limit::{RateLimitPolicy, RateLimiter},
    session::SessionMap,
    AppState, Config as ServerConfig,
};
use tower::ServiceExt;
use uuid::Uuid;

/// Build a complete in-process `AppState` with `auth.enabled = true`
/// and an in-memory sqlite auth DB. The STT worker pool is
/// stubbed — these tests never connect a real WebSocket client
/// because the point of the test is the HTTP gating, not the
/// audio pipeline.
async fn build_state_with_auth() -> Arc<AppState> {
    // Use the real Config plumbing so we exercise the same
    // validation as `main`. `auth.enabled = true` requires
    // `backends` and `[auth.db]` to be set.
    let toml_text = r#"
        [server]
        whisper_model_path = "/tmp/fake-model.bin"
        [auth]
        enabled = true
        backends = ["local"]
        public_url = "https://example.com"
        [auth.db]
        backend = "sqlite"
        url = "sqlite://file:gating_test_{}?mode=memory&cache=shared"
    "#;
    // The `{0}` placeholder is filled per-test so the unique
    // memory file guarantees a fresh schema.
    let toml_text = toml_text.replace(
        "sqlite://file:gating_test_{}?mode=memory&cache=shared",
        &format!(
            "sqlite://file:gating_test_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
    );
    let toml: stt_server::config_file::TomlConfig =
        toml::from_str(&toml_text).expect("TOML must parse");
    let mut cfg = ServerConfig::from_env_with_toml(Some(&toml)).expect("config must load");
    // Sanity-check the assumptions this test depends on.
    assert!(cfg.auth.enabled);
    assert_eq!(cfg.auth.backends, vec![AuthBackendKind::Local]);
    let auth_cfg = AuthConfig {
        enabled: true,
        backends: vec![AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:gating_db_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
        },
        ..AuthConfig::default()
    };
    let auth_store = AuthStore::connect(&auth_cfg)
        .await
        .expect("auth store must connect");
    auth_store.migrate().await.expect("migrations must apply");
    // Force a different DB URL into the config to make sure the
    // router sees the same handle that we use for user creation.
    cfg.auth = auth_cfg;

    let server_cfg = Arc::new(cfg);
    let backend: Arc<dyn stt_core::WhisperBackend> =
        Arc::new(stt_core::MockBackend::new("test-model"));

    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let (job_tx_inner, _job_rx) = tokio::sync::mpsc::channel::<stt_core::InferenceJob>(16);
    let job_tx = stt_core::PoolDispatch::from_single_sender(job_tx_inner);
    let (_resp_tx, _resp_rx) = tokio::sync::mpsc::channel::<stt_core::InferResponse>(16);

    Arc::new(AppState {
        backend,
        sessions,
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: None,
        agents: None,
        tts: None,
        stt_rate_limiter: RateLimiter::new(RateLimitPolicy::stt(
            RateLimitConfig::default().stt_per_min,
        )),
        llm_rate_limiter: RateLimiter::new(RateLimitPolicy::llm(
            RateLimitConfig::default().llm_per_min,
        )),
        auth_store: Some(auth_store),
        auth_oidc: None,
        auth_passkey: None,
        auth_rate_limiter: stt_server::auth::rate_limit::LoginRateLimiter::new(),
    })
}

#[tokio::test]
async fn public_carve_outs_are_reachable_without_auth() {
    let state = build_state_with_auth().await;
    let app = build_router(state);

    // `/` — serves the login page. Anonymous GET must succeed.
    let resp = app
        .clone()
        .oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET / must be public");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/html"),
        "/ must serve HTML, got content-type {ct}"
    );

    // `/healthz` — health probe must stay public so ops tooling
    // can reach it without holding a session cookie.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET /healthz must be public");

    // `/api/version` — version probe must stay public so the
    // frontend update banner can detect upgrades without a session.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /api/version must be public"
    );

    // `/static/app.js` (or any embedded asset) must stay public
    // so the login page can load the JS bundle.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/static/app.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "GET /static/app.js must be public"
    );

    // `/api/auth/login/password` must be reachable without a
    // session — otherwise the user cannot log in to *get* a
    // session. We send a malformed body to avoid exercising the
    // real login path; what matters here is that the request is
    // not short-circuited by `RequireAuth`.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/auth/login/password")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    // The handler returns 400 (missing fields) — anything but
    // 401 proves the route is not behind `RequireAuth`.
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /api/auth/login/password must not be gated by RequireAuth"
    );
}

#[tokio::test]
async fn ws_upgrade_is_rejected_for_anonymous_when_auth_enabled() {
    let state = build_state_with_auth().await;
    let app = build_router(state);

    // `/ws` is the STT WebSocket upgrade. Anonymous must NOT
    // succeed. The middleware returns 401 before the upgrade,
    // so the response is a plain 401 (not a 101 Switching
    // Protocols).
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri("/ws")
                // Required by axum's WebSocketUpgrade extractor
                // to recognise the request as an upgrade attempt.
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "anonymous /ws must 401 when auth is enabled"
    );
}

#[tokio::test]
async fn api_me_is_401_when_anonymous() {
    let state = build_state_with_auth().await;
    let app = build_router(state);

    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri("/api/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_me_succeeds_with_valid_session_cookie() {
    let state = build_state_with_auth().await;

    // Seed a local user + a session so the test can carry a
    // real cookie. The auth router is the only consumer of the
    // user/session rows, so we go through the store directly.
    let auth_store = state.auth_store.clone().expect("auth_store");
    let user_id = auth_store
        .create_user("alice@example.com", "Alice", "local", Some(b"hash"))
        .await
        .expect("create_user");
    let session = auth_store
        .create_session(user_id, std::time::Duration::from_secs(60), None, None)
        .await
        .expect("create_session");

    let app = build_router(state.clone());
    let cookie = format!("{}={}", state.config.auth.cookie_name(), session.id);
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri("/api/me")
                .header(axum::http::header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "cookie must authenticate");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body["email"], "alice@example.com");
}

#[tokio::test]
async fn logout_requires_auth() {
    let state = build_state_with_auth().await;
    let app = build_router(state);

    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /api/auth/logout must 401 when anonymous"
    );
}

/// Suppress unused warnings on the imports that the test
/// scaffolding needs even when individual cases don't reach
/// every helper.
#[allow(dead_code)]
fn _suppress_unused_warnings() {
    let _ = (AgentConfig::default(), TtsConfig::default());
    let _ = LlmConfig {
        enabled: false,
        base_url: String::new(),
        default_model: String::new(),
        api_key: None,
        inbound_auth_key: None,
        auth_mode: LlmAuthMode::default(),
        request_timeout: std::time::Duration::from_secs(0),
        cors_allow_origins: Vec::new(),
        system_prompt: None,
        allow_user_location: false,
        allow_user_timezone: true,
    };
}
