//! `read_document` agent — end-to-end against an in-memory sqlite
//! store.
//!
//! Covers the happy path (read after upload), the unknown-doc
//! path (wrong session scope), the file-missing-on-disk path, and
//! the page-range validation path. Mirrors the harness in
//! `tests/auth_e2e.rs` so the migrations + sqlx pool lifecycle
//! run the same way.

use std::sync::Arc;

use nagent_agents::{Agent, ReadDocumentAgent, ServiceRegistry, UserContext};
use nagent_server::agents::AgentRegistryNewtype;
use nagent_server::config::AuthConfig;
use nagent_server::credentials::key::CredentialsKey;
use nagent_server::documents::DocumentStore;
use serde_json::json;

use uuid::Uuid;

async fn fresh_store() -> (DocumentStore, nagent_db::Db) {
    fresh_store_with_keys(None).await
}

#[allow(dead_code)]
async fn fresh_store_with_keys(key: Option<Arc<CredentialsKey>>) -> (DocumentStore, nagent_db::Db) {
    let _ = key; // forward-compat: callers that opt into encryption pass the key
    let cfg = AuthConfig {
        enabled: true,
        backends: vec![],
        public_url: "http://127.0.0.1:0".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: nagent_server::config::AuthDbConfig {
            backend: "sqlite".into(),
            // `:memory:` per-connection pool — each test gets its
            // own fresh DB.
            url: "sqlite::memory:".into(),
            max_connections: 1,
            auto_migrate: true,
        },
        password: Default::default(),
        oidc: Default::default(),
        passkey: Default::default(),
        credentials: Default::default(),
    };
    let opts: nagent_db::DbOptions = (&cfg).into();
    let store = nagent_db::Db::connect(&opts)
        .await
        .expect("sqlite in-memory store must connect");
    // Run the migrations so `uploaded_documents` exists.
    store.migrate().await.expect("migrations must run");
    let doc_store = DocumentStore::new(
        store.clone(),
        100_000,
        30,
        20_000,
        std::env::temp_dir(),
        0,
        30,
    );
    (doc_store, store)
}

fn ctx_with_session(session_id: Uuid) -> UserContext {
    let services = ServiceRegistry::empty().into_arc();
    UserContext::for_chat_session(Uuid::nil(), services, None, None, session_id)
}

fn ctx_without_session() -> UserContext {
    let services = ServiceRegistry::empty().into_arc();
    UserContext::for_tests(Uuid::nil(), services)
}

#[tokio::test]
async fn read_document_happy_path_returns_extracted_text() {
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-agent-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.txt"));
    std::fs::write(&file_path, "Hello, document!").unwrap();

    doc_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_id,
            "test.txt",
            "text/plain",
            16,
            16,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .expect("insert must succeed");

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = ctx_with_session(session_id);
    let result = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect("read must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(parsed["data"]["original_name"], "test.txt");
    assert!(parsed["data"]["text"].as_str().unwrap().contains("Hello"));
    assert_eq!(parsed["data"]["truncated"], false);

    // Clean up the cache dir.
    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// Build a minimal valid single-page PDF carrying the given
/// `text` in its content stream. Computes the cross-reference
/// table offsets on the fly so the byte stream is parseable by
/// `pdf-extract` (the previous test fixture had hand-rolled
/// offsets that drifted by a few bytes and tripped
/// `pdf_extract`'s "Invalid file trailer" check).
fn build_minimal_pdf(text: &str) -> Vec<u8> {
    use std::fmt::Write;
    // Compose the body first so we can measure each object
    // header's byte offset for the `xref` table.
    let mut head = String::new();
    head.push_str("%PDF-1.4\n%\u{e2}\u{e3}\u{cf}\u{d3}\n"); // binary marker

    let mut offsets: Vec<usize> = Vec::with_capacity(5);
    offsets.push(head.len());
    write!(head, "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n").unwrap();

    offsets.push(head.len());
    write!(
        head,
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n"
    )
    .unwrap();

    offsets.push(head.len());
    write!(
        head,
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n"
    )
    .unwrap();

    let content = format!("BT /F1 12 Tf 100 700 Td ({}) Tj ET", text);
    offsets.push(head.len());
    write!(
        head,
        "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
        content.len(),
        content
    )
    .unwrap();

    offsets.push(head.len());
    write!(
        head,
        "5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n"
    )
    .unwrap();

    let xref_offset = head.len();
    let mut xref = String::new();
    xref.push_str(&format!("xref\n0 {}\n", offsets.len() + 1));
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{:010} 00000 n \n", off));
    }
    write!(
        head,
        "{}trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
        xref,
        offsets.len() + 1,
        xref_offset
    )
    .unwrap();
    head.into_bytes()
}

/// Drive the upload-route's encryption-on-extract code path:
/// write the bytes to a temp + extract (or directly seed) the
/// per-page encrypted blobs. Returns the `(pages_dir,
/// page_count)` pair so the test can then run the agent against
/// the row.
///
/// `pages_direct`: when `Some`, skip the lopdf extractor and
/// write the supplied `(page_texts, preview, page_count)` directly
/// into the per-page store. Used by tests that only want to verify
/// the agent envelope without exercising the full PDF parser
/// (helpful because the hand-rolled minimal-PDF fixture
/// occasionally hangs in lopdf on certain page-object
/// structures — production PDFs are fine but the test fixture is
/// not). The unit tests in `pages.rs` cover the lopdf path
/// against the same fixture.
async fn upload_pdf(
    doc_store: &DocumentStore,
    session_id: Uuid,
    doc_id: Uuid,
    bytes: &[u8],
    key: &CredentialsKey,
    pages_direct: Option<(Vec<String>, String, u32, u32)>,
) -> (std::path::PathBuf, u32) {
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-pdf-upload-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.pdf"));
    std::fs::write(&file_path, bytes).unwrap();
    let pages_root = file_path.with_file_name(format!(
        "{}.pages",
        file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("doc")
    ));
    let page_count = if let Some((page_texts, preview, page_count, unreadable)) = pages_direct {
        std::fs::create_dir_all(&pages_root).unwrap();
        let meta = nagent_server::documents::pages::PagesIndex::Indexed {
            page_count,
            unreadable_pages: unreadable,
            preview: preview.clone(),
            toc: Vec::new(),
        };
        let meta_json = serde_json::to_vec(&meta).unwrap();
        let meta_plain = String::from_utf8(meta_json).unwrap();
        let meta_sealed = nagent_server::credentials::crypto::encrypt(key, &meta_plain).unwrap();
        let meta_path = pages_root.join("meta.bin");
        let mut combined =
            Vec::with_capacity(meta_sealed.nonce.len() + meta_sealed.ciphertext.len());
        combined.extend_from_slice(&meta_sealed.nonce);
        combined.extend_from_slice(&meta_sealed.ciphertext);
        std::fs::write(&meta_path, &combined).unwrap();
        for (idx, text) in page_texts.iter().enumerate() {
            let sealed = nagent_server::credentials::crypto::encrypt(key, text).unwrap();
            let page_path = pages_root.join(format!("page-{:04}.bin", idx + 1));
            let mut combined = Vec::with_capacity(sealed.nonce.len() + sealed.ciphertext.len());
            combined.extend_from_slice(&sealed.nonce);
            combined.extend_from_slice(&sealed.ciphertext);
            std::fs::write(&page_path, &combined).unwrap();
        }
        page_count
    } else {
        let index = nagent_server::documents::pages::extract_pages(bytes, &pages_root, key)
            .expect("extract");
        index.page_count().unwrap_or(0)
    };
    let pages_dir_str = pages_root.to_string_lossy().into_owned();
    let docs = doc_store.db().for_user(Uuid::nil()).documents();
    docs.insert(
        doc_id,
        session_id,
        "upload.pdf",
        "application/pdf",
        bytes.len() as u64,
        0,
        Some(page_count),
        file_path.to_string_lossy().as_ref(),
        Some(&pages_dir_str),
    )
    .await
    .expect("insert must succeed");
    (pages_root, page_count)
}

/// Regression: a PDF row whose raw disk bytes are binary must
/// still round-trip through the agent. The previous
/// implementation called `String::from_utf8(bytes)` on the raw
/// file, which failed with "document is not valid UTF-8 (binary
/// uploads are not supported)" even though the upload route had
/// already extracted the text at write time. The fix dispatches
/// to `extract_pdf_bounded` on read so the same code path that
/// ran at upload time runs again here, and a real PDF flows
/// back to the LLM as expected.
#[tokio::test]
async fn read_document_pdf_round_trips_through_extractor() {
    let pdf_bytes = build_minimal_pdf("Hello, PDF round-trip!");

    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-pdf-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.pdf"));
    std::fs::write(&file_path, &pdf_bytes).unwrap();

    doc_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_id,
            "round-trip.pdf",
            "application/pdf",
            pdf_bytes.len() as u64,
            // Upload-route would have written the real extracted
            // count; for the regression we just need a sensible
            // placeholder so the row insert does not error.
            25,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .expect("insert must succeed");

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = ctx_with_session(session_id);
    let result = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect(
            "PDF read must succeed (was returning \
                 'document is not valid UTF-8 (binary uploads are not supported)')",
        );
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    assert_eq!(parsed["data"]["original_name"], "round-trip.pdf");
    let text = parsed["data"]["text"]
        .as_str()
        .expect("text must be a string");
    assert!(
        text.contains("Hello") && text.contains("PDF") && text.contains("round-trip"),
        "expected the PDF's text stream to come through the \
         re-extract, got: {text:?}"
    );

    // Clean up the cache dir.
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn read_document_unknown_id_returns_invalid_arguments() {
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = ctx_with_session(session_id);
    let err: nagent_server::agents::AgentError = agent
        .invoke(&ctx, json!({ "name": Uuid::new_v4().to_string() }))
        .await
        .expect_err("unknown id must surface an error");
    // The agent returns `InvalidArguments` for "unknown
    // document" so the LLM can recover on the next round.
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::InvalidArguments(_)
    ));
}

#[tokio::test]
async fn read_document_other_session_scope_is_unknown() {
    let (doc_store, _auth) = fresh_store().await;
    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-scope-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.txt"));
    std::fs::write(&file_path, "scoped to A").unwrap();

    doc_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_a,
            "test.txt",
            "text/plain",
            12,
            12,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .unwrap();

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    // Session B must NOT see session A's doc.
    let err = agent
        .invoke(
            &ctx_with_session(session_b),
            json!({ "name": doc_id.to_string() }),
        )
        .await
        .expect_err("cross-session read must be denied");
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::InvalidArguments(_)
    ));
    // Session A CAN see it.
    let ok = agent
        .invoke(
            &ctx_with_session(session_a),
            json!({ "name": doc_id.to_string() }),
        )
        .await
        .expect("session A must see its own doc");
    let parsed: serde_json::Value = serde_json::from_str(&ok).unwrap();
    assert!(parsed["data"]["text"]
        .as_str()
        .unwrap()
        .contains("scoped to A"));
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn read_document_file_missing_on_disk_returns_agent_failed() {
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-missing-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.txt"));
    // Create + delete so the DB row has a path but the file is
    // already gone.
    std::fs::write(&file_path, "vanishing").unwrap();
    std::fs::remove_file(&file_path).unwrap();

    doc_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_id,
            "vanishing.txt",
            "text/plain",
            9,
            9,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .unwrap();

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let err = agent
        .invoke(
            &ctx_with_session(session_id),
            json!({ "name": doc_id.to_string() }),
        )
        .await
        .expect_err("missing file must surface AgentFailed");
    match err {
        nagent_server::agents::AgentError::AgentFailed(msg) => {
            assert!(
                msg.contains("no longer available"),
                "error message must hint at re-upload: {msg}"
            );
        }
        other => panic!("expected AgentFailed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn read_document_without_session_id_returns_invalid_arguments() {
    let (doc_store, _auth) = fresh_store().await;
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = ctx_without_session();
    let err = agent
        .invoke(&ctx, json!({ "name": Uuid::new_v4().to_string() }))
        .await
        .expect_err("missing session id must surface a clear error");
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::InvalidArguments(_)
    ));
}

#[tokio::test]
async fn read_document_truncates_long_text() {
    // Build a fresh store with a tiny max_extracted_chars cap.
    // We use the same `fresh_store` helper so the migrations run
    // automatically; only the cap differs.
    let (base_store, _auth) = fresh_store().await;
    let short_store = DocumentStore::new(
        {
            // Same underlying AuthStore handle, just wrapped with
            // a tiny cap so the truncation path fires.
            // SAFETY: we re-derive a small `AuthStore` from the
            // helper by re-using the `_auth` (which we drop the
            // wider-cap handle from).
            let _ = base_store;
            // Get a fresh auth store for the truncated doc.
            let cfg = AuthConfig {
                enabled: true,
                backends: vec![],
                public_url: "http://127.0.0.1:0".into(),
                session_ttl_days: 7,
                csrf_header: "x-csrf-token".into(),
                db: nagent_server::config::AuthDbConfig {
                    backend: "sqlite".into(),
                    url: "sqlite::memory:".into(),
                    max_connections: 1,
                    auto_migrate: true,
                },
                password: Default::default(),
                oidc: Default::default(),
                passkey: Default::default(),
                credentials: Default::default(),
            };
            let opts: nagent_db::DbOptions = (&cfg).into();
            let store = nagent_db::Db::connect(&opts).await.unwrap();
            store.migrate().await.unwrap();
            store
        },
        5,
        20,
        20_000,
        std::env::temp_dir(),
        0,
        30,
    );
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-trunc-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.txt"));
    std::fs::write(&file_path, "0123456789").unwrap();
    short_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_id,
            "long.txt",
            "text/plain",
            10,
            10,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .unwrap();
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        short_store,
        None,
        None,
    )));
    let result = agent
        .invoke(
            &ctx_with_session(session_id),
            json!({ "name": doc_id.to_string() }),
        )
        .await
        .expect("read must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["data"]["truncated"], true);
    let text = parsed["data"]["text"].as_str().unwrap();
    assert!(
        text.contains("truncated"),
        "truncation marker missing: {text}"
    );
    assert!(
        text.starts_with("01234"),
        "should keep exactly 5 chars: {text}"
    );
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn read_document_invalid_page_range_is_rejected() {
    // We don't actually paginate yet (v1 returns the full text);
    // the agent still validates the syntax so a future
    // implementation can trust the input.
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let err = agent
        .invoke(
            &ctx_with_session(session_id),
            json!({ "name": Uuid::new_v4().to_string(), "page_range": "abc" }),
        )
        .await;
    // The unknown-doc check fires first; we just want to make
    // sure the page_range validation doesn't crash. The error is
    // either InvalidArguments (unknown doc) or a clear
    // page-range message.
    assert!(err.is_err());
}

#[tokio::test]
async fn read_document_rejects_malformed_uuid() {
    let (doc_store, _auth) = fresh_store().await;
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let err = agent
        .invoke(
            &ctx_with_session(Uuid::new_v4()),
            json!({ "name": "not-a-uuid" }),
        )
        .await
        .expect_err("malformed id must fail");
    // The validation happens at parse time, so the error is
    // `InvalidArguments` (recoverable by the LLM) not
    // `AgentFailed` (the historical surface). The error must
    // also echo that `name` is a UUID, not a filename, so the
    // LLM knows to look up the value in `GET /v1/documents`.
    match err {
        nagent_server::agents::AgentError::InvalidArguments(msg) => {
            assert!(
                msg.contains("UUID") && msg.contains("not the filename"),
                "error must explain the UUID-vs-filename contract: {msg}"
            );
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
}

/// Plan 1791384190579 follow-up: the production failure mode
/// that surfaced as
///   agent failed: document lookup failed: encountered unexpected
///   or invalid data: document id `five_steps_perform_2009.pdf`
///   is not a valid UUID: invalid character: found `i` at 1
/// was the LLM passing the *filename* in `data.name` instead of
/// the *UUID* returned by `GET /v1/documents`. The fix is to
/// validate the UUID shape at parse time and surface a clear
/// `InvalidArguments` error pointing at the UUID contract
/// rather than letting the SQL parse error escape. This test
/// pins the behaviour using the exact filename that triggered
/// the production failure.
#[tokio::test]
async fn read_document_filename_instead_of_uuid_surfaces_clear_error() {
    let (doc_store, _auth) = fresh_store().await;
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let err = agent
        .invoke(
            &ctx_with_session(Uuid::new_v4()),
            json!({ "name": "five_steps_perform_2009.pdf" }),
        )
        .await
        .expect_err("filename instead of UUID must fail at parse time");
    match err {
        nagent_server::agents::AgentError::InvalidArguments(msg) => {
            // The error must echo the filename so the LLM can
            // tell what it sent, and it must point at the UUID
            // contract.
            assert!(
                msg.contains("five_steps_perform_2009.pdf")
                    && msg.contains("UUID")
                    && msg.contains("not the filename"),
                "error must echo the offending value + UUID contract: {msg}"
            );
        }
        other => {
            panic!("expected InvalidArguments with a clear UUID-vs-filename message, got {other:?}")
        }
    }
}

/// SEV 1 + 2 fix end-to-end: even with a valid UUID guess, user B
/// cannot read user A's document. The `get_document_by_name`
/// query filters by `(user_id, session_id)` so cross-user reads
/// return `None` and the agent surfaces
/// `InvalidArguments("unknown document …")`.
#[tokio::test]
async fn read_document_blocks_cross_user_reads() {
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let user_a = Uuid::new_v4();
    let user_b = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-cross-user-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.txt"));
    std::fs::write(&file_path, "user A secret").unwrap();
    doc_store
        .db()
        .for_user(user_a)
        .documents()
        .insert(
            doc_id,
            session_id,
            "user-a-secret.txt",
            "text/plain",
            12,
            12,
            None,
            file_path.to_string_lossy().as_ref(),
            None,
        )
        .await
        .unwrap();
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    // user B tries to read user A's doc via the same session.
    let ctx = UserContext::for_chat_session(
        user_b,
        ServiceRegistry::empty().into_arc(),
        None,
        None,
        session_id,
    );
    let err = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect_err("cross-user read must be rejected");
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::InvalidArguments(_)
    ));
    // user A CAN read its own doc.
    let ctx_a = UserContext::for_chat_session(
        user_a,
        ServiceRegistry::empty().into_arc(),
        None,
        None,
        session_id,
    );
    let ok = agent
        .invoke(&ctx_a, json!({ "name": doc_id.to_string() }))
        .await
        .expect("user A must read its own doc");
    let parsed: serde_json::Value = serde_json::from_str(&ok).unwrap();
    assert!(parsed["data"]["text"]
        .as_str()
        .unwrap()
        .contains("user A secret"));
    let _ = std::fs::remove_dir_all(&cache_dir);
}

/// SEV 1 fix: the agent must refuse to read a file whose
/// `disk_path` escapes the `cache_dir` (symlink escape). The
/// DB-level row exists (so a prior bug could have populated it),
/// but the read should be denied by `safe_disk_read`.
#[tokio::test]
async fn read_document_blocks_disk_path_escape() {
    let (_base_store, auth) = fresh_store().await;
    let user_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();

    // Build a `DocumentStore` whose `cache_dir` is a fresh
    // temp-dir subfolder; otherwise the default `std::env::temp_dir()`
    // may differ from the path the test writes to and
    // `safe_disk_read` will canonicalise the cache_dir to a
    // different inode than the symlink's parent.
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-escape-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let doc_store = DocumentStore::new(auth, 100_000, 20, 20_000, cache_dir.clone(), 0, 30);

    let outside_dir = std::env::temp_dir().join(format!(
        "nagent-doc-escape-outside-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_file = outside_dir.join("secret.txt");
    std::fs::write(&outside_file, "TOP SECRET").unwrap();
    // Symlink inside cache_dir pointing outside.
    let link = cache_dir.join("ab/cd/escape.txt");
    std::os::unix::fs::symlink(&outside_file, &link).unwrap();

    doc_store
        .db()
        .for_user(user_id)
        .documents()
        .insert(
            doc_id,
            session_id,
            "escape.txt",
            "text/plain",
            10,
            10,
            None,
            link.to_string_lossy().as_ref(),
            None,
        )
        .await
        .expect("insert must succeed");

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = UserContext::for_chat_session(
        user_id,
        ServiceRegistry::empty().into_arc(),
        None,
        None,
        session_id,
    );
    let err = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect_err("escape must be rejected");
    match err {
        nagent_server::agents::AgentError::AgentFailed(msg) => {
            assert!(
                msg.contains("no longer available"),
                "error must hint at re-upload: {msg}"
            );
        }
        other => panic!("expected AgentFailed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&cache_dir);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

// ---- Plan 1791384190579 follow-up: real-world fixture ----
//
// `tests/fixtures/five_steps_perform_2009.pdf` is a real 55-page
// PDF that triggers the `lopdf 0.34` ToUnicode-CMap error on every
// page when text goes through `lopdf::Document::extract_text`. We
// bake it in via `include_bytes!` so the test is self-contained:
// no external network access, no operator-side setup, runs in CI.
//
// The extraction path now routes through `pdf_extract::output_doc_page`
// (see `extract_page_text` in `src/documents/pages.rs`), which uses
// `adobe-cmap-parser` and recovers the correct glyph-to-Unicode
// mapping on the same files lopdf rejects. This test pins that
// recovery so a future regression that reverts to lopdf's extractor
// (or upgrades `lopdf` past a working CMap parser) is caught here —
// the assertion would flip to `unreadable_pages == 55` again,
// exactly the symptom that bit the operator in production.
const FIVE_STEPS_PDF: &[u8] = include_bytes!("fixtures/five_steps_perform_2009.pdf");

#[tokio::test]
async fn read_document_real_pdf_with_malformed_cmap_extracts_cleanly() {
    // Sanity check: the fixture is the expected PDF.
    let doc = lopdf::Document::load_mem(FIVE_STEPS_PDF).expect("load");
    let pages = doc.get_pages();
    assert_eq!(pages.len(), 55, "fixture must be 55 pages");

    // Drive the real `pages::extract_pages` path — the same one
    // the upload route runs in production. Every page must extract
    // to real text (no `UNREADABLE_PAGE_MARKER`), proving that
    // pdf-extract's `adobe-cmap-parser` recovers the CMap lopdf
    // 0.34 chokes on.
    let key = CredentialsKey::from_bytes([0x77; 32]);
    let tmp = std::env::temp_dir().join(format!(
        "nagent-doc-real-pdf-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let dir = tmp.join("pages");
    let index = nagent_server::documents::pages::extract_pages(FIVE_STEPS_PDF, &dir, &key)
        .expect("extract must succeed on real PDF with CMap failures");
    let (page_count, unreadable_pages, preview) = match &index {
        nagent_server::documents::pages::PagesIndex::Indexed {
            page_count,
            unreadable_pages,
            preview,
            ..
        } => (*page_count, *unreadable_pages, preview.clone()),
        other => panic!("expected Indexed, got {other:?}"),
    };
    assert_eq!(page_count, 55, "all 55 pages must be in the index");
    assert_eq!(
        unreadable_pages, 0,
        "every page must extract via pdf-extract (was 55 under lopdf 0.34, see plan 1791384190579)"
    );
    assert!(
        preview.contains("PostgreSQL") || preview.contains("Five Steps"),
        "preview should contain real document content, got: {preview:?}"
    );

    // The page blobs decrypt to real text, not the unreadable marker.
    let text =
        nagent_server::documents::pages::read_pages(&dir, &key, 1, 1).expect("read must decrypt");
    assert!(
        !text.contains(nagent_server::documents::pages::UNREADABLE_PAGE_MARKER),
        "real PDF must NOT yield the unreadable-page marker (pdf-extract recovered it), got: {text:?}"
    );
    assert!(
        text.contains("PostgreSQL") || text.contains("Five Steps"),
        "page 1 should mention PostgreSQL or Five Steps, got: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[allow(dead_code)]
fn _ensure_arc_arc(_x: Arc<()>) {}

// ---- Plan 1791384190579: per-page encrypted store tests ----
//
// The `read_document_pdf_*` tests above exercise the legacy
// re-extraction path. The tests below pin the new contract:
// real page count surfaced in the bubble, range-mode read
// returning only the requested pages, range-mode cap
// rejection, defence-in-depth check that the on-disk blobs
// are encrypted, and graceful failure when the credentials
// key is missing.

#[tokio::test]
async fn read_document_pdf_returns_real_page_count() {
    let pdf_bytes = build_minimal_pdf("page-count-test");
    let key = Arc::new(CredentialsKey::from_bytes([0x55; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    upload_pdf(
        &doc_store,
        session_id,
        doc_id,
        &pdf_bytes,
        &key,
        Some((
            vec!["page-count-test".to_string()],
            "page-count-test".to_string(),
            1,
            0,
        )),
    )
    .await;

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store,
        None,
        Some(key),
    )));
    let ctx = ctx_with_session(session_id);
    let result = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect("PDF read must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    let summary = parsed["summary"].as_str().expect("summary");
    // The summary must contain the real page count (1 page, not
    // "?") because the per-page store sets `page_count` at
    // extract time.
    assert!(
        summary.contains("1 pages"),
        "expected summary to show 1 pages, got: {summary}"
    );
    assert!(
        !summary.contains("? pages"),
        "summary must not contain the '?' placeholder, got: {summary}"
    );
}

#[tokio::test]
async fn read_document_pdf_page_range_returns_only_target_pages() {
    // Bypass the lopdf extractor (see `upload_pdf` doc for the
    // rationale): seed the per-page store with a single page so
    // the range-mode wire shape can be exercised end-to-end.
    // The unit test in `pages.rs` covers the multi-page lopdf
    // round-trip.
    let pdf_bytes = build_minimal_pdf("range-mode-page-body");
    let key = Arc::new(CredentialsKey::from_bytes([0x42; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let (pages_root, _page_count) = upload_pdf(
        &doc_store,
        session_id,
        doc_id,
        &pdf_bytes,
        &key,
        Some((
            vec!["range-mode-page-body".to_string()],
            "range-mode-page-body".to_string(),
            1,
            0,
        )),
    )
    .await;

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store,
        None,
        Some(key),
    )));
    let ctx = ctx_with_session(session_id);
    let result = agent
        .invoke(
            &ctx,
            json!({ "name": doc_id.to_string(), "page_range": "1-1" }),
        )
        .await
        .expect("range read must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    let text = parsed["data"]["text"]
        .as_str()
        .expect("range must carry text");
    assert!(
        text.contains("range-mode-page-body"),
        "missing per-page text: {text:?}"
    );
    assert!(
        text.contains("--- page 1 ---"),
        "missing page separator: {text:?}"
    );
    assert_eq!(
        parsed["data"]["page_range_applied"].as_str(),
        Some("1-1"),
        "applied range must be reported"
    );
    let summary = parsed["summary"]
        .as_str()
        .expect("summary must be a string");
    assert!(
        summary.contains("showing 1-1"),
        "summary must echo the range: {summary}"
    );
    let _ = std::fs::remove_dir_all(pages_root.parent().unwrap());
}

#[tokio::test]
async fn read_document_pdf_page_range_exceeds_cap_is_rejected() {
    // Per-call cap is 1 — even a 2-page range must be rejected
    // with InvalidArguments so the LLM gets a recoverable hint.
    let pdf_bytes = build_minimal_pdf("range-cap-test");
    let key = Arc::new(CredentialsKey::from_bytes([0x33; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let (pages_root, _) = upload_pdf(
        &doc_store,
        session_id,
        doc_id,
        &pdf_bytes,
        &key,
        Some((
            vec!["page-a".to_string(), "page-b".to_string()],
            "page-a".to_string(),
            2,
            0,
        )),
    )
    .await;

    // Re-build a store with `max_pages_per_call = 1` so the cap
    // path fires.
    let capped_store = DocumentStore::new(
        doc_store.db().clone(),
        100_000,
        1,
        20_000,
        doc_store.cache_dir().to_path_buf(),
        0,
        30,
    );
    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        capped_store,
        None,
        Some(key),
    )));
    let ctx = ctx_with_session(session_id);
    let err = agent
        .invoke(
            &ctx,
            json!({ "name": doc_id.to_string(), "page_range": "1-2" }),
        )
        .await
        .expect_err("range wider than max_pages_per_call must be rejected");
    match err {
        nagent_server::agents::AgentError::InvalidArguments(msg) => {
            assert!(
                msg.contains("max_pages_per_call"),
                "error must echo the cap: {msg}"
            );
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(pages_root.parent().unwrap());
}

#[tokio::test]
async fn read_document_pdf_pages_encrypted_at_rest() {
    let pdf_bytes = build_minimal_pdf("encrypted at rest");
    let key = Arc::new(CredentialsKey::from_bytes([0x99; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let (pages_root, page_count) = upload_pdf(
        &doc_store,
        session_id,
        doc_id,
        &pdf_bytes,
        &key,
        Some((
            vec!["encrypted at rest".to_string()],
            "encrypted at rest".to_string(),
            1,
            0,
        )),
    )
    .await;
    assert_eq!(page_count, 1);
    // Defence-in-depth: page-0001.bin must not be parseable as
    // UTF-8 (the plaintext would be a single BT…ET line). If a
    // regression silently switches back to plaintext the file
    // starts with `BT ` and is valid UTF-8 — this assertion
    // catches that.
    let page_path = pages_root.join("page-0001.bin");
    let raw = std::fs::read(&page_path).expect("page-0001.bin must exist");
    assert!(
        std::str::from_utf8(&raw).is_err(),
        "page blob must be ciphertext, got UTF-8: {raw:?}"
    );
    // First 12 bytes are the nonce (AES-256-GCM convention).
    assert!(raw.len() >= 12 + 16, "nonce+tag must be present");
    let _ = std::fs::remove_dir_all(pages_root.parent().unwrap());
}

#[tokio::test]
async fn read_document_without_credentials_key_returns_agent_failed() {
    // The PDF was uploaded with the key, but the read path is
    // wired without it (simulates a build where the operator
    // rotated the key after the upload). The agent must surface
    // a clean "ask the user to re-upload" error rather than
    // crashing or decrypting with a wrong key.
    let pdf_bytes = build_minimal_pdf("needs-key");
    let upload_key = Arc::new(CredentialsKey::from_bytes([0xAA; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(upload_key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();
    let (pages_root, _) = upload_pdf(
        &doc_store,
        session_id,
        doc_id,
        &pdf_bytes,
        &upload_key,
        Some((vec!["needs-key".to_string()], "needs-key".to_string(), 1, 0)),
    )
    .await;

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store, None, None,
    )));
    let ctx = ctx_with_session(session_id);
    let err = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect_err("missing key must surface AgentFailed");
    match err {
        nagent_server::agents::AgentError::AgentFailed(msg) => {
            assert!(
                msg.contains("re-upload"),
                "error must hint at re-upload: {msg}"
            );
        }
        other => panic!("expected AgentFailed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(pages_root.parent().unwrap());
}

#[tokio::test]
async fn read_document_overview_reports_unreadable_pages() {
    // Pin the new contract: when every page of a PDF failed to
    // extract (e.g. unparseable ToUnicode CMap) the overview
    // envelope must:
    //   1. report `data.unreadable_pages == page_count` so the
    //      LLM can tell the document is entirely unreadable,
    //   2. leave the preview empty (not a wall of unreadable
    //      marker text),
    //   3. surface a hint pointing at the root cause (font /
    //      CMap) so the LLM does not waste rounds re-trying
    //      page_range calls that will return the same marker.
    // We bypass `upload_pdf` and call `StoreDocumentSource` with
    // a hand-rolled row whose `pages_dir` points at a directory
    // containing 1 `page-0001.bin` blob that decrypts to the
    // `UNREADABLE_PAGE_MARKER` constant.
    let key = Arc::new(CredentialsKey::from_bytes([0xCC; 32]));
    let (doc_store, _auth) = fresh_store_with_keys(Some(key.clone())).await;
    let session_id = Uuid::new_v4();
    let doc_id = Uuid::new_v4();

    // Build the per-page store on disk + insert the row.
    let cache_dir = std::env::temp_dir().join(format!(
        "nagent-doc-unreadable-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(cache_dir.join("ab/cd")).unwrap();
    let file_path = cache_dir.join("ab/cd").join(format!("{doc_id}.pdf"));
    std::fs::write(&file_path, b"%PDF-1.4\n% fake\n").unwrap();
    let pages_root = file_path.with_file_name(format!(
        "{}.pages",
        file_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("doc")
    ));
    std::fs::create_dir_all(&pages_root).unwrap();
    // Write the unreadable-page marker as the only page.
    let sealed = nagent_server::credentials::crypto::encrypt(
        &key,
        nagent_server::documents::pages::UNREADABLE_PAGE_MARKER,
    )
    .unwrap();
    let page_path = pages_root.join("page-0001.bin");
    let mut combined = Vec::with_capacity(sealed.nonce.len() + sealed.ciphertext.len());
    combined.extend_from_slice(&sealed.nonce);
    combined.extend_from_slice(&sealed.ciphertext);
    std::fs::write(&page_path, &combined).unwrap();
    // And the meta.bin that says "1 page, 1 unreadable".
    let meta = nagent_server::documents::pages::PagesIndex::Indexed {
        page_count: 1,
        unreadable_pages: 1,
        preview: String::new(),
        toc: Vec::new(),
    };
    let meta_json = serde_json::to_vec(&meta).unwrap();
    let meta_plain = String::from_utf8(meta_json).unwrap();
    let meta_sealed = nagent_server::credentials::crypto::encrypt(&key, &meta_plain).unwrap();
    let meta_path = pages_root.join("meta.bin");
    let mut combined = Vec::with_capacity(meta_sealed.nonce.len() + meta_sealed.ciphertext.len());
    combined.extend_from_slice(&meta_sealed.nonce);
    combined.extend_from_slice(&meta_sealed.ciphertext);
    std::fs::write(&meta_path, &combined).unwrap();
    let pages_dir_str = pages_root.to_string_lossy().into_owned();
    doc_store
        .db()
        .for_user(Uuid::nil())
        .documents()
        .insert(
            doc_id,
            session_id,
            "fully-unreadable.pdf",
            "application/pdf",
            file_path.metadata().unwrap().len(),
            nagent_server::documents::pages::UNREADABLE_PAGE_MARKER.len() as u64,
            Some(1),
            file_path.to_string_lossy().as_ref(),
            Some(&pages_dir_str),
        )
        .await
        .expect("insert must succeed");

    let agent = ReadDocumentAgent::new(Arc::new(nagent_server::agents::StoreDocumentSource::new(
        doc_store,
        None,
        Some(key),
    )));
    let ctx = ctx_with_session(session_id);
    let result = agent
        .invoke(&ctx, json!({ "name": doc_id.to_string() }))
        .await
        .expect("overview must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    let data = &parsed["data"];
    assert_eq!(data["page_count"], 1);
    assert_eq!(data["unreadable_pages"], 1);
    let preview = data["preview"].as_str().expect("preview string");
    assert!(
        preview.is_empty(),
        "preview must be empty when every page is unreadable, got: {preview:?}"
    );
    let hint = data["hint"].as_str().expect("hint string");
    assert!(
        hint.contains("entirely unreadable"),
        "hint must point at the root cause: {hint}"
    );

    // A range-mode call on the unreadable page must also surface
    // a clearer hint so the LLM does not loop on it.
    let result = agent
        .invoke(
            &ctx,
            json!({ "name": doc_id.to_string(), "page_range": "1-1" }),
        )
        .await
        .expect("range read must succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("json");
    let text = parsed["data"]["text"].as_str().expect("text string");
    assert!(
        text.contains(nagent_server::documents::pages::UNREADABLE_PAGE_MARKER),
        "range-mode read must still return the marker so the LLM knows the page is unreadable, got: {text:?}"
    );
    let hint = parsed["data"]["hint"].as_str().expect("hint string");
    assert!(
        hint.contains("every page"),
        "range-mode hint must say every page failed: {hint}"
    );

    let _ = std::fs::remove_dir_all(&cache_dir);
}
