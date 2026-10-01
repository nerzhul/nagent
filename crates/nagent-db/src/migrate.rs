//! Migration runner shared by both engines.
//!
//! Plan 5.D relocates the migration files under `db/migrations/` so
//! every domain table is owned by exactly one migration. Each
//! migration filename encodes its order (`0001_init`, `0002_…`);
//! this module is the single place that knows how to apply, inspect
//! and reverse them.
//!
//! ## Layering note
//!
//! [`crate::auth::store::AuthStore::migrate`] /
//! [`crate::auth::store::AuthStore::migration_status`] /
//! [`crate::auth::store::AuthStore::revert_to`] now delegate here.
//! The CLI (`stt-server migrate …`) and the boot path go through
//! the wrapper, so the migration runner only has to be correct
//! once.

use sqlx::migrate::Migrator;

use crate::error::Error;
use crate::pool::AnyPool;
pub use crate::types::{MigrationRow, MigrationStatus};

/// Embedded `sqlx::migrate!` macro output. Reads the SQL files at
/// compile time and bakes them into the binary; no filesystem
/// access at runtime.
static MIGRATOR: Migrator = sqlx::migrate!("./src/migrations");

/// Run the migrations directory against `pool`. Idempotent — sqlx
/// tracks which migrations have already been applied in the
/// `_sqlx_migrations` table it creates on first run.
pub async fn run(pool: &AnyPool) -> Result<(), Error> {
    match pool {
        AnyPool::Sqlite(p) => MIGRATOR
            .run(p)
            .await
            .map_err(|e| Error::Internal(format!("sqlite migrations failed: {e}"))),
        AnyPool::Postgres(p) => MIGRATOR
            .run(p)
            .await
            .map_err(|e| Error::Internal(format!("postgres migrations failed: {e}"))),
    }
}

/// Migration status types are re-exported from [`crate::types`]
/// so there is only one definition shared by the SQL parser and
/// the migration runner.

/// Inspect `_sqlx_migrations` and cross-reference it against the
/// embedded `MIGRATOR` static to surface the applied / pending
/// split. Returns an empty `applied` set when the table does not
/// exist yet (fresh DB), so the `migrate status` CLI prints "all
/// pending" rather than crashing on a brand-new install.
pub async fn status(pool: &AnyPool) -> Result<MigrationStatus, Error> {
    match pool {
        AnyPool::Sqlite(p) => sqlite_status(p).await,
        AnyPool::Postgres(p) => postgres_status(p).await,
    }
}

/// Roll back every applied migration with a version strictly
/// greater than `target_version`. `target_version` itself stays
/// applied (sqlx 0.8.6 semantics — see `Migrator::undo`).
pub async fn revert_to(pool: &AnyPool, target_version: i64) -> Result<(), Error> {
    match pool {
        AnyPool::Sqlite(p) => MIGRATOR
            .undo(p, target_version)
            .await
            .map_err(|e| Error::Internal(format!("sqlite migrations undo failed: {e}"))),
        AnyPool::Postgres(p) => MIGRATOR
            .undo(p, target_version)
            .await
            .map_err(|e| Error::Internal(format!("postgres migrations undo failed: {e}"))),
    }
}

// ---- Per-engine helpers ---------------------------------------------------

async fn sqlite_status(pool: &SqlitePool) -> Result<MigrationStatus, Error> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT version, description FROM _sqlx_migrations \
         WHERE success = 1 ORDER BY version",
    )
    .fetch_all(pool)
    .await;
    let applied: Vec<MigrationRow> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                Ok::<_, sqlx::Error>(MigrationRow {
                    version: r.try_get::<i64, _>("version")?,
                    description: r.try_get::<String, _>("description")?,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Database)?,
        Err(sqlx::Error::Database(db_err)) if is_table_missing(&*db_err) => Vec::new(),
        Err(e) => return Err(Error::Database(e)),
    };

    let applied_versions: std::collections::HashSet<i64> =
        applied.iter().map(|r| r.version).collect();
    let mut pending: Vec<MigrationRow> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for m in MIGRATOR.iter() {
        if applied_versions.contains(&m.version) {
            continue;
        }
        if !seen.insert(m.version) {
            continue;
        }
        pending.push(MigrationRow {
            version: m.version,
            description: m.description.to_string(),
        });
    }
    pending.sort_by_key(|r| r.version);
    let highest_applied = applied.iter().map(|r| r.version).max();
    Ok(MigrationStatus {
        applied,
        pending,
        highest_applied,
    })
}

async fn postgres_status(pool: &PgPool) -> Result<MigrationStatus, Error> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT version, description FROM _sqlx_migrations \
         WHERE success = true ORDER BY version",
    )
    .fetch_all(pool)
    .await;
    let applied: Vec<MigrationRow> = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                Ok::<_, sqlx::Error>(MigrationRow {
                    version: r.try_get::<i64, _>("version")?,
                    description: r.try_get::<String, _>("description")?,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Database)?,
        Err(sqlx::Error::Database(db_err)) if is_table_missing(&*db_err) => Vec::new(),
        Err(e) => return Err(Error::Database(e)),
    };

    let applied_versions: std::collections::HashSet<i64> =
        applied.iter().map(|r| r.version).collect();
    let mut pending: Vec<MigrationRow> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for m in MIGRATOR.iter() {
        if applied_versions.contains(&m.version) {
            continue;
        }
        if !seen.insert(m.version) {
            continue;
        }
        pending.push(MigrationRow {
            version: m.version,
            description: m.description.to_string(),
        });
    }
    pending.sort_by_key(|r| r.version);
    let highest_applied = applied.iter().map(|r| r.version).max();
    Ok(MigrationStatus {
        applied,
        pending,
        highest_applied,
    })
}

use sqlx::{PgPool, SqlitePool};

fn is_table_missing(db_err: &dyn sqlx::error::DatabaseError) -> bool {
    // sqlx doesn't surface a clean "table doesn't exist" variant.
    // The signal differs per engine:
    // - Postgres: SQLSTATE 42S02 (undefined_table).
    // - SQLite: code 1 (SQLITE_ERROR) with a "no such table"
    // message — the SQLSTATE mapping is not consistent across
    // versions, so the message text is the portable fallback.
    db_err.code().as_deref() == Some("42S02")
        || db_err.message().contains("no such table")
        || db_err.message().contains("no such view")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_status_helpers() {
        let mut s = MigrationStatus::default();
        assert!(s.is_empty());
        assert!(!s.has_pending());
        s.pending.push(MigrationRow {
            version: 1,
            description: "init".into(),
        });
        assert!(s.has_pending());
        // `is_empty()` reflects the `applied` set only — pending
        // entries don't change it.
        assert!(s.is_empty());

        s.applied.push(MigrationRow {
            version: 1,
            description: "init".into(),
        });
        s.pending.clear();
        assert!(!s.is_empty());
        assert!(!s.has_pending());
    }
}
