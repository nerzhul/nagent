//! Embedded static frontend assets.
//!
//! The `static/` directory is bundled into the binary at compile time by
//! `rust-embed`. The `index.html` and supporting files (app.js, style.css)
//! live there.

use rust_embed::Embed;

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
