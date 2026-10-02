//! Embedded static frontend assets and the serving contract.
//!
//! Plan R8a: zero-copy `Cow` bodies (no per-request `into_owned()`),
//! per-file `ETag` derived from a SHA-256 of the embedded bytes,
//! `Cache-Control: no-cache` on first-party assets so the browser
//! revalidates with `If-None-Match` and gets a 304 when the embedded
//! bytes are unchanged, and a short `Cache-Control: public,
//! max-age=300` on the third-party `vendor/` bundles as a
//! placeholder until package I (phase 1) wires content-hashed
//! vendor URLs (`vendor/<name>.<hash8>.<ext>`) and we can promote
//! those to `Cache-Control: public, max-age=31536000, immutable`.
//!
//! The full immutable-vendor cache policy lands with package I
//! because rewriting `index.html` / `audio.js` references at build
//! time to inject the per-file hash is its concern, not this
//! module's.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use rust_embed::Embed;
use sha2::{Digest, Sha256};

#[derive(Embed)]
#[folder = "src/static/"]
pub struct StaticAssets;

impl std::fmt::Debug for StaticAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticAssets").finish()
    }
}

/// Guess a MIME type from the file extension.
pub fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".wasm") {
        // Streaming-compilation hint; falls back to octet-stream in
        // older browsers that don't know the type.
        "application/wasm"
    } else if path.ends_with(".onnx") {
        "application/octet-stream"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        // KaTeX ships WOFF2 fonts under `vendor/katex/fonts/`. Browsers
        // refuse to load a font declared with the wrong MIME type, so
        // we need an explicit `font/woff2` mapping here — otherwise
        // math glyphs fall back to the system serif and look broken.
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}

/// Build an `ETag` header value (quoted, weak) from a SHA-256 of the
/// supplied bytes. The first 16 hex chars (8 bytes) make a value
/// that is short enough to be readable in DevTools and long enough
/// to make accidental collisions astronomically unlikely.
///
/// The `W/` prefix marks these as weak ETags per RFC 9110 §8.8.3:
/// the same bytes will hash identically across requests, but a
/// reverse proxy that re-encodes the body would invalidate a strong
/// ETag while a weak one only signals "same content equivalence".
fn etag_for(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex = format!("{digest:x}");
    format!("W/\"{}\"", &hex[..16])
}

/// Build the `Cache-Control` value for a given asset path.
///
/// Policy:
/// - `index.html` and `version.txt` MUST be revalidated every
///   request so the chat UI catches the "binary changed, page did
///   not" drift. `no-cache` keeps the bytes cached locally and
///   lets the browser short-circuit with a 304 instead of a full
///   re-download.
/// - All other first-party assets (`app.js`, `style.css`, …) get
///   the same `no-cache` policy. Per-file ETags make the 304 round
///   trips free for repeat loads; the alternative (`max-age=…`)
///   would need content-hashed URLs to invalidate on edit.
/// - Third-party bundles under `vendor/` get a short
///   `max-age=300` placeholder. Promoting these to `immutable`
///   needs package I (phase 1) to wire content-hashed vendor URLs
///   so a new bundle version automatically changes the request
///   URL and the cached copy becomes unreachable.
fn cache_control_for(path: &str) -> &'static str {
    if path == "index.html" || path == "version.txt" {
        "no-cache"
    } else if path.starts_with("vendor/") {
        "public, max-age=300"
    } else {
        "no-cache"
    }
}

/// Compute the response for a static asset.
///
/// `if_none_match` is the raw `If-None-Match` header value (which
/// may contain multiple comma-separated tags per RFC 9110
/// §13.1.2). When any of them equals the asset's ETag, the
/// response is `304 Not Modified` with no body — the browser
/// uses its cached copy. Otherwise the full body is returned with
/// the serving contract (`Content-Type`, `ETag`, `Cache-Control`).
pub fn serve(path: &str, if_none_match: Option<&HeaderValue>) -> Response {
    let Some(file) = StaticAssets::get(path) else {
        // Return a real 404 (not "200 OK" + an empty body). Firefox's
        // source-map resolver, the browser's preload scanner, and
        // service-worker caches all key off the status code: a 200
        // with zero bytes is treated as "the asset exists and is
        // broken", which surfaces as a `JSON.parse: unexpected end
        // of data` console error every time the user opens DevTools.
        // `.map` files in particular are fetched opportunistically
        // by Firefox for every minified vendor script — `ort.min.js`,
        // `purify.min.js`, `marked.min.js`, the KaTeX bundle — and
        // we deliberately do NOT ship those `.map` files in the
        // binary (the devtools UX in production is not worth the
        // extra megabytes).
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(format!("not found: {path}")))
            .expect("static 404 builder is valid");
    };

    let etag = etag_for(file.data.as_ref());

    // `If-None-Match` may contain multiple tags (RFC 9110 §13.1.2);
    // a hit on any one of them counts as a match. We also accept the
    // bare `*` wildcard (RFC 9110 §13.1.2: "If the field value is
    // `*`, the condition is true if the origin has a current
    // representation").
    let cached = if_none_match.is_some_and(|raw| {
        let raw = match raw.to_str() {
            Ok(s) => s,
            Err(_) => return false,
        };
        raw.split(',').any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag == etag
        })
    });

    if cached {
        // 304 must NOT carry a `Content-Type` (RFC 9110 §15.4.5: the
        // entity headers from the cached response are reused by the
        // client). It DOES carry `ETag` so the client knows which
        // version it now holds.
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, HeaderValue::from_str(&etag).unwrap())
            .header(
                header::CACHE_CONTROL,
                HeaderValue::from_static(cache_control_for(path)),
            )
            .body(Body::empty())
            .expect("static 304 builder is valid");
    }

    // Zero-copy: `file.data` is `Cow<'static, [u8]>` from rust-embed.
    // The embedded slice lives in the binary's read-only segment;
    // passing it straight into `Body::from` avoids the per-request
    // `Vec<u8>` allocation that `into_owned()` would do. (`Body::from`
    // recognises `Cow<'_, [u8]>` and wraps the borrowed variant
    // directly into a `Bytes` body.)
    let mut response = Response::new(Body::from(file.data));
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(mime_for(path)),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control_for(path)),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etag_is_deterministic_and_starts_with_weak_marker() {
        let a = etag_for(b"hello");
        let b = etag_for(b"hello");
        let c = etag_for(b"hello!");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("W/\""));
        assert!(a.ends_with('"'));
    }

    #[test]
    fn etag_length_is_16_hex_inside_quotes() {
        let e = etag_for(b"any bytes");
        // `W/"` (3 chars) + 16 hex chars + closing `"` (1 char) = 20 chars.
        assert_eq!(e.len(), 3 + 16 + 1);
        assert!(e.starts_with("W/\""));
        assert!(e.ends_with('"'));
        let inner = &e[3..3 + 16];
        assert!(inner.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cache_control_first_party_is_no_cache() {
        assert_eq!(cache_control_for("index.html"), "no-cache");
        assert_eq!(cache_control_for("version.txt"), "no-cache");
        assert_eq!(cache_control_for("app.js"), "no-cache");
        assert_eq!(cache_control_for("style.css"), "no-cache");
    }

    #[test]
    fn cache_control_vendor_is_short_max_age() {
        assert_eq!(
            cache_control_for("vendor/ort/ort.min.js"),
            "public, max-age=300"
        );
        assert_eq!(
            cache_control_for("vendor/katex/fonts/A.woff2"),
            "public, max-age=300"
        );
    }

    #[test]
    fn mime_for_woff2_is_font() {
        assert_eq!(
            mime_for("vendor/katex/fonts/KaTeX_Main-Regular.woff2"),
            "font/woff2"
        );
    }
}
