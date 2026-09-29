//! `stt-server` — axum HTTP/WebSocket server for the STT pipeline.
//!
//! The server is split into:
//! - [`config`] — env-var parsing.
//! - [`session`] — `SessionState` and the shared `SessionMap`.
//! - [`validation`] — input checks applied to inbound WebSocket frames.
//! - [`rate_limit`] — per-source-IP token bucket for STT and LLM traffic.
//! - [`middleware`] — always-on security headers and LLM CORS layer.
//! - [`ws_handler`] — per-connection upgrade + dispatch loop.
//! - [`router`] — `ResultRouter` that forwards worker output to the right session.
//! - [`watchdog`] — periodic sweep that drops idle sessions.
//! - [`static_assets`] — embedded frontend assets served at `/` and `/static/*`.
//! - [`llm`] — optional OpenAI-compatible proxy to a local LLM (Ollama).
//! - [`agents`] — server-side chat agents (e.g. `web_fetch`) callable
//!   from the LLM proxy through OpenAI-style tool/function calling.
//! - [`credentials`] — per-user credentials vault (AES-256-GCM at rest,
//!   decrypted on demand through a per-request `UserContext`).

#![warn(missing_debug_implementations)]

pub mod agents;
pub mod auth;
pub mod config;
pub mod config_file;
pub mod credentials;
pub mod llm;
pub mod llm_prompt;
pub mod middleware;
pub mod rate_limit;
pub mod router;
pub mod session;
pub mod static_assets;
pub mod tts;
pub mod validation;
pub mod version;
pub mod watchdog;
pub mod ws_handler;

use config::LlmConfig;
pub use config::{CliArgs, Config};
use rate_limit::{RateLimitPolicy, RateLimiter};
use session::SessionMap;
pub use version::VersionInfo;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use std::net::SocketAddr;

use stt_core::{PoolDispatch, WhisperBackend};

/// Shared application state injected into every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn WhisperBackend>,
    pub sessions: SessionMap,
    /// Sticky-dispatch handle into the inference worker pool. Each WS
    /// handler clones it (cheap) and sends its jobs through it; jobs
    /// from the same session always land on the same worker so
    /// per-session FIFO order is preserved.
    pub job_tx: PoolDispatch,
    pub ready: Arc<AtomicBool>,
    pub config: Arc<Config>,
    /// Optional LLM proxy. `None` when `LLM_ENABLED=false`; in that
    /// case the `/v1/*` routes are not registered and the chat view
    /// in the UI 404s gracefully.
    pub llm: Option<llm::LlmClient>,
    /// Optional registry of chat agents. `None` when
    /// `AGENTS_ENABLED=false`; the `/v1/agents*` routes are wired
    /// whenever this is set, regardless of whether the LLM proxy is
    /// enabled (so `curl /v1/agents/web_fetch/invoke` keeps working
    /// for local testing).
    pub agents: Option<agents::AgentRegistry>,
    /// Optional Piper TTS engine. `None` when `TTS_ENABLED=false`; in
    /// that case the `/v1/audio/*` routes are not registered and the
    /// discussion-mode "Read response aloud" UI shows no checkbox.
    /// Always wrapped in an `Arc` because the engine is cloned into
    /// the HTTP handler for each request (it holds per-voice lazy
    /// ONNX session state).
    pub tts: Option<Arc<tts::TtsEngine>>,
    /// Per-source-IP token bucket for the STT pipeline (consumed at
    /// WS upgrade and per inbound WS frame).
    pub stt_rate_limiter: RateLimiter,
    /// Per-source-IP token bucket for the `/v1/*` LLM proxy.
    pub llm_rate_limiter: RateLimiter,
    /// Auth DB handle (PR1). `Some(_)` when `auth.enabled = true`,
    /// `None` otherwise so existing tests that do not care about
    /// auth keep building unmodified. Populated by `auth::boot::
    /// auto_bootstrap` at server start.
    pub auth_store: Option<auth::AuthStore>,
    /// OIDC sub-state — `None` when OIDC is not enabled (or
    /// `auth.enabled = false`).
    pub auth_oidc: Option<auth::oidc::OidcState>,
    /// Passkey sub-state — `None` when passkey is not enabled.
    pub auth_passkey: Option<auth::passkey::PasskeyState>,
    /// Login-attempt rate limiter. Shared between the `RequireAuth`
    /// middleware (unused) and the password login handler.
    pub auth_rate_limiter: auth::rate_limit::LoginRateLimiter,
    /// Catalogue of per-user integrations. `Arc`-wrapped because
    /// every per-request `UserContext` clones it; the inner slice is
    /// `&'static` so the registry can be built once at boot.
    pub services: std::sync::Arc<crate::agents::ServiceRegistry>,
    /// Per-request credential resolver. `None` until the auth
    /// subsystem boots AND a server-side encryption key is
    /// available; per-user agents that call `ctx.secret()` will
    /// surface `CredentialsMissing` when the resolver is missing.
    pub credential_resolver: Option<std::sync::Arc<crate::credentials::CredentialResolver>>,
    /// Server-side encryption key for the credentials vault.
    /// `None` when the resolver is `None`. Held separately because
    /// the route handlers need to encrypt on PUT — the resolver
    /// only exposes reads.
    pub credentials_key: Option<std::sync::Arc<crate::credentials::CredentialsKey>>,
}

impl AppState {
    /// Helper for the credentials routes: clone the encryption
    /// key. Panics if the resolver is set but the key is missing
    /// (a programmer error in `main.rs`).
    pub fn credential_encryption_key(&self) -> std::sync::Arc<crate::credentials::CredentialsKey> {
        self.credentials_key
            .clone()
            .expect("credentials_key must match credential_resolver")
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("backend", &"<dyn WhisperBackend>")
            .field("sessions", &self.sessions)
            .field("job_tx", &self.job_tx)
            .field("ready", &self.ready)
            .field("config", &self.config)
            .field("llm", &self.llm.as_ref().map(|_| "<LlmClient>"))
            .field("agents", &self.agents.as_ref().map(|_| "<AgentRegistry>"))
            .field("tts", &self.tts.as_ref().map(|_| "<TtsEngine>"))
            .field("stt_rate_limiter", &self.stt_rate_limiter)
            .field("llm_rate_limiter", &self.llm_rate_limiter)
            .field(
                "credential_resolver",
                &self.credential_resolver.as_ref().map(|_| "<resolver>"),
            )
            .finish()
    }
}

/// Build the axum router around [`AppState`]. Exposed for tests.
///
/// When `auth.enabled = true` the router applies the [`RequireAuth`]
/// middleware (PR1) to every endpoint **except** a small set of
/// public carve-outs: the index page, `/static/*`, `/healthz`,
/// `/api/version`, and the auth login routes. The carve-out exists
/// so the browser can fetch the login page and submit credentials
/// without already being authenticated. Everything else — STT
/// WebSocket upgrade, `/v1/chat/completions`, agents, TTS,
/// `/api/me`, `/api/auth/logout` — requires a valid session cookie
/// (or `Authorization: Bearer <session-id>`).
///
/// When `auth.enabled = false` the router is unchanged: no
/// `RequireAuth` layer is installed anywhere and the pre-PR1
/// single-user trust boundary holds.
///
/// [`RequireAuth`]: crate::auth::middleware::require_auth_middleware
pub fn build_router(state: Arc<AppState>) -> Router {
    // Always-on security headers applied to *every* response (static
    // frontend, health checks, version probe, WS upgrade, LLM proxy,
    // auth subtree). Applied as the outermost layer on the merged
    // router so a single header copy runs regardless of which
    // subtree handled the request.
    let security_layers = (
        middleware::security_headers_layer(),
        middleware::referrer_policy_layer(),
        middleware::nosniff_layer(),
    );

    // ----- Public subtree (no auth required) -----------------------------
    // These endpoints stay reachable even when `auth.enabled = true`
    // so the browser can load the login page, fetch its assets,
    // submit credentials, and have ops tooling (health probes,
    // version probes) keep working. The auth login routes are
    // merged into `public` further down.
    let public = Router::new()
        .route("/", get(ws_handler::index_handler))
        .route("/healthz", get(ws_handler::healthz))
        .route("/api/version", get(ws_handler::version_handler))
        .route("/static/*path", get(ws_handler::static_path_handler));

    // ----- Protected subtree (auth required when enabled) ----------------
    // The STT WebSocket upgrade, LLM proxy, agents, TTS, and the
    // auth-protected identity routes (`/api/me`, `/api/auth/logout`,
    // passkey register). Each optional subtree keeps its own
    // rate-limit + CORS envelope; the global `RequireAuth` layer is
    // applied below, after we know whether auth is enabled.
    let mut protected: Router<Arc<AppState>> =
        Router::new().route("/ws", get(ws_handler::ws_upgrade));

    // The agents routes are gated independently from the LLM proxy so
    // direct curl invocation (`POST /v1/agents/web_fetch/invoke`) keeps
    // working when only the proxy is off, and so disabling the LLM
    // proxy leaves no trace of the agent HTTP routes when both are
    // off. They share the CORS / rate-limit envelope of the LLM
    // proxy: same allow-list (`LLM_CORS_ALLOW_ORIGINS`), same per-IP
    // bucket (`LLM_RATE_PER_MIN`). When the LLM proxy is off we fall
    // back to the empty allow-list (same-origin only) so a
    // misconfigured server does not silently expose the agents
    // endpoints cross-origin.
    if state.agents.is_some() {
        let cors_origins = state
            .llm
            .as_ref()
            .map(|l| l.cfg().cors_allow_origins.clone())
            .unwrap_or_default();
        let cors = middleware::cors_layer(&cors_origins);
        let llm_limiter = state.llm_rate_limiter.clone();
        // Auth always reads from the global `LlmConfig` so operators
        // can gate `/v1/agents*` without enabling the LLM proxy —
        // the two subsystems share the `[llm]` table on purpose so
        // there is one source of truth for "is this server public?".
        let llm_cfg = Arc::new(state.config.llm.clone());
        let agents_app = Router::new()
            .route("/v1/agents", get(llm::agents_list))
            .route("/v1/agents/:name/invoke", post(llm::agent_invoke))
            .layer(axum::middleware::from_fn(move |req, next| {
                let cfg = llm_cfg.clone();
                async move { llm_auth_middleware(Some(cfg), req, next).await }
            }))
            .layer(axum::middleware::from_fn(move |req, next| {
                let limiter = llm_limiter.clone();
                async move { llm_rate_limit_middleware(limiter, req, next).await }
            }))
            .layer(cors);
        protected = protected.merge(agents_app);
    }

    if let Some(llm) = &state.llm {
        // The LLM proxy gets its own CORS layer driven by
        // `LLM_CORS_ALLOW_ORIGINS`, a per-IP rate limiter, the
        // bearer-auth gate driven by `LLM_AUTH_MODE` / `LLM_API_KEY`.
        let cors = middleware::cors_layer(&llm.cfg().cors_allow_origins);
        let llm_limiter = state.llm_rate_limiter.clone();
        let llm_cfg = Arc::new(llm.cfg().clone());
        let llm_app = Router::new()
            .route("/v1/chat/completions", post(llm::chat_completions))
            .route("/v1/models", get(llm::models_list))
            .layer(axum::middleware::from_fn(move |req, next| {
                let cfg = llm_cfg.clone();
                async move { llm_auth_middleware(Some(cfg), req, next).await }
            }))
            .layer(axum::middleware::from_fn(move |req, next| {
                let limiter = llm_limiter.clone();
                async move { llm_rate_limit_middleware(limiter, req, next).await }
            }))
            .layer(cors);
        protected = protected.merge(llm_app);
    }

    // TTS routes share the LLM proxy's CORS / rate-limit envelope
    // (same origin allow-list, same per-IP bucket). When the LLM proxy
    // is off we fall back to the empty allow-list so a misconfigured
    // server does not silently expose TTS cross-origin.
    if let Some(_tts) = &state.tts {
        let cors_origins = state
            .llm
            .as_ref()
            .map(|l| l.cfg().cors_allow_origins.clone())
            .unwrap_or_default();
        let cors = middleware::cors_layer(&cors_origins);
        let llm_limiter = state.llm_rate_limiter.clone();
        let llm_cfg = Arc::new(state.config.llm.clone());
        let tts_app = Router::new()
            .route("/v1/audio/speech", post(tts::audio_speech))
            .route("/v1/audio/voices", get(tts::audio_voices))
            .layer(axum::middleware::from_fn(move |req, next| {
                let cfg = llm_cfg.clone();
                async move { llm_auth_middleware(Some(cfg), req, next).await }
            }))
            .layer(axum::middleware::from_fn(move |req, next| {
                let limiter = llm_limiter.clone();
                async move { llm_rate_limit_middleware(limiter, req, next).await }
            }))
            .layer(cors);
        protected = protected.merge(tts_app);
    }

    // ----- Auth subtree (PR1) --------------------------------------------
    // When `auth.enabled = true`:
    //   - login routes (`/api/auth/login/*`) live in `public` so they
    //     are reachable without a session;
    //   - protected identity routes (`/api/me`, `/api/auth/logout`,
    //     passkey register start/finish) live in `protected`;
    //   - `protected` is wrapped with `RequireAuth` so anonymous
    //     requests get `401 authentication required`.
    // When `auth.enabled = false`: nothing is mounted; the server
    // keeps the pre-PR1 single-user trust boundary.
    if state.config.auth.enabled {
        let auth_store = state
            .auth_store
            .clone()
            .expect("auth_store must be Some when auth is enabled");
        let auth_layer = axum::middleware::from_fn_with_state(
            crate::auth::middleware::AuthState::new(auth_store, state.config.clone()),
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
            .layer(axum::middleware::from_fn(middleware::access_log_middleware))
            .with_state(state)
    } else {
        public
            .merge(protected)
            .layer(security_layers)
            .layer(axum::middleware::from_fn(middleware::access_log_middleware))
            .with_state(state)
    }
}

/// axum middleware that consumes one token from the supplied LLM
/// limiter per request, identified by the peer address attached by
/// [`axum::serve`] (i.e. `ConnectInfo<SocketAddr>`).
///
/// On rejection we return `429 Too Many Requests` with a
/// `Retry-After` header computed from the bucket's refill rate so
/// well-behaved clients can back off. The `loopback` carve-out lives
/// inside the limiter itself.
async fn llm_rate_limit_middleware(
    limiter: RateLimiter,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let peer: Option<ConnectInfo<SocketAddr>> = req.extensions().get().cloned();
    let Some(ConnectInfo(addr)) = peer else {
        // Without `ConnectInfo` (test harness, in-process calls) we
        // cannot key the bucket; let the request through so unit
        // tests don't all need a real TCP listener.
        return next.run(req).await;
    };
    match limiter.check(addr.ip()) {
        Ok(()) => next.run(req).await,
        Err(rate_limit::RateLimitError::Limited { retry_after_ms, .. }) => {
            let retry_secs = retry_after_ms.div_ceil(1000).max(1);
            let mut resp = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_str(&retry_secs.to_string())
                    .unwrap_or(HeaderValue::from_static("1")),
            );
            resp.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain"),
            );
            resp
        }
    }
}

/// Build the per-IP rate limiters from the active configuration. Kept
/// here (rather than next to `Config`) so the wiring stays in one
/// place — the limiter is built once per process and shared via
/// [`AppState`].
pub fn build_rate_limiters(cfg: &Config) -> (RateLimiter, RateLimiter) {
    (
        RateLimiter::new(RateLimitPolicy::stt(cfg.rate_limit.stt_per_min)),
        RateLimiter::new(RateLimitPolicy::llm(cfg.rate_limit.llm_per_min)),
    )
}

/// axum middleware that gates `/v1/*` requests behind the
/// `LLM_AUTH_MODE` policy.
///
/// Behaviour per [`config::LlmAuthMode`]:
/// - [`LlmAuthMode::Bearer`] (with `inbound_auth_key` set): reject
///   requests missing `Authorization: Bearer <key>` or carrying a
///   different key with `401 Unauthorized` and a `WWW-Authenticate`
///   hint so curl and SDKs surface a useful error.
/// - [`LlmAuthMode::Bearer`] (no key set): the auth gate is a no-op
///   and a warning is logged at boot — the operator enabled the
///   `bearer` mode without providing a key, so the proxy is effectively
///   public until they fix the config.
/// - [`LlmAuthMode::Forward`] / [`LlmAuthMode::Disabled`]: no inbound
///   inspection. `Disabled` is a deliberate opt-out and only affects
///   the startup warning emitted by `main`.
async fn llm_auth_middleware(
    cfg: Option<Arc<LlmConfig>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let Some(cfg) = cfg else {
        return next.run(req).await;
    };
    if !matches!(cfg.auth_mode, config::LlmAuthMode::Bearer) {
        return next.run(req).await;
    }
    let Some(expected) = cfg.inbound_auth_key.as_deref() else {
        // `bearer` mode without a key — log once at startup via
        // `main`, and let the request through here so a misconfigured
        // server still functions.
        return next.run(req).await;
    };
    let header_value = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let presented = header_value.and_then(|h| {
        h.strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
    });
    match presented {
        Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => next.run(req).await,
        _ => {
            let mut resp = (
                StatusCode::UNAUTHORIZED,
                "missing or invalid Authorization header",
            )
                .into_response();
            resp.headers_mut().insert(
                axum::http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"nagent-llm-proxy\""),
            );
            resp
        }
    }
}

/// Constant-time byte slice comparison. Avoids leaking the key length
/// via the early-exit path of `==`. Safe for ASCII bearer tokens which
/// never contain non-ASCII bytes.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}
