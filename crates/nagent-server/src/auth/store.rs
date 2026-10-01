//! `auth/store` — thin compatibility shim over [`nagent_db::Db`].
//!
//! The legacy `AuthStore` facade was a 500-line match-dispatched enum
//! over every per-domain repository. Plan 4.A replaces it with
//! [`nagent_db::Db`], a single handle that owns every repository.
//!
//! This module keeps the `AuthStore` name as a newtype around `Db`
//! so existing call sites (`AuthStore::connect`, `store.method`)
//! keep compiling while the call sites are progressively migrated
//! to use `nagent_db::Db` directly. Once every site is migrated
//! (per plan 4.A), this module is deleted.
//!
//! New code should depend on [`nagent_db::Db`] directly and reach
//! the per-domain repositories via `db.users`, `db.documents`, etc.

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;
use crate::config::AuthConfig;
use std::ops::Deref;
use uuid::Uuid;

/// Translate a server-side [`AuthConfig`] into the DB-layer
/// [`nagent_db::DbOptions`]. Keeps the DB crate free of TOML /
/// env / CLI concerns — the server is the only component that
/// knows about those.
impl From<&AuthConfig> for nagent_db::DbOptions {
    fn from(cfg: &AuthConfig) -> Self {
        // `connect()` rejects unknown engines with a clear error
        // message, so we fall back to sqlite here purely to keep
        // `DbOptions` constructible for every input (the actual
        // connect call surfaces the real failure).
        let backend =
            nagent_db::DbEngine::parse(&cfg.db.backend).unwrap_or(nagent_db::DbEngine::Sqlite);
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

/// Backwards-compatible wrapper around [`nagent_db::Db`].
///
/// The legacy `AuthStore` was a match-dispatched enum. Plan 4.A
/// collapses the per-engine state into [`nagent_db::Db`]; this
/// newtype preserves the name + the legacy method set so the
/// auth subsystem + CLI + integration tests keep compiling
/// during the migration.
#[derive(Debug, Clone)]
pub struct AuthStore {
    inner: nagent_db::Db,
}

impl Deref for AuthStore {
    type Target = nagent_db::Db;
    fn deref(&self) -> &nagent_db::Db {
        &self.inner
    }
}

impl AuthStore {
    /// Connect to the auth DB described by `cfg` and wrap the
    /// resulting [`nagent_db::Db`] in an `AuthStore`. Backwards-
    /// compatible shim around [`nagent_db::Db::connect`].
    pub async fn connect(cfg: &AuthConfig) -> Result<Self, AuthError> {
        let opts: nagent_db::DbOptions = cfg.into();
        let db = nagent_db::Db::connect(&opts)
            .await
            .map_err(AuthError::from)?;
        Ok(Self { inner: db })
    }

    /// Borrow the underlying [`nagent_db::Db`].
    pub fn db(&self) -> &nagent_db::Db {
        &self.inner
    }

    /// Underlying engine-agnostic pool. Used by integration tests
    /// that need to run raw SQL.
    pub fn pool(&self) -> &AnyPool {
        self.inner.pool()
    }

    // -- Migrations ------------------------------------------------

    pub async fn migrate(&self) -> Result<(), AuthError> {
        self.inner.migrate().await.map_err(AuthError::from)
    }

    pub async fn migration_status(&self) -> Result<MigrationStatus, AuthError> {
        self.inner.migration_status().await.map_err(AuthError::from)
    }

    pub async fn revert_to(&self, target_version: i64) -> Result<(), AuthError> {
        self.inner
            .revert_to(target_version)
            .await
            .map_err(AuthError::from)
    }

    // -- User accessors -------------------------------------------

    pub async fn get_user_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, AuthError> {
        self.inner
            .users
            .get_by_id(id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        self.inner
            .users
            .get_by_email(email)
            .await
            .map_err(AuthError::from)
    }

    pub async fn create_user(
        &self,
        email: &str,
        display_name: &str,
        provider: &str,
        password_hash: Option<&[u8]>,
    ) -> Result<Uuid, AuthError> {
        self.inner
            .users
            .create(email, display_name, provider, password_hash)
            .await
            .map_err(AuthError::from)
    }

    pub async fn set_password(
        &self,
        user_id: Uuid,
        password_hash: &[u8],
    ) -> Result<u64, AuthError> {
        self.inner
            .users
            .set_password(user_id, password_hash)
            .await
            .map_err(AuthError::from)
    }

    pub async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        self.inner
            .users
            .delete(user_id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn delete_user_by_email(&self, email: &str) -> Result<Option<Uuid>, AuthError> {
        self.inner
            .users
            .delete_by_email(email)
            .await
            .map_err(AuthError::from)
    }

    pub async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        self.inner
            .users
            .count_by_provider(provider)
            .await
            .map_err(AuthError::from)
    }

    pub async fn list_users(
        &self,
        provider_prefix: Option<&str>,
    ) -> Result<Vec<AuthUserRecord>, AuthError> {
        self.inner
            .users
            .list(provider_prefix)
            .await
            .map_err(AuthError::from)
    }

    // -- Session accessors ----------------------------------------

    pub async fn create_session(
        &self,
        user_id: Uuid,
        ttl: std::time::Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<SessionRecord, AuthError> {
        self.inner
            .sessions
            .create(user_id, ttl, ip, user_agent)
            .await
            .map_err(AuthError::from)
    }

    pub async fn lookup_session_by_token_hash(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        self.inner
            .sessions
            .lookup_by_token_hash(token_hash)
            .await
            .map_err(AuthError::from)
    }

    pub async fn touch_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<(), AuthError> {
        self.inner
            .sessions
            .touch(token_hash)
            .await
            .map_err(AuthError::from)
    }

    pub async fn delete_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<u64, AuthError> {
        self.inner
            .sessions
            .delete(token_hash)
            .await
            .map_err(AuthError::from)
    }

    pub async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        self.inner
            .sessions
            .delete_for_user(user_id)
            .await
            .map_err(AuthError::from)
    }

    // -- Passkey accessors -----------------------------------------

    pub async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        self.inner
            .passkeys
            .insert(record)
            .await
            .map_err(AuthError::from)
    }

    pub async fn get_passkey_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        self.inner
            .passkeys
            .get_by_credential_id(credential_id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn bump_passkey_counter(
        &self,
        passkey_id: Uuid,
        new_counter: u32,
    ) -> Result<(), AuthError> {
        self.inner
            .passkeys
            .bump_counter(passkey_id, new_counter)
            .await
            .map_err(AuthError::from)
    }

    // -- Audit -----------------------------------------------------

    /// Append an `auth_events` row. Best-effort.
    pub fn record_event(&self, event: NewAuthEvent) {
        self.inner.events.record(event);
    }

    // -- Per-user credentials --------------------------------------

    pub async fn upsert_user_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), AuthError> {
        self.inner
            .credentials
            .upsert(user_id, service_id, fields)
            .await
            .map_err(AuthError::from)
    }

    pub async fn delete_service_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<u64, AuthError> {
        self.inner
            .credentials
            .delete_service(user_id, service_id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn list_configured_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, AuthError> {
        self.inner
            .credentials
            .list_field_keys(user_id, service_id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn fetch_user_credential(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, AuthError> {
        self.inner
            .credentials
            .fetch(user_id, service_id, field_key)
            .await
            .map_err(AuthError::from)
    }

    // -- Per-user preferences --------------------------------------

    pub async fn get_user_preferences(&self, user_id: Uuid) -> Result<UserPreferences, AuthError> {
        self.inner
            .preferences
            .get(user_id)
            .await
            .map_err(AuthError::from)
    }

    pub async fn upsert_user_preferences(
        &self,
        user_id: Uuid,
        share_location_enabled: bool,
        share_timezone_enabled: bool,
    ) -> Result<UserPreferences, AuthError> {
        self.inner
            .preferences
            .upsert(user_id, share_location_enabled, share_timezone_enabled)
            .await
            .map_err(AuthError::from)
    }
}
