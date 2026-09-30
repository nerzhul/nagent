//! End-to-end tests for the new `stt-server migrate` namespace + the
//! migration primitives on [`AuthStore`].
//!
//! The CLI parsing logic is unit-tested in `migrate_cli.rs`'s
//! inline `tests` module; this file drives the same code paths the
//! CLI does at runtime: `AuthStore::migrate()`,
//! `AuthStore::migration_status()`, and `AuthStore::revert_to()`. The
//! sqlite per-test isolation mirrors `auth_e2e.rs:34-45` exactly so
//! we share the cache=shared trick (sqlite's shared in-memory pool
//! needs a UUID-suffixed name to avoid collision across tests).
//!
//! Each test uses a fresh UUID-suffixed DB so `cache=shared` does
//! NOT leak state between cases. The migration order matters: every
//! test that reverts past a migration expects the dependent tables /
//! indexes to be gone.

mod tests {
    use stt_server::auth::store::AuthStore;
    use stt_server::config::{AuthBackendKind, AuthConfig, AuthDbConfig};

    fn test_config(label: &str) -> AuthConfig {
        AuthConfig {
            enabled: true,
            backends: vec![AuthBackendKind::Local],
            public_url: "http://127.0.0.1:0".into(),
            session_ttl_days: 7,
            csrf_header: "x-csrf-token".into(),
            db: AuthDbConfig {
                backend: "sqlite".into(),
                url: format!(
                    "sqlite://file:migrate_cli_{label}_{}?mode=memory&cache=shared",
                    uuid::Uuid::new_v4()
                ),
                max_connections: 1,
                auto_migrate: true,
            },
            password: stt_server::config::AuthPasswordConfig::default(),
            oidc: stt_server::config::AuthOidcConfig::default(),
            passkey: stt_server::config::AuthPasskeyConfig::default(),
            credentials: stt_server::config::AuthCredentialsConfig::default(),
        }
    }

    #[tokio::test]
    async fn status_fresh_db_lists_all_as_pending() {
        // No `_sqlx_migrations` table yet → the helper swallows the
        // "no such table" error and returns an empty applied set.
        // `pending` must contain every known migration.
        let cfg = test_config("fresh");
        let store = AuthStore::connect(&cfg).await.expect("store");
        let status = store.migration_status().await.expect("status");
        assert!(
            status.applied.is_empty(),
            "fresh DB must have no applied migrations; got {:?}",
            status.applied
        );
        assert!(
            status.highest_applied.is_none(),
            "fresh DB must report highest_applied = None"
        );
        assert!(
            !status.pending.is_empty(),
            "fresh DB must have pending migrations"
        );
        // Sanity: the known migrations are pending.
        let pending_versions: Vec<i64> = status.pending.iter().map(|r| r.version).collect();
        assert!(pending_versions.contains(&1));
        assert!(pending_versions.contains(&2));
        assert!(
            pending_versions.contains(&3),
            "the documents migration must be pending too"
        );
    }

    #[tokio::test]
    async fn migrate_up_is_idempotent() {
        let cfg = test_config("idem");
        let store = AuthStore::connect(&cfg).await.expect("store");
        store.migrate().await.expect("first migrate");
        let after_first = store.migration_status().await.unwrap();
        assert_eq!(after_first.highest_applied, Some(4));
        assert!(after_first.pending.is_empty());

        // Second call must be a no-op — sqlx skips applied migrations.
        store.migrate().await.expect("second migrate");
        let after_second = store.migration_status().await.unwrap();
        assert_eq!(after_second.highest_applied, Some(4));
        assert_eq!(after_second.pending.len(), after_first.pending.len());
    }

    #[tokio::test]
    async fn status_after_apply_lists_both_migrations() {
        let cfg = test_config("both");
        let store = AuthStore::connect(&cfg).await.expect("store");
        store.migrate().await.expect("migrate");
        let status = store.migration_status().await.unwrap();
        assert_eq!(status.applied.len(), 4);
        assert!(status.pending.is_empty());
        assert_eq!(status.highest_applied, Some(4));
        let versions: Vec<i64> = status.applied.iter().map(|r| r.version).collect();
        assert!(versions.contains(&1));
        assert!(versions.contains(&2));
        assert!(versions.contains(&3));
        assert!(versions.contains(&4));
    }

    #[tokio::test]
    async fn revert_to_un_does_one_step() {
        // Apply all, then revert to version 3 (keeping {1,2,3} applied).
        // The .down.sql for 0004 must drop `chat_sessions` +
        // its indexes.
        let cfg = test_config("revert_one");
        let store = AuthStore::connect(&cfg).await.expect("store");
        store.migrate().await.expect("migrate");
        store.revert_to(3).await.expect("revert_to(3)");
        let status = store.migration_status().await.unwrap();
        assert_eq!(status.highest_applied, Some(3));
        assert_eq!(status.applied.len(), 3);
        assert_eq!(status.applied[0].version, 1);
        assert_eq!(status.applied[1].version, 2);
        assert_eq!(status.applied[2].version, 3);
        assert_eq!(status.pending.len(), 1);
        assert_eq!(status.pending[0].version, 4);
    }

    #[tokio::test]
    async fn revert_to_zero_un_does_all_steps() {
        // Apply all, then revert to 0 (sqlx semantics: undo every
        // applied migration). After this, status must report zero
        // applied, three pending.
        let cfg = test_config("revert_all");
        let store = AuthStore::connect(&cfg).await.expect("store");
        store.migrate().await.expect("migrate");
        store.revert_to(0).await.expect("revert_to(0)");
        let status = store.migration_status().await.unwrap();
        assert!(
            status.highest_applied.is_none(),
            "all migrations must be reverted"
        );
        assert!(status.applied.is_empty());
        assert_eq!(status.pending.len(), 4);
    }

    #[tokio::test]
    async fn revert_then_reapply_round_trip() {
        // Apply → revert 0002 → re-apply → status must match the
        // original post-apply state (pending = 0, applied = {1, 2, 3}).
        // Catches the "checksum mismatch after re-applying the same
        // SQL" class of regressions.
        let cfg = test_config("roundtrip");
        let store = AuthStore::connect(&cfg).await.expect("store");
        store.migrate().await.expect("migrate");
        store.revert_to(3).await.expect("revert");
        store.migrate().await.expect("re-apply");
        let status = store.migration_status().await.unwrap();
        assert_eq!(status.applied.len(), 4);
        assert!(status.pending.is_empty());
        assert_eq!(status.highest_applied, Some(4));
    }
}
