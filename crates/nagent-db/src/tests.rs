//! Cross-domain integration tests for [`crate`].
//!
//! The tests run against an in-memory SQLite database per test
//! case. The migration set is applied once via
//! `Db::connect` + `Db::migrate`, so every assertion exercises the
//! same SQL the production server does.
//!
//! Postgres parity tests  live behind a feature + env
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
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"hash"))
            .await
            .expect("create");
        let fetched = db
            .admin()
            .users
            .get_by_id(id)
            .await
            .expect("get")
            .expect("row exists");
        assert_eq!(fetched.email, "alice@example.com");
        assert_eq!(fetched.provider, "local");
        // Case-insensitive duplicate.
        let dup = db
            .admin()
            .users
            .create("ALICE@example.com", "Alice2", "local", Some(b"hash"))
            .await;
        assert!(matches!(dup, Err(crate::Error::Conflict(_))));
        // get_by_email round-trips.
        let by_email = db
            .admin()
            .users
            .get_by_email("Alice@Example.COM")
            .await
            .expect("get_by_email")
            .expect("row");
        assert_eq!(by_email.id, id);
        // set_password + delete + get_by_id is None.
        let affected = db
            .admin()
            .users
            .set_password(id, b"new-hash")
            .await
            .expect("set");
        assert_eq!(affected, 1);
        let deleted = db.admin().users.delete(id).await.expect("delete");
        assert_eq!(deleted, 1);
        assert!(db.admin().users.get_by_id(id).await.expect("get").is_none());
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn list_and_count_by_provider() {
        let db = sqlite_db().await;
        db.admin()
            .users
            .create("a@x", "A", "local", Some(b"h"))
            .await
            .unwrap();
        db.admin()
            .users
            .create("b@x", "B", "local", Some(b"h"))
            .await
            .unwrap();
        db.admin()
            .users
            .create("c@x", "C", "oidc:foo", None)
            .await
            .unwrap();
        let all = db.admin().users.list(None).await.expect("list");
        assert_eq!(all.len(), 3);
        let locals = db
            .admin()
            .users
            .list(Some("local"))
            .await
            .expect("list locals");
        assert_eq!(locals.len(), 2);
        let local_count = db
            .admin()
            .users
            .count_by_provider("local")
            .await
            .expect("count");
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
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).chat_sessions();
        let session = Uuid::new_v4();
        scoped.bind(session).await.expect("bind");
        // First verify succeeds.
        scoped.touch_and_verify(session).await.expect("verify ok");
        // Second bind by a different user must NOT match.
        let other = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_scoped = db.for_user(other).chat_sessions();
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

// ---- preferences (scoped) ----------------------------------------------
//
// Plan 4.A: confirm the scoped view drops the `user_id` argument
// on `get` / `upsert` and that a handler holding one scoped view
// cannot read or write another user's row.

#[cfg(test)]
mod preferences {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_get_default_then_upsert() {
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).preferences();
        // First read returns the defaults (no row yet).
        let prefs = scoped.get().await.expect("default prefs");
        assert!(!prefs.share_location_enabled);
        assert!(!prefs.share_timezone_enabled);
        assert!(
            prefs.reply_language.is_none(),
            "default reply_language is None (Auto)"
        );
        // Scoped upsert writes only the bound user's row.
        let updated = scoped
            .upsert(true, false, None, None, None, false)
            .await
            .unwrap();
        assert!(updated.share_location_enabled);
        assert!(!updated.share_timezone_enabled);
        assert!(updated.reply_language.is_none());
        assert!(updated.additional_instructions.is_none());
        assert!(updated.temperature.is_none());
        assert!(!updated.memory_enabled);
        let reread = scoped.get().await.expect("reread");
        assert!(reread.share_location_enabled);
        assert!(!reread.share_timezone_enabled);
        assert!(reread.reply_language.is_none());
        assert!(reread.additional_instructions.is_none());
        assert!(reread.temperature.is_none());
        assert!(!reread.memory_enabled);
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_views_are_isolated_per_user() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        db.for_user(alice)
            .preferences()
            .upsert(true, false, Some("fr".into()), None, None, true)
            .await
            .unwrap();
        db.for_user(bob)
            .preferences()
            .upsert(false, true, None, None, None, false)
            .await
            .unwrap();
        let alice_prefs = db.for_user(alice).preferences().get().await.unwrap();
        let bob_prefs = db.for_user(bob).preferences().get().await.unwrap();
        assert!(alice_prefs.share_location_enabled);
        assert!(!alice_prefs.share_timezone_enabled);
        assert_eq!(alice_prefs.reply_language.as_deref(), Some("fr"));
        assert!(alice_prefs.memory_enabled);
        assert!(!bob_prefs.share_location_enabled);
        assert!(bob_prefs.share_timezone_enabled);
        assert!(bob_prefs.reply_language.is_none());
        assert!(!bob_prefs.memory_enabled);
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn reply_language_round_trips_some_then_none() {
        // Confirms the new column (added in migration 0007) round-trips
        // both `Some(...)` and `None` so a user can switch between
        // "Auto" and an explicit language without losing the boolean
        // opt-ins. The HTTP handler (`PutPreferencesBody`) enforces the
        // same full-triple contract; this test pins the storage side.
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("lang@example.com", "Lang", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).preferences();

        let updated = scoped
            .upsert(false, false, Some("es".into()), None, None, false)
            .await
            .expect("upsert with Some");
        assert_eq!(updated.reply_language.as_deref(), Some("es"));
        assert!(!updated.memory_enabled);

        let reread = scoped.get().await.expect("reread Some");
        assert_eq!(reread.reply_language.as_deref(), Some("es"));
        assert!(!reread.share_location_enabled);
        assert!(!reread.share_timezone_enabled);
        assert!(!reread.memory_enabled);

        // Clearing the language (PUT with `null`) must NOT touch the
        // boolean opt-ins — the row is a strict atomic replace of the
        // triple, not a partial update.
        let cleared = scoped
            .upsert(true, true, None, None, None, false)
            .await
            .expect("upsert with None");
        assert!(cleared.reply_language.is_none());
        assert!(cleared.share_location_enabled);
        assert!(cleared.share_timezone_enabled);
        assert!(!cleared.memory_enabled);
        let reread = scoped.get().await.expect("reread None");
        assert!(reread.reply_language.is_none());
        assert!(reread.share_location_enabled);
        assert!(reread.share_timezone_enabled);
        assert!(!reread.memory_enabled);
    }

    #[cfg(feature = "db-postgres")]
    #[tokio::test]
    #[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
    async fn postgres_parity_reply_language_round_trip() {
        // Same round-trip as the SQLite test above, against a live
        // Postgres to confirm the SQL shape (`$N` placeholders, the
        // `EXCLUDED.` references, the nullable TEXT cast) all agree.
        let db = pg_or_skip!();
        let user = db
            .admin()
            .users
            .create("pg-lang@example.com", "PG", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).preferences();
        let updated = scoped
            .upsert(false, false, Some("ja".into()), None, None, false)
            .await
            .expect("pg upsert Some");
        assert_eq!(updated.reply_language.as_deref(), Some("ja"));
        let cleared = scoped
            .upsert(false, false, None, None, None, false)
            .await
            .expect("pg upsert None");
        assert!(cleared.reply_language.is_none());
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn llm_settings_round_trip_some_then_none() {
        // Confirms the migration-0008 columns (`additional_instructions`,
        // `temperature`) round-trip both `Some(...)` and `None`. The HTTP
        // handler (`PutPreferencesBody`) treats both as legitimate wire
        // values — `null` JSON is the "use the proxy default" signal,
        // a non-empty string / finite number is the explicit user choice.
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("llm@example.com", "LLM", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).preferences();

        // Initial write: explicit instructions + explicit temperature.
        let updated = scoped
            .upsert(
                false,
                false,
                None,
                Some("Reply concisely.".to_string()),
                Some(0.5),
                false,
            )
            .await
            .expect("upsert with Some");
        assert_eq!(
            updated.additional_instructions.as_deref(),
            Some("Reply concisely.")
        );
        assert_eq!(updated.temperature, Some(0.5));
        assert!(!updated.memory_enabled);

        let reread = scoped.get().await.expect("reread Some");
        assert_eq!(
            reread.additional_instructions.as_deref(),
            Some("Reply concisely.")
        );
        assert_eq!(reread.temperature, Some(0.5));
        // Booleans untouched.
        assert!(!reread.share_location_enabled);
        assert!(!reread.share_timezone_enabled);
        assert!(!reread.memory_enabled);

        // Clearing both (PUT with `null`) must NOT touch the boolean
        // opt-ins or the reply language — the row is a strict atomic
        // replace of the quintuple, not a partial update.
        let cleared = scoped
            .upsert(true, true, Some("fr".into()), None, None, false)
            .await
            .expect("upsert with None");
        assert!(cleared.additional_instructions.is_none());
        assert!(cleared.temperature.is_none());
        assert!(cleared.share_location_enabled);
        assert!(cleared.share_timezone_enabled);
        assert!(!cleared.memory_enabled);
        assert_eq!(cleared.reply_language.as_deref(), Some("fr"));

        let reread = scoped.get().await.expect("reread None");
        assert!(reread.additional_instructions.is_none());
        assert!(reread.temperature.is_none());
        assert!(reread.share_location_enabled);
        assert!(reread.share_timezone_enabled);
        assert!(!reread.memory_enabled);
        assert_eq!(reread.reply_language.as_deref(), Some("fr"));
    }

    #[cfg(feature = "db-postgres")]
    #[tokio::test]
    #[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
    async fn postgres_parity_llm_settings_round_trip() {
        // Same round-trip as the SQLite test above, against a live
        // Postgres to confirm the SQL shape (`$N` placeholders, the
        // `EXCLUDED.` references, the nullable TEXT + REAL casts) all
        // agree on the two columns added in migration 0008.
        let db = pg_or_skip!();
        let user = db
            .admin()
            .users
            .create("pg-llm@example.com", "PG-LLM", "local", Some(b"h"))
            .await
            .unwrap();
        let scoped = db.for_user(user).preferences();
        let updated = scoped
            .upsert(
                false,
                false,
                None,
                Some("Be terse.".to_string()),
                Some(0.3),
                false,
            )
            .await
            .expect("pg upsert Some");
        assert_eq!(
            updated.additional_instructions.as_deref(),
            Some("Be terse.")
        );
        // Floating-point equality is fragile across encodings; we
        // assert on the bit-pattern via `(v - target).abs() < eps`
        // instead so a future change to `f64` doesn't break the
        // parity check.
        let t = updated.temperature.expect("temperature round-trip");
        assert!((t - 0.3_f32).abs() < 1e-5);
        assert!(!updated.memory_enabled);
        let cleared = scoped
            .upsert(false, false, None, None, None, false)
            .await
            .expect("pg upsert None");
        assert!(cleared.additional_instructions.is_none());
        assert!(cleared.temperature.is_none());
        assert!(!cleared.memory_enabled);
    }
}

// ---- passkeys (scoped) -------------------------------------------------
//
// Plan 4.A: confirm the scoped view's `list` / `delete_all` only
// touches the bound user's rows.

#[cfg(test)]
mod passkeys {
    use super::*;
    use crate::types::NewPasskeyRecord;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_list_filters_by_user() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        // Two passkeys for Alice, one for Bob.
        for i in 0..2 {
            db.admin()
                .passkeys
                .insert(NewPasskeyRecord {
                    id: Uuid::new_v4(),
                    user_id: alice,
                    credential_id: vec![i as u8, 0xaa, 0xbb],
                    public_key: vec![0x10, 0x20, 0x30],
                    counter: 0,
                    transports: "internal".to_string(),
                    aaguid: None,
                })
                .await
                .unwrap();
        }
        db.admin()
            .passkeys
            .insert(NewPasskeyRecord {
                id: Uuid::new_v4(),
                user_id: bob,
                credential_id: vec![0xff, 0xee, 0xdd],
                public_key: vec![0x40, 0x50, 0x60],
                counter: 0,
                transports: "internal".to_string(),
                aaguid: None,
            })
            .await
            .unwrap();
        let alice_keys = db.for_user(alice).passkeys().list().await.unwrap();
        let bob_keys = db.for_user(bob).passkeys().list().await.unwrap();
        assert_eq!(alice_keys.len(), 2);
        assert_eq!(bob_keys.len(), 1);
        assert!(alice_keys.iter().all(|p| p.user_id == alice));
        assert!(bob_keys.iter().all(|p| p.user_id == bob));
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_delete_all_only_removes_bound_users_rows() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        for i in 0..2 {
            db.admin()
                .passkeys
                .insert(NewPasskeyRecord {
                    id: Uuid::new_v4(),
                    user_id: alice,
                    credential_id: vec![i as u8, 0xaa, 0xbb],
                    public_key: vec![0x10, 0x20, 0x30],
                    counter: 0,
                    transports: "internal".to_string(),
                    aaguid: None,
                })
                .await
                .unwrap();
        }
        db.admin()
            .passkeys
            .insert(NewPasskeyRecord {
                id: Uuid::new_v4(),
                user_id: bob,
                credential_id: vec![0xff, 0xee, 0xdd],
                public_key: vec![0x40, 0x50, 0x60],
                counter: 0,
                transports: "internal".to_string(),
                aaguid: None,
            })
            .await
            .unwrap();
        let deleted = db.for_user(alice).passkeys().delete_all().await.unwrap();
        assert_eq!(deleted, 2, "only Alice's two rows are removed");
        let bob_keys = db.for_user(bob).passkeys().list().await.unwrap();
        assert_eq!(bob_keys.len(), 1, "Bob's row must survive");
        let alice_keys = db.for_user(alice).passkeys().list().await.unwrap();
        assert!(alice_keys.is_empty());
    }
}

// ---- sessions (scoped) --------------------------------------------------
//
// Plan 4.A: confirm `delete_all` / `count` only touch the bound
// user's rows. The auth-ceremony operations stay on the unscoped
// repository because they are keyed by `token_hash`.

#[cfg(test)]
mod sessions {
    use super::*;

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn scoped_delete_all_and_count() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        for _ in 0..3 {
            db.admin()
                .sessions
                .create(alice, std::time::Duration::from_secs(60), None, None)
                .await
                .unwrap();
        }
        db.admin()
            .sessions
            .create(bob, std::time::Duration::from_secs(60), None, None)
            .await
            .unwrap();
        let alice_scoped = db.for_user(alice).sessions();
        assert_eq!(alice_scoped.count().await.unwrap(), 3);
        let deleted = alice_scoped.delete_all().await.unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(alice_scoped.count().await.unwrap(), 0);
        // Bob's session is untouched.
        assert_eq!(db.for_user(bob).sessions().count().await.unwrap(), 1);
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
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let docs = db.for_user(user).documents();
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
            None,
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
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_docs = db.for_user(other).documents();
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
    async fn find_by_original_name_session_scoped_and_most_recent_wins() {
        // Pin the new filename-lookup path used by `read_document`
        // when the LLM passes the chat UI attachment filename
        // instead of the UUID.
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let docs = db.for_user(alice).documents();
        let session = Uuid::new_v4();
        let id_old = Uuid::new_v4();
        let id_new = Uuid::new_v4();
        // Two rows, same filename, same session. The new insert
        // wins by `created_at DESC`. We sleep briefly between the
        // two inserts so the schema's `created_at` is strictly
        // different — otherwise both rows end up with the same
        // second-precision timestamp and the SQL tie-break falls
        // on the UUID, which is random for UUIDv4 and makes the
        // test flaky.
        docs.insert(
            id_old,
            session,
            "report.pdf",
            "application/pdf",
            10,
            0,
            Some(1),
            "/tmp/old",
            None,
        )
        .await
        .expect("insert old");
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        docs.insert(
            id_new,
            session,
            "report.pdf",
            "application/pdf",
            20,
            0,
            Some(2),
            "/tmp/new",
            None,
        )
        .await
        .expect("insert new");
        let hit = docs
            .find_by_original_name(session, "report.pdf")
            .await
            .expect("lookup")
            .expect("must hit");
        assert_eq!(
            hit.id, id_new,
            "most recent insert must win (got {:?}, expected {:?})",
            hit.id, id_new
        );

        // Determinism: a second call must return the same row.
        let hit2 = docs
            .find_by_original_name(session, "report.pdf")
            .await
            .expect("lookup 2")
            .expect("must hit");
        assert_eq!(hit.id, hit2.id, "filename lookup must be deterministic");

        // Session scope: the same filename in a DIFFERENT session
        // for the same user must not surface here.
        let other_session = Uuid::new_v4();
        assert!(
            docs.find_by_original_name(other_session, "report.pdf")
                .await
                .expect("lookup")
                .is_none(),
            "filename lookup must be session-scoped"
        );

        // User scope: a different user must never see alice's
        // document via filename, even with the right session id.
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let bob_docs = db.for_user(bob).documents();
        assert!(
            bob_docs
                .find_by_original_name(session, "report.pdf")
                .await
                .expect("lookup")
                .is_none(),
            "filename lookup must be user-scoped"
        );

        // Unknown filename in the right session returns None
        // (the caller converts that to InvalidArguments).
        assert!(docs
            .find_by_original_name(session, "nope.pdf")
            .await
            .expect("lookup")
            .is_none());
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
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let session = Uuid::new_v4();
        let id = Uuid::new_v4();
        db.admin()
            .documents
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
                None,
            )
            .await
            .expect("insert");
        let swept = db
            .admin()
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
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let creds = db.for_user(user).credentials();
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
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let other_creds = db.for_user(other).credentials();
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
/// `NAGENT_TEST_PG_URL` is set . Each test carries
/// `#[ignore]` so the local `cargo test` loop stays fast; the CI
/// Postgres job runs them via `cargo test -- --include-ignored`.
#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_create_user() {
    let db = pg_or_skip!();
    let id = db
        .admin()
        .users
        .create("pg@example.com", "PG", "local", Some(b"h"))
        .await
        .expect("pg create");
    let fetched = db
        .admin()
        .users
        .get_by_id(id)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(fetched.email, "pg@example.com");
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_documents_scoped() {
    let db = pg_or_skip!();
    let user = db
        .admin()
        .users
        .create("pg-doc@example.com", "PG", "local", Some(b"h"))
        .await
        .unwrap();
    let docs = db.for_user(user).documents();
    let session = Uuid::new_v4();
    let id = Uuid::new_v4();
    docs.insert(
        id,
        session,
        "x.txt",
        "text/plain",
        1,
        1,
        None,
        "/tmp/x",
        None,
    )
    .await
    .expect("pg insert");
    assert_eq!(docs.count_for_session(session).await.unwrap(), 1);
    let path = docs.delete(id, session).await.expect("delete");
    assert!(path.is_some());
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_find_by_original_name() {
    let db = pg_or_skip!();
    let user = db
        .admin()
        .users
        .create("pg-fbn@example.com", "PG", "local", Some(b"h"))
        .await
        .unwrap();
    let docs = db.for_user(user).documents();
    let session = Uuid::new_v4();
    docs.insert(
        Uuid::new_v4(),
        session,
        "report.pdf",
        "application/pdf",
        10,
        0,
        Some(1),
        "/tmp/old",
        None,
    )
    .await
    .expect("insert old");
    let id_new = Uuid::new_v4();
    docs.insert(
        id_new,
        session,
        "report.pdf",
        "application/pdf",
        20,
        0,
        Some(2),
        "/tmp/new",
        None,
    )
    .await
    .expect("insert new");
    let hit = docs
        .find_by_original_name(session, "report.pdf")
        .await
        .expect("lookup")
        .expect("hit");
    assert_eq!(hit.id, id_new);
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires NAGENT_TEST_PG_URL; run with --include-ignored in CI"]
async fn postgres_parity_credentials_scoped() {
    let db = pg_or_skip!();
    let user = db
        .admin()
        .users
        .create("pg-cred@example.com", "PG", "local", Some(b"h"))
        .await
        .unwrap();
    let creds = db.for_user(user).credentials();
    let nonce = vec![0xCC; 12];
    creds
        .upsert("svc", &[("k".to_string(), nonce.clone(), vec![1])])
        .await
        .expect("pg upsert");
    let row = creds.fetch("svc", "k").await.expect("fetch").expect("row");
    assert_eq!(row.nonce, nonce);
}

// ---- memories (scoped) ---------------------------------------------------
//
// Plan 1791267136806 §3.1: assert the per-user triple table's `upsert` /
// `recall` / `list_meta` / `forget` paths round-trip the planned SQL
// shape. The encryption itself is exercised end-to-end in the
// `nagent-server` integration tests (where the AES-GCM key is
// available); here we focus on the SQL plumbing.

#[cfg(test)]
mod memories {
    use super::*;

    use crate::types::NewMemoryRequest;

    fn sample(
        subject: &str,
        predicate: &str,
        value_ct: &[u8],
        confidence: f32,
        tags: &str,
    ) -> NewMemoryRequest {
        NewMemoryRequest {
            subject: subject.into(),
            predicate: predicate.into(),
            value_nonce: vec![0xAA; 12],
            value_ciphertext: value_ct.to_vec(),
            notes_nonce: None,
            notes_ciphertext: None,
            tags: tags.into(),
            confidence,
            source_session_id: None,
            source_kind: "user_stated".into(),
        }
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_store_dedup_replaces_existing_row() {
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("dedup@example.com", "Dedup", "local", Some(b"h"))
            .await
            .unwrap();
        let mem = db.for_user(user).memories();
        let id1 = mem
            .upsert(sample("doctor", "name", b"first", 1.0, ""))
            .await
            .expect("first upsert");
        // Second upsert with the same (subject, predicate) REPLACES the row.
        let id2 = mem
            .upsert(sample("doctor", "name", b"second", 0.7, ""))
            .await
            .expect("second upsert");
        assert_eq!(id1, id2, "dedup keeps the existing row id");
        let rows = mem.recall(None, None, None, 64).await.expect("recall");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value_ciphertext, b"second");
        // Confidence from the second call wins.
        assert!((rows[0].confidence - 0.7).abs() < 1e-5);
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_store_isolates_per_user() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        db.for_user(alice)
            .memories()
            .upsert(sample("doctor", "name", b"a-doc", 1.0, ""))
            .await
            .unwrap();
        // Bob's recall must NOT see Alice's row.
        let bob_rows = db
            .for_user(bob)
            .memories()
            .recall(None, None, None, 64)
            .await
            .unwrap();
        assert!(bob_rows.is_empty());
        let alice_rows = db
            .for_user(alice)
            .memories()
            .recall(None, None, None, 64)
            .await
            .unwrap();
        assert_eq!(alice_rows.len(), 1);
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_recall_filters_subject_predicate_and_tag() {
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("filter@example.com", "Filter", "local", Some(b"h"))
            .await
            .unwrap();
        let mem = db.for_user(user).memories();
        mem.upsert(sample("doctor", "name", b"doc-1", 1.0, "medical"))
            .await
            .unwrap();
        mem.upsert(sample("allergy", "type", b"penicillin", 1.0, "medical"))
            .await
            .unwrap();
        mem.upsert(sample("wife", "name", b"Alice", 0.9, "family"))
            .await
            .unwrap();

        // subject LIKE %doctor% → 1 row
        let r = mem.recall(Some("doctor"), None, None, 64).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].subject, "doctor");
        // predicate LIKE %type% → 1 row
        let r = mem.recall(None, Some("type"), None, 64).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].predicate, "type");
        // tags LIKE %family% → 1 row
        let r = mem.recall(None, None, Some("family"), 64).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].subject, "wife");
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_recall_respects_limit_and_orders_by_confidence() {
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("limit@example.com", "Limit", "local", Some(b"h"))
            .await
            .unwrap();
        let mem = db.for_user(user).memories();
        // 12 rows with distinct (subject, predicate) pairs so the
        // dedup upsert doesn't collapse — and with descending
        // confidence so the order check is meaningful. The limit
        // is enforced first (LIMIT 5).
        for i in 0..12 {
            let conf = 1.0 - (i as f32) * 0.05;
            mem.upsert(sample(
                &format!("subject-{i}"),
                "name",
                format!("v-{i}").as_bytes(),
                conf,
                "",
            ))
            .await
            .unwrap();
        }
        let rows = mem.recall(None, None, None, 5).await.unwrap();
        assert_eq!(rows.len(), 5);
        // First five confidences, descending.
        let confidences: Vec<f32> = rows.iter().map(|r| r.confidence).collect();
        let mut sorted = confidences.clone();
        sorted.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert_eq!(confidences, sorted, "recall returns confidence-DESC order");
        // And the bound: a `limit` larger than RECALL_HARD_LIMIT
        // still returns at most RECALL_HARD_LIMIT rows.
        let big = mem.recall(None, None, None, 10_000).await.unwrap();
        assert_eq!(big.len(), crate::memories::RECALL_HARD_LIMIT.min(12));
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_recall_bumps_last_used_at() {
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("recency@example.com", "Recency", "local", Some(b"h"))
            .await
            .unwrap();
        let mem = db.for_user(user).memories();
        mem.upsert(sample("doctor", "name", b"doc", 1.0, ""))
            .await
            .unwrap();
        // First recall: the upsert set last_used_at = NULL, so the
        // row pre-recall has no timestamp.
        let pre = mem
            .list_meta(64)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.subject == "doctor")
            .expect("row");
        assert!(pre.last_used_at.is_none());
        // After a recall the bumped timestamp is set.
        let _ = mem.recall(None, None, None, 64).await.unwrap();
        let post = mem
            .list_meta(64)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.subject == "doctor")
            .expect("row");
        assert!(post.last_used_at.is_some());
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_forget_scoped_to_user() {
        let db = sqlite_db().await;
        let alice = db
            .admin()
            .users
            .create("alice@example.com", "Alice", "local", Some(b"h"))
            .await
            .unwrap();
        let bob = db
            .admin()
            .users
            .create("bob@example.com", "Bob", "local", Some(b"h"))
            .await
            .unwrap();
        let id = db
            .for_user(alice)
            .memories()
            .upsert(sample("doctor", "name", b"x", 1.0, ""))
            .await
            .unwrap();
        // Bob forgetting Alice's row returns Ok(0): the row exists
        // but Bob cannot see / delete it.
        let deleted = db.for_user(bob).memories().forget(id).await.unwrap();
        assert_eq!(deleted, 0);
        // Alice's row is still there.
        let alice_rows = db
            .for_user(alice)
            .memories()
            .recall(None, None, None, 64)
            .await
            .unwrap();
        assert_eq!(alice_rows.len(), 1);
        // Alice can delete her own row.
        let deleted = db.for_user(alice).memories().forget(id).await.unwrap();
        assert_eq!(deleted, 1);
        let after = db
            .for_user(alice)
            .memories()
            .recall(None, None, None, 64)
            .await
            .unwrap();
        assert!(after.is_empty());
    }

    #[cfg(feature = "db-sqlite")]
    #[tokio::test]
    async fn memories_value_round_trip_preserves_bytes() {
        // The repository stores ciphertext verbatim and returns it
        // verbatim; this test pins the bytes so a future change
        // (e.g. accidentally hashing the value) is caught.
        let db = sqlite_db().await;
        let user = db
            .admin()
            .users
            .create("roundtrip@example.com", "RT", "local", Some(b"h"))
            .await
            .unwrap();
        let payload = b"the secret of life is 42, with padding \x00\x01\x02\x03";
        let id = db
            .for_user(user)
            .memories()
            .upsert(sample("answer", "value", payload, 1.0, ""))
            .await
            .unwrap();
        let rows = db
            .for_user(user)
            .memories()
            .recall(None, None, None, 64)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].value_ciphertext, payload);
        assert_eq!(rows[0].value_nonce, vec![0xAA; 12]);
    }
}
