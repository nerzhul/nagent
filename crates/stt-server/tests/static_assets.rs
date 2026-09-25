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
    // KaTeX is loaded both as a stylesheet (must precede any math
    // render) and as two scripts: the core library plus the
    // `renderMathInElement` helper from `contrib/auto-render`.
    assert!(
        html.contains("/static/vendor/katex/katex.min.css"),
        "index.html does not include the KaTeX stylesheet link"
    );
    assert!(
        html.contains("/static/vendor/katex/katex.min.js"),
        "index.html does not include the KaTeX core script tag"
    );
    assert!(
        html.contains("/static/vendor/katex/contrib/auto-render.min.js"),
        "index.html does not include the KaTeX auto-render script tag"
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
        (
            "/static/vendor/katex/katex.min.js",
            "katex", // banner: "@licstart KaTeX" / "katex.min.js"
        ),
        (
            "/static/vendor/katex/contrib/auto-render.min.js",
            "renderMathInElement",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn katex_stylesheet_and_fonts_are_served() {
    let base = serve_once().await;

    // The CSS file must arrive with a `text/css` content type or the
    // browser will refuse to apply it; the body must include the
    // `.katex` class so we know the file actually contains KaTeX's
    // styles and not an HTML 404 page.
    let css = reqwest::get(format!("{base}/static/vendor/katex/katex.min.css"))
        .await
        .unwrap();
    assert_eq!(css.status(), reqwest::StatusCode::OK);
    let css_ctype = css
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        css_ctype.starts_with("text/css"),
        "katex.min.css Content-Type was `{css_ctype}`"
    );
    let css_body = css.text().await.unwrap();
    assert!(
        css_body.contains(".katex"),
        "katex.min.css does not look like a KaTeX stylesheet"
    );

    // At least one font file must be served with the correct
    // `font/woff2` MIME type. Browsers refuse to load fonts with the
    // wrong Content-Type, so a missing `font/woff2` mapping in
    // `mime_for` would silently break all math glyphs.
    let font_path = "/static/vendor/katex/fonts/KaTeX_Main-Regular.woff2";
    let font = reqwest::get(format!("{base}{font_path}")).await.unwrap();
    assert_eq!(font.status(), reqwest::StatusCode::OK);
    let font_ctype = font
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        font_ctype.starts_with("font/woff2"),
        "{font_path} Content-Type was `{font_ctype}` (expected `font/woff2`)"
    );
    // The body should start with the WOFF2 magic bytes (`wOF2` in
    // ASCII). If this fails, the binary was corrupted in transit or
    // served from the wrong path.
    let bytes = font.bytes().await.unwrap();
    assert!(
        bytes.len() >= 4 && &bytes[..4] == b"wOF2",
        "{font_path} did not start with the WOFF2 magic — got {} bytes",
        bytes.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_carries_math_bracket_normalizer() {
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The chat JS must define `normalizeMathDelimiters`. Without it,
    // an LLM that wraps math in `[ ... ]` (its own LaTeX-flavoured
    // bracket pair) leaves the source verbatim on screen because
    // marked has no math support and KaTeX has no delimiters to match.
    assert!(
        body.contains("function normalizeMathDelimiters"),
        "chat.js no longer defines normalizeMathDelimiters — bracket-wrapped math will render as raw LaTeX source again"
    );

    // Both regex passes must guard against eating the inner of a real
    // markdown link (`(?!\s*\()`) and against re-processing the
    // already-converted `\[...\]` blocks (the `(?<!\\)` lookbehind on
    // the single-line pass). Without the lookbehind, the second pass
    // double-applies and the output ends up with stray `\\` prefixes.
    assert!(
        body.contains("(?<!\\\\)\\[(\\s*\\\\[a-zA-Z]"),
        "chat.js is missing the negative lookbehind on the single-line math regex — already-converted blocks will be re-processed and produce stray backslashes"
    );
    assert!(
        body.matches(r"(?!\s*\()").count() >= 2,
        r"chat.js no longer carries the (?!\s*\() link-protection lookahead on both math regex passes"
    );
}
