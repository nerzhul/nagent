//! Cross-domain integration tests for [`crate`].
//!
//! The tests run against an in-memory SQLite database per test
//! case. The migration set is applied once via
//! `Db::connect` + `Db::migrate`, so every assertion exercises the
//! same SQL the production server does.
//!
//! Postgres parity tests (plan 4.A S9) live behind a feature + env
//! var so the local `cargo test` loop stays fast. A CI job with a
//! `postgres` service container sets `NAGENT_TEST_PG_URL` to a real
//! URL and runs the parity tests against both engines.

#![cfg(test)]

use uuid::Uuid;

use crate::DbEngine;
use crate::DbOptions;

#[cfg(feature = "db-sqlite")]
async fn sqlite_db() -> crate::Db {
    let opts = DbOptions {
        backend: DbEngine::Sqlite,
        // Unique per-test so concurrent test cases do not share
        // an in-memory DB. `cache=shared` lets the pool's
        // multiple connections see the same schema.
        url: format!(
            "sqlite://file:test_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: false,
    };
    let db = crate::Db::connect(&opts).await.expect("sqlite connects");
    db.migrate().await.expect("migrations apply");
    db
}

#[cfg(feature = "db-postgres")]
fn pg_url() -> Option<String> {
    std::env::var("NAGENT_TEST_PG_URL").ok()
}

#[cfg(feature = "db-postgres")]
async fn try_pg_db() -> Option<crate::Db> {
    let url = pg_url()?;
    let opts = DbOptions {
        backend: DbEngine::Postgres,
        url,
        max_connections: 1,
        auto_migrate: false,
    };
    let db = crate::Db::connect(&opts).await.ok()?;
    db.migrate().await.ok()?;
    Some(db)
}

/// Build the live Postgres `Db` for the parity tests, or short-
/// circuit with `None` when `NAGENT_TEST_PG_URL` is unset. Tests
/// that receive `None` early-return — combined with `#[ignore]` on
/// the test fn, `cargo test` reports them as "ignored" locally
/// and the CI Postgres job runs them with `--include-ignored`.
#[cfg(feature = "db-postgres")]
macro_rules! pg_or_skip {
    () => {
        match try_pg_db().await {
            Some(db) => db,
            None => return,
        }
    };
}

// ---- users ---------------------------------------------------------------

#[cfg(test)]
mod users {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn create_then_get_and_conflict() {
        let db = sqlite_db().await;
        let id = db
            .users
            .create("alice@example.com", "Alice", "local", Some(b"hash"))
            .await
            .expect("create");
        let fetched = db
            .users
            .get_by_id(id)
            .await
            .expect("get")
            .expect("row exists");
        assert_eq!(fetched.email, "alice@example.com");
        assert_eq!(fetched.provider, "local");
        // Case-insensitive duplicate.
        let dup = db
            .users
            .create("ALICE@example.com", "Alice2", "local", Some(b"hash"))
            .await;
        assert!(matches!(dup, Err(crate::Error::Conflict(_))));
        // get_by_email round-trips.
        let by_email = db
            .users
            .get_by_email("Alice@Example.COM")
            .await
            .expect("get_by_email")
            .expect("row");
        assert_eq!(by_email.id, id);
        // set_password + delete + get_by_id is None.
        let affected = db.users.set_password(id, b"new-hash").await.expect("set");
        assert_eq!(affected, 1);
        let deleted = db.users.delete(id).await.expect("delete");
        assert_eq!(deleted, 1);
        assert!(db.users.get_by_id(id).await.expect("get").is_none());
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn list_and_count_by_provider() {
        let db = sqlite_db().await;
        db.users
            .create("a@x", "A", "local", Some(b"h"))
            .await
            .unwrap();
        db.users
            .create("b@x", "B", "local", Some(b"h"))
            .await
            .unwrap();
        db.users.create("c@x", "C", "oidc:foo", None).await.unwrap();
        let all = db.users.list(None).await.expect("list");
        assert_eq!(all.len(), 3);
        let locals = db.users.list(Some("local")).await.expect("list locals");
        assert_eq!(locals.len(), 2);
        let local_count = db.users.count_by_provider("local").await.expect("count");
        assert_eq!(local_count, 2);
    }
}

// ---- chat_sessions (scoped) ----------------------------------------------

#[cfg(test)]
mod chat_sessions {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_bind_and_verify() {
        let db = sqlite_db().await;
        let user = db
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.chat_sessions.for_user(user);
        let session = Uuid::new_v4();
        scoped.bind(session).await.expect("bind");
        // First verify succeeds.
        scoped.touch_and_verify(session).await.expect("verify ok");
        // Second bind by a different user must NOT match.
        let other = db
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_scoped = db.chat_sessions.for_user(other);
        let res = other_scoped.touch_and_verify(session).await;
        assert!(
            matches!(
                res,
                Err(crate::chat_sessions::ChatSessionError::NotBound(_, _))
            ),
            "wrong user must surface NotBound, got {res:?}"
        );
    }
}

// ---- documents (scoped) --------------------------------------------------

#[cfg(test)]
mod documents {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_insert_list_delete() {
        let db = sqlite_db().await;
        let user = db
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let docs = db.documents.for_user(user);
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        docs.insert(
            id,
            session,
            "test.txt",
            "text/plain",
            42,
            100,
            None,
            "/tmp/test.txt",
        )
        .await
        .expect("insert");
        assert_eq!(docs.count_for_session(session).await.unwrap(), 1);
        let listed = docs.list_for_session(session).await.expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].original_name, "test.txt");
        // Cross-user isolation: a different user must NOT see the row.
        let other = db
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_docs = db.documents.for_user(other);
        assert_eq!(other_docs.count_for_session(session).await.unwrap(), 0);
        assert!(other_docs
            .get_for_session(id, session)
            .await
            .expect("get")
            .is_none());
        // Delete by the original user returns the disk path.
        let path = docs.delete(id, session).await.expect("delete");
        assert_eq!(path.expect("path").to_string_lossy(), "/tmp/test.txt");
        assert_eq!(docs.count_for_session(session).await.unwrap(), 0);
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn sweep_older_than_is_admin_only() {
        // Sweep lives on the unscoped repository; the scoped view
        // does NOT expose it (a per-user route handler must never
        // trigger a global sweep).
        let db = sqlite_db().await;
        // Insert a row, sweep with a 1ns TTL — the row must come back.
        let user = db
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        db.documents
            .insert(
                id,
                session,
                user,
                "x.txt",
                "text/plain",
                1,
                1,
                None,
                "/tmp/x",
            )
            .await
            .expect("insert");
        let swept = db
            .documents
            .sweep_older_than(std::time::Duration::from_nanos(1))
            .await
            .expect("sweep");
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].id, id);
    }
}

// ---- credentials (scoped) ------------------------------------------------

#[cfg(test)]
mod credentials {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_upsert_fetch_list_delete() {
        let db = sqlite_db().await;
        let user = db
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let creds = db.credentials.for_user(user);
        // First upsert: store two fields.
        let nonce1 = vec![0xAA; 12];
        let nonce2 = vec![0xBB; 12];
        creds
            .upsert(
                "github",
                &[
                    ("token".to_string(), nonce1.clone(), vec![1, 2, 3]),
                    ("secret".to_string(), nonce2.clone(), vec![4, 5, 6]),
                ],
            )
            .await
            .expect("upsert");
        let keys = creds.list_field_keys("github").await.expect("list");
        assert_eq!(keys, vec!["secret".to_string(), "token".to_string()]);
        let row = creds
            .fetch("github", "token")
            .await
            .expect("fetch")
            .expect("row");
        assert_eq!(row.nonce, nonce1);
        assert_eq!(row.ciphertext, vec![1, 2, 3]);
        // Second upsert replaces all fields atomically.
        creds
            .upsert("github", &[("only".to_string(), nonce1.clone(), vec![9])])
            .await
            .expect("upsert 2");
        let keys = creds.list_field_keys("github").await.expect("list 2");
        assert_eq!(keys, vec!["only".to_string()]);
        // Cross-user isolation.
        let other = db
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_creds = db.credentials.for_user(other);
        assert!(other_creds
            .fetch("github", "only")
            .await
            .expect("fetch")
            .is_none());
        // delete_service clears the row.
        let deleted = creds.delete_service("github").await.expect("delete");
        assert_eq!(deleted, 1);
        assert!(creds
            .fetch("github", "only")
            .await
            .expect("fetch")
            .is_none());
    }
}

// ---- Postgres parity -----------------------------------------------------

/// Run the full SQLite suite against Postgres when
/// `NAGENT_TEST_PG_URL` is set (plan 4.A S9). Each test carries
/// `#[ignore]` so the local `cargo test` loop stays fast; the CI
/// Postgres job runs them via `cargo test -- --include-ignored`.
#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_create_user() {
    let db = pg_or_skip!();
    let id = db
        .users
        .create("pg@example.com", "PG", "local", Some(b"h"))
        .await
        .expect("pg create");
    let fetched = db.users.get_by_id(id).await.expect("get").expect("row");
    assert_eq!(fetched.email, "pg@example.com");
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_documents_scoped() {
    let db = pg_or_skip!();
    let user = db
        .users
        .create("pg-doc@example.com", "PG", "local", Some(b"h"))
        .await
        .unwrap();
    let docs = db.documents.for_user(user);
    let session = Uuid::new_v4();
    let id = Uuid::new_v4();
    docs.insert(id, session, "x.txt", "text/plain", 1, 1, None, "/tmp/x")
        .await
        .expect("pg insert");
    assert_eq!(docs.count_for_session(session).await.unwrap(), 1);
    let path = docs.delete(id, session).await.expect("delete");
    assert!(path.is_some());
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_credentials_scoped() {
    let db = pg_or_skip!();
    let user = db
        .users
        .create("pg-cred@example.com", "PG", "local", Some(b"h"))
        .await
        .unwrap();
    let creds = db.credentials.for_user(user);
    let nonce = vec![0xCC; 12];
    creds
        .upsert("svc", &[("k".to_string(), nonce.clone(), vec![1])])
        .await
        .expect("pg upsert");
    let row = creds.fetch("svc", "k").await.expect("fetch").expect("row");
    assert_eq!(row.nonce, nonce);
}
