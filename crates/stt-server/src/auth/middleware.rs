//! `require_auth_middleware` — the axum middleware that resolves
//! a request's session cookie (or `Authorization: Bearer`) into an
//! [`AuthUser`] extension.
//!
//! Mirrors the structure of [`crate::llm_auth_middleware`] (the
//! `LLM_API_KEY` bearer gate that protects `/v1/*`): a
//! `from_fn`-friendly `Request → Response` future that short-
//! circuits with `401` on miss / expiry and otherwise injects the
//! [`AuthUser`] for downstream handlers via `req.extensions_mut()`.
//!
//! When `auth.enabled` is false the router does not install this
//! layer at all, so a server that has not opted in keeps the
//! pre-PR1 single-user trust boundary.

#[cfg(feature = "auth")]
use axum::extract::Request;
#[cfg(feature = "auth")]
use axum::http::{header, HeaderValue, StatusCode};
#[cfg(feature = "auth")]
use axum::middleware::Next;
#[cfg(feature = "auth")]
use axum::response::{IntoResponse, Response};

#[cfg(feature = "auth")]
use crate::auth::error::AuthError;
#[cfg(feature = "auth")]
use crate::auth::session::{self, AuthUser};
#[cfg(feature = "auth")]
use crate::auth::store::AuthStore;

/// Shared state passed into the middleware closure. Cheap to
/// clone (Arc-wrapped store + config). Also used by the auth
/// handlers as their `State<S>` so the auth subtree can be merged
/// into the application router without a state-type mismatch
/// (axum 0.7 requires the merged routers to share one `S`).
#[cfg(feature = "auth")]
#[derive(Clone, Debug)]
pub struct AuthState {
    pub store: AuthStore,
    pub cfg: std::sync::Arc<crate::config::Config>,
    /// OIDC sub-state — `None` when OIDC is not enabled. Handlers
    /// unwrap this through the `oidc()` accessor.
    pub oidc: Option<crate::auth::oidc::OidcState>,
    /// Passkey sub-state — `None` when passkey is not enabled.
    pub passkey: Option<crate::auth::passkey::PasskeyState>,
    /// Login-attempt rate limiter. Shared between the middleware
    /// (unused) and the password login handler.
    pub rate_limiter: crate::auth::rate_limit::LoginRateLimiter,
}

#[cfg(feature = "auth")]
impl AuthState {
    pub fn new(store: AuthStore, cfg: std::sync::Arc<crate::config::Config>) -> Self {
        Self {
            store,
            cfg,
            oidc: None,
            passkey: None,
            rate_limiter: crate::auth::rate_limit::LoginRateLimiter::new(),
        }
    }
}

/// Extract an `AuthUser` from the request headers. Returns
/// `Ok(Some(user))` when the cookie/bearer resolves to a live
/// session, `Ok(None)` when no session is present (the caller is
/// expected to map that to a 401), `Err(_)` on database errors.
///
/// This is the same logic as the middleware, but pulled out so
/// handlers can also call it (e.g. the `register` handler needs to
/// check the caller's identity without going through the
/// middleware).
#[cfg(feature = "auth")]
pub async fn extract_auth_user(
    headers: &axum::http::HeaderMap,
    state: &AuthState,
) -> Result<Option<AuthUser>, AuthError> {
    let Some(session_id) = session::extract_session_id(headers, state.cfg.auth.cookie_name())
    else {
        return Ok(None);
    };
    let lookup = state.store.lookup_session(session_id).await?;
    let Some((session, user)) = lookup else {
        return Ok(None);
    };
    let auth_user = AuthUser {
        id: user.id,
        email: user.email,
        display_name: user.display_name,
        provider: user.provider,
        roles: Vec::new(),
        created_at: user.created_at,
        csrf_token: session.csrf_token,
        session_expires_at: session.expires_at,
    };
    // Fire-and-forget touch; we do not block the request on the
    // round-trip because last_seen_at is debug-only.
    let store = state.store.clone();
    let session_id_for_touch = session_id;
    tokio::spawn(async move {
        let _ = store.touch_session(session_id_for_touch).await;
    });
    Ok(Some(auth_user))
}

/// CSRF check helper used by handlers that mutate state.
///
/// Returns `Ok(())` when the request carries a valid CSRF token
/// (matching the session row's csrf_token, constant-time compared)
/// OR when the request comes from a bearer client (bearer clients
/// cannot be tricked into cross-site submissions, so the check is
/// skipped for them). Returns `Err(AuthError::Forbidden)` on a
/// mismatch.
#[cfg(feature = "auth")]
pub fn check_csrf(headers: &axum::http::HeaderMap, user: &AuthUser) -> Result<(), AuthError> {
    // Skip for bearer requests — the Authorization header proves
    // the caller is the API client itself, not a victim of a CSRF
    // attack.
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.to_ascii_lowercase().starts_with("bearer "))
    {
        return Ok(());
    }
    let presented = headers
        .get("x-csrf-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if session::constant_time_eq_str(presented, &user.csrf_token) {
        Ok(())
    } else {
        Err(AuthError::Forbidden)
    }
}

/// The actual axum middleware. Wired via
/// `axum::middleware::from_fn_with_state(state.clone(), require_auth_middleware)`.
#[cfg(feature = "auth")]
pub async fn require_auth_middleware(
    axum::extract::State(state): axum::extract::State<AuthState>,
    mut req: Request,
    next: Next,
) -> Response {
    match extract_auth_user(req.headers(), &state).await {
        Ok(Some(user)) => {
            // Method-aware CSRF check. GETs and OPTIONS pass
            // through; mutating verbs need the header (or a
            // bearer Authorization).
            let method = req.method().clone();
            if session::requires_csrf_check(&method) {
                if let Err(e) = check_csrf(req.headers(), &user) {
                    return e.into_response();
                }
            }
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        Ok(None) => unauthorized_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(feature = "auth")]
fn unauthorized_response() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Cookie realm=\"nagent\""),
    );
    resp
}

#[cfg(all(test, feature = "auth"))]
mod tests {
    use super::*;
    use crate::auth::store::AuthStore;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    /// Build an in-memory sqlite-backed `AuthStore` and run the
    /// migrations. Returns the store + the auth config that drove
    /// its creation.
    async fn temp_store() -> (AuthStore, std::sync::Arc<Config>) {
        // We never read the bind address; an ephemeral port is
        // fine. The `public_url` is set to `https://example.com`
        // so the cookie code path picks `Secure = true` for the
        // test fixtures. We seed a `whisper_model_path` so the
        // no-TOML config path doesn't trip on the model-path
        // required knob (which is unrelated to the test).
        let toml_text = r#"
            [server]
            whisper_model_path = "/tmp/m.bin"
        "#;
        let toml: crate::config_file::TomlConfig =
            toml::from_str(toml_text).expect("TOML must parse");
        let mut cfg = Config::from_env_with_toml(Some(&toml)).expect("default config");
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
        let store = AuthStore::connect(&cfg.auth)
            .await
            .expect("store must connect");
        store.migrate().await.expect("migrations must apply");
        (store, cfg)
    }

    /// Test handler that echoes back the `AuthUser` extension as
    /// JSON. Useful for asserting "the middleware injected the
    /// right identity".
    async fn whoami(
        axum::Extension(user): axum::Extension<AuthUser>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "email": user.email,
            "provider": user.provider,
        }))
    }

    fn make_router(state: AuthState) -> Router {
        Router::new()
            .route("/whoami", get(whoami))
            .layer(from_fn_with_state(state.clone(), require_auth_middleware))
            .with_state(state)
    }

    #[tokio::test]
    async fn require_auth_happy_path() {
        let (store, cfg) = temp_store().await;
        let state = AuthState::new(store.clone(), cfg.clone());

        // Create a local user via the store directly (the HTTP
        // register handler is gated by RequireAuth so it cannot
        // bootstrap itself).
        let user_id = store
            .create_user("alice@example.com", "Alice", "local", Some(b"dummy"))
            .await
            .unwrap();
        // Mint a session row.
        let session = store
            .create_session(
                user_id,
                std::time::Duration::from_secs(60),
                Some("127.0.0.1"),
                None,
            )
            .await
            .unwrap();

        // Cookie path.
        let cookie = format!("{}={}", cfg.auth.cookie_name(), session.id);
        let app = make_router(state.clone());
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "cookie must authenticate");
        let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["email"], "alice@example.com");

        // Bearer path.
        let app = make_router(state.clone());
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {}", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bearer must authenticate");
    }

    #[tokio::test]
    async fn require_auth_401_paths() {
        let (store, cfg) = temp_store().await;
        let state = AuthState::new(store.clone(), cfg.clone());
        let app = make_router(state.clone());

        // Missing cookie + bearer → 401 with WWW-Authenticate.
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));

        // Malformed bearer (not a UUID) → 401.
        let app = make_router(state.clone());
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .header(header::AUTHORIZATION, "Bearer not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Bearer pointing at a non-existent session → 401.
        let app = make_router(state.clone());
        let bogus = uuid::Uuid::new_v4();
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .header(header::AUTHORIZATION, format!("Bearer {bogus}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expired_session_returns_401() {
        let (store, cfg) = temp_store().await;
        let user_id = store
            .create_user("bob@example.com", "Bob", "local", Some(b"dummy"))
            .await
            .unwrap();
        // 1-second TTL then sleep past expiry.
        let session = store
            .create_session(user_id, std::time::Duration::from_secs(1), None, None)
            .await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));

        let state = AuthState::new(store.clone(), cfg.clone());
        let app = make_router(state);
        let cookie = format!("{}={}", cfg.auth.cookie_name(), session.id);
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/whoami")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expired session must 401"
        );
    }

    #[tokio::test]
    async fn csrf_mismatch_on_state_changer_returns_403() {
        let (store, cfg) = temp_store().await;
        let user_id = store
            .create_user("eve@example.com", "Eve", "local", Some(b"dummy"))
            .await
            .unwrap();
        let session = store
            .create_session(user_id, std::time::Duration::from_secs(60), None, None)
            .await
            .unwrap();
        let state = AuthState::new(store.clone(), cfg.clone());
        let router = Router::new()
            .route(
                "/change",
                axum::routing::post(|axum::Extension(_u): axum::Extension<AuthUser>| async {
                    (StatusCode::OK, "ok")
                })
                .get(|axum::Extension(_u): axum::Extension<AuthUser>| async {
                    (StatusCode::OK, "ok")
                }),
            )
            .layer(from_fn_with_state(state.clone(), require_auth_middleware))
            .with_state(state);
        // POST without CSRF → 403.
        let cookie = format!("{}={}", cfg.auth.cookie_name(), session.id);
        let resp = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/change")
                    .header(header::COOKIE, cookie.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // POST with WRONG CSRF → 403.
        let resp = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/change")
                    .header(header::COOKIE, cookie.clone())
                    .header("x-csrf-token", "deadbeef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // POST with correct CSRF → 200.
        let resp = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/change")
                    .header(header::COOKIE, cookie)
                    .header("x-csrf-token", session.csrf_token.as_str())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // GET with no CSRF → 200 (CSRF only checked on state changers).
        let resp = router
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("GET")
                    .uri("/change")
                    .header(
                        header::COOKIE,
                        format!("{}={}", cfg.auth.cookie_name(), session.id),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET must not require CSRF");
        // Bearer skips CSRF — same POST without header but with Bearer.
        let resp = router
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/change")
                    .header(header::AUTHORIZATION, format!("Bearer {}", session.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "bearer must skip CSRF");
    }
}
