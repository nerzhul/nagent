//! `chat_messages` HTTP handlers (plan 1791464974103).
//!
//! Five routes, all `RequireAuth` + CSRF on state-changing methods:
//!
//! | Method | Path                                                | Action                                |
//! |-------:|-----------------------------------------------------|---------------------------------------|
//! | GET    | `/v1/chat/session/:sid/messages`                    | List messages, oldest first           |
//! | POST   | `/v1/chat/session/:sid/messages`                    | Append `{role, content, model?}`      |
//! | PATCH  | `/v1/chat/session/:sid/messages/:mid`               | Edit content, requires `version`      |
//! | DELETE | `/v1/chat/session/:sid/messages/:mid`               | Delete one message                    |
//! | POST   | `/v1/chat/session/:sid/regenerate`                  | Delete the last assistant message     |
//!
//! The streaming path (`/v1/chat/completions`) is unchanged: the
//! browser keeps using it to stream the LLM reply, then persists
//! the finalised message via `POST /messages` once the stream
//! closes. The edit / regenerate flow re-uses the same
//! `streamReply` after a server-side `truncate_after` so the
//! LLM-side change is zero — only the persisted row count moves.
//!
//! ## Wire shape
//!
//! ```http
//! POST /v1/chat/session/0123…/messages
//! Cookie: nagent_session=<id>
//! x-csrf-token: <csrf>
//! X-Chat-Session-Id: 0123… (kept for backwards-compat with the
//!                       documents routes; the path id is the
//!                       authoritative one)
//! Content-Type: application/json
//!
//! {"role": "user", "content": "Hello"}
//!
//! 201 Created
//! { "id": "…", "session_id": "…", "role": "user", "content": "Hello",
//!   "model": null, "ts": "2026-…", "ordinal": 4, "version": 1 }
//! ```

use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::session::AuthUser;

use nagent_db::chat_messages::ScopedChatMessages;
pub use nagent_db::chat_messages::{MessageError, MessageRecord, NewMessage};

use super::sessions::ChatSessions;

/// HTTP error wrapper around [`MessageError`]. The orphan rule
/// forbids `impl IntoResponse for MessageError` (the error type
/// lives in `nagent_db`), so the routes use this newtype as the
/// response error. The wire shape mirrors `chat::sessions::RouteError`
/// — same `status + plaintext body`, no JSON envelope.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error(transparent)]
    Message(#[from] MessageError),
    #[error(transparent)]
    Chat(#[from] crate::chat::sessions::ChatSessionError),
    #[error("missing required field: {0}")]
    BadRequest(String),
    #[error("forbidden")]
    Forbidden,
}

impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            // `NotBound` is the SEV 2 contract: missing row and
            // wrong-user row are indistinguishable from the
            // outside. We surface 403 to keep the chat_sessions
            // property intact.
            RouteError::Chat(crate::chat::sessions::ChatSessionError::NotBound(_, _)) => {
                (StatusCode::FORBIDDEN, "forbidden".to_string())
            }
            RouteError::Chat(crate::chat::sessions::ChatSessionError::Sqlx(_)) => {
                tracing::error!(error = %self, "chat_sessions DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "chat_sessions DB error".to_string(),
                )
            }
            RouteError::Message(MessageError::NotFound(_, _)) => {
                (StatusCode::NOT_FOUND, "message not found".to_string())
            }
            RouteError::Message(MessageError::NotOwned(_, _)) => {
                // Same 403 shape as `NotBound` — a probing caller
                // cannot tell the two cases apart.
                (StatusCode::FORBIDDEN, "forbidden".to_string())
            }
            RouteError::Message(MessageError::VersionMismatch(_, _, _)) => (
                StatusCode::CONFLICT,
                "version mismatch — reload to see latest".to_string(),
            ),
            RouteError::Message(MessageError::BadRole(_)) => (
                StatusCode::BAD_REQUEST,
                "role must be one of: user, assistant, system".to_string(),
            ),
            RouteError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            RouteError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            RouteError::Message(MessageError::Chat(
                crate::chat::sessions::ChatSessionError::NotBound(_, _),
            )) => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            RouteError::Message(MessageError::Chat(
                crate::chat::sessions::ChatSessionError::Sqlx(_),
            )) => {
                tracing::error!(error = %self, "chat_sessions DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "chat_sessions DB error".to_string(),
                )
            }
            RouteError::Message(MessageError::Sqlx(_)) => {
                tracing::error!(error = %self, "chat_messages DB error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "chat_messages DB error".to_string(),
                )
            }
        };
        (status, msg).into_response()
    }
}

/// Cheap to clone (the inner `Arc` wraps a `nagent_db::Db`).
#[derive(Clone)]
pub struct ChatMessages {
    inner: Arc<ChatMessagesInner>,
}

struct ChatMessagesInner {
    repo: nagent_db::chat_messages::ChatMessages,
    sessions: ChatSessions,
}

impl std::fmt::Debug for ChatMessages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatMessages")
            .field("repo", &"<nagent_db::chat_messages::ChatMessages>")
            .field("sessions", &"<ChatSessions>")
            .finish()
    }
}

impl ChatMessages {
    /// Build a `ChatMessages` from the typed repositories
    /// already owned by [`crate::AppState::chat_messages`].
    pub fn new(repo: nagent_db::chat_messages::ChatMessages, sessions: ChatSessions) -> Self {
        Self {
            inner: Arc::new(ChatMessagesInner { repo, sessions }),
        }
    }

    /// Scoped view bound to `user_id`. Handlers that already hold
    /// an `AuthUser` should prefer this so the `user_id` filter
    /// cannot be dropped.
    pub fn for_user(&self, user_id: Uuid) -> ScopedChatMessages {
        self.inner.repo.for_user(user_id)
    }

    /// Verify the `(user_id, session_id)` binding. The session
    /// mint route (see [`super::sessions`]) bound it earlier;
    /// this helper re-checks the binding AND refreshes
    /// `last_seen_at` on every per-row write. A probing caller
    /// gets a 403 either way.
    pub async fn verify_session(
        &self,
        user_id: Uuid,
        session_id: Uuid,
    ) -> Result<(), crate::chat::sessions::ChatSessionError> {
        self.inner
            .sessions
            .touch_and_verify(session_id, user_id)
            .await
    }
}

/// Response shape for `POST /v1/chat/session/:sid/messages` /
/// `GET` (single message) / `PATCH`. Mirrors
/// [`MessageRecord`]; the explicit `Serialize` impl keeps the
/// wire shape stable even if the DB row type grows a new field.
#[derive(Debug, Clone, Serialize)]
pub struct MessageResponse {
    pub id: Uuid,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub ts: String,
    pub ordinal: i64,
    pub version: i32,
}

impl From<MessageRecord> for MessageResponse {
    fn from(r: MessageRecord) -> Self {
        Self {
            id: r.id,
            session_id: r.session_id,
            user_id: r.user_id,
            role: r.role,
            content: r.content,
            model: r.model,
            ts: r.ts.to_rfc3339(),
            ordinal: r.ordinal,
            version: r.version,
        }
    }
}

/// Request body for `POST /v1/chat/session/:sid/messages`. The
/// `id` is server-minted (the client never gets to pick); the
/// `model` is optional and ignored for `role = "user"`.
#[derive(Debug, Deserialize)]
pub struct AppendRequest {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request body for `PATCH /v1/chat/session/:sid/messages/:mid`.
/// The `version` is the optimistic-concurrency token the client
/// read from a prior `GET` or `POST` response.
#[derive(Debug, Deserialize)]
pub struct EditRequest {
    pub content: String,
    pub version: i32,
}

// ---- GET /v1/chat/session/:sid/messages ----------------------------------

/// List every message in the session, oldest first.
pub async fn list_handler(
    State(state): State<crate::ChatMessagesState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    AxPath(session_id): AxPath<Uuid>,
) -> Result<Response, RouteError> {
    state
        .messages
        .verify_session(user.id, session_id)
        .await
        .map_err(RouteError::from)?;
    let messages = state.messages.for_user(user.id).list(session_id).await?;
    let out: Vec<MessageResponse> = messages.into_iter().map(MessageResponse::from).collect();
    Ok(Json(serde_json::json!({ "data": out })).into_response())
}

// ---- POST /v1/chat/session/:sid/messages ---------------------------------

/// Append a new message. The server mints the id and the
/// `ordinal`; the client just provides `role`, `content`, and
/// the optional `model`.
pub async fn append_handler(
    State(state): State<crate::ChatMessagesState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    AxPath(session_id): AxPath<Uuid>,
    Json(req): Json<AppendRequest>,
) -> Result<Response, RouteError> {
    crate::auth::middleware::check_csrf(&headers, &user).map_err(|_| RouteError::Forbidden)?;
    state
        .messages
        .verify_session(user.id, session_id)
        .await
        .map_err(RouteError::from)?;
    let role = req.role.trim().to_string();
    if !matches!(role.as_str(), "user" | "assistant" | "system") {
        return Err(RouteError::BadRequest(format!("invalid role: {role}")));
    }
    if req.content.is_empty() {
        return Err(RouteError::BadRequest("content must not be empty".into()));
    }
    let record = state
        .messages
        .for_user(user.id)
        .append(
            session_id,
            NewMessage {
                id: Uuid::new_v4(),
                role: role.clone(),
                content: req.content,
                model: req.model,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(MessageResponse::from(record))).into_response())
}

// ---- PATCH /v1/chat/session/:sid/messages/:mid ----------------------------

/// Edit a single message in place. The `version` in the body is
/// the optimistic-concurrency token: a stale edit returns 409
/// and the row is left untouched.
pub async fn edit_handler(
    State(state): State<crate::ChatMessagesState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    AxPath((session_id, message_id)): AxPath<(Uuid, Uuid)>,
    Json(req): Json<EditRequest>,
) -> Result<Response, RouteError> {
    crate::auth::middleware::check_csrf(&headers, &user).map_err(|_| RouteError::Forbidden)?;
    state
        .messages
        .verify_session(user.id, session_id)
        .await
        .map_err(RouteError::from)?;
    let record = state
        .messages
        .for_user(user.id)
        .edit(session_id, message_id, &req.content, req.version)
        .await?;
    Ok(Json(MessageResponse::from(record)).into_response())
}

// ---- DELETE /v1/chat/session/:sid/messages/:mid --------------------------

/// Delete a single message. Returns 204 on success, 404 if the
/// row is missing, 403 if it is owned by a different user.
pub async fn delete_handler(
    State(state): State<crate::ChatMessagesState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    AxPath((session_id, message_id)): AxPath<(Uuid, Uuid)>,
) -> Result<Response, RouteError> {
    crate::auth::middleware::check_csrf(&headers, &user).map_err(|_| RouteError::Forbidden)?;
    state
        .messages
        .verify_session(user.id, session_id)
        .await
        .map_err(RouteError::from)?;
    state
        .messages
        .for_user(user.id)
        .delete(session_id, message_id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- POST /v1/chat/session/:sid/regenerate -------------------------------

/// Delete the last assistant message (if any). The browser then
/// re-streams the reply through the existing
/// `POST /v1/chat/completions` path with the same user turn the
/// previous reply was based on. Returns 204 on success and 404
/// when there is nothing to regenerate (no assistant row).
pub async fn regenerate_handler(
    State(state): State<crate::ChatMessagesState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: HeaderMap,
    AxPath(session_id): AxPath<Uuid>,
) -> Result<Response, RouteError> {
    crate::auth::middleware::check_csrf(&headers, &user).map_err(|_| RouteError::Forbidden)?;
    state
        .messages
        .verify_session(user.id, session_id)
        .await
        .map_err(RouteError::from)?;
    // The last row is the candidate to drop. The plan only
    // supports "regenerate the last assistant" so we additionally
    // gate on `role = 'assistant'` — if the most recent row is a
    // user turn (because the user sent another turn before
    // clicking 🔁) the route returns 404 and the UI falls back
    // to the edit flow.
    let last = state
        .messages
        .for_user(user.id)
        .last_ordinal(session_id)
        .await?;
    let Some((last_id, _ordinal, last_role)) = last else {
        return Err(RouteError::Message(MessageError::NotFound(
            Uuid::nil(),
            session_id,
        )));
    };
    if last_role != "assistant" {
        return Err(RouteError::Message(MessageError::NotFound(
            last_id, session_id,
        )));
    }
    state
        .messages
        .for_user(user.id)
        .delete(session_id, last_id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_owned_error_maps_to_403() {
        // Same security property as `chat::sessions::RouteError`:
        // a probing caller cannot tell `NotFound` from `NotOwned`
        // — both surface as a 403 in the `IntoResponse` impl.
        let e = MessageError::NotOwned(Uuid::new_v4(), Uuid::new_v4());
        let resp = RouteError::from(e).into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn version_mismatch_maps_to_409() {
        let e = MessageError::VersionMismatch(Uuid::new_v4(), 1, 2);
        let resp = RouteError::from(e).into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn bad_role_maps_to_400() {
        let e = MessageError::BadRole("tool".into());
        let resp = RouteError::from(e).into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
