//! `http/` — HTTP transport plumbing.
//!
//! Pure plumbing: security headers, the static frontend embedding, the
//! `/api/features` discovery endpoint, the axum middleware shared by
//! every `/v1/*` route, and the [`build_router`] composition root that
//! stitches every per-subsystem subtree into a single `axum::Router`.
//!
//! Subsystems (STT, LLM proxy, agents, documents, chat-sessions,
//! TTS, auth, credentials, `/api/features`) each contribute a
//! `mount_*` helper. [`build_router`] reads top-to-bottom: public
//! subtree → optional protected subtrees → auth subtree (when
//! `auth.enabled = true`) → outermost security-header + access-log
//! layers.

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::AppState;

pub mod features;
pub mod llm_guards;
pub mod security_headers;
pub mod static_assets;

// The `build_rate_limiters` / `llm_auth_middleware` /
// `llm_rate_limit_middleware` helpers live in [`llm_guards`] (the
// shared envelope around the `/v1/*` subtree) but are re-exported at
// the `http::` level so `main.rs` does not need to know about the
// internal split.
pub use llm_guards::{build_rate_limiters, llm_auth_middleware, llm_rate_limit_middleware};

/// Build the axum router around [`AppState`]. Exposed for tests.
///
/// When `auth.enabled = true` the router applies the [`RequireAuth`]
/// middleware  to every endpoint **except** a small set of
/// public carve-outs: the index page, `/static/*`, `/healthz`,
/// `/api/version`, and the auth login routes. The carve-out exists
/// so the browser can fetch the login page and submit credentials
/// without already being authenticated. Everything else — STT
/// WebSocket upgrade, `/v1/chat/completions`, agents, TTS,
/// `/api/me`, `/api/auth/logout` — requires a valid session cookie
/// (or `Authorization: Bearer <session-id>`).
///
/// When `auth.enabled = false` the router is unchanged: no
/// `RequireAuth` layer is installed anywhere and the pre-/// single-user trust boundary holds.
///
/// [`RequireAuth`]: crate::auth::middleware::require_auth_middleware
pub fn build_router(state: Arc<AppState>) -> Router {
    // Always-on security headers applied to *every* response (static
    // frontend, health checks, version probe, WS upgrade, LLM proxy,
    // auth subtree). Applied as the outermost layer on the merged
    // router so a single header copy runs regardless of which
    // subtree handled the request.
    let security_layers = (
        security_headers::security_headers_layer(),
        security_headers::referrer_policy_layer(),
        security_headers::nosniff_layer(),
    );

    // ----- Public subtree (no auth required) -----------------------------
    // These endpoints stay reachable even when `auth.enabled = true`
    // so the browser can load the login page, fetch its assets,
    // submit credentials, and have ops tooling (health probes,
    // version probes) keep working. The auth login routes are
    // merged into `public` further down.
    let public = Router::new()
        // `/` and `/index.html` both serve the same shell; the
        // explicit alias catches crawlers / link-checkers that
        // probe `/index.html` directly.
        .route("/", get(crate::stt::ws_handler::index_handler))
        .route("/index.html", get(crate::stt::ws_handler::index_handler))
        .route("/healthz", get(crate::stt::ws_handler::healthz))
        .route("/api/version", get(crate::stt::ws_handler::version_handler))
        .route(
            "/static/*path",
            get(crate::stt::ws_handler::static_path_handler),
        );

    // ----- Protected subtree (auth required when enabled) ----------------
    // Each subsystem contributes its own `mount_*` helper that returns
    // a Router pre-wrapped in the shared CORS / rate-limit / bearer-auth
    // envelope (when applicable). The global `RequireAuth` layer is
    // applied below, after we know whether auth is enabled.
    //
    // Every handler extracts its sub-state through
    // `FromRef<Arc<AppState>>`, so the whole tree keeps the
    // `Router<Arc<AppState>>` type. Start from `mount_features`
    // (which already returns `Router<Arc<AppState>>`) so axum
    // infers the state type from that branch's signature rather
    // than from the first `State<T>` extractor on a fresh `Router`.
    let mut protected: Router<Arc<AppState>> =
        mount_features(state.clone()).route("/ws", get(crate::stt::ws_handler::ws_upgrade));

    if state.agents.is_some() {
        protected = protected.merge(mount_agents(state.clone()));
    }

    if state.llm.is_some() {
        protected = protected.merge(mount_llm_proxy(state.clone()));
    }

    if state.documents.is_some() {
        protected = protected.merge(mount_documents(state.clone()));
    }

    if state.chat_sessions.is_some() {
        protected = protected.merge(mount_chat_sessions(state.clone()));
    }

    if state.tts.is_some() {
        protected = protected.merge(mount_tts(state.clone()));
    }

    // ----- Auth subtree  --------------------------------------------
    // When `auth.enabled = true`:
    // - login routes (`/api/auth/login/*`) live in `public` so they
    // are reachable without a session;
    // - protected identity routes (`/api/me`, `/api/auth/logout`,
    // passkey register start/finish) live in `protected`;
    // - `protected` is wrapped with `RequireAuth` so anonymous
    // requests get `401 authentication required`.
    // When `auth.enabled = false`: nothing is mounted; the server
    // keeps the single-user trust boundary.
    if state.config.auth.enabled {
        let auth_state = state
            .auth
            .clone()
            .expect("auth must be Some when auth is enabled");
        let auth_layer = axum::middleware::from_fn_with_state(
            auth_state.clone(),
            crate::auth::middleware::require_auth_middleware,
        );
        let login = crate::auth::router::build_public_auth_router(state.clone());
        let identity = crate::auth::router::build_protected_auth_router(state.clone());
        let credentials_routes =
            crate::credentials::routes::build_protected_credentials_router(state.clone());
        let protected_with_auth = protected
            .merge(identity)
            .merge(credentials_routes)
            .layer(auth_layer);
        // Layer order is applied bottom-up; the LAST `.layer()`
        // becomes the OUTERMOST. The access log is the outermost
        // so it sees the final response status (after `RequireAuth`
        // and the security-header layers have run) and the
        // `ConnectInfo` IP from the axum server. Security headers
        // are second-outermost so they can mutate the response
        // before the access log snapshots the status.
        public
            .merge(login)
            .merge(protected_with_auth)
            .layer(security_layers)
            .layer(axum::middleware::from_fn(
                security_headers::access_log_middleware,
            ))
            .with_state(state)
    } else {
        public
            .merge(protected)
            .layer(security_layers)
            .layer(axum::middleware::from_fn(
                security_headers::access_log_middleware,
            ))
            .with_state(state)
    }
}

// ---------------------------------------------------------------------------
// Per-subsystem mount helpers
// ---------------------------------------------------------------------------
//
// Each helper returns a `Router<Arc<AppState>>` already wrapped in the
// shared envelope (CORS allow-list, per-IP rate limit, bearer-auth gate)
// when one applies. The envelope is factored out into
// [`v1_envelope`] so the five `/v1/*` subtrees (LLM, agents, documents,
// chat-sessions, TTS) share one CORS / rate-limit / bearer-auth wiring.

/// Wrap a Router with the shared `/v1/*` envelope: bearer-auth gate
/// (driven by the operator-configured `[llm]` block), per-IP rate
/// limit (shared across every `/v1/*` subtree), and CORS layer.
fn v1_envelope(state: &Arc<AppState>, router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    let cors_origins = state
        .llm
        .as_ref()
        .map(|l| l.client.cfg().cors_allow_origins.clone())
        .unwrap_or_default();
    let cors = security_headers::cors_layer(&cors_origins);
    let llm_limiter = state
        .llm
        .as_ref()
        .map(|l| l.rate_limiter.clone())
        .unwrap_or_else(|| {
            // No LLM proxy wired — the envelope is unused, so any
            // limiter would do; build a permissive default so a
            // future code path that mounts the envelope without an
            // LLM still works (the auth middleware will gate before
            // it).
            crate::rate_limit::RateLimiter::new(crate::rate_limit::RateLimitPolicy::llm(0))
        });
    // Auth always reads from the global `LlmConfig` so operators can
    // gate `/v1/*` without enabling the LLM proxy — the two
    // subsystems share the `[llm]` table on purpose so there is one
    // source of truth for "is this server public?".
    let llm_cfg = Arc::new(state.config.llm.clone());
    router
        .layer(axum::middleware::from_fn(move |req, next| {
            let cfg = llm_cfg.clone();
            async move { llm_auth_middleware(Some(cfg), req, next).await }
        }))
        .layer(axum::middleware::from_fn(move |req, next| {
            let limiter = llm_limiter.clone();
            async move { llm_rate_limit_middleware(limiter, req, next).await }
        }))
        .layer(cors)
}

/// `GET /api/features` — feature discovery for the frontend. Mounted at
/// the protected subtree level so it benefits from `RequireAuth` when
/// auth is enabled. The handler is cheap and stateless, so it does not
/// need its own rate limit / CORS envelope.
fn mount_features(state: Arc<AppState>) -> Router<Arc<AppState>> {
    features::build_features_router(state)
}

/// Agents routes — `GET /v1/agents` and `POST /v1/agents/:name/invoke`.
///
/// Gated independently from the LLM proxy so direct curl invocation
/// (`POST /v1/agents/web_fetch/invoke`) keeps working when only the
/// proxy is off, and so disabling the LLM proxy leaves no trace of
/// the agent HTTP routes when both are off.
fn mount_agents(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let agents_app = Router::new()
        .route("/v1/agents", get(crate::llm::agents_list))
        .route("/v1/agents/:name/invoke", post(crate::llm::agent_invoke));
    v1_envelope(&state, agents_app)
}

/// LLM proxy — `POST /v1/chat/completions` and `GET /v1/models`.
fn mount_llm_proxy(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let llm_app = Router::new()
        .route("/v1/chat/completions", post(crate::llm::chat_completions))
        .route("/v1/models", get(crate::llm::models_list));
    v1_envelope(&state, llm_app)
}

/// Document uploads + downloads — share the LLM proxy's CORS /
/// rate-limit envelope. Mounted only when the `documents` cargo
/// feature is on AND the runtime flag is on AND the auth DB is
/// reachable (so the table exists). `state.documents` is `Some` iff
/// all three are true.
fn mount_documents(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let documents_app = crate::documents::routes::build_documents_router(state.clone());
    v1_envelope(&state, documents_app)
}

/// `POST /v1/chat/session` — server-bound chat-session id mint. Mounted
/// independently of `[documents].enabled` because the SEV 2 binding is a
/// general-purpose feature (chat-history scoping may use it later).
/// Reachable only when `state.chat_sessions.is_some()`, which is true
/// iff auth is enabled.
fn mount_chat_sessions(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let chat_session_app = crate::documents::routes::build_chat_session_router(state.clone());
    v1_envelope(&state, chat_session_app)
}

/// TTS routes — `POST /v1/audio/speech` and `GET /v1/audio/voices`.
fn mount_tts(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let tts_app = Router::new()
        .route("/v1/audio/speech", post(crate::tts::audio_speech))
        .route("/v1/audio/voices", get(crate::tts::audio_voices));
    v1_envelope(&state, tts_app)
}
