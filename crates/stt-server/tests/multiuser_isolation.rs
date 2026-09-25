//! End-to-end test that **proves** the per-session isolation invariant.
//!
//! ## What this test asserts
//!
//! 1. Two concurrent WebSocket clients (`alice`, `bob`) connect.
//! 2. Each sends two `AudioFrame` messages through the wire.
//! 3. Each receives exactly the transcripts that include **its own**
//!    session ID echoed by the mock backend.
//! 4. No transcript leaks across sessions.
//! 5. When one client disconnects, the server drops subsequent results
//!    for it silently, and the other client is not perturbed.
//!
//! The mock backend echoes `format!("session=<id>")`, so the test can
//! assert the wire-level identity of every transcript without trusting
//! the server's internal state.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use futures_util::{SinkExt, StreamExt};
use stt_core::{InferenceJob, InferenceWorker, MockBackend, WhisperBackend};
use stt_proto::{
    decode_frame, encode, AudioFrame, Config, FinalTranscript, Payload, StartSession, Tag,
};
use stt_server::{
    build_router,
    config::RateLimitConfig,
    rate_limit::{RateLimitPolicy, RateLimiter},
    router::ResultRouter,
    session::SessionMap,
    AppState, Config as ServerConfig,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

/// Build a complete in-process pipeline (worker + router + axum app) and
/// return the bound address so the test can dial it.
async fn start_test_server() -> (String, SessionMap) {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));

    // Dummy config; tests don't bind to a real port via Config.
    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: stt_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        // LLM is opt-in; the STT-focused multiuser test keeps it off
        // so the /v1/* routes are not registered and there is no
        // accidental dependency on a local Ollama install.
        llm: stt_server::config::LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
        },
        agents: stt_server::config::AgentConfig::default(),
    });

    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());

    let (job_tx, job_rx) = mpsc::channel::<InferenceJob>(16);
    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(16);

    // Worker + result router.
    let _worker = InferenceWorker::spawn(Arc::clone(&backend), job_rx);
    let _shutdown = ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx: job_tx.clone(),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: None,
        agents: None,
        stt_rate_limiter: RateLimiter::new(RateLimitPolicy::stt(
            RateLimitConfig::default().stt_per_min,
        )),
        llm_rate_limiter: RateLimiter::new(RateLimitPolicy::llm(
            RateLimitConfig::default().llm_per_min,
        )),
    });

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/ws");

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Keep tx alive for the duration of the test.
    std::mem::forget(tx);

    (url, sessions)
}

/// Open a WebSocket client and skip the very first `BackendInfo` frame
/// the server sends.
async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    use tokio_tungstenite::connect_async;
    let (mut ws, _) = connect_async(url).await.expect("ws connect");
    let first = ws
        .next()
        .await
        .expect("BackendInfo frame")
        .expect("BackendInfo frame ok");
    match first {
        Message::Binary(buf) => {
            assert_eq!(buf[0], Tag::BackendInfo as u8);
            let payload = decode_frame(&buf).expect("decode BackendInfo");
            assert!(matches!(payload, Payload::Backend(_)));
        }
        other => panic!("expected binary BackendInfo, got {other:?}"),
    }
    ws
}

/// Read frames until we see `count` `FinalTranscript`s matching `filter`.
async fn collect_matching_transcripts<S>(
    ws: &mut S,
    count: usize,
    filter: impl Fn(&FinalTranscript) -> bool,
) -> Vec<FinalTranscript>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        match ws.next().await {
            Some(Ok(Message::Binary(buf))) => match decode_frame(&buf) {
                Ok(Payload::Final(t)) => {
                    if filter(&t) {
                        out.push(t);
                    } else {
                        panic!("unexpected transcript leaked: {:?}", t);
                    }
                }
                Ok(_) => {} // ignore other server frames
                Err(e) => panic!("decode error: {e}"),
            },
            Some(Ok(Message::Close(_))) | None => break,
            Some(Err(e)) => panic!("ws error: {e}"),
            _ => {}
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_do_not_cross_talk() {
    let (url, sessions) = start_test_server().await;

    // Sanity: server has no sessions yet.
    assert!(sessions.is_empty());

    let mut ws1 = connect(&url).await;
    let mut ws2 = connect(&url).await;

    // Sessions should now be visible in the registry.
    assert_eq!(sessions.len(), 2);

    // Collect the two server-side session IDs. We can't tell which is
    // which from outside, but we *can* assert they are distinct.
    let ids: Vec<Uuid> = sessions.iter().map(|e| *e.key()).collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    let id1 = ids[0];
    let id2 = ids[1];

    // Both clients open their session explicitly.
    ws1.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some("en".into()),
            sample_rate: 16_000,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();
    ws2.send(Message::Binary(
        encode(Payload::Start(StartSession {
            lang_hint: Some("fr".into()),
            sample_rate: 16_000,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();

    // Each client sends 2 distinct audio frames.
    for samples in [vec![1.0_f32; 1600], vec![-1.0_f32; 1600]] {
        ws1.send(Message::Binary(
            encode(Payload::Audio(AudioFrame { samples })).unwrap(),
        ))
        .await
        .unwrap();
    }
    for samples in [vec![2.0_f32; 1600], vec![-2.0_f32; 1600]] {
        ws2.send(Message::Binary(
            encode(Payload::Audio(AudioFrame { samples })).unwrap(),
        ))
        .await
        .unwrap();
    }

    // Helper: extract the session UUID embedded in a transcript.
    fn extract_session_id(t: &FinalTranscript) -> Option<Uuid> {
        // Mock text is "mock-session=<uuid>".
        let prefix = "mock-session=";
        let s = t.text.strip_prefix(prefix)?;
        Uuid::parse_str(s).ok()
    }

    // Each client receives 2 transcripts that all share the same session ID.
    let ws1_transcripts = collect_matching_transcripts(&mut ws1, 2, |_| true).await;
    assert_eq!(ws1_transcripts.len(), 2, "ws1 missed transcripts");
    let ws1_id = extract_session_id(&ws1_transcripts[0]).expect("uuid in text");
    for t in &ws1_transcripts {
        let id = extract_session_id(t).expect("uuid in text");
        assert_eq!(id, ws1_id, "ws1 saw mixed session IDs");
    }

    let ws2_transcripts = collect_matching_transcripts(&mut ws2, 2, |_| true).await;
    assert_eq!(ws2_transcripts.len(), 2, "ws2 missed transcripts");
    let ws2_id = extract_session_id(&ws2_transcripts[0]).expect("uuid in text");
    for t in &ws2_transcripts {
        let id = extract_session_id(t).expect("uuid in text");
        assert_eq!(id, ws2_id, "ws2 saw mixed session IDs");
    }

    // Cross-talk check: the two clients must have received *different*
    // session IDs (otherwise the server routed them to the same session).
    assert_ne!(
        ws1_id, ws2_id,
        "ws1 and ws2 share the same session ID — isolation broken"
    );

    // The IDs observed on the wire must match the ones in the SessionMap.
    assert!(ids.contains(&ws1_id));
    assert!(ids.contains(&ws2_id));
    // Either ws1_id is id1 or id2 — both are valid; just confirm consistency.
    let _ = (id1, id2);

    // Disconnect ws1; ws2 should still receive transcripts.
    ws1.send(Message::Close(None)).await.unwrap();
    ws2.send(Message::Binary(
        encode(Payload::Config(Config {
            language: Some("en".into()),
            translate: false,
        }))
        .unwrap(),
    ))
    .await
    .unwrap();
    ws2.send(Message::Binary(
        encode(Payload::Audio(AudioFrame {
            samples: vec![3.0_f32; 800],
        }))
        .unwrap(),
    ))
    .await
    .unwrap();
    let ws2_extra = collect_matching_transcripts(&mut ws2, 1, |_| true).await;
    assert_eq!(ws2_extra.len(), 1, "ws2 should still receive transcripts");
    let ws2_extra_id = extract_session_id(&ws2_extra[0]).expect("uuid in text");
    assert_eq!(ws2_extra_id, ws2_id, "ws2 session ID changed unexpectedly");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_returns_200() {
    let (url, _sessions) = start_test_server().await;
    // Convert ws URL -> http URL.
    let http_url = url
        .replacen("ws://", "http://", 1)
        .replace("/ws", "/healthz");
    let resp = reqwest::get(http_url).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_version_returns_expected_shape() {
    let (url, _sessions) = start_test_server().await;
    let http_url = url
        .replacen("ws://", "http://", 1)
        .replace("/ws", "/api/version");

    let resp = reqwest::get(http_url).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Version metadata must never be cached, otherwise a stale "we're
    // up to date" answer could defeat the reload banner.
    let cache_control = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        cache_control.contains("no-store") || cache_control.contains("no-cache"),
        "expected no-store / no-cache, got {cache_control:?}"
    );

    let info: stt_server::VersionInfo = resp.json().await.expect("VersionInfo json");
    // backend version comes from CARGO_PKG_VERSION; assert non-empty and
    // semver-shaped rather than hard-coding "0.1.0" so the test survives
    // a future version bump.
    assert!(!info.backend.is_empty(), "backend version is empty");
    assert!(
        info.backend
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '-' || c.is_ascii_alphabetic()),
        "backend version `{info_backend}` has unexpected characters",
        info_backend = info.backend
    );

    // frontend hash is 16 hex chars produced by build.rs.
    assert_eq!(
        info.frontend.len(),
        16,
        "frontend hash `{info}` looks wrong",
        info = info.frontend
    );
    assert!(
        info.frontend.chars().all(|c| c.is_ascii_hexdigit()),
        "frontend hash `{info}` contains non-hex characters",
        info = info.frontend
    );
}
