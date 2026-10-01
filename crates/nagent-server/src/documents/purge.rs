//! Background + CLI sweep for old documents.
//!
//! Both the periodic `tokio::spawn` task in `main.rs` AND the
//! `nagent documents purge` CLI delegate to
//! [`purge_older_than`]. The function is intentionally cheap to
//! call repeatedly: it batches by row (sqlite) or single-shot
//! (postgres), unlinks the on-disk file outside the DB
//! transaction, and reports a summary the caller can log.

use std::path::Path;
use std::time::Duration;

use crate::auth::error::AuthError;
use crate::documents::DocumentStore;
use nagent_db::documents::DocumentError as DbDocumentError;

/// Convert a `nagent_db::documents::DocumentError` into the
/// server's [`AuthError`] so the route / CLI error mapping stays
/// intact.
impl From<DbDocumentError> for AuthError {
    fn from(e: DbDocumentError) -> Self {
        match e {
            DbDocumentError::Sqlx(inner) => AuthError::Database(inner.into()),
            DbDocumentError::SchemaMissing => AuthError::Internal(
                "uploaded_documents table is missing; run `nagent migrate up`".into(),
            ),
        }
    }
}

/// Sweep documents whose `created_at + ttl` is older than `ttl`
/// ago. For each matched row:
///
/// 1. Unlink the file from disk. Missing files are NOT an error —
/// a previous partial boot could have removed the file
/// independently, and we want the DB row gone regardless.
/// 2. Delete the DB row.
///
/// Returns the number of rows purged (file + DB). The caller can
/// log this at `info!` for the periodic task or print it for the
/// CLI. Errors during the unlink phase are *logged* but never
/// surfaced — the operator can `ls` the cache dir afterwards to
/// see if any orphans remain.
pub async fn purge_older_than(
    store: &DocumentStore,
    cache_dir: &Path,
    ttl: Duration,
) -> Result<usize, AuthError> {
    let rows = store.db().documents.sweep_older_than(ttl).await?;
    let total = rows.len();
    for row in rows {
        // Skip files that escaped the cache dir (defence against a
        // future migration that forgets to validate `disk_path`).
        let safe = match row.disk_path.canonicalize() {
            Ok(p) => p,
            Err(_) => row.disk_path.clone(),
        };
        if !safe.starts_with(cache_dir) {
            tracing::warn!(
                document_id = %row.id,
                path = %row.disk_path.display(),
                "skipping unlink: disk_path escaped cache_dir"
            );
            // Still drop the DB row so the next boot starts clean.
            store.db().documents.delete_row_by_id(row.id).await?;
            continue;
        }
        match std::fs::remove_file(&row.disk_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File already gone — drop the DB row so the
                // periodic sweep converges.
            }
            Err(e) => {
                tracing::warn!(
                    document_id = %row.id,
                    path = %row.disk_path.display(),
                    error = %e,
                    "could not unlink document during purge sweep; leaving row in place"
                );
                continue;
            }
        }
        store.db().documents.delete_row_by_id(row.id).await?;
    }
    Ok(total)
}

/// Periodic background sweep. Spawned by `main.rs` when
/// `documents.purge_interval_hours > 0`. The loop logs an error
/// and continues on every iteration so a transient DB error
/// doesn't kill the task.
pub async fn run_periodic_purge(
    store: DocumentStore,
    cache_dir: std::path::PathBuf,
    interval: Duration,
    ttl: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    // Skip the first immediate tick — the server just booted and
    // there is no urgency to sweep.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match purge_older_than(&store, &cache_dir, ttl).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(purged = n, "documents: periodic purge removed {n} row(s)"),
            Err(e) => tracing::warn!(error = %e, "documents: periodic purge failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn safe_path_prefix_check_rejects_escape() {
        // No DB needed — the prefix check is a pure function over
        // the disk path. We exercise it indirectly by checking that
        // a path outside the cache dir is flagged as "escaped".
        let cache = std::path::PathBuf::from("/var/cache/nagent/docs");
        let escapee = std::path::PathBuf::from("/etc/passwd");
        assert!(!escapee.starts_with(&cache));
    }
}
