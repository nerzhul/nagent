//! `chat_messages` repository.
//!
//! Per-session chat message store (plan 1791464974103). The table
//! is the source of truth for messages in the Discussion-mode UI
//! — localStorage becomes a hydration cache. A3 (edit / regenerate)
//! needs a stable per-message id and optimistic concurrency on
//! edits, both of which require a server-side row.
//!
//! ## Ownership
//!
//! Every row is keyed on `(user_id, session_id)`. Routes verify the
//! session belongs to the calling user through the existing
//! [`crate::chat_sessions::ChatSessions::touch_and_verify`] helper,
//! then only ever touch rows scoped to that pair. The
//! [`ScopedChatMessages`] view does not take a `user_id` argument
//! on its per-row methods, so the `WHERE user_id = ?` filter cannot
//! be accidentally dropped.
//!
//! ## Dual-backend
//!
//! Same `Inner { Sqlite, Postgres }` pattern as
//! [`crate::chat_sessions`]. The only engine-divergent bit is the
//! placeholder syntax (`?` vs `$N`) — kept inside the per-engine
//! sub-modules so the enum dispatch stays mechanical.

use uuid::Uuid;

use crate::chat_sessions::ChatSessionError;
use crate::error::Error;
use crate::pool::AnyPool;

/// Outcome of a per-row write. Surfaces the row's new `version`
/// so the caller (the HTTP handler) can echo it back in the
/// `Location` header / response body and the next edit can use it
/// for the optimistic-concurrency check.
#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub ordinal: i64,
    pub version: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("chat message {0} not found in session {1}")]
    NotFound(Uuid, Uuid),
    #[error("chat message {0} is not owned by user {1}")]
    NotOwned(Uuid, Uuid),
    #[error("version mismatch on message {0} (expected {1}, found {2})")]
    VersionMismatch(Uuid, i32, i32),
    #[error("invalid role: {0} (expected user|assistant|system)")]
    BadRole(String),
    /// Wraps a session-binding failure so the route layer can
    /// surface the same 403 shape as `ChatSessionError::NotBound`
    /// (a probing caller cannot tell session / message ownership
    /// apart — that property is the whole point of folding the
    /// two into a single 403 mapping).
    #[error(transparent)]
    Chat(#[from] ChatSessionError),
}

impl From<Error> for MessageError {
    fn from(e: Error) -> Self {
        match e {
            Error::Database(inner) => MessageError::Sqlx(inner),
            other => MessageError::Sqlx(sqlx::Error::Protocol(other.to_string())),
        }
    }
}

/// Input shape for [`ScopedChatMessages::append`]. The `id` /
/// `ts` / `version` are server-minted; the caller provides the
/// content the user just sent.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub id: Uuid,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChatMessages {
    Sqlite(sqlite::SqliteChatMessages),
    Postgres(postgres::PgChatMessages),
}

impl ChatMessages {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteChatMessages::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgChatMessages::new(p.clone())),
        }
    }

    /// Scope per-user operations to a single `user_id`. The
    /// returned [`ScopedChatMessages`] does not take a `user_id`
    /// argument on its per-row methods so a handler holding a
    /// scoped view cannot accidentally read or mutate another
    /// user's messages.
    pub fn for_user(&self, user_id: Uuid) -> ScopedChatMessages {
        ScopedChatMessages {
            inner: self.clone(),
            user_id,
        }
    }
}

/// Per-user scoped view over [`ChatMessages`]. Every per-row
/// method filters by the scoped `user_id` automatically.
#[derive(Debug, Clone)]
pub struct ScopedChatMessages {
    inner: ChatMessages,
    user_id: Uuid,
}

impl ScopedChatMessages {
    /// `user_id` this view is bound to. Surfaced for tests +
    /// diagnostic logs that need to echo the binding.
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// List every message for `session_id`, oldest first. The
    /// caller is expected to have verified the session binding
    /// through [`crate::chat_sessions::ChatSessions::touch_and_verify`]
    /// before calling; this method does not re-check it (the
    /// scoped `user_id` filter is the only ownership gate).
    pub async fn list(&self, session_id: Uuid) -> Result<Vec<MessageRecord>, MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => s.list(self.user_id, session_id).await,
            ChatMessages::Postgres(s) => s.list(self.user_id, session_id).await,
        }
    }

    /// Fetch a single message by id. Returns `NotFound` if the
    /// row does not exist OR is owned by a different user — the
    /// route layer maps both to 403/404, never 200, so a probing
    /// caller cannot tell the two cases apart.
    pub async fn get(
        &self,
        session_id: Uuid,
        message_id: Uuid,
    ) -> Result<MessageRecord, MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => s.get(self.user_id, session_id, message_id).await,
            ChatMessages::Postgres(s) => s.get(self.user_id, session_id, message_id).await,
        }
    }

    /// Append a new message to `session_id`. The row is keyed on
    /// `(user_id, session_id)` so the scoped view is the only
    /// ownership gate; the caller still has to verify the session
    /// binding first (see [`crate::chat_sessions`]).
    ///
    /// The next `ordinal` is the current `MAX(ordinal) + 1` for
    /// the session, or `1` for an empty session. The new row's
    /// `version` starts at `1` (the default).
    pub async fn append(
        &self,
        session_id: Uuid,
        msg: NewMessage,
    ) -> Result<MessageRecord, MessageError> {
        if !matches!(msg.role.as_str(), "user" | "assistant" | "system") {
            return Err(MessageError::BadRole(msg.role));
        }
        match &self.inner {
            ChatMessages::Sqlite(s) => s.append(self.user_id, session_id, msg).await,
            ChatMessages::Postgres(s) => s.append(self.user_id, session_id, msg).await,
        }
    }

    /// Edit a single message in place. The `expected_version`
    /// check is the optimistic-concurrency gate: if the row's
    /// current `version` does not match, the call returns
    /// [`MessageError::VersionMismatch`] and the row is left
    /// untouched. On success the new `version` is `expected + 1`.
    pub async fn edit(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        new_content: &str,
        expected_version: i32,
    ) -> Result<MessageRecord, MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => {
                s.edit(
                    self.user_id,
                    session_id,
                    message_id,
                    new_content,
                    expected_version,
                )
                .await
            }
            ChatMessages::Postgres(s) => {
                s.edit(
                    self.user_id,
                    session_id,
                    message_id,
                    new_content,
                    expected_version,
                )
                .await
            }
        }
    }

    /// Delete a single message. Returns `NotFound` if the row
    /// does not exist OR is owned by a different user.
    pub async fn delete(&self, session_id: Uuid, message_id: Uuid) -> Result<(), MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => s.delete(self.user_id, session_id, message_id).await,
            ChatMessages::Postgres(s) => s.delete(self.user_id, session_id, message_id).await,
        }
    }

    /// Delete every message in `session_id` whose `ordinal` is
    /// strictly greater than `cutoff_ordinal`. Returns the number
    /// of rows removed. Used by the A3 edit flow to drop the
    /// trailing assistant turn before re-sending the user's edit
    /// (one atomic SQL statement, no half-state on failure).
    pub async fn truncate_after(
        &self,
        session_id: Uuid,
        cutoff_ordinal: i64,
    ) -> Result<u64, MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => {
                s.truncate_after(self.user_id, session_id, cutoff_ordinal)
                    .await
            }
            ChatMessages::Postgres(s) => {
                s.truncate_after(self.user_id, session_id, cutoff_ordinal)
                    .await
            }
        }
    }

    /// Find the row with the highest `ordinal` in `session_id`
    /// (regardless of role) and return its id. Returns `None`
    /// for an empty session. Used by the A3 regenerate flow to
    /// find the last assistant row to drop.
    pub async fn last_ordinal(
        &self,
        session_id: Uuid,
    ) -> Result<Option<(Uuid, i64, String)>, MessageError> {
        match &self.inner {
            ChatMessages::Sqlite(s) => s.last_ordinal(self.user_id, session_id).await,
            ChatMessages::Postgres(s) => s.last_ordinal(self.user_id, session_id).await,
        }
    }
}

pub(crate) mod sqlite {
    use chrono::{DateTime, Utc};
    use sqlx::{Row, SqlitePool};
    use uuid::Uuid;

    use crate::chat_messages::{MessageError, MessageRecord, NewMessage};

    #[derive(Clone, Debug)]
    pub struct SqliteChatMessages {
        pub pool: SqlitePool,
    }

    impl SqliteChatMessages {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn list(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Vec<MessageRecord>, MessageError> {
            let rows = sqlx::query(
                "SELECT id, session_id, user_id, role, content, model, ts, ordinal, version \
                 FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 \
                 ORDER BY ordinal ASC",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(row_to_record).collect()
        }

        pub async fn get(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
        ) -> Result<MessageRecord, MessageError> {
            let row = sqlx::query(
                "SELECT id, session_id, user_id, role, content, model, ts, ordinal, version \
                 FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 AND id = ?3",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .bind(message_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            match row {
                Some(r) => row_to_record(r),
                None => {
                    // The row is either missing or owned by
                    // another user. Same 403/404 mapping at the
                    // route layer — `NotFound` carries the
                    // (message, session) pair so the error
                    // message is useful in logs.
                    let _ = user_id;
                    Err(MessageError::NotFound(message_id, session_id))
                }
            }
        }

        pub async fn append(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            msg: NewMessage,
        ) -> Result<MessageRecord, MessageError> {
            // Compute the next ordinal in the same statement so a
            // concurrent append from the same session cannot
            // produce a duplicate `ordinal` (the `(session_id,
            // ordinal)` index would refuse the second insert).
            // SQLite's `COALESCE(MAX(ordinal), 0) + 1` is
            // single-statement atomic at the engine level; no
            // explicit transaction needed.
            let next_ordinal: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .fetch_one(&self.pool)
            .await?;
            let now = Utc::now();
            sqlx::query(
                "INSERT INTO chat_messages \
                 (id, session_id, user_id, role, content, model, ts, ordinal, version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)",
            )
            .bind(msg.id.to_string())
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(msg.model.as_deref())
            .bind(now.to_rfc3339())
            .bind(next_ordinal)
            .execute(&self.pool)
            .await?;
            Ok(MessageRecord {
                id: msg.id,
                session_id,
                user_id,
                role: msg.role,
                content: msg.content,
                model: msg.model,
                ts: now,
                ordinal: next_ordinal,
                version: 1,
            })
        }

        pub async fn edit(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
            new_content: &str,
            expected_version: i32,
        ) -> Result<MessageRecord, MessageError> {
            // Read the current row to distinguish "row does not
            // exist" from "version mismatch" — both surface as
            // different errors so the route layer can map to
            // 404 / 409 respectively.
            let row = sqlx::query(
                "SELECT version FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 AND id = ?3",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .bind(message_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Err(MessageError::NotFound(message_id, session_id));
            };
            let current_version: i32 = r.try_get("version")?;
            if current_version != expected_version {
                return Err(MessageError::VersionMismatch(
                    message_id,
                    expected_version,
                    current_version,
                ));
            }
            sqlx::query(
                "UPDATE chat_messages \
                 SET content = ?1, version = version + 1 \
                 WHERE user_id = ?2 AND session_id = ?3 AND id = ?4",
            )
            .bind(new_content)
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .bind(message_id.to_string())
            .execute(&self.pool)
            .await?;
            // Re-read the row so the returned record reflects
            // the post-update state.
            self.get(user_id, session_id, message_id).await
        }

        pub async fn delete(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
        ) -> Result<(), MessageError> {
            let res = sqlx::query(
                "DELETE FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 AND id = ?3",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .bind(message_id.to_string())
            .execute(&self.pool)
            .await?;
            if res.rows_affected() == 0 {
                return Err(MessageError::NotFound(message_id, session_id));
            }
            Ok(())
        }

        pub async fn truncate_after(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            cutoff_ordinal: i64,
        ) -> Result<u64, MessageError> {
            let res = sqlx::query(
                "DELETE FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 AND ordinal > ?3",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .bind(cutoff_ordinal)
            .execute(&self.pool)
            .await?;
            Ok(res.rows_affected())
        }

        pub async fn last_ordinal(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Option<(Uuid, i64, String)>, MessageError> {
            let row = sqlx::query(
                "SELECT id, ordinal, role FROM chat_messages \
                 WHERE user_id = ?1 AND session_id = ?2 \
                 ORDER BY ordinal DESC LIMIT 1",
            )
            .bind(user_id.to_string())
            .bind(session_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            let id: String = r.try_get("id")?;
            let ordinal: i64 = r.try_get("ordinal")?;
            let role: String = r.try_get("role")?;
            let id = Uuid::parse_str(&id).expect("DB UUID must parse");
            Ok(Some((id, ordinal, role)))
        }
    }

    fn row_to_record(r: sqlx::sqlite::SqliteRow) -> Result<MessageRecord, MessageError> {
        let id: String = r.try_get("id")?;
        let session_id: String = r.try_get("session_id")?;
        let user_id: String = r.try_get("user_id")?;
        let ts: String = r.try_get("ts")?;
        let ts = DateTime::parse_from_rfc3339(&ts)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        Ok(MessageRecord {
            id: Uuid::parse_str(&id).expect("DB UUID must parse"),
            session_id: Uuid::parse_str(&session_id).expect("DB UUID must parse"),
            user_id: Uuid::parse_str(&user_id).expect("DB UUID must parse"),
            role: r.try_get("role")?,
            content: r.try_get("content")?,
            model: r.try_get("model")?,
            ts,
            ordinal: r.try_get("ordinal")?,
            version: r.try_get("version")?,
        })
    }
}

pub(crate) mod postgres {
    use chrono::{DateTime, Utc};
    use sqlx::{PgPool, Row};
    use uuid::Uuid;

    use crate::chat_messages::{MessageError, MessageRecord, NewMessage};

    #[derive(Clone, Debug)]
    pub struct PgChatMessages {
        pub pool: PgPool,
    }

    impl PgChatMessages {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn list(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Vec<MessageRecord>, MessageError> {
            let rows = sqlx::query(
                "SELECT id::text AS id, session_id::text AS session_id, \
                        user_id::text AS user_id, role, content, model, \
                        ts, ordinal, version \
                 FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 \
                 ORDER BY ordinal ASC",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_all(&self.pool)
            .await?;
            rows.into_iter().map(row_to_record).collect()
        }

        pub async fn get(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
        ) -> Result<MessageRecord, MessageError> {
            let row = sqlx::query(
                "SELECT id::text AS id, session_id::text AS session_id, \
                        user_id::text AS user_id, role, content, model, \
                        ts, ordinal, version \
                 FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 AND id = $3",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Err(MessageError::NotFound(message_id, session_id));
            };
            row_to_record(r)
        }

        pub async fn append(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            msg: NewMessage,
        ) -> Result<MessageRecord, MessageError> {
            // Postgres exposes `RETURNING` so the round-trip is
            // a single statement. The `COALESCE(MAX(ordinal), 0)
            // + 1` subquery is wrapped in a CTE so it runs
            // atomically with the INSERT; the existing
            // `(session_id, ordinal)` index refuses a duplicate
            // (defence in depth — the CTE is sufficient on its
            // own at the default isolation level).
            let row = sqlx::query(
                "WITH next AS ( \
                     SELECT COALESCE(MAX(ordinal), 0) + 1 AS ord \
                     FROM chat_messages \
                     WHERE user_id = $1 AND session_id = $2 \
                 ) \
                 INSERT INTO chat_messages \
                 (id, session_id, user_id, role, content, model, ts, ordinal, version) \
                 SELECT $3, $4, $5, $6, $7, $8, NOW(), next.ord, 1 FROM next \
                 RETURNING id::text AS id, session_id::text AS session_id, \
                           user_id::text AS user_id, role, content, model, \
                           ts, ordinal, version",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(msg.id)
            .bind(session_id)
            .bind(user_id)
            .bind(&msg.role)
            .bind(&msg.content)
            .bind(msg.model.as_deref())
            .fetch_one(&self.pool)
            .await?;
            row_to_record(row)
        }

        pub async fn edit(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
            new_content: &str,
            expected_version: i32,
        ) -> Result<MessageRecord, MessageError> {
            // Read the current row first so the error mapping
            // matches sqlite (NotFound vs VersionMismatch).
            let row = sqlx::query(
                "SELECT version FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 AND id = $3",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else {
                return Err(MessageError::NotFound(message_id, session_id));
            };
            let current_version: i32 = r.try_get("version")?;
            if current_version != expected_version {
                return Err(MessageError::VersionMismatch(
                    message_id,
                    expected_version,
                    current_version,
                ));
            }
            sqlx::query(
                "UPDATE chat_messages \
                 SET content = $1, version = version + 1 \
                 WHERE user_id = $2 AND session_id = $3 AND id = $4",
            )
            .bind(new_content)
            .bind(user_id)
            .bind(session_id)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
            self.get(user_id, session_id, message_id).await
        }

        pub async fn delete(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            message_id: Uuid,
        ) -> Result<(), MessageError> {
            let res = sqlx::query(
                "DELETE FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 AND id = $3",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(message_id)
            .execute(&self.pool)
            .await?;
            if res.rows_affected() == 0 {
                return Err(MessageError::NotFound(message_id, session_id));
            }
            Ok(())
        }

        pub async fn truncate_after(
            &self,
            user_id: Uuid,
            session_id: Uuid,
            cutoff_ordinal: i64,
        ) -> Result<u64, MessageError> {
            let res = sqlx::query(
                "DELETE FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 AND ordinal > $3",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(cutoff_ordinal)
            .execute(&self.pool)
            .await?;
            Ok(res.rows_affected())
        }

        pub async fn last_ordinal(
            &self,
            user_id: Uuid,
            session_id: Uuid,
        ) -> Result<Option<(Uuid, i64, String)>, MessageError> {
            let row = sqlx::query(
                "SELECT id, ordinal, role FROM chat_messages \
                 WHERE user_id = $1 AND session_id = $2 \
                 ORDER BY ordinal DESC LIMIT 1",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(r) = row else { return Ok(None) };
            let id: Uuid = r.try_get("id")?;
            let ordinal: i64 = r.try_get("ordinal")?;
            let role: String = r.try_get("role")?;
            Ok(Some((id, ordinal, role)))
        }
    }

    fn row_to_record(r: sqlx::postgres::PgRow) -> Result<MessageRecord, MessageError> {
        let ts: DateTime<Utc> = r.try_get("ts")?;
        Ok(MessageRecord {
            id: r.try_get("id")?,
            session_id: r.try_get("session_id")?,
            user_id: r.try_get("user_id")?,
            role: r.try_get("role")?,
            content: r.try_get("content")?,
            model: r.try_get("model")?,
            ts,
            ordinal: r.try_get("ordinal")?,
            version: r.try_get("version")?,
        })
    }
}

#[cfg(test)]
mod tests {
    //! Minimal smoke tests against an in-memory sqlite pool. The
    //! HTTP integration tests in `tests/chat_messages.rs` cover
    //! the full surface; these only lock the per-engine SQL
    //! shape so a refactor of the dispatch enum surfaces here
    //! first.

    use uuid::Uuid;

    use super::*;

    async fn fresh() -> crate::Db {
        let opts = crate::DbOptions {
            backend: crate::DbEngine::Sqlite,
            url: "sqlite::memory:".into(),
            max_connections: 1,
            auto_migrate: false,
        };
        let db = crate::Db::connect(&opts).await.expect("db connects");
        db.migrate().await.expect("migrations run");
        db
    }

    async fn seed_user(db: &crate::Db, user_id: Uuid) {
        // Same minimal user shape the chat_sessions tests use.
        // `provider = 'local'` + a dummy argon2 hash satisfies the
        // NOT NULL columns on `users`; we never verify the
        // password in these tests. `raw_insert_one_str` is gated
        // behind the `test-util` cargo feature, which the
        // test-only build enables for the entire crate.
        let now = chrono::Utc::now().to_rfc3339();
        let params: [&str; 5] = [
            &user_id.to_string(),
            &format!("{user_id}@test.invalid"),
            "Test User",
            "test-only-hash",
            &now,
        ];
        db.raw_insert_one_str(
            "INSERT INTO users (id, email, display_name, provider, password_hash, created_at) \
             VALUES (?1, ?2, ?3, 'local', ?4, ?5)",
            &params,
        )
        .await
        .expect("users insert must succeed");
    }

    #[tokio::test]
    async fn append_and_list_round_trip() {
        let db = fresh().await;
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();
        seed_user(&db, user).await;
        db.admin().chat_sessions.bind(session, user).await.unwrap();
        let messages = db.for_user(user).chat_messages();
        let m1 = messages
            .append(
                session,
                NewMessage {
                    id: Uuid::new_v4(),
                    role: "user".into(),
                    content: "hello".into(),
                    model: None,
                },
            )
            .await
            .expect("first append");
        let m2 = messages
            .append(
                session,
                NewMessage {
                    id: Uuid::new_v4(),
                    role: "assistant".into(),
                    content: "hi".into(),
                    model: Some("llama3.1".into()),
                },
            )
            .await
            .expect("second append");
        let list = messages.list(session).await.expect("list");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, m1.id);
        assert_eq!(list[0].ordinal, 1);
        assert_eq!(list[1].id, m2.id);
        assert_eq!(list[1].ordinal, 2);
        assert_eq!(list[1].version, 1);
    }

    #[tokio::test]
    async fn edit_bumps_version_and_rejects_mismatch() {
        let db = fresh().await;
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();
        seed_user(&db, user).await;
        db.admin().chat_sessions.bind(session, user).await.unwrap();
        let messages = db.for_user(user).chat_messages();
        let row = messages
            .append(
                session,
                NewMessage {
                    id: Uuid::new_v4(),
                    role: "user".into(),
                    content: "first".into(),
                    model: None,
                },
            )
            .await
            .unwrap();
        let updated = messages
            .edit(session, row.id, "second", row.version)
            .await
            .unwrap();
        assert_eq!(updated.version, row.version + 1);
        assert_eq!(updated.content, "second");
        // Stale version -> VersionMismatch.
        let err = messages
            .edit(session, row.id, "third", row.version)
            .await
            .expect_err("stale version must fail");
        assert!(matches!(err, MessageError::VersionMismatch(_, _, _)));
    }

    #[tokio::test]
    async fn truncate_after_drops_tail_in_one_statement() {
        let db = fresh().await;
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();
        seed_user(&db, user).await;
        db.admin().chat_sessions.bind(session, user).await.unwrap();
        let messages = db.for_user(user).chat_messages();
        for i in 0..3 {
            messages
                .append(
                    session,
                    NewMessage {
                        id: Uuid::new_v4(),
                        role: if i % 2 == 0 {
                            "user".into()
                        } else {
                            "assistant".into()
                        },
                        content: format!("m{i}"),
                        model: None,
                    },
                )
                .await
                .unwrap();
        }
        let removed = messages.truncate_after(session, 1).await.unwrap();
        assert_eq!(removed, 2);
        let list = messages.list(session).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].content, "m0");
    }

    #[tokio::test]
    async fn scoped_view_cannot_read_another_users_messages() {
        let db = fresh().await;
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let session = Uuid::new_v4();
        seed_user(&db, alice).await;
        seed_user(&db, bob).await;
        db.admin().chat_sessions.bind(session, alice).await.unwrap();
        let alice_messages = db.for_user(alice).chat_messages();
        let row = alice_messages
            .append(
                session,
                NewMessage {
                    id: Uuid::new_v4(),
                    role: "user".into(),
                    content: "secret".into(),
                    model: None,
                },
            )
            .await
            .unwrap();
        // Bob's scoped view must see zero rows for the same
        // session.
        let bob_messages = db.for_user(bob).chat_messages();
        let list = bob_messages.list(session).await.unwrap();
        assert_eq!(list.len(), 0);
        // And a direct `get` must surface NotFound, never the row.
        let err = bob_messages.get(session, row.id).await.unwrap_err();
        assert!(matches!(err, MessageError::NotFound(_, _)));
    }

    #[tokio::test]
    async fn append_rejects_unknown_role() {
        let db = fresh().await;
        let user = Uuid::new_v4();
        let session = Uuid::new_v4();
        seed_user(&db, user).await;
        db.admin().chat_sessions.bind(session, user).await.unwrap();
        let messages = db.for_user(user).chat_messages();
        let err = messages
            .append(
                session,
                NewMessage {
                    id: Uuid::new_v4(),
                    role: "tool".into(),
                    content: "x".into(),
                    model: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MessageError::BadRole(_)));
    }
}
