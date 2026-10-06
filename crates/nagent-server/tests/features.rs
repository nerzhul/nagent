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

use nagent_server::config::{AuthBackendKind, AuthConfig, AuthDbConfig, LlmConfig};
use nagent_server::http::build_router;
use nagent_server::testing::app_state;
use nagent_server::AppState;
use tokio::net::TcpListener;
use tower::ServiceExt;
use uuid::Uuid;

async fn connect_db(cfg: &AuthConfig) -> nagent_db::Db {
    let opts: nagent_db::DbOptions = cfg.into();
    nagent_db::Db::connect(&opts).await.expect("store connects")
}

/// Build a minimal in-process `AppState`. The STT backend is
/// stubbed; we only exercise HTTP routing here.
///
/// `llm_enabled` turns the LLM proxy on. When true we point the
/// LLM client at `http://127.0.0.1:1` (nothing listens there) so
/// `fetch_upstream_model_list` hits the offline-fallback path
/// instead of accidentally reaching a real Ollama on the test
/// host's `localhost:11434` and returning its model inventory.
/// Tests that want a deterministic upstream list use their own
/// dedicated helper (see
/// `features_llm_models_reflects_upstream_list`).
async fn build_state_with_features(
    documents_enabled: bool,
    llm_enabled: bool,
) -> (Arc<AppState>, nagent_db::Db) {
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
    let auth_store = connect_db(&auth_cfg).await;
    auth_store.migrate().await.expect("migrations must apply");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).auth = auth_cfg.clone();
    Arc::make_mut(&mut builder.config).documents.enabled = documents_enabled;
    builder = builder.with_auth(auth_store.clone());

    let documents = if documents_enabled {
        Some(nagent_server::documents::DocumentStore::new(
            auth_store.clone(),
            100_000,
            std::path::PathBuf::from("/tmp/features-test-cache"),
            0,
        ))
    } else {
        None
    };
    if let Some(store) = documents {
        builder = builder.with_documents(store);
    }
    builder = builder.with_chat_sessions(nagent_server::chat::sessions::ChatSessions::new(
        auth_store.admin().chat_sessions.clone(),
    ));
    if llm_enabled {
        // Deliberately unreachable so the upstream fetch falls back
        // to `[default_model]` regardless of what runs on the test
        // host. Tests that want a deterministic upstream install
        // their own mock — see `features_llm_models_reflects_upstream_list`.
        let llm_cfg = LlmConfig {
            base_url: "http://127.0.0.1:1".into(),
            ..LlmConfig::default()
        };
        Arc::make_mut(&mut builder.config).llm = llm_cfg.clone();
        builder = builder
            .with_llm(nagent_server::llm::LlmClient::new(Arc::new(llm_cfg)).expect("LLM stub"));
    }
    let state = builder.build();
    (state, auth_store)
}

/// Mint a session for `email` and return the cookie string the
/// browser would send.
async fn login_cookie(store: &nagent_db::Db, email: &str) -> String {
    let user_id = store
        .admin()
        .users
        .create(email, "Alice", "local", Some(b"hash"))
        .await
        .expect("create_user");
    let session = store
        .admin()
        .sessions
        .create(user_id, Duration::from_secs(60), None, None)
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
    assert_eq!(
        body["llm_models"],
        serde_json::json!([]),
        "llm_models must be an empty array when LLM is disabled (the dropdown should never list phantom models)"
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
    // `LlmConfig::default()` has `base_url = "http://localhost:11434"`
    // which the test env doesn't serve. The fetch fails inside the
    // bounded `UPSTREAM_MODELS_TIMEOUT` and collapses to
    // `[default_model]` (the default LLM config's
    // `default_model = "llama3.1"`). Pin both halves of that
    // contract — the field is non-empty AND it is the fallback —
    // so a future regression that returns `[]` (or hangs past the
    // timeout) would surface here.
    assert_eq!(
        body["llm_models"],
        serde_json::json!(["llama3.1"]),
        "llm_models must collapse to [default_model] when the upstream fetch fails (offline fallback)"
    );
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

/// `/api/features.llm_models` reflects whatever the upstream
/// `/v1/models` returns. The chat UI uses this field to paint the
/// Discussion-mode `<select id="chat-model">` without a separate
/// `/v1/models` round-trip; the response has to round-trip the
/// upstream list byte-for-byte so an operator who pulled a model
/// on the Ollama host sees it in the dropdown immediately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn features_llm_models_reflects_upstream_list() {
    use axum::routing::get;
    use axum::Router;

    // Mock upstream that serves a deterministic three-model list.
    // Returning the OpenAI-compatible `{ object: "list", data: [...] }`
    // shape is what the real Ollama endpoint emits, so the helper
    // `fetch_upstream_model_list` reaches its happy path.
    let app_upstream = Router::new().route(
        "/v1/models",
        get(|| async {
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                )],
                r#"{"object":"list","data":[{"id":"qwen2.5:14b","object":"model"},{"id":"llama3.1","object":"model"},{"id":"deepseek-r1:14b","object":"model"}]}"#,
            )
        }),
    );
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, app_upstream).await;
    });

    // Wire a stt-server with LLM enabled AND pointing at the mock.
    let auth_cfg = AuthConfig {
        enabled: true,
        backends: vec![AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:features_upstream_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..AuthConfig::default()
    };
    let auth_store = connect_db(&auth_cfg).await;
    auth_store.migrate().await.expect("migrations must apply");

    let llm_cfg = LlmConfig {
        enabled: true,
        base_url: upstream_url,
        default_model: "llama3.1".into(),
        api_key: None,
        inbound_auth_key: None,
        auth_mode: nagent_server::config::LlmAuthMode::Forward,
        request_timeout: Duration::from_secs(120),
        cors_allow_origins: vec![],
        system_prompt: None,
        allow_user_location: true,
        allow_user_timezone: true,
        allow_user_reply_language: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = nagent_server::llm::LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new must succeed");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).auth = auth_cfg;
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    builder = builder.with_auth(auth_store.clone());
    builder = builder.with_llm(llm_client);
    builder = builder.with_chat_sessions(nagent_server::chat::sessions::ChatSessions::new(
        auth_store.admin().chat_sessions.clone(),
    ));
    let state = builder.build();

    let app = build_router(state);
    let cookie = login_cookie(&auth_store, "carol@test.invalid").await;

    let resp = app
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
    let models: Vec<&str> = body["llm_models"]
        .as_array()
        .expect("llm_models must be an array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(
        models,
        vec!["qwen2.5:14b", "llama3.1", "deepseek-r1:14b"],
        "llm_models must mirror the upstream /v1/models list byte-for-byte so an operator who pulled a model sees it in the dropdown"
    );
}

/// When the upstream `/v1/models` fetch fails (timeout, network
/// error, non-2xx, …) the features response must collapse to
/// `[default_model]` rather than `[]` — the chat UI's contract is
/// "the dropdown is never empty when `llm: true`", and an empty
/// array would render a single `(no models)` placeholder. We point
/// the LLM client at a deliberately unbound address; the
/// `connect_timeout` on `LlmClient` (10s) is the upper bound but
/// `UPSTREAM_MODELS_TIMEOUT` (3s) caps the call before that, so
/// the test completes in well under 10s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn features_llm_models_falls_back_when_upstream_unreachable() {
    let auth_cfg = AuthConfig {
        enabled: true,
        backends: vec![AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:features_fallback_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..AuthConfig::default()
    };
    let auth_store = connect_db(&auth_cfg).await;
    auth_store.migrate().await.unwrap();

    // Use a port that nothing is listening on so the connect
    // attempt fails fast inside the bounded timeout. We pick an
    // ephemeral high port to avoid clashing with anything else on
    // the test host; the OS will reject the connect immediately
    // with `ECONNREFUSED`.
    let llm_cfg = LlmConfig {
        enabled: true,
        base_url: "http://127.0.0.1:1".into(),
        default_model: "fallback-model".into(),
        api_key: None,
        inbound_auth_key: None,
        auth_mode: nagent_server::config::LlmAuthMode::Forward,
        request_timeout: Duration::from_secs(120),
        cors_allow_origins: vec![],
        system_prompt: None,
        allow_user_location: true,
        allow_user_timezone: true,
        allow_user_reply_language: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = nagent_server::llm::LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new must succeed");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).auth = auth_cfg;
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    builder = builder.with_auth(auth_store.clone());
    builder = builder.with_llm(llm_client);
    builder = builder.with_chat_sessions(nagent_server::chat::sessions::ChatSessions::new(
        auth_store.admin().chat_sessions.clone(),
    ));
    let state = builder.build();

    let app = build_router(state);
    let cookie = login_cookie(&auth_store, "dave@test.invalid").await;

    let started = std::time::Instant::now();
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .uri("/api/features")
                .header("cookie", cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    // The bound is `UPSTREAM_MODELS_TIMEOUT = 3s` plus the rest of
    // the response build; assert we land well below it.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "/api/features must respond within the UPSTREAM_MODELS_TIMEOUT bound even when the upstream is unreachable, took {:?}",
        started.elapsed()
    );
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body["llm_models"],
        serde_json::json!(["fallback-model"]),
        "upstream unreachable must collapse to [default_model] — empty array would break the dropdown contract"
    );
}
