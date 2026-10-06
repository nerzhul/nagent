//! Per-user long-term memory HTTP surface (plan 1791267136806).
//!
//! Two routes share the `auth.enabled` boundary:
//!
//! - `GET /api/memories` — list metadata for the authenticated
//!   user. Powers the Settings-tab Memory section so the user
//!   can audit / forget what the LLM has stored. **Metadata only**:
//!   the encrypted bytes are not returned over HTTP — the SPA
//!   never holds plaintext memory rows outside the auto-injection
//!   prompt block.
//! - `DELETE /api/memories/:id` — forget one memory by id.
//!   `ScopedMemories::forget` is filtered on `user_id`, so a
//!   cross-user attempt returns `404` (no row exists from the
//!   caller's perspective).
//!
//! Both routes sit on the protected subtree so they inherit the
//! `RequireAuth` + CSRF middleware automatically. They are mounted
//! in [`crate::http::build_router`] (gated on `auth.enabled`)
//! alongside `/api/me/preferences`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Extension as AxumExtension, Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use std::sync::Arc;

use crate::auth::error::AuthError;
use crate::auth::middleware::check_csrf;
use crate::auth::session::AuthUser;
use crate::AppState;

/// `GET /api/memories` — list the authenticated user's metadata
/// rows, newest first.
///
/// Returns 200 with `{ "data": [...], "count": N }`. The body
/// shape mirrors the nagent-agents `MemoryMeta` so the SPA can
/// render the Settings-tab list without translating between server
/// and client types.
pub async fn list_memories_handler(
    State(state): State<Arc<AppState>>,
    AxumExtension(user): AxumExtension<AuthUser>,
) -> Result<Response, AuthError> {
    let rows = state
        .auth
        .as_ref()
        .ok_or(AuthError::Internal(
            "/api/memories mounted without an auth backend".into(),
        ))?
        .store
        .for_user(user.id)
        .memories()
        .list_meta(64)
        .await
        .map_err(|e| AuthError::Internal(format!("memory list failed: {e}")))?;
    let data: Vec<serde_json::Value> = rows
        .iter()
        .map(|m| {
            json!({
                "id": m.id.to_string(),
                "subject": m.subject,
                "predicate": m.predicate,
                "tags": m.tags,
                "confidence": m.confidence,
                "source_session_id": m.source_session_id,
                "source_kind": m.source_kind,
                "created_at": m.created_at.to_rfc3339(),
                "last_used_at": m.last_used_at.map(|d| d.to_rfc3339()),
                "expires_at": m.expires_at.map(|d| d.to_rfc3339()),
            })
        })
        .collect();
    Ok(Json(json!({ "data": data, "count": rows.len() })).into_response())
}

/// Path payload for `DELETE /api/memories/:id`.
#[derive(Debug, Deserialize)]
pub struct MemoryIdPath {
    pub id: String,
}

/// `DELETE /api/memories/:id` — forget one memory. Cross-user
/// attempts return `404` (the row is invisible to the caller)
/// instead of leaking the existence of another user's row.
///
/// CSRF-protected: the `RequireAuth` middleware on the protected
/// subtree enforces the `x-csrf-token` header check on every
/// non-GET verb.
pub async fn delete_memory_handler(
    State(state): State<Arc<AppState>>,
    AxumExtension(user): AxumExtension<AuthUser>,
    headers: axum::http::HeaderMap,
    Path(path): Path<MemoryIdPath>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    let memory_id = Uuid::parse_str(path.id.trim()).map_err(|_| {
        AuthError::BadRequest(format!("`id` must be a UUID; received {:?}", path.id))
    })?;
    let auth = state.auth.as_ref().ok_or(AuthError::Internal(
        "/api/memories/:id DELETE mounted without an auth backend".into(),
    ))?;
    let affected = auth
        .store
        .for_user(user.id)
        .memories()
        .forget(memory_id)
        .await
        .map_err(|e| AuthError::Internal(format!("memory_forget failed: {e}")))?;
    if affected == 0 {
        // Cross-user OR non-existent. Both surface as 404 — the
        // caller cannot tell which (per plan §4 risk mitigation).
        return Err(AuthError::NotFound);
    }
    Ok((StatusCode::NO_CONTENT, "").into_response())
}

/// Build the `/api/memories/*` router. Mounted on the auth-protected
/// subtree (`RequireAuth` + CSRF middleware) by
/// [`crate::http::build_router`].
pub fn build_memories_router(_state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/memories", get(list_memories_handler))
        .route("/api/memories/:id", delete(delete_memory_handler))
}
