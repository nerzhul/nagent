//! `stt-server` — axum HTTP/WebSocket server for the STT pipeline.
//!
//! The server is split into:
//! - [`config`] — env-var parsing.
//! - [`session`] — `SessionState` and the shared `SessionMap`.
//! - [`ws_handler`] — per-connection upgrade + dispatch loop.
//! - [`router`] — `ResultRouter` that forwards worker output to the right session.
//! - [`watchdog`] — periodic sweep that drops idle sessions.
//! - [`static_assets`] — embedded frontend assets served at `/` and `/static/*`.

#![warn(missing_debug_implementations)]

pub mod config;
pub mod router;
pub mod session;
pub mod static_assets;
pub mod watchdog;
pub mod ws_handler;

pub use config::Config;
use session::SessionMap;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::routing::get;
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
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("backend", &"<dyn WhisperBackend>")
            .field("sessions", &self.sessions)
            .field("job_tx", &self.job_tx)
            .field("ready", &self.ready)
            .field("config", &self.config)
            .finish()
    }
}

/// Build the axum router around [`AppState`]. Exposed for tests.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(ws_handler::index_handler))
        .route("/healthz", get(ws_handler::healthz))
        .route("/ws", get(ws_handler::ws_upgrade))
        .route("/static/*path", get(ws_handler::static_path_handler))
        .with_state(state)
}
