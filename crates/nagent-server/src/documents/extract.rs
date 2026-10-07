//! Text extraction for non-PDF uploaded documents.
//!
//! Supports one MIME family today:
//! - `text/plain` (`.txt`, `.md`, `.log`) — read as UTF-8 with
//!   lossy fallback.
//!
//! Everything else (notably `application/pdf`) is refused at the
//! route layer with `415 Unsupported Media Type`; the upload
//! route branches on the extension BEFORE calling into this
//! module so the PDF path (extraction-once at upload time +
//! encrypted per-page store) lives in [`super::pages`]. The
//! extractor never throws a `Result::Err` for "no text could be
//! extracted" — it returns the empty string so the upload still
//! succeeds and the LLM sees a tool result that explains why the
//! document was empty.

use std::time::Duration;

/// MIME types we know how to extract without a per-page index.
/// PDFs go through [`super::pages::extract_pages`] instead.
pub const TEXT_MIME: &str = "text/plain";

/// MIME marker the upload route uses to sniff a PDF. Re-exported
/// here so route / agent code that needs to branch on the MIME
/// does not have to hard-code the string. Mirrors the historical
/// `PDF_MIME` constant — kept under the old name so the route
/// layer's `sniff_mime` helper and the `DocumentSource` impl can
/// keep the same call pattern.
pub const PDF_MIME: &str = "application/pdf";

/// Legacy `pdf-extract` bounded-pool call. Today only the
/// legacy read path (pre-migration rows with `pages_dir = NULL`)
/// uses it; the upload route and the per-page read path go
/// through [`super::pages::extract_pages`] instead. Kept here
/// so the legacy path is reachable without re-adding `lopdf` or
/// `pdf-extract` to the agents module.
pub async fn extract_pdf_bounded(
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    bytes: &[u8],
    timeout: std::time::Duration,
) -> Result<ExtractionResult, ExtractionError> {
    use nagent_support::cpu::RunError;
    use std::time::Duration;
    let payload = bytes.to_vec();
    let cfg = nagent_support::cpu::BoundedConfig {
        semaphore: semaphore.clone(),
        queue: None,
        timeout: None,
    };
    let parse = nagent_support::cpu::run_bounded(cfg, move |_permit| {
        pdf_extract::extract_text_from_mem(&payload).map_err(|e| e.to_string())
    });

    let text = match tokio::time::timeout(timeout, parse).await {
        Ok(Ok(Ok(text))) => text,
        Ok(Ok(Err(parse_err))) => return Err(ExtractionError::ParseFailed(parse_err)),
        Ok(Err(RunError::QueueFull(max))) => {
            return Err(ExtractionError::Saturated { max_waiters: max });
        }
        Ok(Err(RunError::QueueTimeout(t))) => {
            return Err(ExtractionError::SaturatedTimeout(t));
        }
        Ok(Err(RunError::Closed)) => {
            return Err(ExtractionError::ParseFailed(
                "pdf semaphore closed during boot shutdown".into(),
            ))
        }
        Ok(Err(RunError::Join(e))) => {
            return Err(ExtractionError::ParseFailed(format!(
                "pdf extract task panicked: {e}"
            )))
        }
        Err(_elapsed) => return Err(ExtractionError::Timeout(timeout)),
    };

    // The legacy `pdf-extract` path cannot recover the page
    // count — `lopdf` is required for that. We leave the field
    // as `None` so callers fall through to the row's stored
    // value (the upload-route snapshot taken before the
    // migration).
    let _ = Duration::from_secs(0);
    Ok(ExtractionResult {
        text,
        page_count: None,
        mime: PDF_MIME.to_string(),
    })
}

/// Extract text from a plain-text / Markdown / log buffer. Cheap
/// (no copy, no allocation beyond the lossy UTF-8 conversion) and
/// stays synchronous because `from_utf8_lossy` is not CPU-heavy
/// and runs on the calling task.
pub fn extract_text(bytes: &[u8], extension: &str) -> Result<ExtractionResult, ExtractionError> {
    let ext_lower = extension.to_ascii_lowercase();
    let ext_str = ext_lower.as_str();
    if matches!(ext_str, "txt" | "md" | "log" | "") {
        return Ok(ExtractionResult {
            text: String::from_utf8_lossy(bytes).into_owned(),
            page_count: None,
            mime: TEXT_MIME.to_string(),
        });
    }
    Err(ExtractionError::UnsupportedMime(format!(
        "unsupported extension: .{ext_str}"
    )))
}

/// Successful extraction payload. Returned to the route handler
/// which writes the relevant fields into the `uploaded_documents`
/// row.
#[derive(Debug, Clone)]
pub struct ExtractionResult {
    pub text: String,
    pub page_count: Option<u32>,
    pub mime: String,
}

/// Errors surfaced by [`extract_text`]. Mapped to HTTP statuses at
/// the route layer.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    /// The MIME / extension is one the server does not know how
    /// to extract (e.g. a `.png` mistakenly routed here). Surfaces
    /// as `415 Unsupported Media Type`.
    #[error("unsupported mime: {0}")]
    UnsupportedMime(String),
    /// Reserved for the (now-removed) PDF branch. The PDF path
    /// lives in [`super::pages`] which surfaces its own error
    /// variant set (`PagesError`). Kept here so the route layer's
    /// match exhaustiveness does not break; mapping is to
    /// `422 Unprocessable Entity`.
    #[error("pdf parse failed: {0}")]
    ParseFailed(String),
    /// Reserved for the (now-removed) PDF branch. The route layer
    /// maps this to `422`.
    #[error("pdf extract exceeded timeout of {0:?}")]
    Timeout(Duration),
    /// Reserved for the (now-removed) PDF branch. The route layer
    /// maps this to `503 + Retry-After`.
    #[error("pdf extract saturated: queue full ({max_waiters} waiters)")]
    Saturated { max_waiters: usize },
    /// Reserved for the (now-removed) PDF branch. The route layer
    /// maps this to `429 + Retry-After`.
    #[error("pdf extract saturated: wait timeout of {0:?}")]
    SaturatedTimeout(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_passthrough_round_trips_utf8() {
        let bytes = "Hello, 世界\nLine 2\n".as_bytes();
        let res = extract_text(bytes, "txt").expect("txt extract");
        assert_eq!(res.mime, TEXT_MIME);
        assert_eq!(res.text, "Hello, 世界\nLine 2\n");
        assert_eq!(res.page_count, None);
    }

    #[test]
    fn unknown_extension_is_rejected() {
        // `.png` slips past the upload handler's mime sniff (the
        // browser may label it `image/png` even when the extension
        // is `.pdf`). The extractor must still refuse to run.
        let res = extract_text(b"not an image", "png");
        assert!(matches!(res, Err(ExtractionError::UnsupportedMime(_))));
    }

    #[test]
    fn txt_passthrough_uses_lossy_for_invalid_utf8() {
        // The extractor must never panic on bad bytes — replace
        // with the U+FFFD replacement char so the LLM still gets
        // SOMETHING to look at.
        let bytes: &[u8] = &[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72];
        let res = extract_text(bytes, "txt").expect("txt extract");
        assert!(res.text.contains("foo"));
        assert!(res.text.contains("bar"));
    }
}
