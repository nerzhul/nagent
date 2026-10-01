//! Shared DB pool + engine enum.
//!
//! Wraps the two sqlx pools behind a single enum so call sites can
//! write one match instead of two. Heavy work — connection
//! establishment, pool tuning, foreign-key enforcement, journal
//! mode — lives here once so the per-domain repositories only
//! have to think about their SQL.
//!
//! The per-engine pool types are only compiled when the matching
//! `db-sqlite` / `db-postgres` cargo feature is on .
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
#[derive(Clone)]
pub struct DbOptions {
    pub backend: DbEngine,
    pub url: String,
    pub max_connections: u32,
    pub auto_migrate: bool,
}

/// Manually implemented to redact any password embedded in `url`
/// (plan S11). A Postgres URL of the form
/// `postgres://user:secret@host/db` would otherwise leak the
/// `secret` half into every `Debug` print, log line, and panic
/// message — both the access log and the startup banner build
/// their lines from these options.
impl std::fmt::Debug for DbOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbOptions")
            .field("backend", &self.backend)
            .field("url", &redact_url(&self.url))
            .field("max_connections", &self.max_connections)
            .field("auto_migrate", &self.auto_migrate)
            .finish()
    }
}

/// Strip the userinfo password (if any) from a connection URL.
/// The scheme + host + database are kept so an operator reading
/// the log can still see which database is being connected to.
fn redact_url(url: &str) -> String {
    // Cheap parser: locate `://`, then look for `@`. The substring
    // between them is the userinfo; if it contains a `:`, only the
    // password half is masked.
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after_scheme = &url[scheme_end + 3..];
    let Some(at) = after_scheme.find('@') else {
        return url.to_string();
    };
    let userinfo = &after_scheme[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    format!(
        "{}://{}:****{}",
        &url[..scheme_end],
        &userinfo[..colon],
        &after_scheme[at..]
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_password_in_postgres_url() {
        // Postgres URL: scheme://user:password@host:port/db
        let opts = DbOptions {
            backend: DbEngine::Postgres,
            url: "postgres://alice:hunter2@db.example.com:5432/app".into(),
            max_connections: 4,
            auto_migrate: false,
        };
        let dbg = format!("{opts:?}");
        assert!(
            !dbg.contains("hunter2"),
            "password must be redacted; got: {dbg}"
        );
        assert!(dbg.contains("alice"), "user must remain visible");
        assert!(dbg.contains("db.example.com"), "host must remain visible");
        assert!(dbg.contains("****"), "redaction marker must appear");
    }

    #[test]
    fn debug_passes_through_url_without_password() {
        // SQLite path-style URLs have no userinfo; the redaction
        // helper must leave them alone.
        let opts = DbOptions {
            backend: DbEngine::Sqlite,
            url: "sqlite://file:auth.db?mode=rwc".into(),
            max_connections: 1,
            auto_migrate: true,
        };
        let dbg = format!("{opts:?}");
        assert!(
            dbg.contains("sqlite://file:auth.db"),
            "url must round-trip: {dbg}"
        );
        assert!(
            !dbg.contains("****"),
            "no-password url must not gain a redaction marker"
        );
    }

    #[test]
    fn debug_redacts_password_in_userless_url() {
        // Some URL formats embed `: something` without a user, e.g.
        // `scheme://:password@host`. The redaction helper should
        // still mask the password half.
        let opts = DbOptions {
            backend: DbEngine::Postgres,
            url: "postgres://:hunter2@db.example.com/app".into(),
            max_connections: 1,
            auto_migrate: false,
        };
        let dbg = format!("{opts:?}");
        assert!(
            !dbg.contains("hunter2"),
            "userless-password must be redacted; got: {dbg}"
        );
        assert!(dbg.contains("****"));
    }

    #[test]
    fn debug_leaves_user_only_url_alone() {
        // `user@host` (no password) — there is nothing to redact,
        // the helper should round-trip.
        let opts = DbOptions {
            backend: DbEngine::Postgres,
            url: "postgres://alice@db.example.com/app".into(),
            max_connections: 1,
            auto_migrate: false,
        };
        let dbg = format!("{opts:?}");
        assert!(dbg.contains("alice"));
        assert!(
            !dbg.contains("****"),
            "user-only url must not gain a redaction marker"
        );
    }
}
