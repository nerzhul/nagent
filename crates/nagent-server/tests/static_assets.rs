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

use nagent_server::http::build_router;
use nagent_server::testing::app_state;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Spin up an axum router on an ephemeral port. Uses the
/// phase-5.B test builder so the only thing the test file owns
/// is the bind + listen boilerplate (mirrored in
/// `multiuser_isolation.rs`; kept inline so each `tests/*.rs`
/// stays self-contained — each is a separate compilation unit).
async fn serve_once() -> String {
    let state = app_state().build();
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
async fn missing_static_asset_returns_404() {
    // Firefox (and the browser preload scanner) probes for
    // `<asset>.map` next to every minified vendor script —
    // `ort.min.js.map`, `purify.min.js.map`, `marked.min.js.map`,
    // the KaTeX bundle. We deliberately do NOT ship those `.map`
    // files in the binary (devtools UX in production is not worth
    // the extra megabytes), so the server MUST answer with a real
    // 404, not "200 OK" + an empty body.
    //
    // The previous behaviour returned 200 + zero bytes for any
    // path the embedded `StaticAssets` did not contain. Firefox
    // treats that as "the source map exists and is broken" and
    // emits `JSON.parse: unexpected end of data at line 1 column 1`
    // every time the user opens DevTools — exactly the console
    // noise reported against `ort.min.js.map` and
    // `purify.min.js.map`.
    let base = serve_once().await;

    for path in [
        "/static/vendor/ort/ort.min.js.map",
        "/static/vendor/sanitize/purify.min.js.map",
        "/static/vendor/marked/marked.min.js.map",
        "/static/vendor/katex/katex.min.js.map",
        // A path that is also "missing" but exercises a
        // non-vendor lookup and the empty subdirectory case, just
        // to confirm the lookup is done by full key.
        "/static/does-not-exist.js",
    ] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path} returned {} instead of 404; a 200-with-empty-body response \
             would surface in Firefox as `JSON.parse: unexpected end of data` \
             every time the user opens DevTools.",
            resp.status(),
        );
        // The body must NOT carry a JS / source-map / JSON MIME
        // type — that combination is exactly what Firefox tries to
        // JSON.parse. We assert against the offending types so a
        // future refactor that changes the 404 MIME trips this
        // guard.
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            !ctype.starts_with("application/javascript")
                && !ctype.starts_with("application/json")
                && !ctype.starts_with("application/octet-stream"),
            "{path} returned Content-Type `{ctype}` for a missing asset. \
             Firefox / Chrome would attempt to parse this as a source map and \
             surface `JSON.parse: unexpected end of data` to the user.",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_static_assets_still_return_200() {
    // Regression guard for the previous fix: turning the
    // missing-asset path into a 404 must not flip existing
    // assets to 404 too. We probe a known-present vendor file
    // and assert its body starts with the right banner.
    let base = serve_once().await;
    for (path, banner) in [
        ("/static/vendor/ort/ort.min.js", "ort"),
        ("/static/vendor/sanitize/purify.min.js", "DOMPurify"),
    ] {
        let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "{path} regressed: existing vendor asset no longer served with 200 after the missing-asset 404 fix"
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains(banner),
            "{path} body did not contain expected banner `{banner}` — was the asset replaced?"
        );
    }
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
async fn chat_js_render_history_avoids_duplicate_tool_bubble_after_refresh() {
    // Regression guard for the "tool running…" pill that used to stick
    // around on past assistant turns after a page refresh. The
    // rehydration loop in `renderHistory` walks an assistant message's
    // `tool_calls[]` (creating one running `<details>` per id) and
    // then the matching `role: "tool"` entry. Calling
    // `appendToolBubble` again on the `role: "tool"` path produces a
    // second `<details>` with the same `data-tool-id`; the subsequent
    // `resolveToolBubble` only flips the FIRST match to `ok`, leaving
    // the duplicate stuck in `--running`.
    //
    // The fix gates the second `appendToolBubble` call on a
    // `querySelector` for an existing matching `<details>`. We assert
    // the source still carries the guard so a future refactor that
    // re-merges the two branches trips this test.
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("data-tool-id=\"${CSS.escape(id)}\""),
        "chat.js `renderHistory` lost the existing-`<details>` lookup used to deduplicate tool bubbles on refresh. Without it each tool call renders twice and one of them stays stuck on the 'running…' indicator."
    );
    // The persisted `role: \"tool\"` entry must carry the
    // server-curated `summary` so the rehydrated pill text matches
    // the live stream byte-for-byte (≤ 80 chars on success vs.
    // ≤ 117 chars + \"…\" recomputed from `content`).
    assert!(
        body.contains("summary: summary || \"\""),
        "chat.js `resolveToolBubble` no longer persists the server-curated `summary` on the `role: \"tool\"` history entry. A page refresh now shows a longer (and visually divergent) tool pill than the user saw live."
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_renders_weather_widget_for_get_weather() {
    // `get_weather` returns a structured JSON payload. chat.js is
    // expected to detect that in `resolveToolBubble`, parse the JSON,
    // and call `renderWeatherWidget` so the user gets a compact card
    // instead of scanning a prose paragraph. This substring-level
    // guard catches a refactor that drops the wiring silently — the
    // helper function, the detection branch, and the icon map entry
    // all have to be present together for the widget to render.
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("function renderWeatherWidget("),
        "chat.js is missing `renderWeatherWidget(...)`. The get_weather tool will fall back to the summary-only tool bubble and the structured card never renders."
    );
    assert!(
        body.contains("function conditionEmoji("),
        "chat.js is missing `conditionEmoji(...)`. Without the emoji lookup the widget has no icons."
    );
    assert!(
        body.contains("name === \"get_weather\""),
        "chat.js no longer keys the weather widget path on `name === \"get_weather\"`. The widget will not render (or will render for every tool)."
    );
    assert!(
        body.contains("renderWeatherWidget(div,"),
        "chat.js does not call `renderWeatherWidget(div, ...)` from inside the `name === \"get_weather\"` branch. The card is wired but never attached."
    );
    assert!(
        body.contains("get_weather: \""),
        "TOOL_ICON map is missing the get_weather icon. The header bubble falls back to the generic wrench glyph."
    );
    // The widget finalizes the assistant bubble on `get_weather`
    // success: a short one-line acknowledgment ("Voici les
    // informations demandées.") is left visible; a long restatement
    // of the card's fields is replaced with a one-liner hint. The
    // helper + the tool-pending hide + the deferral wiring in
    // streamReply must all be present together.
    assert!(
        body.contains("function finalizeAssistantForToolResult("),
        "chat.js is missing `finalizeAssistantForToolResult(...)`. The assistant prose bubble stays full-length after a successful get_weather and the widget loses visibility behind it."
    );
    assert!(
        body.contains("function setAssistantToolPending("),
        "chat.js is missing `setAssistantToolPending(...)`. The 'let me check…' LLM preamble bleeds through during the tool run; the widget should be the visual focus."
    );
    assert!(
        body.contains("inflight.weatherFinalizeEl"),
        "chat.js no longer defers weather finalization to stream end. Mid-stream judging of the LLM's prose would clobber tokens still being written after the tool result."
    );
    assert!(
        body.contains("chat-message--weather-replaced"),
        "chat.js no longer marks the finalized assistant bubble with `chat-message--weather-replaced`. The CSS rule in style.css loses its target and the bubble re-renders the prose."
    );
    assert!(
        body.contains("chat-message--tool-pending"),
        "chat.js no longer marks the assistant bubble with `chat-message--tool-pending` while the tool runs. The 'Préparation…' placeholder no longer hides the streaming prose."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_places_weather_card_outside_the_tool_trace() {
    // docs/ui_features.md §4.6.3 requires the weather card to live
    // OUTSIDE the tool trace `<details class="chat-message__tool-usage">`.
    // An earlier revision tucked the card inside the `<details>`, so
    // collapsing the tool summary silently hid the answer — exactly
    // the UX regression this guard exists to prevent. The card is
    // tracked on `assistantEl._weatherCards` so `applyMarkdown` can
    // re-insert it after the per-tick `innerHTML = ""` reset (the
    // card is a direct child of the bubble, not inside any container
    // that `applyMarkdown` re-mounts).
    let base = serve_once().await;
    let body = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The renderWeatherWidget function must find the enclosing tool
    // trace <details> via .closest() and insert the card as a sibling
    // of it — not as a child via `parentEl.insertAdjacentElement`.
    assert!(
        body.contains("parentEl.closest(\"details.chat-message__tool-usage\")"),
        "chat.js `renderWeatherWidget` no longer looks up the enclosing tool trace <details> via .closest(). Per docs/ui_features.md §4.6.3 the weather card must live as a sibling of the <details> so collapsing the tool summary doesn't hide the answer."
    );

    // The card must NOT be inserted via `parentEl.insertAdjacentElement`,
    // which would put it inside the <details> (parentEl is the
    // `.chat-tool-bubble` div whose direct parent is the <details>).
    // The test counts occurrences of the bare call as a substring
    // outside a comment; a real regression would re-introduce it.
    // We accept it ONLY inside the trailing "previous layout" comment.
    let adjacent_after = body.matches("parentEl.insertAdjacentElement").count();
    assert!(
        adjacent_after <= 1,
        "chat.js calls `parentEl.insertAdjacentElement` {adjacent_after} times — once would still put the weather card inside the <details>, which collapses it with the tool summary."
    );

    // The `_weatherCards` tracking array is the contract that lets
    // `applyMarkdown` re-insert the card after every `innerHTML = ""`.
    // Both the render path and the re-mount path must reference it.
    assert!(
        body.contains("_weatherCards"),
        "chat.js no longer tracks weather cards in `assistantEl._weatherCards`. Without it, `applyMarkdown`'s per-tick `innerHTML = \"\"` reset silently drops the card between streaming ticks."
    );

    // `applyMarkdown` must re-insert the tracked cards alongside
    // `_toolUsageEls`. The streaming tick path runs many times per
    // reply; losing the re-mount would make the card flicker in/out
    // on every delta.
    assert!(
        body.contains("bubbleEl._weatherCards"),
        "chat.js `applyMarkdown` does not re-insert tracked weather cards after its `innerHTML = \"\"` reset. Without this, the card disappears between streaming ticks and only reappears once streaming stops."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn css_carries_weather_card_rules() {
    // The weather widget relies on `.chat-weather-card` and the
    // responsive collapse to a single-column day strip below 520px.
    // A future CSS refactor that drops either rule would leave the
    // widget unstyled or horizontally scrolling on narrow viewports.
    let base = serve_once().await;
    let css = reqwest::get(format!("{base}/static/style.css"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        css.contains(".chat-weather-card"),
        "style.css no longer defines `.chat-weather-card`. The widget has no styling and renders as an unstyled block."
    );
    assert!(
        css.contains("chat-weather-card__days"),
        "style.css is missing the `.chat-weather-card__days` rule. The 3-day forecast strip is unstyled."
    );
    assert!(
        css.contains("chat-weather-card__wind"),
        "style.css is missing the dedicated `.chat-weather-card__wind` row. The user can't see wind at a glance."
    );
    assert!(
        css.contains("@media (max-width: 520px)") && css.contains("grid-template-columns: 1fr"),
        "style.css is missing the <=520px collapse to a single-column day strip. Mobile users see a horizontally scrolling widget."
    );
    assert!(
        css.contains("chat-message--weather-replaced"),
        "style.css is missing the `.chat-message--weather-replaced` rule. The suppressed assistant bubble reverts to the full prose look after a get_weather success."
    );
    assert!(
        css.contains("chat-message--tool-pending")
            && css.contains("display: none")
            && css.contains("chat-message__tool-loading"),
        "style.css is missing the `.chat-message--tool-pending` rule that hides the LLM's accumulating prose while a tool runs. The 'let me check…' preamble bleeds through."
    );
    // Regression guard: the weather widget is injected as a child of the
    // assistant bubble right after the `<details>` tool trace, but the
    // `chat-message--tool-pending` class stays on the bubble until the
    // SSE stream ends. The hide-everything-while-pending rule has to
    // exempt `.chat-weather-card` so the widget stays visible to the
    // user while the LLM still streams the post-tool preamble (or while
    // the stream is just slow to close). Without this exemption the
    // weather card renders as `display: none` and the user sees an
    // empty bubble where the widget should be.
    //
    // The check looks for the actual selector fragment, not just the
    // presence of both class names: a future CSS split that defines
    // `.chat-message--tool-pending { display: none }` on one line and
    // `.chat-weather-card { ... }` on another would satisfy a naive
    // `contains` check but still leave the bug in place.
    //
    // We walk every top-level rule and require that the same rule
    // (selector + body) carries BOTH the `.chat-message--tool-pending`
    // class on the selector side AND the `:not(.chat-weather-card)`
    // exemption on the same selector. The `display: none` body is
    // implied by the existing earlier assertion that this rule exists.
    //
    // Implementation note: we reset `rule` on the *closing* `}` of
    // each top-level rule, not on the opening `{`, so the selector
    // chars (which arrive before the `{`) survive into the captured
    // rule. Clearing on `{` would discard them and the check would
    // miss the actual selector.
    let mut tool_pending_rule_has_weather_exemption = false;
    let mut depth = 0;
    let mut rule = String::new();
    for ch in css.chars() {
        if ch == '{' {
            depth += 1;
            rule.push(ch);
            continue;
        }
        if ch == '}' {
            rule.push(ch);
            depth -= 1;
            if depth == 0 {
                if rule.contains("chat-message--tool-pending")
                    && rule.contains(":not(.chat-weather-card)")
                {
                    tool_pending_rule_has_weather_exemption = true;
                    break;
                }
                rule.clear();
            }
            continue;
        }
        rule.push(ch);
    }
    assert!(
        tool_pending_rule_has_weather_exemption,
        "style.css hides every non-`<details>` child of `.chat-message--tool-pending`; the weather widget card is not in the `:not(...)` exemption list and gets `display:none` while the tool trace and the prose stay visible."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn css_pins_inline_voice_graph_to_natural_height() {
    // The Discussion-mode voice oscilloscope (docs/ui_features.md
    // §4.10) lives inside `#chat-messages`, a flex column container
    // whose total content regularly exceeds the `max-height: 65vh`
    // cap. With `flex-shrink: 1` (the default for flex items), the
    // flex algorithm compresses the widget to the height of its
    // tallest unbreakable child (~17px for the voice-graph-header
    // pill) and clips the canvas. That's exactly the regression
    // the user reported: "le widget… il n'est plus assez haut et on
    // perd une partie du contenu à l'affichage".
    //
    // `flex-shrink: 0` on the inline variant keeps the widget at
    // its natural ~124px height so the canvas is fully visible.
    // The cost — the widget always reserves its full height inside
    // the scroll container — is intentional: sticky bottom then
    // keeps it pinned to the visible bottom edge.
    let base = serve_once().await;
    let css = reqwest::get(format!("{base}/static/style.css"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Look at the rule body for `.voice-graph--inline` and require
    // `flex-shrink: 0` to be present in the same rule body. We walk
    // top-level rules so a future refactor that splits the inline
    // variant across multiple selectors can't silently drop the
    // guard.
    let mut inline_rule_has_flex_shrink_zero = false;
    let mut depth = 0;
    let mut rule = String::new();
    for ch in css.chars() {
        if ch == '{' {
            depth += 1;
            rule.push(ch);
            continue;
        }
        if ch == '}' {
            rule.push(ch);
            depth -= 1;
            if depth == 0 {
                if rule.contains("voice-graph--inline") && rule.contains("flex-shrink: 0") {
                    inline_rule_has_flex_shrink_zero = true;
                    break;
                }
                rule.clear();
            }
            continue;
        }
        rule.push(ch);
    }
    assert!(
        inline_rule_has_flex_shrink_zero,
        "style.css is missing `flex-shrink: 0` on the `.voice-graph--inline` rule. Without it, the flex algorithm compresses the widget down to the height of its voice-graph-header pill and clips the canvas — the user reports the oscilloscope losing part of its content as soon as the conversation is long enough to scroll."
    );
}

#[allow(dead_code)]
async fn html_mounts_two_distinct_voice_graph_instances() {
    // The voice oscilloscope is documented as a "reusable widget, not a
    // single shared DOM node" (docs/ui_features.md §1.3): Transcript
    // mode mounts its instance at the top of the transcript view and
    // Discussion mode mounts its own instance inline inside
    // `#chat-messages` as a voice bubble (§4.10). A regression that
    // collapses the two back into a single shared element would break
    // the per-mode placement contract.
    //
    // We assert against the served HTML: both `id`s must exist, the
    // Transcript instance must sit inside `#view-transcript`, and the
    // Discussion instance must sit inside `#chat-messages`. We also
    // assert that no legacy `#voice-graph-shared` id remains — a
    // rename that left the old id dangling would mean the JS still
    // points at a now-stale element and the widget is invisible.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        html.contains(r#"id="voice-graph-transcript""#),
        "index.html no longer carries the Transcript-mode voice-graph instance. docs/ui_features.md §1.3 requires the widget to be mounted at the top of the transcript view; removing it would leave Transcript mode with no oscilloscope."
    );
    assert!(
        html.contains(r#"id="voice-graph-discussion""#),
        "index.html no longer carries the Discussion-mode voice-graph instance. docs/ui_features.md §4.10 requires the widget to be mounted inline as a voice bubble inside `#chat-messages`; removing it would leave Discussion mode with no oscilloscope."
    );
    assert!(
        !html.contains("voice-graph-shared"),
        "index.html still references the legacy `voice-graph-shared` id. Per docs/ui_features.md §1.3 the oscilloscope is a reusable widget, not a single shared DOM node — both modes must own their own instance."
    );

    // The Transcript instance must live inside the Transcript view.
    let transcript_view_start = html
        .find(r#"id="view-transcript""#)
        .expect("index.html is missing #view-transcript");
    let transcript_view_end = html[transcript_view_start..]
        .find("</main>")
        .map(|i| transcript_view_start + i)
        .expect("index.html is missing the closing </main> for #view-transcript");
    let transcript_graph_pos = html[transcript_view_start..transcript_view_end]
        .find(r#"id="voice-graph-transcript""#)
        .unwrap_or_else(|| panic!(
            "#voice-graph-transcript must be mounted inside #view-transcript per docs/ui_features.md §1.3, but it was placed outside the Transcript view."
        ));
    // The Transcript voice-graph must appear above the toolbar so it
    // sits "above the controls" (spec §1.3) — guard against a future
    // refactor that moves it to the bottom of the view.
    let controls_pos = html[transcript_view_start..transcript_view_end]
        .find(r#"class="controls""#)
        .expect("index.html is missing the Transcript-mode `.controls` section");
    assert!(
        transcript_graph_pos < controls_pos,
        "#voice-graph-transcript must be mounted above the Transcript-mode `.controls` per docs/ui_features.md §1.3; it was placed below them."
    );

    // The Discussion instance must live inside #chat-messages so it
    // scrolls with the conversation and sits "in the same scroll
    // context as the user / assistant turns" (spec §4.10).
    let chat_messages_start = html
        .find(r#"id="chat-messages""#)
        .expect("index.html is missing #chat-messages");
    let chat_messages_end = html[chat_messages_start..]
        .find("</div>")
        .map(|i| chat_messages_start + i)
        .expect("index.html is missing the closing </div> for #chat-messages");
    assert!(
        html[chat_messages_start..chat_messages_end].contains(r#"id="voice-graph-discussion""#),
        "#voice-graph-discussion must be mounted inside #chat-messages per docs/ui_features.md §4.10. Mounting it elsewhere (e.g. above or below the conversation) breaks the per-mode placement contract."
    );
    // The inline instance must carry the `voice-graph--inline` modifier
    // so the CSS can opt the bubble out of `#chat-messages`'s flex
    // `gap` (otherwise a hidden bubble leaves a phantom 0.75rem
    // below the last visible message).
    assert!(
        html.contains(r#"class="voice-graph voice-graph--inline"#),
        "Discussion-mode voice-graph must carry the `voice-graph--inline` modifier so the CSS removes it from the `#chat-messages` flex flow when hidden."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_and_chat_point_at_per_mode_voice_graph_ids() {
    // Both frontends must point their `AudioCapture` at the
    // per-mode voice-graph id, not the old shared id. A regression
    // that left a JS reference on `voice-graph-shared*` would mean
    // `$("voice-graph-shared-canvas")` returns null and the canvas
    // never draws — a silent UI regression that is hard to spot
    // without booting the browser.
    let base = serve_once().await;

    let app = reqwest::get(format!("{base}/static/app.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        app.contains("voice-graph-transcript-canvas")
            && app.contains("voice-graph-transcript-level")
            && app.contains(r#"$("voice-graph-transcript")"#),
        "app.js does not wire the Transcript-mode AudioCapture to its per-mode voice-graph ids. The canvas/level/graph references must all point at #voice-graph-transcript-* so the waveform renders in the transcript view."
    );
    assert!(
        !app.contains("voice-graph-shared"),
        "app.js still references the legacy `voice-graph-shared` id. The Transcript instance must use its own per-mode id."
    );

    let chat = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // The Discussion-mode AudioCapture is constructed lazily (see
    // `initAudioCaptureOnce` in chat.js — it lives inside
    // `<template id="app-shell-template">` so constructing it at
    // module top-level would crash on a null `cfg.buttonEl`).
    // We accept either the `$("…")` shorthand or the
    // `document.getElementById("…")` form.
    let uses_discussion_ids = |src: &str| {
        (src.contains("voice-graph-discussion-canvas")
            || src.contains("getElementById(\"voice-graph-discussion-canvas\")"))
            && (src.contains("voice-graph-discussion-level")
                || src.contains("getElementById(\"voice-graph-discussion-level\")"))
            && (src.contains("voice-graph-discussion")
                || src.contains("getElementById(\"voice-graph-discussion\")"))
    };
    assert!(
        uses_discussion_ids(&chat),
        "chat.js does not wire the Discussion-mode AudioCapture to its per-mode voice-graph ids. Per docs/ui_features.md §4.10 the Discussion instance must use its own canvas/level/graph ids so the inline voice bubble in #chat-messages renders independently of the Transcript view."
    );
    assert!(
        !chat.contains("voice-graph-shared"),
        "chat.js still references the legacy `voice-graph-shared` id. The Discussion instance must use its own per-mode id, not the shared one."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_js_preserves_inline_voice_graph_across_history_rehydrate() {
    // The Discussion-mode voice oscilloscope (docs/ui_features.md
    // §4.10) lives as a direct child of `#chat-messages`. The
    // `renderHistory` function wipes the conversation with
    // `messagesEl.innerHTML = ""` before re-rendering each history
    // message — without a preservation step, that reset orphans the
    // voice-graph element and the `AudioCapture`'s `graphEl`
    // reference silently dangles. Clicking Record then toggles
    // `is-hidden` on a detached node and the recording UI never
    // appears.
    //
    // The fix is a two-part contract in chat.js:
    //   1. `renderHistory` saves the inline voice-graph element
    //      *before* the wipe and re-appends it *after* the bubbles.
    //   2. `appendBubble` (and `appendError`) call
    //      `ensureInlineVoiceGraphAtEnd` after `messagesEl.appendChild`
    //      so the voice-graph stays the last child of the
    //      conversation across subsequent turns — the CSS uses
    //      `position: sticky; bottom: 0` to pin it to the bottom of
    //      the scroll container, and sticky only matches the bottom
    //      of the viewport when the element really is the last DOM
    //      child.
    //
    // A regression in either half makes the voice-graph disappear
    // the moment the page loads, and a stale-browser-cache issue
    // would look identical from the user's side.
    let base = serve_once().await;
    let chat = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The fix in `renderHistory`: save the inline voice-graph before
    // `messagesEl.innerHTML = ""` and re-append it after.
    assert!(
        chat.contains("messagesEl.querySelector(\".voice-graph--inline\")"),
        "chat.js `renderHistory` does not look up the inline voice-graph before its `messagesEl.innerHTML = \"\"` reset, so the wipe orphans the AudioCapture's `graphEl` reference. Per docs/ui_features.md §4.10 the discussion voice-graph must survive history rehydration; without this guard, clicking Record in Discussion mode shows no waveform."
    );
    // The re-append step: the saved reference is moved back into
    // `#chat-messages` after the bubbles are rendered.
    assert!(
        chat.contains("messagesEl.appendChild(inlineVoiceGraph)"),
        "chat.js `renderHistory` does not re-append the saved inline voice-graph after rehydrating bubbles. Without this, `#chat-messages` ends up without the voice-graph on every session switch / boot, and the recording UI never appears."
    );

    // The fix in `appendBubble` (and `appendError`): a helper
    // re-pins the inline voice-graph to the end of the conversation
    // every time a new bubble is appended, so `position: sticky;
    // bottom: 0` keeps matching the bottom of the viewport across
    // turns.
    assert!(
        chat.contains("function ensureInlineVoiceGraphAtEnd("),
        "chat.js is missing `ensureInlineVoiceGraphAtEnd()`. The inline voice-graph must be re-pinned to the last child of `#chat-messages` after every `messagesEl.appendChild` so the CSS `position: sticky; bottom: 0` continues to match the bottom of the chat-messages scroll container."
    );
    assert!(
        chat.contains("ensureInlineVoiceGraphAtEnd()"),
        "chat.js does not call `ensureInlineVoiceGraphAtEnd()` anywhere. Without this, every new bubble pushes the inline voice-graph above the bottom of the conversation and sticky positioning stops matching the viewport bottom."
    );
}

// ---- Transcript export (P0 — Export SRT / VTT / JSON) -----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_js_exposes_srt_vtt_json_builders() {
    // The export dropdown is wired to four pure functions inside
    // `app.js`: `buildSrt`, `buildVtt`, `buildJson`, `buildTxt`. A
    // future refactor that drops one of them would silently break
    // the matching menu item (the user clicks and nothing happens).
    // Substring-level guard against that.
    let base = serve_once().await;
    let app = reqwest::get(format!("{base}/static/app.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        app.contains("function buildSrt("),
        "app.js is missing `buildSrt()`. The SubRip export will produce an empty file."
    );
    assert!(
        app.contains("function buildVtt("),
        "app.js is missing `buildVtt()`. The WebVTT export will produce an empty file."
    );
    assert!(
        app.contains("function buildJson("),
        "app.js is missing `buildJson()`. The structured export will produce an empty file."
    );
    assert!(
        app.contains("function buildTxt("),
        "app.js is missing `buildTxt()`. The legacy plain-text export stops working."
    );
    // SRT/VTT must format timestamps with the millisecond separator
    // the spec requires. Both functions delegate to a single
    // `formatSrtTimestamp` helper that swaps the comma for a dot in
    // VTT; pin both halves of that contract so a future copy/paste
    // refactor does not regress them.
    assert!(
        app.contains("WEBVTT"),
        "buildVtt() no longer emits the `WEBVTT` magic header required by the WebVTT spec."
    );
    assert!(
        app.contains(".replace(\",\", \".\")"),
        "formatVttTimestamp is missing the comma→dot swap that differentiates SRT from WebVTT timestamps."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_html_wires_download_dropdown_to_exporters() {
    // The four export formats are listed in the `<details>` dropdown
    // by `data-format` attribute; `app.js` reads them back via
    // `[data-format]`. A refactor that uses a different selector
    // (e.g. `<button>` + JSON) would break the click handler. The
    // index must mention all four formats so every menu entry maps
    // to a real builder.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    for fmt in ["txt", "srt", "vtt", "json"] {
        assert!(
            html.contains(&format!("data-format=\"{fmt}\"")),
            "index.html is missing a `data-format=\"{fmt}\"` menu entry. The download dropdown will not offer this format."
        );
    }
    assert!(
        html.contains("id=\"download-menu\""),
        "index.html is missing `id=\"download-menu\"`. The dropdown wrapper cannot be enabled/disabled by app.js."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_js_passes_segments_to_on_final_transcript() {
    // `app.js` derives SRT/VTT timing from the per-chunk `segments[]`
    // carried in each `FinalTranscript`. If `audio.js` ever stops
    // surfacing that array to the callback (e.g. someone "cleans up"
    // the unused 4th argument), every caption export becomes empty.
    // Substring-level guard against that.
    let base = serve_once().await;
    let audio = reqwest::get(format!("{base}/static/audio.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        audio.contains("onFinalTranscript?.(p.text, p.lang, latencyMs, p.segments)"),
        "audio.js no longer forwards `p.segments` to `onFinalTranscript`. The SRT/VTT/JSON exporters will produce empty files."
    );
}

// ---- Keyboard shortcuts surface (P1) ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_html_mounts_shortcuts_modal() {
    // The `?` help modal lives at the bottom of `index.html`. It must
    // be present, hidden by default (`hidden` attribute), and expose
    // `role="dialog"` so screen readers announce it. The static
    // markup is the only place we keep the user-facing list — it has
    // to mirror the wiring in `shortcuts.js`, hence the paired
    // substring assertions.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        html.contains("id=\"shortcuts-modal\""),
        "index.html is missing the `#shortcuts-modal` dialog wrapper. The `?` shortcut has no surface to open."
    );
    assert!(
        html.contains("role=\"dialog\""),
        "index.html help modal is missing `role=\"dialog\"`. Screen readers will not announce it as a dialog."
    );
    // Every shortcut documented in `shortcuts.js` must appear as a
    // legend entry so the modal stays in sync with the actual
    // bindings. Pinning the marker strings rather than the wording
    // keeps the test stable through copy tweaks.
    for marker in [
        "Toggle recording",
        "Toggle voice recording",
        "Stop an in-flight",
        "Export the current transcript",
    ] {
        assert!(
            html.contains(marker),
            "index.html help modal is missing the `{marker}` entry. The legend has drifted from the keyboard wiring in shortcuts.js."
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_html_wraps_app_shell_in_template() {
    // The "global portal" contract: when the server has
    // `auth.enabled = true` and the visitor is anonymous, the
    // chat/voice UI must not appear in the document tree at all.
    // The whole UI lives inside a `<template id="app-shell-template">`
    // and `auth.js` clones it into `#app-root` only after a
    // successful `/api/me` probe. The login modal stays outside
    // the template so the auth flow can run before any UI is
    // mounted.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        html.contains("<template id=\"app-shell-template\""),
        "index.html is missing <template id=\"app-shell-template\">. The app shell must live in an inert template so anonymous users do not see the UI in DevTools."
    );
    assert!(
        html.contains("<div id=\"app-root\">"),
        "index.html is missing the empty <div id=\"app-root\"> mount point where auth.js clones the template."
    );
    // The login modal is intentionally outside the template so it
    // can be shown before the app shell is mounted.
    assert!(
        html.contains("<dialog id=\"login-modal\""),
        "index.html is missing the <dialog id=\"login-modal\"> wrapper. The login modal must live in the body so auth.js can show it before the template is cloned."
    );
    // The Transcript and Discussion views + the shortcuts modal must
    // sit INSIDE the template (not as direct children of <body>),
    // otherwise they are visible to anonymous visitors.
    let template_open = html
        .find("<template id=\"app-shell-template\"")
        .expect("app-shell-template must be present");
    let template_close = html.rfind("</template>").expect("template must be closed");
    assert!(
        template_open < template_close,
        "template open/close markers are out of order"
    );
    for must_be_inside in [
        "<main id=\"view-transcript\"",
        "<main id=\"view-discussion\"",
        "id=\"shortcuts-modal\"",
        "id=\"auth-pill\"",
    ] {
        let pos = html
            .find(must_be_inside)
            .unwrap_or_else(|| panic!("index.html is missing `{must_be_inside}`"));
        assert!(
            pos > template_open && pos < template_close,
            "`{must_be_inside}` must live inside <template id=\"app-shell-template\"> so it is absent from the DOM for anonymous users. Found at position {pos}, template spans {template_open}..{template_close}."
        );
    }
    // Conversely, the login modal must live OUTSIDE the template.
    let modal_pos = html
        .find("<dialog id=\"login-modal\"")
        .expect("login modal must be present");
    assert!(
        modal_pos < template_open || modal_pos > template_close,
        "<dialog id=\"login-modal\"> must live outside the app-shell template, but it is inside it."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_js_is_loaded_outside_template() {
    // The auth probe, login form, and mount/unmount logic live in
    // `auth.js`. It is the only script in `index.html` that is
    // loaded directly in the body (outside the template) — every
    // other script (mode.js, chat.js, app.js, shortcuts.js) is
    // inside the template so they only execute after `auth.js`
    // has cloned it.
    let base = serve_once().await;
    let html = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let auth_tag = "<script type=\"module\" src=\"/static/auth.js\">";
    let pos = html
        .find(auth_tag)
        .unwrap_or_else(|| panic!("index.html is missing `{auth_tag}`"));
    let template_open = html
        .find("<template id=\"app-shell-template\"")
        .expect("app-shell-template must be present");
    let template_close = html.rfind("</template>").expect("template must be closed");
    assert!(
        pos < template_open || pos > template_close,
        "`{auth_tag}` must live outside the app-shell template so it can run before the rest of the UI exists. Found at position {pos}, template spans {template_open}..{template_close}."
    );
    // Verify the auth.js source itself ships the portal pieces.
    let auth_src = reqwest::get(format!("{base}/static/auth.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for marker in [
        "/api/me",
        "login-modal--forced",
        "nagent:logout",
        "window.nagentAuth",
        "app-shell-template",
    ] {
        assert!(
            auth_src.contains(marker),
            "auth.js is missing `{marker}`. The portal contract is incomplete."
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shortcuts_js_loads_and_lists_every_binding() {
    // Pairs with `index_html_mounts_shortcuts_modal`: the JS side
    // must keep every binding it advertises in the help. We check
    // for the actual `keydown` guards so a refactor that drops one
    // (e.g. `Ctrl+S`) fails CI instead of silently breaking the
    // shortcut.
    let base = serve_once().await;
    let js = reqwest::get(format!("{base}/static/shortcuts.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    for needle in [
        "e.key === \"?\"",
        "e.key === \"Escape\"",
        "(e.key === \"s\" || e.key === \"S\")",
        "(e.key === \"R\" || e.key === \"r\")",
        "__nagentExportTranscript",
        "isTypingTarget(e.target)",
    ] {
        assert!(
            js.contains(needle),
            "shortcuts.js is missing the binding guarded by `{needle}`. Update the help modal legend at the same time."
        );
    }
    assert!(
        js.contains("shortcuts-modal"),
        "shortcuts.js no longer references the `#shortcuts-modal` element. The `?` key handler has nothing to open."
    );
    // Regression guard for the bug where typing `?` inside the chat
    // textarea popped the help modal over the half-typed message.
    // Pin the helper definition so a future "simplification" that
    // drops it surfaces in CI rather than as a user-visible glitch.
    assert!(
        js.contains("function isTypingTarget("),
        "shortcuts.js is missing the `isTypingTarget` helper. The `?`, `Ctrl+S`, and `Ctrl+Shift+R` shortcuts will pop over focused editable fields (chat textarea, system-prompt textarea, etc.)."
    );
}

/// Regression guard for the "403 on /v1/chat/completions after
/// login" bug.
///
/// The server's `RequireAuth` middleware rejects every state-changing
/// request (POST/PUT/PATCH/DELETE) on a protected route that does
/// not carry the per-session `x-csrf-token` header. The login
/// response hands the token to the SPA via `authState.user.csrf_token`
/// (and the cloned app.js / chat.js / tts.js read it back through
/// `window.nagentAuth.csrfHeaders()`), but a `fetch()` call that
/// builds its own `headers` object without spreading the helper
/// will miss it and be rejected.
///
/// We pin the marker string in every JS module that issues a
/// state-changing fetch to a protected route so a future refactor
/// that drops the spread (or re-introduces a hardcoded `headers`
/// object) is caught in CI instead of as a user-visible 403.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_changing_fetches_carry_csrf_header() {
    let base = serve_once().await;
    // chat.js: chat completions + TTS test voice.
    let chat = reqwest::get(format!("{base}/static/chat.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // tts.js: two distinct playback call sites.
    let tts = reqwest::get(format!("{base}/static/tts.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for (name, body, needle) in [
        ("chat.js (chat completions)", &chat, "csrfHeaders()"),
        ("chat.js (TTS test voice)", &chat, "csrfHeaders()"),
        ("tts.js (first playback site)", &tts, "csrfHeaders()"),
        ("tts.js (second playback site)", &tts, "csrfHeaders()"),
    ] {
        assert!(
            body.contains(needle),
            "{name} is missing `{needle}`. The fetch() call to a protected route will be rejected by RequireAuth with 403 because no x-csrf-token header is sent. Spread the helper in the headers object: {{ 'Content-Type': 'application/json', ...window.nagentAuth?.csrfHeaders() }}."
        );
    }
    // Pin the helper itself so a future "simplification" that drops
    // it from auth.js surfaces here.
    let auth = reqwest::get(format!("{base}/static/auth.js"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        auth.contains("csrfHeaders"),
        "auth.js is missing the `csrfHeaders` helper. Protected POSTs have no way to read the per-session CSRF token."
    );
}
