//! `stt-server` — axum HTTP/WebSocket server for the STT pipeline.
//!
//! The server is split into:
//! - [`config`] — env-var parsing.
//! - [`session`] — `SessionState` and the shared `SessionMap`.
//! - [`ws_handler`] — per-connection upgrade + dispatch loop.
//! - [`router`] — `ResultRouter` that forwards worker output to the right session.
//! - [`watchdog`] — periodic sweep that drops idle sessions.
//! - [`static_assets`] — embedded frontend assets served at `/` and `/static/*`.
//! - [`llm`] — optional OpenAI-compatible proxy to a local LLM (Ollama).

#![warn(missing_debug_implementations)]

pub mod config;
pub mod llm;
pub mod router;
pub mod session;
pub mod static_assets;
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
    // STT routes are always registered. The LLM routes are gated on
    // `LLM_ENABLED` so disabling the feature leaves no trace of it
    // (the chat view simply sees 404 on `/v1/*`).
    let mut app = Router::new()
        .route("/", get(ws_handler::index_handler))
        .route("/healthz", get(ws_handler::healthz))
        .route("/api/version", get(ws_handler::version_handler))
        .route("/ws", get(ws_handler::ws_upgrade))
        .route("/static/*path", get(ws_handler::static_path_handler));
    if state.llm.is_some() {
        app = app
            .route("/v1/chat/completions", post(llm::chat_completions))
            .route("/v1/models", get(llm::models_list));
    }
    app.with_state(state)
}
