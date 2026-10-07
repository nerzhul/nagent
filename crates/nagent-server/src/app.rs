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
use crate::llm::discovered_tools::DiscoveredTools;
use crate::llm::LlmClient;
use crate::state::{
    AppState, AuthState, ChatSessionsState, DocumentsState, LlmState, SttState, TtsState,
};
use crate::stt::{result_router, session, watchdog};
use crate::tts::TtsEngine;
use crate::{agents, llm, tts};
use nagent_agents::{ServiceRegistry, ToolsRouter};

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
                cfg.documents.max_pages_per_call,
                cfg.documents.max_page_chars_per_call,
                cfg.documents.cache_dir.clone(),
                cfg.documents.pdf_extract_concurrency,
                cfg.documents.pdf_extract_timeout_secs,
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
    let mut llm = if cfg.llm.enabled {
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
            // Plan 1791267136806 §7.6: filled in by the post-boot
            // patch below (the credentials key + DB clone are not
            // in scope yet at this point — they live behind the
            // `has_credentials` gate a few lines below).
            memory_source: None,
            // Plan 1791317253718: filled in after the agent
            // registry is built (the router needs the registry's
            // agents to index). `None` until that patch runs.
            tools_router: None,
        })
    } else {
        info!("LLM proxy disabled (set LLM_ENABLED=true to enable)");
        None
    };
    // STT rate limiter (the LLM one was already used above if the
    // proxy is enabled, or discarded otherwise).
    let (stt_rate_limiter, _) = build_rate_limiters(&cfg);

    // ---- Per-user credentials resolver ----------------------------------
    //
    // Required when `auth.enabled` AND at least one agent is
    // registered — the boot fails loudly otherwise so a
    // misconfigured deployment does not silently lose the ability
    // to decrypt per-user credentials. The encryption key comes
    // from `[auth.credentials].key` in the resolved config
    // (TOML-only, no env-var indirection).
    //
    // Plan 1791384190579: the same key is also wired into the
    // `StoreDocumentSource` so the per-page encrypted store can
    // be decrypted at read time. The key is only resolved when
    // `auth.enabled` and a non-empty agent registry is in scope;
    // a slim build with documents-only auth receives `None`
    // here — the upload route then refuses PDFs and the read path
    // falls back to the legacy on-the-fly extraction.
    let has_credentials = auth_store.is_some() && documents_state.is_some();
    let (credentials_key, credentials_key_init_log) = if has_credentials {
        match CredentialsKey::from_hex(&cfg.auth.credentials.key) {
            Ok(k) => (Some(Arc::new(k)), "per-user credentials framework enabled"),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "[auth.credentials].key is required when auth.enabled and \
                     documents.enabled (per-page encrypted store): {e}"
                ));
            }
        }
    } else {
        (
            None,
            "per-user credentials framework disabled (auth.enabled or documents.enabled is off)",
        )
    };
    info!("{credentials_key_init_log}");

    // ---- Agent registry ---------------------------------------------------
    let agents = agents::build_registry(
        &cfg.agents,
        documents_state.as_ref().map(|d| d.store.clone()),
        chat_sessions_state.as_ref().map(|c| c.sessions.clone()),
        credentials_key.clone(),
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

    // ---- ServiceRegistry ------------------------------------------------
    //
    // Build the static `ServiceRegistry` from the per-feature
    // service catalog. Each per-feature entry is gated on the
    // matching cargo feature so a build without the feature has
    // no surface to render.
    #[cfg(all(feature = "caldav-agent", not(feature = "x-agent")))]
    let services =
        ServiceRegistry::new(&[nagent_agents::caldav_service::CALDAV_SERVICE]).into_arc();
    #[cfg(all(not(feature = "caldav-agent"), feature = "x-agent"))]
    let services =
        ServiceRegistry::new(&[nagent_agents::x_account_service::X_ACCOUNT_SERVICE]).into_arc();
    #[cfg(all(feature = "caldav-agent", feature = "x-agent"))]
    let services = ServiceRegistry::new(&[
        nagent_agents::caldav_service::CALDAV_SERVICE,
        nagent_agents::x_account_service::X_ACCOUNT_SERVICE,
    ])
    .into_arc();
    #[cfg(not(any(feature = "caldav-agent", feature = "x-agent")))]
    let services = ServiceRegistry::empty().into_arc();

    // Plan 1790695073418: build the X OAuth state when the cargo
    // feature is on AND the operator supplied a `client_id` (the
    // master switch is `x_oauth.enabled`, defaulted to `false`).
    // The encryption key is the same `[auth.credentials].key` the
    // per-user vault uses, so X OAuth tokens and CalDAV-style
    // credentials share the same audit row shape.
    #[cfg(feature = "x-agent")]
    let auth_x_opt = match (auth_store.clone(), credentials_key.clone()) {
        (Some(store), Some(key)) => crate::oauth::x::build_state(
            Arc::new(crate::oauth::x::XOAuthConfig {
                enabled: cfg.x_oauth.enabled,
                client_id: cfg.x_oauth.client_id.clone(),
                client_secret: cfg.x_oauth.client_secret.clone(),
                redirect_path: cfg.x_oauth.redirect_path.clone(),
                scopes: cfg.x_oauth.scopes.clone(),
                timeout_ms: cfg.x_oauth.timeout_ms,
            }),
            store,
            key,
            cfg.auth.public_url.clone(),
        ),
        _ => None,
    };
    #[cfg(feature = "x-agent")]
    if auth_x_opt.is_some() {
        tracing::info!("X OAuth backend ready");
    }

    // Build the per-user credential resolver against the same
    // encryption key. The resolver is mounted even when no
    // per-user agent (CalDAV / X) is enabled because the
    // `/api/integrations*` HTTP routes consult it directly.
    let credential_resolver = match (auth_store.clone(), credentials_key.clone()) {
        (Some(store), Some(key)) => {
            let r = CredentialResolver::new(store, key, None, None);
            Some(Arc::new(r))
        }
        _ => None,
    };

    // ---- Compose AuthState ----------------------------------------------
    // Capture clones BEFORE the move so the LLM memory builder
    // (which runs after this closure) can share the key + DB.
    let auth_credentials_key_for_memory = credentials_key.clone();
    let auth_store_for_memory = auth_store.clone();
    let auth = auth_store.clone().map(|store: nagent_db::Db| {
        let hash_concurrency = cfg.auth.password.hash_concurrency.max(1);
        AuthState {
            store,
            cfg: Arc::new(cfg.auth.clone()),
            oidc: auth_oidc,
            passkey: auth_passkey,
            #[cfg(feature = "x-agent")]
            x: auth_x_opt.map(Arc::new),
            login_rate_limiter: LoginRateLimiter::new(),
            services: services.clone(),
            credential_resolver,
            credentials_key,
            password_semaphore: Arc::new(tokio::sync::Semaphore::new(hash_concurrency)),
        }
    });

    // ---- Patch LlmState with the per-user memory source ----------------
    //
    // Plan 1791267136806 §7.6: the source is built AFTER
    // `credentials_key` so it can share the encryption key with
    // the per-user vault. The `disable_key` sentinel leaves the
    // source as `None` so the LLM proxy threads a None
    // `MemorySource` into every chat-session `UserContext` —
    // the four `memory_*` agents then fail closed with a clean
    // error instead of attempting to encrypt.
    if let Some(llm_ref) = llm.as_mut() {
        llm_ref.memory_source =
            build_memory_source(&cfg, auth_credentials_key_for_memory, auth_store_for_memory);
        // Plan 1791317253718: build the BM25 router from the
        // final registry (the agents we just built, including
        // any server-side extras like `read_document`) and
        // hand it to every agent that wants it (today:
        // `search_tools`). Wired in one place so future
        // contributors cannot accidentally skip the step.
        if let Some(registry) = agents.as_ref() {
            let router = Arc::new(ToolsRouter::from_registry(registry));
            registry.wire_router(router.clone());
            llm_ref.tools_router = Some(router);
            info!(
                indexed = registry.len(),
                "tools_router (BM25) built and wired into the agent registry"
            );
        }
    }

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
        permission_store: crate::llm::permission::PermissionStore::new(),
        discovered_tools: DiscoveredTools::new(),
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

/// Build the per-user long-term memory source (plan
/// 1791267136806, §7.6). `Some(_)` when:
/// - `[auth.credentials].key` was successfully parsed (so we can
///   encrypt / decrypt the value / notes columns), AND
/// - `LLM_ALLOW_USER_MEMORY` is not explicitly `false`
///   (`LLM_ALLOW_USER_MEMORY=false` is the operator kill switch,
///   plan §1.5; default is `true`), AND
/// - the auth DB connection succeeded (the `memories` table lives
///   on the auth DB, so without one there is nothing to read /
///   write from).
///
/// Returning `None` is the safe degraded mode: the proxy threads
/// a `None` into every chat-session `UserContext`, and the four
/// `memory_*` agents surface a clean
/// `AgentError::AgentFailed("memory: source not wired in this
/// context")` instead of attempting to encrypt.
fn build_memory_source(
    cfg: &Config,
    credentials_key: Option<Arc<crate::credentials::CredentialsKey>>,
    db: Option<nagent_db::Db>,
) -> Option<Arc<dyn nagent_agents::agents::MemorySource>> {
    if !cfg.llm.allow_user_memory {
        info!("memory subsystem killed by LLM_ALLOW_USER_MEMORY=false");
        return None;
    }
    let (Some(key), Some(db)) = (credentials_key, db) else {
        info!("memory subsystem disabled (no credentials key or DB)");
        return None;
    };
    let source = crate::memories::adapter::UserDbMemorySource::new(db, key.clone());
    Some(Arc::new(source))
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
