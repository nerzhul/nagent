//! Integration-test builder for [`crate::AppState`].
//!
//! Single builder replaces hand-built `AppState { ... }` literals
//! across `crates/stt-server/tests/*.rs`. New fields on any
//! sub-state are picked up automatically; tests that need to set
//! them explicitly go through the [`AppStateBuilder`] setters.
//!
//! ## Usage
//!
//! ```ignore
//! use nagent_server::testing::app_state;
//!
//! let state = app_state().build();
//! let state = app_state().with_llm(LlmClient::new(...)?).build();
//! ```
//!
//! The default builder produces an `AppState` suitable for tests
//! that exercise the routing / feature-discovery / static-assets
//! paths:
//!
//! - mock whisper backend (`MockBackend`);
//! - ephemeral session map;
//! - default `Config` (`Config::default()`);
//! - no auth (`auth = None`);
//! - no LLM, agents, TTS, documents, chat sessions;
//! - permissive per-IP rate limiters (the policy default).
//!
//! Tests that need to enable a subsystem call the matching
//! `with_*` setter before `.build()`.
//!
//! ## Feature gate
//!
//! The module is only compiled when the `test-util` cargo feature
//! is on. In the default build (`cargo build` without
//! `--features test-util`) the module is absent so production
//! binaries do not pay the cost of the helper imports. The
//! feature is added to the dev profile through `cargo test` (and
//! through every integration test that depends on it).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use stt_core::{MockBackend, PoolDispatch, WhisperBackend};
use tokio::sync::mpsc;

use crate::agents::{AgentRegistry, ServiceRegistry};
use crate::auth::login_rate_limit::LoginRateLimiter;
use crate::config::{
    AgentConfig, AuthConfig, Config, DocumentsConfig, LimitsConfig, LlmAuthMode, LlmConfig,
    RateLimitConfig, TrustedProxiesConfig, TtsConfig,
};
use crate::credentials::{CredentialResolver, CredentialsKey};
use crate::llm::LlmClient;
use crate::rate_limit::{RateLimitPolicy, RateLimiter};
use crate::state::{
    AppState, AuthState, ChatSessionsState, DocumentsState, LlmState, SttState, TtsState,
};
use crate::stt::session::SessionMap;
use crate::tts::TtsEngine;

/// Builder for an integration-test [`AppState`].
///
/// Construction is intentionally cheap — the builder holds an
/// `Arc<Config>` (cheap to clone) and a few `Option`s for the
/// subsystems that need to be opted in. `build()` allocates the
/// worker pool internals (a `DashMap`, an `mpsc` channel) but no
/// real backend is initialised: tests that only inspect the
/// router shape never pay for whisper-rs.
///
/// All fields are `pub` so the existing tests that need to
/// poke at the inner `Config` (e.g. `auth_gating.rs`) can keep
/// doing so without going through a setter.
//
// `Debug` cannot be derived (the `Option<Arc<dyn WhisperBackend>>`
// carries an opaque backend); the lib's `missing_debug_implementations`
// lint allows per-field `Debug` overrides via a manual impl below.
pub struct AppStateBuilder {
    /// Configuration to share with the `AppState`. Mutated by
    /// the `with_*` setters that need to flip a runtime flag
    /// (e.g. `with_auth_enabled`).
    pub config: Arc<Config>,
    /// Mock backend to clone into `state.stt.backend`. Replace
    /// with a custom `WhisperBackend` via [`Self::with_backend`].
    pub backend: Option<Arc<dyn WhisperBackend>>,
    /// Pre-built `LlmClient`. `None` by default; set via
    /// [`Self::with_llm`].
    pub llm: Option<LlmClient>,
    /// Pre-built `AgentRegistry`. `None` by default; set via
    /// [`Self::with_agents`].
    pub agents: Option<AgentRegistry>,
    /// Pre-built `AuthStore` (sqlite / postgres). `None` by
    /// default; set via [`Self::with_auth`].
    pub auth_store: Option<nagent_db::Db>,
    /// OIDC sub-state to attach to `state.auth.oidc`. `None` by
    /// default; tests that exercise OIDC routes build their own.
    pub auth_oidc: Option<crate::auth::OidcState>,
    /// Passkey sub-state to attach to `state.auth.passkey`.
    /// `None` by default; tests that exercise passkey routes
    /// build their own.
    pub auth_passkey: Option<crate::auth::PasskeyState>,
    /// Per-user credential resolver to attach to
    /// `state.auth.credential_resolver`. `None` by default.
    pub credential_resolver: Option<Arc<CredentialResolver>>,
    /// Per-user credential encryption key. `None` by default;
    /// when set together with `credential_resolver`, the
    /// resulting `AuthState` is consistent.
    pub credentials_key: Option<Arc<CredentialsKey>>,
    /// Per-user integrations registry. Defaults to
    /// `ServiceRegistry::empty()`.
    pub services: Option<Arc<ServiceRegistry>>,
    /// Document store. `None` by default; set via
    /// [`Self::with_documents`].
    pub documents: Option<crate::documents::DocumentStore>,
    /// Chat session binding handle. `None` by default.
    pub chat_sessions: Option<crate::chat::sessions::ChatSessions>,
    /// TTS engine. `None` by default; set via [`Self::with_tts`].
    pub tts: Option<Arc<TtsEngine>>,
    /// Custom STT rate limiter policy (per-minute cap). Defaults
    /// to the config default. Tests that exercise the rate
    /// limiter (e.g. `rate_limit.rs`) override via
    /// [`Self::with_stt_rate_per_min`].
    pub stt_rate_per_min: Option<u32>,
    /// Custom LLM rate limiter policy (per-minute cap). Defaults
    /// to the config default. Tests that exercise the rate
    /// limiter override via [`Self::with_llm_rate_per_min`].
    pub llm_rate_per_min: Option<u32>,
}

impl std::fmt::Debug for AppStateBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppStateBuilder")
            .field("config", &self.config)
            .field("backend", &self.backend.as_ref().map(|_| "<backend>"))
            .field("llm", &self.llm.as_ref().map(|_| "<LlmClient>"))
            .field("agents", &self.agents.as_ref().map(|_| "<AgentRegistry>"))
            .field(
                "auth_store",
                &self.auth_store.as_ref().map(|_| "<AuthStore>"),
            )
            .field("auth_oidc", &self.auth_oidc.as_ref().map(|_| "<OidcState>"))
            .field(
                "auth_passkey",
                &self.auth_passkey.as_ref().map(|_| "<PasskeyState>"),
            )
            .field(
                "credential_resolver",
                &self.credential_resolver.as_ref().map(|_| "<resolver>"),
            )
            .field(
                "credentials_key",
                &self.credentials_key.as_ref().map(|_| "<key>"),
            )
            .field("services", &self.services)
            .field(
                "documents",
                &self.documents.as_ref().map(|_| "<DocumentStore>"),
            )
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessions>"),
            )
            .field("tts", &self.tts.as_ref().map(|_| "<TtsEngine>"))
            .field("stt_rate_per_min", &self.stt_rate_per_min)
            .field("llm_rate_per_min", &self.llm_rate_per_min)
            .finish()
    }
}

impl AppStateBuilder {
    /// Default config: in-memory backend, default rate limits,
    /// auth/credentials/documents/tts off. Suitable for any test
    /// that exercises the router shape, the static frontend, the
    /// feature-discovery endpoint, or the WS upgrade.
    pub fn new() -> Self {
        Self {
            config: Arc::new(default_test_config()),
            backend: None,
            llm: None,
            agents: None,
            auth_store: None,
            auth_oidc: None,
            auth_passkey: None,
            credential_resolver: None,
            credentials_key: None,
            services: None,
            documents: None,
            chat_sessions: None,
            tts: None,
            stt_rate_per_min: None,
            llm_rate_per_min: None,
        }
    }

    /// Convenience: identical to [`Self::new`] — kept for the
    /// historical call sites that reach for it. The default
    /// config is already tuned for integration tests.
    pub fn for_tests() -> Self {
        Self::new()
    }

    /// Use a custom backend (real or mock).
    pub fn with_backend(mut self, backend: Arc<dyn WhisperBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Wire the LLM proxy.
    pub fn with_llm(mut self, client: LlmClient) -> Self {
        self.llm = Some(client);
        self
    }

    /// Wire the agent registry.
    pub fn with_agents(mut self, registry: AgentRegistry) -> Self {
        self.agents = Some(registry);
        self
    }

    /// Wire an auth store. OIDC/passkey sub-states are attached
    /// separately via [`Self::with_oidc`] / [`Self::with_passkey`].
    pub fn with_auth(mut self, store: nagent_db::Db) -> Self {
        self.auth_store = Some(store);
        self
    }

    pub fn with_oidc(mut self, oidc: crate::auth::OidcState) -> Self {
        self.auth_oidc = Some(oidc);
        self
    }

    pub fn with_passkey(mut self, passkey: crate::auth::PasskeyState) -> Self {
        self.auth_passkey = Some(passkey);
        self
    }

    /// Wire the credentials resolver + key.
    pub fn with_credential_resolver(
        mut self,
        resolver: Arc<CredentialResolver>,
        key: Arc<CredentialsKey>,
    ) -> Self {
        self.credential_resolver = Some(resolver);
        self.credentials_key = Some(key);
        self
    }

    /// Override the per-user integrations registry.
    pub fn with_services(mut self, services: Arc<ServiceRegistry>) -> Self {
        self.services = Some(services);
        self
    }

    /// Wire the document store.
    pub fn with_documents(mut self, store: crate::documents::DocumentStore) -> Self {
        self.documents = Some(store);
        self
    }

    /// Wire the chat-sessions binding handle.
    pub fn with_chat_sessions(mut self, sessions: crate::chat::sessions::ChatSessions) -> Self {
        self.chat_sessions = Some(sessions);
        self
    }

    /// Wire the TTS engine.
    pub fn with_tts(mut self, engine: Arc<TtsEngine>) -> Self {
        self.tts = Some(engine);
        self
    }

    /// Override the per-minute STT rate-limit cap.
    pub fn with_stt_rate_per_min(mut self, per_min: u32) -> Self {
        self.stt_rate_per_min = Some(per_min);
        self
    }

    /// Override the per-minute LLM rate-limit cap.
    pub fn with_llm_rate_per_min(mut self, per_min: u32) -> Self {
        self.llm_rate_per_min = Some(per_min);
        self
    }

    /// Build the [`AppState`]. Consumes the builder; call the
    /// `with_*` setters first.
    pub fn build(self) -> Arc<AppState> {
        let backend = self
            .backend
            .unwrap_or_else(|| Arc::new(MockBackend::new("test-model")));

        let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
        let (job_tx_inner, job_rx) = mpsc::channel::<stt_core::InferenceJob>(16);
        let job_tx = PoolDispatch::from_single_sender(job_tx_inner);
        // Spawn a worker so any `InferenceJob` the test sends through
        // `job_tx` lands on a real consumer (otherwise the channel
        // hits its cap and `try_send` panics). The worker simply
        // drives the backend's no-op `enqueue` for the in-process
        // mock.
        let worker_backend = Arc::clone(&backend);
        let _worker = stt_core::InferenceWorker::spawn(worker_backend, job_rx);
        // Mirror the production wiring — spawn the result router so
        // the in-process pipeline is end-to-end. Tests that dial
        // `/ws` need the router consuming `InferResponse`s; without
        // it the channel fills and blocks the worker.
        let (_resp_tx, resp_rx) = mpsc::channel::<stt_core::InferResponse>(16);
        let _result_router =
            crate::stt::result_router::ResultRouter::spawn(Arc::clone(&sessions), resp_rx);

        let stt_rate = self
            .stt_rate_per_min
            .unwrap_or_else(|| self.config.rate_limit.stt_per_min);
        let llm_rate = self
            .llm_rate_per_min
            .unwrap_or_else(|| self.config.rate_limit.llm_per_min);
        let stt_limiter = RateLimiter::new(RateLimitPolicy::stt(stt_rate));
        let llm_limiter = RateLimiter::new(RateLimitPolicy::llm(llm_rate));

        let llm = self.llm.map(|client| LlmState {
            client,
            rate_limiter: llm_limiter.clone(),
        });

        let auth = self.auth_store.map(|store| {
            let hash_concurrency = self.config.auth.password.hash_concurrency.max(1);
            AuthState {
                store,
                cfg: Arc::new(self.config.auth.clone()),
                oidc: self.auth_oidc,
                passkey: self.auth_passkey,
                login_rate_limiter: LoginRateLimiter::new(),
                services: self
                    .services
                    .unwrap_or_else(|| ServiceRegistry::empty().into_arc()),
                credential_resolver: self.credential_resolver,
                credentials_key: self.credentials_key,
                password_semaphore: Arc::new(tokio::sync::Semaphore::new(hash_concurrency)),
            }
        });

        let documents = self.documents.map(|store| DocumentsState { store });
        let chat_sessions = self
            .chat_sessions
            .map(|sessions| ChatSessionsState { sessions });
        let tts = self.tts.map(|engine| TtsState { engine });

        let stt = SttState {
            backend,
            sessions,
            job_tx,
            ready: Arc::new(AtomicBool::new(true)),
            rate_limiter: stt_limiter,
        };

        Arc::new(AppState {
            stt,
            llm,
            agents: self.agents,
            auth,
            documents,
            chat_sessions,
            tts,
            config: self.config,
        })
    }
}

impl Default for AppStateBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level convenience: returns a builder configured with the
/// integration-test defaults (`for_tests`). Equivalent to
/// `AppStateBuilder::for_tests()` but shorter at the call site.
pub fn app_state() -> AppStateBuilder {
    AppStateBuilder::for_tests()
}

/// `Config` shape used by every integration test that does not
/// care about any specific subsystem:
/// - `bind_addr = 127.0.0.1:0` (ephemeral port for tests that
/// actually bind),
/// - `whisper_model_path = /tmp/fake-model.bin` (so the no-TOML
/// config path does not trip on the required model-path knob),
/// - everything else defaulted.
fn default_test_config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        inference_workers: None,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        trusted_proxies: TrustedProxiesConfig::default(),
        llm: LlmConfig {
            enabled: false,
            base_url: "http://localhost:11434".into(),
            default_model: "llama3.1".into(),
            api_key: None,
            inbound_auth_key: None,
            auth_mode: LlmAuthMode::default(),
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
            system_prompt: None,
            allow_user_location: true,
            allow_user_timezone: true,
        },
        agents: AgentConfig::default(),
        tts: TtsConfig::default(),
        auth: AuthConfig::default(),
        documents: DocumentsConfig::default(),
    }
}
