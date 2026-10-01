//! Documents repository — plan 4.A scoped per-user view.
//!
//! Owns the SQL for the `uploaded_documents` table. Callers that
//! have already resolved a `user_id` should prefer
//! [`Documents::for_user`] so the `WHERE user_id = ?` filter
//! cannot be accidentally dropped (plan 4.A S4). Admin / CLI paths
//! (background sweep, `nagent documents purge`) keep using the
//! unscoped repository directly.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use crate::error::Error;
use crate::migrate::MigrationStatus;
use crate::pool::AnyPool;
use crate::types::DocumentRow;

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("uploaded_documents table is missing; run `nagent migrate up`")]
    SchemaMissing,
}

impl From<Error> for DocumentError {
    fn from(e: Error) -> Self {
        match e {
            Error::Database(inner) => DocumentError::Sqlx(inner),
            other => DocumentError::Sqlx(sqlx::Error::Protocol(other.to_string())),
        }
    }
}

/// Engine-agnostic documents repository. Cheap to clone (each
/// variant wraps a sqlx pool which itself is `Arc`-backed).
#[derive(Debug, Clone)]
pub enum Documents {
    Sqlite(sqlite::SqliteDocuments),
    Postgres(postgres::PgDocuments),
}

impl Documents {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteDocuments::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgDocuments::new(p.clone())),
        }
    }

    /// Scope every per-row method to a single `user_id`. The
    /// returned [`ScopedDocuments`] does not take a `user_id`
    /// argument on its per-row methods, so a route handler that
    /// holds a `ScopedDocuments` cannot accidentally drop the
    /// `WHERE user_id = ?` filter (plan 4.A S4).
    pub fn for_user(&self, user_id: Uuid) -> ScopedDocuments {
        ScopedDocuments {
            inner: self.clone(),
            user_id,
        }
    }

    /// Look up a document by UUID within a `(user, session)` pair.
    /// Kept on the unscoped repository for the legacy `name`-based
    /// call path used by the routes today; new callers should use
    /// [`ScopedDocuments::get_by_id`].
    pub async fn get_by_name(
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
        self.get_by_id(id, user_id, Some(session_id)).await
    }

    /// Look up a document by UUID + user, optionally scoped to a
    /// session. Kept unscoped for the download handler, which is
    /// gated by `RequireAuth` but does not enforce a session match.
    pub async fn get_by_id(
        &self,
        id: Uuid,
        user_id: Uuid,
        session_id: Option<Uuid>,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        match self {
            Documents::Sqlite(s) => s.get_by_id(id, user_id, session_id).await,
            Documents::Postgres(s) => s.get_by_id(id, user_id, session_id).await,
        }
    }

    /// Insert a fresh row. Caller writes the on-disk file before
    /// calling this so a failed DB write surfaces as `422` without
    /// leaving an orphaned row behind.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
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
        match self {
            Documents::Sqlite(s) => {
                s.insert(
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
            Documents::Postgres(s) => {
                s.insert(
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

    /// Sweep every row whose `expires_at` (or `created_at` when
    /// the row has no explicit expiry) is older than `ttl` ago.
    /// Used by the background sweep + the `nagent documents purge`
    /// CLI; the per-user route handlers should NOT use this.
    pub async fn sweep_older_than(&self, ttl: Duration) -> Result<Vec<DocumentRow>, DocumentError> {
        let cutoff = Utc::now() - chrono::Duration::from_std(ttl).unwrap_or_default();
        match self {
            Documents::Sqlite(s) => s.sweep_older_than(cutoff).await,
            Documents::Postgres(s) => s.sweep_older_than(cutoff).await,
        }
    }

    /// Delete a row by id without a session or user filter. Used
    /// by the CLI sweep after the operator has approved the plan.
    pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), DocumentError> {
        match self {
            Documents::Sqlite(s) => s.delete_row_by_id(id).await,
            Documents::Postgres(s) => s.delete_row_by_id(id).await,
        }
    }

    /// Migration status for the `uploaded_documents` table. Used
    /// by `boot::ensure_documents_table` to surface a clear error
    /// when the operator ran a partial boot.
    pub async fn migration_status(&self) -> Result<MigrationStatus, DocumentError> {
        match self {
            Documents::Sqlite(s) => {
                Ok(crate::migrate::status(&AnyPool::Sqlite(s.pool.clone())).await?)
            }
            Documents::Postgres(s) => {
                Ok(crate::migrate::status(&AnyPool::Postgres(s.pool.clone())).await?)
            }
        }
    }
}

/// Per-user scoped view over [`Documents`]. The `user_id` is
/// baked in at construction; per-row methods therefore cannot be
/// called with the wrong `user_id` by accident (plan 4.A S4).
#[derive(Debug, Clone)]
pub struct ScopedDocuments {
    inner: Documents,
    user_id: Uuid,
}

impl ScopedDocuments {
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// List every document for `(user, session)`.
    pub async fn list_for_session(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<DocumentRow>, DocumentError> {
        match &self.inner {
            Documents::Sqlite(s) => s.list_for_session(self.user_id, session_id).await,
            Documents::Postgres(s) => s.list_for_session(self.user_id, session_id).await,
        }
    }

    /// Count documents for `(user, session)`. Used to enforce
    /// `max_docs_per_session` BEFORE the on-disk write.
    pub async fn count_for_session(&self, session_id: Uuid) -> Result<u64, DocumentError> {
        match &self.inner {
            Documents::Sqlite(s) => s.count_for_session(self.user_id, session_id).await,
            Documents::Postgres(s) => s.count_for_session(self.user_id, session_id).await,
        }
    }

    /// Look up a document by UUID within `(user, session)`.
    pub async fn get_for_session(
        &self,
        id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        self.inner
            .get_by_id(id, self.user_id, Some(session_id))
            .await
    }

    /// Look up a document by UUID + user (download path; no
    /// session scope).
    pub async fn get_by_id(&self, id: Uuid) -> Result<Option<DocumentRow>, DocumentError> {
        self.inner.get_by_id(id, self.user_id, None).await
    }

    /// Insert a fresh row. `session_id` is the chat-session binding.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        &self,
        id: Uuid,
        session_id: Uuid,
        original_name: &str,
        mime: &str,
        size_bytes: u64,
        extracted_chars: u64,
        page_count: Option<u32>,
        disk_path: &str,
    ) -> Result<(), DocumentError> {
        self.inner
            .insert(
                id,
                session_id,
                self.user_id,
                original_name,
                mime,
                size_bytes,
                extracted_chars,
                page_count,
                disk_path,
            )
            .await
    }

    /// Delete a row scoped by `(user, session)`. Returns the on-disk
    /// path so the caller can unlink the file.
    pub async fn delete(
        &self,
        id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<PathBuf>, DocumentError> {
        match &self.inner {
            Documents::Sqlite(s) => s.delete(id, self.user_id, session_id).await,
            Documents::Postgres(s) => s.delete(id, self.user_id, session_id).await,
        }
    }
}

pub(crate) mod sqlite {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::documents::DocumentError;
    use crate::types::DocumentRow;

    #[derive(Clone, Debug)]
    pub struct SqliteDocuments {
        pub pool: SqlitePool,
    }

    impl SqliteDocuments {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn get_by_id(
            &self,
            id: Uuid,
            user_id: Uuid,
            session_id: Option<Uuid>,
        ) -> Result<Option<DocumentRow>, DocumentError> {
            let row = match session_id {
                Some(sid) => sqlx::query(
                    "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
                     FROM uploaded_documents WHERE id = ?1 AND user_id = ?2 AND session_id = ?3",
                )
                .bind(id.to_string())
                .bind(user_id.to_string())
                .bind(sid.to_string())
                .fetch_optional(&self.pool)
                .await?,
                None => sqlx::query(
                    "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
                     FROM uploaded_documents WHERE id = ?1 AND user_id = ?2",
                )
                .bind(id.to_string())
                .bind(user_id.to_string())
                .fetch_optional(&self.pool)
                .await?,
            };
            row.map(decode_row).transpose()
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn insert(
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
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        pub async fn list_for_session(
            &self,
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
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(decode_row).collect()
        }

        pub async fn count_for_session(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<u64, DocumentError> {
            let row = sqlx::query(
                "SELECT COUNT(*) AS n FROM uploaded_documents WHERE user_id = ?1 AND session_id = ?2",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .fetch_one(&self.pool)
            .await?;
            let n: i64 = row.try_get("n")?;
            Ok(n.max(0) as u64)
        }

        pub async fn delete(
            &self,
            id: Uuid,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Option<PathBuf>, DocumentError> {
            let mut tx = self.pool.begin().await?;
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

        pub async fn sweep_older_than(
            &self,
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
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(decode_row).collect()
        }

        pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), DocumentError> {
            sqlx::query("DELETE FROM uploaded_documents WHERE id = ?1")
                .bind(id.to_string())
                .execute(&self.pool)
                .await?;
            Ok(())
        }
    }

    fn decode_row(row: sqlx::sqlite::SqliteRow) -> Result<DocumentRow, DocumentError> {
        let id_str: String = row.try_get("id")?;
        let original_name: String = row.try_get("original_name")?;
        let mime: String = row.try_get("mime")?;
        let size_bytes: i64 = row.try_get("size_bytes")?;
        let extracted_chars: i64 = row.try_get("extracted_chars")?;
        let page_count: Option<i64> = row.try_get("page_count")?;
        let disk_path: String = row.try_get("disk_path")?;
        Ok(DocumentRow {
            id: Uuid::parse_str(&id_str)
                .map_err(|e| DocumentError::Sqlx(sqlx::Error::Protocol(e.to_string())))?,
            original_name,
            mime,
            size_bytes: size_bytes.max(0) as u64,
            extracted_chars: extracted_chars.max(0) as u64,
            page_count: page_count.map(|p| p.max(0) as u32),
            disk_path: PathBuf::from(disk_path),
        })
    }
}

pub(crate) mod postgres {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::documents::DocumentError;
    use crate::types::DocumentRow;

    #[derive(Clone, Debug)]
    pub struct PgDocuments {
        pub pool: PgPool,
    }

    impl PgDocuments {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn get_by_id(
            &self,
            id: Uuid,
            user_id: Uuid,
            session_id: Option<Uuid>,
        ) -> Result<Option<DocumentRow>, DocumentError> {
            let row = match session_id {
                Some(sid) => sqlx::query(
                    "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
                     FROM uploaded_documents WHERE id = $1 AND user_id = $2 AND session_id = $3",
                )
                .bind(id)
                .bind(user_id)
                .bind(sid)
                .fetch_optional(&self.pool)
                .await?,
                None => sqlx::query(
                    "SELECT id, original_name, mime, size_bytes, extracted_chars, page_count, disk_path \
                     FROM uploaded_documents WHERE id = $1 AND user_id = $2",
                )
                .bind(id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
            };
            row.map(decode_row).transpose()
        }

        #[allow(clippy::too_many_arguments)]
        pub async fn insert(
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
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        pub async fn list_for_session(
            &self,
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
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(decode_row).collect()
        }

        pub async fn count_for_session(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<u64, DocumentError> {
            let row = sqlx::query(
                "SELECT COUNT(*) AS n FROM uploaded_documents WHERE user_id = $1 AND session_id = $2",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(&self.pool)
            .await?;
            let n: i64 = row.try_get("n")?;
            Ok(n.max(0) as u64)
        }

        pub async fn delete(
            &self,
            id: Uuid,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Option<PathBuf>, DocumentError> {
            let mut tx = self.pool.begin().await?;
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

        pub async fn sweep_older_than(
            &self,
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
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(decode_row).collect()
        }

        pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), DocumentError> {
            sqlx::query("DELETE FROM uploaded_documents WHERE id = $1")
                .bind(id)
                .execute(&self.pool)
                .await?;
            Ok(())
        }
    }

    fn decode_row(row: sqlx::postgres::PgRow) -> Result<DocumentRow, DocumentError> {
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
}
