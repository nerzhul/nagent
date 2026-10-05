//! `state/` — per-subsystem application state.
//!
//! `AppState` groups its fields into sub-states (`SttState`,
//! `LlmState`, `AuthState`, `DocumentsState`, `ChatSessionsState`,
//! `TtsState`) and exposes an axum [`FromRef<Arc<AppState>>`] impl
//! for each one. No handler outside `http/` and `app.rs` names
//! `AppState` — each handler extracts only the sub-state it reads.
//!
//! `AuthState` carries the narrowed [`AuthConfig`] it needs
//! (cookie / session / password / passkey knobs) instead of the
//! whole resolved `Config`. The shared [`AppState`] still owns
//! the full `Arc<Config>` for the `http/` composition root and
//! the per-IP rate-limit resolver, both of which legitimately
//! span subsystems.
//!
//! ## Composition
//!
//! ```text
//! AppState
//! ├── stt:          SttState       (always Some)
//! ├── llm:          Option<LlmState>
//! ├── agents:       Option<AgentRegistry>
//! ├── auth:         Option<AuthState>
//! ├── documents:    Option<DocumentsState>
//! ├── chat_sessions:Option<ChatSessionsState>
//! ├── tts:          Option<TtsState>
//! └── config:       Arc<Config>
//! ```

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::FromRef;
use stt_core::{PoolDispatch, WhisperBackend};
use tokio::sync::Semaphore;

use crate::auth::login_rate_limit::LoginRateLimiter;
use crate::auth::{OidcState, PasskeyState};
use crate::config::{AuthConfig, Config, LlmConfig};
use crate::credentials::{CredentialResolver, CredentialsKey};
use crate::rate_limit::RateLimiter;
use crate::stt::session::SessionMap;
use crate::stt::ws_concurrency::WsConcurrency;
use crate::{agents, llm, tts};
use nagent_agents::ServiceRegistry;

#[cfg(feature = "x-agent")]
use crate::oauth::x::XOAuthState;

/// STT pipeline sub-state. Always present (the WebSocket
/// `/ws` route is registered unconditionally).
#[derive(Clone)]
pub struct SttState {
    pub backend: Arc<dyn WhisperBackend>,
    pub sessions: SessionMap,
    pub job_tx: PoolDispatch,
    pub ready: Arc<AtomicBool>,
    pub rate_limiter: RateLimiter,
    /// Global + per-IP WebSocket concurrency counter (plan S-1).
    /// Lives next to `sessions` because the two are decremented
    /// together when the per-connection task exits.
    pub ws_concurrency: WsConcurrency,
}

impl std::fmt::Debug for SttState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttState")
            .field("backend", &"<dyn WhisperBackend>")
            .field("sessions", &self.sessions)
            .field("job_tx", &self.job_tx)
            .field("ready", &self.ready)
            .field("rate_limiter", &self.rate_limiter)
            .field("ws_concurrency", &self.ws_concurrency)
            .finish()
    }
}

/// LLM proxy sub-state. `Some` when `LLM_ENABLED=true`.
#[derive(Clone)]
pub struct LlmState {
    pub client: llm::LlmClient,
    pub rate_limiter: RateLimiter,
    /// Owning clone of the [`LlmConfig`] the proxy was wired
    /// from. Carrying the config on the state (rather than just
    /// `Arc<Config>`) keeps the proxy's signature narrow: the
    /// per-tool-loop knobs (`llm_max_tool_rounds`,
    /// `llm_max_auto_continues`) and the per-response generation
    /// cap (`num_predict`) live on the LLM subtree, not on
    /// `AgentConfig`, and the proxy needs both the runtime state
    /// (client + rate limiter) and the config (knobs).
    pub cfg: LlmConfig,
}

impl std::fmt::Debug for LlmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmState")
            .field("client", &"<LlmClient>")
            .field("rate_limiter", &self.rate_limiter)
            .finish()
    }
}

/// Auth subsystem sub-state. `Some` when `auth.enabled = true`
/// AND `auto_bootstrap` produced a `nagent_db::Db` connection.
///
/// `cfg` is `Arc<AuthConfig>` (not the whole `Config`) so the
/// auth subtree cannot reach into unrelated sections (LLM,
/// agents, documents, …).
#[derive(Clone)]
pub struct AuthState {
    pub store: nagent_db::Db,
    pub cfg: Arc<AuthConfig>,
    pub oidc: Option<OidcState>,
    pub passkey: Option<PasskeyState>,
    #[cfg(feature = "x-agent")]
    pub x: Option<Arc<XOAuthState>>,
    pub login_rate_limiter: LoginRateLimiter,
    pub services: Arc<ServiceRegistry>,
    pub credential_resolver: Option<Arc<CredentialResolver>>,
    pub credentials_key: Option<Arc<CredentialsKey>>,
    /// Semaphore gating concurrent argon2 hash/verify calls on
    /// the blocking pool (plan R1a). Sized by
    /// `cfg.password.hash_concurrency` at boot; clone once and
    /// pass to the password helpers via `Arc::clone`.
    pub password_semaphore: Arc<Semaphore>,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("store", &self.store)
            .field("cfg", &self.cfg)
            .field("oidc", &self.oidc.as_ref().map(|_| "<OidcState>"))
            .field("passkey", &self.passkey.as_ref().map(|_| "<PasskeyState>"))
            .field(
                "x",
                #[cfg(feature = "x-agent")]
                &self.x.as_ref().map(|_| "<XOAuthState>"),
                #[cfg(not(feature = "x-agent"))]
                &"<x-agent off>",
            )
            .field("login_rate_limiter", &self.login_rate_limiter)
            .field("services", &self.services)
            .field(
                "credential_resolver",
                &self.credential_resolver.as_ref().map(|_| "<resolver>"),
            )
            .field(
                "credentials_key",
                &self.credentials_key.as_ref().map(|_| "<key>"),
            )
            .finish()
    }
}

impl AuthState {
    /// Build the minimum viable auth state from a store and a
    /// config. `oidc`/`passkey`/credentials are off — the
    /// `app::build_app` boot path adds them when the operator
    /// opts in. Used by tests that only care about the
    /// middleware/auth-store interaction.
    pub fn new(store: nagent_db::Db, cfg: Arc<Config>) -> Self {
        let hash_concurrency = cfg.auth.password.hash_concurrency.max(1);
        Self {
            store,
            cfg: Arc::new(cfg.auth.clone()),
            oidc: None,
            passkey: None,
            #[cfg(feature = "x-agent")]
            x: None,
            login_rate_limiter: LoginRateLimiter::new(),
            services: ServiceRegistry::empty().into_arc(),
            credential_resolver: None,
            credentials_key: None,
            password_semaphore: Arc::new(Semaphore::new(hash_concurrency)),
        }
    }

    /// Helper for the credentials routes: clone the encryption
    /// key. Panics if the resolver is `Some` but the key is
    /// missing (a programmer error in `app::build_app`).
    pub fn credential_encryption_key(&self) -> Arc<CredentialsKey> {
        self.credentials_key
            .clone()
            .expect("credentials_key must match credential_resolver")
    }
}

/// Documents subsystem state. `Some` when
/// `[documents].enabled = true` AND the auth DB is reachable
/// (so the `uploaded_documents` table exists).
#[derive(Clone, Debug)]
pub struct DocumentsState {
    pub store: crate::documents::DocumentStore,
}

/// Server-bound chat-session id binding sub-state (SEV 2 fix).
/// `Some` when `auth.enabled = true`.
#[derive(Clone, Debug)]
pub struct ChatSessionsState {
    pub sessions: crate::chat::sessions::ChatSessions,
}

/// TTS sub-state. `Some` when `TTS_ENABLED=true` AND the
/// `stt-server/tts` cargo feature is on.
#[derive(Clone)]
pub struct TtsState {
    pub engine: Arc<tts::TtsEngine>,
}

impl std::fmt::Debug for TtsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtsState")
            .field("engine", &"<TtsEngine>")
            .finish()
    }
}

/// Top-level shared application state injected into every axum
/// handler.
///
/// Built once per process by [`crate::app::build_app`] and
/// shared via `Arc<AppState>`. Every optional subsystem is
/// `None` when the corresponding runtime feature is off; the
/// `http::build_router` composition root reads those flags to
/// decide which routes to mount.
#[derive(Clone)]
pub struct AppState {
    pub stt: SttState,
    pub llm: Option<LlmState>,
    pub agents: Option<agents::AgentRegistry>,
    pub auth: Option<AuthState>,
    pub documents: Option<DocumentsState>,
    pub chat_sessions: Option<ChatSessionsState>,
    pub tts: Option<TtsState>,
    pub config: Arc<Config>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("stt", &self.stt)
            .field("llm", &self.llm.as_ref().map(|_| "<LlmState>"))
            .field("agents", &self.agents.as_ref().map(|_| "<AgentRegistry>"))
            .field("auth", &self.auth.as_ref().map(|_| "<AuthState>"))
            .field(
                "documents",
                &self.documents.as_ref().map(|_| "<DocumentsState>"),
            )
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessionsState>"),
            )
            .field("tts", &self.tts.as_ref().map(|_| "<TtsState>"))
            .field("config", &self.config)
            .finish()
    }
}

impl AppState {
    /// Convenience helper for the credentials routes: clone the
    /// encryption key off the auth sub-state. Returns `None`
    /// when auth is disabled (callers are mounted only when
    /// the resolver is `Some`, so they must never see `None`).
    pub fn credential_encryption_key(&self) -> Arc<CredentialsKey> {
        self.auth
            .as_ref()
            .expect("auth must be enabled for credential_encryption_key")
            .credential_encryption_key()
    }
}

// axum `FromRef` impls — no handler outside `http/` and `app.rs` names
// `AppState`; each handler extracts its sub-state through `FromRef`.
//
// Rust's orphan rule forbids `impl FromRef<Arc<AppState>> for Arc<T>` for
// local `T` (the self type `Arc<T>` is foreign), so the few
// `Arc<LocalType>` extractors used by handlers
// (`Arc<Config>`, `Arc<LlmState>`, `Arc<AgentRegistry>`,
// `Arc<ServiceRegistry>`) live behind local newtype wrappers.
// The local-only sub-states (`SttState`, `LlmState`, `AuthState`,
// `DocumentsState`, `ChatSessionsState`, `TtsState`, `AgentRegistry`)
// have a direct `FromRef` impl because they are themselves local types.

impl FromRef<Arc<AppState>> for SttState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        state.stt.clone()
    }
}

impl FromRef<Arc<AppState>> for LlmState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        // The LLM subtree is mounted only when `llm.is_some()`; the
        // handler that takes `State<LlmState>` therefore panics if
        // the route is reached without the LLM being wired (a
        // programmer error — the router never registers the route
        // in that case).
        state
            .llm
            .clone()
            .expect("LLM proxy handler reached without an LlmState on AppState")
    }
}

impl FromRef<Arc<AppState>> for AuthState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        // Same `Some(_)` precondition as `LlmState` — the
        // auth-protected subtree is mounted only when auth is
        // enabled.
        state
            .auth
            .clone()
            .expect("auth handler reached without an AuthState on AppState")
    }
}

impl FromRef<Arc<AppState>> for DocumentsState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        state
            .documents
            .clone()
            .expect("documents handler reached without a DocumentsState on AppState")
    }
}

impl FromRef<Arc<AppState>> for ChatSessionsState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        state
            .chat_sessions
            .clone()
            .expect("chat-session handler reached without a ChatSessionsState on AppState")
    }
}

impl FromRef<Arc<AppState>> for TtsState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        state
            .tts
            .clone()
            .expect("TTS handler reached without a TtsState on AppState")
    }
}

impl FromRef<Arc<AppState>> for agents::AgentRegistryNewtype {
    fn from_ref(state: &Arc<AppState>) -> Self {
        agents::AgentRegistryNewtype(
            state
                .agents
                .clone()
                .expect("agents handler reached without an AgentRegistry on AppState"),
        )
    }
}

// ----- Newtype wrappers for the Arc<LocalType> extractors ------------------
//
// These exist solely to satisfy Rust's orphan rule: `impl FromRef<…> for
// Arc<LocalType>` is rejected because both `FromRef` (foreign) and `Arc`
// (foreign) wrap a local type. Wrapping the Arc in a local newtype turns
// the self type into a local type and the impl is accepted.
//
// Handlers that need the inner value should call `state.0.clone()` or
// `&state.0` rather than going through `Arc::clone` directly — both
// helpers below implement `Deref` for ergonomics.

/// Combined STT + Config extractor for the `/ws` upgrade handler.
/// Bundles the two sub-states the WS task needs so the handler
/// signature has a single `State<…>` extractor (axum 0.7 infers
/// the router's state type from the first `State<T>` it sees).
pub struct SttWithConfig {
    pub stt: Arc<SttState>,
    pub config: Arc<Config>,
}

impl Clone for SttWithConfig {
    fn clone(&self) -> Self {
        Self {
            stt: Arc::clone(&self.stt),
            config: Arc::clone(&self.config),
        }
    }
}

impl std::fmt::Debug for SttWithConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttWithConfig")
            .field("stt", &self.stt)
            .field("config", &self.config)
            .finish()
    }
}

impl FromRef<Arc<AppState>> for SttWithConfig {
    fn from_ref(state: &Arc<AppState>) -> Self {
        Self {
            stt: Arc::new(state.stt.clone()),
            config: Arc::clone(&state.config),
        }
    }
}

/// `Arc<LlmState>` wrapper. The LLM proxy handler clones the underlying
/// state once to hold it in the spawned tool-loop task.
pub struct ArcLlmState(pub Arc<LlmState>);

impl Clone for ArcLlmState {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for ArcLlmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcLlmState")
            .field("state", &"<LlmState>")
            .finish()
    }
}

impl std::ops::Deref for ArcLlmState {
    type Target = LlmState;
    fn deref(&self) -> &LlmState {
        &self.0
    }
}

impl FromRef<Arc<AppState>> for ArcLlmState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        ArcLlmState(Arc::new(state.llm.clone().expect(
            "LLM proxy handler reached without an LlmState on AppState",
        )))
    }
}

/// `Arc<AgentRegistry>` wrapper. The agent HTTP routes keep the registry
/// behind an Arc so the catalogue (`Arc<ServiceRegistry>` shared with the
/// auth subtree) does not get duplicated on every call.
pub struct ArcAgentRegistry(pub Arc<agents::AgentRegistry>);

impl Clone for ArcAgentRegistry {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl std::fmt::Debug for ArcAgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcAgentRegistry")
            .field("registry", &"<AgentRegistry>")
            .finish()
    }
}

impl std::ops::Deref for ArcAgentRegistry {
    type Target = agents::AgentRegistry;
    fn deref(&self) -> &agents::AgentRegistry {
        &self.0
    }
}

impl FromRef<Arc<AppState>> for ArcAgentRegistry {
    fn from_ref(state: &Arc<AppState>) -> Self {
        ArcAgentRegistry(Arc::new(
            state
                .agents
                .clone()
                .expect("agents handler reached without an AgentRegistry on AppState"),
        ))
    }
}

/// `Option<Arc<AgentRegistry>>` — the chat-completions handler threads
/// this through the tool loop. `None` is the expected value when agents
/// are disabled (the proxy still works, the tool loop is a pass-through,
/// and the `tools` array is empty).
pub struct OptArcAgentRegistry(pub Option<Arc<agents::AgentRegistry>>);

impl Clone for OptArcAgentRegistry {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for OptArcAgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptArcAgentRegistry")
            .field("registry", &self.0.as_ref().map(|_| "<AgentRegistry>"))
            .finish()
    }
}

impl FromRef<Arc<AppState>> for OptArcAgentRegistry {
    fn from_ref(state: &Arc<AppState>) -> Self {
        OptArcAgentRegistry(state.agents.clone().map(Arc::new))
    }
}

/// `Option<Arc<AuthState>>` — the LLM proxy handler threads this
/// through so it can look up the authenticated user's per-row
/// preferences (today: reply language) when `auth.enabled = true`.
/// `None` is the expected value when auth is disabled (the
/// anonymous trust boundary) so the proxy must check before
/// dereferencing.
pub struct OptArcAuthState(pub Option<Arc<AuthState>>);

impl Clone for OptArcAuthState {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for OptArcAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptArcAuthState")
            .field("state", &self.0.as_ref().map(|_| "<AuthState>"))
            .finish()
    }
}

impl FromRef<Arc<AppState>> for OptArcAuthState {
    fn from_ref(state: &Arc<AppState>) -> Self {
        OptArcAuthState(state.auth.clone().map(Arc::new))
    }
}

/// `Arc<ServiceRegistry>` wrapper. The agents HTTP routes build a
/// `UserContext` with this catalog so per-user agents (currently
/// `read_document`) can resolve configured-only integrations.
pub struct ArcServices(pub Arc<nagent_agents::ServiceRegistry>);

impl Clone for ArcServices {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl std::fmt::Debug for ArcServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcServices")
            .field("services", &"<ServiceRegistry>")
            .finish()
    }
}

impl std::ops::Deref for ArcServices {
    type Target = nagent_agents::ServiceRegistry;
    fn deref(&self) -> &nagent_agents::ServiceRegistry {
        &self.0
    }
}

impl FromRef<Arc<AppState>> for ArcServices {
    fn from_ref(state: &Arc<AppState>) -> Self {
        // The agents subtree is mounted only when `state.agents`
        // is `Some`, and `state.auth.services` is populated
        // alongside the agents boot. When the operator runs with
        // `agents.enabled = false` but `auth.enabled = true` we
        // fall back to the empty registry so handlers that need
        // the catalog still type-check; the catalog itself is
        // empty in that case.
        let services = state
            .auth
            .as_ref()
            .map(|a| a.services.clone())
            .unwrap_or_else(|| nagent_agents::ServiceRegistry::empty().into_arc());
        ArcServices(services)
    }
}

/// `Arc<AgentsConfig>` wrapper (the `[agents]` section of `Config`).
/// Always present — the section is unconditional.
pub struct ArcAgentsConfig(pub Arc<crate::config::AgentConfig>);

impl Clone for ArcAgentsConfig {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl std::fmt::Debug for ArcAgentsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcAgentsConfig")
            .field("config", &"<AgentConfig>")
            .finish()
    }
}

impl std::ops::Deref for ArcAgentsConfig {
    type Target = crate::config::AgentConfig;
    fn deref(&self) -> &crate::config::AgentConfig {
        &self.0
    }
}

impl FromRef<Arc<AppState>> for ArcAgentsConfig {
    fn from_ref(state: &Arc<AppState>) -> Self {
        ArcAgentsConfig(Arc::new(state.config.agents.clone()))
    }
}
