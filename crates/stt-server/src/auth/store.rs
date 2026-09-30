//! `auth/store` — thin facade over the per-domain `db/` repositories.
//!
//! Plan 5.D splits the god repository into per-domain modules
//! ([`crate::db::users`], [`crate::db::sessions`], [`crate::db::passkeys`],
//! [`crate::db::events`], [`crate::db::credentials`],
//! [`crate::db::preferences`]). Each method on `AuthStore` is now a
//! single-line dispatch into the matching repository so the call
//! sites keep working while the SQL lives next to the rest of the
//! `db/` code.
//!
//! The `SqliteStore` / `PgStore` variants are kept so the
//! `match self.store() { AuthStore::Sqlite(s) => … }` pattern used
//! by [`crate::chat::sessions`] and the legacy [`crate::documents::db`]
//! still resolves. Each variant now holds the per-domain
//! repositories rather than the SQL methods themselves; the SQL
//! moved to [`crate::db`] in this commit.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;
use crate::config::AuthConfig;
use crate::db::{chat_sessions::ChatSessions as DbChatSessions, documents::Documents};

// Re-export the shared `AnyPool` enum from `db::pool` under the
// legacy path so external callers (`tests/auth_e2e.rs`,
// `tests/chat_sessions.rs`) keep resolving it through
// `crate::auth::store::AnyPool`.
pub use crate::db::pool::AnyPool;

// ---- Row types ------------------------------------------------------------

/// Public-facing user record. Mirrors the row in the `users` table
/// minus the password hash (which stays inside the store).
#[derive(Debug, Clone)]
pub struct AuthUserRecord {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    /// `password_hash` is exposed only to the password backend —
    /// other callers should not see it (defence in depth; the field
    /// is on the struct so we don't need a second parallel type).
    pub password_hash: Option<Vec<u8>>,
}

/// Parameters for [`AuthStore::insert_passkey`].
#[derive(Debug, Clone)]
pub struct NewPasskeyRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub counter: u32,
    pub transports: String,
    pub aaguid: Option<Vec<u8>>,
}

/// Passkey row. Returned by [`AuthStore::get_passkey_by_credential_id`].
/// `public_key` carries the full `webauthn_rs::Passkey` serialised
/// as JSON — keeping the entire struct (not just the COSE bytes)
/// lets `passkey.rs` reconstruct the `Passkey` without poking at
/// the crate's private `cred` field.
#[derive(Debug, Clone)]
pub struct PasskeyRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub counter: u32,
    pub transports: String,
}

/// Parameters for [`AuthStore::record_event`].
#[derive(Debug, Clone, Default)]
pub struct NewAuthEvent {
    pub user_id: Option<Uuid>,
    pub kind: String,
    pub provider: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// Integration name the audit row is correlated with (set for
    /// `credential_access` / `credential_missing` /
    /// `credential_decrypt_failed`; `None` for the auth subtree).
    /// Added by the `0002_credentials.sql` migration.
    pub target_service: Option<String>,
}

/// Per-user UI preferences fetched via `GET /api/me/preferences`
/// and updated via `PUT /api/me/preferences`. Backed by the
/// `user_preferences` table added in `0006_user_preferences.up.sql`.
#[derive(Debug, Clone)]
pub struct UserPreferences {
    pub share_location_enabled: bool,
    pub share_timezone_enabled: bool,
    pub updated_at: DateTime<Utc>,
}

impl NewAuthEvent {
    /// Build a non-credential audit row (the auth subtree never sets
    /// `target_service`).
    pub fn auth(
        user_id: Option<Uuid>,
        kind: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            user_id,
            kind: kind.into(),
            provider: provider.into(),
            ..Default::default()
        }
    }
}

/// One row from the `user_credentials` table — only the columns the
/// resolver needs to decrypt.
#[derive(Debug, Clone)]
pub struct UserCredentialRow {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

// ---- Re-exports -----------------------------------------------------------

pub use crate::db::migrate::{MigrationRow, MigrationStatus};

// ---- Variants ------------------------------------------------------------

/// SQLite engine arm of [`AuthStore`].
#[derive(Clone, Debug)]
pub struct SqliteStore {
    pub(crate) pool: sqlx::SqlitePool,
    pub(crate) users: crate::db::users::sqlite::SqliteUsers,
    pub(crate) sessions: crate::db::sessions::sqlite::SqliteSessions,
    pub(crate) passkeys: crate::db::passkeys::sqlite::SqlitePasskeys,
    pub(crate) events: crate::db::events::sqlite::SqliteEvents,
    pub(crate) credentials: crate::db::credentials::sqlite::SqliteCredentials,
    pub(crate) preferences: crate::db::preferences::sqlite::SqlitePreferences,
    pub(crate) documents: crate::db::documents::sqlite::SqliteDocuments,
    pub(crate) chat_sessions: crate::db::chat_sessions::sqlite::SqliteChatSessions,
}

/// Postgres engine arm of [`AuthStore`].
#[derive(Clone, Debug)]
pub struct PgStore {
    pub(crate) pool: sqlx::PgPool,
    pub(crate) users: crate::db::users::postgres::PgUsers,
    pub(crate) sessions: crate::db::sessions::postgres::PgSessions,
    pub(crate) passkeys: crate::db::passkeys::postgres::PgPasskeys,
    pub(crate) events: crate::db::events::postgres::PgEvents,
    pub(crate) credentials: crate::db::credentials::postgres::PgCredentials,
    pub(crate) preferences: crate::db::preferences::postgres::PgPreferences,
    pub(crate) documents: crate::db::documents::postgres::PgDocuments,
    pub(crate) chat_sessions: crate::db::chat_sessions::postgres::PgChatSessions,
}

impl SqliteStore {
    pub(crate) async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let pool = crate::db::pool::connect_sqlite_for(cfg).await?;
        Ok(Self {
            users: crate::db::users::sqlite::SqliteUsers::new(pool.clone()),
            sessions: crate::db::sessions::sqlite::SqliteSessions::new(pool.clone()),
            passkeys: crate::db::passkeys::sqlite::SqlitePasskeys::new(pool.clone()),
            events: crate::db::events::sqlite::SqliteEvents::new(pool.clone()),
            credentials: crate::db::credentials::sqlite::SqliteCredentials::new(pool.clone()),
            preferences: crate::db::preferences::sqlite::SqlitePreferences::new(pool.clone()),
            documents: crate::db::documents::sqlite::SqliteDocuments::new(pool.clone()),
            chat_sessions: crate::db::chat_sessions::sqlite::SqliteChatSessions::new(pool.clone()),
            pool,
        })
    }

    pub(crate) fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    /// Public pool handle for legacy callers (`chat/sessions.rs`,
    /// `documents/db.rs`) that still dispatch on
    /// `AuthStore::Sqlite(s).pool()`.
    pub fn pool_handle(&self) -> sqlx::SqlitePool {
        self.pool.clone()
    }
}

impl PgStore {
    pub(crate) async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let pool = crate::db::pool::connect_postgres_for(cfg).await?;
        Ok(Self {
            users: crate::db::users::postgres::PgUsers::new(pool.clone()),
            sessions: crate::db::sessions::postgres::PgSessions::new(pool.clone()),
            passkeys: crate::db::passkeys::postgres::PgPasskeys::new(pool.clone()),
            events: crate::db::events::postgres::PgEvents::new(pool.clone()),
            credentials: crate::db::credentials::postgres::PgCredentials::new(pool.clone()),
            preferences: crate::db::preferences::postgres::PgPreferences::new(pool.clone()),
            documents: crate::db::documents::postgres::PgDocuments::new(pool.clone()),
            chat_sessions: crate::db::chat_sessions::postgres::PgChatSessions::new(pool.clone()),
            pool,
        })
    }

    pub(crate) fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    pub fn pool_handle(&self) -> sqlx::PgPool {
        self.pool.clone()
    }
}

// ---- AuthStore facade ----------------------------------------------------

/// DB-agnostic auth store. Cheap to clone (each variant wraps a
/// sqlx pool which itself is `Arc`-backed).
#[derive(Clone, Debug)]
pub enum AuthStore {
    Sqlite(SqliteStore),
    Postgres(PgStore),
}

impl AuthStore {
    pub async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        match cfg.db.backend.as_str() {
            "sqlite" => SqliteStore::connect(cfg).await.map(AuthStore::Sqlite),
            "postgres" => PgStore::connect(cfg).await.map(AuthStore::Postgres),
            other => Err(AuthError::Internal(format!(
                "unsupported auth.db.backend: {other}"
            ))),
        }
    }

    pub async fn migrate(&self) -> Result<(), AuthError> {
        crate::db::migrate::run(&self.pool()).await
    }

    pub async fn migration_status(&self) -> Result<MigrationStatus, AuthError> {
        crate::db::migrate::status(&self.pool()).await
    }

    pub async fn revert_to(&self, target_version: i64) -> Result<(), AuthError> {
        crate::db::migrate::revert_to(&self.pool(), target_version).await
    }

    pub fn pool(&self) -> AnyPool {
        match self {
            AuthStore::Sqlite(s) => AnyPool::Sqlite(s.pool.clone()),
            AuthStore::Postgres(s) => AnyPool::Postgres(s.pool.clone()),
        }
    }

    // ---- Users ------------------------------------------------------

    pub async fn get_user_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.get_by_id(id).await,
            AuthStore::Postgres(s) => s.users.get_by_id(id).await,
        }
    }

    pub async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.get_by_email(email).await,
            AuthStore::Postgres(s) => s.users.get_by_email(email).await,
        }
    }

    pub async fn create_user(
        &self,
        email: &str,
        display_name: &str,
        provider: &str,
        password_hash: Option<&[u8]>,
    ) -> Result<Uuid, AuthError> {
        match self {
            AuthStore::Sqlite(s) => {
                s.users
                    .create(email, display_name, provider, password_hash)
                    .await
            }
            AuthStore::Postgres(s) => {
                s.users
                    .create(email, display_name, provider, password_hash)
                    .await
            }
        }
    }

    pub async fn set_password(
        &self,
        user_id: Uuid,
        password_hash: &[u8],
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.set_password(user_id, password_hash).await,
            AuthStore::Postgres(s) => s.users.set_password(user_id, password_hash).await,
        }
    }

    pub async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.delete(user_id).await,
            AuthStore::Postgres(s) => s.users.delete(user_id).await,
        }
    }

    pub async fn delete_user_by_email(&self, email: &str) -> Result<Option<Uuid>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.delete_by_email(email).await,
            AuthStore::Postgres(s) => s.users.delete_by_email(email).await,
        }
    }

    pub async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.count_by_provider(provider).await,
            AuthStore::Postgres(s) => s.users.count_by_provider(provider).await,
        }
    }

    pub async fn list_users(
        &self,
        provider_prefix: Option<&str>,
    ) -> Result<Vec<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.list(provider_prefix).await,
            AuthStore::Postgres(s) => s.users.list(provider_prefix).await,
        }
    }

    // ---- Sessions ---------------------------------------------------

    pub async fn create_session(
        &self,
        user_id: Uuid,
        ttl: std::time::Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<SessionRecord, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.create(user_id, ttl, ip, user_agent).await,
            AuthStore::Postgres(s) => s.sessions.create(user_id, ttl, ip, user_agent).await,
        }
    }

    pub async fn lookup_session_by_token_hash(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.lookup_by_token_hash(token_hash).await,
            AuthStore::Postgres(s) => s.sessions.lookup_by_token_hash(token_hash).await,
        }
    }

    pub async fn touch_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.touch(token_hash).await,
            AuthStore::Postgres(s) => s.sessions.touch(token_hash).await,
        }
    }

    pub async fn delete_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.delete(token_hash).await,
            AuthStore::Postgres(s) => s.sessions.delete(token_hash).await,
        }
    }

    pub async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.delete_for_user(user_id).await,
            AuthStore::Postgres(s) => s.sessions.delete_for_user(user_id).await,
        }
    }

    // ---- Passkeys ---------------------------------------------------

    pub async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.passkeys.insert(record).await,
            AuthStore::Postgres(s) => s.passkeys.insert(record).await,
        }
    }

    pub async fn get_passkey_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.passkeys.get_by_credential_id(credential_id).await,
            AuthStore::Postgres(s) => s.passkeys.get_by_credential_id(credential_id).await,
        }
    }

    pub async fn bump_passkey_counter(
        &self,
        passkey_id: Uuid,
        new_counter: u32,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.passkeys.bump_counter(passkey_id, new_counter).await,
            AuthStore::Postgres(s) => s.passkeys.bump_counter(passkey_id, new_counter).await,
        }
    }

    // ---- Audit ------------------------------------------------------

    /// Append an `auth_events` row. Best-effort.
    pub fn record_event(&self, event: NewAuthEvent) {
        match self {
            AuthStore::Sqlite(s) => s.events.record(event),
            AuthStore::Postgres(s) => s.events.record(event),
        }
    }

    // ---- Per-user credentials --------------------------------------

    pub async fn upsert_user_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.credentials.upsert(user_id, service_id, fields).await,
            AuthStore::Postgres(s) => s.credentials.upsert(user_id, service_id, fields).await,
        }
    }

    pub async fn delete_service_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.credentials.delete_service(user_id, service_id).await,
            AuthStore::Postgres(s) => s.credentials.delete_service(user_id, service_id).await,
        }
    }

    pub async fn list_configured_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.credentials.list_field_keys(user_id, service_id).await,
            AuthStore::Postgres(s) => s.credentials.list_field_keys(user_id, service_id).await,
        }
    }

    pub async fn fetch_user_credential(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.credentials.fetch(user_id, service_id, field_key).await,
            AuthStore::Postgres(s) => s.credentials.fetch(user_id, service_id, field_key).await,
        }
    }

    pub async fn get_user_preferences(&self, user_id: Uuid) -> Result<UserPreferences, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.preferences.get(user_id).await,
            AuthStore::Postgres(s) => s.preferences.get(user_id).await,
        }
    }

    pub async fn upsert_user_preferences(
        &self,
        user_id: Uuid,
        share_location_enabled: bool,
        share_timezone_enabled: bool,
    ) -> Result<UserPreferences, AuthError> {
        match self {
            AuthStore::Sqlite(s) => {
                s.preferences
                    .upsert(user_id, share_location_enabled, share_timezone_enabled)
                    .await
            }
            AuthStore::Postgres(s) => {
                s.preferences
                    .upsert(user_id, share_location_enabled, share_timezone_enabled)
                    .await
            }
        }
    }

    // ---- Documents + chat sessions (legacy accessors) ---------------

    pub fn documents(&self) -> Documents {
        match self {
            AuthStore::Sqlite(s) => Documents::Sqlite(s.documents.clone()),
            AuthStore::Postgres(s) => Documents::Postgres(s.documents.clone()),
        }
    }

    pub fn chat_sessions_repo(&self) -> DbChatSessions {
        match self {
            AuthStore::Sqlite(s) => DbChatSessions::Sqlite(s.chat_sessions.clone()),
            AuthStore::Postgres(s) => DbChatSessions::Postgres(s.chat_sessions.clone()),
        }
    }
}
