//! `stt-server` — axum HTTP/WebSocket server for the `nagent`
//! chat-with-agents stack (STT + LLM proxy + chat agents + TTS +
//! per-user documents + auth).
//!
//! ## Subsystems
//!
//! - [`config`], [`config_file`] — env-var + TOML parsing, layering.
//! - [`agents`] — server-side chat agents (e.g. `web_fetch`,
//!   `read_document`) callable from the LLM proxy through OpenAI-style
//!   tool/function calling. Direct agent HTTP routes live in
//!   [`crate::llm::proxy`].
//! - [`auth`] — multi-user authentication (password / OIDC / passkey)
//!   plus the cookie + bearer session machinery. Login-attempt
//!   rate-limit lives at [`crate::auth::login_rate_limit`].
//! - [`chat`] — server-bound chat-session bookkeeping
//!   (`(user, session)` SEV 2 binding used by `read_document`).
//! - [`cli`] — operator subcommands (`stt-server {auth,migrate,documents}`).
//! - [`credentials`] — per-user credentials vault (AES-256-GCM at rest,
//!   decrypted on demand through a per-request `UserContext`).
//! - [`documents`] — Discussion-mode document uploads + the
//!   `read_document` LLM tool.
//! - [`http`] — HTTP transport plumbing: security headers, the static
//!   frontend embedding, the `/api/features` discovery endpoint, the
//!   LLM-proxy shared envelope middleware, and the `build_router`
//!   composition root that wires every `/v1/*` and `/api/*` route into
//!   a single axum `Router`.
//! - [`llm`] — optional OpenAI-compatible proxy to a local LLM
//!   (Ollama). Split into
//!   [`llm::client`](crate::llm::client) /
//!   [`llm::proxy`](crate::llm::proxy) /
//!   [`llm::tool_loop`](crate::llm::tool_loop) /
//!   [`llm::sse`](crate::llm::sse) /
//!   [`llm::privacy`](crate::llm::privacy) /
//!   [`llm::prompt`](crate::llm::prompt) submodules.
//! - [`rate_limit`] — per-source-IP token bucket for STT and LLM
//!   traffic (the login-attempt limiter lives at
//!   [`crate::auth::login_rate_limit`]).
//! - [`stt`] — WebSocket STT pipeline (per-connection upgrade,
//!   session map, the `ResultRouter`, the watchdog that drops idle
//!   sessions).
//! - [`tts`] — local Piper text-to-speech engine (split into
//!   [`tts::engine`](crate::tts::engine) + [`tts::routes`](crate::tts::routes)).

#![warn(missing_debug_implementations)]

pub mod agents;
pub mod auth;
pub mod chat;
pub mod cli;
pub mod config;
pub mod config_file;
pub mod credentials;
pub mod documents;
pub mod http;
pub mod llm;
pub mod rate_limit;
pub mod stt;
pub mod tts;
pub mod version;

pub use config::{CliArgs, Config};
pub use version::VersionInfo;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use stt_core::{PoolDispatch, WhisperBackend};

use crate::stt::session::SessionMap;

/// Shared application state injected into every axum handler.
#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn WhisperBackend>,
    pub sessions: SessionMap,
    /// Sticky-dispatch handle into the inference worker pool. Each WS
    /// handler clones it (cheap) and sends its jobs through it; jobs
    /// from the same session always land on the same worker so
    /// per-session FIFO order is preserved.
    pub job_tx: PoolDispatch,
    pub ready: Arc<AtomicBool>,
    pub config: Arc<Config>,
    /// Optional LLM proxy. `None` when `LLM_ENABLED=false`; in that
    /// case the `/v1/*` routes are not registered and the chat view
    /// in the UI 404s gracefully.
    pub llm: Option<llm::LlmClient>,
    /// Optional registry of chat agents. `None` when
    /// `AGENTS_ENABLED=false`; the `/v1/agents*` routes are wired
    /// whenever this is set, regardless of whether the LLM proxy is
    /// enabled (so `curl /v1/agents/web_fetch/invoke` keeps working
    /// for local testing).
    pub agents: Option<agents::AgentRegistry>,
    /// Optional Piper TTS engine. `None` when `TTS_ENABLED=false`; in
    /// that case the `/v1/audio/*` routes are not registered and the
    /// discussion-mode "Read response aloud" UI shows no checkbox.
    /// Always wrapped in an `Arc` because the engine is cloned into
    /// the HTTP handler for each request (it holds per-voice lazy
    /// ONNX session state).
    pub tts: Option<Arc<tts::TtsEngine>>,
    /// Per-source-IP token bucket for the STT pipeline (consumed at
    /// WS upgrade and per inbound WS frame).
    pub stt_rate_limiter: rate_limit::RateLimiter,
    /// Per-source-IP token bucket for the `/v1/*` LLM proxy.
    pub llm_rate_limiter: rate_limit::RateLimiter,
    /// Auth DB handle (PR1). `Some(_)` when `auth.enabled = true`,
    /// `None` otherwise so existing tests that do not care about
    /// auth keep building unmodified. Populated by `auth::boot::
    /// auto_bootstrap` at server start.
    pub auth_store: Option<auth::AuthStore>,
    /// OIDC sub-state — `None` when OIDC is not enabled (or
    /// `auth.enabled = false`).
    pub auth_oidc: Option<auth::oidc::OidcState>,
    /// Passkey sub-state — `None` when passkey is not enabled.
    pub auth_passkey: Option<auth::passkey::PasskeyState>,
    /// Login-attempt rate limiter. Shared between the `RequireAuth`
    /// middleware (unused) and the password login handler.
    pub auth_rate_limiter: auth::login_rate_limit::LoginRateLimiter,
    /// Catalogue of per-user integrations. `Arc`-wrapped because
    /// every per-request `UserContext` clones it; the inner slice is
    /// `&'static` so the registry can be built once at boot.
    pub services: std::sync::Arc<crate::agents::ServiceRegistry>,
    /// Per-request credential resolver. `None` until the auth
    /// subsystem boots AND a server-side encryption key is
    /// available; per-user agents that call `ctx.secret()` will
    /// surface `CredentialsMissing` when the resolver is missing.
    pub credential_resolver: Option<std::sync::Arc<crate::credentials::CredentialResolver>>,
    /// Server-side encryption key for the credentials vault.
    /// `None` when the resolver is `None`. Held separately because
    /// the route handlers need to encrypt on PUT — the resolver
    /// only exposes reads.
    pub credentials_key: Option<std::sync::Arc<crate::credentials::CredentialsKey>>,
    /// Document store handle (`Some` when `cfg.documents.enabled =
    /// true` AND the auth DB is reachable). Mounts the
    /// `/v1/documents*` routes and powers the `read_document`
    /// agent.
    pub documents: Option<crate::documents::DocumentStore>,
    /// Server-bound chat session id binding (SEV 2 fix). The
    /// `POST /v1/chat/session` mint handler reads from this
    /// field; the documents routes / `read_document` agent read
    /// it through `state.app.chat_sessions` to verify the
    /// `(user, session)` binding before any DB lookup. Lives at
    /// the `AppState` level (not under `documents`) because the
    /// concept is broader than documents — a future plan may
    /// scope agent conversations or chat-history entries to a
    /// chat session too.
    pub chat_sessions: Option<crate::chat::sessions::ChatSessions>,
}

impl AppState {
    /// Helper for the credentials routes: clone the encryption
    /// key. Panics if the resolver is set but the key is missing
    /// (a programmer error in `main.rs`).
    pub fn credential_encryption_key(&self) -> std::sync::Arc<crate::credentials::CredentialsKey> {
        self.credentials_key
            .clone()
            .expect("credentials_key must match credential_resolver")
    }
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
            .field("agents", &self.agents.as_ref().map(|_| "<AgentRegistry>"))
            .field("tts", &self.tts.as_ref().map(|_| "<TtsEngine>"))
            .field("stt_rate_limiter", &self.stt_rate_limiter)
            .field("llm_rate_limiter", &self.llm_rate_limiter)
            .field(
                "credential_resolver",
                &self.credential_resolver.as_ref().map(|_| "<resolver>"),
            )
            .field(
                "documents",
                &self.documents.as_ref().map(|_| "<DocumentStore>"),
            )
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessions>"),
            )
            .finish()
    }
}
