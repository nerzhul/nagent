//! `read_document` agent — exposes uploaded documents to the LLM.
//!
//! ## Wire format
//!
//! The agent is registered as a standard [`crate::agents::Agent`]
//! with the following schema:
//!
//! ```json
//! {
//!   "name": "read_document",
//!   "parameters": {
//!     "type": "object",
//!     "properties": {
//!       "name":       { "type": "string" },
//!       "page_range": { "type": "string" }
//!     },
//!     "required": ["name"]
//!   }
//! }
//! ```
//!
//! The `name` argument is the document UUID returned by
//! `GET /v1/documents` (NOT the original filename — multiple uploads
//! can share a name and the LLM needs a stable handle). `page_range`
//! is optional and currently advisory; the v1 tool returns the
//! first `[documents].max_extracted_chars` characters of the
//! extracted text. The full file is stored on disk so a follow-up
//! plan can stream larger payloads / page ranges without changing
//! the schema.
//!
//! ## Session scoping
//!
//! Documents are scoped to the chat session that uploaded them
//! (via `UserContext::chat_session_id()`). Direct
//! `/v1/agents/read_document/invoke` callers without a session id
//! hit a `400 Bad Request` at the route layer; the agent itself
//! refuses to run on a missing session id with a clear error so a
//! future code path that forgets the route guard still fails
//! loudly.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, UserContext};
use crate::documents::DocumentStore;

/// `read_document` agent. Stateless — holds a cheap `DocumentStore`
/// clone so each `invoke` can run a single SELECT + read, plus
/// the server-bound chat session binding so the SEV 2 fix
/// (`X-Chat-Session-Id` validated against `(user, session)`) runs
/// before any DB lookup.
#[derive(Clone)]
pub struct ReadDocumentAgent {
    store: DocumentStore,
    chat_sessions: Option<crate::chat::sessions::ChatSessions>,
}

impl std::fmt::Debug for ReadDocumentAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadDocumentAgent")
            .field("store", &"<DocumentStore>")
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessions>"),
            )
            .finish()
    }
}

impl ReadDocumentAgent {
    pub fn new(
        store: DocumentStore,
        chat_sessions: Option<crate::chat::sessions::ChatSessions>,
    ) -> Self {
        Self {
            store,
            chat_sessions,
        }
    }
}

#[async_trait]
impl Agent for ReadDocumentAgent {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read the text content of a document previously uploaded by the user to this chat session. \
         Use the `name` field returned by GET /v1/documents (a UUID). For PDFs, optionally restrict \
         to a `page_range` (e.g. \"3-7\") to limit context size. The tool returns at most \
         max_extracted_chars characters; if the document is truncated, ask the user for the \
         specific section you need."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Document id (UUID) returned by GET /v1/documents."
                },
                "page_range": {
                    "type": "string",
                    "description": "Optional, format 'N' or 'N-M'. Currently advisory; v1 returns the full extracted text up to max_extracted_chars."
                }
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        // 1. Parse + validate arguments.
        let req = parse_args(&args)?;
        // 2. Resolve the chat session id. Direct invocation (curl)
        //    carries no session; refuse loudly so the LLM can
        //    recover rather than getting an empty tool result.
        let session_id = ctx.chat_session_id().ok_or_else(|| {
            AgentError::InvalidArguments(
                "read_document requires an active chat session id (X-Chat-Session-Id header)"
                    .to_string(),
            )
        })?;
        // SEV 2 fix: verify the (user, session) binding minted by
        // POST /v1/chat/session before doing any DB lookup. A
        // session id that was minted by another user, or never
        // minted at all, surfaces a clear "forbidden" error so
        // the LLM can recover (and so the audit log records the
        // attempt). `touch_and_verify` ALSO refreshes
        // `last_seen_at` so the next periodic sweep sees a recent
        // binding.
        if let Some(cs) = self.chat_sessions.as_ref() {
            cs.touch_and_verify(session_id, ctx.user_id())
                .await
                .map_err(|_| {
                    AgentError::AgentFailed(
                        "chat session is not bound to the current user; \
                         ask the user to refresh the page"
                            .to_string(),
                    )
                })?;
        }
        // 3. Fetch the document row, scoped to the user AND the
        //    session. SEV 2 fix: a user cannot reach another
        //    user's docs even if they guess the UUID.
        let row = self
            .store
            .db()
            .documents
            .get_by_name(&req.name, ctx.user_id(), session_id)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("document lookup failed: {e}")))?;
        let row = row.ok_or_else(|| {
            AgentError::InvalidArguments(format!(
                "unknown document `{}` in this chat session",
                req.name
            ))
        })?;
        // 4. Read the file from disk. The DB row's `disk_path` is
        //    operator-supplied / attacker-controlled if auth is
        //    bypassed; `safe_disk_read` re-canonicalises the path
        //    and refuses anything outside the cache dir. See
        //    `storage::safe_disk_read` for the SEV 1 fix.
        let cache_dir = self.store.cache_dir();
        let bytes = match super::storage::safe_disk_read(&row.disk_path, cache_dir) {
            Ok(b) => b,
            Err(super::storage::DiskReadError::EscapesCacheDir(_)) => {
                // DB row pointed outside the cache dir. Treat as
                // "the file is gone" from the agent's perspective
                // — surfacing the real reason would leak the
                // diagnostic to a probing caller. Log at warn!
                // for the audit trail.
                tracing::warn!(
                    document_id = %row.id,
                    path = %row.disk_path.display(),
                    "read_document: refusing to read disk_path outside cache_dir",
                );
                return Err(AgentError::AgentFailed(
                    "document file no longer available on disk; ask the user to re-upload".into(),
                ));
            }
            Err(super::storage::DiskReadError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(AgentError::AgentFailed(
                    "document file no longer available on disk; ask the user to re-upload".into(),
                ));
            }
            Err(e) => {
                return Err(AgentError::AgentFailed(format!(
                    "could not read document: {e}"
                )));
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                return Err(AgentError::AgentFailed(
                    "document is not valid UTF-8 (binary uploads are not supported)".into(),
                ));
            }
        };
        // 5. Truncate to the configured cap and emit the result.
        //    The full file is preserved on disk so a follow-up
        //    request can target a specific page range.
        let max_chars = self.store.max_extracted_chars();
        let truncated = text.chars().count() > max_chars;
        let (kept, marker) = if truncated {
            let kept: String = text.chars().take(max_chars).collect();
            (kept, format!("\n[… truncated at {max_chars} characters …]"))
        } else {
            (text, String::new())
        };
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": format!(
                "{} ({} bytes, {} pages)",
                row.original_name,
                row.size_bytes,
                row.page_count
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
            "data": {
                "name": row.id.to_string(),
                "original_name": row.original_name,
                "mime": row.mime,
                "text": format!("{kept}{marker}"),
                "page_range": req.page_range,
                "truncated": truncated,
                "extracted_chars": row.extracted_chars,
            },
            "source": "local",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    name: String,
    #[allow(dead_code)]
    page_range: Option<String>,
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
    let page_range = obj
        .get("page_range")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref pr) = page_range {
        validate_page_range(pr)?;
    }
    Ok(ParsedArgs { name, page_range })
}

/// Cheap sanity check on `page_range` so a typo doesn't silently
/// return the full text. Accepts `"N"` or `"N-M"` (1-indexed). The
/// actual page indexing lives in the PDF extractor (future plan)
/// — v1 only flags malformed input.
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

// ---- DB row ----------------------------------------------------------------

// Plan 4.A: the `DocumentRow` type lives in `nagent_db::types`
// alongside the repository that decodes it. Re-export it here so
// the `documents::agent` module surface (and the legacy
// `documents::db` wrapper) keeps compiling while the rest of the
// modules are migrated.
pub use nagent_db::DocumentRow;
