//! Documents repository — plan 5.D extraction.
//!
//! Owns the SQL for the `uploaded_documents` table that previously
//! lived in [`crate::documents::db`].

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use crate::error::Error;
use crate::migrate::{self as migrate_runner, MigrationStatus};
use crate::pool::AnyPool;
use crate::types::DocumentRow;

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("uploaded_documents table is missing; run `stt-server migrate up`")]
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

    pub async fn get_by_name(
        &self,
        name: &str,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        let id = Uuid::parse_str(name).map_err(|parse_err| {
            DocumentError::Sqlx(sqlx::Error::Protocol(format!(
                "document id `{name}` is not a valid UUID: {parse_err}"
            )))
        })?;
        match self {
            Documents::Sqlite(s) => s.get_by_id(id, user_id, Some(session_id)).await,
            Documents::Postgres(s) => s.get_by_id(id, user_id, Some(session_id)).await,
        }
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

    pub async fn list_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<DocumentRow>, DocumentError> {
        match self {
            Documents::Sqlite(s) => s.list_for_session(user_id, session_id).await,
            Documents::Postgres(s) => s.list_for_session(user_id, session_id).await,
        }
    }

    pub async fn count_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<u64, DocumentError> {
        match self {
            Documents::Sqlite(s) => s.count_for_session(user_id, session_id).await,
            Documents::Postgres(s) => s.count_for_session(user_id, session_id).await,
        }
    }

    pub async fn get_by_id(
        &self,
        id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<DocumentRow>, DocumentError> {
        match self {
            Documents::Sqlite(s) => s.get_by_id(id, user_id, None).await,
            Documents::Postgres(s) => s.get_by_id(id, user_id, None).await,
        }
    }

    pub async fn delete(
        &self,
        id: Uuid,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<PathBuf>, DocumentError> {
        match self {
            Documents::Sqlite(s) => s.delete(id, user_id, session_id).await,
            Documents::Postgres(s) => s.delete(id, user_id, session_id).await,
        }
    }

    pub async fn sweep_older_than(&self, ttl: Duration) -> Result<Vec<DocumentRow>, DocumentError> {
        let cutoff = Utc::now() - chrono::Duration::from_std(ttl).unwrap_or_default();
        match self {
            Documents::Sqlite(s) => s.sweep_older_than(cutoff).await,
            Documents::Postgres(s) => s.sweep_older_than(cutoff).await,
        }
    }

    pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), DocumentError> {
        match self {
            Documents::Sqlite(s) => s.delete_row_by_id(id).await,
            Documents::Postgres(s) => s.delete_row_by_id(id).await,
        }
    }

    pub async fn migration_status(&self) -> Result<MigrationStatus, DocumentError> {
        match self {
            Documents::Sqlite(s) => {
                Ok(migrate_runner::status(&AnyPool::Sqlite(s.pool.clone())).await?)
            }
            Documents::Postgres(s) => {
                Ok(migrate_runner::status(&AnyPool::Postgres(s.pool.clone())).await?)
            }
        }
    }
}

pub mod sqlite {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::documents::DocumentError;
    use crate::types::DocumentRow;

    #[derive(Clone, Debug)]
    pub struct SqliteDocuments {
        pub(crate) pool: SqlitePool,
    }

    impl SqliteDocuments {
        pub fn new(pool: SqlitePool) -> Self {
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

pub mod postgres {
    use std::path::PathBuf;

    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::documents::DocumentError;
    use crate::types::DocumentRow;

    #[derive(Clone, Debug)]
    pub struct PgDocuments {
        pub(crate) pool: PgPool,
    }

    impl PgDocuments {
        pub fn new(pool: PgPool) -> Self {
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
