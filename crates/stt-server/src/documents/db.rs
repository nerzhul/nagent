//! DB access for the `uploaded_documents` table.
//!
//! Wraps an existing [`AuthStore`] so the documents feature
//! re-uses the same sqlx pool as the auth subsystem. The schema
//! lives in `migrations/0003_uploaded_documents.up.sql` and is
//! applied by the auth migrator (sqlx picks it up via the same
//! static `MIGRATOR` that runs the auth migrations).
//!
//! All SQL is written as raw strings rather than the
//! `sqlx::query!` compile-time-checked macros so the binary does
//! not need `DATABASE_URL` set at build time. The auth migrations
//! set the same precedent.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::store::{AuthStore, MigrationStatus};

use super::agent::DocumentRow;
use super::DocumentStore;

/// Errors surfaced by the documents DB layer. Mapped to HTTP
/// statuses by the route handlers; the CLI passes them straight to
/// `anyhow!` so a single error type stays at the call sites.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    /// `sqlx` returned an error. Most causes are operator-visible
    /// (DB unreachable, schema drift, …) so the message is
    /// preserved verbatim.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    /// The migration set was applied but `uploaded_documents` is
    /// still missing. Indicates a half-failed boot — the operator
    /// should run `stt-server migrate up` manually.
    #[error("uploaded_documents table is missing; run `stt-server migrate up`")]
    SchemaMissing,
}

impl From<AuthError> for DocumentError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Database(inner) => DocumentError::Sqlx(inner),
            other => DocumentError::Sqlx(sqlx::Error::Protocol(other.to_string())),
        }
    }
}

impl DocumentStore {
    /// `uploaded_documents` row for a given (session, name) pair.
    /// `uploaded_documents` row for a given (user, session, name)
    /// triple. Returns `None` when no row matches so the caller can
    /// surface "unknown document" rather than an SQL error.
    ///
    /// The `user_id` filter is the primary authorisation gate
    /// (SEV 2 fix). The `session_id` is the secondary gate that
    /// keeps two tabs of the same user from sharing docs.
    pub async fn get_document_by_name(
        &self,
        name: &str,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        let id = Uuid::from_str(name).map_err(|parse_err| {
            DocumentError::Sqlx(sqlx::Error::Protocol(format!(
                "document id `{name}` is not a valid UUID: {parse_err}"
            )))
        })?;
        match self.store() {
            AuthStore::Sqlite(s) => get_document_sqlite(s.pool(), id, user_id, session_id).await,
            AuthStore::Postgres(s) => {
                get_document_postgres(s.pool(), id, user_id, session_id).await
            }
        }
    }

    /// Insert a fresh row. The disk file is already in place at
    /// `disk_path` — the route handler writes it before calling this
    /// so a failed write surfaces as `422` without leaving an
    /// orphaned DB row behind.
    ///
    /// `user_id` is mandatory at every DB layer: the routes pass
    /// `AuthUser.id` (extracted via `axum::Extension`), the
    /// migration column is `NOT NULL`-equivalent by convention
    /// (inserts always include it).
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_document(
        &self,
        id: Uuid,
        session_id: Uuid,
        user_id: Uuid,
        original_name: &str,
        mime: &str,
        size_bytes: u64,
        extracted_chars: u64,
        page_count: Option<u32>,
        disk_path: &str,
    ) -> Result<(), DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => {
                insert_document_sqlite(
                    s.pool(),
                    id,
                    session_id,
                    user_id,
                    original_name,
                    mime,
                    size_bytes,
                    extracted_chars,
                    page_count,
                    disk_path,
                )
                .await
            }
            AuthStore::Postgres(s) => {
                insert_document_postgres(
                    s.pool(),
                    id,
                    session_id,
                    user_id,
                    original_name,
                    mime,
                    size_bytes,
                    extracted_chars,
                    page_count,
                    disk_path,
                )
                .await
            }
        }
    }

    /// List documents for a session, newest first. Powers
    /// `GET /v1/documents`.
    pub async fn list_documents_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<DocumentRow>, DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => {
                list_documents_for_session_sqlite(s.pool(), user_id, session_id).await
            }
            AuthStore::Postgres(s) => {
                list_documents_for_session_postgres(s.pool(), user_id, session_id).await
            }
        }
    }

    /// Count documents for a session. Used to enforce
    /// `max_docs_per_session`.
    pub async fn count_documents_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<u64, DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => {
                count_documents_for_session_sqlite(s.pool(), user_id, session_id).await
            }
            AuthStore::Postgres(s) => {
                count_documents_for_session_postgres(s.pool(), user_id, session_id).await
            }
        }
    }

    /// Look up a document by id WITHOUT scoping it to a session —
    /// used by the `GET /v1/documents/{id}` download handler, which
    /// is gated by `RequireAuth` but does not enforce a session
    /// match (a future plan may want admin-style "download any
    /// doc" endpoints).
    pub async fn get_document_by_id(
        &self,
        id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => get_document_by_id_sqlite(s.pool(), id, user_id).await,
            AuthStore::Postgres(s) => get_document_by_id_postgres(s.pool(), id, user_id).await,
        }
    }

    /// Delete a row + return the disk path so the caller can
    /// unlink the file. The on-disk unlink happens outside the
    /// transaction so a missing file does not roll back the DB
    /// write.
    pub async fn delete_document(
        &self,
        id: Uuid,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<PathBuf>, DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => delete_document_sqlite(s.pool(), id, user_id, session_id).await,
            AuthStore::Postgres(s) => {
                delete_document_postgres(s.pool(), id, user_id, session_id).await
            }
        }
    }

    /// Sweep every row whose `created_at + ttl_days` is older than
    /// `older_than` ago. Returns the list of (id, disk_path) pairs
    /// the caller should unlink.
    ///
    /// Used by both the background task (with `ttl_days =
    /// config.default_ttl_days`) and the `nagent documents purge
    /// --older-than 30d` CLI.
    pub async fn sweep_older_than(&self, ttl: Duration) -> Result<Vec<DocumentRow>, DocumentError> {
        let cutoff = Utc::now() - chrono::Duration::from_std(ttl).unwrap_or_default();
        match self.store() {
            AuthStore::Sqlite(s) => sweep_older_than_sqlite(s.pool(), cutoff).await,
            AuthStore::Postgres(s) => sweep_older_than_postgres(s.pool(), cutoff).await,
        }
    }

    /// Delete a row by id (no session scope — used by the CLI
    /// sweep after the operator already approved the plan).
    pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), DocumentError> {
        match self.store() {
            AuthStore::Sqlite(s) => delete_row_by_id_sqlite(s.pool(), id).await,
            AuthStore::Postgres(s) => delete_row_by_id_postgres(s.pool(), id).await,
        }
    }

    /// Migration status for the `uploaded_documents` table. Used by
    /// `boot::ensure_documents_table` to surface a clear error when
    /// the operator ran a partial boot (e.g. set `documents.enabled =
    /// true` but never ran `migrate up`).
    pub async fn migration_status(&self) -> Result<MigrationStatus, DocumentError> {
        Ok(self.store().migration_status().await?)
    }
}

// ---- sqlite --------------------------------------------------------------

async fn get_document_sqlite(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Option<DocumentRow>, DocumentError> {
    let row = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE id = ?1 AND user_id = ?2 AND session_id = ?3",
    )
    .bind(id.to_string())
    .bind(user_id.to_string())
    .bind(session_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(decode_row_sqlite).transpose()
}

async fn get_document_by_id_sqlite(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    user_id: Uuid,
) -> Result<Option<DocumentRow>, DocumentError> {
    let row = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE id = ?1 AND user_id = ?2",
    )
    .bind(id.to_string())
    .bind(user_id.to_string())
    .fetch_optional(pool)
    .await?;
    row.map(decode_row_sqlite).transpose()
}

#[allow(clippy::too_many_arguments)]
async fn insert_document_sqlite(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    session_id: Uuid,
    user_id: Uuid,
    original_name: &str,
    mime: &str,
    size_bytes: u64,
    extracted_chars: u64,
    page_count: Option<u32>,
    disk_path: &str,
) -> Result<(), DocumentError> {
    sqlx::query(
        "INSERT INTO uploaded_documents \
         (id, session_id, user_id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(id.to_string())
    .bind(session_id.to_string())
    .bind(user_id.to_string())
    .bind(original_name)
    .bind(mime)
    .bind(size_bytes as i64)
    .bind(extracted_chars as i64)
    .bind(page_count.map(|p| p as i64))
    .bind(disk_path)
    .execute(pool)
    .await?;
    Ok(())
}

async fn list_documents_for_session_sqlite(
    pool: &sqlx::SqlitePool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<DocumentRow>, DocumentError> {
    let rows = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE user_id = ?1 AND session_id = ?2 \
         ORDER BY created_at DESC",
    )
    .bind(user_id.to_string())
    .bind(session_id.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode_row_sqlite).collect()
}

async fn count_documents_for_session_sqlite(
    pool: &sqlx::SqlitePool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<u64, DocumentError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM uploaded_documents WHERE user_id = ?1 AND session_id = ?2",
    )
    .bind(user_id.to_string())
    .bind(session_id.to_string())
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    Ok(n.max(0) as u64)
}

async fn delete_document_sqlite(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Option<PathBuf>, DocumentError> {
    // Fetch + delete in a single transaction so two concurrent
    // `DELETE /v1/documents/{id}` calls cannot both unlink the
    // file. The `user_id` filter is the primary auth gate (SEV 2
    // fix); the `session_id` filter is the secondary gate that
    // keeps two tabs of the same user from deleting each other's
    // docs.
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT disk_path FROM uploaded_documents \
         WHERE id = ?1 AND user_id = ?2 AND session_id = ?3",
    )
    .bind(id.to_string())
    .bind(user_id.to_string())
    .bind(session_id.to_string())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await.ok();
        return Ok(None);
    };
    let disk_path: String = row.try_get("disk_path")?;
    sqlx::query("DELETE FROM uploaded_documents WHERE id = ?1")
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(PathBuf::from(disk_path)))
}

async fn sweep_older_than_sqlite(
    pool: &sqlx::SqlitePool,
    cutoff: DateTime<Utc>,
) -> Result<Vec<DocumentRow>, DocumentError> {
    let rows = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents \
         WHERE (expires_at IS NOT NULL AND expires_at <= ?1) \
            OR created_at <= ?1 \
         ORDER BY created_at ASC",
    )
    .bind(cutoff.to_rfc3339())
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode_row_sqlite).collect()
}

async fn delete_row_by_id_sqlite(pool: &sqlx::SqlitePool, id: Uuid) -> Result<(), DocumentError> {
    sqlx::query("DELETE FROM uploaded_documents WHERE id = ?1")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

fn decode_row_sqlite(row: sqlx::sqlite::SqliteRow) -> Result<DocumentRow, DocumentError> {
    let id_str: String = row.try_get("id")?;
    let original_name: String = row.try_get("original_name")?;
    let mime: String = row.try_get("mime")?;
    let size_bytes: i64 = row.try_get("size_bytes")?;
    let extracted_chars: i64 = row.try_get("extracted_chars")?;
    let page_count: Option<i64> = row.try_get("page_count")?;
    let disk_path: String = row.try_get("disk_path")?;
    Ok(DocumentRow {
        id: Uuid::from_str(&id_str)
            .map_err(|e| DocumentError::Sqlx(sqlx::Error::Protocol(e.to_string())))?,
        original_name,
        mime,
        size_bytes: size_bytes.max(0) as u64,
        extracted_chars: extracted_chars.max(0) as u64,
        page_count: page_count.map(|p| p.max(0) as u32),
        disk_path: PathBuf::from(disk_path),
    })
}

// ---- postgres ------------------------------------------------------------

async fn get_document_postgres(
    pool: &sqlx::PgPool,
    id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Option<DocumentRow>, DocumentError> {
    let row = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE id = $1 AND user_id = $2 AND session_id = $3",
    )
    .bind(id)
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_row_postgres).transpose()
}

async fn get_document_by_id_postgres(
    pool: &sqlx::PgPool,
    id: Uuid,
    user_id: Uuid,
) -> Result<Option<DocumentRow>, DocumentError> {
    let row = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    row.map(decode_row_postgres).transpose()
}

#[allow(clippy::too_many_arguments)]
async fn insert_document_postgres(
    pool: &sqlx::PgPool,
    id: Uuid,
    session_id: Uuid,
    user_id: Uuid,
    original_name: &str,
    mime: &str,
    size_bytes: u64,
    extracted_chars: u64,
    page_count: Option<u32>,
    disk_path: &str,
) -> Result<(), DocumentError> {
    sqlx::query(
        "INSERT INTO uploaded_documents \
         (id, session_id, user_id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id)
    .bind(session_id)
    .bind(user_id)
    .bind(original_name)
    .bind(mime)
    .bind(size_bytes as i64)
    .bind(extracted_chars as i64)
    .bind(page_count.map(|p| p as i32))
    .bind(disk_path)
    .execute(pool)
    .await?;
    Ok(())
}

async fn list_documents_for_session_postgres(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<DocumentRow>, DocumentError> {
    let rows = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents WHERE user_id = $1 AND session_id = $2 \
         ORDER BY created_at DESC",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode_row_postgres).collect()
}

async fn count_documents_for_session_postgres(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<u64, DocumentError> {
    let row = sqlx::query(
        "SELECT COUNT(*) AS n FROM uploaded_documents WHERE user_id = $1 AND session_id = $2",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    Ok(n.max(0) as u64)
}

async fn delete_document_postgres(
    pool: &sqlx::PgPool,
    id: Uuid,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<Option<PathBuf>, DocumentError> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT disk_path FROM uploaded_documents \
         WHERE id = $1 AND user_id = $2 AND session_id = $3",
    )
    .bind(id)
    .bind(user_id)
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await.ok();
        return Ok(None);
    };
    let disk_path: String = row.try_get("disk_path")?;
    sqlx::query("DELETE FROM uploaded_documents WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(PathBuf::from(disk_path)))
}

async fn sweep_older_than_postgres(
    pool: &sqlx::PgPool,
    cutoff: DateTime<Utc>,
) -> Result<Vec<DocumentRow>, DocumentError> {
    let rows = sqlx::query(
        "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
         FROM uploaded_documents \
         WHERE (expires_at IS NOT NULL AND expires_at <= $1) \
            OR created_at <= $1 \
         ORDER BY created_at ASC",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(decode_row_postgres).collect()
}

async fn delete_row_by_id_postgres(pool: &sqlx::PgPool, id: Uuid) -> Result<(), DocumentError> {
    sqlx::query("DELETE FROM uploaded_documents WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

fn decode_row_postgres(row: sqlx::postgres::PgRow) -> Result<DocumentRow, DocumentError> {
    let id: Uuid = row.try_get("id")?;
    let original_name: String = row.try_get("original_name")?;
    let mime: String = row.try_get("mime")?;
    let size_bytes: i64 = row.try_get("size_bytes")?;
    let extracted_chars: i64 = row.try_get("extracted_chars")?;
    let page_count: Option<i32> = row.try_get("page_count")?;
    let disk_path: String = row.try_get("disk_path")?;
    Ok(DocumentRow {
        id,
        original_name,
        mime,
        size_bytes: size_bytes.max(0) as u64,
        extracted_chars: extracted_chars.max(0) as u64,
        page_count: page_count.map(|p| p.max(0) as u32),
        disk_path: PathBuf::from(disk_path),
    })
}
