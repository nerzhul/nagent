//! HTTP middleware: security headers and CORS for the LLM proxy.
//!
//! Two `tower_http` layers are exposed:
//!
//! - [`security_headers_layer`]: applied to every response, including
//!   the static frontend, the `/healthz` probe, the `/api/version`
//!   endpoint, and the WebSocket upgrade handshake. It sets a strict
//!   Content-Security-Policy, a `Referrer-Policy: no-referrer`, and an
//!   `X-Content-Type-Options: nosniff` marker so the browser applies
//!   the MIME types we sent (in particular `font/woff2` for KaTeX
//!   fonts — see [`crate::static_assets::mime_for`]).
//!
//! - [`cors_layer`]: applied **only** to the `/v1/*` routes. The
//!   default `LLM_CORS_ALLOW_ORIGINS=""` produces an "allow no
//!   origins" layer, i.e. every cross-origin preflight gets a 403
//!   and the browser never even attempts the call. Operators who
//!   point a third-party OpenAI-compatible client at the server set
//!   the env var to a comma-separated allow-list.
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

use axum::http::{header, HeaderValue};
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
