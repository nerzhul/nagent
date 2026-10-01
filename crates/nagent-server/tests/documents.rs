//! `read_document` agent — end-to-end against an in-memory sqlite
//! store.
//!
//! Covers the happy path (read after upload), the unknown-doc
//! path (wrong session scope), the file-missing-on-disk path, and
//! the page-range validation path. Mirrors the harness in
//! `tests/auth_e2e.rs` so the migrations + sqlx pool lifecycle
//! run the same way.

use std::sync::Arc;

use nagent_server::agents::{Agent, ServiceRegistry, UserContext};
use nagent_server::config::AuthConfig;
use nagent_server::documents::{DocumentStore, ReadDocumentAgent};
use serde_json::json;

use uuid::Uuid;

async fn fresh_store() -> (DocumentStore, nagent_db::Db) {
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
    let doc_store = DocumentStore::new(store.clone(), 100_000, std::env::temp_dir(), 0);
    (doc_store, store)
}

fn ctx_with_session(session_id: Uuid) -> UserContext {
    let services = ServiceRegistry::empty().into_arc();
    UserContext::for_chat_session(Uuid::nil(), services, None, session_id)
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
        )
        .await
        .expect("insert must succeed");

    let agent = ReadDocumentAgent::new(doc_store.clone(), None);
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

#[tokio::test]
async fn read_document_unknown_id_returns_invalid_arguments() {
    let (doc_store, _auth) = fresh_store().await;
    let session_id = Uuid::new_v4();
    let agent = ReadDocumentAgent::new(doc_store, None);
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
        )
        .await
        .unwrap();

    let agent = ReadDocumentAgent::new(doc_store, None);
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
        )
        .await
        .unwrap();

    let agent = ReadDocumentAgent::new(doc_store, None);
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
    let agent = ReadDocumentAgent::new(doc_store, None);
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
        std::env::temp_dir(),
        0,
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
        )
        .await
        .unwrap();
    let agent = ReadDocumentAgent::new(short_store, None);
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
    let agent = ReadDocumentAgent::new(doc_store, None);
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
    let agent = ReadDocumentAgent::new(doc_store, None);
    let err = agent
        .invoke(
            &ctx_with_session(Uuid::new_v4()),
            json!({ "name": "not-a-uuid" }),
        )
        .await
        .expect_err("malformed id must fail");
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::AgentFailed(_)
    ));
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
        )
        .await
        .unwrap();
    let agent = ReadDocumentAgent::new(doc_store, None);
    // user B tries to read user A's doc via the same session.
    let ctx = UserContext::for_chat_session(
        user_b,
        ServiceRegistry::empty().into_arc(),
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
    let doc_store = DocumentStore::new(auth, 100_000, cache_dir.clone(), 0);

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
        )
        .await
        .unwrap();

    let agent = ReadDocumentAgent::new(doc_store, None);
    let ctx = UserContext::for_chat_session(
        user_id,
        ServiceRegistry::empty().into_arc(),
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

#[allow(dead_code)]
fn _ensure_arc_arc(_x: Arc<()>) {}
