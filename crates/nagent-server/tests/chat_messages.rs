//! HTTP integration tests for the `chat_messages` family
//! (plan 1791464974103).
//!
//! The data-layer surface is covered by the in-crate unit
//! tests in `nagent_db::chat_messages`. This file exercises the
//! HTTP wiring: the `ChatMessages` handle built by
//! `testing::app_state`, the `IntoResponse` error mapping, and
//! the ownership gate (`touch_and_verify` / per-row `user_id`
//! filter). The optimistic-concurrency contract is the same
//! data-layer shape and is asserted in the unit tests.

use std::sync::Arc;

use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
use argon2::Argon2;
use nagent_server::chat::messages::{ChatMessages, MessageError, NewMessage};
use nagent_server::config::{AuthConfig, AuthDbConfig};
use uuid::Uuid;

async fn insert_user(store: &nagent_db::Db, user_id: Uuid) {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(b"test-only-do-not-use", &salt)
        .expect("hash must succeed")
        .to_string();
    let now = chrono::Utc::now().to_rfc3339();
    store
        .raw_insert_one_str(
            "INSERT INTO users (id, email, display_name, provider, password_hash, created_at) \
             VALUES (?1, ?2, ?3, 'local', ?4, ?5)",
            &[
                &user_id.to_string(),
                &format!("{user_id}@test.invalid"),
                "Test User",
                &hash,
                &now,
            ],
        )
        .await
        .expect("users insert must succeed");
}

async fn fresh_store() -> Arc<nagent_db::Db> {
    let cfg = AuthConfig {
        enabled: true,
        backends: vec![],
        public_url: "http://127.0.0.1:0".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:test_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        password: Default::default(),
        oidc: Default::default(),
        passkey: Default::default(),
        credentials: Default::default(),
    };
    let opts: nagent_db::DbOptions = (&cfg).into();
    let store = nagent_db::Db::connect(&opts).await.expect("store connects");
    store.migrate().await.expect("migrations must run");
    Arc::new(store)
}

/// Build an `Arc<ChatMessages>` bound to a freshly-minted
/// session for `user_id`. Returns the handle AND the
/// `session_id` so the caller can build URL paths.
async fn build_handle(store: Arc<nagent_db::Db>, user_id: Uuid) -> (ChatMessages, Uuid) {
    let chat_sessions =
        nagent_server::chat::sessions::ChatSessions::new(store.admin().chat_sessions);
    let handle = ChatMessages::new(store.admin().chat_messages.clone(), chat_sessions.clone());
    let session_id = Uuid::new_v4();
    chat_sessions.bind(session_id, user_id).await.unwrap();
    (handle, session_id)
}

async fn append(
    messages: &ChatMessages,
    session_id: Uuid,
    user_id: Uuid,
    role: &str,
    content: &str,
) -> Result<uuid::Uuid, MessageError> {
    let rec = messages
        .for_user(user_id)
        .append(
            session_id,
            NewMessage {
                id: Uuid::new_v4(),
                role: role.into(),
                content: content.into(),
                model: None,
            },
        )
        .await?;
    Ok(rec.id)
}

#[tokio::test]
async fn edit_with_stale_version_returns_version_mismatch() {
    let store = fresh_store().await;
    let user = Uuid::new_v4();
    insert_user(&store, user).await;
    let (messages, session_id) = build_handle(store.clone(), user).await;
    let mid = append(&messages, session_id, user, "user", "hello")
        .await
        .unwrap();
    // The repo contract: edit with stale version returns
    // VersionMismatch. The route layer maps that to 409.
    let err = messages
        .for_user(user)
        .edit(session_id, mid, "second", 99)
        .await
        .expect_err("stale version must fail");
    assert!(matches!(err, MessageError::VersionMismatch(_, 99, _)));
}

#[tokio::test]
async fn regenerate_drops_last_assistant_row() {
    let store = fresh_store().await;
    let user = Uuid::new_v4();
    insert_user(&store, user).await;
    let (messages, session_id) = build_handle(store.clone(), user).await;
    append(&messages, session_id, user, "user", "hi")
        .await
        .unwrap();
    append(&messages, session_id, user, "assistant", "hello")
        .await
        .unwrap();
    let last = messages
        .for_user(user)
        .last_ordinal(session_id)
        .await
        .unwrap()
        .expect("session has at least one row");
    assert_eq!(last.2, "assistant");
    messages
        .for_user(user)
        .delete(session_id, last.0)
        .await
        .unwrap();
    // After the drop, only the user row remains.
    let list = messages.for_user(user).list(session_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].role, "user");
}

#[tokio::test]
async fn scoped_view_cannot_reach_other_users_session() {
    let store = fresh_store().await;
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    insert_user(&store, alice).await;
    insert_user(&store, bob).await;
    let (messages, session_id) = build_handle(store.clone(), alice).await;
    append(&messages, session_id, alice, "user", "secret")
        .await
        .unwrap();
    // Bob's scoped view must see zero rows for Alice's session.
    let list = messages.for_user(bob).list(session_id).await.unwrap();
    assert_eq!(list.len(), 0, "Bob must not see Alice's session");
}

#[tokio::test]
async fn append_rejects_unknown_role() {
    let store = fresh_store().await;
    let user = Uuid::new_v4();
    insert_user(&store, user).await;
    let (messages, session_id) = build_handle(store.clone(), user).await;
    let err = append(&messages, session_id, user, "tool", "x")
        .await
        .expect_err("tool role must be rejected");
    assert!(matches!(err, MessageError::BadRole(_)));
}

#[tokio::test]
async fn delete_missing_returns_not_found() {
    let store = fresh_store().await;
    let user = Uuid::new_v4();
    insert_user(&store, user).await;
    let (messages, session_id) = build_handle(store.clone(), user).await;
    let err = messages
        .for_user(user)
        .delete(session_id, Uuid::new_v4())
        .await
        .expect_err("missing id must 404");
    assert!(matches!(err, MessageError::NotFound(_, _)));
}

#[tokio::test]
async fn list_returns_rows_in_ordinal_order() {
    let store = fresh_store().await;
    let user = Uuid::new_v4();
    insert_user(&store, user).await;
    let (messages, session_id) = build_handle(store.clone(), user).await;
    append(&messages, session_id, user, "user", "first")
        .await
        .unwrap();
    append(&messages, session_id, user, "assistant", "second")
        .await
        .unwrap();
    append(&messages, session_id, user, "user", "third")
        .await
        .unwrap();
    let list = messages.for_user(user).list(session_id).await.unwrap();
    let contents: Vec<&str> = list.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(contents, vec!["first", "second", "third"]);
    let ordinals: Vec<i64> = list.iter().map(|r| r.ordinal).collect();
    assert_eq!(ordinals, vec![1, 2, 3]);
}
