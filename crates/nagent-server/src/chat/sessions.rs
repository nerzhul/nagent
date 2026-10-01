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
//!
//! Plan 4.A: the SQL and the binding primitives live in
//! `nagent_db::chat_sessions`; this module only owns the HTTP
//! handler + the `axum` state wrapper.

use std::sync::Arc;

pub use nagent_db::chat_sessions::ChatSessionError;
use nagent_db::chat_sessions::ScopedChatSessions;
use serde::Serialize;
use uuid::Uuid;

/// HTTP error wrapper around [`ChatSessionError`]. The orphan
/// rule forbids `impl IntoResponse for ChatSessionError` (the
/// error type lives in `nagent_db`), so the routes use this
/// newtype as the response error.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(transparent)]
    Chat(#[from] ChatSessionError),
}

impl axum::response::IntoResponse for RouteError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        let (status, msg) = match &self {
            RouteError::Chat(ChatSessionError::NotBound(_, _)) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "chat sessions unavailable".to_string(),
            ),
            RouteError::Chat(ChatSessionError::Sqlx(_)) => {
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

/// Cheap to clone (the inner `Arc` wraps a `nagent_db::Db`).
#[derive(Clone)]
pub struct ChatSessions {
    inner: Arc<ChatSessionsInner>,
}

struct ChatSessionsInner {
    repo: nagent_db::chat_sessions::ChatSessions,
}

impl std::fmt::Debug for ChatSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatSessions")
            .field("repo", &"<nagent_db::chat_sessions::ChatSessions>")
            .finish()
    }
}

impl ChatSessions {
    /// Build a `ChatSessions` from the `ChatSessions` repository
    /// already owned by [`crate::AppState::chat_sessions`]'s
    /// underlying `AuthStore`.
    pub fn new(repo: nagent_db::chat_sessions::ChatSessions) -> Self {
        Self {
            inner: Arc::new(ChatSessionsInner { repo }),
        }
    }

    /// Bind `session_id` to `user_id` in the `chat_sessions`
    /// table. The browser calls this through
    /// `POST /v1/chat/session` (the server mints the UUID).
    pub async fn bind(&self, session_id: Uuid, user_id: Uuid) -> Result<(), ChatSessionError> {
        self.inner.repo.bind(session_id, user_id).await
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
        self.inner.repo.touch_and_verify(session_id, user_id).await
    }

    /// Return a scoped view of the repository bound to `user_id`.
    /// Route handlers that already hold an `AuthUser` should
    /// prefer this so the `user_id` filter cannot be dropped
    /// (plan 4.A S4).
    pub fn for_user(&self, user_id: Uuid) -> ScopedChatSessions {
        self.inner.repo.for_user(user_id)
    }
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
    axum::extract::State(state): axum::extract::State<crate::ChatSessionsState>,
    axum::Extension(user): axum::Extension<crate::auth::session::AuthUser>,
) -> Result<axum::Json<ChatSessionResponse>, RouteError> {
    // SEV 2 fix: server-bound chat session id. We mint a fresh
    // UUID v4 server-side (the browser is not allowed to pick)
    // and bind it to the authenticated user. The browser
    // persists the returned id in localStorage and reuses it
    // across page reloads until the server rejects it with 403
    // (e.g. logout from another tab).
    let id = uuid::Uuid::new_v4();
    // Use the scoped accessor so the binding cannot target a
    // different user (plan 4.A S4).
    state.sessions.for_user(user.id).bind(id).await?;
    Ok(axum::Json(ChatSessionResponse { id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_bound_error_carries_ids_in_message() {
        // Sanity: the variant carries enough info to drive the
        // IntoResponse impl + surface a useful diagnostic.
        let session = Uuid::nil();
        let user = Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap();
        let e = ChatSessionError::NotBound(session, user);
        let s = e.to_string();
        assert!(s.contains("not bound"));
        assert!(s.contains(&user.to_string()));
    }
}
