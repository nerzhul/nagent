//! `auth/store` — thin facade over the per-domain `nagent-db` repositories.
//!
//! The row types live in `nagent-db::types` since plan 4.G; the
//! legacy `crate::auth::store::AuthUserRecord` and friends are
//! re-exported from there so existing call sites in this crate
//! keep resolving.

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;
use crate::config::AuthConfig;
use uuid::Uuid;

/// Translate a server-side [`AuthConfig`] into the DB-layer
/// [`nagent_db::DbOptions`]. Keeps the DB crate free of TOML /
/// env / CLI concerns — the server is the only component that
/// knows about those.
impl From<&AuthConfig> for nagent_db::DbOptions {
    fn from(cfg: &AuthConfig) -> Self {
        use nagent_db::DbEngine;
        let backend = match cfg.db.backend.as_str() {
            "sqlite" => DbEngine::Sqlite,
            "postgres" => DbEngine::Postgres,
            _other => DbEngine::Sqlite, // unreachable: connect() rejects unknown engines
        };
        Self {
            backend,
            url: cfg.db.url.clone(),
            max_connections: cfg.db.max_connections,
            auto_migrate: cfg.db.auto_migrate,
        }
    }
}

// Re-export row types from nagent_db so legacy call sites
// (`crate::auth::store::AuthUserRecord`, etc.) keep compiling.
pub use nagent_db::{
    AuthUserRecord, MigrationRow, MigrationStatus, NewAuthEvent, NewPasskeyRecord, PasskeyRecord,
    UserCredentialRow, UserPreferences,
};

// Re-export the shared `AnyPool` enum from `nagent_db::pool` so
// external callers (`tests/auth_e2e.rs`, `tests/chat_sessions.rs`)
// keep resolving it through `crate::auth::store::AnyPool`.
pub use nagent_db::AnyPool;

use nagent_db::{chat_sessions::ChatSessions as DbChatSessions, documents::Documents};

// ---- Row types are re-exported from `nagent_db::types` above. ----

// ---- Variants ------------------------------------------------------------

/// SQLite engine arm of [`AuthStore`].
#[derive(Clone, Debug)]
pub struct SqliteStore {
    pub(crate) pool: sqlx::SqlitePool,
    pub(crate) users: nagent_db::users::sqlite::SqliteUsers,
    pub(crate) sessions: nagent_db::sessions::sqlite::SqliteSessions,
    pub(crate) passkeys: nagent_db::passkeys::sqlite::SqlitePasskeys,
    pub(crate) events: nagent_db::events::sqlite::SqliteEvents,
    pub(crate) credentials: nagent_db::credentials::sqlite::SqliteCredentials,
    pub(crate) preferences: nagent_db::preferences::sqlite::SqlitePreferences,
    pub(crate) documents: nagent_db::documents::sqlite::SqliteDocuments,
    pub(crate) chat_sessions: nagent_db::chat_sessions::sqlite::SqliteChatSessions,
}

/// Postgres engine arm of [`AuthStore`].
#[derive(Clone, Debug)]
pub struct PgStore {
    pub(crate) pool: sqlx::PgPool,
    pub(crate) users: nagent_db::users::postgres::PgUsers,
    pub(crate) sessions: nagent_db::sessions::postgres::PgSessions,
    pub(crate) passkeys: nagent_db::passkeys::postgres::PgPasskeys,
    pub(crate) events: nagent_db::events::postgres::PgEvents,
    pub(crate) credentials: nagent_db::credentials::postgres::PgCredentials,
    pub(crate) preferences: nagent_db::preferences::postgres::PgPreferences,
    pub(crate) documents: nagent_db::documents::postgres::PgDocuments,
    pub(crate) chat_sessions: nagent_db::chat_sessions::postgres::PgChatSessions,
}

impl SqliteStore {
    pub(crate) async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let pool = nagent_db::pool::connect_sqlite(&cfg.into()).await?;
        Ok(Self {
            users: nagent_db::users::sqlite::SqliteUsers::new(pool.clone()),
            sessions: nagent_db::sessions::sqlite::SqliteSessions::new(pool.clone()),
            passkeys: nagent_db::passkeys::sqlite::SqlitePasskeys::new(pool.clone()),
            events: nagent_db::events::sqlite::SqliteEvents::new(pool.clone()),
            credentials: nagent_db::credentials::sqlite::SqliteCredentials::new(pool.clone()),
            preferences: nagent_db::preferences::sqlite::SqlitePreferences::new(pool.clone()),
            documents: nagent_db::documents::sqlite::SqliteDocuments::new(pool.clone()),
            chat_sessions: nagent_db::chat_sessions::sqlite::SqliteChatSessions::new(pool.clone()),
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
        let pool = nagent_db::pool::connect_postgres(&cfg.into()).await?;
        Ok(Self {
            users: nagent_db::users::postgres::PgUsers::new(pool.clone()),
            sessions: nagent_db::sessions::postgres::PgSessions::new(pool.clone()),
            passkeys: nagent_db::passkeys::postgres::PgPasskeys::new(pool.clone()),
            events: nagent_db::events::postgres::PgEvents::new(pool.clone()),
            credentials: nagent_db::credentials::postgres::PgCredentials::new(pool.clone()),
            preferences: nagent_db::preferences::postgres::PgPreferences::new(pool.clone()),
            documents: nagent_db::documents::postgres::PgDocuments::new(pool.clone()),
            chat_sessions: nagent_db::chat_sessions::postgres::PgChatSessions::new(pool.clone()),
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
        nagent_db::migrate::run(&self.pool())
            .await
            .map_err(AuthError::from)
    }

    pub async fn migration_status(&self) -> Result<MigrationStatus, AuthError> {
        nagent_db::migrate::status(&self.pool())
            .await
            .map_err(AuthError::from)
    }

    pub async fn revert_to(&self, target_version: i64) -> Result<(), AuthError> {
        nagent_db::migrate::revert_to(&self.pool(), target_version)
            .await
            .map_err(AuthError::from)
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
            AuthStore::Sqlite(s) => s.users.get_by_id(id).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.users.get_by_id(id).await.map_err(AuthError::from),
        }
    }

    pub async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.get_by_email(email).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.users.get_by_email(email).await.map_err(AuthError::from),
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
            AuthStore::Sqlite(s) => s
                .users
                .create(email, display_name, provider, password_hash)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .users
                .create(email, display_name, provider, password_hash)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn set_password(
        &self,
        user_id: Uuid,
        password_hash: &[u8],
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .users
                .set_password(user_id, password_hash)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .users
                .set_password(user_id, password_hash)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.delete(user_id).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.users.delete(user_id).await.map_err(AuthError::from),
        }
    }

    pub async fn delete_user_by_email(&self, email: &str) -> Result<Option<Uuid>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .users
                .delete_by_email(email)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .users
                .delete_by_email(email)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .users
                .count_by_provider(provider)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .users
                .count_by_provider(provider)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn list_users(
        &self,
        provider_prefix: Option<&str>,
    ) -> Result<Vec<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.users.list(provider_prefix).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.users.list(provider_prefix).await.map_err(AuthError::from),
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
            AuthStore::Sqlite(s) => s
                .sessions
                .create(user_id, ttl, ip, user_agent)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .sessions
                .create(user_id, ttl, ip, user_agent)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn lookup_session_by_token_hash(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .sessions
                .lookup_by_token_hash(token_hash)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .sessions
                .lookup_by_token_hash(token_hash)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn touch_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => Ok(s.sessions.touch(token_hash).await?),
            AuthStore::Postgres(s) => Ok(s.sessions.touch(token_hash).await?),
        }
    }

    pub async fn delete_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.sessions.delete(token_hash).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.sessions.delete(token_hash).await.map_err(AuthError::from),
        }
    }

    pub async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .sessions
                .delete_for_user(user_id)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .sessions
                .delete_for_user(user_id)
                .await
                .map_err(AuthError::from),
        }
    }

    // ---- Passkeys ---------------------------------------------------

    pub async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.passkeys.insert(record).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.passkeys.insert(record).await.map_err(AuthError::from),
        }
    }

    pub async fn get_passkey_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .passkeys
                .get_by_credential_id(credential_id)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .passkeys
                .get_by_credential_id(credential_id)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn bump_passkey_counter(
        &self,
        passkey_id: Uuid,
        new_counter: u32,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .passkeys
                .bump_counter(passkey_id, new_counter)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .passkeys
                .bump_counter(passkey_id, new_counter)
                .await
                .map_err(AuthError::from),
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
            AuthStore::Sqlite(s) => s
                .credentials
                .upsert(user_id, service_id, fields)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .credentials
                .upsert(user_id, service_id, fields)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn delete_service_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .credentials
                .delete_service(user_id, service_id)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .credentials
                .delete_service(user_id, service_id)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn list_configured_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .credentials
                .list_field_keys(user_id, service_id)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .credentials
                .list_field_keys(user_id, service_id)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn fetch_user_credential(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .credentials
                .fetch(user_id, service_id, field_key)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .credentials
                .fetch(user_id, service_id, field_key)
                .await
                .map_err(AuthError::from),
        }
    }

    pub async fn get_user_preferences(&self, user_id: Uuid) -> Result<UserPreferences, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.preferences.get(user_id).await.map_err(AuthError::from),
            AuthStore::Postgres(s) => s.preferences.get(user_id).await.map_err(AuthError::from),
        }
    }

    pub async fn upsert_user_preferences(
        &self,
        user_id: Uuid,
        share_location_enabled: bool,
        share_timezone_enabled: bool,
    ) -> Result<UserPreferences, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s
                .preferences
                .upsert(user_id, share_location_enabled, share_timezone_enabled)
                .await
                .map_err(AuthError::from),
            AuthStore::Postgres(s) => s
                .preferences
                .upsert(user_id, share_location_enabled, share_timezone_enabled)
                .await
                .map_err(AuthError::from),
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
