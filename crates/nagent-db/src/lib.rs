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
//! dropped . Admin / CLI paths keep using the
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

    // -- Raw SQL escape hatch (integration tests only) -------------
    //
    // Production code talks to the per-domain repositories
    // (`db.users.create`, `db.documents.for_user(uid).insert`, …).
    // The integration tests, however, need to assert on schema
    // shape (column counts, audit rows the spawned task wrote
    // behind the scenes) that the typed API does not expose. To
    // keep those tests from importing sqlx directly — which would
    // force the `nagent-server` crate to keep a sqlx dev-dep that
    // mirrors the type the per-domain SQL uses — these helpers
    // route raw SQL through the engine enum and return plain
    // values the tests can assert on.

    /// Run a raw statement (DML or DDL). Used by tests for
    /// schema probes (`COUNT(*)`, `PRAGMA`) and for INSERTs that
    /// bypass the typed `users.create` helper (when the test needs
    /// to seed an arbitrary row shape).
    pub async fn raw_execute(&self, sql: &str) -> Result<(), Error> {
        match &self.pool {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(p) => {
                sqlx::query(sql).execute(p).await.map_err(Error::Database)?;
            }
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(p) => {
                sqlx::query(sql).execute(p).await.map_err(Error::Database)?;
            }
        }
        Ok(())
    }

    /// First column of the first row of `sql`, decoded as `i64`.
    /// Used for row counts, `pragma` ints, etc. Postgres-only
    /// callers must set `params` to a non-empty slice; sqlite uses
    /// positional `?N` markers.
    pub async fn raw_query_scalar_i64(&self, sql: &str) -> Result<i64, Error> {
        match &self.pool {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(p) => {
                let row = sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map_err(Error::Database)?;
                use sqlx::Row;
                row.try_get::<i64, _>(0).map_err(Error::Database)
            }
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(p) => {
                let row = sqlx::query(sql)
                    .fetch_one(p)
                    .await
                    .map_err(Error::Database)?;
                use sqlx::Row;
                row.try_get::<i64, _>(0).map_err(Error::Database)
            }
        }
    }

    /// All rows of `sql` reduced to a `Vec<String>` of their first
    /// column. Used for `SELECT id FROM auth_events WHERE …`
    /// style assertions in the integration tests.
    pub async fn raw_query_text_vec(&self, sql: &str) -> Result<Vec<String>, Error> {
        match &self.pool {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(p) => {
                let rows = sqlx::query(sql)
                    .fetch_all(p)
                    .await
                    .map_err(Error::Database)?;
                use sqlx::Row;
                rows.into_iter()
                    .map(|r| r.try_get::<String, _>(0).map_err(Error::Database))
                    .collect()
            }
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(p) => {
                let rows = sqlx::query(sql)
                    .fetch_all(p)
                    .await
                    .map_err(Error::Database)?;
                use sqlx::Row;
                rows.into_iter()
                    .map(|r| r.try_get::<String, _>(0).map_err(Error::Database))
                    .collect()
            }
        }
    }

    /// Single row `(String, Option<String>)` — enough for the
    /// `kind` / `target_service` audit-row assertions used by the
    /// tests. The postgres variant binds its arguments in order
    /// (Postgres uses `$1`, `$2`, …); sqlite positional `?1`/`?2`
    /// takes them in the same order.
    pub async fn raw_query_one_pair(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<(String, Option<String>), Error> {
        match &self.pool {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(p) => {
                let mut q = sqlx::query(sql);
                for p in params {
                    q = q.bind(*p);
                }
                let row = q.fetch_one(p).await.map_err(Error::Database)?;
                use sqlx::Row;
                Ok((
                    row.try_get::<String, _>(0).map_err(Error::Database)?,
                    row.try_get::<Option<String>, _>(1)
                        .map_err(Error::Database)?,
                ))
            }
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(p) => {
                let mut q = sqlx::query(sql);
                for p in params {
                    q = q.bind(*p);
                }
                let row = q.fetch_one(p).await.map_err(Error::Database)?;
                use sqlx::Row;
                Ok((
                    row.try_get::<String, _>(0).map_err(Error::Database)?,
                    row.try_get::<Option<String>, _>(1)
                        .map_err(Error::Database)?,
                ))
            }
        }
    }

    /// INSERT one row with up to five positional string bindings.
    /// Used by `tests/chat_sessions.rs` to seed a `users` row
    /// without depending on the typed `users.create` helper (the
    /// tests need full control over the column shape).
    pub async fn raw_insert_one_str(&self, sql: &str, params: &[&str]) -> Result<(), Error> {
        match &self.pool {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(p) => {
                let mut q = sqlx::query(sql);
                for p in params {
                    q = q.bind(*p);
                }
                q.execute(p).await.map_err(Error::Database)?;
            }
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(p) => {
                let mut q = sqlx::query(sql);
                for p in params {
                    q = q.bind(*p);
                }
                q.execute(p).await.map_err(Error::Database)?;
            }
        }
        Ok(())
    }
}
