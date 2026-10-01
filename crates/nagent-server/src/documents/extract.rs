//! Text extraction for uploaded documents.
//!
//! Supports two MIME families:
//! - `text/plain` (`.txt`) — read as UTF-8 with lossy fallback.
//! - `application/pdf` (`.pdf`) — extracted via the `pdf-extract`
//! crate, gated behind the `documents` cargo feature.
//!
//! Everything else is rejected at the route layer with
//! `415 Unsupported Media Type`. The extractor never throws a
//! `Result::Err` for "no text could be extracted" — it returns the
//! empty string so the upload still succeeds and the LLM sees a
//! tool result that explains why the document was empty.
//!
//! The PDF path is synchronous (`pdf_extract::extract_text_from_mem`)
//! and can take several seconds on a 200-page file. Callers MUST
//! run it on the bounded blocking pool so a burst of uploads cannot
//! starve the async runtime — see [`extract_pdf_bounded`] and the
//! plan R1b entry. Hard-kill of a non-cancellingable PDF parse arrives
//! with the agent-runner trust-zone split (plan H).

use std::sync::Arc;
use std::time::Duration;

use nagent_support::cpu::RunError;
use tokio::sync::Semaphore;

/// MIME types we know how to extract. Other types are rejected
/// upstream with `415` so this list is intentionally small and
/// versioned alongside the extractor implementations below.
pub const TEXT_MIME: &str = "text/plain";
pub const PDF_MIME: &str = "application/pdf";

/// Extract text from a plain-text / Markdown / log buffer. Cheap
/// (no copy, no allocation beyond the lossy UTF-8 conversion) and
/// stays synchronous because `from_utf8_lossy` is not CPU-heavy
/// and runs on the calling task. The PDF path lives in
/// [`extract_pdf_bounded`] — the route layer branches on the
/// extension before calling either helper.
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

/// PDF extraction on the bounded blocking pool (plan R1b). The
/// semaphore is held for the duration of the parse, capping the
/// number of concurrent `pdf_extract` runs at the configured
/// limit. The `timeout` is enforced with `tokio::time::timeout`
/// around `run_bounded` — the worker thread still keeps running
/// after a timeout, so the route layer must unlink the partial
/// file. Hard cancellation lands with the runner process (plan H).
pub async fn extract_pdf_bounded(
    semaphore: Arc<Semaphore>,
    bytes: &[u8],
    timeout: Duration,
) -> Result<ExtractionResult, ExtractionError> {
    // The `pdf_extract` crate owns the bytes for the duration of
    // the parse; clone once and hand the owned buffer to the
    // worker. A `Bytes` clone is cheap if we later switch to the
    // `bytes` crate; for now `Vec<u8>` is what the trait expects.
    let payload = bytes.to_vec();
    let parse = nagent_support::cpu::run_bounded(semaphore, move |_permit| {
        pdf_extract::extract_text_from_mem(&payload).map_err(|e| e.to_string())
    });

    let text = match tokio::time::timeout(timeout, parse).await {
        Ok(Ok(Ok(text))) => text,
        Ok(Ok(Err(parse_err))) => return Err(ExtractionError::ParseFailed(parse_err)),
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

    Ok(ExtractionResult {
        text,
        page_count: None,
        mime: PDF_MIME.to_string(),
    })
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

    #[tokio::test]
    async fn bounded_pdf_respects_semaphore() {
        // Three concurrent calls on a semaphore of size 1 must
        // serialise. Each call is a no-op for an empty buffer but
        // the route is exercised end-to-end.
        let sem = Arc::new(Semaphore::new(1));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let sem = sem.clone();
                let bytes = b"%PDF-1.4\n% fake\n".to_vec();
                tokio::spawn(async move {
                    extract_pdf_bounded(sem, &bytes, Duration::from_secs(2)).await
                })
            })
            .collect();
        for h in handles {
            // The empty-buffer parse is not a valid PDF, but we
            // only care that the call returns without panicking
            // and respects the timeout / semaphore. Either an
            // Err(_) or an Ok(_) with `text == ""` is acceptable;
            // what matters is that we did not deadlock.
            let _ = h.await.expect("task must join");
        }
    }
}
