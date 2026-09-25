//! `stt-server` — axum HTTP/WebSocket server for the STT pipeline.
//!
//! The server is split into:
//! - [`config`] — env-var parsing.
//! - [`session`] — `SessionState` and the shared `SessionMap`.
//! - [`validation`] — input checks applied to inbound WebSocket frames.
//! - [`middleware`] — always-on security headers and LLM CORS layer.
//! - [`ws_handler`] — per-connection upgrade + dispatch loop.
//! - [`router`] — `ResultRouter` that forwards worker output to the right session.
//! - [`watchdog`] — periodic sweep that drops idle sessions.
//! - [`static_assets`] — embedded frontend assets served at `/` and `/static/*`.
//! - [`llm`] — optional OpenAI-compatible proxy to a local LLM (Ollama).

#![warn(missing_debug_implementations)]

pub mod config;
pub mod llm;
pub mod middleware;
pub mod router;
pub mod session;
pub mod static_assets;
pub mod validation;
pub mod version;
pub mod watchdog;
pub mod ws_handler;

pub use config::Config;
use session::SessionMap;
pub use version::VersionInfo;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
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

    if let Some(llm) = &state.llm {
        // The LLM proxy gets its own CORS layer driven by
        // `LLM_CORS_ALLOW_ORIGINS` plus the same security headers.
        let cors = middleware::cors_layer(&llm.cfg().cors_allow_origins);
        let llm_app = Router::new()
            .route("/v1/chat/completions", post(llm::chat_completions))
            .route("/v1/models", get(llm::models_list))
            .layer(cors)
            .layer(security_layers);
        stt_app.merge(llm_app).with_state(state)
    } else {
        stt_app.with_state(state)
    }
}
