//! DB-agnostic user + session storage.
//!
//! The public type is [`AuthStore`], an enum that wraps either a
//! sqlite pool or a postgres pool. Call sites use `match` on the
//! variant — there is no `dyn Trait` indirection so the call graph
//! stays small (important for a hot path like the auth middleware).
//!
//! [`SqliteStore`] and [`PgStore`] live in [`crate::auth::db_sqlite`]
//! and [`crate::auth::db_postgres`] respectively. They both expose
//! the same surface so the variants can be swapped without changing
//! any handler code.
//!
//! All timestamps are stored as RFC 3339 strings in UTC (the Rust
//! code converts with `chrono::{DateTime, Utc}`'s `to_rfc3339()` /
//! `parse_from_rfc3339()`). The migration DDL uses `TEXT NOT NULL
//! DEFAULT CURRENT_TIMESTAMP` so both engines pick a sensible
//! default without a round-trip through the application.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::session::SessionRecord;

pub use crate::auth::db_postgres::PgStore;
pub use crate::auth::db_sqlite::SqliteStore;

/// DB-pool handle returned by [`AuthStore::pool`].
///
/// Lets subsystem code (OIDC state persistence, passkey ceremonies
/// that need to issue additional queries outside the trait surface)
/// run a one-off query without re-plumbing the generic pool type.
#[derive(Debug)]
pub enum AnyPool {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

impl AnyPool {
    pub fn sqlite(&self) -> Option<&sqlx::SqlitePool> {
        match self {
            AnyPool::Sqlite(p) => Some(p),
            _ => None,
        }
    }
    pub fn postgres(&self) -> Option<&sqlx::PgPool> {
        match self {
            AnyPool::Postgres(p) => Some(p),
            _ => None,
        }
    }
}

/// DB-agnostic auth store. Cheap to clone (each variant wraps a
/// sqlx pool which itself is `Arc`-backed).
#[derive(Clone, Debug)]
pub enum AuthStore {
    Sqlite(SqliteStore),
    Postgres(PgStore),
}

impl AuthStore {
    /// Build the store from the runtime config. Returns an error if
    /// the URL fails to parse, the pool cannot connect, or the
    /// migration step fails.
    pub async fn connect(cfg: &crate::config::AuthConfig) -> Result<Self, AuthError> {
        match cfg.db.backend.as_str() {
            "sqlite" => SqliteStore::connect(cfg).await.map(AuthStore::Sqlite),
            "postgres" => PgStore::connect(cfg).await.map(AuthStore::Postgres),
            other => Err(AuthError::Internal(format!(
                "unsupported auth.db.backend: {other}"
            ))),
        }
    }

    /// Run the migrations directory. Idempotent — sqlx tracks which
    /// migrations have already been applied in the `_sqlx_migrations`
    /// table it creates on first run.
    pub async fn migrate(&self) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.migrate().await,
            AuthStore::Postgres(s) => s.migrate().await,
        }
    }

    /// Inspect the `_sqlx_migrations` table and cross-reference it
    /// against the `MIGRATOR` static to surface the applied /
    /// pending split. Returns an empty applied set when the
    /// `_sqlx_migrations` table does not exist yet (fresh DB), so the
    /// `migrate status` CLI prints "all pending" rather than
    /// crashing on a brand-new install.
    pub async fn migration_status(&self) -> Result<MigrationStatus, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.migration_status().await,
            AuthStore::Postgres(s) => s.migration_status().await,
        }
    }

    /// Roll back every applied migration with a version strictly
    /// greater than `target_version`. `target_version` itself stays
    /// applied (sqlx 0.8.6 semantics — see `Migrator::undo`).
    ///
    /// The CLI computes the target from the applied set:
    /// `latest - N` for `--steps N`, or an explicit version for
    /// `--to`. Operators can therefore always inspect the plan with
    /// `migrate status` before running `migrate down`.
    pub async fn revert_to(&self, target_version: i64) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.revert_to(target_version).await,
            AuthStore::Postgres(s) => s.revert_to(target_version).await,
        }
    }

    /// Return a `AnyPool` handle for subsystem code that needs to
    /// run a one-off SQL query outside the trait surface (OIDC
    /// state persistence, etc.).
    pub fn pool(&self) -> AnyPool {
        match self {
            AuthStore::Sqlite(s) => AnyPool::Sqlite(s.pool().clone()),
            AuthStore::Postgres(s) => AnyPool::Postgres(s.pool().clone()),
        }
    }

    /// Look up a user by id. Returns `None` if the user does not
    /// exist or has been soft-disabled.
    pub async fn get_user_by_id(&self, id: Uuid) -> Result<Option<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.get_user_by_id(id).await,
            AuthStore::Postgres(s) => s.get_user_by_id(id).await,
        }
    }

    /// Look up a user by email (case-insensitive on both engines
    /// via the LOWER() comparison). Returns the active user only;
    /// soft-disabled users are excluded.
    pub async fn get_user_by_email(
        &self,
        email: &str,
    ) -> Result<Option<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.get_user_by_email(email).await,
            AuthStore::Postgres(s) => s.get_user_by_email(email).await,
        }
    }

    /// Create a new user. `password_hash` is `Some(bytes)` for the
    /// local backend and `None` for OIDC / passkey-only users.
    /// Fails with [`AuthError::Conflict`] if the email already
    /// exists.
    pub async fn create_user(
        &self,
        email: &str,
        display_name: &str,
        provider: &str,
        password_hash: Option<&[u8]>,
    ) -> Result<Uuid, AuthError> {
        match self {
            AuthStore::Sqlite(s) => {
                s.create_user(email, display_name, provider, password_hash)
                    .await
            }
            AuthStore::Postgres(s) => {
                s.create_user(email, display_name, provider, password_hash)
                    .await
            }
        }
    }

    /// Update the local password hash for an existing user.
    /// Returns the updated row count.
    pub async fn set_password(
        &self,
        user_id: Uuid,
        password_hash: &[u8],
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.set_password(user_id, password_hash).await,
            AuthStore::Postgres(s) => s.set_password(user_id, password_hash).await,
        }
    }

    /// Delete a user by id. Cascades to their sessions and passkeys
    /// via the FK constraints in the migration. Returns the number
    /// of rows removed (0 means the user did not exist).
    pub async fn delete_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.delete_user(user_id).await,
            AuthStore::Postgres(s) => s.delete_user(user_id).await,
        }
    }

    /// Delete a user by email. Returns the deleted user id if any.
    pub async fn delete_user_by_email(&self, email: &str) -> Result<Option<Uuid>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.delete_user_by_email(email).await,
            AuthStore::Postgres(s) => s.delete_user_by_email(email).await,
        }
    }

    /// Count users by provider (e.g. `"local"`, `"oidc:<issuer>"`).
    /// Used by the `auth delete-user` CLI to refuse removing the
    /// last local user when auth is enabled.
    pub async fn count_users_by_provider(&self, provider: &str) -> Result<i64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.count_users_by_provider(provider).await,
            AuthStore::Postgres(s) => s.count_users_by_provider(provider).await,
        }
    }

    /// List all users, optionally filtered by `provider` prefix.
    /// Used by `auth list-users`. PR1 returns the bare minimum for
    /// operator auditing (id, email, provider, created_at).
    pub async fn list_users(
        &self,
        provider_prefix: Option<&str>,
    ) -> Result<Vec<AuthUserRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.list_users(provider_prefix).await,
            AuthStore::Postgres(s) => s.list_users(provider_prefix).await,
        }
    }

    // ---- Sessions ----------------------------------------------------

    /// Create a fresh session row. Mints the opaque session token
    /// AND the CSRF token internally and returns the new
    /// `SessionRecord` (with `token_hash` for subsequent lookups
    /// AND `csrf_token`).
    /// Security plan #7: the **plaintext token is returned by the
    /// call site** (see [`crate::auth::password::login_handler`]
    /// AND [`crate::auth::passkey::finish_ceremony_handler`]) so
    /// the caller can set the cookie AND the JSON body. The store
    /// itself never holds the plaintext.
    pub async fn create_session(
        &self,
        user_id: Uuid,
        ttl: std::time::Duration,
        ip: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<SessionRecord, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.create_session(user_id, ttl, ip, user_agent).await,
            AuthStore::Postgres(s) => s.create_session(user_id, ttl, ip, user_agent).await,
        }
    }

    /// Look up a session by the SHA-256 hash of its opaque token
    /// and join the user row in a single round-trip. Returns
    /// `Ok(None)` if the session does not exist or has expired
    /// (`expires_at <= now`). Returns `Ok(Some((session, user)))`
    /// on a hit.
    ///
    /// Security plan #7: the plaintext token is never reflected
    /// back from the database — callers hand in the SHA-256 hash
    /// (which they computed from the cookie / bearer header) and
    /// get the matching row back. A DB leak no longer yields
    /// usable session ids.
    pub async fn lookup_session_by_token_hash(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<Option<(SessionRecord, AuthUserRecord)>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.lookup_session_by_token_hash(token_hash).await,
            AuthStore::Postgres(s) => s.lookup_session_by_token_hash(token_hash).await,
        }
    }

    /// Update `last_seen_at` to `now`. This is fire-and-forget
    /// debug data — the absolute expiry is never extended (plan D6a).
    /// Returns `Ok(())` if the session no longer exists (silently
    /// skipped so a slow background task cannot resurrect a session
    /// that was just deleted).
    pub async fn touch_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.touch_session(token_hash).await,
            AuthStore::Postgres(s) => s.touch_session(token_hash).await,
        }
    }

    /// Delete a session by its token hash. Returns the deleted row
    /// count (0 means the session was already gone).
    pub async fn delete_session(
        &self,
        token_hash: &crate::auth::session::SessionTokenHash,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.delete_session(token_hash).await,
            AuthStore::Postgres(s) => s.delete_session(token_hash).await,
        }
    }

    /// Delete every session owned by `user_id`. Used by the
    /// `DELETE /api/auth/sessions` (PR2) and the CLI's
    /// "logout everywhere" path. PR1 exposes this on the route
    /// only for the user's own sessions.
    pub async fn delete_sessions_for_user(&self, user_id: Uuid) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.delete_sessions_for_user(user_id).await,
            AuthStore::Postgres(s) => s.delete_sessions_for_user(user_id).await,
        }
    }

    // ---- Passkeys ----------------------------------------------------

    /// Persist a newly registered passkey. Returns `Conflict` if the
    /// `credential_id` is already taken (the authenticator
    /// generated a colliding id, which is astronomically unlikely
    /// for the algorithm but worth checking).
    pub async fn insert_passkey(&self, record: NewPasskeyRecord) -> Result<Uuid, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.insert_passkey(record).await,
            AuthStore::Postgres(s) => s.insert_passkey(record).await,
        }
    }

    /// Look up a passkey by its raw credential id bytes.
    pub async fn get_passkey_by_credential_id(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<PasskeyRecord>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.get_passkey_by_credential_id(credential_id).await,
            AuthStore::Postgres(s) => s.get_passkey_by_credential_id(credential_id).await,
        }
    }

    /// Increment the passkey counter after a successful assertion.
    /// The WebAuthn spec requires this monotonic check to detect
    /// cloned authenticators.
    pub async fn bump_passkey_counter(
        &self,
        passkey_id: Uuid,
        new_counter: u32,
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.bump_passkey_counter(passkey_id, new_counter).await,
            AuthStore::Postgres(s) => s.bump_passkey_counter(passkey_id, new_counter).await,
        }
    }

    // ---- Audit -------------------------------------------------------

    /// Append an `auth_events` row. Best-effort — a write failure is
    /// logged but never propagated to the caller (the audit row
    /// would block a successful login). Spawned on the runtime so
    /// the caller does not have to await the write.
    pub fn record_event(&self, event: NewAuthEvent) {
        let this = self.clone();
        tokio::spawn(async move {
            match this {
                AuthStore::Sqlite(s) => s.record_event(event).await,
                AuthStore::Postgres(s) => s.record_event(event).await,
            }
        });
    }

    // ---- Per-user credentials --------------------------------------

    /// Atomic upsert of all `(field_key, nonce, ciphertext)` triples
    /// for one `(user_id, service_id)`. Implemented as
    /// `DELETE THEN INSERT` inside a single transaction so partial
    /// writes never leave a service half-configured (the
    /// `UNIQUE (user_id, service_id, field_key)` would otherwise
    /// reject a duplicate insert with a 409 we would have to map
    /// manually).
    ///
    /// `fields` is the new full set — the caller is responsible for
    /// pre-validating unknown `field_key`s against the
    /// `ServiceRegistry` and for encrypting the plaintexts with the
    /// `CredentialsKey` before calling.
    pub async fn upsert_user_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
        fields: &[(String, Vec<u8>, Vec<u8>)],
    ) -> Result<(), AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.upsert_user_credentials(user_id, service_id, fields).await,
            AuthStore::Postgres(s) => s.upsert_user_credentials(user_id, service_id, fields).await,
        }
    }

    /// Delete every `(field_key)` row for one `(user_id, service_id)`.
    /// Returns the number of fields cleared.
    pub async fn delete_service_credentials(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<u64, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.delete_service_credentials(user_id, service_id).await,
            AuthStore::Postgres(s) => s.delete_service_credentials(user_id, service_id).await,
        }
    }

    /// List the `field_key`s the user has configured for a service.
    /// Used by `GET /api/integrations/:id` to compute the per-field
    /// `filled` boolean without exposing the ciphertexts.
    pub async fn list_configured_field_keys(
        &self,
        user_id: Uuid,
        service_id: &str,
    ) -> Result<Vec<String>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => s.list_configured_field_keys(user_id, service_id).await,
            AuthStore::Postgres(s) => s.list_configured_field_keys(user_id, service_id).await,
        }
    }

    /// Fetch the encrypted blob for one `(user_id, service, field)`.
    /// `None` if no row exists. The resolver decrypts with the
    /// server-side `CredentialsKey`.
    pub async fn fetch_user_credential(
        &self,
        user_id: Uuid,
        service_id: &str,
        field_key: &str,
    ) -> Result<Option<UserCredentialRow>, AuthError> {
        match self {
            AuthStore::Sqlite(s) => {
                s.fetch_user_credential(user_id, service_id, field_key)
                    .await
            }
            AuthStore::Postgres(s) => {
                s.fetch_user_credential(user_id, service_id, field_key)
                    .await
            }
        }
    }
}

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

/// One row of the migration status. `description` is the free-text
/// label after the version prefix in the migration filename
/// (`0001_init` → `"init"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRow {
    pub version: i64,
    pub description: String,
}

/// Snapshot of the migration state at one point in time. Returned by
/// [`AuthStore::migration_status`] and serialised by the
/// `stt-server migrate status` CLI (`--json`).
///
/// `applied` reflects `_sqlx_migrations` rows with `success = 1`,
/// `pending` is the set of known migrations (from the
/// `sqlx::migrate!` static) that are NOT in `applied`. `highest_applied`
/// is `None` on a fresh DB where `_sqlx_migrations` does not exist
/// yet — every migration is then `pending`.
#[derive(Debug, Clone, Default)]
pub struct MigrationStatus {
    pub applied: Vec<MigrationRow>,
    pub pending: Vec<MigrationRow>,
    pub highest_applied: Option<i64>,
}

impl MigrationStatus {
    /// True when at least one migration is pending.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// True when no migration is applied (fresh DB).
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}
