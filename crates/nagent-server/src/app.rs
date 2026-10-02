//! `app::build_app` — composition root.
//!
//! Owns the boot wiring that used to live inline in `main.rs`:
//! auth store bootstrap, OIDC + passkey sub-states, the whisper
//! backend + worker pool, the LLM client, the agent registry,
//! the TTS engine, the documents store, and the per-IP rate
//! limiters. Returns `Arc<AppState>`; `main.rs` calls it after
//! CLI parsing and feeds the result to `http::build_router`.
//!
//! `build_app` is `pub` so tests can drive the full boot path
//! (or short-circuit specific subsystems through
//! [`crate::testing`]).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use stt_core::{default_worker_count, WhisperBackend, WorkerPool};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::auth::login_rate_limit::LoginRateLimiter;
use crate::config::{AuthBackendKind, Config};
use crate::credentials::{CredentialResolver, CredentialsKey};
use crate::http::build_rate_limiters;
use crate::llm::LlmClient;
use crate::state::{
    AppState, AuthState, ChatSessionsState, DocumentsState, LlmState, SttState, TtsState,
};
use crate::stt::{result_router, session, watchdog};
use crate::tts::TtsEngine;
use crate::{agents, llm, tts};
use nagent_agents::ServiceRegistry;

/// Build the full application state from a resolved `Config`.
///
/// All boot wiring — auth store bootstrap, OIDC + passkey
/// sub-states, the whisper backend, the worker pool, the LLM
/// client, the agent registry, the TTS engine, the documents
/// store, the per-IP rate limiters — happens here in one place
/// so tests can swap pieces in/out through the [`crate::testing`]
/// builder instead of re-implementing the chain.
pub async fn build_app(cfg: &Config) -> anyhow::Result<Arc<AppState>> {
    let cfg = Arc::new(cfg.clone());

    // ---- Auth subsystem --------------------------------------------------
    //
    // `auto_bootstrap` runs the migrations and (when the auth
    // backend is sqlite) creates the first admin. Errors here are
    // fatal — a half-broken auth DB on a server that expects it
    // is the worst-case state (silent fallthrough to the pre-// trust boundary would be a security regression).
    let auth_store: Option<nagent_db::Db> = match crate::auth::boot::auto_bootstrap(&cfg).await {
        Ok(store) => store,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "{}",
                crate::auth::boot::format_bootstrap_err(&e)
            ));
        }
    };

    // Build the OIDC + passkey sub-states when their respective
    // backends are enabled. Both are best-effort: a missing
    // configuration or a discovery failure only logs a warning so
    // the server still boots (the auth routes for that backend
    // simply 501 at request time).
    let oidc_enabled = cfg.auth.backends.contains(&AuthBackendKind::Oidc);
    let passkey_enabled = cfg.auth.backends.contains(&AuthBackendKind::Passkey);
    let auth_oidc = if oidc_enabled {
        match auth_store.clone() {
            Some(store) => crate::auth::oidc::build_state(store, cfg.clone())
                .await
                .ok(),
            None => None,
        }
    } else {
        None
    };
    let auth_passkey = if passkey_enabled && auth_store.is_some() {
        auth_store
            .clone()
            .and_then(|s| crate::auth::passkey::build_state(s, cfg.clone()).ok())
    } else {
        None
    };
    if let Some(ref s) = auth_oidc {
        tracing::info!(issuer = %s.cfg.issuer, "OIDC backend ready");
    }
    if auth_passkey.is_some() {
        tracing::info!("passkey backend ready");
    }

    // ---- Documents -------------------------------------------------------
    //
    // Build the `DocumentStore` whenever the cargo feature is on AND
    // `documents.enabled = true` AND the auth DB is reachable (the
    // documents table lives in the auth DB so `auth_store` must be
    // `Some`). The check runs early — a misconfigured server refuses
    // to boot with a clear error instead of 500-ing on every upload.
    let chat_sessions_state = auth_store.clone().map(|s| ChatSessionsState {
        sessions: crate::chat::sessions::ChatSessions::new(s.admin().chat_sessions),
    });
    let documents_state = if cfg.documents.enabled {
        let store = match auth_store.clone() {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "[documents].enabled = true requires auth.enabled = true; \
                     the documents table lives in the auth DB."
                ));
            }
        };
        if let Err(e) = crate::documents::storage::check_writable(&cfg.documents.cache_dir) {
            return Err(anyhow::anyhow!(
                "[documents].cache_dir {} is not writable: {e}",
                cfg.documents.cache_dir.display()
            ));
        }
        let store = DocumentsState {
            store: crate::documents::DocumentStore::new(
                store,
                cfg.documents.max_extracted_chars,
                cfg.documents.cache_dir.clone(),
                cfg.documents.pdf_extract_concurrency,
            ),
        };
        match crate::documents::purge::purge_older_than(
            &store.store,
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
            let store_for_task = store.store.clone();
            let cache_dir = cfg.documents.cache_dir.clone();
            tokio::spawn(async move {
                crate::documents::purge::run_periodic_purge(
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
        None
    };

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

    // ---- Worker pool sizing ----------------------------------------------
    let worker_count = cfg
        .inference_workers
        .unwrap_or_else(|| default_worker_count(&backend_info))
        .max(1);
    info!(
        inference_workers = worker_count,
        "worker pool sizing decided"
    );

    let sessions: session::SessionMap = Arc::new(dashmap::DashMap::new());
    let backend_for_pool = Arc::clone(&backend);
    let pool = WorkerPool::spawn(worker_count, move || Arc::clone(&backend_for_pool));
    let job_tx = pool.dispatch();

    let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(cfg.max_queue);
    let _router_shutdown = result_router::ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

    let watchdog_sessions = Arc::clone(&sessions);
    let watchdog_timeout = cfg.session_idle_timeout;
    tokio::spawn(async move {
        watchdog::run(watchdog_sessions, watchdog_timeout).await;
    });

    // ---- HTTP router prep ------------------------------------------------
    let ready = Arc::new(AtomicBool::new(true));
    if !cfg.bind_addr.ip().is_loopback() && cfg.trusted_proxies.cidrs.is_empty() {
        warn!(
            bind_addr = %cfg.bind_addr,
            "server is bound to a non-loopback address with no `trusted_proxies.cidr` \
             configured — X-Forwarded-For is ignored, so every client behind a reverse \
             proxy shares one rate-limit bucket. Set NAGENT_TRUSTED_PROXIES (or \
             [server].trusted_proxies.cidr) to the proxy's IP range."
        );
    }
    if cfg.llm.auth_mode == crate::config::LlmAuthMode::Disabled
        && !cfg.bind_addr.ip().is_loopback()
    {
        warn!(
            bind_addr = %cfg.bind_addr,
            "LLM_AUTH_MODE=disabled while binding a non-loopback address; \
             /v1/* is fully public. Set LLM_AUTH_MODE=bearer and LLM_API_KEY \
             to require an Authorization header."
        );
    }
    if cfg.llm.auth_mode == crate::config::LlmAuthMode::Bearer && cfg.llm.inbound_auth_key.is_none()
    {
        warn!(
            "LLM_AUTH_MODE=bearer but LLM_API_KEY is unset; the /v1/* \
             auth gate is effectively disabled. Set LLM_API_KEY to enforce it."
        );
    }

    // ---- LLM client + per-LLM rate limiter -------------------------------
    let llm = if cfg.llm.enabled {
        info!(
            base_url = %cfg.llm.base_url,
            default_model = %cfg.llm.default_model,
            "LLM proxy enabled"
        );
        let llm_cfg = Arc::new(cfg.llm.clone());
        let client = LlmClient::new(llm_cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (_stt_limiter, llm_limiter) = build_rate_limiters(&cfg);
        Some(LlmState {
            client,
            rate_limiter: llm_limiter,
            cfg: cfg.llm.clone(),
        })
    } else {
        info!("LLM proxy disabled (set LLM_ENABLED=true to enable)");
        None
    };
    // STT rate limiter (the LLM one was already used above if the
    // proxy is enabled, or discarded otherwise).
    let (stt_rate_limiter, _) = build_rate_limiters(&cfg);

    // ---- Agent registry ---------------------------------------------------
    let agents = agents::build_registry(
        &cfg.agents,
        documents_state.as_ref().map(|d| d.store.clone()),
        chat_sessions_state.as_ref().map(|c| c.sessions.clone()),
    );
    if !agents.is_empty() {
        info!(count = agents.len(), "agent registry built");
    } else {
        info!("agent registry empty (no agents compiled in or AGENTS_ENABLED=false)");
    }
    let agents = if agents.is_empty() {
        None
    } else {
        Some(agents)
    };

    // ---- TTS ------------------------------------------------------------
    setup_espeak_data_dir();
    // Plan 4.F: the engine itself lives in `nagent-tts`; we project
    // the server's `TtsConfig` onto its plain `TtsSettings` so the
    // crate stays decoupled from our config plumbing.
    let tts = TtsEngine::load(&nagent_tts::TtsSettings::from(&cfg.tts))
        .await
        .map_err(|e| anyhow::anyhow!("TTS init failed: {e}"))?
        .map(|engine| TtsState {
            engine: Arc::new(engine),
        });

    // ---- Per-user credentials resolver ----------------------------------
    //
    // Required when `auth.enabled` AND at least one agent is
    // registered — the boot fails loudly otherwise so a
    // misconfigured deployment does not silently lose the ability
    // to decrypt per-user credentials. The encryption key comes
    // from `[auth.credentials].key` in the resolved config
    // (TOML-only, no env-var indirection).
    //
    // Plan 1790963194218: the `caldav` `ServiceDef` is appended
    // to the registry when the `caldav-agent` cargo feature is
    // on. The chat agents that consume the per-user vault
    // (`caldav_list_events` / `caldav_get_event` /
    // `caldav_create_event`) are feature-gated the same way; a
    // build without the feature has no CalDAV surface and the
    // registry stays empty (the historical default).
    #[cfg(feature = "caldav-agent")]
    let services =
        ServiceRegistry::new(&[nagent_agents::caldav_service::CALDAV_SERVICE]).into_arc();
    #[cfg(not(feature = "caldav-agent"))]
    let services = ServiceRegistry::empty().into_arc();
    let has_credentials = auth_store.is_some() && agents.as_ref().is_some_and(|a| !a.is_empty());
    let (credential_resolver, credentials_key) = if has_credentials {
        let key = match CredentialsKey::from_hex(&cfg.auth.credentials.key) {
            Ok(k) => Arc::new(k),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "[auth.credentials].key is required when auth.enabled and \
                     agents.enabled (per-user credentials framework): {e}"
                ));
            }
        };
        let resolver = CredentialResolver::new(
            auth_store.clone().expect("auth_store checked above"),
            key.clone(),
            None,
            None,
        );
        info!("per-user credentials framework enabled");
        (Some(Arc::new(resolver)), Some(key))
    } else {
        info!("per-user credentials framework disabled (auth.enabled or agents.enabled is off)");
        (None, None)
    };

    // ---- Compose AuthState ----------------------------------------------
    let auth = auth_store.map(|store: nagent_db::Db| {
        let hash_concurrency = cfg.auth.password.hash_concurrency.max(1);
        AuthState {
            store,
            cfg: Arc::new(cfg.auth.clone()),
            oidc: auth_oidc,
            passkey: auth_passkey,
            login_rate_limiter: LoginRateLimiter::new(),
            services: services.clone(),
            credential_resolver,
            credentials_key,
            password_semaphore: Arc::new(tokio::sync::Semaphore::new(hash_concurrency)),
        }
    });

    // ---- SttState -------------------------------------------------------
    let stt = SttState {
        backend,
        sessions: Arc::clone(&sessions),
        job_tx,
        ready,
        rate_limiter: stt_rate_limiter,
        ws_concurrency: crate::stt::ws_concurrency::WsConcurrency::new(
            cfg.limits.ws_max_concurrent,
            cfg.limits.ws_max_per_ip,
        ),
    };

    Ok(Arc::new(AppState {
        stt,
        llm,
        agents,
        auth,
        documents: documents_state,
        chat_sessions: chat_sessions_state,
        tts,
        config: cfg,
    }))
}

/// Build the backend: real `whisper-rs` when the `real-backend`
/// feature is enabled, otherwise the in-process mock used by CI /
/// tests / dev.
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

/// Locate the directory containing the bundled `espeak-ng-data/`
/// phoneme + voice tables and expose it via the
/// `PIPER_ESPEAKNG_DATA_DIRECTORY` env var that `espeak-rs` reads
/// at init time. See the original `main.rs` module for the full
/// rationale; this is the same logic, factored out so `build_app`
/// can call it before constructing any `Piper`.
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
                candidates.sort_by(|a, b| b.0.cmp(&a.0));
                if let Some((_, path)) = candidates.into_iter().next() {
                    std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &path);
                    return;
                }
            }
        }
    }

    for parent in ["/usr/share", "/usr/local/share"] {
        if let Some(p) = find_data(std::path::Path::new(parent)) {
            std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", &p);
            return;
        }
    }
}

// Silences the unused-import lint when the build skips `tts` and
// `llm` (every binary that compiles `app.rs` has at least one of
// them; the all-off build is for `cargo check` only).
#[allow(dead_code)]
fn _unused_imports(_: &llm::LlmClient, _: &tts::TtsEngine) {}
