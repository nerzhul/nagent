//! Shared DB pool + engine enum.
//!
//! Wraps the two sqlx pools behind a single enum so call sites can
//! write one match instead of two (the same pattern used by the
//! legacy `AuthStore`). Heavy work — connection establishment, pool
//! tuning, foreign-key enforcement, journal mode — lives here once
//! so the per-domain repositories only have to think about their
//! SQL.
//!
//! [`AnyPool`] is what every repository receives; it implements
//! [`From`] conversions for the two underlying pools so per-domain
//! code can downcast to the engine it needs.

use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{PgPool, SqlitePool};
use std::str::FromStr;

use crate::auth::error::AuthError;
use crate::config::AuthConfig;

/// Backend enum. Matches the value of `auth.db.backend` in the
/// runtime config (`sqlite` or `postgres`). New backends land here
/// first; the per-domain SQL follows in the same commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbEngine {
    Sqlite,
    Postgres,
}

/// DB-pool handle returned by [`connect_from_auth_config`] and by
/// [`crate::auth::store::AuthStore::pool`].
///
/// Lets subsystem code (OIDC state persistence, passkey ceremonies
/// that need to issue additional queries outside the trait surface)
/// run a one-off query without re-plumbing the generic pool type.
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

    /// Engine the pool was built against. Mirrors the `backend`
    /// string in the runtime config.
    pub fn engine(&self) -> DbEngine {
        match self {
            AnyPool::Sqlite(_) => DbEngine::Sqlite,
            AnyPool::Postgres(_) => DbEngine::Postgres,
        }
    }
}

/// Connect to the configured backend. Returns an error when the URL
/// fails to parse, the pool cannot connect, or `auth.db.backend`
/// names an unsupported engine.
///
/// Engine-specific options (sqlite WAL mode + foreign-key
/// enforcement, postgres TLS, …) are applied here so the
/// repositories do not have to repeat the tuning.
pub async fn connect_from_auth_config(cfg: &AuthConfig) -> Result<AnyPool, AuthError> {
    match cfg.db.backend.as_str() {
        "sqlite" => Ok(AnyPool::Sqlite(connect_sqlite(cfg).await?)),
        "postgres" => Ok(AnyPool::Postgres(connect_postgres(cfg).await?)),
        other => Err(AuthError::Internal(format!(
            "unsupported auth.db.backend: {other}"
        ))),
    }
}

/// Build a [`SqlitePool`] from an [`AuthConfig`] without dispatching
/// on `cfg.db.backend`. Used by [`crate::auth::store::SqliteStore`]
/// when the caller already knows the engine.
pub(crate) async fn connect_sqlite_for(cfg: &AuthConfig) -> Result<SqlitePool, AuthError> {
    connect_sqlite(cfg).await
}

/// Build a [`PgPool`] from an [`AuthConfig`] without dispatching
/// on `cfg.db.backend`. See [`connect_sqlite_for`].
pub(crate) async fn connect_postgres_for(cfg: &AuthConfig) -> Result<PgPool, AuthError> {
    connect_postgres(cfg).await
}

async fn connect_sqlite(cfg: &AuthConfig) -> Result<SqlitePool, AuthError> {
    let opts = SqliteConnectOptions::from_str(&cfg.db.url)
        .map_err(|e| AuthError::Internal(format!("invalid sqlite URL: {e}")))?
        .create_if_missing(true)
        // The DB lives on disk by default; explicitly disable the
        // in-memory mode so an operator who forgets the `memory:`
        // prefix does not silently lose all users on every restart.
        .foreign_keys(true)
        // WAL gives us concurrent readers + a single writer
        // (the migration step + the occasional login) without
        // the "database is locked" errors that the default
        // journal mode triggers under load.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(cfg.db.max_connections.max(1))
        .connect_with(opts)
        .await?;
    Ok(pool)
}

async fn connect_postgres(cfg: &AuthConfig) -> Result<PgPool, AuthError> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(cfg.db.max_connections.max(1))
        .connect(&cfg.db.url)
        .await?;
    Ok(pool)
}
