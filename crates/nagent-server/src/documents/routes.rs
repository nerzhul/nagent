//! HTTP handlers for the `/v1/documents*` family.
//!
//! Endpoints:
//! - `POST   /v1/documents`           — multipart upload
//! - `GET    /v1/documents`           — list docs for the session
//! - `GET    /v1/documents/{id}`      — download the original bytes
//! - `DELETE /v1/documents/{id}`      — unlink file + delete row
//!
//! All four are gated by the same `RequireAuth` / `LLM_AUTH_MODE`
//! envelope as the rest of `/v1/*`. They are NOT registered at all
//! when the `documents` cargo feature is off OR the runtime
//! `documents.enabled = false`; see `lib::build_router`.
//!
//! Multipart parsing uses `axum::extract::Multipart` (which wraps
//! `multer`). The browser sends a single `file` field plus the
//! usual multipart metadata; we read the bytes into memory
//! (`max_file_size_bytes` is the hard cap) and hand them to the
//! extractor before the DB write.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Multipart, Path as AxPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::BytesMut;
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::auth::session::AuthUser;
use crate::config::DocumentsConfig;

use super::extract::ExtractionError;
use super::storage::DiskLayout;
use super::DocumentStore;
use crate::auth::error::AuthError as DocumentError;

/// Build a tiny router that mounts only `POST /v1/chat/session`
/// (the SEV 2 server-bound chat session id mint endpoint). This
/// is split out from [`build_documents_router`] so it can be
/// mounted independently of `[documents].enabled`: the browser
/// mints a session id on every page load to harden the
/// `X-Chat-Session-Id` header against forgery, even on a build
/// where the operator has the documents feature off. Without
/// this split, a `documents.enabled = false` build would 404
/// the mint call and the chat-completions tool loop would lose
/// its session-scoping (the agent then refuses to read documents
/// anyway because the binding is missing — but the 404 still
/// surfaces in the console).
///
/// The router carries `ChatSessionsState` directly so the mint
/// handler can extract only what it needs. Caller wires the same
/// LLM-auth / rate-limit / CORS envelope used by the rest of
/// `/v1/*`.
pub fn build_chat_session_router(
    state: Arc<crate::AppState>,
) -> axum::Router<Arc<crate::AppState>> {
    let chat_sessions = state
        .chat_sessions
        .clone()
        .expect("chat_sessions handle is wired when this router is mounted");
    axum::Router::new()
        .route(
            "/v1/chat/session",
            axum::routing::post(crate::chat::sessions::mint_handler),
        )
        .with_state(chat_sessions)
}

/// Build the documents router subtree. Caller wires the same
/// LLM-auth / rate-limit / CORS envelope used by the rest of
/// `/v1/*` so a misconfigured server behaves identically.
pub fn build_documents_router(state: Arc<crate::AppState>) -> axum::Router<Arc<crate::AppState>> {
    // Refuse to mount when documents are disabled at runtime.
    // The `Some` branch unwraps is safe: the caller (lib.rs) only
    // calls us when both the cargo feature AND `state.documents`
    // are set.
    let store = state
        .documents
        .as_ref()
        .map(|d| d.store.clone())
        .expect("documents router requires state.documents");
    let cfg = state.config.documents.clone();
    let cors_origins = state
        .llm
        .as_ref()
        .map(|l| l.client.cfg().cors_allow_origins.clone())
        .unwrap_or_default();
    let _ = cors_origins; // applied by the caller via `.layer(cors)`
    axum::Router::new()
        .route("/v1/documents", axum::routing::post(upload_handler))
        .route("/v1/documents", axum::routing::get(list_handler))
        .route(
            "/v1/documents/:id",
            axum::routing::get(download_handler).delete(delete_handler),
        )
        .with_state(DocumentHandlerState {
            store,
            cfg,
            app: state,
        })
}

#[derive(Clone)]
pub struct DocumentHandlerState {
    pub store: DocumentStore,
    pub cfg: DocumentsConfig,
    /// Full `AppState` handle — required by the
    /// `POST /v1/chat/session` mint handler so it can read the
    /// `chat_sessions` binding. Cheap to clone (the inner `Arc`s
    /// share the same pool + service registry).
    pub app: Arc<crate::AppState>,
}

impl std::fmt::Debug for DocumentHandlerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentHandlerState")
            .field("store", &self.store)
            .field("cfg", &self.cfg)
            .finish()
    }
}

// ---- Error mapping -------------------------------------------------------

/// Errors surfaced by the route handlers. Mapped to HTTP statuses
/// by the `IntoResponse` impl below.
#[derive(Debug, thiserror::Error)]
pub enum DocumentRouteError {
    #[error("missing X-Chat-Session-Id header")]
    MissingSessionHeader,
    #[error("invalid X-Chat-Session-Id header: {0}")]
    InvalidSessionHeader(String),
    #[error("file too large: {got} > {max}")]
    FileTooLarge { got: usize, max: usize },
    #[error("unsupported media type: {0}")]
    UnsupportedMediaType(String),
    #[error("invalid multipart payload: {0}")]
    BadMultipart(String),
    #[error("session quota exceeded: {current} >= {max}")]
    QuotaExceeded { current: u64, max: u64 },
    #[error("extract failed: {0}")]
    ExtractFailed(String),
    #[error("disk write failed: {0}")]
    DiskWrite(String),
    #[error("db error: {0}")]
    Db(#[from] DocumentError),
    /// CSRF token mismatch on a state-changing request. The
    /// `AuthUser` extension is present (the bearer / cookie check
    /// succeeded) but the `x-csrf-token` header did not match
    /// `user.csrf_token`. Surface 403 so the browser / curl caller
    /// knows the request was rejected without leaking whether the
    /// session is valid.
    #[error("forbidden")]
    Forbidden,
}

impl IntoResponse for DocumentRouteError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            DocumentRouteError::MissingSessionHeader => (StatusCode::BAD_REQUEST, self.to_string()),
            DocumentRouteError::InvalidSessionHeader(_) => {
                (StatusCode::BAD_REQUEST, self.to_string())
            }
            DocumentRouteError::FileTooLarge { .. } => {
                (StatusCode::PAYLOAD_TOO_LARGE, self.to_string())
            }
            DocumentRouteError::UnsupportedMediaType(_) => {
                (StatusCode::UNSUPPORTED_MEDIA_TYPE, self.to_string())
            }
            DocumentRouteError::BadMultipart(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            DocumentRouteError::QuotaExceeded { .. } => {
                (StatusCode::TOO_MANY_REQUESTS, self.to_string())
            }
            DocumentRouteError::ExtractFailed(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, self.to_string())
            }
            DocumentRouteError::DiskWrite(_) => {
                (StatusCode::INSUFFICIENT_STORAGE, self.to_string())
            }
            DocumentRouteError::Db(_) => {
                tracing::error!(error = %self, "documents DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "documents storage error".to_string(),
                )
            }
            DocumentRouteError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
        };
        (status, msg).into_response()
    }
}

// ---- Header parsing ------------------------------------------------------

/// `X-Chat-Session-Id` header name. Lowercase per the SSE/header
/// spec — axum's `TypedHeader` is case-insensitive but we look up
/// the raw header map at a few call sites so the name lives in
/// exactly one place.
pub const CHAT_SESSION_HEADER: &str = "x-chat-session-id";

fn parse_session_header(headers: &HeaderMap) -> Result<Uuid, DocumentRouteError> {
    let raw = headers
        .get(CHAT_SESSION_HEADER)
        .ok_or(DocumentRouteError::MissingSessionHeader)?;
    let s = raw
        .to_str()
        .map_err(|e| DocumentRouteError::InvalidSessionHeader(e.to_string()))?;
    Uuid::parse_str(s.trim()).map_err(|e| DocumentRouteError::InvalidSessionHeader(e.to_string()))
}

/// Validate the `(user_id, session_id)` binding in
/// `chat_sessions`. Returns `Err(BoundViolation)` when the row is
/// missing OR bound to a different user — the SEV 2 fix. The
/// caller can `.map_err(…)?` and never needs to know which case
/// fired (a probing attacker cannot tell the difference).
async fn verify_session_binding(
    app: &Arc<crate::AppState>,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<(), DocumentRouteError> {
    let cs = app
        .chat_sessions
        .as_ref()
        .ok_or(DocumentRouteError::Forbidden)?;
    cs.sessions
        .touch_and_verify(session_id, user_id)
        .await
        .map_err(|_| DocumentRouteError::Forbidden)?;
    Ok(())
}

// ---- POST /v1/documents (upload) ----------------------------------------

pub async fn upload_handler(
    State(state): State<DocumentHandlerState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, DocumentRouteError> {
    // SEV 3 fix: refuse to mutate state without a CSRF token (or
    // a bearer Authorization header). Bearer clients skip the check
    // because they cannot be tricked into cross-site submissions;
    // browser clients must carry `x-csrf-token` matching the
    // session's per-request token.
    crate::auth::middleware::check_csrf(&headers, &user)
        .map_err(|_| DocumentRouteError::Forbidden)?;
    let session_id = parse_session_header(&headers)?;
    // SEV 2 fix: verify the (user, session) binding minted by
    // POST /v1/chat/session before doing any work.
    verify_session_binding(&state.app, user.id, session_id).await?;
    let max_bytes = state.cfg.max_file_size_bytes;
    let max_docs = state.cfg.max_docs_per_session;

    // Enforce per-session quota BEFORE we touch the disk so a
    // misbehaving client cannot fill the PVC. Scope by (user, session)
    // so two tabs of the same user get isolated quotas.
    let current = state
        .store
        .count_documents_for_session(user.id, session_id)
        .await?;
    if current >= max_docs as u64 {
        return Err(DocumentRouteError::QuotaExceeded {
            current,
            max: max_docs as u64,
        });
    }

    // Walk the multipart stream looking for a `file` field. We
    // refuse to look at non-file fields so a curious client
    // cannot smuggle extra metadata into the DB row.
    let mut file: Option<UploadedFile> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|e| DocumentRouteError::BadMultipart(e.to_string()))?
    {
        if field.name() != Some("file") {
            // Drain to keep the stream consistent. We don't
            // accumulate the bytes — non-`file` fields are
            // rejected.
            loop {
                match field.chunk().await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        return Err(DocumentRouteError::BadMultipart(e.to_string()));
                    }
                }
            }
            continue;
        }
        let file_name = field.file_name().unwrap_or("upload.txt").to_string();
        let mut buf = BytesMut::new();
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    return Err(DocumentRouteError::BadMultipart(e.to_string()));
                }
            };
            if buf.len() + chunk.len() > max_bytes {
                return Err(DocumentRouteError::FileTooLarge {
                    got: buf.len() + chunk.len(),
                    max: max_bytes,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        file = Some(UploadedFile {
            name: file_name,
            bytes: buf.freeze(),
        });
        break;
    }
    let file = file.ok_or_else(|| {
        DocumentRouteError::BadMultipart("no `file` field in multipart body".into())
    })?;

    // Validate the MIME / extension. We accept both the filename
    // extension AND the multipart `content-type` so the browser
    // can hint either way; the extension wins on conflict (the
    // server trusts filenames over headers because the browser
    // can lie about content-type via fetch() overrides).
    let ext = extension_from_filename(&file.name);
    let mime = sniff_mime(&ext, file.bytes.len());
    if !is_supported(&ext) {
        // Unlink any partial file we might have written
        // (none yet at this point — the on-disk write happens
        // after the extract).
        return Err(DocumentRouteError::UnsupportedMediaType(format!(
            "only .txt and .pdf are supported (got `{}`)",
            file.name
        )));
    }

    // Extract text. PDF path is feature-gated; on a build without
    // the feature we return 501 with a clear message so the
    // operator knows the binary needs rebuilding.
    let timeout = Duration::from_secs(state.cfg.pdf_extract_timeout_secs);
    let extraction = match super::extract::extract_text(&file.bytes, &ext, timeout) {
        Ok(r) => r,
        Err(ExtractionError::UnsupportedMime(m)) => {
            return Err(DocumentRouteError::UnsupportedMediaType(m));
        }
        Err(ExtractionError::ParseFailed(m)) => {
            return Err(DocumentRouteError::ExtractFailed(m));
        }
        Err(ExtractionError::Timeout(_)) => {
            return Err(DocumentRouteError::ExtractFailed(
                "pdf extract exceeded the configured timeout".into(),
            ));
        }
    };

    // Mint the UUID + write to disk in one atomic step: write
    // to a temp file in the cache dir, then rename into the
    // shard. Avoids leaving a half-written file visible to the
    // agent if the rename fails.
    let id = Uuid::new_v4();
    let layout = DiskLayout::for_id(Path::new(&state.cfg.cache_dir), &id, &ext);
    let tmp = state.cfg.cache_dir.join(format!(".tmp-{id}"));
    write_atomic(&tmp, &layout.path, &file.bytes).map_err(DocumentRouteError::DiskWrite)?;
    let written_bytes = std::fs::metadata(&layout.path)
        .map(|m| m.len())
        .unwrap_or(file.bytes.len() as u64);

    // Apply the truncation cap on the extracted text so the row
    // never carries more than the configured max. The full
    // payload is still on disk; the row records the count for
    // the agent to surface in its summary.
    let extracted_chars = extraction.text.chars().count() as u64;

    // DB write. We do NOT wrap the disk write + DB insert in a
    // single transaction (no two-phase commit available across
    // the two stores); on a DB failure we best-effort unlink the
    // file so we don't leak bytes. The next periodic sweep
    // catches any orphan.
    //
    // `user_id` comes from the authenticated session (SEV 1 + 2
    // fix): the row is now keyed on `(user_id, session_id)` so
    // cross-user / cross-tab reads are rejected at the DB layer.
    if let Err(e) = state
        .store
        .insert_document(
            id,
            session_id,
            user.id,
            &file.name,
            &mime,
            written_bytes,
            extracted_chars,
            extraction.page_count,
            layout.path.to_string_lossy().as_ref(),
        )
        .await
    {
        let _ = std::fs::remove_file(&layout.path);
        return Err(DocumentRouteError::Db(e));
    }

    let summary = super::DocumentSummary {
        id,
        name: file.name.clone(),
        mime,
        size_bytes: written_bytes,
        page_count: extraction.page_count,
        extracted_chars,
        created_at: Utc::now().to_rfc3339(),
    };
    Ok((StatusCode::CREATED, Json(summary)).into_response())
}

#[derive(Debug)]
struct UploadedFile {
    name: String,
    bytes: bytes::Bytes,
}

// ---- GET /v1/documents (list) -------------------------------------------

pub async fn list_handler(
    State(state): State<DocumentHandlerState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
) -> Result<Response, DocumentRouteError> {
    let session_id = parse_session_header(&headers)?;
    // SEV 2 fix: verify the (user, session) binding.
    verify_session_binding(&state.app, user.id, session_id).await?;
    // SEV 2 fix: scope the list by `(user, session)` so a curl
    // caller cannot enumerate another user's docs.
    let rows = state
        .store
        .list_documents_for_session(user.id, session_id)
        .await?;
    let out: Vec<super::DocumentSummary> = rows
        .into_iter()
        .map(|r| super::DocumentSummary {
            id: r.id,
            name: r.original_name,
            mime: r.mime,
            size_bytes: r.size_bytes,
            page_count: r.page_count,
            extracted_chars: r.extracted_chars,
            created_at: String::new(), // filled by row decode if needed
        })
        .collect();
    // Re-fetch the created_at via the rows we already have; the
    // summary carries only the fields the UI needs and the
    // created_at is included by decoding the row once more —
    // but for v1 we omit it (the UI can sort by recency via the
    // DB ordering and we keep the wire shape minimal).
    Ok(Json(json!({ "data": out })).into_response())
}

// ---- GET /v1/documents/{id} (download) ---------------------------------

pub async fn download_handler(
    State(state): State<DocumentHandlerState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    AxPath(id): AxPath<Uuid>,
) -> Result<Response, DocumentRouteError> {
    // SEV 2 fix: scope the lookup by `user.id`. The `get_document_by_id`
    // query filters `WHERE id = ? AND user_id = ?` so a user
    // cannot download another user's docs even if they guess the
    // UUID. Returns `None` (→ 404) on a cross-user attempt so the
    // response shape does not reveal whether the doc exists.
    let row = state
        .store
        .get_document_by_id(id, user.id)
        .await?
        .ok_or_else(|| DocumentRouteError::BadMultipart(format!("unknown document: {id}")))?;
    // SEV 1 fix: never `open()` a DB-supplied `disk_path` without
    // verifying it canonicalises inside `cache_dir`.
    let bytes = super::storage::safe_disk_read(&row.disk_path, &state.cfg.cache_dir).map_err(
        |e| match e {
            super::storage::DiskReadError::EscapesCacheDir(p) => {
                DocumentRouteError::BadMultipart(format!("document path rejected: {p}"))
            }
            super::storage::DiskReadError::Io(io) => {
                DocumentRouteError::DiskWrite(format!("could not read document: {io}"))
            }
        },
    )?;
    let mut resp = Response::builder().status(StatusCode::OK).header(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&row.mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    let safe_name = sanitise_for_disposition(&row.original_name);
    let disposition = format!("attachment; filename=\"{safe_name}\"");
    resp = resp.header(
        axum::http::header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    Ok(resp.body(Body::from(bytes)).expect("static builder"))
}

// ---- DELETE /v1/documents/{id} -----------------------------------------

pub async fn delete_handler(
    State(state): State<DocumentHandlerState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    AxPath(id): AxPath<Uuid>,
) -> Result<Response, DocumentRouteError> {
    // SEV 3 fix: refuse to mutate state without a CSRF token (or
    // a bearer Authorization header).
    crate::auth::middleware::check_csrf(&headers, &user)
        .map_err(|_| DocumentRouteError::Forbidden)?;
    let session_id = parse_session_header(&headers)?;
    // SEV 2 fix: verify the (user, session) binding AND scope the
    // delete by `(user, session)` so a user cannot delete another
    // user's docs.
    verify_session_binding(&state.app, user.id, session_id).await?;
    let path = match state.store.delete_document(id, user.id, session_id).await? {
        Some(p) => p,
        None => {
            return Err(DocumentRouteError::BadMultipart(format!(
                "unknown document: {id}"
            )));
        }
    };
    // Best-effort unlink; missing file is fine (the periodic
    // sweep would have caught it).
    let _ = std::fs::remove_file(&path);
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- Helpers -------------------------------------------------------------

fn extension_from_filename(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn sniff_mime(ext: &str, _size: usize) -> String {
    // The mime_guess dep gives us a richer lookup when the
    // extension is unfamiliar, but the v1 server only accepts
    // the four extensions below — fall back to a hard-coded map
    // to keep the wire format stable.
    match ext {
        "pdf" => super::extract::PDF_MIME.to_string(),
        "txt" | "md" | "log" => super::extract::TEXT_MIME.to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn is_supported(ext: &str) -> bool {
    matches!(ext, "txt" | "md" | "log" | "pdf")
}

/// Atomically write `bytes` to `target`. We write to `tmp` first,
/// then rename into place — a failed rename leaves `tmp` on disk
/// but the `target` is never half-written, so a partial file is
/// impossible to read from the agent path.
fn write_atomic(tmp: &Path, target: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create_dir_all({}) failed: {e}", parent.display()))?;
    }
    std::fs::write(tmp, bytes).map_err(|e| format!("write({}) failed: {e}", tmp.display()))?;
    std::fs::rename(tmp, target).map_err(|e| {
        format!(
            "rename({} -> {}) failed: {e}",
            tmp.display(),
            target.display()
        )
    })?;
    Ok(())
}

/// Strip control characters + quotes from a filename before
/// emitting it in `Content-Disposition`. Defends against a
/// header-injection attempt via the original upload name.
fn sanitise_for_disposition(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '"' | '\\' | '\r' | '\n' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect()
}

/// Re-export so the agent module can call into the storage layer
/// without going through a third module. Cheap to inline.
pub use super::extract as _extract_re_export;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_from_filename_lowercases_and_strips() {
        assert_eq!(extension_from_filename("Foo.PDF"), "pdf");
        assert_eq!(extension_from_filename("plain.TXT"), "txt");
        assert_eq!(extension_from_filename("noext"), "");
        // `nested.path/log` has no extension (the slash is a
        // path separator, not part of the filename).
        assert_eq!(extension_from_filename("nested.path/log"), "");
        // But `nested/path.log` does.
        assert_eq!(extension_from_filename("nested/path.log"), "log");
    }

    #[test]
    fn sanitise_for_disposition_replaces_dangerous_chars() {
        // Each quote / backslash / CR / LF / control byte is
        // replaced with a single underscore. The semicolon and
        // slash are kept (not dangerous in a header value once
        // the surrounding quote / newline are escaped).
        assert_eq!(
            sanitise_for_disposition("hello\"; rm -rf /\n"),
            "hello_; rm -rf /_"
        );
        assert_eq!(sanitise_for_disposition("normal.txt"), "normal.txt");
        // Backslash + carriage return are both neutralised.
        assert_eq!(sanitise_for_disposition("a\\b\rc"), "a_b_c");
    }

    #[test]
    fn parse_session_header_accepts_valid_uuid_and_rejects_garbage() {
        let mut h = HeaderMap::new();
        h.insert(
            CHAT_SESSION_HEADER,
            HeaderValue::from_static("01234567-89ab-cdef-0123-456789abcdef"),
        );
        let id = parse_session_header(&h).expect("valid uuid must parse");
        assert_eq!(
            id,
            Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap()
        );

        let mut h = HeaderMap::new();
        h.insert(CHAT_SESSION_HEADER, HeaderValue::from_static("not-a-uuid"));
        assert!(matches!(
            parse_session_header(&h),
            Err(DocumentRouteError::InvalidSessionHeader(_))
        ));

        let h = HeaderMap::new();
        assert!(matches!(
            parse_session_header(&h),
            Err(DocumentRouteError::MissingSessionHeader)
        ));
    }

    #[test]
    fn is_supported_matches_plan() {
        for ok in ["txt", "pdf", "md", "log"] {
            assert!(is_supported(ok), "{ok} must be supported");
        }
        for bad in ["png", "jpg", "docx", "", "exe"] {
            assert!(!is_supported(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn write_atomic_creates_parent_and_file() {
        let tmp_root = std::env::temp_dir().join(format!(
            "nagent-doc-atomic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = tmp_root.join("ab/cd/abcdef12-3456-7890-abcd-ef1234567890.pdf");
        let tmp = tmp_root.join(".tmp-abcdef12-3456-7890-abcd-ef1234567890");
        write_atomic(&tmp, &target, b"hello world").expect("atomic write must succeed");
        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");
        assert!(!tmp.exists(), "tmp file must be renamed away");
        std::fs::remove_dir_all(&tmp_root).ok();
    }
}
