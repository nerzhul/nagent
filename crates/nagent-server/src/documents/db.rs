//! Domain-layer wrapper over [`nagent_db::documents`].
//!
//! Plan 4.A removed the per-engine SQL duplication: every query
//! now lives in `nagent_db::documents::Documents` (and its
//! per-engine sub-modules). This file keeps the `DocumentStore`
//! shape (a cheap-to-clone handle that bundles the DB
//! repository, the on-disk cache directory, and the extraction
//! caps) so the documents routes + the read_document agent keep
//! resolving their state through `state.documents.store`.
//!
//! The methods on `DocumentStore` keep their `user_id` +
//! `session_id` arguments at this layer for backwards
//! compatibility; a follow-up commit will switch the routes +
//! CLI to use [`nagent_db::documents::ScopedDocuments`]
//! directly so the `user_id` filter cannot be dropped (plan
//! 4.A S4).
#![allow(dead_code)]

use std::path::PathBuf;

use uuid::Uuid;

use crate::auth::error::AuthError;

use super::agent::DocumentRow;
use super::DocumentStore;

// Re-export `nagent_db::documents::DocumentError` so the legacy
// `documents::db::DocumentError` import path keeps resolving for
// the routes + purge module.
pub use nagent_db::documents::DocumentError;

/// Translate a `nagent_db::documents::DocumentError` into an
/// `AuthError` so the routes can `?` the documents DB through
/// the existing HTTP error mapping without a bespoke `Into`
/// impl in `nagent_db`.
impl From<DocumentError> for AuthError {
    fn from(e: DocumentError) -> Self {
        match e {
            DocumentError::Sqlx(inner) => AuthError::Database(inner.into()),
            DocumentError::SchemaMissing => AuthError::Internal(
                "uploaded_documents table is missing; run `nagent migrate up`".into(),
            ),
        }
    }
}

impl DocumentStore {
    /// `uploaded_documents` row for a given (user, session, name)
    /// triple. Returns `None` when no row matches so the caller
    /// can surface "unknown document" rather than an SQL error.
    pub async fn get_document_by_name(
        &self,
        name: &str,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<DocumentRow>, AuthError> {
        let id = Uuid::parse_str(name).map_err(|parse_err| {
            AuthError::BadRequest(format!(
                "document id `{name}` is not a valid UUID: {parse_err}"
            ))
        })?;
        let row = self
            .store()
            .db()
            .documents
            .get_by_id(id, user_id, Some(session_id))
            .await?;
        Ok(row)
    }

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
    ) -> Result<(), AuthError> {
        self.store()
            .db()
            .documents
            .insert(
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
            .await?;
        Ok(())
    }

    pub async fn list_documents_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<DocumentRow>, AuthError> {
        Ok(self
            .store()
            .db()
            .documents
            .for_user(user_id)
            .list_for_session(session_id)
            .await?)
    }

    pub async fn count_documents_for_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<u64, AuthError> {
        Ok(self
            .store()
            .db()
            .documents
            .for_user(user_id)
            .count_for_session(session_id)
            .await?)
    }

    pub async fn get_document_by_id(
        &self,
        id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<DocumentRow>, AuthError> {
        Ok(self
            .store()
            .db()
            .documents
            .get_by_id(id, user_id, None)
            .await?)
    }

    pub async fn delete_document(
        &self,
        id: Uuid,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<Option<PathBuf>, AuthError> {
        Ok(self
            .store()
            .db()
            .documents
            .for_user(user_id)
            .delete(id, session_id)
            .await?)
    }

    pub async fn sweep_older_than(
        &self,
        ttl: std::time::Duration,
    ) -> Result<Vec<DocumentRow>, AuthError> {
        Ok(self.store().db().documents.sweep_older_than(ttl).await?)
    }

    pub async fn delete_row_by_id(&self, id: Uuid) -> Result<(), AuthError> {
        Ok(self.store().db().documents.delete_row_by_id(id).await?)
    }
}
