//! `state/` — application state grouping.
//!
//! Phase 5.B of the architecture refactor moves the flat
//! `AppState` god-struct out of `lib.rs` and groups its fields into
//! per-subsystem sub-states (`SttState`, `LlmState`, `AuthState`,
//! `DocumentsState`, `ChatSessionsState`, `TtsState`). The goal
//! is twofold:
//!
//! 1. **Adding a field touches one sub-state, not `AppState`.** A
//!    new STT knob lives on `SttState`, a new auth knob on
//!    `AuthState`, etc.; the test builder in
//!    [`crate::testing`] (feature `test-util`) does not have to
//!    learn about any of them.
//! 2. **Handlers can extract only what they need.** Handlers
//!    currently still take `State<Arc<AppState>>` (the
//!    per-handler migration is incremental — phase 5.H); the
//!    sub-states exist as types so the migration is a sequence
//!    of small, mechanical changes.
//!
//! ## Composition
//!
//! ```text
//! AppState
//! ├── stt:          SttState       (always Some)
//! ├── llm:          Option<LlmState>
//! ├── agents:       Option<AgentRegistry>
//! ├── auth:         Option<AuthState>    ← replaces auth::middleware::AuthState
//! ├── documents:    Option<DocumentsState>
//! ├── chat_sessions:Option<ChatSessionsState>
//! ├── tts:          Option<TtsState>
//! └── config:       Arc<Config>
//! ```
//!
//! `AuthState` is the union of every auth-related sub-field the
//! previous flat `AppState` carried (`auth_store`, `auth_oidc`,
//! `auth_passkey`, `auth_rate_limiter`, `credential_resolver`,
//! `credentials_key`) plus the per-user `ServiceRegistry` and the
//! resolved `Arc<Config>` that the middleware needs. The legacy
//! `auth::middleware::AuthState` struct was deleted; `AuthState`
//! is the single source of truth for the auth subtree's state.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use stt_core::{PoolDispatch, WhisperBackend};

use crate::agents::ServiceRegistry;
use crate::auth::login_rate_limit::LoginRateLimiter;
use crate::auth::{AuthStore, OidcState, PasskeyState};
use crate::config::Config;
use crate::credentials::{CredentialResolver, CredentialsKey};
use crate::rate_limit::RateLimiter;
use crate::stt::session::SessionMap;
use crate::{agents, llm, tts};

/// STT pipeline sub-state. Always present (the WebSocket
/// `/ws` route is registered unconditionally).
#[derive(Clone)]
pub struct SttState {
    pub backend: Arc<dyn WhisperBackend>,
    pub sessions: SessionMap,
    pub job_tx: PoolDispatch,
    pub ready: Arc<AtomicBool>,
    pub rate_limiter: RateLimiter,
}

impl std::fmt::Debug for SttState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SttState")
            .field("backend", &"<dyn WhisperBackend>")
            .field("sessions", &self.sessions)
            .field("job_tx", &self.job_tx)
            .field("ready", &self.ready)
            .field("rate_limiter", &self.rate_limiter)
            .finish()
    }
}

/// LLM proxy sub-state. `Some` when `LLM_ENABLED=true`.
#[derive(Clone)]
pub struct LlmState {
    pub client: llm::LlmClient,
    pub rate_limiter: RateLimiter,
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
/// AND `auto_bootstrap` produced an `AuthStore`.
///
/// This struct replaces the previous `auth::middleware::AuthState`
/// (which carried only `store`, `cfg`, `oidc`, `passkey`,
/// `rate_limiter`) plus the scattered `auth_store`,
/// `auth_oidc`, `auth_passkey`, `auth_rate_limiter`,
/// `credential_resolver`, `credentials_key`, and `services`
/// fields of the old flat `AppState`. Every auth handler and
/// the auth middleware now read from `AppState.auth`.
#[derive(Clone)]
pub struct AuthState {
    pub store: AuthStore,
    pub cfg: Arc<Config>,
    pub oidc: Option<OidcState>,
    pub passkey: Option<PasskeyState>,
    pub login_rate_limiter: LoginRateLimiter,
    pub services: Arc<ServiceRegistry>,
    pub credential_resolver: Option<Arc<CredentialResolver>>,
    pub credentials_key: Option<Arc<CredentialsKey>>,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("store", &self.store)
            .field("cfg", &self.cfg)
            .field("oidc", &self.oidc.as_ref().map(|_| "<OidcState>"))
            .field("passkey", &self.passkey.as_ref().map(|_| "<PasskeyState>"))
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
    pub fn new(store: AuthStore, cfg: Arc<Config>) -> Self {
        Self {
            store,
            cfg,
            oidc: None,
            passkey: None,
            login_rate_limiter: LoginRateLimiter::new(),
            services: ServiceRegistry::empty().into_arc(),
            credential_resolver: None,
            credentials_key: None,
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
