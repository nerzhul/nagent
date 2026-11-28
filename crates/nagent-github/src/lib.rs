//! `nagent-github` — pure GitHub REST API client
//! (`https://api.github.com`, GHES via `base_url` override).
//!
//! This crate is the single place that knows how to talk to the
//! GitHub API. It is deliberately free of any LLM / agent
//! machinery: the API **personal access token is always a
//! parameter**, never resolved from a `UserContext`, so both
//! consumers can share it:
//!
//! - `nagent-agents` (the `github_*` chat tools) resolves the
//!   per-user token from the vault via `UserContext::secret` and
//!   hands it to [`GithubClient::get`];
//! - `nagent-server` (the pure-API dashboard endpoints
//!   `GET /api/integrations/github/prs…` and the setup-only
//!   `POST /api/integrations/github/probe-repo`) reads the token
//!   from the vault itself and hands it to the same client — the
//!   PR dashboard never goes through the LLM.
//!
//! Layout:
//!
//! - [`GithubConfig`] — plain config struct (timeouts, caps,
//!   allow-list, base URL). `nagent-agents` re-exports it as
//!   `GithubAgentConfig` so the server's config layer keeps its
//!   existing `From` impl.
//! - [`GithubClient`] — one authenticated GET against the API,
//!   plus the short-lived in-process response cache.
//! - [`project`] — JSON projections from raw upstream payloads to
//!   the shapes consumed by the LLM tools and by the dashboard UI.
//! - The free functions at the crate root
//!   ([`get_repo`], [`list_prs_for_repo`], [`get_pr_detail`],
//!   [`list_dashboard_prs`]) are the dashboard / probe reads; they
//!   share the client's response cache.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The token only ever travels inside the `Authorization: Bearer`
//! header. No code path formats the header (or the token) into an
//! error string, a log line, or a `Debug` output —
//! [`redact_authorization`] is the single helper that renders a
//! header map safely. The upstream response body is safe to
//! surface (truncated); the request that carried the credential
//! is not.

pub mod accounts;
pub mod client;
pub mod project;

pub use accounts::{
    account_meta, merge_owner_repos, merge_owners, merge_tagged, parse_accounts,
    serialize_accounts, valid_account_name, AccountDef, ALL_ACCOUNTS, DEFAULT_ACCOUNT_NAME,
};
pub use client::{
    build_url, ci_status, get_pr_detail, get_repo, list_dashboard_prs, list_dashboard_prs_multi,
    list_owner_repos, list_owners, list_prs_for_repo, redact_authorization, resolve_repo_account,
    truncate, valid_owner_name, valid_repo_full_name, AccountToken, DashboardPrsQuery,
    GithubClient, MultiDashboardPrsQuery, RepoPrsQuery, GITHUB_API_VERSION, MAX_DASHBOARD_REPOS,
    MAX_RAW_RESPONSE_BYTES,
};
pub use project::{project_pr_detail, project_pr_row, project_repo_summary};

/// Errors surfaced by [`GithubClient`]. `nagent-agents` maps
/// these onto `AgentError`; `nagent-server` maps them onto HTTP
/// responses. Neither mapping ever includes the token.
#[derive(Debug, thiserror::Error)]
pub enum GithubError {
    /// Transport / connect / TLS / read failure. The reqwest
    /// error `Display` carries the URL but never the request
    /// headers, so the token cannot leak through this variant.
    #[error("github transport: {0}")]
    Transport(String),
    /// Upstream returned a non-2xx status other than 401 /
    /// rate-limit. `body` is the (truncated) upstream reply —
    /// safe to surface per AGENTS.md §6.
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },
    /// Upstream returned 401 — the PAT was revoked or is wrong.
    /// The message carries the reconnect hint, never the token.
    #[error("{0}")]
    Unauthorized(String),
    /// Upstream returned 403 / 429 with
    /// `X-RateLimit-Remaining: 0` — the primary rate limit is
    /// exhausted. `hint` carries the `Retry-After` /
    /// `X-RateLimit-Reset` value when present.
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

/// Knobs for the GitHub client. Plain data; the TOML / env
/// parsing stays in `nagent-server`'s config layer, which converts
/// into this struct via the `From` impl in `nagent-server`.
///
/// The per-user personal access token is NOT part of this config —
/// it lives in the per-user vault under the `github` service id
/// and is passed explicitly to every [`GithubClient::get`] call.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    /// Per-request connect+read timeout in milliseconds.
    pub timeout_ms: u64,
    /// Hard cap on the `limit` argument of every list call.
    pub max_items: usize,
    /// Cap on issue / PR / file bodies the LLM ever sees (bytes).
    /// Longer values are truncated with a marker. The dashboard
    /// endpoints do not apply this cap to the list rows.
    pub max_body_bytes: usize,
    /// In-process response cache TTL, keyed by
    /// `(user_id, tool, args)`. `0` disables the cache. GitHub
    /// PAT quota is 5 000 req/h, so a short TTL keeps repeated
    /// "what's open?" questions cheap.
    pub cache_ttl_secs: u64,
    /// Hostname allow-list applied by the egress client to every
    /// GitHub API URL. Default `["api.github.com"]`. A GitHub
    /// Enterprise Server deployment overrides `base_url` AND this
    /// list with its own host.
    pub allowlist: Vec<String>,
    /// Override the upstream base URL — useful for integration
    /// tests against a loopback fixture and for GHES. Defaults to
    /// `https://api.github.com`.
    pub base_url: String,
    /// Cap on the number of named accounts stored in the per-user
    /// `github.accounts` vault field (the `"default"` account backed
    /// by `personal_access_token` is not counted). Default 8.
    pub max_accounts: usize,
    /// Bound on the concurrent fan-out of an `account: "all"`
    /// aggregation (chat tools + dashboard). Default 4.
    pub aggregate_concurrency: usize,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            max_items: 100,
            max_body_bytes: 65_536,
            cache_ttl_secs: 60,
            allowlist: vec!["api.github.com".to_string()],
            base_url: "https://api.github.com".to_string(),
            max_accounts: 8,
            aggregate_concurrency: 4,
        }
    }
}
