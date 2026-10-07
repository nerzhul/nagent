//! `read_document` agent.
//!
//! Lives in `nagent-agents` so adding a new agent is one directory,
//! one feature line, docs. Talks to the documents subsystem through
//! the [`DocumentSource`] trait — the agent itself has zero
//! dependency on `nagent_db` or the on-disk storage layout.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, DocumentShape, DocumentSource, PageRange, UserContext};

/// `read_document` agent. Stateless — holds an `Arc<dyn
/// DocumentSource>` so each `invoke` can run a single document
/// fetch + read.
#[derive(Clone)]
pub struct ReadDocumentAgent {
    source: std::sync::Arc<dyn DocumentSource>,
}

impl std::fmt::Debug for ReadDocumentAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadDocumentAgent")
            .field("source", &"<dyn DocumentSource>")
            .finish()
    }
}

impl ReadDocumentAgent {
    /// Build from a [`DocumentSource`] implementation. The server
    /// hands in a wrapper around [`crate::documents::DocumentStore`]
    /// + [`crate::chat::sessions::ChatSessions`] so the agent sees
    /// only the narrow capability surface it needs.
    pub fn new(source: std::sync::Arc<dyn DocumentSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl Agent for ReadDocumentAgent {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read the text content of a file or document (PDF, TXT, MD, LOG) previously uploaded \
         by the user to this chat session. The `name` field accepts EITHER the document UUID \
         (the value of `data.name` in GET /v1/documents) OR the original filename shown in \
         the chat UI attachment card — exact filename match, scoped to this chat session; \
         ties pick the most recent. The default call returns an OVERVIEW (page count, size, \
         short preview); pass `page_range` (e.g. \"3-7\") to fetch only specific pages. Each \
         range-mode call is capped at max_pages_per_call pages or max_page_chars_per_call \
         characters. Make further calls with different `page_range` arguments to read \
         additional sections."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Document identifier: EITHER the document UUID (the value of `data.name` in GET /v1/documents) OR the exact original filename shown in the chat UI attachment card (e.g. \"report.pdf\"). Filename match is scoped to the current chat session; on duplicate names, the most recently uploaded document wins."
                },
                "page_range": {
                    "type": "string",
                    "description": "Optional, format 'N' or 'N-M' (1-indexed). When set, restricts the response to that page range; the response is capped at max_pages_per_call pages and max_page_chars_per_call characters. Make additional calls with further ranges to read the rest of the document."
                }
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    /// Documents are operator-controlled, not untrusted remote content.
    /// The LLM does not need the indirect-prompt-injection fence.
    fn untrusted_output(&self) -> bool {
        false
    }

    /// BM25 synonyms for the router's pre-selection step. The
    /// description alone scores the LLM query "read a document or
    /// file" weakly because it mentions "read" + "document" but
    /// not "file" / "PDF" / "txt" / etc. — the LLM usually
    /// reaches for `read_document` directly (via the system
    /// prompt's mention in `llm/prompt.rs`), but when the proxy
    /// pre-selects from this agent first the keywords give the
    /// BM25 walk the synonyms it needs to land on top of the
    /// unrelated `x_timeline` / `caldav_*` candidates.
    fn keywords(&self) -> &'static [&'static str] {
        &[
            "file",
            "files",
            "pdf",
            "txt",
            "md",
            "log",
            "markdown",
            "fiche",
            "bulletin",
            "paie",
            "payslip",
            "statement",
            "invoice",
            "facture",
            "notice",
        ]
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let session_id = ctx.chat_session_id().ok_or_else(|| {
            AgentError::InvalidArguments(
                "read_document requires an active chat session id (X-Chat-Session-Id header)"
                    .to_string(),
            )
        })?;

        // No `page_range` → overview mode. Range mode otherwise.
        let page_range = match req.page_range_raw.as_deref() {
            None => None,
            Some(raw) => Some(parse_page_range(raw, self.source.max_pages_per_call())?),
        };
        let payload = self
            .source
            .resolve_and_read(ctx.user_id(), session_id, &req.name, page_range)
            .await?;

        // Branch on the underlying shape to assemble the LLM-facing
        // envelope. The wire format keeps `summary` stable
        // (`"<name> (N bytes, M pages)"`) so the existing SSE
        // client-side parser keeps working; the extra fields
        // (`data.page_range_applied`, `data.hint`) are additive.
        let page_count_str = payload
            .page_count
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".into());
        let summary = match (&payload.shape, page_range) {
            // Range mode (PDF, requested page_range): per-page
            // joined text + applied range in the summary suffix.
            (DocumentShape::Range(_), Some(range)) => format!(
                "{} ({} bytes, {} pages, showing {}-{})",
                payload.original_name,
                payload.size_bytes,
                page_count_str,
                range.start,
                range.end_inclusive,
            ),
            _ => format!(
                "{} ({} bytes, {} pages)",
                payload.original_name, payload.size_bytes, page_count_str,
            ),
        };
        let mut envelope = json!({
            "ok": true,
            "summary": summary,
            "data": {
                "name": payload.id.to_string(),
                "original_name": payload.original_name,
                "mime": payload.mime,
                "page_count": payload.page_count,
                "extracted_chars": payload.extracted_chars,
                // Plan 1791384190579 follow-up: surface the count
                // of pages whose text extraction failed at
                // upload time (e.g. unparseable ToUnicode CMap).
                // The LLM uses this to know when the preview
                // is empty because the document is entirely
                // unreadable, not because the document is short.
                "unreadable_pages": payload.unreadable_pages,
            },
            "source": "local",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        });
        let data = envelope
            .get_mut("data")
            .and_then(|v| v.as_object_mut())
            .expect("object");

        match (&payload.shape, page_range) {
            (DocumentShape::Range(text), Some(range)) => {
                let start = range.start;
                let end = range.end_inclusive;
                data.insert("text".into(), json!(text));
                data.insert("truncated".into(), json!(false));
                data.insert("page_range_applied".into(), json!(format!("{start}-{end}")));
                // When the requested range is entirely inside the
                // unreadable-pages set, the joined text is just
                // N copies of the marker. Surface a clearer hint
                // so the LLM does not waste rounds calling for
                // adjacent ranges that will all return the same
                // marker.
                let hint = if payload.unreadable_pages == payload.page_count.unwrap_or(0) {
                    json!(
                        "every page in this document failed text extraction; \
                           the per-page call will keep returning the unreadable-page \
                           marker regardless of page_range"
                    )
                } else {
                    json!("make additional read_document calls with different page_range arguments to read further sections")
                };
                data.insert("hint".into(), hint);
            }
            // Overview mode (PDF, no page_range): short preview,
            // no `text` field, hint pointing at range mode. When
            // every page failed to extract the preview is empty
            // and the LLM needs a clearer hint than the default
            // "call read_document again with page_range" (which
            // would just return the same unreadable-page marker
            // for every page).
            (DocumentShape::Overview(preview), None) => {
                data.insert("preview".into(), json!(preview));
                data.insert("truncated".into(), json!(false));
                let hint = if payload.unreadable_pages > 0
                    && payload.unreadable_pages == payload.page_count.unwrap_or(0)
                {
                    json!(format!(
                        "this document is entirely unreadable: every page ({n} pages) \
                         failed text extraction at upload time, likely because the PDF \
                         embeds a ToUnicode CMap or font lopdf cannot decode; \
                         the file itself is on disk, you can ask the user to open it \
                         in a PDF viewer or run it through an OCR tool",
                        n = payload.unreadable_pages
                    ))
                } else if payload.unreadable_pages > 0 {
                    json!(format!(
                        "this document has {n} unreadable pages (ToUnicode CMap or font lopdf \
                         cannot decode) out of {total}; call read_document again with \
                         page_range to fetch specific pages that did extract",
                        n = payload.unreadable_pages,
                        total = payload.page_count.unwrap_or(0)
                    ))
                } else {
                    json!("call read_document again with page_range to fetch specific pages")
                };
                data.insert("hint".into(), hint);
            }
            // Legacy `.txt`/`.md`/`.log` path: full text under
            // `data.text`, truncation marker if the cap fires.
            // The default `read_with_request` impl hands the full
            // payload back as `FullText` so non-PDF rows fall
            // through this branch unchanged.
            (DocumentShape::FullText(text), None) => {
                let max_chars = self.source.max_extracted_chars();
                let truncated = text.chars().count() > max_chars;
                let (kept, marker) = if truncated {
                    let kept: String = text.chars().take(max_chars).collect();
                    (kept, format!("\n[… truncated at {max_chars} characters …]"))
                } else {
                    (text.clone(), String::new())
                };
                data.insert("text".into(), json!(format!("{kept}{marker}")));
                data.insert("truncated".into(), json!(truncated));
            }
            // A non-PDF row that still requested a `page_range`
            // (the LLM was sloppy). Fall through to the full-text
            // branch and surface the ignored range as a `hint`.
            (DocumentShape::FullText(text), Some(range)) => {
                let max_chars = self.source.max_extracted_chars();
                let truncated = text.chars().count() > max_chars;
                let (kept, marker) = if truncated {
                    let kept: String = text.chars().take(max_chars).collect();
                    (kept, format!("\n[… truncated at {max_chars} characters …]"))
                } else {
                    (text.clone(), String::new())
                };
                let _ = range; // range ignored for non-PDF
                data.insert("text".into(), json!(format!("{kept}{marker}")));
                data.insert("truncated".into(), json!(truncated));
                data.insert(
                    "hint".into(),
                    json!("page_range is only honoured for PDFs — this document is plain text and was returned in full"),
                );
            }
            // Defensive: an Overview / Range payload reaching a
            // mismatched branch. Surface the text under `data.text`
            // so the LLM still gets something useful rather than
            // a hard 502.
            (other @ (DocumentShape::Overview(_) | DocumentShape::Range(_)), _) => {
                let text = match other {
                    DocumentShape::Overview(s) | DocumentShape::Range(s) => s,
                    _ => unreachable!(),
                };
                let max_chars = self.source.max_extracted_chars();
                let truncated = text.chars().count() > max_chars;
                let kept: String = text.chars().take(max_chars).collect();
                data.insert("text".into(), json!(kept));
                data.insert("truncated".into(), json!(truncated));
            }
        }
        Ok(serde_json::to_string(&envelope).expect("json encode"))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    name: String,
    page_range_raw: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`name` (string) is required".into()))?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`name` must not be empty".into(),
        ));
    }
    // `name` accepts EITHER a document UUID (the value returned by
    // `data.name` in GET /v1/documents) OR the original filename
    // shown in the chat UI attachment card. The actual dispatch
    // happens in `DocumentSource::resolve_and_read` (server-side
    // override in `StoreDocumentSource`); this parser only does
    // the shape check. We no longer reject bare filenames at the
    // agent boundary — doing so made the first call from the LLM
    // fail with an opaque validation error whenever the user
    // attached a file via the chat UI (the LLM has only the
    // filename in its context, not the UUID). Filenames are
    // matched exactly within `(user, session, original_name)`,
    // and a duplicate name resolves to the most recently
    // uploaded row.
    let page_range = obj
        .get("page_range")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref pr) = page_range {
        validate_page_range(pr)?;
    }
    Ok(ParsedArgs {
        name,
        page_range_raw: page_range,
    })
}

fn validate_page_range(pr: &str) -> Result<(), AgentError> {
    let parts: Vec<&str> = pr.split('-').collect();
    match parts.as_slice() {
        [n] => n.parse::<u32>().map(|_| ()).map_err(|_| {
            AgentError::InvalidArguments(format!(
                "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
            ))
        }),
        [lo, hi] => {
            let lo_v: u32 = lo.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            let hi_v: u32 = hi.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            if lo_v == 0 || hi_v == 0 || lo_v > hi_v {
                return Err(AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; lo must be >= 1 and <= hi"
                )));
            }
            Ok(())
        }
        _ => Err(AgentError::InvalidArguments(format!(
            "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
        ))),
    }
}

/// Parse a validated `N` / `N-M` string into a [`PageRange`],
/// clamping `end_inclusive` to `max_pages_per_call` pages after the
/// start so the LLM cannot request a range wider than the
/// configured cap. The cap is checked here so the agent fails fast
/// on a malformed request rather than silently dropping pages.
fn parse_page_range(raw: &str, max_pages: u32) -> Result<PageRange, AgentError> {
    let parts: Vec<&str> = raw.split('-').collect();
    let (start, end) = match parts.as_slice() {
        [n] => {
            let v: u32 = n.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{raw}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            (v, v)
        }
        [lo, hi] => {
            let lo_v: u32 = lo.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{raw}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            let hi_v: u32 = hi.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{raw}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            if lo_v == 0 || hi_v == 0 || lo_v > hi_v {
                return Err(AgentError::InvalidArguments(format!(
                    "invalid page_range `{raw}`; lo must be >= 1 and <= hi"
                )));
            }
            (lo_v, hi_v)
        }
        _ => {
            return Err(AgentError::InvalidArguments(format!(
                "invalid page_range `{raw}`; expected `N` or `N-M` (1-indexed)"
            )))
        }
    };
    let span = end - start + 1;
    if span > max_pages {
        return Err(AgentError::InvalidArguments(format!(
            "page_range `{raw}` covers {span} pages but max_pages_per_call is {max_pages}; \
             split the request into smaller ranges"
        )));
    }
    Ok(PageRange {
        start,
        end_inclusive: end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pin: `parse_args` accepts BOTH a document UUID and the
    /// exact original filename shown in the chat UI attachment
    /// card. The actual dispatch (UUID vs filename) happens in
    /// `DocumentSource::resolve_and_read`; the parser must not
    /// pre-reject non-UUID inputs — doing so would force the LLM
    /// to guess the UUID on its first call after the user
    /// attached a file, and the only identifier in its context
    /// is the filename.
    #[test]
    fn parse_args_accepts_uuid() {
        let id = uuid::Uuid::new_v4().to_string();
        let parsed = parse_args(&json!({ "name": &id })).expect("UUID must parse");
        assert_eq!(parsed.name, id);
        assert!(parsed.page_range_raw.is_none());
    }

    #[test]
    fn parse_args_accepts_filename() {
        let parsed = parse_args(&json!({ "name": "report.pdf" }))
            .expect("filename must parse (no longer a hard error)");
        assert_eq!(parsed.name, "report.pdf");
    }

    #[test]
    fn parse_args_accepts_filename_with_page_range() {
        let parsed = parse_args(&json!({
            "name": "five_steps_perform_2009.pdf",
            "page_range": "3-7"
        }))
        .expect("filename + page_range must parse");
        assert_eq!(parsed.name, "five_steps_perform_2009.pdf");
        assert_eq!(parsed.page_range_raw.as_deref(), Some("3-7"));
    }

    #[test]
    fn parse_args_trims_whitespace() {
        let id = uuid::Uuid::new_v4().to_string();
        let parsed =
            parse_args(&json!({ "name": format!("  {id}  ") })).expect("trimmed UUID must parse");
        assert_eq!(parsed.name, id);
    }

    #[test]
    fn parse_args_rejects_missing_name() {
        let err = parse_args(&json!({})).expect_err("missing name must error");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_empty_name() {
        let err = parse_args(&json!({ "name": "   " })).expect_err("empty must error");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_non_string_name() {
        let err = parse_args(&json!({ "name": 42 })).expect_err("non-string must error");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_bad_page_range() {
        let err = parse_args(&json!({
            "name": "report.pdf",
            "page_range": "not-a-range"
        }))
        .expect_err("bad page_range must error");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    /// Pin: the description + parameter doc still mention BOTH
    /// accepted inputs after the filename fallback landed.
    /// Future drafts that drop the filename mention would
    /// re-introduce the "agent rejects the LLM's first call"
    /// bug.
    #[test]
    fn description_and_schema_mention_filename_fallback() {
        let agent = ReadDocumentAgent::new(std::sync::Arc::new(NoopSource));
        let desc = agent.description();
        assert!(
            desc.contains("filename"),
            "description must mention the filename fallback; got: {desc}"
        );
        let schema = agent.parameters_schema();
        let name_desc = schema["properties"]["name"]["description"]
            .as_str()
            .expect("name.description must be a string");
        assert!(
            name_desc.contains("filename"),
            "name.description must mention the filename fallback; got: {name_desc}"
        );
    }

    /// Stub `DocumentSource` used only by the description/schema
    /// pin above. None of the agent methods that need a working
    /// source are exercised in the unit tests (those tests go
    /// through the live `StoreDocumentSource` integration suite).
    struct NoopSource;
    #[async_trait::async_trait]
    impl DocumentSource for NoopSource {
        async fn read(
            &self,
            _: uuid::Uuid,
            _: uuid::Uuid,
            _: &str,
        ) -> Result<crate::agents::DocumentPayload, AgentError> {
            unimplemented!()
        }
        fn max_extracted_chars(&self) -> usize {
            0
        }
        fn max_pages_per_call(&self) -> u32 {
            0
        }
        fn max_page_chars_per_call(&self) -> usize {
            0
        }
    }
}
