//! Shared DB pool + engine enum.
//!
//! Wraps the two sqlx pools behind a single enum so call sites can
//! write one match instead of two. Heavy work — connection
//! establishment, pool tuning, foreign-key enforcement, journal
//! mode — lives here once so the per-domain repositories only
//! have to think about their SQL.

use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{PgPool, SqlitePool};

use crate::error::Error;

/// Backend enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbEngine {
    Sqlite,
    Postgres,
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

/// DB-pool handle returned by [`connect`] and by the auth store.
#[derive(Debug, Clone)]
pub enum AnyPool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

impl AnyPool {
    pub fn sqlite(&self) -> Option<&SqlitePool> {
        match self {
            AnyPool::Sqlite(p) => Some(p),
            _ => None,
        }
    }
    pub fn postgres(&self) -> Option<&PgPool> {
        match self {
            AnyPool::Postgres(p) => Some(p),
            _ => None,
        }
    }

    /// Engine the pool was built against.
    pub fn engine(&self) -> DbEngine {
        match self {
            AnyPool::Sqlite(_) => DbEngine::Sqlite,
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
            DbEngine::Sqlite => Ok(AnyPool::Sqlite(connect_sqlite(opts).await?)),
            DbEngine::Postgres => Ok(AnyPool::Postgres(connect_postgres(opts).await?)),
        }
    }
}

pub async fn connect_sqlite(opts: &DbOptions) -> Result<SqlitePool, Error> {
    let connect = SqliteConnectOptions::from_str(&opts.url)
        .map_err(|e| Error::Internal(format!("invalid sqlite URL: {e}")))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(opts.max_connections.max(1))
        .connect_with(connect)
        .await?;
    Ok(pool)
}

pub async fn connect_postgres(opts: &DbOptions) -> Result<PgPool, Error> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(opts.max_connections.max(1))
        .connect(&opts.url)
        .await?;
    Ok(pool)
}
