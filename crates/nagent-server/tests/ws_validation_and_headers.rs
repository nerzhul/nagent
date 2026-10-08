//! End-to-end tests for two security/robustness features:
//!
//! - **WS frame validation** (sample rate, audio frame size,
//! language hint allow-list).
//! - **Security headers** (CSP, Referrer-Policy, X-Content-Type-Options)
//! applied to every response, including the static frontend and the
//! `/healthz` probe.
//! - **CORS allow-list** on `/v1/*` driven by
//! `LLM_CORS_ALLOW_ORIGINS`.
//!
//! The tests share the in-process server scaffolding from
//! `multiuser_isolation.rs` (mock backend, in-memory session map,
//! ephemeral port) so they run without a GPU and without external
//! network access.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request as HttpRequest, StatusCode};
use futures_util::{SinkExt, StreamExt};
use nagent_server::config::{LimitsConfig, LlmConfig, RateLimitConfig};
use nagent_server::http::build_router;
use nagent_server::llm::LlmClient;
use nagent_server::rate_limit::{RateLimitPolicy, RateLimiter};
use nagent_server::testing::app_state;
use stt_proto::{decode_frame, encode, error_code, AudioFrame, Config, Payload, StartSession, Tag};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tower::util::ServiceExt;
use uuid::Uuid;

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
///
/// Uses the phase-5.B test builder. The test then reaches into the
/// resulting `AppState` to grab the `SessionMap` (used by the
/// per-session-isolation test below) and replaces the rate-limiters
/// with hand-built ones so the test can observe the bucket state
/// from outside without going through HTTP. We deliberately bypass
/// `build_rate_limiters` because the latter couples to the whole
/// `Config` and is exercised in its own unit test.
async fn start_test_server_with_rate_limit(
    llm_enabled: bool,
    cors_allow_origins: Vec<String>,
    rate_limit: RateLimitConfig,
) -> (String, RateLimiter, RateLimiter) {
    start_test_server_with_overrides(llm_enabled, cors_allow_origins, rate_limit, None, None).await
}

/// Lowest-level helper. `ws_max_concurrent` / `ws_max_per_ip`
/// default to `LimitsConfig::default()` when `None`; pass explicit
/// values to test the S-1 concurrency caps.
async fn start_test_server_with_overrides(
    llm_enabled: bool,
    cors_allow_origins: Vec<String>,
    rate_limit: RateLimitConfig,
    ws_max_concurrent: Option<usize>,
    ws_max_per_ip: Option<usize>,
) -> (String, RateLimiter, RateLimiter) {
    let mut limits = LimitsConfig {
        // Make the audio cap easy to exceed in tests without needing
        // a multi-megabyte buffer.
        max_audio_frame_samples: 64,
        ..LimitsConfig::default()
    };
    if let Some(n) = ws_max_concurrent {
        limits.ws_max_concurrent = n;
    }
    if let Some(n) = ws_max_per_ip {
        limits.ws_max_per_ip = n;
    }
    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).limits = limits;
    Arc::make_mut(&mut builder.config).llm = LlmConfig {
        enabled: llm_enabled,
        base_url: "http://localhost:11434".into(),
        default_model: "llama3.1".into(),
        api_key: None,
        inbound_auth_key: None,
        auth_mode: nagent_server::config::LlmAuthMode::default(),
        request_timeout: Duration::from_secs(120),
        cors_allow_origins,
        system_prompt: None,
        allow_user_location: true,
        allow_user_timezone: true,
        allow_user_reply_language: true,
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let stt_limiter = RateLimiter::new(RateLimitPolicy::stt(rate_limit.stt_per_min));
    let llm_limiter = RateLimiter::new(RateLimitPolicy::llm(rate_limit.llm_per_min));
    if llm_enabled {
        let cfg = Arc::new(builder.config.llm.clone());
        builder = builder.with_llm(LlmClient::new(cfg).expect("LlmClient::new"));
    }
    // Re-install the test-controlled limiters (the builder produced its
    // own from the config defaults; we need to share *these* with the
    // caller so the bucket can be observed from outside).
    builder = builder
        .with_stt_rate_per_min(rate_limit.stt_per_min)
        .with_llm_rate_per_min(rate_limit.llm_per_min);
    let state = builder.build();
    // Tests share the *same* `RateLimiter` instance with the server
    // (via `AppState`) so we can observe the bucket state. The builder
    // gives us `Arc`s; for the test's local copies we just need to
    // know the per-minute caps, not the live instance — read them
    // back from the config so the helper stays single-purpose.
    let _ = (stt_limiter.clone(), llm_limiter.clone()); // see note above
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

// ====================================================================
// Plan S-1: WebSocket concurrency caps + body limits
// ====================================================================

/// Cap the WS concurrency at 1 global / 1 per IP and confirm a
/// second upgrade from the same loopback IP is rejected. The
/// global cap and per-IP cap happen to be equal, so this also covers
/// the "global full" branch — both reject with the same `503 +
/// Retry-After: 1` shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_upgrade_rejected_when_concurrency_cap_reached() {
    let (base, _stt, _llm) = start_test_server_with_overrides(
        false,
        vec![],
        RateLimitConfig::default(),
        Some(1),
        Some(1),
    )
    .await;

    // First connection succeeds: the WS handshake completes and we
    // receive the BackendInfo frame.
    let _first = connect(&base).await;

    // Second connection from the same loopback peer (the test only
    // binds 127.0.0.1) hits the per-IP cap. The HTTP upgrade must
    // be rejected with 503 + Retry-After BEFORE the WS handshake
    // starts — otherwise the rejection would only be visible as a
    // WS close frame that operators would have to decode.
    let ws_url = base.replacen("http://", "ws://", 1) + "/ws";
    let outcome = tokio_tungstenite::connect_async(&ws_url)
        .await
        .expect_err("second WS upgrade must be rejected");
    let tokio_tungstenite::tungstenite::Error::Http(resp) = &outcome else {
        panic!("expected tungstenite Http error, got {outcome:?}");
    };
    let status = resp.status().as_u16();
    assert_eq!(
        status, 503,
        "concurrency-saturated upgrade must return 503 (got {status})"
    );
    // axum rejects the upgrade at the HTTP layer; tokio-tungstenite
    // surfaces the status. The `Retry-After: 1` header is also set
    // by the handler — verified separately via the raw HTTP path
    // below because the WS client does not surface response headers.
    let raw = reqwest::Client::new()
        .get(format!("{base}/ws"))
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("raw ws upgrade");
    assert_eq!(
        raw.status().as_u16(),
        503,
        "second raw upgrade must also return 503"
    );
    assert_eq!(
        raw.headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap_or("")),
        Some("1"),
        "saturated WS upgrade must carry Retry-After: 1"
    );
}

/// Bodies larger than `[server.limits].body_limit_bytes` are
/// rejected by `DefaultBodyLimit` before they reach a handler. The
/// LLM proxy is mounted on the protected subtree, so a POST with a
/// 4 MiB JSON body (against an 8 KiB cap) must come back as
/// `413 Payload Too Large`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_body_limit_rejects_oversized_upload() {
    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).limits = LimitsConfig {
        body_limit_bytes: 8 * 1024, // 8 KiB — easy to exceed
        ..LimitsConfig::default()
    };
    Arc::make_mut(&mut builder.config).llm.enabled = true;
    Arc::make_mut(&mut builder.config).llm.inbound_auth_key = None;
    Arc::make_mut(&mut builder.config).llm.base_url = "http://localhost:11434".into();
    let cfg = Arc::new(builder.config.llm.clone());
    let state = builder
        .with_llm(LlmClient::new(cfg).expect("LlmClient::new"))
        .build();
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

    // 4 MiB of JSON — should hit the 8 KiB cap.
    let big_body = "x".repeat(4 * 1024 * 1024);
    let payload =
        format!(r#"{{"model":"llama3.1","messages":[{{"role":"user","content":"{big_body}"}}]}}"#);
    // The server may close the connection mid-upload (axum's
    // `DefaultBodyLimit` over hyper sometimes does that for very
    // large bodies); tolerate either a clean `413` response or a
    // `BrokenPipe` transport error from reqwest. Both mean "the
    // transport guardrail fired" which is the contract under test.
    let send_result = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await;
    match send_result {
        Ok(resp) => assert_eq!(
            resp.status().as_u16(),
            413,
            "oversized body must be rejected at the transport (got {})",
            resp.status()
        ),
        Err(e) => {
            let is_broken_pipe = e.is_connect() || e.is_timeout() || e.is_body() || e.is_request();
            assert!(
                is_broken_pipe,
                "unexpected transport error from oversized body: {e:?}"
            );
        }
    }
}

// ====================================================================
// Plan S-2: Origin / Host validation on /ws and state-changing routes
// ====================================================================

/// `POST /v1/chat/completions` from a cross-origin attacker must
/// be rejected with `403 Forbidden`. With auth enabled and a
/// cross-origin Origin, the request never reaches the auth gate —
/// the S-2 origin middleware short-circuits first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_without_matching_origin_is_forbidden() {
    let base = start_test_server(true, vec![]).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header(header::ORIGIN, "https://attacker.example")
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"model":"llama3.1","messages":[]}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "cross-origin POST must be rejected by Origin guard (got {})",
        resp.status()
    );
}

/// WebSocket upgrade from a cross-origin attacker must be
/// rejected with `403 Forbidden` at the HTTP layer — before the
/// handshake starts — so the rejection is visible in plain HTTP
/// logs without decoding a WS close frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_upgrade_with_cross_origin_origin_is_rejected() {
    let base = start_test_server(false, vec![]).await;
    // The WS handler rejects the upgrade with `403 Forbidden` when the
    // `Origin` header points at an attacker-controlled site. The
    // `tokio-tungstenite` client does not expose a builder for raw
    // headers, so we use the raw HTTP path via `reqwest` and
    // assert the 403 status + Vary header.
    let resp = reqwest::Client::new()
        .get(format!("{base}/ws"))
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header(header::HOST, "127.0.0.1:0")
        .header(header::ORIGIN, "https://attacker.example")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("raw ws upgrade");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "cross-origin WS upgrade must return 403 (got {})",
        resp.status().as_u16()
    );
    assert_eq!(
        resp.headers()
            .get(header::VARY)
            .map(|v| v.to_str().unwrap_or("")),
        Some("Origin"),
        "cross-origin rejection must carry `Vary: Origin`"
    );
}

// ====================================================================
// Plan S-1b: documents subtree overrides the transport body cap
// ====================================================================
//
// The protected subtree installs
// `DefaultBodyLimit::max([server.limits].body_limit_bytes)` (the
// generic 2 MiB cap that protects the LLM proxy, agents, and the
// rest of `/v1/*`). `/v1/documents` mounts its OWN inner
// `DefaultBodyLimit::max([documents].max_file_size_bytes)` so PDF
// uploads up to the documents cap reach the handler instead of
// being chopped at the transport layer with an opaque axum
// `MultipartError` ("Error parsing `multipart/form-data` request").
//
// These two tests pin that override:
//  1. `documents_route_overrides_transport_body_limit` (this file)
//     — a body larger than `body_limit_bytes` but smaller than
//     `documents.max_file_size_bytes` must reach the handler and
//     succeed (or fail with a structured handler-side error), NOT
//     the opaque axum 400.
//  2. `http_body_limit_rejects_oversized_upload` (further up in
//     this file) — `/v1/chat/completions` (on the protected
//     subtree but NOT inside the documents override) must still
//     trip the transport cap with the existing 413 contract.
//
// Without the override, test 1 would see
// `400 Bad Request — "Error parsing multipart/form-data request"`
// (axum's MultipartError body) instead of a structured handler
// response.

/// Build a minimal `AppState` for the documents body-limit override
/// tests. Auth + documents + chat_sessions are wired; LLM is left
/// off because the test does not need it and `v1_envelope` falls
/// back to a permissive limiter when `state.llm` is `None`.
async fn build_state_for_documents_body_limit_test(
    body_limit_bytes: usize,
    documents_max_file_size_bytes: usize,
) -> (
    Arc<nagent_server::AppState>,
    nagent_db::Db,
    nagent_server::config::AuthConfig,
) {
    let auth_cfg = nagent_server::config::AuthConfig {
        enabled: true,
        backends: vec![nagent_server::config::AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: nagent_server::config::AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:doc_body_limit_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..nagent_server::config::AuthConfig::default()
    };
    let auth_store = nagent_db::Db::connect(&(&auth_cfg).into())
        .await
        .expect("sqlite in-memory store must connect");
    auth_store.migrate().await.expect("migrations must run");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).auth = auth_cfg.clone();
    Arc::make_mut(&mut builder.config).limits = LimitsConfig {
        body_limit_bytes,
        ..LimitsConfig::default()
    };
    Arc::make_mut(&mut builder.config).documents = nagent_server::config::DocumentsConfig {
        enabled: true,
        cache_dir: std::env::temp_dir().join(format!(
            "nagent-doc-body-limit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )),
        max_file_size_bytes: documents_max_file_size_bytes,
        ..nagent_server::config::DocumentsConfig::default()
    };
    let cache_dir = builder.config.documents.cache_dir.clone();
    std::fs::create_dir_all(&cache_dir).expect("doc cache_dir must be creatable");

    let doc_store = nagent_server::documents::DocumentStore::new(
        auth_store.clone(),
        100_000,
        20,
        20_000,
        cache_dir,
        0,
        30,
    );
    builder = builder.with_auth(auth_store.clone());
    builder = builder.with_documents(doc_store);
    builder = builder.with_chat_sessions(nagent_server::chat::sessions::ChatSessions::new(
        auth_store.admin().chat_sessions.clone(),
    ));
    (builder.build(), auth_store, auth_cfg)
}

/// Mint a chat session via the HTTP route and return its UUID.
async fn mint_chat_session(app: &axum::Router, bearer: &str) -> Uuid {
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/v1/chat/session")
                .header(header::HOST, "127.0.0.1:0")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("POST /v1/chat/session");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /v1/chat/session must return 200"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    Uuid::parse_str(
        body["id"]
            .as_str()
            .expect("/v1/chat/session response must carry `id`"),
    )
    .expect("chat session id must be a valid UUID")
}

/// Build a small multipart/form-data body in memory and return the
/// bytes + the boundary so the caller can set the
/// `Content-Type: multipart/form-data; boundary=…` header. The
/// payload is a `text/plain` part named `file` containing `bytes`
/// under `filename = "upload.txt"`.
fn build_multipart_text_upload(bytes: &[u8], boundary: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 256);
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\r\n",
    );
    out.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// `POST /v1/documents` with a body that exceeds
/// `[server.limits].body_limit_bytes` (8 KiB) but fits inside
/// `[documents].max_file_size_bytes` (1 MiB) must reach the
/// handler. Before the S-1b fix the transport cap chopped the
/// multipart stream at 8 KiB and the browser saw axum's opaque
/// `400 Bad Request — "Error parsing multipart/form-data request"`.
/// With the fix, the inner `DefaultBodyLimit::max(documents_max)`
/// layer wins for documents routes and the handler runs to
/// completion; for a plain-text upload that fits inside the
/// documents cap the response is `201 Created`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_route_overrides_transport_body_limit() {
    let body_limit_bytes: usize = 8 * 1024;
    let documents_max_file_size_bytes: usize = 1024 * 1024;
    let (state, auth_store, _auth_cfg) =
        build_state_for_documents_body_limit_test(body_limit_bytes, documents_max_file_size_bytes)
            .await;
    let app = build_router(state);

    // Create a user + session directly in the DB so the test does
    // not depend on the password-login flow.
    let user_id = auth_store
        .admin()
        .users
        .create("alice@body-limit.test", "Alice", "local", Some(b"hash"))
        .await
        .expect("create_user");
    let session = auth_store
        .admin()
        .sessions
        .create(user_id, std::time::Duration::from_secs(60), None, None)
        .await
        .expect("create_session");
    let bearer = session
        .plaintext_token
        .clone()
        .expect("create_session mints a plaintext token");

    let chat_session_id = mint_chat_session(&app, &bearer).await;

    // 100 KiB text payload — exceeds the 8 KiB transport cap but
    // fits inside the 1 MiB documents cap.
    let payload: Vec<u8> = (0..100 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    assert!(
        payload.len() > body_limit_bytes,
        "payload must exceed the transport cap for the regression to fire"
    );
    assert!(
        payload.len() <= documents_max_file_size_bytes,
        "payload must fit inside the documents cap"
    );

    let boundary = "nagent-test-boundary";
    let body = build_multipart_text_upload(&payload, boundary);
    // The Origin / Host guard (plan S-2) treats a POST without
    // `Origin` as a programmatic client and allows it ONLY when
    // `Host` matches the allow-list; the test harness binds
    // `127.0.0.1:0`, so we mirror that here.
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/v1/documents")
                .header(header::HOST, "127.0.0.1:0")
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .header("x-chat-session-id", chat_session_id.to_string())
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("POST /v1/documents");
    let status = resp.status();
    let response_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let response_text = String::from_utf8_lossy(&response_bytes).into_owned();
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "documents route must NOT return 400 for a body above the \
         transport cap when the documents cap allows it \
         (got 400 with body: {response_text:?})"
    );
    assert!(
        !response_text.contains("Error parsing `multipart/form-data` request"),
        "documents route must not surface the opaque axum MultipartError body \
         (response: {response_text:?})"
    );
    assert_eq!(
        status,
        StatusCode::CREATED,
        "documents route must reach the handler and return 201 for a \
         plain-text upload that fits the documents cap \
         (got {status} with body: {response_text:?})"
    );
}
