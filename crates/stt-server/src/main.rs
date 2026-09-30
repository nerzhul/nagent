//! Server entry point.
//!
//! Wires together the configuration, the [`WhisperBackend`], the inference
//! worker, the session map, the WebSocket router, and the watchdog.

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use stt_server::agents::ServiceRegistry;
use stt_server::config::AuthBackendKind;
use stt_server::documents_cli;
use stt_server::{
    agents, auth, build_rate_limiters, build_router, credentials, llm, migrate_cli, router,
    session, tts, watchdog, AppState, CliArgs, Config,
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

use stt_core::{default_worker_count, WhisperBackend, WorkerPool};

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    // `stt-server auth …` subcommand dispatch happens FIRST so the
    // CLI subcommand arguments (`create-admin`, `--email`, etc.)
    // do not get rejected by the strict `CliArgs::parse` loop below.
    // The dispatch walks the full argv (skipping the binary name)
    // so `--config FOO auth list-users` routes correctly even when
    // the operator puts the global flag before the subcommand.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(idx) = argv.iter().position(|a| a == "auth") {
        // Concatenate leading global flags (`--config FOO`) with
        // the trailing subcommand args (`list-users`). The auth CLI
        // re-parses `--config` from the combined vector and routes
        // the rest to the subcommand parser. This handles both
        // `stt-server auth …` and `stt-server --config FOO auth …`.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return auth::cli::run_auth_cli(combined).await;
    }
    if let Some(idx) = argv.iter().position(|a| a == "migrate") {
        // Same dispatcher trick as the `auth` branch above. Order
        // matters: `auth` stays first so existing behaviour is
        // unchanged, then `migrate`, then fallthrough to server
        // boot.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return migrate_cli::run_migrate_cli(combined).await;
    }
    if let Some(idx) = argv.iter().position(|a| a == "documents") {
        // Same dispatcher trick as the `auth` / `migrate` branches
        // above. Sits after both so `--config FOO documents purge
        // …` routes correctly.
        let mut argv = argv;
        let trailing = argv.split_off(idx + 1);
        let mut combined = argv;
        combined.extend(trailing);
        return documents_cli::run_documents_cli(combined).await;
    }

    init_tracing();

    // CLI flags must be parsed before tracing emits its first
    // `info!` so a malformed `--config` exits cleanly without
    // producing a half-initialised log line.
    let cli = CliArgs::parse();

    let cfg = Config::load(&cli).map_err(|e| anyhow::anyhow!("{e}"))?;
    info!(addr = %cfg.bind_addr, model = ?cfg.whisper_model_path, "starting nagent stt-server");

    // Auth bootstrap (migrations + sqlite first-admin). Runs only
    // when `auth.enabled = true`; a `None` result here means the
    // subsystem is disabled. Errors here are fatal — a half-broken
    // auth DB on a server that expects it is the worst-case state
    // (silent fallthrough to the pre-PR1 trust boundary would be a
    // security regression).
    let auth_store = match auth::boot::auto_bootstrap(&Arc::new(cfg.clone())).await {
        Ok(store) => store,
        Err(e) => {
            return Err(anyhow::anyhow!("{}", auth::boot::format_bootstrap_err(&e)));
        }
    };

    // Build the OIDC + passkey sub-states when their respective
    // backends are enabled (i.e. listed in `cfg.auth.backends`).
    // Both are best-effort: a missing configuration or a discovery
    // failure only logs a warning so the server still boots (the
    // auth routes for that backend simply 501 at request time).
    let oidc_enabled = cfg.auth.backends.contains(&AuthBackendKind::Oidc);
    let passkey_enabled = cfg.auth.backends.contains(&AuthBackendKind::Passkey);
    let auth_oidc = if oidc_enabled {
        auth::oidc::build_state(
            auth_store.clone().unwrap_or_else(|| unreachable!()),
            Arc::new(cfg.clone()),
        )
        .await
        .ok()
    } else {
        None
    };
    let auth_passkey = if passkey_enabled && auth_store.is_some() {
        auth::passkey::build_state(auth_store.clone().unwrap(), Arc::new(cfg.clone())).ok()
    } else {
        None
    };
    if let Some(ref s) = auth_oidc {
        tracing::info!(issuer = %s.cfg.issuer, "OIDC backend ready");
    }
    if let Some(ref _p) = auth_passkey {
        tracing::info!("passkey backend ready");
    }

    // ---- Backend ---------------------------------------------------------
    let backend: Arc<dyn WhisperBackend> = build_backend(&cfg).await?;
    let backend_info = backend.info();
    info!(
        backend = %backend_info.name,
        model = %backend_info.model_id,
        size_bytes = backend_info.model_size_bytes,
        recommended_workers = backend_info.recommended_workers,
        "backend ready"
    );

    // ---- Worker pool sizing (P0 + P2 of the perf plan) ------------------
    // The pool owns N independent backend instances (each gets a fresh
    // WhisperState, so they actually run in parallel instead of
    // serializing on a shared mutex). Dispatch is sticky on
    // `session_id`, which keeps per-session FIFO ordering intact and
    // lets us warm any per-session cache once and reuse it across
    // turns.
    //
    // Priority for the chosen count:
    //   1. `Config::inference_workers` if the operator set it.
    //   2. `BackendInfo::recommended_workers` (model-size aware) if
    //      the backend advertises a hint.
    //   3. `stt_core::default_worker_count` (CPU-count fallback, capped
    //      at 8).
    let worker_count = cfg
        .inference_workers
        .unwrap_or_else(|| default_worker_count(&backend_info))
        .max(1);
    info!(
        inference_workers = worker_count,
        "worker pool sizing decided"
    );

    // ---- Session map (shared) --------------------------------------------
    let sessions: session::SessionMap = Arc::new(dashmap::DashMap::new());

    // ---- Worker pool -----------------------------------------------------
    // Each worker gets its own backend clone. For the in-process mock
    // that's a cheap `Clone`; for the real whisper-rs backend the
    // factory would build N independent `WhisperState`s. The
    // `backend_for_pool` Arc is captured by the factory closure and
    // cloned per worker — keep it alive for the lifetime of the pool.
    let backend_for_pool = Arc::clone(&backend);
    let pool = WorkerPool::spawn(worker_count, move || Arc::clone(&backend_for_pool));
    let job_tx = pool.dispatch();

    // ---- Result router (unchanged) --------------------------------------
    // The router still consumes `InferResponse`s from a single channel
    // — same wiring as before the pool. The actual responses today
    // flow back to the WS handler through the per-job oneshot, but
    // the router is kept around as the integration point for the P1
    // "Streaming partial transcripts" feature.
    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(cfg.max_queue);
    let _router_shutdown = router::ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

    // ---- Watchdog --------------------------------------------------------
    let watchdog_sessions = Arc::clone(&sessions);
    let watchdog_timeout = cfg.session_idle_timeout;
    tokio::spawn(async move {
        watchdog::run(watchdog_sessions, watchdog_timeout).await;
    });

    // ---- Documents ----------------------------------------------------------
    //
    // Build the `DocumentStore` whenever the cargo feature is on AND
    // `documents.enabled = true` AND the auth DB is reachable (the
    // documents table lives in the auth DB so `auth_store` must be
    // `Some`). The check runs early — a misconfigured server refuses
    // to boot with a clear error instead of 500-ing on every upload.
    //
    // The periodic purge sweep lives in its own `tokio::spawn` so a
    // transient DB error does not block the rest of the server.
    //
    // SEV 2 fix: build the chat-session binding handle UP-FRONT so
    // the `POST /v1/chat/session` mint endpoint is always reachable
    // (when auth is enabled). The documents routes + the
    // `read_document` agent also read this handle to verify
    // `(user, session)` bindings on every request.
    let chat_sessions_state: Option<stt_server::chat_sessions::ChatSessions> = auth_store
        .clone()
        .map(stt_server::chat_sessions::ChatSessions::new);
    let documents_state: Option<stt_server::documents::DocumentStore> = if cfg.documents.enabled {
        let store = match auth_store.clone() {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "[documents].enabled = true requires auth.enabled = true; \
                 the documents table lives in the auth DB."
                ));
            }
        };
        // Verify the cache dir is writable before opening the door to
        // uploads — a misconfigured PVC should surface at boot, not at
        // the first 5xx.
        if let Err(e) = stt_server::documents::storage::check_writable(&cfg.documents.cache_dir) {
            return Err(anyhow::anyhow!(
                "[documents].cache_dir {} is not writable: {e}",
                cfg.documents.cache_dir.display()
            ));
        }
        // Run a sweep at boot so a long downtime does not leave the
        // cache dir full. The first tick of the periodic task is
        // skipped (see `run_periodic_purge`); this explicit one is
        // the "boot" sweep.
        let store = stt_server::documents::DocumentStore::new(
            store,
            cfg.documents.max_extracted_chars,
            cfg.documents.cache_dir.clone(),
        );
        match stt_server::documents::purge::purge_older_than(
            &store,
            &cfg.documents.cache_dir,
            std::time::Duration::from_secs(cfg.documents.default_ttl_days as u64 * 86_400),
        )
        .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(purged = n, "documents: boot sweep removed {n} row(s)"),
            Err(e) => tracing::warn!(error = %e, "documents: boot sweep failed"),
        }
        if cfg.documents.purge_interval_hours > 0 {
            let interval =
                std::time::Duration::from_secs(cfg.documents.purge_interval_hours * 3_600);
            let ttl =
                std::time::Duration::from_secs(cfg.documents.default_ttl_days as u64 * 86_400);
            let store_for_task = store.clone();
            let cache_dir = cfg.documents.cache_dir.clone();
            tokio::spawn(async move {
                stt_server::documents::purge::run_periodic_purge(
                    store_for_task,
                    cache_dir,
                    interval,
                    ttl,
                )
                .await;
            });
        }
        Some(store)
    } else {
        // Documents module is always compiled in; runtime flag
        // off → the routes are simply not mounted. No rebuild
        // required.
        None
    };

    // ---- HTTP router -----------------------------------------------------
    let ready = Arc::new(AtomicBool::new(true));
    // Surface a startup warning when the operator is exposing the
    // proxy on a non-loopback bind with auth explicitly disabled —
    // the worst-case deployment we are trying to prevent (P0 of the
    // security plan).
    if cfg.llm.auth_mode == stt_server::config::LlmAuthMode::Disabled
        && !cfg.bind_addr.ip().is_loopback()
    {
        tracing::warn!(
            bind_addr = %cfg.bind_addr,
            "LLM_AUTH_MODE=disabled while binding a non-loopback address; \
             /v1/* is fully public. Set LLM_AUTH_MODE=bearer and LLM_API_KEY \
             to require an Authorization header."
        );
    }
    if cfg.llm.auth_mode == stt_server::config::LlmAuthMode::Bearer
        && cfg.llm.inbound_auth_key.is_none()
    {
        tracing::warn!(
            "LLM_AUTH_MODE=bearer but LLM_API_KEY is unset; the /v1/* \
             auth gate is effectively disabled. Set LLM_API_KEY to enforce it."
        );
    }
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
    // Build the agent registry. When `documents.enabled = true` AND
    // `documents_state` is `Some`, append the `read_document` agent
    // so the LLM can pull text out of an uploaded file. The two-
    // step build is intentional — `from_config` is the stable
    // surface used by every binary variant;
    // `from_config_with_documents` is the documents-specific
    // extension.
    let agents = agents::AgentRegistry::from_config_with_documents(
        &cfg.agents,
        &cfg.documents,
        documents_state.clone(),
        chat_sessions_state.clone(),
    );
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
    // Per-user credentials resolver. Required when `auth.enabled`
    // AND at least one agent is registered — the boot fails
    // loudly otherwise so a misconfigured deployment does not
    // silently lose the ability to decrypt per-user credentials.
    // The encryption key comes from `[auth.credentials].key` in
    // the resolved config (TOML-only, no env-var indirection).
    let services = ServiceRegistry::empty().into_arc();
    let (credential_resolver, credentials_key) = if auth_store.is_some() && !agents.is_empty() {
        let key = match credentials::key::CredentialsKey::from_hex(&cfg.auth.credentials.key) {
            Ok(k) => Arc::new(k),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "[auth.credentials].key is required when auth.enabled and \
                     agents.enabled (per-user credentials framework): {e}"
                ));
            }
        };
        let resolver = credentials::CredentialResolver::new(
            auth_store.clone().expect("auth_store checked above"),
            key.clone(),
            None,
            None,
        );
        tracing::info!("per-user credentials framework enabled");
        (Some(Arc::new(resolver)), Some(key))
    } else {
        tracing::info!(
            "per-user credentials framework disabled (auth.enabled or agents.enabled is off)"
        );
        (None, None)
    };

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
        // `auto_bootstrap` returns `Some(store)` when `auth.enabled
        // = true` and the sqlite DB is reachable. We stash it on the
        // `AppState` so the HTTP handlers can reach it without
        // opening a second pool.
        auth_store: auth_store.clone(),
        auth_oidc: auth_oidc.clone(),
        auth_passkey: auth_passkey.clone(),
        auth_rate_limiter: auth::rate_limit::LoginRateLimiter::new(),
        services,
        credential_resolver,
        credentials_key,
        documents: documents_state.clone(),
        chat_sessions: chat_sessions_state.clone(),
    });

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    info!("listening on http://{}", cfg.bind_addr);
    // Use the `with_connect_info` variant so the auth login handler
    // can read the peer IP via `ConnectInfo<SocketAddr>` (used for
    // the per-(email, ip) login rate-limit + the audit row). The
    // vanilla `into_make_service` would omit it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn init_tracing() {
    // Default filter demotes the ONNX Runtime allocator chatter
    // (`ort::logging` emits BFC-arena allocation lines at `info` on
    // every model load — see the
    // "Allocated memory at 0x…", "Extending BFCArena", "Extended
    // allocation by … bytes" lines) down to `debug` so the runtime
    // log stays usable. Operators who want to see the allocator
    // accounting can opt back in with `RUST_LOG=ort=debug`. The
    // `whisper_rs::*` target is treated the same way for symmetry:
    // whisper-rs emits a per-chunk "mel" trace at info that drowns
    // out everything else during a long session.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,stt_server=debug,stt_core=debug,ort=debug,ort::logging=debug,whisper_rs=debug",
        )
    });
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
