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

#![cfg(feature = "auth")]

#[cfg(feature = "auth")]
mod tests {
    use std::time::Duration;

    use stt_server::auth::store::{
        AnyPool, AuthStore, AuthUserRecord, NewAuthEvent, NewPasskeyRecord, PasskeyRecord,
    };
    use stt_server::auth::AuthError;
    use stt_server::config::{AuthBackendKind, AuthConfig, AuthDbConfig};

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
            },
            password: stt_server::config::AuthPasswordConfig::default(),
            oidc: stt_server::config::AuthOidcConfig::default(),
            passkey: stt_server::config::AuthPasskeyConfig::default(),
        }
    }

    /// Run a SQL statement on the underlying pool regardless of which
    /// engine is in use. PR1 only runs against sqlite in the test
    /// suite (postgres would need a test container) but the helper is
    /// kept generic so the same test body can move to a postgres
    /// harness in PR2.
    async fn raw_execute(pool: &AnyPool, sql: &str) -> Result<(), sqlx::Error> {
        match pool {
            AnyPool::Sqlite(p) => sqlx::query(sql).execute(p).await.map(|_| ()),
            AnyPool::Postgres(p) => sqlx::query(sql).execute(p).await.map(|_| ()),
        }
    }

    async fn raw_query_scalar_i64(pool: &AnyPool, sql: &str) -> Result<i64, sqlx::Error> {
        use sqlx::Row;
        match pool {
            AnyPool::Sqlite(p) => {
                let row = sqlx::query(sql).fetch_one(p).await?;
                row.try_get::<i64, _>(0)
            }
            AnyPool::Postgres(p) => {
                let row = sqlx::query(sql).fetch_one(p).await?;
                row.try_get::<i64, _>(0)
            }
        }
    }

    #[tokio::test]
    async fn full_lifecycle_local_user_session_and_passkey() {
        let cfg = test_config();
        let store = AuthStore::connect(&cfg).await.expect("store connects");
        store.migrate().await.expect("migrations apply");
        let pool = store.pool();

        let hash = b"fake-argon2id-blob".to_vec();
        let user_id = store
            .create_user("alice@example.com", "Alice Doe", "local", Some(&hash))
            .await
            .expect("create_user");

        let dup = store
            .create_user("ALICE@example.com", "Alice Other", "local", Some(&hash))
            .await;
        assert!(
            matches!(dup, Err(AuthError::Conflict(_))),
            "duplicate email must return Conflict, got {dup:?}"
        );

        let by_email = store
            .get_user_by_email("Alice@Example.COM")
            .await
            .expect("get_user_by_email")
            .expect("user exists");
        assert_eq!(by_email.id, user_id);
        assert_eq!(by_email.provider, "local");
        assert!(by_email.password_hash.is_some());

        let disabled_at = chrono::Utc::now().to_rfc3339();
        raw_execute(
            &pool,
            &format!(
                "UPDATE users SET disabled_at = '{disabled_at}' WHERE id = '{}'",
                user_id
            ),
        )
        .await
        .expect("disable update");

        let session = store
            .create_session(user_id, Duration::from_secs(60), None, None)
            .await
            .expect("create_session");
        let lookup = store
            .lookup_session(session.id)
            .await
            .expect("lookup_session");
        assert!(lookup.is_none(), "disabled user must not resolve a session");

        raw_execute(
            &pool,
            &format!(
                "UPDATE users SET disabled_at = NULL WHERE id = '{}'",
                user_id
            ),
        )
        .await
        .expect("re-enable update");
        let lookup = store
            .lookup_session(session.id)
            .await
            .expect("lookup_session")
            .expect("session now resolves");
        assert_eq!(lookup.1.id, user_id);

        let cred_id = vec![0xa1u8; 32];
        let public_key = vec![0xb2u8; 64];
        let pk_id = uuid::Uuid::new_v4();
        store
            .insert_passkey(NewPasskeyRecord {
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
            .get_passkey_by_credential_id(&cred_id)
            .await
            .expect("get_passkey_by_credential_id")
            .expect("passkey exists");
        assert_eq!(fetched.user_id, user_id);
        assert_eq!(fetched.counter, 0);

        store
            .bump_passkey_counter(pk_id, 7)
            .await
            .expect("bump_passkey_counter");
        let fetched: PasskeyRecord = store
            .get_passkey_by_credential_id(&cred_id)
            .await
            .expect("get_passkey_by_credential_id")
            .expect("passkey exists");
        assert_eq!(fetched.counter, 7, "counter must advance monotonically");

        let dup = store
            .insert_passkey(NewPasskeyRecord {
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
            matches!(dup, Err(AuthError::Conflict(_))),
            "duplicate credential_id must return Conflict, got {dup:?}"
        );

        store
            .delete_session(session.id)
            .await
            .expect("delete_session");
        assert!(
            store
                .lookup_session(session.id)
                .await
                .expect("lookup_session after delete")
                .is_none(),
            "session must be gone after delete"
        );

        let deleted = store.delete_user(user_id).await.expect("delete_user");
        assert_eq!(deleted, 1, "exactly one row removed");
        assert!(
            store
                .get_user_by_id(user_id)
                .await
                .expect("get_user_by_id after delete")
                .is_none(),
            "user must be gone after delete"
        );
        assert!(
            store
                .get_passkey_by_credential_id(&cred_id)
                .await
                .expect("get_passkey_by_credential_id after delete")
                .is_none(),
            "passkey must cascade-delete with its user"
        );

        store.record_event(NewAuthEvent {
            user_id: None,
            kind: "test_event".into(),
            provider: "test".into(),
            ip: None,
            user_agent: None,
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let count = raw_query_scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM auth_events WHERE kind = 'test_event'",
        )
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
        let store = AuthStore::connect(&cfg).await.expect("store connects");
        store.migrate().await.expect("migrations apply");

        let h = b"hash".to_vec();
        store
            .create_user("a@x.com", "A", "local", Some(&h))
            .await
            .unwrap();
        store
            .create_user("b@x.com", "B", "local", Some(&h))
            .await
            .unwrap();
        store
            .create_user("c@x.com", "C", "oidc:https://idp/", None)
            .await
            .unwrap();
        store
            .create_user("d@x.com", "D", "passkey", None)
            .await
            .unwrap();

        let all: Vec<AuthUserRecord> = store.list_users(None).await.expect("list_users(None)");
        assert_eq!(all.len(), 4, "all users");

        let locals: Vec<AuthUserRecord> = store
            .list_users(Some("local"))
            .await
            .expect("list_users(Some(local))");
        assert_eq!(locals.len(), 2, "only local");
        assert!(locals.iter().all(|u| u.provider == "local"));

        let oidcs: Vec<AuthUserRecord> = store
            .list_users(Some("oidc"))
            .await
            .expect("list_users(Some(oidc))");
        assert_eq!(oidcs.len(), 1);
        assert!(oidcs[0].provider.starts_with("oidc"));
    }

    #[tokio::test]
    async fn last_local_user_count_via_store() {
        let cfg = test_config();
        let store = AuthStore::connect(&cfg).await.expect("store connects");
        store.migrate().await.expect("migrations apply");
        let h = b"hash".to_vec();
        store
            .create_user("a@x.com", "A", "local", Some(&h))
            .await
            .unwrap();
        let count = store.count_users_by_provider("local").await.unwrap();
        assert_eq!(count, 1, "exactly one local user");
        let count_oidc = store.count_users_by_provider("oidc:foo").await.unwrap();
        assert_eq!(count_oidc, 0, "no oidc users");
    }
}
