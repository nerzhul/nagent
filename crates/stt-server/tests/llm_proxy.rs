//! End-to-end tests for the LLM proxy.
//!
//! ## What this test asserts
//!
//! 1. `POST /v1/chat/completions` forwards SSE chunks verbatim, in
//!    order, with the correct content type.
//! 2. `Authorization: Bearer <key>` is forwarded when
//!    `OLLAMA_API_KEY` is set.
//! 3. `Authorization` is **not** forwarded when `OLLAMA_API_KEY` is
//!    unset (the server never injects one).
//! 4. Non-2xx upstream surfaces the original status to the client.
//! 5. `LLM_ENABLED=false` returns 404 on both `/v1/*` routes.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use stt_core::{MockBackend, WhisperBackend};
use stt_server::{
    build_router, config::LlmConfig, llm::LlmClient, session::SessionMap, AppState,
    Config as ServerConfig,
};
use tokio::net::TcpListener;

/// Spawn a mock upstream that records the incoming request and
/// returns a configurable SSE body. Returns the bound address.
async fn spawn_mock_upstream(
    body: String,
    on_request: Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) -> String {
    // The closure must be `Clone` for axum's `Handler` bound, so any
    // state the handler needs lives behind `Arc` and is rebuilt per
    // request. The body string is `Clone`, which makes this simple.
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post({
                let on_request = Arc::clone(&on_request);
                let body = body.clone();
                move |headers: axum::http::HeaderMap, _body: axum::body::Bytes| {
                    let on_request = Arc::clone(&on_request);
                    let body = body.clone();
                    async move {
                        *on_request.lock().await = Some(headers);
                        (
                            StatusCode::OK,
                            [(
                                header::CONTENT_TYPE,
                                HeaderValue::from_static("text/event-stream"),
                            )],
                            body,
                        )
                    }
                }
            }),
        )
        .route(
            "/v1/models",
            get(|| async {
                let body = r#"{"object":"list","data":[{"id":"llama3.1","object":"model"}]}"#;
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    body,
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Build a full stt-server with the LLM proxy enabled and pointing at
/// `upstream_url`.
async fn start_test_server_with_llm(
    upstream_url: String,
    api_key: Option<String>,
) -> (
    String,
    Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));

    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: stt_server::config::LimitsConfig::default(),
        llm: LlmConfig {
            enabled: true,
            base_url: upstream_url,
            default_model: "llama3.1".into(),
            api_key,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
        },
    });

    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let llm_cfg = Arc::new(server_cfg.llm.clone());
    let llm_client =
        LlmClient::new(llm_cfg).expect("LlmClient::new should succeed for test config");

    let (job_tx, _job_rx) = tokio::sync::mpsc::channel::<stt_core::InferenceJob>(16);

    let on_request = Arc::new(tokio::sync::Mutex::new(None));

    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: Some(llm_client),
    });

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Keep tx alive for the duration of the test.
    std::mem::forget(tx);

    (url, on_request)
}

/// Build a stt-server with `LLM_ENABLED=false`. The `/v1/*` routes
/// are never registered, so the chat view sees a clean 404.
async fn start_test_server_disabled() -> String {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));

    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: stt_server::config::LimitsConfig::default(),
        llm: LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
        },
    });

    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let (job_tx, _job_rx) = tokio::sync::mpsc::channel::<stt_core::InferenceJob>(16);
    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: None,
    });

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    url
}

/// Compose a SSE body string from the given events. Each event is
/// emitted as a complete `data: …\n\n` frame, terminated by the
/// OpenAI-compatible `data: [DONE]\n\n` sentinel.
fn sse_body(events: &[&str]) -> String {
    let mut body = String::new();
    for e in events {
        body.push_str(&format!("data: {e}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn compose_events() -> Vec<&'static str> {
    vec![
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_sse_chunks_verbatim() {
    let events = compose_events();
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&events), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "text/event-stream"
    );
    // Defeat buffering hint must be set.
    assert_eq!(
        resp.headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );

    let body = resp.text().await.expect("body");
    let expected = sse_body(&events);
    assert_eq!(body, expected);

    // Content-Type was forwarded.
    let captured_headers = captured.lock().await.take().expect("captured headers");
    let upstream_ct = captured_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        upstream_ct.starts_with("application/json"),
        "upstream should see Content-Type: application/json, got `{upstream_ct}`"
    );

    // Authorization must NOT be forwarded when the server has no key.
    assert!(
        captured_headers.get(header::AUTHORIZATION).is_none(),
        "proxy must not inject an Authorization header when OLLAMA_API_KEY is unset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_bearer_when_key_set() {
    let events = compose_events();
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&events), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, Some("secret-token".into())).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured_headers = captured.lock().await.take().expect("captured headers");
    let auth = captured_headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(auth, "Bearer secret-token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_non_2xx_is_surfaced_verbatim() {
    // Mock upstream that always 404s with a JSON error body.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(
            |_headers: axum::http::HeaderMap, _body: axum::body::Bytes| async {
                (
                    StatusCode::NOT_FOUND,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    r#"{"error":"model not found"}"#,
                )
            },
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let upstream_url = format!("http://{addr}");
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("model not found"),
        "upstream error body must be forwarded, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_non_streaming_request() {
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":false}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_list_proxies_upstream() {
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["unused"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::get(format!("{url}/v1/models"))
        .await
        .expect("get models");
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.expect("json");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["llama3.1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_returns_404_on_v1_routes() {
    let url = start_test_server_disabled().await;

    let chat = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .body("{}")
        .send()
        .await
        .expect("post chat");
    assert_eq!(chat.status(), StatusCode::NOT_FOUND);

    let models = reqwest::get(format!("{url}/v1/models"))
        .await
        .expect("get models");
    assert_eq!(models.status(), StatusCode::NOT_FOUND);
}
