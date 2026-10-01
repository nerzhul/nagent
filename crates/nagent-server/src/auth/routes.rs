//! HTTP routes for the auth subsystem.
//!
//! - `GET  /api/me` — returns the [`AuthUser`] resolved by the
//! `RequireAuth` middleware. The browser SPA reads this on boot
//! to decide whether to render the login panel or the chat view.
//! - `GET  /api/me/preferences` — per-user UI preferences (location /
//! timezone sharing toggles, formerly held in `localStorage`).
//! - `PUT  /api/me/preferences` — same shape, CSRF-protected.
//! - `POST /api/auth/logout` — deletes the current session and
//! clears the cookie. Requires both the session cookie AND the
//! matching CSRF token (constant-time compared).
//!
//! The `/api/auth/login/*` and `/api/auth/password/register`
//! routes live in [`crate::auth::password`], [`crate::auth::oidc`]
//! and [`crate::auth::passkey`]; the router wires them all up.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::error::{require_auth_store_from_auth, AuthError};
use crate::auth::middleware::check_csrf;
use crate::auth::session;
use crate::auth::AuthUser;

/// `GET /api/me` — returns the [`AuthUser`] resolved by the
/// `RequireAuth` middleware.
pub async fn me_handler(axum::Extension(user): axum::Extension<AuthUser>) -> Json<AuthUser> {
    Json(user)
}

/// `GET /api/me/preferences` — returns the per-user UI
/// preferences. The shape mirrors the localStorage flags the
/// frontend used to manage client-side, so the migration is a
/// drop-in replacement for `loadLocationEnabled` /
/// `loadTimezoneEnabled`.
pub async fn get_preferences_handler(
    State(state): State<crate::AuthState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> Result<Response, AuthError> {
    let store = require_auth_store_from_auth(&state)?;
    let prefs = store.preferences.get(user.id).await?;
    Ok(Json(json!({
        "share_location_enabled": prefs.share_location_enabled,
        "share_timezone_enabled": prefs.share_timezone_enabled,
        "updated_at": prefs.updated_at.to_rfc3339(),
    }))
    .into_response())
}

/// Body shape for `PUT /api/me/preferences`. Both flags are
/// required so a PUT always represents the full desired state —
/// a UI that wants to flip just `share_location_enabled` reads
/// the current value, flips the bit, and writes both back. This
/// avoids the partial-update ambiguity the original localStorage
/// flags had (one write per flag, no atomicity, possible drift
/// between two browser tabs).
#[derive(Debug, Deserialize)]
pub struct PutPreferencesBody {
    #[serde(default)]
    pub share_location_enabled: Option<bool>,
    #[serde(default)]
    pub share_timezone_enabled: Option<bool>,
}

/// `PUT /api/me/preferences` — atomic replace of the per-user
/// preferences. CSRF-protected (the middleware enforces it for
/// every non-GET route on the protected subtree). Returns the
/// updated row so the client can sync without a second GET.
pub async fn put_preferences_handler(
    State(state): State<crate::AuthState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PutPreferencesBody>,
) -> Result<Response, AuthError> {
    check_csrf(&headers, &user)?;
    let Some(loc) = body.share_location_enabled else {
        return Err(AuthError::BadRequest(
            "share_location_enabled is required".into(),
        ));
    };
    let Some(tz) = body.share_timezone_enabled else {
        return Err(AuthError::BadRequest(
            "share_timezone_enabled is required".into(),
        ));
    };
    let store = require_auth_store_from_auth(&state)?;
    let prefs = store.preferences.upsert(user.id, loc, tz).await?;
    Ok(Json(json!({
        "share_location_enabled": prefs.share_location_enabled,
        "share_timezone_enabled": prefs.share_timezone_enabled,
        "updated_at": prefs.updated_at.to_rfc3339(),
    }))
    .into_response())
}

/// `POST /api/auth/logout`
pub async fn logout_handler(
    State(state): State<crate::AuthState>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AuthError> {
    // The middleware would normally have rejected anonymous
    // requests before we get here, but the handler is also called
    // from the per-route test helpers — be defensive.
    let user = crate::auth::middleware::extract_auth_user(&headers, &state)
        .await?
        .ok_or(AuthError::Unauthenticated)?;
    check_csrf(&headers, &user)?;
    // Security plans #1 + #7: `delete_session` expects the
    // SHA-256 of the opaque session token (the
    // `sessions.token_hash` primary key), NOT the user id.
    require_auth_store_from_auth(&state)?
        .sessions
        .delete(&user.session_token_hash)
        .await?;
    let cookie = session::build_clear_cookie(state.cfg.cookie_name(), state.cfg.cookie_secure());
    tracing::info!(
        event = "auth.logout",
        outcome = "ok",
        email = %user.email,
        user_id = %user.id,
        provider = %user.provider,
        "auth logout ok"
    );
    require_auth_store_from_auth(&state)?
        .events
        .record(nagent_db::NewAuthEvent::auth(
            Some(user.id),
            "logout",
            user.provider.clone(),
        ));
    let mut response = (StatusCode::NO_CONTENT, "").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        cookie
            .parse()
            .map_err(|e: axum::http::header::InvalidHeaderValue| {
                AuthError::Internal(format!("set-cookie parse: {e}"))
            })?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    //! Regression tests for security plan #1 ("logout does not revoke
    //! the session"). The pre-fix bug: `logout_handler` called
    //! `delete_session(user.id)` instead of
    //! `delete_session(user.session_token_hash)`, so the SQL `DELETE FROM
    //! sessions WHERE id = ?` matched zero rows and the stolen
    //! session stayed valid until expiry.
    //!
    //! The test exercises the full path:
    //! 1. `extract_auth_user` populates `AuthUser.session_id`
    //! from the same row it authenticates against.
    //! 2. `delete_session(user.session_token_hash)` removes exactly that
    //! row.
    //! 3. Reusing the same cookie on `/api/me` returns `401`.
    //! 4. Reusing the same bearer on `/api/me` returns `401`.
    //!
    //! The tests exercise `extract_auth_user` + `delete_session`
    //! directly (mirroring what `logout_handler` does) rather than
    //! calling the handler itself, so there is nothing from the
    //! outer module to import.
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn temp_store() -> (nagent_db::Db, std::sync::Arc<crate::config::Config>) {
        let toml_text = r#"
            [server]
            whisper_model_path = "/tmp/m.bin"
        "#;
        let toml: crate::config_file::TomlConfig =
            toml::from_str(toml_text).expect("TOML must parse");
        let mut cfg =
            crate::config::Config::from_env_with_toml(Some(&toml)).expect("default config");
        cfg.auth.enabled = true;
        cfg.auth.backends = vec![crate::config::AuthBackendKind::Local];
        cfg.auth.public_url = "https://example.com".into();
        cfg.auth.db.backend = "sqlite".into();
        cfg.auth.db.url = format!(
            "sqlite://file:test_{}?mode=memory&cache=shared",
            uuid::Uuid::new_v4()
        );
        cfg.auth.db.max_connections = 1;
        let cfg = std::sync::Arc::new(cfg);
        let opts: nagent_db::DbOptions = (&cfg.auth).into();
        let store = nagent_db::Db::connect(&opts)
            .await
            .expect("store must connect");
        store.migrate().await.expect("migrations must apply");
        (store, cfg)
    }

    /// `GET /api/me` shaped like the real router — extracts the
    /// `AuthUser` extension and returns the email. Mirrors
    /// [`crate::auth::routes::me_handler`].
    async fn me_handler(
        axum::Extension(user): axum::Extension<crate::auth::session::AuthUser>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({ "email": user.email }))
    }

    fn whoami_router(state: crate::auth::AuthState) -> Router {
        Router::new()
            .route("/api/me", get(me_handler))
            .layer(from_fn_with_state(
                state.clone(),
                crate::auth::middleware::require_auth_middleware,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn logout_revokes_session_so_cookie_returns_401() {
        let (store, cfg) = temp_store().await;
        let user_id = store
            .users
            .create("alice@example.com", "Alice", "local", Some(b"hash"))
            .await
            .unwrap();
        let session = store
            .sessions
            .create(
                user_id,
                std::time::Duration::from_secs(60),
                Some("127.0.0.1"),
                None,
            )
            .await
            .unwrap();
        let session_token = session
            .plaintext_token
            .clone()
            .expect("create_session must mint a plaintext token");
        let state = crate::auth::AuthState::new(store.clone(), cfg.clone());
        let app = whoami_router(state.clone());
        let cookie = format!("{}={}", cfg.auth.cookie_name(), session_token);

        // 1. Cookie authenticates BEFORE logout.
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/me")
                    .header(axum::http::header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "cookie must auth before logout"
        );

        // 2. Re-resolve via the middleware path and call
        // `delete_session(user.session_token_hash)` exactly like
        // `logout_handler` does (this is the regression for the
        // pre-fix `delete_session(user.id)` bug).
        let headers = {
            let mut h = axum::http::HeaderMap::new();
            h.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
            h
        };
        let user = crate::auth::middleware::extract_auth_user(&headers, &state)
            .await
            .unwrap()
            .expect("user must be present");
        assert_eq!(
            user.session_token_hash, session.token_hash,
            "extract_auth_user must populate user.session_token_hash from the row"
        );
        let deleted = store
            .sessions
            .delete(&user.session_token_hash)
            .await
            .unwrap();
        assert_eq!(
            deleted, 1,
            "delete_session(user.session_token_hash) must remove exactly 1 row (the bug pre-fix deleted 0 because user.id was passed)"
        );

        // 3. Same cookie on /api/me must now return 401.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/me")
                    .header(axum::http::header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "logout must revoke the cookie"
        );
    }

    #[tokio::test]
    async fn logout_revokes_session_so_bearer_returns_401() {
        let (store, cfg) = temp_store().await;
        let user_id = store
            .users
            .create("bob@example.com", "Bob", "local", Some(b"hash"))
            .await
            .unwrap();
        let session = store
            .sessions
            .create(user_id, std::time::Duration::from_secs(60), None, None)
            .await
            .unwrap();
        let session_token = session
            .plaintext_token
            .clone()
            .expect("create_session must mint a plaintext token");
        let state = crate::auth::AuthState::new(store.clone(), cfg.clone());
        let app = whoami_router(state.clone());

        // 1. Bearer works before logout.
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/me")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        format!("Bearer {}", session_token),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "bearer must auth before logout"
        );

        // 2. Delete via session token hash (the regression).
        let deleted = store.sessions.delete(&session.token_hash).await.unwrap();
        assert_eq!(deleted, 1);

        // 3. Bearer now 401.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/me")
                    .header(
                        axum::http::header::AUTHORIZATION,
                        format!("Bearer {}", session_token),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "logout must revoke the bearer too"
        );
    }

    #[tokio::test]
    async fn logout_does_not_revoke_other_sessions_for_the_same_user() {
        // The pre-fix `delete_session(user.id)` was wrong; even if
        // it had been rewritten to filter by `user_id`, it would
        // have nuked every active session for that user — bad UX
        // and a data-loss vector if `user_id` had been a magic
        // value. The fix calls `delete_session(session_token)` so
        // only the current session is revoked. This test pins that
        // contract.
        let (store, _cfg) = temp_store().await;
        let user_id = store
            .users
            .create("carol@example.com", "Carol", "local", Some(b"hash"))
            .await
            .unwrap();
        let s1 = store
            .sessions
            .create(user_id, std::time::Duration::from_secs(60), None, None)
            .await
            .unwrap();
        let s1_token = s1
            .plaintext_token
            .clone()
            .expect("create_session must mint a plaintext token");
        let s2 = store
            .sessions
            .create(user_id, std::time::Duration::from_secs(60), None, None)
            .await
            .unwrap();
        let s2_token = s2
            .plaintext_token
            .clone()
            .expect("create_session must mint a plaintext token");
        assert_ne!(s1_token, s2_token);

        let deleted = store.sessions.delete(&s1.token_hash).await.unwrap();
        assert_eq!(deleted, 1, "only the targeted session is removed");

        // s2 must still resolve (i.e. it was NOT deleted by s1's
        // logout).
        let s2_lookup = store
            .sessions
            .lookup_by_token_hash(&s2.token_hash)
            .await
            .unwrap();
        assert!(
            s2_lookup.is_some(),
            "the other session must survive — confirms delete_session targets the session id, not the user id"
        );
    }

    // ---- /api/me/preferences ----------------------------------------------
    //
    // The HTTP handlers for `/api/me/preferences` are thin wrappers
    // over `AuthStore::get_user_preferences` /
    // `upsert_user_preferences` — they unwrap the auth store off
    // `Arc<AppState>` (via `require_auth_store`), validate the CSRF
    // token (via `check_csrf`), and serialise the row as JSON. The
    // full round-trip (insert + read + atomicity + isolation)
    // is covered by the SQLite-level tests in
    // `db_sqlite::tests::preferences_*`. The CSRF middleware is
    // covered by the wider `middleware::tests` suite. Building a
    // full `Arc<AppState>` in this file would require every
    // optional subsystem to be wired (the Whisper backend, the
    // worker pool, the LLM proxy, …) which the route handler does
    // not need; the integration cost would dominate the test
    // value.
}
