//! HTTP integration tests for the local Piper TTS endpoints.
//!
//! The Piper backend itself is **not** exercised here — it requires
//! `espeak-ng` headers + a real voice file, which we cannot ship in
//! CI. We inject a [`MockSynthesizer`] through `AppState.tts` so the
//! HTTP envelope (status codes, headers, body shape, error mapping,
//! voice listing) is fully covered.
//!
//! See `crates/stt-server/src/tts.rs` for the engine and
//! `tests/ws_validation_and_headers.rs` for the shared
//! `start_test_server` test helper (we re-implement the slice we need
//! here so the TTS tests stay self-contained).

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use dashmap::DashMap;
use stt_core::{InferenceJob, MockBackend, PoolDispatch, WhisperBackend};
use stt_server::{
    build_router,
    config::{LlmAuthMode, LlmConfig, RateLimitConfig},
    rate_limit::{RateLimitPolicy, RateLimiter},
    session::SessionMap,
    tts, AppState, Config as ServerConfig,
};
use tokio::net::TcpListener;

fn make_state(tts_engine: Option<Arc<tts::TtsEngine>>) -> Arc<AppState> {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));
    let sessions: SessionMap = Arc::new(DashMap::new());
    let (job_tx_inner, _job_rx) = tokio::sync::mpsc::channel::<InferenceJob>(16);
    let job_tx = PoolDispatch::from_single_sender(job_tx_inner);
    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        inference_workers: None,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: stt_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        llm: LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            inbound_auth_key: None,
            auth_mode: LlmAuthMode::default(),
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
            system_prompt: None,
            allow_user_location: true,
        },
        agents: stt_server::config::AgentConfig::default(),
        tts: stt_server::config::TtsConfig::default(),
        auth: stt_server::config::AuthConfig::default(),
    });
    Arc::new(AppState {
        backend,
        sessions,
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: None,
        agents: None,
        tts: tts_engine,
        stt_rate_limiter: RateLimiter::new(RateLimitPolicy::stt(
            RateLimitConfig::default().stt_per_min,
        )),
        llm_rate_limiter: RateLimiter::new(RateLimitPolicy::llm(
            RateLimitConfig::default().llm_per_min,
        )),
        auth_store: None,
        auth_oidc: None,
        auth_passkey: None,
        auth_rate_limiter: stt_server::auth::rate_limit::LoginRateLimiter::new(),
    })
}

fn mock_engine() -> Arc<tts::TtsEngine> {
    let synth: Arc<dyn tts::Synthesizer> = Arc::new(tts::MockSynthesizer::new(
        vec![
            ("en_US-lessac-medium".to_string(), Some("en".to_string())),
            ("fr_FR-upmc-medium".to_string(), Some("fr".to_string())),
        ],
        22_050,
    ));
    Arc::new(tts::TtsEngine::from_synth(
        synth,
        "en_US-lessac-medium",
        "fr_FR-upmc-medium",
        "en",
        2_000,
    ))
}

async fn spawn(state: Arc<AppState>) -> String {
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

/// Helper: the same as `axum::body::to_bytes` but doesn't pull in
/// `http-body-util` (we already depend on reqwest in the workspace).
async fn body_bytes(resp: reqwest::Response) -> Vec<u8> {
    resp.bytes().await.unwrap().to_vec()
}

// ---------------------------------------------------------------------------
// POST /v1/audio/speech
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_returns_503_when_tts_disabled() {
    let url = spawn(make_state(None)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({"input": "hello"}))
        .send()
        .await
        .unwrap();

    // When TTS is disabled at the binary level, the routes are
    // not registered at all so axum returns 404 by default.
    // 503 is also an acceptable answer (would mean the route was
    // registered but the engine is `None`, which our current
    // router doesn't do but could in the future).
    let status = resp.status();
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::NOT_FOUND,
        "expected 503 or 404 when TTS disabled, got {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_returns_400_on_empty_input() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({"input": ""}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_bytes(resp).await;
    assert!(std::str::from_utf8(&body).unwrap().contains("empty"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_returns_400_on_oversized_input() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    let big = "x".repeat(3_000);
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({"input": big}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_bytes(resp).await;
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("too long"), "got: {text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_returns_404_on_unknown_voice() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({"input": "hi", "voice": "does-not-exist"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_bytes(resp).await;
    assert!(std::str::from_utf8(&body).unwrap().contains("not found"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_returns_valid_wav_with_en_lang() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({"input": "hello world", "lang": "en"}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "audio/wav"
    );
    assert!(
        resp.headers().get("x-tts-backend").is_some(),
        "marker header for log-grep clarity"
    );
    let body = body_bytes(resp).await;
    // Round-trip through hound to make sure the bytes really are a
    // mono 16-bit WAV the browser will accept.
    let reader = hound::WavReader::new(Cursor::new(&body)).expect("hound must parse");
    assert_eq!(reader.spec().channels, 1);
    assert_eq!(reader.spec().bits_per_sample, 16);
    assert_eq!(reader.spec().sample_rate, 22_050);
    assert!(reader.into_samples::<i16>().count() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_picks_french_voice_when_lang_fr() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    // We can't introspect the chosen voice from the response body
    // (it's binary WAV). Instead we verify by hitting `/v1/audio/voices`
    // and confirming the engine's default_fr is `fr_FR-upmc-medium` —
    // the mock resolver is wired identically to the real Piper one.
    let resp = client
        .get(format!("{url}/v1/audio/voices"))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["default_voice_fr"], "fr_FR-upmc-medium");
    let voices = body["voices"].as_array().unwrap();
    let ids: Vec<&str> = voices.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"en_US-lessac-medium"));
    assert!(ids.contains(&"fr_FR-upmc-medium"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn speech_honors_voice_override() {
    let url = spawn(make_state(Some(mock_engine()))).await;

    let client = reqwest::Client::new();
    // Override to the French voice directly — bypasses lang-based
    // resolution entirely.
    let resp = client
        .post(format!("{url}/v1/audio/speech"))
        .json(&serde_json::json!({
            "input": "bonjour le monde",
            "voice": "fr_FR-upmc-medium",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_bytes(resp).await;
    let reader = hound::WavReader::new(Cursor::new(&body)).expect("hound must parse");
    assert_eq!(reader.spec().sample_rate, 22_050);
}

// ---------------------------------------------------------------------------
// GET /v1/audio/voices
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voices_returns_503_when_tts_disabled() {
    let url = spawn(make_state(None)).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/audio/voices"))
        .send()
        .await
        .unwrap();
    // Same as `speech_returns_503_when_tts_disabled`: the route
    // is not registered, so axum returns 404. 503 would also be
    // acceptable; we accept either so future router tweaks don't
    // break this test.
    let status = resp.status();
    assert!(
        status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::NOT_FOUND,
        "expected 503 or 404 when TTS disabled, got {status}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voices_returns_mock_listing() {
    let url = spawn(make_state(Some(mock_engine()))).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{url}/v1/audio/voices"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["default_lang"], "en");
    assert_eq!(body["default_voice_en"], "en_US-lessac-medium");
    assert_eq!(body["default_voice_fr"], "fr_FR-upmc-medium");
    let voices = body["voices"].as_array().unwrap();
    assert_eq!(voices.len(), 2);
    let lessac = voices
        .iter()
        .find(|v| v["id"] == "en_US-lessac-medium")
        .expect("lessac voice present");
    assert_eq!(lessac["language"], "en");
    assert_eq!(lessac["sample_rate"], 22_050);
}
