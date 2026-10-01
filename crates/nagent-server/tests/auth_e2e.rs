//! End-to-end test for the auth subsystem (PR1).
//!
//! Exercises the same code paths the CLI + HTTP handlers use, in
//! one place: connect to a per-test sqlite database, run the
//! migrations, create a local user, mint a session, and verify
//! every read path resolves. This is the canary that catches the
//! "all the unit tests are green but the wiring is broken" class
//! of regressions (e.g. a column rename that compiles but breaks
//! the lookup join).
//!
//! We deliberately do NOT spin up the full axum router here —
//! `auth::middleware::tests` already covers the HTTP-side
//! integration (cookie parsing, CSRF, bearer fallback, expiry).
//! This test is about the data layer: the auth store, the
//! migrations, and the cross-table consistency guarantees
//! (passkey cascades, session expiry, etc.).

mod tests {
    use std::time::Duration;

    use nagent_db::{AuthUserRecord, NewAuthEvent, NewPasskeyRecord, PasskeyRecord};
    use nagent_server::config::{AuthBackendKind, AuthConfig, AuthDbConfig};

    async fn connect_db(cfg: &AuthConfig) -> nagent_db::Db {
        let opts: nagent_db::DbOptions = cfg.into();
        nagent_db::Db::connect(&opts).await.expect("store connects")
    }

    fn test_config() -> AuthConfig {
        AuthConfig {
            enabled: true,
            backends: vec![AuthBackendKind::Local],
            public_url: "http://127.0.0.1:0".into(),
            session_ttl_days: 7,
            csrf_header: "x-csrf-token".into(),
            db: AuthDbConfig {
                backend: "sqlite".into(),
                // `mode=memory` keeps the DB in-process; `cache=shared` lets
                // multiple `SqlitePool` connections share the same
                // in-memory database (the unique-per-uuid ensures each test
                // gets a fresh schema).
                url: format!(
                    "sqlite://file:test_{}?mode=memory&cache=shared",
                    uuid::Uuid::new_v4()
                ),
                max_connections: 1,
                auto_migrate: true,
            },
            password: nagent_server::config::AuthPasswordConfig::default(),
            oidc: nagent_server::config::AuthOidcConfig::default(),
            passkey: nagent_server::config::AuthPasskeyConfig::default(),
            credentials: nagent_server::config::AuthCredentialsConfig::default(),
        }
    }

    #[tokio::test]
    async fn full_lifecycle_local_user_session_and_passkey() {
        let cfg = test_config();
        let store = connect_db(&cfg).await;
        store.migrate().await.expect("migrations apply");

        let hash = b"fake-argon2id-blob".to_vec();
        let user_id = store
            .users
            .create("alice@example.com", "Alice Doe", "local", Some(&hash))
            .await
            .expect("create_user");

        let dup = store
            .users
            .create("ALICE@example.com", "Alice Other", "local", Some(&hash))
            .await;
        assert!(
            matches!(dup, Err(nagent_db::Error::Conflict(_))),
            "duplicate email must return Conflict, got {dup:?}"
        );

        let by_email = store
            .users
            .get_by_email("Alice@Example.COM")
            .await
            .expect("get_user_by_email")
            .expect("user exists");
        assert_eq!(by_email.id, user_id);
        assert_eq!(by_email.provider, "local");
        assert!(by_email.password_hash.is_some());

        let disabled_at = chrono::Utc::now().to_rfc3339();
        store
            .raw_execute(&format!(
                "UPDATE users SET disabled_at = '{disabled_at}' WHERE id = '{}'",
                user_id
            ))
            .await
            .expect("disable update");

        let session = store
            .sessions
            .create(user_id, Duration::from_secs(60), None, None)
            .await
            .expect("create_session");
        let lookup = store
            .sessions
            .lookup_by_token_hash(&session.token_hash)
            .await
            .expect("lookup_session");
        assert!(lookup.is_none(), "disabled user must not resolve a session");

        store
            .raw_execute(&format!(
                "UPDATE users SET disabled_at = NULL WHERE id = '{}'",
                user_id
            ))
            .await
            .expect("re-enable update");
        let lookup = store
            .sessions
            .lookup_by_token_hash(&session.token_hash)
            .await
            .expect("lookup_session")
            .expect("session now resolves");
        assert_eq!(lookup.1.id, user_id);

        let cred_id = vec![0xa1u8; 32];
        let public_key = vec![0xb2u8; 64];
        let pk_id = uuid::Uuid::new_v4();
        store
            .passkeys
            .insert(NewPasskeyRecord {
                id: pk_id,
                user_id,
                credential_id: cred_id.clone(),
                public_key: public_key.clone(),
                counter: 0,
                transports: "usb".into(),
                aaguid: None,
            })
            .await
            .expect("insert_passkey");

        let fetched: PasskeyRecord = store
            .passkeys
            .get_by_credential_id(&cred_id)
            .await
            .expect("get_passkey_by_credential_id")
            .expect("passkey exists");
        assert_eq!(fetched.user_id, user_id);
        assert_eq!(fetched.counter, 0);

        store
            .passkeys
            .bump_counter(pk_id, 7)
            .await
            .expect("bump_passkey_counter");
        let fetched: PasskeyRecord = store
            .passkeys
            .get_by_credential_id(&cred_id)
            .await
            .expect("get_passkey_by_credential_id")
            .expect("passkey exists");
        assert_eq!(fetched.counter, 7, "counter must advance monotonically");

        let dup = store
            .passkeys
            .insert(NewPasskeyRecord {
                id: uuid::Uuid::new_v4(),
                user_id,
                credential_id: cred_id.clone(),
                public_key: vec![0x00u8; 64],
                counter: 0,
                transports: String::new(),
                aaguid: None,
            })
            .await;
        assert!(
            matches!(dup, Err(nagent_db::Error::Conflict(_))),
            "duplicate credential_id must return Conflict, got {dup:?}"
        );

        store
            .sessions
            .delete(&session.token_hash)
            .await
            .expect("delete_session");
        assert!(
            store
                .sessions
                .lookup_by_token_hash(&session.token_hash)
                .await
                .expect("lookup_session after delete")
                .is_none(),
            "session must be gone after delete"
        );

        let deleted = store.users.delete(user_id).await.expect("delete_user");
        assert_eq!(deleted, 1, "exactly one row removed");
        assert!(
            store
                .users
                .get_by_id(user_id)
                .await
                .expect("get_user_by_id after delete")
                .is_none(),
            "user must be gone after delete"
        );
        assert!(
            store
                .passkeys
                .get_by_credential_id(&cred_id)
                .await
                .expect("get_passkey_by_credential_id after delete")
                .is_none(),
            "passkey must cascade-delete with its user"
        );

        store.events.record(NewAuthEvent {
            user_id: None,
            kind: "test_event".into(),
            provider: "test".into(),
            ip: None,
            user_agent: None,
            target_service: None,
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count = store
            .raw_query_scalar_i64("SELECT COUNT(*) FROM auth_events WHERE kind = 'test_event'")
            .await
            .expect("count auth_events");
        assert!(
            count >= 1,
            "audit row should have been written, got count = {count}"
        );
    }

    #[tokio::test]
    async fn list_users_filters_by_provider_prefix() {
        let cfg = test_config();
        let store = connect_db(&cfg).await;
        store.migrate().await.expect("migrations apply");

        let h = b"hash".to_vec();
        store
            .users
            .create("a@x.com", "A", "local", Some(&h))
            .await
            .unwrap();
        store
            .users
            .create("b@x.com", "B", "local", Some(&h))
            .await
            .unwrap();
        store
            .users
            .create("c@x.com", "C", "oidc:https://idp/", None)
            .await
            .unwrap();
        store
            .users
            .create("d@x.com", "D", "passkey", None)
            .await
            .unwrap();

        let all: Vec<AuthUserRecord> = store.users.list(None).await.expect("list_users(None)");
        assert_eq!(all.len(), 4, "all users");

        let locals: Vec<AuthUserRecord> = store
            .users
            .list(Some("local"))
            .await
            .expect("list_users(Some(local))");
        assert_eq!(locals.len(), 2, "only local");
        assert!(locals.iter().all(|u| u.provider == "local"));

        let oidcs: Vec<AuthUserRecord> = store
            .users
            .list(Some("oidc"))
            .await
            .expect("list_users(Some(oidc))");
        assert_eq!(oidcs.len(), 1);
        assert!(oidcs[0].provider.starts_with("oidc"));
    }

    #[tokio::test]
    async fn last_local_user_count_via_store() {
        let cfg = test_config();
        let store = connect_db(&cfg).await;
        store.migrate().await.expect("migrations apply");
        let h = b"hash".to_vec();
        store
            .users
            .create("a@x.com", "A", "local", Some(&h))
            .await
            .unwrap();
        let count = store.users.count_by_provider("local").await.unwrap();
        assert_eq!(count, 1, "exactly one local user");
        let count_oidc = store.users.count_by_provider("oidc:foo").await.unwrap();
        assert_eq!(count_oidc, 0, "no oidc users");
    }

    /// Regression test for the "UNIQUE constraint failed: auth_events.id"
    /// warning that surfaced on every server restart under the
    /// pre-`0002_auth_events_uuid.sql` schema (process-local BIGINT
    /// counter that reset to 1 on every restart and collided with
    /// rows from the previous run). `auth_events.id` is now a
    /// UUIDv4 generated in Rust — the DB enforces uniqueness via
    /// the PRIMARY KEY constraint and the value is portable across
    /// sqlite and postgres. This test creates a store, writes a
    /// batch of events, drops it, then creates a fresh store over
    /// the same DB and writes more — exactly mirroring the restart
    /// scenario. Every row must land in the table, the two batches
    /// must be disjoint (no id is reused), and the union must
    /// contain 10 unique UUIDs.
    #[tokio::test]
    async fn record_event_survives_a_process_restart() {
        // On-disk file so both `AuthStore`s share the same DB across
        // the simulated "restart" (an in-memory DB would be torn down
        // when the first store is dropped).
        let path = std::env::temp_dir().join(format!(
            "nagent-auth-events-restart-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let mut cfg = test_config();
        cfg.db.url = url.clone();
        cfg.db.max_connections = 1;

        // First "process": write 5 events.
        let store1 = connect_db(&cfg).await;
        store1.migrate().await.expect("store 1 migrate");
        for i in 0..5 {
            store1.events.record(NewAuthEvent {
                user_id: None,
                kind: format!("first_run_{i}"),
                provider: "test".into(),
                ip: None,
                user_agent: None,
                target_service: None,
            });
        }
        // `AuthStore::record_event` spawns the actual write on the
        // tokio runtime so the HTTP handler is not blocked. Wait
        // briefly for the spawned task to drain — 5 small writes
        // comfortably fit in this budget on every test host we
        // support.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let first_ids: Vec<String> = store1
            .raw_query_text_vec(
                "SELECT id FROM auth_events WHERE kind LIKE 'first_run_%' ORDER BY kind",
            )
            .await
            .unwrap();
        assert_eq!(
            first_ids.len(),
            5,
            "first batch must have produced 5 rows, got {}",
            first_ids.len()
        );
        for id in &first_ids {
            uuid::Uuid::parse_str(id)
                .unwrap_or_else(|_| panic!("auth_events.id must be a UUID, got {id:?}"));
        }
        drop(store1);

        // Second "process": fresh AuthStore. With the pre-0002
        // schema, the first INSERT here would try id=1 and collide.
        // The new UUID-based schema makes every id unique by
        // construction, so all 5 second-batch rows must land and
        // none of them can collide with the first batch.
        let store2 = connect_db(&cfg).await;
        for i in 0..5 {
            store2.events.record(NewAuthEvent {
                user_id: None,
                kind: format!("second_run_{i}"),
                provider: "test".into(),
                ip: None,
                user_agent: None,
                target_service: None,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let second_ids: Vec<String> = store2
            .raw_query_text_vec(
                "SELECT id FROM auth_events WHERE kind LIKE 'second_run_%' ORDER BY kind",
            )
            .await
            .unwrap();
        assert_eq!(
            second_ids.len(),
            5,
            "second batch must have produced 5 rows, got {}",
            second_ids.len()
        );
        // No id may appear twice across the restart — the bug we
        // are regressing against used to violate the PRIMARY KEY
        // constraint on every restart, and `record_event` swallows
        // the error so the symptom was a quiet audit gap.
        let first_set: std::collections::HashSet<&String> = first_ids.iter().collect();
        let second_set: std::collections::HashSet<&String> = second_ids.iter().collect();
        assert!(
            first_set.is_disjoint(&second_set),
            "first and second batches must not share any id, found overlap: {:?}",
            first_set.intersection(&second_set).collect::<Vec<_>>()
        );
        let all_unique: std::collections::HashSet<&String> =
            first_ids.iter().chain(second_ids.iter()).collect();
        assert_eq!(
            all_unique.len(),
            10,
            "all 10 ids must be unique, got {} distinct values",
            all_unique.len()
        );

        let _ = std::fs::remove_file(&path);
    }
}
