//! Discussion-mode document uploads + the `read_document` LLM tool.
//!
//! The `documents/` module owns the per-session document store:
//! multipart uploads land on disk under `[documents].cache_dir`,
//! rows in the `uploaded_documents` table link the original
//! filename + extracted text to a UUID, and the [`ReadDocumentAgent`]
//! re-reads the file when the LLM asks for it.
//!
//! The module is always compiled (so the env / TOML plumbing always
//! resolves the `[documents]` table), but the heavy deps
//! (`pdf-extract` + `mime_guess`) are gated behind the `documents`
//! cargo feature. When the feature is OFF the PDF code path is
//! replaced by a "PDF support not compiled in" error at the route
//! layer so a misconfigured binary fails fast rather than silently
//! dropping PDFs.
//!
//! ## Sub-modules
//!
//! - [`storage`] — disk layout (UUID + 2-char shard) + the
//! `DocumentStore` DB wrapper.
//! - [`extract`] — text extraction (`.txt` passthrough, `.pdf` via
//! `pdf-extract`). Gated on the `documents` feature.
//! - [`agent`] — [`ReadDocumentAgent`], registered in
//! [`crate::agents::AgentRegistry::from_config`] when both the
//! cargo feature AND `documents.enabled = true` are on.
//! - [`routes`] — `POST/GET/DELETE /v1/documents*` HTTP handlers
//! + multipart parsing.
//! - [`purge`] — `purge_older_than(Duration)` helper used by the
//! background sweep + the `nagent documents purge` CLI.
//!
//! ## Feature matrix
//!
//! | Cargo feature | Runtime `documents.enabled` | What you get |
//! |---|---|---|
//! | OFF            | (anything)      | `[documents]` parses but `/v1/documents*` returns 404; `read_document` not registered; PDF upload rejected with 501 |
//! | ON             | `false`         | Same as OFF |
//! | ON             | `true`          | Full feature set |
//!
//! Plan 4.A: `DocumentStore` now wraps [`nagent_db::Db`] directly
//! (no more `AuthStore` shim) and exposes a `.db()` accessor for the
//! shared DB handle so routes / CLI / agent can reach per-user
//! repositories without an intermediate facade.

pub mod agent;
pub mod db;
pub mod extract;
pub mod purge;
pub mod routes;
pub mod storage;

use std::sync::Arc;

use serde::Serialize;

/// Cheap to clone (the inner `Arc` wraps a `nagent_db::Db`).
#[derive(Clone)]
pub struct DocumentStore {
    inner: Arc<DocumentStoreInner>,
}

struct DocumentStoreInner {
    /// Shared [`nagent_db::Db`]. The `uploaded_documents` table
    /// lives in the auth DB so the CLI can sweep rows + files from a
    /// single connection without touching a second pool. Routes
    /// reach the per-user `documents` repository via
    /// [`DocumentStore::db`].
    db: nagent_db::Db,
    /// Maximum number of characters the agent returns per read.
    /// Pulled from `DocumentsConfig` at boot so a reload (out of
    /// scope today) would pick it up.
    max_extracted_chars: usize,
    /// Absolute path to the on-disk cache directory. Captured at
    /// boot from `[documents].cache_dir` so the agent / download
    /// handler can validate `disk_path` against it without
    /// reaching into `Config`. Always canonicalised on first use
    /// (see `storage::safe_disk_read`).
    cache_dir: std::path::PathBuf,
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("db", &"<nagent_db::Db>")
            .field("max_extracted_chars", &self.inner.max_extracted_chars)
            .field("cache_dir", &self.inner.cache_dir)
            .finish()
    }
}

impl DocumentStore {
    /// Build the shared documents state.
    pub fn new(
        db: nagent_db::Db,
        max_extracted_chars: usize,
        cache_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(DocumentStoreInner {
                db,
                max_extracted_chars,
                cache_dir,
            }),
        }
    }

    /// Borrow the shared [`nagent_db::Db`]. Routes / CLI / agent
    /// reach per-user repositories through this handle; the
    /// documents domain in particular exposes a scoped per-user
    /// view via [`nagent_db::documents::Documents::for_user`].
    pub fn db(&self) -> &nagent_db::Db {
        &self.inner.db
    }

    /// Maximum number of characters the agent returns per read.
    /// Resolved from `[documents].max_extracted_chars` at boot.
    pub fn max_extracted_chars(&self) -> usize {
        self.inner.max_extracted_chars
    }

    /// Absolute path to the on-disk cache directory. Used by the
    /// `read_document` agent and the download handler to validate
    /// `disk_path` against the cache root before any `open()` call.
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.inner.cache_dir
    }
}

/// Public summary row returned by `GET /v1/documents`. Keeps the
/// wire format stable across migrations so the browser can rely on
/// the field names forever.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentSummary {
    pub id: UuidLike,
    pub name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub page_count: Option<u32>,
    pub extracted_chars: u64,
    pub created_at: String,
}

/// Wrapper around a UUID-as-string so the JSON shape matches the
/// pattern `id: <uuid>` everywhere (and serde can pick up the
/// changes if we ever switch to native `Uuid`).
pub type UuidLike = uuid::Uuid;

pub use storage::{path_for_uuid, DiskLayout, DEFAULT_EXTENSION};

/// Re-export so the integration tests can wire the agent without
/// reaching into the `agent` submodule.
pub use agent::ReadDocumentAgent;
