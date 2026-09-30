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
//!   [`crate::agents::routes`].
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
//! - [`state`] — the per-subsystem sub-state groups (`SttState`,
//!   `LlmState`, `AuthState`, `DocumentsState`, `ChatSessionsState`,
//!   `TtsState`) and the top-level [`state::AppState`] that composes
//!   them. Built once per process by [`crate::app::build_app`].
//! - [`stt`] — WebSocket STT pipeline (per-connection upgrade,
//!   session map, the `ResultRouter`, the watchdog that drops idle
//!   sessions).
//! - [`testing`] — integration-test builder for `AppState`,
//!   feature-gated behind `test-util` so production binaries do
//!   not pay the cost.
//! - [`tts`] — local Piper text-to-speech engine (split into
//!   [`tts::engine`](crate::tts::engine) + [`tts::routes`](crate::tts::routes)).
//!
//! ## Application boot
//!
//! [`app::build_app`] does the entire boot wiring (auth store
//! bootstrap, OIDC/passkey state, backend + worker pool, LLM
//! client, agent registry, TTS engine, documents store, per-IP
//! rate limiters) from a single `&Config` and returns an
//! `Arc<AppState>`. `main.rs` calls it after CLI parsing and
//! feeds the result to [`http::build_router`].

#![warn(missing_debug_implementations)]

pub mod agents;
pub mod app;
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
pub mod state;
pub mod stt;
#[cfg(feature = "test-util")]
pub mod testing;
pub mod tts;
pub mod version;

/// Convenience alias for the per-agent section of `Config`.
pub type AgentsConfig = config::AgentConfig;

pub use config::{CliArgs, Config};
pub use state::{
    AppState, AuthState, ChatSessionsState, DocumentsState, LlmState, SttState, TtsState,
};
pub use version::VersionInfo;
