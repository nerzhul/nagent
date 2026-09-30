//! Chat sessions repository — plan 5.D extraction.

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

pub mod sqlite {
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use crate::chat_sessions::ChatSessionError;

    #[derive(Clone, Debug)]
    pub struct SqliteChatSessions {
        pub(crate) pool: SqlitePool,
    }

    impl SqliteChatSessions {
        pub fn new(pool: SqlitePool) -> Self {
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

pub mod postgres {
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::chat_sessions::ChatSessionError;

    #[derive(Clone, Debug)]
    pub struct PgChatSessions {
        pub(crate) pool: PgPool,
    }

    impl PgChatSessions {
        pub fn new(pool: PgPool) -> Self {
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
