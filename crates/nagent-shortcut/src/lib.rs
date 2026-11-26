//! `nagent-shortcut` — pure Shortcut REST API v3 client
//! (`https://api.app.shortcut.com/api/v3`).
//!
//! This crate is the single place that knows how to talk to the
//! Shortcut API. It is deliberately free of any LLM / agent
//! machinery: the API **token is always a parameter**, never
//! resolved from a `UserContext`, so both consumers can share it:
//!
//! - `nagent-agents` (the `shortcut_*` chat tools) resolves the
//!   per-user token from the vault via `UserContext::secret` and
//!   hands it to [`ShortcutClient::get`];
//! - `nagent-server` (the pure-API dashboard endpoints
//!   `GET /api/integrations/shortcut/stories…`) reads the token
//!   from the vault itself and hands it to the same client — the
//!   story list / detail never goes through the LLM.
//!
//! Layout:
//!
//! - [`ShortcutConfig`] — plain config struct (timeouts, caps,
//!   allow-list, base URL, backlog query). `nagent-agents`
//!   re-exports it as `ShortcutAgentConfig` so the server's config
//!   layer keeps its existing `From` impl.
//! - [`ShortcutClient`] — one authenticated GET against the API,
//!   plus the short-lived in-process response cache.
//! - [`project`] — JSON projections from raw upstream payloads to
//!   the shapes consumed by the LLM tools and by the dashboard UI.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The token only ever travels inside the `Shortcut-Token` header.
//! No code path formats the header (or the token) into an error
//! string, a log line, or a `Debug` output —
//! [`redact_shortcut_token`] is the single helper that renders a
//! header map safely. The upstream response body is safe to
//! surface (truncated); the request that carried the credential
//! is not.

pub mod client;
pub mod project;

pub use client::{
    build_url, get_story_detail, list_stories, redact_shortcut_token, truncate, ShortcutClient,
    MAX_RAW_RESPONSE_BYTES,
};
pub use project::{
    find_member, label_names, label_objects, mention_name_of, name_of_member, project_comment,
    project_story, project_story_detail,
};

/// Errors surfaced by [`ShortcutClient`]. `nagent-agents` maps
/// these onto `AgentError`; `nagent-server` maps them onto HTTP
/// responses. Neither mapping ever includes the token.
#[derive(Debug, thiserror::Error)]
pub enum ShortcutError {
    /// Transport / connect / TLS / read failure. The reqwest
    /// error `Display` carries the URL but never the request
    /// headers, so the token cannot leak through this variant.
    #[error("shortcut transport: {0}")]
    Transport(String),
    /// Upstream returned a non-2xx status other than 401 / 429.
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },
    /// Upstream returned 401 — the token was revoked or is wrong.
    /// The message carries the reconnect hint, never the token.
    #[error("{0}")]
    Unauthorized(String),
    /// Upstream returned 429 (Shortcut quota is 200 req/min per
    /// token). `hint` carries the `Retry-After` value when present.
    #[error("rate limited by upstream: {hint}")]
    RateLimited { hint: String },
    /// The response body exceeded the raw-body budget.
    #[error("response exceeded max_bytes={budget}")]
    ResponseExceeded { budget: usize },
    /// The egress SSRF pre-flight / hostname allow-list rejected
    /// the URL.
    #[error("sandbox denied: {0}")]
    SandboxDenied(String),
    /// The URL / arguments did not parse.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
}

/// Knobs for the Shortcut client. Plain data; the TOML / env
/// parsing stays in `nagent-server`'s config layer, which converts
/// into this struct via the `From` impl in `nagent-server`.
///
/// The per-user API token is NOT part of this config — it lives in
/// the per-user vault under the `shortcut` service id and is
/// passed explicitly to every [`ShortcutClient::get`] call.
#[derive(Debug, Clone)]
pub struct ShortcutConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// Hard cap on the `limit` argument of every list call.
    pub max_items: usize,
    /// Cap on story/epic descriptions and comment bodies the LLM
    /// ever sees (bytes). Longer values are truncated with a
    /// marker. The dashboard endpoints do not apply this cap.
    pub max_body_bytes: usize,
    /// In-process response cache TTL, keyed by
    /// `(user_id, tool, args)`. `0` disables the cache.
    /// Shortcut's quota is 200 req/min per token, so a short TTL
    /// keeps repeated questions cheap.
    pub cache_ttl_secs: u64,
    /// Hostname allow-list applied by the egress client to every
    /// Shortcut API URL. Default `["api.app.shortcut.com"]`.
    pub allowlist: Vec<String>,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture. Defaults to
    /// `https://api.app.shortcut.com/api/v3`.
    pub base_url: String,
    /// Default Shortcut search query used by `shortcut_list_backlog`
    /// and by the dashboard story list: stories in the backlog
    /// (unstarted workflow state, not in an iteration — the
    /// `is:backlog` operator), not archived.
    pub backlog_query: String,
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            max_items: 200,
            max_body_bytes: 65_536,
            cache_ttl_secs: 60,
            allowlist: vec!["api.app.shortcut.com".to_string()],
            base_url: "https://api.app.shortcut.com/api/v3".to_string(),
            backlog_query: "is:story is:backlog !is:archived".to_string(),
        }
    }
}
