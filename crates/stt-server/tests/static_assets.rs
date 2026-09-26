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
use stt_server::{
    build_router,
    config::RateLimitConfig,
    rate_limit::{RateLimitPolicy, RateLimiter},
    AppState, Config as ServerConfig,
};
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
        limits: stt_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        llm: stt_server::config::LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
            system_prompt: None,
        },
        agents: stt_server::config::AgentConfig::default(),
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
async fn ui_relabels_system_prompt_to_additional_instructions() {
    // The admin-configured `LLM_SYSTEM_PROMPT` is the new canonical
    // system message; the textarea in `index.html` is an *extension*
    // the user can append. Renaming the label is a load-bearing
    // documentation change — the previous "System prompt" wording
    // would mislead users into thinking their input replaces the
    // server default. Substring guard against the rename being
    // reverted or skipped in a future refactor.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("Additional instructions"),
        "index.html no longer carries the `Additional instructions` label \
         for the #chat-system textarea. Users would mistake the field \
         for a full system-prompt replacement instead of an extension \
         appended after the server default."
    );
    assert!(
        html.contains("placeholder=\"Optional. Appended to the server's default system prompt.\""),
        "index.html #chat-system placeholder no longer mentions the server default. \
         Without it, users have no in-UI signal that the server prepends its own prompt."
    );
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

    // Guard against the duplicate-declaration footgun this commit
    // fixes: a sloppy `Edit` of the markdown layer had left two
    // `function renderMarkdown` declarations in the file, which the
    // browser then surfaces as `Uncaught SyntaxError: redeclaration
    // of function renderMarkdown` and the chat dies on first reply.
    let render_md_count = body.matches("function renderMarkdown(").count();
    assert_eq!(
        render_md_count, 1,
        "chat.js declares `function renderMarkdown` {render_md_count} times (expected 1). Duplicate declarations make the chat fail with `Uncaught SyntaxError: redeclaration of function renderMarkdown` on first reply."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_js_routes_shared_vad_through_active_recorder() {
    // Both Transcript and Discussion modes create their own
    // `AudioCapture` (each with its own WebSocket) but share a single
    // MicVAD — there is only one microphone stream. MicVAD accepts
    // its `onFrameProcessed` / `onSpeechEnd` callbacks *once at
    // construction time* and offers no API to swap them later, so the
    // shared closures must dispatch through a mutable "active
    // recorder" pointer that the currently-recording instance sets in
    // `_onButtonClick` and clears in `_stop`. Without this dispatch,
    // the first instance to call `ensureVad()` permanently owns the
    // VAD events — audio frames would keep being sent through that
    // instance's (eventually closed) WebSocket, and the second
    // instance would silently stop receiving FinalTranscripts after a
    // single mode switch.
    //
    // This is a substring-level guard: it does not exercise the JS,
    // it only fails if the dispatch mechanism disappears from the
    // served file (e.g. a refactor reintroduces per-instance closures
    // that capture `this`).
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/audio.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("let activeRecorder = null"),
        "audio.js is missing the module-level `activeRecorder` pointer. Without it, the first AudioCapture to initialize the VAD permanently owns the shared MicVAD callbacks and the second mode silently loses voice input."
    );

    // The shared VAD's frame and speech-end callbacks must route
    // through `activeRecorder`, not capture `this` of whichever
    // instance initialized the VAD first. We check for the dispatch
    // shim inside both callbacks.
    assert!(
        body.contains("const rec = activeRecorder;")
            && body.contains("rec.pendingAudioTs.push(performance.now());")
            && body.contains("rec._sendFrame(encodeAudioFrame(audioFloat32));"),
        "audio.js VAD closures do not dispatch through `activeRecorder`. Audio frames will be sent to the wrong instance's WebSocket on mode switches."
    );

    // The pointer must be set *before* `sharedVad.start()` so the
    // first post-resume frame already routes to the new instance's
    // WebSocket, and cleared in `_stop()` so stale frames between
    // `sharedVad.pause()` and the WS close get dropped.
    assert!(
        body.contains("activeRecorder = this;")
            && body.contains("if (activeRecorder === this) activeRecorder = null;"),
        "audio.js does not set/clear `activeRecorder` on the recorder lifecycle. Either the wiring is missing or it has been moved to the wrong hook."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_js_yields_other_recorders_on_record_click() {
    // Pressing Record in one mode must stop the other mode's
    // `AudioCapture` and discard the audio it was capturing at that
    // instant — otherwise the user gets a stray FinalTranscript in
    // the wrong mode whenever they cross-talk between Transcript
    // and Discussion. Implementation contract:
    //
    //   - Every `AudioCapture` registers itself in a module-level
    //     set so the Record-click handler can find its siblings.
    //   - Each click captures a generation counter, then iterates
    //     that set and `_stop()`s every other instance.
    //   - Each `_stop()` bumps the generation, and the in-flight
    //     `_onButtonClick` checks it after every `await` to avoid
    //     racing through `_connect()` after a takeover.
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/audio.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("const allRecorders = new Set()"),
        "audio.js is missing the module-level `allRecorders` registry. Without it, clicking Record in one mode has no way to find and stop the AudioCapture instance owned by the other mode."
    );

    // The constructor must register `this`. We check for the call site
    // rather than the constructor line because the source has shifted
    // around across refactors and a brittle anchor would fail
    // unnecessarily.
    assert!(
        body.contains("allRecorders.add(this)"),
        "audio.js no longer registers each AudioCapture in `allRecorders` from its constructor. Cross-instance takeover is impossible."
    );

    // The Record-click handler must iterate the registry and `_stop()`
    // every peer before starting its own session.
    assert!(
        body.contains("for (const other of allRecorders)")
            && body.contains("if (other !== this) other._stop()"),
        "audio.js Record-click handler no longer tears down peer AudioCapture instances. Pressing Record in one mode leaves the other mode's session running, and audio captured at that instant still ends up in its transcript/chat."
    );

    // Generation counter: each click must capture its own generation,
    // and each `_stop()` must bump it, so an in-flight setup bails out
    // after a takeover instead of clobbering `activeRecorder`.
    assert!(
        body.contains("this._clickGeneration = 0")
            && body.contains("++this._clickGeneration")
            && body.contains("myGen !== this._clickGeneration"),
        "audio.js is missing the click-generation guard. After a takeover, the losing click can race past `await this._connect()` and overwrite `activeRecorder`, breaking the new owner."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_aborts_inflight_reply_on_mode_toggle() {
    // Switching modes is symmetric in the visual layer (`.view[hidden]
    // { display: none }` hides the inactive view), but the in-flight
    // LLM reply in Discussion is owned by `chat.js` and has no DOM
    // hook — it needs an explicit `modechange` listener that aborts
    // the controller and resets the turn queue. Without it, tokens
    // keep streaming into a hidden view and any queued turns (typed
    // or transcribed) wake up against a session the user just left.
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains(r#"document.addEventListener("modechange""#),
        "chat.js does not listen for `modechange`. Switching to the Transcript tab while a chat reply is streaming leaves tokens flowing into the hidden Discussion view."
    );

    // The listener must (a) only act on the way *out* of Discussion
    // (otherwise switching back would abort nothing-and-everything),
    // and (b) actually abort the controller + drop the queue — same
    // teardown shape as `switchToSession` / `newSession` / `clearChat`.
    assert!(
        body.contains("e?.detail?.mode === \"discussion\"")
            && body.contains("if (inflight) inflight.controller.abort()")
            && body.contains("resetTurnQueue()"),
        "chat.js modechange handler does not call both `inflight.controller.abort()` and `resetTurnQueue()`, or it does not guard against firing when switching back to Discussion. Without both calls the in-flight reply or the queued turns survive a mode switch."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_parses_tool_call_and_tool_result_sse_events() {
    // The LLM proxy emits named SSE events (`event: tool_call`,
    // `event: tool_result`, `event: error`) alongside the OpenAI
    // `data:` chunks. chat.js must route each event to the matching
    // handler — without this, tool bubbles never render and the
    // assistant bubble stays on its initial loader spinner.
    //
    // This is a substring-level guard against the SSE-routing code
    // disappearing from a refactor.
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("currentEventName === \"tool_call\""),
        "chat.js no longer routes `event: tool_call` to the tool-bubble renderer. Tool calls from the LLM proxy will never render."
    );
    assert!(
        body.contains("currentEventName === \"tool_result\""),
        "chat.js no longer routes `event: tool_result` to resolveToolBubble. Tool results stay invisible and the spinner never clears."
    );
    assert!(
        body.contains("appendToolBubble(") && body.contains("resolveToolBubble("),
        "chat.js is missing appendToolBubble/resolveToolBubble — the tool-bubble DOM contract is broken."
    );
    // The named-event paths must call JSON.parse on a *trimmed*
    // payload. The server emits single-line `data:` so any
    // implementation that accumulates with a trailing `\n` will
    // throw on `JSON.parse` and silently drop the event. The
    // existence of `dataPayload.trim()` next to the named-event
    // branches is what we guard here.
    assert!(
        body.contains("JSON.parse(dataPayload.trim())"),
        "chat.js does not trim `dataPayload` before JSON.parse on the named-event branches (tool_call / tool_result / error). Trailing whitespace from multi-line accumulation would throw JSON.parse and silently drop the event — exactly the bug that left tool bubbles stuck on the spinner."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn css_hides_inactive_view_with_higher_specificity() {
    // Both mode views are `<main>` elements. The Transcript tab sets
    // `hidden` on `#view-discussion` (and vice versa) to swap the
    // active panel. CSS specificity is the catch: `#view-discussion
    // { display: flex; ... }` has specificity (1, 0, 0), so a plain
    // `.view[hidden] { display: none; }` rule at (0, 2, 0) loses and
    // the Discussion panel keeps showing — the user clicks Transcript
    // and nothing visibly changes (besides the in-flight abort path).
    //
    // The fix raises the hide rule's specificity by including the ID,
    // so `(1, 1, 0)` beats the flex override. This test guards the
    // served CSS for both halves of that contract: the selector list
    // mentions both view IDs, and it really is `display: none`.
    let base = serve_once().await;
    let css = reqwest::get(format!("{base}/static/style.css"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The hide rule must mention both view IDs together with the
    // `[hidden]` attribute and `display: none`. We don't pin the
    // exact line layout (single-line vs multi-line) — a brittle
    // anchor would just force a future formatter refactor to fight
    // this test.
    assert!(
        css.contains("#view-transcript[hidden]")
            && css.contains("#view-discussion[hidden]")
            && css.contains("display: none"),
        "style.css no longer carries the high-specificity `[hidden] {{ display: none }}` rule for both view IDs. Without it, the `#view-discussion {{ display: flex }}` rule wins on specificity and the inactive view stays visible after a mode switch."
    );
}
