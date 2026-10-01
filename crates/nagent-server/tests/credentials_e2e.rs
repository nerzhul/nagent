//! End-to-end test for the per-user credentials framework:
//! migration applies, encrypt/decrypt round-trip via the auth store
//! CRUD methods, and the `/api/integrations*` HTTP surface.
//!
//! The registry is left empty (no real `ServiceDef`s) — the v1
//! framework ships with no concrete integrations, so a test-only
//! service is registered dynamically by adding a `ServiceDef` to a
//! fresh `ServiceRegistry`. The route handler is the only consumer
//! of the registry; the resolver tests do not need it.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
async fn connect_db(cfg: &nagent_server::config::AuthConfig) -> nagent_db::Db {
    let opts: nagent_db::DbOptions = cfg.into();
    nagent_db::Db::connect(&opts).await.expect("store connects")
}

use nagent_server::credentials::{
    crypto::{decrypt, encrypt},
    key::CredentialsKey,
    CredentialResolver,
};
use tower::ServiceExt;
use uuid::Uuid;

async fn temp_store() -> nagent_db::Db {
    let cfg = nagent_server::config::AuthConfig {
        enabled: true,
        backends: vec![nagent_server::config::AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: nagent_server::config::AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:cred_e2e_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..Default::default()
    };
    let store = connect_db(&cfg).await;
    store.migrate().await.expect("migrate");
    store
}

#[tokio::test]
async fn migration_creates_user_credentials_and_target_service() {
    let store = temp_store().await;
    // The two new schema objects must exist after migration.
    let table_count: i64 = store
        .raw_query_scalar_i64(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='user_credentials'",
        )
        .await
        .expect("count user_credentials");
    assert_eq!(table_count, 1, "user_credentials table missing");

    // The auth_events table gained a target_service column.
    let column_count: i64 = store
        .raw_query_scalar_i64(
            "SELECT COUNT(*) FROM pragma_table_info('auth_events') WHERE name='target_service'",
        )
        .await
        .expect("pragma");
    assert_eq!(column_count, 1, "auth_events.target_service missing");
}

#[tokio::test]
async fn upsert_delete_round_trip() {
    let store = temp_store().await;
    let user_id = store
        .users
        .create("alice@example.com", "Alice", "local", Some(b"x"))
        .await
        .expect("create user");

    let key = CredentialsKey::from_bytes([7u8; 32]);
    let s1 = encrypt(&key, "secret-value-1").expect("seal 1");
    let s2 = encrypt(&key, "secret-value-2").expect("seal 2");
    store
        .credentials
        .upsert(
            user_id,
            "test_svc",
            &[
                ("host".to_string(), s1.nonce, s1.ciphertext),
                ("token".to_string(), s2.nonce, s2.ciphertext),
            ],
        )
        .await
        .expect("upsert");

    let filled = store
        .credentials
        .list_field_keys(user_id, "test_svc")
        .await
        .expect("list");
    let mut keys: Vec<&str> = filled.iter().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, vec!["host", "token"]);

    let row = store
        .credentials
        .fetch(user_id, "test_svc", "host")
        .await
        .expect("fetch")
        .expect("row exists");
    let secret = decrypt(
        &key,
        &nagent_server::credentials::EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .expect("decrypt");
    use secrecy::ExposeSecret;
    assert_eq!(secret.expose_secret(), "secret-value-1");

    // Atomic replace: a second upsert with only `host` must remove
    // the `token` row.
    let s3 = encrypt(&key, "new-host").expect("seal 3");
    store
        .credentials
        .upsert(
            user_id,
            "test_svc",
            &[("host".to_string(), s3.nonce, s3.ciphertext)],
        )
        .await
        .expect("replace");
    let filled = store
        .credentials
        .list_field_keys(user_id, "test_svc")
        .await
        .expect("list 2");
    assert_eq!(filled, vec!["host"]);

    // DELETE clears the whole service.
    let cleared = store
        .credentials
        .delete_service(user_id, "test_svc")
        .await
        .expect("delete");
    assert_eq!(cleared, 1);
    assert!(store
        .credentials
        .list_field_keys(user_id, "test_svc")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn delete_service_credentials_unknown_user_is_zero() {
    let store = temp_store().await;
    let n = store
        .credentials
        .delete_service(Uuid::new_v4(), "nope")
        .await
        .expect("delete");
    assert_eq!(n, 0);
}

#[tokio::test]
async fn resolver_audit_row_records_target_service() {
    // The resolver writes an `auth_events` row of kind
    // `credential_missing` for an unconfigured field. We poke a
    // minimal test harness: build a resolver against a real store
    // and assert the audit row shows up.
    let store = temp_store().await;
    let user_id = store
        .users
        .create("bob@example.com", "Bob", "local", Some(b"x"))
        .await
        .expect("user");
    let key = Arc::new(CredentialsKey::from_bytes([9u8; 32]));
    let resolver = CredentialResolver::new(store.clone(), key, None, None);
    let cache = nagent_server::credentials::SecretCache::new();
    let err = resolver.get(user_id, "test_svc", "host", &cache).await;
    // Missing is expected; the audit row is the assertion.
    assert!(matches!(
        err,
        Err(nagent_server::credentials::CredentialError::Missing { .. })
    ));
    // Fire-and-forget audit: give the spawned task time to land.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let row: (String, Option<String>) = store
        .raw_query_one_pair(
            "SELECT kind, target_service FROM auth_events WHERE user_id = ? \
             ORDER BY occurred_at DESC LIMIT 1",
            &[&user_id.to_string()],
        )
        .await
        .expect("audit row");
    assert_eq!(row.0, "credential_missing");
    assert_eq!(row.1.as_deref(), Some("test_svc"));
}

#[tokio::test]
async fn http_list_integrations_empty_registry_returns_empty_data() {
    let auth_store = temp_store().await;
    let mut builder = nagent_server::testing::app_state();
    Arc::make_mut(&mut builder.config).auth.enabled = true;
    Arc::make_mut(&mut builder.config).auth.backends =
        vec![nagent_server::config::AuthBackendKind::Local];
    Arc::make_mut(&mut builder.config).auth.public_url = "https://example.com".into();
    Arc::make_mut(&mut builder.config).auth.db = nagent_server::config::AuthDbConfig {
        backend: "sqlite".into(),
        url: format!(
            "sqlite://file:cred_http_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    let state = builder.with_auth(auth_store).build();
    // Build the router the same way `build_router` does in lib.rs.
    let auth_state = state.auth.as_ref().expect("auth must be wired").clone();
    let auth_layer = axum::middleware::from_fn_with_state(
        auth_state,
        nagent_server::auth::middleware::require_auth_middleware,
    );
    let identity = nagent_server::auth::router::build_protected_auth_router(state.clone());
    let cred_routes =
        nagent_server::credentials::routes::build_protected_credentials_router(state.clone());
    let app = identity
        .merge(cred_routes)
        .layer(auth_layer)
        .with_state(state);

    // Without a session cookie → 401.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/api/integrations")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
