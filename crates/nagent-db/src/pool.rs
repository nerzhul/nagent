//! Shared DB pool + engine enum.
//!
//! Wraps the two sqlx pools behind a single enum so call sites can
//! write one match instead of two. Heavy work — connection
//! establishment, pool tuning, foreign-key enforcement, journal
//! mode — lives here once so the per-domain repositories only
//! have to think about their SQL.
//!
//! The per-engine pool types are only compiled when the matching
//! `db-sqlite` / `db-postgres` cargo feature is on (plan 4.A R6).
//! Trying to construct the wrong variant surfaces a clear compile
//! error at the call site instead of linking the unused driver.

use std::str::FromStr;
use std::time::Duration;

use crate::error::Error;

/// Backend enum. `Sqlite` / `Postgres` variants are only present
/// when the matching feature is enabled; callers must therefore
/// gate any branching on `DbEngine::Sqlite` / `DbEngine::Postgres`
/// behind the same `cfg`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbEngine {
    #[cfg(feature = "db-sqlite")]
    Sqlite,
    #[cfg(feature = "db-postgres")]
    Postgres,
}

impl DbEngine {
    pub fn parse(s: &str) -> Result<Self, Error> {
        match s {
            #[cfg(feature = "db-sqlite")]
            "sqlite" => Ok(DbEngine::Sqlite),
            #[cfg(feature = "db-postgres")]
            "postgres" => Ok(DbEngine::Postgres),
            _ => Err(Error::Internal(format!("unsupported auth.db.backend: {s}"))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            #[cfg(feature = "db-sqlite")]
            DbEngine::Sqlite => "sqlite",
            #[cfg(feature = "db-postgres")]
            DbEngine::Postgres => "postgres",
        }
    }
}

impl std::str::FromStr for DbEngine {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Engine-specific DB options. The DB crate is HTTP-agnostic and
/// config-agnostic: the server crate translates its
/// `config::AuthDbConfig` into this struct so the per-domain SQL
/// never has to know about TOML keys or env vars.
#[derive(Debug, Clone)]
pub struct DbOptions {
    pub backend: DbEngine,
    pub url: String,
    pub max_connections: u32,
    pub auto_migrate: bool,
}

/// DB-pool handle returned by [`connect`] and by the [`Db`] facade.
/// The variants are `cfg`-gated so a slim build does not pull in
/// the unused driver.
#[derive(Debug, Clone)]
pub enum AnyPool {
    #[cfg(feature = "db-sqlite")]
    Sqlite(sqlx::SqlitePool),
    #[cfg(feature = "db-postgres")]
    Postgres(sqlx::PgPool),
}

impl AnyPool {
    #[cfg(feature = "db-sqlite")]
    pub fn sqlite(&self) -> Option<&sqlx::SqlitePool> {
        match self {
            AnyPool::Sqlite(p) => Some(p),
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(_) => None,
        }
    }
    #[cfg(feature = "db-postgres")]
    pub fn postgres(&self) -> Option<&sqlx::PgPool> {
        match self {
            AnyPool::Postgres(p) => Some(p),
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(_) => None,
        }
    }

    /// Engine the pool was built against.
    pub fn engine(&self) -> DbEngine {
        match self {
            #[cfg(feature = "db-sqlite")]
            AnyPool::Sqlite(_) => DbEngine::Sqlite,
            #[cfg(feature = "db-postgres")]
            AnyPool::Postgres(_) => DbEngine::Postgres,
        }
    }
}

impl AnyPool {
    /// Build an `AnyPool` directly from a [`DbOptions`]. Used by
    /// callers (e.g. integration tests) that don't have an
    /// `AuthConfig`.
    pub async fn connect(opts: &DbOptions) -> Result<Self, Error> {
        match opts.backend {
            #[cfg(feature = "db-sqlite")]
            DbEngine::Sqlite => Ok(AnyPool::Sqlite(connect_sqlite(opts).await?)),
            #[cfg(feature = "db-postgres")]
            DbEngine::Postgres => Ok(AnyPool::Postgres(connect_postgres(opts).await?)),
        }
    }
}

#[cfg(feature = "db-sqlite")]
pub async fn connect_sqlite(opts: &DbOptions) -> Result<sqlx::SqlitePool, Error> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteSynchronous};
    let connect = SqliteConnectOptions::from_str(&opts.url)
        .map_err(|e| Error::Internal(format!("invalid sqlite URL: {e}")))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(opts.max_connections.max(1))
        .connect_with(connect)
        .await?;
    Ok(pool)
}

#[cfg(feature = "db-postgres")]
pub async fn connect_postgres(opts: &DbOptions) -> Result<sqlx::PgPool, Error> {
    use sqlx::postgres::PgPoolOptions;
    let pool = PgPoolOptions::new()
        .max_connections(opts.max_connections.max(1))
        .connect(&opts.url)
        .await?;
    Ok(pool)
}
