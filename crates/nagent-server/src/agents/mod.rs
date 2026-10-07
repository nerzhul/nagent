//! Server-side chat agents (plan 4.C thin layer).
//!
//! The trait, the error type, the `UserContext`, the
//! `AgentRegistry`, and every agent implementation live in the
//! `nagent-agents` crate. This module is a thin server-side glue
//! layer:
//!
//! - Re-exports the trait + the registry so the rest of the server
//!   can keep `use crate::agents::{Agent, AgentRegistry}` without
//!   caring where they live.
//! - Builds the server-specific [`SecretSource`] impl that wraps
//!   [`crate::credentials::resolver::CredentialResolver`].
//! - Builds the server-specific [`DocumentSource`] impl that wraps
//!   [`crate::documents::DocumentStore`] + the chat-session
//!   binding, so the `read_document` agent can stay in
//!   `nagent-agents` without reaching into the DB.
//! - Provides the [`AgentRegistryFactory`] helper that the boot
//!   path calls to wire the registry given a `Config` +
//!   `DocumentStore` + `ChatSessions`. The static factory table
//!   lives on `nagent-agents::AgentRegistry::from_config`; this
//!   module wraps it with the server-only `read_document`
//!   registration.

pub mod routes;

use std::sync::Arc;

// Re-export every public item from the `nagent-agents` crate so the
// rest of the server code can keep referring to `crate::agents::Agent`,
// `crate::agents::UserContext`, etc.
pub use nagent_agents::{
    Agent, AgentError, AgentRegistry, AgentSummary, ConfirmationDecision, DocumentPayload,
    DocumentReadRequest, DocumentShape, DocumentSource, PageRange, SecretSource, UserContext,
};

/// Local newtype around [`AgentRegistry`] so we can implement
/// `FromRef<Arc<AppState>>` for it without tripping the orphan rule
/// (both the trait and the registry are now foreign types).
/// Handlers either destructure via the newtype's `Deref` impl or
/// use the blanket `impl From<AgentRegistryNewtype> for AgentRegistry`
/// in `state.rs`.
#[derive(Clone)]
pub struct AgentRegistryNewtype(pub AgentRegistry);

impl std::ops::Deref for AgentRegistryNewtype {
    type Target = AgentRegistry;
    fn deref(&self) -> &AgentRegistry {
        &self.0
    }
}

impl std::fmt::Debug for AgentRegistryNewtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AgentRegistryNewtype")
            .field(&self.0)
            .finish()
    }
}

use crate::config::AgentConfig;
use crate::credentials::key::CredentialsKey;
use crate::credentials::resolver::{CredentialError, CredentialResolver};
use crate::documents::DocumentStore;

/// Adapter that implements [`SecretSource`] on top of the server's
/// [`CredentialResolver`]. One per process; `Arc`-cloned into every
/// [`UserContext`].
pub struct ResolverSecretSource {
    resolver: Arc<CredentialResolver>,
}

impl std::fmt::Debug for ResolverSecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverSecretSource")
            .field("resolver", &"<CredentialResolver>")
            .finish()
    }
}

impl ResolverSecretSource {
    pub fn new(resolver: Arc<CredentialResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl SecretSource for ResolverSecretSource {
    async fn fetch(
        &self,
        user_id: uuid::Uuid,
        service: &str,
        field: &str,
    ) -> Result<Option<secrecy::SecretString>, AgentError> {
        // We deliberately do NOT use a per-request cache here —
        // `UserContext` has its own cache, and stacking two caches
        // doubles the memory footprint for no benefit. The
        // resolver's own plaintext buffer is wiped by
        // `SecretString::Drop`.
        match self.resolver.get_raw(user_id, service, field).await {
            Ok(opt) => Ok(opt),
            Err(CredentialError::Missing { service, field }) => {
                Err(AgentError::CredentialsMissing { service, field })
            }
            Err(CredentialError::DecryptFailed { service, field }) => {
                Err(AgentError::CredentialsDecryptFailed { service, field })
            }
            Err(CredentialError::Store(e)) => {
                Err(AgentError::AgentFailed(format!("credentials store: {e}")))
            }
        }
    }
}

/// Adapter that implements [`DocumentSource`] on top of the
/// server's [`DocumentStore`]. Constructed once per process and
/// cloned into the `ReadDocumentAgent` via `Arc`.
///
/// `credentials_key` is `Some` when the operator has configured
/// `[auth.credentials].key` (the same AES-256-GCM key that
/// protects the per-user vault and the long-term memory
/// subsystem). The upload route refuses to mount PDFs when this
/// is `None`; the read path falls back to the (unencrypted) legacy
/// on-the-fly extraction for pre-migration rows and surfaces an
/// `AgentFailed` for any other row.
pub struct StoreDocumentSource {
    store: DocumentStore,
    chat_sessions: Option<crate::chat::sessions::ChatSessions>,
    credentials_key: Option<Arc<CredentialsKey>>,
}

impl std::fmt::Debug for StoreDocumentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreDocumentSource")
            .field("store", &"<DocumentStore>")
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessions>"),
            )
            .field(
                "credentials_key",
                &self.credentials_key.as_ref().map(|_| "<CredentialsKey>"),
            )
            .finish()
    }
}

impl StoreDocumentSource {
    pub fn new(
        store: DocumentStore,
        chat_sessions: Option<crate::chat::sessions::ChatSessions>,
        credentials_key: Option<Arc<CredentialsKey>>,
    ) -> Self {
        Self {
            store,
            chat_sessions,
            credentials_key,
        }
    }
}

#[async_trait::async_trait]
impl DocumentSource for StoreDocumentSource {
    fn max_extracted_chars(&self) -> usize {
        self.store.max_extracted_chars()
    }

    fn max_pages_per_call(&self) -> u32 {
        self.store.max_pages_per_call()
    }

    fn max_page_chars_per_call(&self) -> usize {
        self.store.max_page_chars_per_call()
    }

    async fn read(
        &self,
        user_id: uuid::Uuid,
        chat_session_id: uuid::Uuid,
        name: &str,
    ) -> Result<DocumentPayload, AgentError> {
        // Default behaviour for callers that ignore the
        // `page_range` field: pull the full text back as
        // `DocumentShape::FullText`. The new
        // [`Self::read_with_request`] method is the entry point
        // used by the agent.
        self.read_with_request(
            user_id,
            chat_session_id,
            DocumentReadRequest {
                name: name.to_string(),
                page_range: None,
            },
        )
        .await
    }

    async fn read_with_request(
        &self,
        user_id: uuid::Uuid,
        chat_session_id: uuid::Uuid,
        request: DocumentReadRequest,
    ) -> Result<DocumentPayload, AgentError> {
        // SEV 2 fix: verify the (user, session) binding minted by
        // POST /v1/chat/session before any DB lookup. A session id
        // that was minted by another user, or never minted at all,
        // surfaces a clear "forbidden" error so the LLM can
        // recover (and so the audit log records the attempt).
        // `touch_and_verify` ALSO refreshes `last_seen_at` so the
        // next periodic sweep sees a recent binding.
        if let Some(cs) = self.chat_sessions.as_ref() {
            cs.touch_and_verify(chat_session_id, user_id)
                .await
                .map_err(|_| {
                    AgentError::AgentFailed(
                        "chat session is not bound to the current user; \
                         ask the user to refresh the page"
                            .to_string(),
                    )
                })?;
        }
        // Fetch the document row, scoped to the user AND the
        // session. SEV 2 fix: a user cannot reach another user's
        // docs even if they guess the UUID. `read_with_request`
        // is the UUID-only entry point; the LLM-facing flow
        // goes through [`Self::resolve_and_read`] so a bare
        // filename (the value shown in the chat UI attachment
        // card) is accepted too. Callers that already have a
        // UUID should keep using this method.
        let row = self
            .store
            .db()
            .admin()
            .documents
            .get_by_name(&request.name, user_id, chat_session_id)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("document lookup failed: {e}")))?
            .ok_or_else(|| {
                AgentError::InvalidArguments(format!(
                    "unknown document `{}` in this chat session",
                    request.name
                ))
            })?;

        // PDF rows with a populated `pages_dir` are served from
        // the encrypted per-page store. `pages_dir = NULL` (a
        // pre-migration row) falls through to the legacy
        // re-extraction path below.
        if row.mime == crate::documents::extract::PDF_MIME {
            if let Some(pages_dir) = row.pages_dir.as_ref() {
                return self
                    .read_pdf_pages(&row, pages_dir, request.page_range)
                    .await;
            }
        }

        // Legacy path: re-parse the original file from disk on
        // every read. Used for plain-text rows (no per-page
        // store — they're small enough to be returned whole) AND
        // for pre-migration PDF rows whose `pages_dir` is NULL.
        self.read_full_text(&row, request.page_range).await
    }

    /// LLM-facing entry point that accepts either the document
    /// UUID (existing path) OR the exact original filename
    /// (session-scoped lookup). Used by the `read_document`
    /// agent so the LLM can pass the filename shown in the chat
    /// UI attachment card without first having to discover the
    /// UUID.
    ///
    /// Resolution order:
    /// 1. Try to parse `name_or_filename` as a UUID; on success,
    ///    delegate to [`Self::read_with_request`] which runs the
    ///    existing scoped-by-id path.
    /// 2. Otherwise, look up the most recently uploaded row in
    ///    `(user, session, original_name = name_or_filename)`.
    ///    `find_by_original_name` returns `None` on no match;
    ///    we surface an `InvalidArguments` error that names both
    ///    valid inputs so the LLM can recover on its next call.
    /// 3. Never both: a UUID hit wins over a filename hit even
    ///    when a filename in the same session would also match —
    ///    the LLM likely intends the UUID in that case.
    async fn resolve_and_read(
        &self,
        user_id: uuid::Uuid,
        chat_session_id: uuid::Uuid,
        name_or_filename: &str,
        page_range: Option<PageRange>,
    ) -> Result<DocumentPayload, AgentError> {
        // Fast-path: UUID-shaped input goes through the
        // existing path verbatim. The inner `get_by_name` would
        // also do the UUID parse, but a short-circuit keeps the
        // filename fallback isolated to genuinely non-UUID
        // inputs (which is what the LLM passes when it has only
        // the chat UI attachment card in context).
        if uuid::Uuid::parse_str(name_or_filename).is_ok() {
            return self
                .read_with_request(
                    user_id,
                    chat_session_id,
                    DocumentReadRequest {
                        name: name_or_filename.to_string(),
                        page_range,
                    },
                )
                .await;
        }

        // Filename path: same (user, session) verification +
        // exact `original_name` lookup. We DO NOT do the session
        // touch_and_verify twice when both paths would land on
        // the same row, so the read_with_request call below is
        // avoided in favour of calling the inner reader helpers
        // directly.
        if let Some(cs) = self.chat_sessions.as_ref() {
            cs.touch_and_verify(chat_session_id, user_id)
                .await
                .map_err(|_| {
                    AgentError::AgentFailed(
                        "chat session is not bound to the current user; \
                         ask the user to refresh the page"
                            .to_string(),
                    )
                })?;
        }
        let row = self
            .store
            .db()
            .for_user(user_id)
            .documents()
            .find_by_original_name(chat_session_id, name_or_filename)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("document lookup failed: {e}")))?
            .ok_or_else(|| {
                AgentError::InvalidArguments(format!(
                    "no document with name `{name_or_filename}` in this chat session; \
                     pass either the document UUID (the `data.name` returned by \
                     GET /v1/documents) or the exact original filename shown in the \
                     chat UI attachment card"
                ))
            })?;
        // Once the row is resolved, the per-row read path is
        // identical to `read_with_request`'s, so we dispatch
        // through it with the canonical UUID. This keeps the
        // per-page + legacy branches in one place.
        self.read_with_request(
            user_id,
            chat_session_id,
            DocumentReadRequest {
                name: row.id.to_string(),
                page_range,
            },
        )
        .await
    }
}

impl StoreDocumentSource {
    /// Read the requested pages of a PDF row from the encrypted
    /// per-page store. Returns `DocumentShape::Overview` /
    /// `DocumentShape::Range` according to the request.
    async fn read_pdf_pages(
        &self,
        row: &nagent_db::DocumentRow,
        pages_dir: &std::path::Path,
        page_range: Option<PageRange>,
    ) -> Result<DocumentPayload, AgentError> {
        let key = match self.credentials_key.as_ref() {
            Some(k) => k,
            None => {
                // The upload route refuses PDFs when the key is
                // missing, so reaching this branch with `None`
                // means the row pre-dates the credentials gate
                // entirely. Surface a clear error so the LLM can
                // ask the operator to re-upload.
                return Err(AgentError::AgentFailed(
                    "this PDF was uploaded before the credentials vault was \
                     configured; ask the user to re-upload the document"
                        .into(),
                ));
            }
        };
        let key_ref: &CredentialsKey = key.as_ref();
        let dir_for_key = pages_dir.to_path_buf();
        let index = crate::documents::pages::read_overview(&dir_for_key, key_ref)
            .map_err(|e| map_pages_err(e, row))?;
        // The legacy row detection: `Missing` index means the
        // migration never wrote `meta.bin`. Fall back to
        // re-extracting on the fly — same shape as the pre-2026
        // behaviour.
        if matches!(index, crate::documents::pages::PagesIndex::Missing) {
            return self.read_pdf_legacy(row, page_range).await;
        }
        let page_count = match &index {
            crate::documents::pages::PagesIndex::Indexed { page_count, .. } => *page_count,
            crate::documents::pages::PagesIndex::Missing => 0,
        };
        let unreadable_pages = match &index {
            crate::documents::pages::PagesIndex::Indexed {
                unreadable_pages, ..
            } => *unreadable_pages,
            crate::documents::pages::PagesIndex::Missing => 0,
        };
        let payload = match page_range {
            None => {
                let preview = match &index {
                    crate::documents::pages::PagesIndex::Indexed { preview, .. } => preview.clone(),
                    crate::documents::pages::PagesIndex::Missing => String::new(),
                };
                DocumentPayload {
                    id: row.id,
                    original_name: row.original_name.clone(),
                    mime: row.mime.clone(),
                    size_bytes: row.size_bytes,
                    page_count: Some(page_count),
                    extracted_chars: row.extracted_chars,
                    unreadable_pages,
                    shape: DocumentShape::Overview(preview),
                }
            }
            Some(range) => {
                let text = crate::documents::pages::read_pages(
                    &dir_for_key,
                    key_ref,
                    range.start,
                    range.end_inclusive,
                )
                .map_err(|e| map_pages_err(e, row))?;
                let chars = text.chars().count() as u64;
                DocumentPayload {
                    id: row.id,
                    original_name: row.original_name.clone(),
                    mime: row.mime.clone(),
                    size_bytes: row.size_bytes,
                    page_count: Some(page_count),
                    extracted_chars: chars,
                    unreadable_pages,
                    shape: DocumentShape::Range(text),
                }
            }
        };
        Ok(payload)
    }

    /// Legacy PDF read path: re-parse the PDF bytes on every
    /// call. Used when the row has no `pages_dir` (pre-migration)
    /// or when the encryption key is missing (a row uploaded when
    /// the operator had not yet configured the vault).
    async fn read_pdf_legacy(
        &self,
        row: &nagent_db::DocumentRow,
        page_range: Option<PageRange>,
    ) -> Result<DocumentPayload, AgentError> {
        let cache_dir = self.store.cache_dir();
        let bytes = read_pdf_bytes(&row.disk_path, cache_dir)?;
        let extraction = self.extract_pdf(&bytes).await?;
        let page_count = extraction.page_count.or(row.page_count);
        let text = if let Some(range) = page_range {
            // Re-extract per page using the legacy pdf-extract
            // text. The legacy path has no `pages_dir` so we
            // cannot pick individual pages — best-effort: if the
            // user requested a range, slice the joined text by
            // approximate characters per page. A future plan will
            // replace this with a proper per-page re-extract;
            // today the slice is approximate but the LLM still
            // gets a smaller tool result.
            let total_chars = extraction.text.chars().count() as u64;
            let pc = page_count.unwrap_or(1).max(1) as u64;
            let chars_per_page = (total_chars / pc).max(1);
            let start = ((range.start as u64).saturating_sub(1)) * chars_per_page;
            let end = (range.end_inclusive as u64) * chars_per_page;
            let end = end.min(total_chars);
            let start = start.min(end);
            extraction
                .text
                .chars()
                .skip(start as usize)
                .take((end - start) as usize)
                .collect::<String>()
        } else {
            extraction.text
        };
        Ok(DocumentPayload::full_text(
            row.id,
            row.original_name.clone(),
            row.mime.clone(),
            row.size_bytes,
            page_count,
            row.extracted_chars,
            text,
        ))
    }

    /// Plain-text / Markdown / log read path: re-read the file
    /// and run the lossy UTF-8 passthrough. Plain-text rows have
    /// no per-page store so a `page_range` argument is ignored
    /// (the agent surfaces a hint via the response envelope).
    async fn read_full_text(
        &self,
        row: &nagent_db::DocumentRow,
        page_range: Option<PageRange>,
    ) -> Result<DocumentPayload, AgentError> {
        let _ = page_range; // ignored for non-PDF rows
        let cache_dir = self.store.cache_dir();
        let bytes = match crate::documents::storage::safe_disk_read(&row.disk_path, cache_dir) {
            Ok(b) => b,
            Err(crate::documents::storage::DiskReadError::EscapesCacheDir(_)) => {
                tracing::warn!(
                    document_id = %row.id,
                    path = %row.disk_path.display(),
                    "read_document: refusing to read disk_path outside cache_dir",
                );
                return Err(AgentError::AgentFailed(
                    "document file no longer available on disk; ask the user to re-upload".into(),
                ));
            }
            Err(crate::documents::storage::DiskReadError::Io(e))
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
        if row.mime == crate::documents::extract::PDF_MIME {
            // PDF row with no `pages_dir`: legacy fallback.
            let extraction = self.extract_pdf(&bytes).await?;
            return Ok(DocumentPayload::full_text(
                row.id,
                row.original_name.clone(),
                row.mime.clone(),
                row.size_bytes,
                extraction.page_count.or(row.page_count),
                extraction.text.chars().count() as u64,
                extraction.text,
            ));
        }
        let ext = row
            .disk_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("txt");
        let extraction = match crate::documents::extract::extract_text(&bytes, ext) {
            Ok(r) => r,
            Err(crate::documents::extract::ExtractionError::UnsupportedMime(m)) => {
                return Err(AgentError::AgentFailed(format!(
                    "unsupported document type: {m}"
                )));
            }
            Err(e) => {
                return Err(AgentError::AgentFailed(format!(
                    "could not read document text: {e}"
                )));
            }
        };
        Ok(DocumentPayload::full_text(
            row.id,
            row.original_name.clone(),
            row.mime.clone(),
            row.size_bytes,
            extraction.page_count.or(row.page_count),
            extraction.text.chars().count() as u64,
            extraction.text,
        ))
    }

    /// Synchronous PDF extraction through the bounded blocking
    /// pool. Mirrors the upload route's call so the legacy read
    /// path honours the same timeout / concurrency envelope.
    async fn extract_pdf(
        &self,
        bytes: &[u8],
    ) -> Result<crate::documents::extract::ExtractionResult, AgentError> {
        // The legacy path uses `pdf-extract` directly because
        // `lopdf` here would be a redundant dependency. The
        // function lives in `extract.rs` and returns the same
        // `ExtractionResult` shape (with `page_count = None`
        // since `pdf-extract` does not expose the count).
        let semaphore = self.store.pdf_semaphore();
        let timeout = self.store.pdf_extract_timeout();
        crate::documents::extract::extract_pdf_bounded(semaphore, bytes, timeout)
            .await
            .map_err(|err| match err {
                crate::documents::extract::ExtractionError::ParseFailed(m) => {
                    AgentError::AgentFailed(format!(
                        "could not parse PDF: {m}; ask the user to re-upload a non-scanned copy"
                    ))
                }
                crate::documents::extract::ExtractionError::Timeout(_) => AgentError::AgentFailed(
                    "PDF extraction exceeded the configured timeout; \
                     try a smaller page range or a text-only document"
                        .into(),
                ),
                crate::documents::extract::ExtractionError::Saturated { .. }
                | crate::documents::extract::ExtractionError::SaturatedTimeout(_) => {
                    AgentError::AgentFailed(
                        "PDF extractor is saturated; please retry in a moment".into(),
                    )
                }
                crate::documents::extract::ExtractionError::UnsupportedMime(m) => {
                    AgentError::AgentFailed(format!("document is not a supported PDF: {m}"))
                }
            })
    }
}

/// Read the raw PDF bytes for the legacy read path. Mirrors the
/// `read_full_text` safe-read logic but stays a free helper so
/// the legacy PDF branch can reuse it without rebuilding the
/// error mapping in two places.
fn read_pdf_bytes(
    disk_path: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<Vec<u8>, AgentError> {
    match crate::documents::storage::safe_disk_read(disk_path, cache_dir) {
        Ok(b) => Ok(b),
        Err(crate::documents::storage::DiskReadError::EscapesCacheDir(_)) => {
            tracing::warn!(
                path = %disk_path.display(),
                "read_document legacy PDF: refusing to read disk_path outside cache_dir",
            );
            Err(AgentError::AgentFailed(
                "document file no longer available on disk; ask the user to re-upload".into(),
            ))
        }
        Err(crate::documents::storage::DiskReadError::Io(e))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            Err(AgentError::AgentFailed(
                "document file no longer available on disk; ask the user to re-upload".into(),
            ))
        }
        Err(e) => Err(AgentError::AgentFailed(format!(
            "could not read document: {e}"
        ))),
    }
}

fn map_pages_err(
    e: crate::documents::pages::PagesError,
    row: &nagent_db::DocumentRow,
) -> AgentError {
    use crate::documents::pages::PagesError;
    match e {
        PagesError::ParseFailed(m) => AgentError::AgentFailed(format!(
            "could not parse stored PDF index: {m}; ask the user to re-upload"
        )),
        PagesError::DirectoryMissing(p) => {
            tracing::warn!(
                document_id = %row.id,
                path = %p.display(),
                "read_document: pages_dir missing on disk; falling back to legacy re-extract"
            );
            AgentError::AgentFailed(
                "document per-page index is missing on disk; ask the user to re-upload".into(),
            )
        }
        PagesError::DecryptFailed(m) => AgentError::AgentFailed(format!(
            "could not decrypt document pages: {m}; check [auth.credentials].key was not rotated"
        )),
        PagesError::Io { path, source } => {
            AgentError::AgentFailed(format!("io error on {}: {source}", path.display()))
        }
    }
}

/// Build the [`AgentRegistry`] from a server-side [`AgentConfig`] +
/// optional document store + chat-session binding.
///
/// When the documents subsystem is enabled AND the caller hands in
/// a non-`None` `DocumentStore`, the `read_document` agent is added
/// to the registry (gated by the `read-document-agent` cargo
/// feature). `credentials_key` mirrors `build_memory_source`'s
/// gating: when `None`, the `StoreDocumentSource` is still wired
/// but cannot decrypt the per-page store — the upload route
/// refuses PDFs to keep the protocol offline; the read path
/// surfaces a clear "ask the user to re-upload" message.
#[allow(clippy::too_many_arguments)]
pub fn build_registry(
    cfg: &AgentConfig,
    document_store: Option<DocumentStore>,
    chat_sessions: Option<crate::chat::sessions::ChatSessions>,
    credentials_key: Option<Arc<CredentialsKey>>,
) -> AgentRegistry {
    // Plan 4.C: the plain per-agent *Config structs moved to
    // `nagent-agents`; the server-side `AgentConfig` owns the
    // `from_env_with_toml` parser and feeds the sub-configs to
    // `AgentRegistry::from_config` after a straight field-by-field
    // conversion (the runtime structs are deliberately identical
    // in shape so the conversion is a plain `clone()`).
    let cfgs = nagent_agents::AgentConfigs {
        web_fetch: cfg.web_fetch.clone().into(),
        weather: cfg.weather.clone().into(),
        unit_convert: cfg.unit_convert.clone().into(),
        wikipedia: cfg.wikipedia.clone().into(),
        dictionary: cfg.dictionary.clone().into(),
        stock: cfg.stock.clone().into(),
        calculate: Default::default(),
        datetime: Default::default(),
        read_document: cfg.read_document.clone().into(),
        caldav: cfg.caldav.clone().into(),
        x_timeline: cfg.x_timeline.clone().into(),
        memory: cfg.memory.clone().into(),
        // Plan 1791317253718: the `search_tools` meta-agent has
        // its own per-feature knob (`default_top_k`); the v1
        // server-side config does not surface it — `default()` is
        // the conservative starting point. A future operator knob
        // can be threaded through `AgentConfig` the same way the
        // other per-agent configs are.
        tool_search: Default::default(),
    };
    // Plan 4.C (C): one shared `EgressPool` is built per process
    // and passed to every agent. Strict-class agents
    // (`web_fetch`) drain the strict client; public-class agents
    // (`weather`, `dictionary`, `stock`, `wikipedia`) drain the
    // public client. Connection reuse + warm DNS / TLS roots are
    // the gains; per-agent SSRF policy still applies.
    let pool = nagent_agents::egress::EgressPool::new();
    let mut registry = AgentRegistry::from_config(&cfgs, cfg.enabled, &pool);
    // The `read_document` agent is auto-registered when both
    // `[agents].enabled = true` and the documents subsystem
    // produced a `DocumentStore` at boot. The historical
    // per-agent `read_document_enabled` toggle was redundant —
    // the documents subsystem already had its own master
    // switch (`[documents].enabled`). The old TOML key is
    // still parsed for backward compat (see the comment on
    // `TomlAgentConfig::read_document_enabled`) but its value
    // is dropped on the floor by the merge layer.
    if let (true, Some(store)) = (cfg.enabled, document_store) {
        #[cfg(feature = "read-document-agent")]
        registry.push_agent_boxed(Box::new(nagent_agents::ReadDocumentAgent::new(Arc::new(
            StoreDocumentSource::new(store, chat_sessions, credentials_key),
        ))));
        // Without the `read-document-agent` cargo feature the
        // agent is not compiled in, so the request is a no-op.
        #[cfg(not(feature = "read-document-agent"))]
        let _ = (store, chat_sessions, credentials_key);
    }
    // Startup surface tool inventory. Logged once per registry build
    // so the operator can sanity-check what the registry is
    // shipping. The full list is no longer projected into
    // `tools=[]` on every round (plan 1791317253718 — the
    // round-level builder now ships only `search_tools` plus
    // the BM25 pre-selection plus the per-session discovered
    // set); the count + names are still useful for spotting
    // regressions like the CalDAV / `read_document` omissions of
    // earlier sessions, so the log line stays.
    let tool_names: Vec<String> = registry.iter().map(|a| a.name().to_string()).collect();
    if cfg.enabled {
        tracing::info!(
            count = tool_names.len(),
            tools = ?tool_names,
            "agents: tools registered (exposed to LLM via search_tools + BM25 pre-selection per round)"
        );
    } else {
        tracing::info!(
            "agents: tools disabled by config; proxy will send no tools=[] payload to the upstream LLM"
        );
    }
    registry
}

// ---- Server-side *Config → agents crate *Config adapters ------------------
//
// The server-side runtime structs are deliberately identical in shape
// to the agents crate's plain structs; the `From` impls below let
// `build_registry` convert the lot with a single `.into()` per field.
// When the shape diverges (e.g. a new knob the agents crate does
// not need), the `From` impl picks the safe default.

impl From<crate::config::WebFetchConfig> for nagent_agents::WebFetchAgentConfig {
    fn from(c: crate::config::WebFetchConfig) -> Self {
        Self {
            allow_public: c.allow_public,
            allowlist: c.allowlist,
            max_bytes: c.max_bytes,
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::WeatherConfig> for nagent_agents::WeatherAgentConfig {
    fn from(c: crate::config::WeatherConfig) -> Self {
        Self {
            api_key: c.api_key,
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::UnitConvertConfig> for nagent_agents::UnitConvertAgentConfig {
    fn from(c: crate::config::UnitConvertConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::WikipediaConfig> for nagent_agents::WikipediaAgentConfig {
    fn from(c: crate::config::WikipediaConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
            user_agent: c.user_agent,
        }
    }
}

impl From<crate::config::DictionaryConfig> for nagent_agents::DictionaryAgentConfig {
    fn from(c: crate::config::DictionaryConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::StockConfig> for nagent_agents::StockAgentConfig {
    fn from(c: crate::config::StockConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::ReadDocumentConfig> for nagent_agents::ReadDocumentAgentConfig {
    fn from(c: crate::config::ReadDocumentConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            max_extracted_chars: c.max_extracted_chars,
            max_pages_per_call: c.max_pages_per_call,
            max_page_chars_per_call: c.max_page_chars_per_call,
        }
    }
}

impl From<crate::config::agents::CalDavConfig> for nagent_agents::CalDavAgentConfig {
    fn from(c: crate::config::agents::CalDavConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            allowlist: c.allowlist,
            max_events: c.max_events,
            max_body_bytes: c.max_body_bytes,
        }
    }
}

impl From<crate::config::agents::XTimelineConfig> for nagent_agents::XTimelineAgentConfig {
    fn from(c: crate::config::agents::XTimelineConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            max_posts: c.max_posts,
            allowlist: c.allowlist,
            cache_ttl_secs: c.cache_ttl_secs,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::agents::MemoryConfig> for nagent_agents::config::MemoryAgentConfig {
    fn from(c: crate::config::agents::MemoryConfig) -> Self {
        // The runtime config is field-for-field identical to the
        // agents-crate struct; the only reason for the pair is
        // that the TOML / env parsing lives in `nagent-server`'s
        // `config::agents` module while the agents crate stays
        // TOML-free.
        Self {
            recalled_top_k: c.recalled_top_k,
        }
    }
}
