//! Text extraction for uploaded documents.
//!
//! Supports two MIME families:
//! - `text/plain` (`.txt`) — read as UTF-8 with lossy fallback.
//! - `application/pdf` (`.pdf`) — extracted via the `pdf-extract`
//!   crate, gated behind the `documents` cargo feature.
//!
//! Everything else is rejected at the route layer with
//! `415 Unsupported Media Type`. The extractor never throws a
//! `Result::Err` for "no text could be extracted" — it returns the
//! empty string so the upload still succeeds and the LLM sees a
//! tool result that explains why the document was empty.
//!
//! The PDF path is synchronous (`pdf_extract::extract_text_from_mem`)
//! and can take several seconds on a 200-page file. Callers MUST
//! run it on a blocking task with a timeout — see
//! [`crate::documents::routes::upload_handler`].

use std::time::Duration;

/// MIME types we know how to extract. Other types are rejected
/// upstream with `415` so this list is intentionally small and
/// versioned alongside the extractor implementations below.
pub const TEXT_MIME: &str = "text/plain";
pub const PDF_MIME: &str = "application/pdf";

/// Maximum time we will spend on a single PDF extract. Mirrors the
/// `[documents].pdf_extract_timeout_secs` default; the caller passes
/// the resolved value in so unit tests can shrink it.
pub fn extract_text(
    bytes: &[u8],
    extension: &str,
    timeout: Duration,
) -> Result<ExtractionResult, ExtractionError> {
    let ext_lower = extension.to_ascii_lowercase();
    let ext_str = ext_lower.as_str();
    if matches!(ext_str, "txt" | "md" | "log" | "") {
        return Ok(ExtractionResult {
            text: String::from_utf8_lossy(bytes).into_owned(),
            page_count: None,
            mime: TEXT_MIME.to_string(),
        });
    }
    if ext_str == "pdf" {
        return extract_pdf(bytes, timeout);
    }
    Err(ExtractionError::UnsupportedMime(format!(
        "unsupported extension: .{ext_str}"
    )))
}

fn extract_pdf(bytes: &[u8], timeout: Duration) -> Result<ExtractionResult, ExtractionError> {
    use std::sync::mpsc;
    use std::thread;

    // `pdf_extract::extract_text_from_mem` is synchronous and
    // CPU-bound; we run it on a worker thread so the async runtime
    // stays free, and we enforce the timeout via a channel.
    let (tx, rx) = mpsc::channel::<Result<String, pdf_extract::OutputError>>();
    let payload = bytes.to_vec();
    let handle = thread::spawn(move || {
        let result = pdf_extract::extract_text_from_mem(&payload);
        // `tx.send` only fails if the receiver was dropped, which
        // happens when the timeout fired and the caller moved on.
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(text)) => {
            // `join` is best-effort: the worker has already
            // returned by the time we get here.
            let _ = handle.join();
            // lopdf 0.34 / pdf-extract 0.7 do not expose a page
            // count without a second pass over the bytes. v1
            // reports `None` and lets the LLM use `page_range`
            // semantics to bound the read instead.
            Ok(ExtractionResult {
                text,
                page_count: None,
                mime: PDF_MIME.to_string(),
            })
        }
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(ExtractionError::ParseFailed(e.to_string()))
        }
        Err(_timeout) => Err(ExtractionError::Timeout(timeout)),
    }
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
    /// The PDF parser returned a non-recoverable error. Surfaces
    /// as `422 Unprocessable Entity` so the UI can show
    /// "could not parse PDF" without polluting the access log.
    #[error("pdf parse failed: {0}")]
    ParseFailed(String),
    /// The PDF parse did not finish before the timeout. The
    /// route layer unlinks the partial file before returning
    /// `422`.
    #[error("pdf extract exceeded timeout of {0:?}")]
    Timeout(Duration),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_passthrough_round_trips_utf8() {
        let bytes = "Hello, 世界\nLine 2\n".as_bytes();
        let res = extract_text(bytes, "txt", Duration::from_secs(1)).expect("txt extract");
        assert_eq!(res.mime, TEXT_MIME);
        assert_eq!(res.text, "Hello, 世界\nLine 2\n");
        assert_eq!(res.page_count, None);
    }

    #[test]
    fn unknown_extension_is_rejected() {
        // `.png` slips past the upload handler's mime sniff (the
        // browser may label it `image/png` even when the extension
        // is `.pdf`). The extractor must still refuse to run.
        let res = extract_text(b"not an image", "png", Duration::from_secs(1));
        assert!(matches!(res, Err(ExtractionError::UnsupportedMime(_))));
    }

    #[test]
    fn txt_passthrough_uses_lossy_for_invalid_utf8() {
        // The extractor must never panic on bad bytes — replace
        // with the U+FFFD replacement char so the LLM still gets
        // SOMETHING to look at.
        let bytes: &[u8] = &[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72];
        let res = extract_text(bytes, "txt", Duration::from_secs(1)).expect("txt extract");
        assert!(res.text.contains("foo"));
        assert!(res.text.contains("bar"));
    }
}
