//! End-to-end tests for two security/robustness features:
//!
//! - **WS frame validation** (sample rate, audio frame size,
//!   language hint allow-list).
//! - **Security headers** (CSP, Referrer-Policy, X-Content-Type-Options)
//!   applied to every response, including the static frontend and the
//!   `/healthz` probe.
//! - **CORS allow-list** on `/v1/*` driven by
//!   `LLM_CORS_ALLOW_ORIGINS`.
//!
//! The tests share the in-process server scaffolding from
//! `multiuser_isolation.rs` (mock backend, in-memory session map,
//! ephemeral port) so they run without a GPU and without external
//! network access.

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use futures_util::{SinkExt, StreamExt};
use stt_core::{InferenceJob, InferenceWorker, MockBackend, WhisperBackend};
use stt_proto::{decode_frame, encode, error_code, AudioFrame, Config, Payload, StartSession, Tag};
use stt_server::{
    build_router,
    config::{LimitsConfig, LlmConfig, RateLimitConfig},
    llm::LlmClient,
    rate_limit::{RateLimitPolicy, RateLimiter},
    router::ResultRouter,
    session::SessionMap,
    AppState, Config as ServerConfig,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

const SAMPLE_RATE: u32 = 16_000;

/// Build an in-process server with the LLM proxy optionally enabled.
/// Returns the base HTTP URL (the WS endpoint is `ws://…/ws`).
async fn start_test_server(llm_enabled: bool, cors_allow_origins: Vec<String>) -> String {
    start_test_server_with_rate_limit(llm_enabled, cors_allow_origins, RateLimitConfig::default())
        .await
        .0
}

/// Like [`start_test_server`] but lets the test pick the
/// per-source-IP rate limits (defaulting to generous values to keep
/// existing tests unaffected). Returns `(url, stt_limiter, llm_limiter)`.
async fn start_test_server_with_rate_limit(
    llm_enabled: bool,
    cors_allow_origins: Vec<String>,
    rate_limit: RateLimitConfig,
) -> (String, RateLimiter, RateLimiter) {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));
    let limits = LimitsConfig {
        // Make the audio cap easy to exceed in tests without needing
        // a multi-megabyte buffer.
        max_audio_frame_samples: 64,
        ..LimitsConfig::default()
    };
    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits,
        rate_limit: rate_limit.clone(),
        llm: LlmConfig {
            enabled: llm_enabled,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins,
            system_prompt: None,
        },
        agents: stt_server::config::AgentConfig::default(),
    });

    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let (job_tx, job_rx) = mpsc::channel::<InferenceJob>(16);
    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(16);

    let _worker = InferenceWorker::spawn(Arc::clone(&backend), job_rx);
    let _shutdown = ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

    let llm = if llm_enabled {
        let cfg = Arc::new(server_cfg.llm.clone());
        Some(LlmClient::new(cfg).expect("LlmClient::new"))
    } else {
        None
    };

    // Tests share the *same* `RateLimiter` instance with the server
    // (via the `AppState`) so we can observe the bucket state from
    // outside without going through HTTP. We deliberately bypass
    // `build_rate_limiters` because the latter couples to the whole
    // `Config` and is exercised in its own unit test.
    let stt_limiter = RateLimiter::new(RateLimitPolicy::stt(rate_limit.stt_per_min));
    let llm_limiter = RateLimiter::new(RateLimitPolicy::llm(rate_limit.llm_per_min));

    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm,
        agents: None,
        stt_rate_limiter: stt_limiter.clone(),
        llm_rate_limiter: llm_limiter.clone(),
    });
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    (url, stt_limiter, llm_limiter)
}

/// Open a WS connection and skip the first `BackendInfo` frame.
async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let ws_url = url.replacen("http://", "ws://", 1) + "/ws";
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .expect("ws connect");
    let first = ws
        .next()
        .await
        .expect("BackendInfo")
        .expect("BackendInfo ok");
    match first {
        Message::Binary(buf) => {
            assert_eq!(buf[0], Tag::BackendInfo as u8);
            let _ = decode_frame(&buf).expect("decode BackendInfo");
        }
        other => panic!("expected binary BackendInfo, got {other:?}"),
    }
    ws
}

/// Read WS frames until we see an `Error` payload, a Close, or the
/// stream runs dry. Returns the first `Error` payload encountered.
async fn next_error<S>(ws: &mut S) -> Option<stt_proto::ErrorMessage>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = ws.next().await {
        match msg {
            Ok(Message::Binary(buf)) => {
                if let Ok(Payload::Error(e)) = decode_frame(&buf) {
                    return Some(e);
                }
            }
            Ok(Message::Close(_)) | Err(_) => return None,
            _ => {}
        }
    }
    None
}

// ====================================================================
// Security headers
// ====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn security_headers_present_on_root_and_static() {
    let base = start_test_server(false, vec![]).await;

    for path in ["/", "/static/style.css", "/api/version"] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {path}");

        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            csp.contains("default-src 'self'") && csp.contains("script-src 'self'"),
            "{path}: CSP missing required directives, got `{csp}`"
        );
        // `'wasm-unsafe-eval'` MUST be present in `script-src` — the
        // vendored onnxruntime-web worker compiles its WASM modules
        // at runtime, and a CSP without this token kills VAD load with
        // `CompileError: WebAssembly.instantiateStreaming() blocked by
        // CSP`. If a future refactor strips it, this assertion fires.
        assert!(
            csp.contains("'wasm-unsafe-eval'"),
            "{path}: CSP missing `'wasm-unsafe-eval'` in script-src; \
             onnxruntime-web will fail to compile its WASM modules \
             and VAD will not load. Got: `{csp}`"
        );
        // The CSP must NOT allow remote script sources.
        assert!(
            !csp.contains("https://") && !csp.contains("http://*"),
            "{path}: CSP allows remote sources, got `{csp}`"
        );

        let referrer = resp
            .headers()
            .get(header::REFERRER_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(referrer, "no-referrer", "{path}: bad Referrer-Policy");

        let nosniff = resp
            .headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(nosniff, "nosniff", "{path}: bad X-Content-Type-Options");
    }
}

// ====================================================================
// WS frame validation
// ====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_wrong_sample_rate_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    ws.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some("en".into()),
            sample_rate: 44_100, // not 16_000
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    let err = next_error(&mut ws).await.expect("expected Error frame");
    assert_eq!(
        err.code,
        error_code::INVALID_FRAME,
        "wrong sample rate must be INVALID_FRAME, got {:?}",
        err
    );
    assert!(
        err.message.contains("44100") || err.message.contains("sample_rate"),
        "error message should mention sample_rate, got `{}`",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_unknown_language_hint_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    ws.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some("klingon".into()),
            sample_rate: SAMPLE_RATE,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    let err = next_error(&mut ws).await.expect("expected Error frame");
    assert_eq!(err.code, error_code::INVALID_FRAME);
    assert!(
        err.message.to_lowercase().contains("language"),
        "error message should mention language, got `{}`",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_with_overlong_language_hint_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    let huge = "a".repeat(64);
    ws.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some(huge),
            sample_rate: SAMPLE_RATE,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    let err = next_error(&mut ws).await.expect("expected Error frame");
    assert_eq!(err.code, error_code::INVALID_FRAME);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_audio_frame_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    // The test server caps audio frames at 64 samples; ship 65 and
    // expect an INVALID_FRAME error rather than a panic or an OOM.
    let samples = vec![0.0_f32; 65];
    ws.send(Message::Binary(
        encode(Payload::Audio(AudioFrame { samples })).unwrap(),
    ))
    .await
    .unwrap();

    let err = next_error(&mut ws).await.expect("expected Error frame");
    assert_eq!(err.code, error_code::INVALID_FRAME);
    assert!(
        err.message.contains("65"),
        "error message should include the offending size, got `{}`",
        err.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_with_unknown_language_hint_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    ws.send(Message::Binary(
        encode(Payload::Config(Config {
            language: Some("klingon".into()),
            translate: false,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    let err = next_error(&mut ws).await.expect("expected Error frame");
    assert_eq!(err.code, error_code::INVALID_FRAME);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_audio_at_size_boundary_is_accepted() {
    let base = start_test_server(false, vec![]).await;
    let mut ws = connect(&base).await;

    // Open the session first so the worker's `oneshot` resolves on
    // a known session, then send exactly the cap-sized frame.
    ws.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some("en".into()),
            sample_rate: SAMPLE_RATE,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    let samples = vec![0.0_f32; 64];
    ws.send(Message::Binary(
        encode(Payload::Audio(AudioFrame { samples })).unwrap(),
    ))
    .await
    .unwrap();

    // Should NOT receive an Error frame: read for a short while and
    // assert nothing arrives.
    let short = tokio::time::timeout(Duration::from_millis(200), ws.next()).await;
    match short {
        Ok(Some(Ok(Message::Binary(buf)))) => {
            // A FinalTranscript from the mock is the expected happy
            // path; an Error would be the failure we want to surface.
            let payload = decode_frame(&buf).expect("decode");
            assert!(
                !matches!(payload, Payload::Error(_)),
                "valid frame at boundary triggered Error: {payload:?}"
            );
        }
        Ok(Some(Ok(Message::Close(_)))) => {}
        Ok(None) => {}
        Err(_) => {
            // No frame in 200 ms: also fine, the mock worker may be
            // busy. The important assertion is "no Error frame".
        }
        other => panic!("unexpected ws frame: {other:?}"),
    }
}

// ====================================================================
// CORS on /v1/*
// ====================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cors_preflight_blocked_by_default() {
    // No CORS allow-list configured → preflight from any origin must
    // not return the CORS allow headers. The browser will then refuse
    // to issue the actual request.
    let base = start_test_server(true, vec![]).await;
    let resp = reqwest::Client::new()
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/v1/chat/completions"),
        )
        .header(header::ORIGIN, "https://attacker.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .send()
        .await
        .expect("options");
    let allow_origin = resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .map(|v| v.to_str().unwrap_or("").to_string());
    assert!(
        allow_origin.is_none() || allow_origin.as_deref() == Some(""),
        "default CORS policy leaked an allow-origin header: {allow_origin:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cors_preflight_allowed_when_origin_listed() {
    let allowed = "https://trusted.example".to_string();
    let base = start_test_server(true, vec![allowed.clone()]).await;

    let resp = reqwest::Client::new()
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/v1/chat/completions"),
        )
        .header(header::ORIGIN, &allowed)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .send()
        .await
        .expect("options");
    let allow_origin = resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        allow_origin, allowed,
        "trusted origin did not receive allow-origin"
    );
    let allow_methods = resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_METHODS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        allow_methods.contains("POST"),
        "allow-methods missing POST, got `{allow_methods}`"
    );

    // `Vary: Origin` MUST be present on CORS responses. Without it a
    // caching reverse proxy can serve one origin's
    // `Access-Control-Allow-Origin` header to a different origin on a
    // subsequent hit — the canonical CORS cache-poisoning scenario.
    // tower-http's `CorsLayer` adds this by default; if a future
    // refactor adds an explicit `.vary([])` (or anything that strips
    // it), this assertion fires.
    let vary = resp
        .headers()
        .get(header::VARY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        vary.to_ascii_lowercase().contains("origin"),
        "CORS preflight missing `Vary: Origin` header (got `{vary}`); \
         a caching reverse proxy could serve the wrong Allow-Origin \
         to a different origin"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cors_invalid_origin_in_allow_list_is_skipped() {
    // A syntactically invalid origin must not crash the server and
    // must not be added to the CORS allow set. With everything in the
    // list dropped, the resulting policy behaves like the default
    // (no origin allowed).
    let base = start_test_server(true, vec!["not a url".into(), "".into()]).await;

    let resp = reqwest::Client::new()
        .request(
            reqwest::Method::OPTIONS,
            format!("{base}/v1/chat/completions"),
        )
        .header(header::ORIGIN, "https://attacker.example")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .send()
        .await
        .expect("options");
    let allow_origin = resp
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .map(|v| v.to_str().unwrap_or("").to_string());
    assert!(
        allow_origin.is_none() || allow_origin.as_deref() == Some(""),
        "invalid origin in env leaked into CORS response: {allow_origin:?}"
    );
}
