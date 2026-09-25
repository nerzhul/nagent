//! Server entry point.
//!
//! Wires together the configuration, the [`WhisperBackend`], the inference
//! worker, the session map, the WebSocket router, and the watchdog.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use stt_server::{
    build_rate_limiters, build_router, llm, router, session, watchdog, AppState, Config,
};

use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stt_core::{InferenceJob, InferenceWorker, WhisperBackend};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = Config::from_env()?;
    info!(addr = %cfg.bind_addr, model = ?cfg.whisper_model_path, "starting nagent stt-server");

    // ---- Backend ---------------------------------------------------------
    let backend: Arc<dyn WhisperBackend> = build_backend(&cfg).await?;
    info!(
        backend = backend.backend_name(),
        model = backend.model_id(),
        "backend ready"
    );

    // ---- Session map (shared) --------------------------------------------
    let sessions: session::SessionMap = Arc::new(dashmap::DashMap::new());

    // ---- Channels --------------------------------------------------------
    let (job_tx, job_rx) = mpsc::channel::<InferenceJob>(cfg.max_queue);
    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(cfg.max_queue);

    // ---- Worker ----------------------------------------------------------
    let _worker = InferenceWorker::spawn(Arc::clone(&backend), job_rx);

    // ---- Result router ---------------------------------------------------
    let _router_shutdown = router::ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

    // ---- Watchdog --------------------------------------------------------
    let watchdog_sessions = Arc::clone(&sessions);
    let watchdog_timeout = cfg.session_idle_timeout;
    tokio::spawn(async move {
        watchdog::run(watchdog_sessions, watchdog_timeout).await;
    });

    // ---- HTTP router -----------------------------------------------------
    let ready = Arc::new(AtomicBool::new(true));
    let llm = if cfg.llm.enabled {
        info!(
            base_url = %cfg.llm.base_url,
            default_model = %cfg.llm.default_model,
            "LLM proxy enabled"
        );
        let cfg = Arc::new(cfg.llm.clone());
        Some(llm::LlmClient::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?)
    } else {
        info!("LLM proxy disabled (set LLM_ENABLED=true to enable)");
        None
    };
    let (stt_rate_limiter, llm_rate_limiter) = build_rate_limiters(&cfg);
    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready,
        config: Arc::new(cfg.clone()),
        llm,
        stt_rate_limiter,
        llm_rate_limiter,
    });

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    info!("listening on http://{}", cfg.bind_addr);
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,stt_server=debug,stt_core=debug"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Build the backend: real `whisper-rs` when the `real-backend` feature is
/// enabled, otherwise the in-process mock used by CI / tests / dev.
async fn build_backend(cfg: &Config) -> anyhow::Result<Arc<dyn WhisperBackend>> {
    #[cfg(feature = "real-backend")]
    {
        let backend = stt_core::whisper_backend::WhisperRsBackend::load(&cfg.whisper_model_path)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Arc::new(backend))
    }
    #[cfg(not(feature = "real-backend"))]
    {
        let model_id = cfg
            .whisper_model_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("mock-model")
            .to_string();
        let backend = stt_core::MockBackend::new(model_id);
        Ok(Arc::new(backend))
    }
}
