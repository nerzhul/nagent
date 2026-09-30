//! Server-side proxy to an OpenAI-compatible chat server (typically Ollama).
//!
//! The proxy exists for two reasons:
//! 1. Hide the upstream base URL / API key behind the same origin as the
//!    static frontend, so the browser never sees them.
//! 2. Keep the wire protocol identical to OpenAI's `/v1/chat/completions`
//!    so any other OpenAI-compatible client (curl, the OpenAI Python
//!    SDK, etc.) can also point at `stt-server` once `LLM_ENABLED=true`.
//!
//! SSE is forwarded verbatim: we do not parse or re-encode chunks.
//! Re-encoding would risk breaking clients that depend on the exact
//! framing, and there is nothing the server gains by inspecting the
//! tokens.
//!
//! Phase 1 of the architecture refactor split the file into:
//!
//! - [`client`] — `LlmClient` + the shared `LlmError` type + the
//!   chat-session header parser.
//! - [`privacy`] — admin kill-switches for the browser-injected
//!   location and timezone blocks.
//! - [`prompt`] — server-default system prompt and the
//!   user-integration block injected into every chat completion.
//! - [`sse`] — SSE frame parser + tool-loop event builders.
//! - [`tool_loop`] — the per-chat-completion tool loop, including
//!   the security-plan-#10 "read_document → web_fetch" rule.
//! - [`proxy`] — the top-level HTTP handlers (`chat_completions`,
//!   `models_list`, `agents_list`, `agent_invoke`).

pub mod client;
pub mod privacy;
pub mod prompt;
pub mod proxy;
pub mod sse;
pub mod tool_loop;

// Back-compat re-exports — the pre-split callers reached these items
// through `crate::llm::*`; keep that working until the follow-up
// commit rewrites the call sites in place.
pub use client::{parse_chat_session_header, LlmClient, LlmError, CHAT_SESSION_HEADER};
pub use privacy::{strip_user_location_if_disabled, strip_user_timezone_if_disabled};
pub use proxy::{agent_invoke, agents_list, chat_completions, models_list};
