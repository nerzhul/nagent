//! Server entry point.
//!
//! Wires together the configuration, the [`WhisperBackend`], the inference
//! worker, the session map, the WebSocket router, and the watchdog.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use stt_server::{
    agents, build_rate_limiters, build_router, llm, router, session, tts, watchdog, AppState,
    CliArgs, Config,
};

/// Locate the directory containing the bundled `espeak-ng-data/`
/// phoneme + voice tables and expose it via the
/// `PIPER_ESPEAKNG_DATA_DIRECTORY` env var that `espeak-rs` reads at
/// init time.
///
/// `espeak-rs` only looks at three locations:
///   1. `$PIPER_ESPEAKNG_DATA_DIRECTORY/espeak-ng-data/`
///   2. `$CWD/espeak-ng-data/`
///   3. `$EXE_DIR/espeak-ng-data/`
///
/// The bundled build (CMake in `espeak-rs-sys`) puts the data under
/// `target/release/build/espeak-rs-sys-{hash}/out/share/espeak-ng-data/`,
/// which is none of the above. Without help, `espeak_Initialize`
/// returns 0 (failure) and `text_to_phonemes` blows up with
/// "Failed to initialize eSpeak-ng (code 0). Try setting
/// `PIPER_ESPEAKNG_DATA_DIRECTORY`...".
///
/// We try, in order:
///   - The bundled build's `out/share/` dir (stable path under
///     `OUT_DIR/../share/` from any of our build script's artefacts,
///     but here we reach it via `target/` next to the executable
///     which is good enough for `cargo run` + `cargo install`).
///   - System-installed espeak-ng data (`/usr/share/espeak-ng-data`
///     on Arch/Debian/Fedora; the package puts the dir directly there
///     so the env var should point to `/usr/share`).
///   - The current working directory + exe directory (already
///     covered by `espeak-rs` itself, no action needed).
fn setup_espeak_data_dir() {
    use std::path::PathBuf;

    let find_data = |parent: &std::path::Path| -> Option<PathBuf> {
        let direct = parent.join("espeak-ng-data");
        if direct.is_dir() {
            return Some(parent.to_path_buf());
        }
        let under_share = parent.join("share").join("espeak-ng-data");
        if under_share.is_dir() {
            return Some(parent.join("share"));
        }
        None
    };

    // 1. Bundled: walk the target dir looking for the latest
    //    `espeak-rs-sys-*` build. The hash suffix is non-deterministic
    //    so we scan the directory and pick the most recently modified.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(target_dir) = exe.parent().and_then(|p| p.parent()) {
            if let Ok(rd) = std::fs::read_dir(target_dir.join("build")) {
                let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = rd
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if !name.starts_with("espeak-rs-sys-") {
                            return None;
                        }
                        let out_share = e.path().join("out").join("share");
                        let modified = e
                            .metadata()
                            .and_then(|m| m.modified())
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                        find_data(&out_share).map(|p| (modified, p))
                    })
                    .collect();
                candidates.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
                if let Some((_, path)) = candidates.into_iter().next() {
                    std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &path);
                    return;
                }
            }
        }
    }

    // 2. System-installed espeak-ng (Arch: `/usr/share/espeak-ng-data/`,
    //    Debian/Fedora: same path). Point the env var at `/usr/share`
    //    so `espeak-rs`'s `parent.join("espeak-ng-data")` check finds it.
    for parent in ["/usr/share", "/usr/local/share"] {
        if let Some(p) = find_data(std::path::Path::new(parent)) {
            std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &p);
            return;
        }
    }
}

use tokio::sync::mpsc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use stt_core::{InferenceJob, InferenceWorker, WhisperBackend};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // CLI flags must be parsed before tracing emits its first
    // `info!` so a malformed `--config` exits cleanly without
    // producing a half-initialised log line.
    let cli = CliArgs::parse();
    let cfg = Config::load(&cli).map_err(|e| anyhow::anyhow!("{e}"))?;
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
    let agents = agents::AgentRegistry::from_config(&cfg.agents);
    if !agents.is_empty() {
        info!(count = agents.len(), "agent registry built");
    } else {
        info!("agent registry empty (no agents compiled in or AGENTS_ENABLED=false)");
    }
    // espeak-rs reads `PIPER_ESPEAKNG_DATA_DIRECTORY` once at first
    // init. Set it BEFORE constructing any `Piper` so the bundled
    // `espeak-ng-data/` tables are findable. This is a no-op when
    // the env var is already set (e.g. by a wrapper script) or
    // when TTS is disabled.
    setup_espeak_data_dir();
    let tts = tts::TtsEngine::load(&cfg.tts)
        .await
        .map_err(|e| anyhow::anyhow!("TTS init failed: {e}"))?
        .map(Arc::new);
    let state = Arc::new(AppState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready,
        config: Arc::new(cfg.clone()),
        llm,
        agents: if agents.is_empty() {
            None
        } else {
            Some(agents)
        },
        tts,
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
