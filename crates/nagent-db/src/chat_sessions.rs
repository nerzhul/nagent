//! Chat sessions repository
//!
//! Binds `(user_id, session_id)` rows so the documents + agent
//! paths can refuse a mismatched `X-Chat-Session-Id` header with
//! the same shape whether the row is missing OR bound to a
//! different user (security plan SEV 2).
//!
//! Callers that have already resolved a `user_id` should prefer
//! [`ChatSessions::for_user`] so the `WHERE user_id = ?` filter
//! cannot be accidentally dropped .

use uuid::Uuid;

use crate::error::Error;
use crate::pool::AnyPool;

#[derive(Debug, thiserror::Error)]
pub enum ChatSessionError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("chat session {0} is not bound to user {1}")]
    NotBound(Uuid, Uuid),
}

impl From<Error> for ChatSessionError {
    fn from(e: Error) -> Self {
        match e {
            Error::Database(inner) => ChatSessionError::Sqlx(inner),
            other => ChatSessionError::Sqlx(sqlx::Error::Protocol(other.to_string())),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ChatSessions {
    Sqlite(sqlite::SqliteChatSessions),
    Postgres(postgres::PgChatSessions),
}

impl ChatSessions {
    pub fn new(pool: &AnyPool) -> Self {
        match pool {
            AnyPool::Sqlite(p) => Self::Sqlite(sqlite::SqliteChatSessions::new(p.clone())),
            AnyPool::Postgres(p) => Self::Postgres(postgres::PgChatSessions::new(p.clone())),
        }
    }

    /// Scope every per-row method to a single `user_id`.
    pub fn for_user(&self, user_id: Uuid) -> ScopedChatSessions {
        ScopedChatSessions {
            inner: self.clone(),
            user_id,
        }
    }

    pub async fn bind(&self, session_id: Uuid, user_id: Uuid) -> Result<(), ChatSessionError> {
        match self {
            ChatSessions::Sqlite(s) => s.bind(session_id, user_id).await,
            ChatSessions::Postgres(s) => s.bind(session_id, user_id).await,
        }
    }

    pub async fn touch_and_verify(
        &self,
        session_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), ChatSessionError> {
        match self {
            ChatSessions::Sqlite(s) => s.touch_and_verify(session_id, user_id).await,
            ChatSessions::Postgres(s) => s.touch_and_verify(session_id, user_id).await,
        }
    }
}

/// Per-user scoped view over [`ChatSessions`].
#[derive(Debug, Clone)]
pub struct ScopedChatSessions {
    inner: ChatSessions,
    user_id: Uuid,
}

impl ScopedChatSessions {
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Bind `session_id` to the scoped `user_id`.
    pub async fn bind(&self, session_id: Uuid) -> Result<(), ChatSessionError> {
        self.inner.bind(session_id, self.user_id).await
    }

    /// Verify `session_id` is bound to the scoped `user_id` AND
    /// refresh its `last_seen_at`.
    pub async fn touch_and_verify(&self, session_id: Uuid) -> Result<(), ChatSessionError> {
        self.inner.touch_and_verify(session_id, self.user_id).await
    }
}

pub(crate) mod sqlite {
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use crate::chat_sessions::ChatSessionError;

    #[derive(Clone, Debug)]
    pub struct SqliteChatSessions {
        pub pool: SqlitePool,
    }

    impl SqliteChatSessions {
        pub(crate) fn new(pool: SqlitePool) -> Self {
            Self { pool }
        }

        pub async fn bind(&self, session_id: Uuid, user_id: Uuid) -> Result<(), ChatSessionError> {
            sqlx::query(
                "INSERT OR REPLACE INTO chat_sessions (id, user_id, created_at, last_seen_at) \
                 VALUES (?1, ?2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
            )
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        pub async fn touch_and_verify(
            &self,
            session_id: Uuid,
            user_id: Uuid,
        ) -> Result<(), ChatSessionError> {
            let updated = sqlx::query(
                "UPDATE chat_sessions SET last_seen_at = CURRENT_TIMESTAMP \
                 WHERE id = ?1 AND user_id = ?2",
            )
            .bind(session_id.to_string())
            .bind(user_id.to_string())
            .execute(&self.pool)
            .await?
            .rows_affected();
            if updated == 0 {
                return Err(ChatSessionError::NotBound(session_id, user_id));
            }
            Ok(())
        }
    }
}

pub(crate) mod postgres {
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::chat_sessions::ChatSessionError;

    #[derive(Clone, Debug)]
    pub struct PgChatSessions {
        pub pool: PgPool,
    }

    impl PgChatSessions {
        pub(crate) fn new(pool: PgPool) -> Self {
            Self { pool }
        }

        pub async fn bind(&self, session_id: Uuid, user_id: Uuid) -> Result<(), ChatSessionError> {
            sqlx::query(
                "INSERT INTO chat_sessions (id, user_id, created_at, last_seen_at) \
                 VALUES ($1, $2, NOW(), NOW()) \
                 ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, last_seen_at = NOW()",
            )
            .bind(session_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
            Ok(())
        }

        pub async fn touch_and_verify(
            &self,
            session_id: Uuid,
            user_id: Uuid,
        ) -> Result<(), ChatSessionError> {
            let updated = sqlx::query(
                "UPDATE chat_sessions SET last_seen_at = NOW() \
                 WHERE id = $1 AND user_id = $2",
            )
            .bind(session_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
            if updated == 0 {
                return Err(ChatSessionError::NotBound(session_id, user_id));
            }
            Ok(())
        }
    }
}
