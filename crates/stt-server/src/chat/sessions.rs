//! `chat_sessions` — server-bound chat session id binding.
//!
//! General-purpose module: lives at the crate root because the
//! concept is broader than the documents feature (a future plan
//! may scope agent conversations or chat-history entries to a
//! chat session too). The `documents` feature was the first
//! consumer; the SEV 2 audit flagged that
//! `X-Chat-Session-Id` was fully client-controlled and pushed
//! for a server-minted binding table.
//!
//! ## How the binding works
//!
//! 1. On page load, `chat.js` calls `POST /v1/chat/session` with
//!    the per-session CSRF token. The handler generates a fresh
//!    UUID, inserts it into `chat_sessions` bound to the
//!    authenticated user, and returns `{id}` to the browser.
//! 2. The browser stores the returned id in localStorage (so a
//!    page reload doesn't mint a new one) and uses it on every
//!    subsequent `/v1/documents*` and `/v1/chat/completions`
//!    request via the `X-Chat-Session-Id` header.
//! 3. Every documents route / agent invocation validates
//!    `(current_user, X-Chat-Session-Id)` against `chat_sessions`
//!    and rejects mismatches with `403 Forbidden` — a user
//!    cannot reach docs uploaded in another user's session.
//!
//! ## Why not just store the id in the auth cookie?
//!
//! Because each browser tab is a different chat session (the UI
//! uses a per-tab UUID as the chat session key) and the auth
//! cookie is shared across tabs. A table that binds
//! `(user_id, chat_session_id)` lets multiple tabs coexist
//! without one tab's docs leaking to the others.

use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::store::AuthStore;

/// Errors surfaced by the `chat_sessions` API. Mapped to HTTP
/// statuses by the route layer.
#[derive(Debug, thiserror::Error)]
pub enum ChatSessionError {
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error("chat session {0} is not bound to user {1}")]
    NotBound(Uuid, Uuid),
}

impl From<AuthError> for ChatSessionError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::Database(inner) => ChatSessionError::Sqlx(inner),
            other => ChatSessionError::Sqlx(sqlx::Error::Protocol(other.to_string())),
        }
    }
}

/// `ChatSessionError` → HTTP status mapping. `NotBound` surfaces
/// as `503` (the chat sessions subsystem is reachable but the
/// binding is missing — a misconfigured server or a stale id);
/// `Sqlx` becomes `500` with a generic message so we never leak
/// DB internals.
impl axum::response::IntoResponse for ChatSessionError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let (status, msg) = match &self {
            ChatSessionError::NotBound(_, _) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "chat sessions unavailable".to_string(),
            ),
            ChatSessionError::Sqlx(_) => {
                tracing::error!(error = %self, "chat_sessions DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "chat sessions DB error".to_string(),
                )
            }
        };
        axum::response::IntoResponse::into_response((status, msg))
    }
}

/// Cheap to clone (the inner `Arc` wraps the auth sqlx pool).
#[derive(Clone)]
pub struct ChatSessions {
    inner: Arc<ChatSessionsInner>,
}

struct ChatSessionsInner {
    store: AuthStore,
}

impl std::fmt::Debug for ChatSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatSessions")
            .field("store", &self.inner.store)
            .finish()
    }
}

impl ChatSessions {
    pub fn new(store: AuthStore) -> Self {
        Self {
            inner: Arc::new(ChatSessionsInner { store }),
        }
    }

    /// Bind `session_id` to `user_id` in the `chat_sessions`
    /// table. The browser calls this through
    /// `POST /v1/chat/session` (the server mints the UUID).
    pub async fn bind(&self, session_id: Uuid, user_id: Uuid) -> Result<(), ChatSessionError> {
        match &self.inner.store {
            AuthStore::Sqlite(s) => bind_sqlite(s.pool(), session_id, user_id).await,
            AuthStore::Postgres(s) => bind_postgres(s.pool(), session_id, user_id).await,
        }
    }

    /// Verify `session_id` is bound to `user_id` AND refresh its
    /// `last_seen_at`. Returns `Err(NotBound)` when the row is
    /// missing OR bound to a different user — the route layer
    /// maps both to `403 Forbidden` (same response shape so a
    /// probing caller cannot tell the difference).
    pub async fn touch_and_verify(
        &self,
        session_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), ChatSessionError> {
        match &self.inner.store {
            AuthStore::Sqlite(s) => touch_and_verify_sqlite(s.pool(), session_id, user_id).await,
            AuthStore::Postgres(s) => {
                touch_and_verify_postgres(s.pool(), session_id, user_id).await
            }
        }
    }
}

// ---- sqlite --------------------------------------------------------------

async fn bind_sqlite(
    pool: &sqlx::SqlitePool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(), ChatSessionError> {
    // `INSERT OR REPLACE` so re-binding the same (session_id,
    // user_id) is idempotent — useful when the user logs out
    // and back in. A different `user_id` would orphan the old
    // row; the FK ON DELETE CASCADE on `users.id` cleans up.
    sqlx::query(
        "INSERT OR REPLACE INTO chat_sessions (id, user_id, created_at, last_seen_at) \
         VALUES (?1, ?2, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
    )
    .bind(session_id.to_string())
    .bind(user_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

async fn touch_and_verify_sqlite(
    pool: &sqlx::SqlitePool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(), ChatSessionError> {
    // Atomic update — the WHERE user_id = ?2 clause is the
    // auth gate. If the row is missing OR owned by a different
    // user, the UPDATE matches zero rows and we surface
    // `NotBound`.
    let updated = sqlx::query(
        "UPDATE chat_sessions SET last_seen_at = CURRENT_TIMESTAMP \
         WHERE id = ?1 AND user_id = ?2",
    )
    .bind(session_id.to_string())
    .bind(user_id.to_string())
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ChatSessionError::NotBound(session_id, user_id));
    }
    Ok(())
}

// ---- postgres ------------------------------------------------------------

async fn bind_postgres(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(), ChatSessionError> {
    sqlx::query(
        "INSERT INTO chat_sessions (id, user_id, created_at, last_seen_at) \
         VALUES ($1, $2, NOW(), NOW()) \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, last_seen_at = NOW()",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn touch_and_verify_postgres(
    pool: &sqlx::PgPool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<(), ChatSessionError> {
    let updated = sqlx::query(
        "UPDATE chat_sessions SET last_seen_at = NOW() \
         WHERE id = $1 AND user_id = $2",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    if updated == 0 {
        return Err(ChatSessionError::NotBound(session_id, user_id));
    }
    Ok(())
}

/// Response shape of `POST /v1/chat/session`. The browser caches
/// the `id` in localStorage and reuses it across page reloads
/// until the server returns `403` (which then triggers a re-mint).
#[derive(Debug, Clone, Serialize)]
pub struct ChatSessionResponse {
    pub id: Uuid,
}

/// HTTP handler for `POST /v1/chat/session`. The browser calls
/// this on page load (and whenever the previous id was rejected
/// with 403) to get a server-minted UUID bound to the
/// authenticated user.
///
/// The route is gated by `RequireAuth` like every other
/// `/v1/documents*` route — `AuthUser.id` becomes the binding
/// key on `chat_sessions.user_id`. The handler is mounted on
/// the documents router so it shares the same auth envelope
/// without duplicating the middleware stack; the state type
/// is `DocumentHandlerState` (which embeds the full
/// `Arc<AppState>` under `.app`).
///
/// # Wire shape
///
/// ```http
/// POST /v1/chat/session
/// Cookie: nagent_session=<id>
/// x-csrf-token: <csrf>
///
/// {"id": "01234567-89ab-cdef-0123-456789abcdef"}
/// ```
pub async fn mint_handler(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::AppState>>,
    axum::Extension(user): axum::Extension<crate::auth::session::AuthUser>,
) -> Result<axum::Json<ChatSessionResponse>, ChatSessionError> {
    // SEV 2 fix: server-bound chat session id. We mint a fresh
    // UUID v4 server-side (the browser is not allowed to pick)
    // and bind it to the authenticated user. The browser
    // persists the returned id in localStorage and reuses it
    // across page reloads until the server rejects it with 403
    // (e.g. logout from another tab).
    let chat_sessions = state.chat_sessions.as_ref().ok_or_else(|| {
        ChatSessionError::Sqlx(sqlx::Error::Protocol(
            "chat_sessions handle is not wired".into(),
        ))
    })?;
    let id = uuid::Uuid::new_v4();
    chat_sessions.bind(id, user.id).await?;
    Ok(axum::Json(ChatSessionResponse { id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors_have_correct_display() {
        // Sanity: every variant carries enough info to drive
        // the IntoResponse impl without panicking on missing
        // fields.
        let id = Uuid::nil();
        let e = ChatSessionError::NotBound(id, id);
        assert!(e.to_string().contains("not bound"));
        // Sqlx variant is opaque — we don't assert on the
        // message but ensure Debug works.
        let _ = format!(
            "{:?}",
            ChatSessionError::Sqlx(sqlx::Error::Protocol("x".into()))
        );
    }
}
