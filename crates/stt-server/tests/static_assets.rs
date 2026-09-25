//! Smoke tests for the embedded static frontend assets.
//!
//! The chat UI pulls in a few vendored JavaScript libraries (`marked`,
//! DOMPurify) for markdown rendering. These tests guard the
//! compile-time `rust-embed` wiring so a future refactor that drops
//! one of the files (or breaks the `<script>` tags in `index.html`)
//! surfaces as a CI failure rather than a silent UI regression.
//!
//! The tests do not exercise any JavaScript — they only confirm that
//! the right bytes are reachable at the right paths with the right
//! MIME types.

use std::sync::Arc;
use std::time::Duration;

use stt_core::MockBackend;
use stt_server::{build_router, AppState, Config as ServerConfig};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

/// Spin up an axum router on an ephemeral port. Mirrors the boilerplate
/// in `multiuser_isolation.rs`; kept inline so this file stays
/// self-contained (each `tests/*.rs` is a separate compilation unit).
async fn serve_once() -> String {
    let backend: Arc<dyn stt_core::WhisperBackend> = Arc::new(MockBackend::new("test-model"));
    let server_cfg = Arc::new(ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        llm: stt_server::config::LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
        },
    });
    let sessions = Arc::new(dashmap::DashMap::new());
    let (job_tx, job_rx) = mpsc::channel::<stt_core::InferenceJob>(16);
    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(16);
    let _worker = stt_core::InferenceWorker::spawn(Arc::clone(&backend), job_rx);
    let _shutdown = stt_server::router::ResultRouter::spawn(Arc::clone(&sessions), resp_rx);
    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx: job_tx.clone(),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: None,
    });
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Hold the shutdown sender alive until the test ends so the server
    // does not exit between the test body and any later assertions.
    std::mem::forget(tx);
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_html_references_markdown_vendors() {
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The chat UI parses assistant replies through `marked` and
    // sanitizes the HTML through `DOMPurify`. If either `<script>` tag
    // is missing, the chat will fall back to rendering raw markdown
    // source (`**bold**`, etc.) to the user.
    assert!(
        html.contains("/static/vendor/marked/marked.min.js"),
        "index.html does not include the marked vendor script tag"
    );
    assert!(
        html.contains("/static/vendor/sanitize/purify.min.js"),
        "index.html does not include the DOMPurify vendor script tag"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn markdown_vendors_are_served_with_js_mime() {
    let base = serve_once().await;

    for (path, expect_banner) in [
        (
            "/static/vendor/marked/marked.min.js",
            "marked v", // banner starts with "marked v15.0.7 - a markdown parser"
        ),
        (
            "/static/vendor/sanitize/purify.min.js",
            "DOMPurify", // banner: "DOMPurify 3.2.4"
        ),
    ] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "{path} did not return 200"
        );
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ctype.starts_with("application/javascript"),
            "{path} had unexpected Content-Type `{ctype}` (expected `application/javascript…`)"
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains(expect_banner),
            "{path} body did not contain the expected banner `{expect_banner}` — was the file replaced?"
        );
    }
}
