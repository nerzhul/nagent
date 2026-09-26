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

#![warn(missing_debug_implementations)]

pub mod agents;
pub mod config;
pub mod config_file;
pub mod llm;
pub mod llm_prompt;
pub mod middleware;
pub mod rate_limit;
pub mod router;
pub mod session;
pub mod static_assets;
pub mod validation;
pub mod version;
pub mod watchdog;
pub mod ws_handler;

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
use tokio::sync::mpsc;

use stt_core::{InferenceJob, WhisperBackend};

/// Shared application state injected into every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn WhisperBackend>,
    pub sessions: SessionMap,
    pub job_tx: mpsc::Sender<InferenceJob>,
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
    /// Per-source-IP token bucket for the STT pipeline (consumed at
    /// WS upgrade and per inbound WS frame).
    pub stt_rate_limiter: RateLimiter,
    /// Per-source-IP token bucket for the `/v1/*` LLM proxy.
    pub llm_rate_limiter: RateLimiter,
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
            .field("stt_rate_limiter", &self.stt_rate_limiter)
            .field("llm_rate_limiter", &self.llm_rate_limiter)
            .finish()
    }
}

/// Build the axum router around [`AppState`]. Exposed for tests.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Always-on security headers applied to *every* response (static
    // frontend, health checks, version probe, WS upgrade, LLM proxy).
    let security_layers = (
        middleware::security_headers_layer(),
        middleware::referrer_policy_layer(),
        middleware::nosniff_layer(),
    );

    // STT routes are always registered. The LLM routes are gated on
    // `LLM_ENABLED` so disabling the feature leaves no trace of it
    // (the chat view simply sees 404 on `/v1/*`).
    let stt_app = Router::new()
        .route("/", get(ws_handler::index_handler))
        .route("/healthz", get(ws_handler::healthz))
        .route("/api/version", get(ws_handler::version_handler))
        .route("/ws", get(ws_handler::ws_upgrade))
        .route("/static/*path", get(ws_handler::static_path_handler))
        .layer(security_layers.clone());

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
    let stt_app = if state.agents.is_some() {
        let cors_origins = state
            .llm
            .as_ref()
            .map(|l| l.cfg().cors_allow_origins.clone())
            .unwrap_or_default();
        let cors = middleware::cors_layer(&cors_origins);
        let llm_limiter = state.llm_rate_limiter.clone();
        let agents_app = Router::new()
            .route("/v1/agents", get(llm::agents_list))
            .route("/v1/agents/:name/invoke", post(llm::agent_invoke))
            .layer(axum::middleware::from_fn(move |req, next| {
                let limiter = llm_limiter.clone();
                async move { llm_rate_limit_middleware(limiter, req, next).await }
            }))
            .layer(cors)
            .layer(security_layers.clone());
        stt_app.merge(agents_app)
    } else {
        stt_app
    };

    if let Some(llm) = &state.llm {
        // The LLM proxy gets its own CORS layer driven by
        // `LLM_CORS_ALLOW_ORIGINS`, a per-IP rate limiter, and the
        // same security headers.
        let cors = middleware::cors_layer(&llm.cfg().cors_allow_origins);
        let llm_limiter = state.llm_rate_limiter.clone();
        let llm_app = Router::new()
            .route("/v1/chat/completions", post(llm::chat_completions))
            .route("/v1/models", get(llm::models_list))
            .layer(axum::middleware::from_fn(move |req, next| {
                let limiter = llm_limiter.clone();
                async move { llm_rate_limit_middleware(limiter, req, next).await }
            }))
            .layer(cors)
            .layer(security_layers);
        stt_app.merge(llm_app).with_state(state)
    } else {
        stt_app.with_state(state)
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
