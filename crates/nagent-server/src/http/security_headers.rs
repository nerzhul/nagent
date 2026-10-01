//! HTTP middleware: security headers, CORS, and the access log.
//!
//! Two `tower_http` layers are exposed:
//!
//! - [`security_headers_layer`]: applied to every response, including
//! the static frontend, the `/healthz` probe, the `/api/version`
//! endpoint, and the WebSocket upgrade handshake. It sets a strict
//! Content-Security-Policy, a `Referrer-Policy: no-referrer`, and an
//! `X-Content-Type-Options: nosniff` marker so the browser applies
//! the MIME types we sent (in particular `font/woff2` for KaTeX
//! fonts — see [`crate::http::static_assets::mime_for`]).
//!
//! - [`cors_layer`]: applied **only** to the `/v1/*` routes. The
//! default `LLM_CORS_ALLOW_ORIGINS=""` produces an "allow no
//! origins" layer, i.e. every cross-origin preflight gets a 403
//! and the browser never even attempts the call. Operators who
//! point a third-party OpenAI-compatible client at the server set
//! the env var to a comma-separated allow-list.
//!
//! The CSP is deliberately strict on `script-src` and `connect-src`
//! (both `'self'` only) so a compromised dependency cannot pull a
//! remote script or exfiltrate data to a third-party endpoint.
//! KaTeX and marked need inline `<style>` for math layout and
//! code-block theming, which is why `style-src 'self' 'unsafe-inline'`
//! is the only loosening — see `static/vendor/katex/katex.min.css` and
//! `static/vendor/marked/marked.min.js`.
//!
//! `'wasm-unsafe-eval'` is added to `script-src` because the
//! vendored `onnxruntime-web` worker (`vendor/ort/ort.min.js`,
//! `vendor/ort/ort-wasm-simd-threaded.mjs`) compiles its WASM
//! modules at runtime via `WebAssembly.instantiateStreaming()`. Without
//! this token the Silero VAD model fails to load with
//! `CompileError: call to WebAssembly.instantiateStreaming() blocked
//! by CSP` and the entire audio pipeline dies. The token is scoped to
//! `'self'` (no remote WASM) so the practical exposure is limited to
//! same-origin binaries — i.e. files we explicitly vendored under
//! `static/vendor/`.
//!
//! [`access_log_middleware`] is wired as the **outermost** layer on
//! the whole router (above the security headers) so it sees every
//! request and the final status, including 401s from `RequireAuth`.
//! The resolved `AuthUser` flows in via the response extensions —
//! `RequireAuth` injects it on the way back, so the access log
//! attributes the request to a user without doing its own DB
//! lookup. When `RequireAuth` short-circuits (no cookie / bearer /
//! expired session), the response carries no `AuthUser` extension
//! and the line is logged with `user_id = None` and an empty
//! `email` — exactly the right shape for "anonymous request" in a
//! log aggregator.

use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;

/// Build the always-on security headers layer.
///
/// The CSP is constructed without per-origin dynamic content so it
/// can be baked into a `SetResponseHeaderLayer`; operators that need
/// `connect-src` to allow a third-party origin can extend the layer
/// at the call site.
pub fn security_headers_layer() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             script-src 'self' 'wasm-unsafe-eval'; \
             style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; \
             font-src 'self'; \
             connect-src 'self'; \
             media-src 'self' blob:; \
             worker-src 'self' blob:; \
             object-src 'none'; \
             base-uri 'self'; \
             frame-ancestors 'none'",
        ),
    )
}

/// `Referrer-Policy: no-referrer` — never leak the nagent URL to
/// upstream resources (KaTeX fonts, fonts referenced from CSS, etc.).
pub fn referrer_policy_layer() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    )
}

/// `X-Content-Type-Options: nosniff` — stop the browser from guessing
/// a MIME type for our static assets (the `font/woff2` mapping in
/// `mime_for` would be silently bypassed otherwise).
pub fn nosniff_layer() -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    )
}

/// Build the CORS layer for `/v1/*` based on
/// `LLM_CORS_ALLOW_ORIGINS`. With an empty allow-list the layer is
/// built with `CorsLayer::new()` and a `.allow_origin([])` call so
/// cross-origin preflights are rejected outright. The headers the
/// proxy accepts are limited to `Content-Type` + `Authorization`
/// (the only ones the frontend and the OpenAI Python SDK send).
pub fn cors_layer(allow_origins: &[String]) -> CorsLayer {
    let methods = [
        axum::http::Method::GET,
        axum::http::Method::POST,
        axum::http::Method::OPTIONS,
    ];

    if allow_origins.is_empty() {
        // Same-origin only: explicitly empty allow-origin list.
        return CorsLayer::new()
            .allow_methods(methods.to_vec())
            .allow_headers(tower_http::cors::AllowHeaders::list([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
            ]))
            .allow_origin([]);
    }

    // Parse and validate the supplied origins. Anything that fails to
    // parse is logged and skipped — better to ship a server that
    // works for one fewer origin than to crash on boot.
    let parsed: Vec<HeaderValue> = allow_origins
        .iter()
        .filter_map(|s| match HeaderValue::from_str(s) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(origin = %s, error = %e, "ignoring invalid LLM_CORS_ALLOW_ORIGINS entry");
                None
            }
        })
        .collect();

    if parsed.is_empty() {
        return CorsLayer::new()
            .allow_methods(methods.to_vec())
            .allow_headers(tower_http::cors::AllowHeaders::list([
                header::CONTENT_TYPE,
                header::AUTHORIZATION,
            ]))
            .allow_origin([]);
    }

    CorsLayer::new()
        .allow_methods(methods.to_vec())
        .allow_headers(tower_http::cors::AllowHeaders::list([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
        ]))
        .allow_credentials(false)
        .allow_origin(parsed)
        .max_age(std::time::Duration::from_secs(3600))
    // Do NOT call `.vary([])` here. The default `CorsLayer` Vary set
    // is `Vary: Origin, Access-Control-Request-Method,
    // Access-Control-Request-Headers`; clearing it lets a caching
    // reverse proxy serve one origin's `Access-Control-Allow-Origin`
    // to a different origin and break CORS isolation. See
    // tower-http `cors/vary.rs::Default for Vary`.
}

/// Per-request access log. Emits one `tracing` event per HTTP
/// request with method, path, status, duration, peer IP, and
/// (when authenticated) the resolved `user_id` / `email`.
///
/// Wired as the outermost layer in [`crate::http::build_router`] so it
/// sees every request, including the 401s returned by
/// `RequireAuth` for missing/expired sessions. The user info
/// flows in via the response extensions: `RequireAuth` injects
/// the resolved `AuthUser` on the way back so this middleware
/// can attribute the request without doing its own DB lookup.
/// An anonymous request shows up with `user_id = None` and an
/// empty `email` — the same shape a log aggregator needs to
/// count "unauthenticated 401s" cleanly.
///
/// The level is `INFO` for 2xx/3xx, `WARN` for 4xx, and
/// `ERROR` for 5xx. A noisy CI test can down-grade to `INFO`
/// for everything with `RUST_LOG=info` (the access log line
/// is tagged `event = "http.access"` so it is easy to filter
/// out with `RUST_LOG=info,nagent_server::http::middleware::access=off`).
///
/// The peer IP comes from `ConnectInfo<SocketAddr>` — the same
/// extension that the LLM rate limiter reads. We pull it
/// directly from the request extensions instead of using it as
/// an extractor, so the middleware degrades gracefully when the
/// request did not come through `axum::serve(...)` (test harness
/// using `tower::ServiceExt::oneshot`, in-process CLI
/// subcommands, etc.). Without that fallback the extractor
/// would 500 every test that does not spin up a real TCP
/// listener.
pub async fn access_log_middleware(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // The peer IP lives on the request extensions when the
    // service was started via `axum::serve(..., app
    // .into_make_service_with_connect_info::<SocketAddr>())`.
    // Anything else (test harness, in-process) just sees `None`
    // and we log the line with `ip = ""` — the request still
    // gets through to the handler.
    let peer: Option<ConnectInfo<SocketAddr>> = req.extensions().get().cloned();
    let ip = peer
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_default();
    let started = std::time::Instant::now();

    let response = next.run(req).await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let status = response.status();

    // `RequireAuth` (when wired) puts the resolved `AuthUser` on the
    // response extensions. Anonymous requests and 401/403 responses
    // have no such extension and the line is logged with the user
    // fields empty — exactly the right shape for a log aggregator.
    //
    // We pre-render `user_id` to a `String` (empty when anonymous)
    // rather than logging the raw `Option<Uuid>`. The Debug
    // repr of `Option` is `Some(uuid)` / `None`, which would
    // either render the literal `Some(...)` (the bug that
    // surfaced here) or a bare `None` that a log aggregator
    // cannot distinguish from a real value. An empty string is
    // what `email` already does on line 229 — matching that
    // shape keeps every log filter ("`user_id != ""`" to count
    // authenticated requests, "`email ~= @`" for per-domain
    // cuts, …) consistent across the two fields.
    let user = response.extensions().get::<crate::auth::AuthUser>();
    let user_id = user.map(|u| u.id.to_string()).unwrap_or_default();
    let email = user.map(|u| u.email.as_str()).unwrap_or("");

    let level = if status.is_server_error() {
        tracing::Level::ERROR
    } else if status.is_client_error() {
        tracing::Level::WARN
    } else {
        tracing::Level::INFO
    };

    // Local macro: `tracing::event!` itself only accepts a
    // constant `Level`, so we dispatch on the runtime-resolved
    // level into the right per-level macro. The structured
    // fields carry `method` / `path` / `user_agent` (so a log
    // aggregator can index them without parsing the message);
    // the message uses `{method:?}` / `{path:?}` / `{user_agent:?}`
    // so spaces and embedded quotes in `path` (rare) and
    // `user_agent` (always) are properly quoted in the line.
    macro_rules! access_log {
        ($lvl:expr) => {
            tracing::event!(
                $lvl,
                event = "http.access",
                method = %method,
                path = %path,
                status = status.as_u16(),
                duration_ms = elapsed_ms,
                user_id = %user_id,
                email = %email,
                ip = %ip,
                user_agent = %user_agent,
                "http {method:?} {path:?} -> {status} in {elapsed_ms}ms from {ip}"
            )
        };
    }
    match level {
        tracing::Level::ERROR => access_log!(tracing::Level::ERROR),
        tracing::Level::WARN => access_log!(tracing::Level::WARN),
        _ => access_log!(tracing::Level::INFO),
    }

    response
}
