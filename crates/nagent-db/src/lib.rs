//! `nagent-db` — per-domain repositories over a shared sqlx pool.
//!
//! Each per-domain module (`users`, `sessions`, `passkeys`, …) owns
//! its SQL and exposes an engine-agnostic enum repository
//! ([`users::Users`], [`sessions::Sessions`], …). The engine-specific
//! implementations live under `pub(crate)` sub-modules so callers
//! outside the crate cannot depend on a single backend.
//!
//! ## Db facade
//!
//! [`Db`] bundles every per-domain repository on top of a shared
//! [`AnyPool`]. Production code talks to a `Db`; the legacy
//! `auth/store.rs::AuthStore` facade in `nagent-server` is a thin
//! wrapper that forwards every call to a `Db`.
//!
//! ## Per-user scoping
//!
//! The user-bound repositories ([`documents::Documents`],
//! [`credentials::Credentials`], [`chat_sessions::ChatSessions`])
//! expose a `for_user(user_id)` helper that returns a scoped view.
//! The scoped view does not have a `user_id` argument on its
//! per-row methods, so the SQL filter cannot be accidentally
//! dropped (plan 4.A S4). Admin / CLI paths keep using the
//! unscoped `Db` repository directly.
//!
//! ## SQL policy
//!
//! SQL is per-engine today (sqlite vs postgres disagree on `?` vs
//! `$N` placeholders, `TEXT` vs `TIMESTAMP` types, and a few SQL
//! features). Each repository keeps its SQL next to the rust code
//! so the next refactor that consolidates dialects does not have
//! to crawl the call graph. We deliberately avoid `sqlx::Any`
//! until a concrete need pushes us there.

pub mod chat_sessions;
pub mod credentials;
pub mod documents;
pub mod error;
pub mod events;
pub mod migrate;
pub mod passkeys;
pub mod pool;
pub mod preferences;
pub mod sessions;
pub mod types;
pub mod users;

#[cfg(any(test, feature = "db-sqlite", feature = "db-postgres"))]
mod tests;

pub use pool::{AnyPool, DbEngine, DbOptions};

// Re-export the row types that the per-domain SQL
// serialises / deserialises so external callers can keep
// importing them from `nagent_db::types` regardless of which
// domain module they came from.
pub use error::Error;
pub use types::{
    AuthUser, AuthUserRecord, DocumentRow, MigrationRow, MigrationStatus, NewAuthEvent,
    NewPasskeyRecord, PasskeyRecord, SessionRecord, SessionSource, SessionTokenHash,
    UserCredentialRow, UserPreferences, SESSION_HASH_BYTES, SESSION_TOKEN_BYTES,
};

/// Engine-agnostic DB handle. Cheap to clone (each repository wraps
/// the same `Arc`-backed [`AnyPool`]).
///
/// Produced by [`Db::connect`]; the legacy `auth/store.rs::AuthStore`
/// facade in `nagent-server` is a thin wrapper around this type.
#[derive(Debug, Clone)]
pub struct Db {
    pool: AnyPool,
    pub users: users::Users,
    pub sessions: sessions::Sessions,
    pub passkeys: passkeys::Passkeys,
    pub events: events::Events,
    pub credentials: credentials::Credentials,
    pub preferences: preferences::Preferences,
    pub documents: documents::Documents,
    pub chat_sessions: chat_sessions::ChatSessions,
}

impl Db {
    /// Connect to the DB described by `opts` and build a fresh
    /// [`Db`] handle. Each per-domain repository is a thin wrapper
    /// over the same pool, so cloning the `Db` is essentially free.
    pub async fn connect(opts: &DbOptions) -> Result<Self, Error> {
        let pool = AnyPool::connect(opts).await?;
        Ok(Self::from_pool(pool))
    }

    /// Build a [`Db`] from an existing [`AnyPool`]. Used by the
    /// integration tests and any code path that already holds a
    /// pool (the `migrate` CLI, the auto-bootstrap).
    pub fn from_pool(pool: AnyPool) -> Self {
        Self {
            users: users::Users::new(&pool),
            sessions: sessions::Sessions::new(&pool),
            passkeys: passkeys::Passkeys::new(&pool),
            events: events::Events::new(&pool),
            credentials: credentials::Credentials::new(&pool),
            preferences: preferences::Preferences::new(&pool),
            documents: documents::Documents::new(&pool),
            chat_sessions: chat_sessions::ChatSessions::new(&pool),
            pool,
        }
    }

    /// Underlying pool. Useful for the few places that need to run
    /// raw SQL (integration tests, one-off admin queries).
    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    /// Engine this DB was built against.
    pub fn engine(&self) -> DbEngine {
        self.pool.engine()
    }

    /// Run the embedded migration set against the pool. Idempotent;
    /// sqlx skips already-applied migrations on the second call.
    pub async fn migrate(&self) -> Result<(), Error> {
        migrate::run(&self.pool).await
    }

    /// Inspect `_sqlx_migrations` and return the applied + pending
    /// split. The CLI uses this to render `migrate status`.
    pub async fn migration_status(&self) -> Result<MigrationStatus, Error> {
        migrate::status(&self.pool).await
    }

    /// Roll back every applied migration with a version strictly
    /// greater than `target_version`.
    pub async fn revert_to(&self, target_version: i64) -> Result<(), Error> {
        migrate::revert_to(&self.pool, target_version).await
    }
}
