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
//! [`Db`] owns the shared [`AnyPool`] and exposes two capability-
//! narrowed handles (plan S4):
//!
//! - [`Db::for_user`] returns a [`UserDb`] that owns the scoped
//!   per-user views ([`documents::Documents::for_user`],
//!   [`credentials::Credentials::for_user`],
//!   [`chat_sessions::ChatSessions::for_user`]) and the per-user
//!   repositories that already take `user_id` as a parameter
//!   ([`preferences::Preferences`], [`sessions::Sessions`]).
//!   Handlers and agents receive a `UserDb`; the scoped views do
//!   not have a `user_id` argument on their per-row methods, so
//!   the SQL filter cannot be accidentally dropped.
//! - [`Db::admin`] returns an [`AdminDb`] with every unscoped
//!   repository (`users`, `events`, `passkeys`, the unscoped
//!   `sessions`, …). Production code reaches for it only in the
//!   `nagent-server` CLI subcommands and the documents purge job;
//!   the layering guard (package E) bans the call site everywhere
//!   else.
//!
//! ## Raw SQL escape hatch
//!
//! The integration tests need to assert on schema shape (column
//! counts, audit rows the spawned task wrote behind the scenes)
//! that the typed API does not expose. The [`Db::raw_execute`] /
//! [`Db::raw_query_*`] / [`Db::raw_insert_one_str`] helpers route
//! raw SQL through the engine enum and return plain values the
//! tests can assert on. They run caller-supplied SQL on the live
//! pool and are gated behind the `test-util` cargo feature (plan
//! S10) so production binaries cannot reach them at the type
//! level.
//!
//! ## SQL policy
//!
//! SQL is per-engine today (sqlite vs postgres disagree on `?` vs
//! `$N` placeholders, `TEXT` vs `TIMESTAMP` types, and a few SQL
//! features). Each repository keeps its SQL next to the rust code
//! so the next refactor that consolidates dialects does not have
//! to crawl the call graph. We deliberately avoid `sqlx::Any`
//! until a concrete need pushes us there.

pub mod chat_messages;
pub mod chat_sessions;
pub mod credentials;
pub mod documents;
pub mod error;
pub mod events;
pub mod memories;
pub mod migrate;
pub mod passkeys;
pub mod pool;
pub mod preferences;
pub mod sessions;
pub mod types;
pub mod users;

#[cfg(any(test, feature = "db-sqlite", feature = "db-postgres"))]
mod tests;

pub(crate) use pool::AnyPool;
pub use pool::{DbEngine, DbOptions};

// Re-export the row types that the per-domain SQL
// serialises / deserialises so external callers can keep
// importing them from `nagent_db::types` regardless of which
// domain module they came from.
pub use error::Error;
pub use types::{
    AuthUser, AuthUserRecord, DocumentRow, MemoryMeta, MemoryRow, MigrationRow, MigrationStatus,
    NewAuthEvent, NewMemoryRequest, NewPasskeyRecord, PasskeyRecord, SessionRecord, SessionSource,
    SessionTokenHash, UserCredentialRow, UserPreferences, SESSION_HASH_BYTES, SESSION_TOKEN_BYTES,
};

/// Engine-agnostic DB handle. Cheap to clone (each repository wraps
/// the same `Arc`-backed [`AnyPool`]).
///
/// Produced by [`Db::connect`]; the migration runner and the test
/// suite are the only paths that still hold a `Db` directly —
/// production code goes through one of the two capability-
/// narrowed handles ([`UserDb`] for handlers and agents,
/// [`AdminDb`] for the CLI / purge job).
#[derive(Debug, Clone)]
pub struct Db {
    pub(crate) pool: AnyPool,
}

impl Db {
    /// Connect to the DB described by `opts` and build a fresh
    /// [`Db`] handle. The pool is the only piece of state worth
    /// sharing, so cloning the `Db` is essentially free.
    pub async fn connect(opts: &DbOptions) -> Result<Self, Error> {
        let pool = AnyPool::connect(opts).await?;
        Ok(Self::from_pool(pool))
    }

    /// Build a [`Db`] from an existing [`AnyPool`]. Used by the
    /// integration tests and any code path that already holds a
    /// pool (the `migrate` CLI, the auto-bootstrap).
    pub fn from_pool(pool: AnyPool) -> Self {
        Self { pool }
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

    /// Capability-narrowed handle for code paths that already
    /// resolved a `user_id`. Owns the scoped per-user views
    /// ([`documents::Documents::for_user`],
    /// [`credentials::Credentials::for_user`],
    /// [`chat_sessions::ChatSessions::for_user`]) plus the
    /// per-user repositories that take `user_id` as a parameter
    /// ([`preferences::Preferences`], [`sessions::Sessions`]).
    ///
    /// Handlers and agents receive a `UserDb`; the scoped views
    /// do not have a `user_id` argument on their per-row methods,
    /// so the SQL filter cannot be accidentally dropped.
    pub fn for_user(&self, user_id: uuid::Uuid) -> UserDb {
        UserDb {
            inner: self.clone(),
            user_id,
        }
    }

    /// Capability-narrowed handle for unscoped, cross-user
    /// operations. Production code reaches for it only from the
    /// `nagent-server` CLI subcommands and the documents purge job;
    /// the layering guard (package E) bans the call site anywhere
    /// else.
    pub fn admin(&self) -> AdminDb {
        AdminDb {
            users: users::Users::new(&self.pool),
            sessions: sessions::Sessions::new(&self.pool),
            passkeys: passkeys::Passkeys::new(&self.pool),
            events: events::Events::new(&self.pool),
            credentials: credentials::Credentials::new(&self.pool),
            preferences: preferences::Preferences::new(&self.pool),
            documents: documents::Documents::new(&self.pool),
            chat_sessions: chat_sessions::ChatSessions::new(&self.pool),
            chat_messages: chat_messages::ChatMessages::new(&self.pool),
            memories: memories::Memories::new(&self.pool),
        }
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
    //
    // SECURITY: the helpers run caller-supplied SQL on the live
    // pool. They are gated behind the `test-util` cargo feature
    // (plan S10) so production binaries cannot reach them at the
    // type level. The `nagent-server` dev-dependency enables
    // `test-util` so the integration tests still compile.

    /// Run a raw statement (DML or DDL). Used by tests for
    /// schema probes (`COUNT(*)`, `PRAGMA`) and for INSERTs that
    /// bypass the typed `users.create` helper (when the test needs
    /// to seed an arbitrary row shape).
    #[cfg(feature = "test-util")]
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
    #[cfg(feature = "test-util")]
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
    #[cfg(feature = "test-util")]
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
    #[cfg(feature = "test-util")]
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
    #[cfg(feature = "test-util")]
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

/// Capability-narrowed handle for code paths that already
/// resolved a `user_id`. Built by [`Db::for_user`].
///
/// Owns the **scoped** per-user views (the `for_user` helpers on
/// [`documents::Documents`], [`credentials::Credentials`],
/// [`chat_sessions::ChatSessions`],
/// [`preferences::Preferences`], [`passkeys::Passkeys`],
/// [`sessions::Sessions`]).
///
/// The scoped views do not have a `user_id` argument on their
/// per-row methods, so the SQL filter cannot be accidentally
/// dropped. [`UserDb`] is `Clone` (everything is `Arc`-backed)
/// and is the type handlers / agents carry around; production
/// code never sees a bare [`AdminDb`] (it would mean the caller is
/// about to do something cross-user, which only the CLI /
/// purge job should).
#[derive(Debug, Clone)]
pub struct UserDb {
    inner: Db,
    user_id: uuid::Uuid,
}

impl UserDb {
    /// User this handle is scoped to. Surfaced for tests +
    /// diagnostic logs that need to echo the binding.
    pub fn user_id(&self) -> uuid::Uuid {
        self.user_id
    }

    /// Borrow the underlying [`Db`] for operations that genuinely
    /// need a non-scoped handle (migrations, the raw-SQL test
    /// helpers). Production code should prefer the scoped fields
    /// below; the layering guard (package E) tracks this.
    pub fn db(&self) -> &Db {
        &self.inner
    }

    /// Scoped documents: every per-row method filters by
    /// `user_id` automatically.
    pub fn documents(&self) -> documents::ScopedDocuments {
        documents::Documents::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped credentials: every per-row method filters by
    /// `user_id` automatically.
    pub fn credentials(&self) -> credentials::ScopedCredentials {
        credentials::Credentials::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped chat sessions: every per-row method filters by
    /// `user_id` automatically.
    pub fn chat_sessions(&self) -> chat_sessions::ScopedChatSessions {
        chat_sessions::ChatSessions::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped chat messages: every per-row method filters by
    /// `user_id` automatically (plan 1791464974103). The A3
    /// edit / regenerate flow goes through this view so a
    /// handler holding a scoped `chat_messages()` cannot
    /// accidentally read or mutate another user's messages.
    pub fn chat_messages(&self) -> chat_messages::ScopedChatMessages {
        chat_messages::ChatMessages::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped preferences: `get` / `upsert` filter by `user_id`
    /// automatically. Plan 4.A.
    pub fn preferences(&self) -> preferences::ScopedPreferences {
        preferences::Preferences::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped passkeys: `list` / `delete_all` filter by `user_id`
    /// automatically. Plan 4.A. The auth-ceremony lookups
    /// (`get_by_credential_id`, `bump_counter`) stay on the
    /// unscoped repository because they are keyed by
    /// `credential_id` / `passkey_id`.
    pub fn passkeys(&self) -> passkeys::ScopedPasskeys {
        passkeys::Passkeys::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped sessions: `delete_all` / `count` filter by `user_id`
    /// automatically. Plan 4.A. The auth-ceremony operations
    /// (`lookup_by_token_hash`, `touch`, `delete`) stay on the
    /// unscoped repository because they are keyed by `token_hash`.
    pub fn sessions(&self) -> sessions::ScopedSessions {
        sessions::Sessions::new(&self.inner.pool).for_user(self.user_id)
    }

    /// Scoped memories: `upsert` / `recall` / `list_meta` /
    /// `forget` filter by `user_id` automatically. Plan
    /// 1791267136806 §1.4 (memory row stays encrypted at rest;
    /// the server-side `MemorySource` adapter owns the
    /// `[auth.credentials].key` and decrypts inside the request).
    pub fn memories(&self) -> memories::ScopedMemories {
        memories::Memories::new(&self.inner.pool).for_user(self.user_id)
    }
}

/// Capability-narrowed handle for unscoped, cross-user
/// operations. Built by [`Db::admin`].
///
/// Production code reaches for it only from the `nagent-server`
/// CLI subcommands and the documents purge job; the layering
/// guard (package E) bans the call site anywhere else. Every
/// repository here already takes the scoping argument it needs
/// (`user_id`, `service_id`, etc.) — the unscoped access is the
/// point, not a footgun.
#[derive(Debug, Clone)]
pub struct AdminDb {
    pub users: users::Users,
    pub sessions: sessions::Sessions,
    pub passkeys: passkeys::Passkeys,
    pub events: events::Events,
    pub credentials: credentials::Credentials,
    pub preferences: preferences::Preferences,
    pub documents: documents::Documents,
    pub chat_sessions: chat_sessions::ChatSessions,
    pub chat_messages: chat_messages::ChatMessages,
    pub memories: memories::Memories,
}

impl AdminDb {
    /// Convenience constructor for the migration runner and the
    /// integration tests, which already hold a pool and want to
    /// build a fresh handle without going through [`Db::admin`].
    pub fn from_pool(pool: &AnyPool) -> Self {
        Self {
            users: users::Users::new(pool),
            sessions: sessions::Sessions::new(pool),
            passkeys: passkeys::Passkeys::new(pool),
            events: events::Events::new(pool),
            credentials: credentials::Credentials::new(pool),
            preferences: preferences::Preferences::new(pool),
            documents: documents::Documents::new(pool),
            chat_sessions: chat_sessions::ChatSessions::new(pool),
            chat_messages: chat_messages::ChatMessages::new(pool),
            memories: memories::Memories::new(pool),
        }
    }
}
