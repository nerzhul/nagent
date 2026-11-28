//! LLM-side wrapper around the pure [`nagent_github::GithubClient`].
//!
//! The wire client (authenticated GET, egress SSRF pre-flight,
//! cache, URL / redaction helpers, dashboard reads) lives in the
//! `nagent-github` crate so `nagent-server`'s pure-API dashboard
//! endpoints and the setup-only repo probe can share it without
//! depending on the agent machinery. This module only adds the
//! two things the chat tools need on top:
//!
//! - [`GithubClient::resolve_account`] — resolve a named account
//!   (or the default) from the vault via `UserContext::secret`
//!   (plan 1795793328177 §1.2);
//! - [`GithubClient::get`] — the `ctx`-flavoured GET the ten
//!   `github_*` tools call (resolve the default account →
//!   delegate); [`GithubClient::get_as`] takes an explicit
//!   resolved account;
//! - `From<GithubError> for AgentError` — map the wire client's
//!   error type onto the agent error type.
//!
//! The wrapper `Deref`s to the inner client, so field accesses
//! like `client.cfg` and method calls like `client.cache_get(…)`
//! in the tool implementations keep working unchanged.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The token is resolved into a `SecretString` and only ever
//! handed to the inner client, which puts it in the
//! `Authorization` header. No error path formats it — see
//! `nagent_github::redact_authorization` for the one safe
//! header-rendering helper.

use secrecy::SecretString;
use serde_json::Value;

use crate::agents::{AgentError, UserContext};
use crate::GithubAgentConfig;

// Re-exported so `super::client::truncate` / `build_url` /
// `redact_authorization` keep resolving at their old paths.
pub use nagent_github::{
    build_url, redact_authorization, truncate, GITHUB_API_VERSION, MAX_RAW_RESPONSE_BYTES,
};

/// Wrapper around [`nagent_github::GithubClient`] that resolves
/// the PAT from the [`UserContext`]. `Clone` shares the inner
/// client (cache + reqwest pool) — needed by the `account: "all"`
/// fan-out, which clones one handle per spawned task.
#[derive(Clone)]
pub struct GithubClient {
    inner: nagent_github::GithubClient,
}

impl std::ops::Deref for GithubClient {
    type Target = nagent_github::GithubClient;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl GithubClient {
    pub fn new(cfg: GithubAgentConfig, http: reqwest::Client) -> Self {
        Self {
            inner: nagent_github::GithubClient::new(cfg, http),
        }
    }

    /// Resolve the per-user personal access token as the
    /// `"default"` account. Missing field surfaces as
    /// `CredentialsMissing` so the chat UI prompts the user to
    /// connect GitHub.
    pub async fn token(&self, ctx: &UserContext) -> Result<SecretString, AgentError> {
        ctx.secret("github", "personal_access_token")
            .await?
            .ok_or_else(|| AgentError::CredentialsMissing {
                service: "github".to_string(),
                field: "personal_access_token".to_string(),
            })
    }

    /// Resolve one account (plan 1795793328177 §1.2): explicit
    /// `account` arg → `github.default_account` field → `"default"`.
    pub async fn resolve_account(
        &self,
        ctx: &UserContext,
        account_arg: Option<&str>,
    ) -> Result<super::accounts::Account, AgentError> {
        super::accounts::resolve_account(ctx, &self.cfg, account_arg).await
    }

    /// Every configured account, default first (for the
    /// `account: "all"` fan-out).
    pub async fn all_accounts(
        &self,
        ctx: &UserContext,
    ) -> Result<Vec<super::accounts::Account>, AgentError> {
        super::accounts::all_accounts(ctx, &self.cfg).await
    }

    /// Authenticated GET with the default account resolved from
    /// `ctx`. This shadows the inner client's token-taking `get` —
    /// the tool implementations keep their `client.get(ctx, …)`
    /// shape.
    pub async fn get(
        &self,
        ctx: &UserContext,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, AgentError> {
        let account = self.resolve_account(ctx, None).await?;
        self.get_with_account(&account, path, query).await
    }

    /// Authenticated GET with an explicitly resolved account. Takes
    /// no `ctx` so fan-out tasks can call it from spawned tasks. The
    /// account name only ever tags cache entries and error
    /// messages — never the token.
    pub async fn get_with_account(
        &self,
        account: &super::accounts::Account,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, AgentError> {
        self.inner
            .get(&account.token, path, query)
            .await
            .map_err(AgentError::from)
    }
}

impl From<nagent_github::GithubError> for AgentError {
    fn from(e: nagent_github::GithubError) -> Self {
        use nagent_github::GithubError as E;
        match e {
            E::Transport(m) => AgentError::AgentFailed(m),
            E::Upstream { status, body } => AgentError::Upstream { status, body },
            E::Unauthorized(m) => AgentError::AgentFailed(m),
            E::RateLimited { hint } => AgentError::RateLimited { hint },
            E::ResponseExceeded { budget } => AgentError::ResponseExceeded { budget },
            E::SandboxDenied(m) => AgentError::SandboxDenied(m),
            E::InvalidArguments(m) => AgentError::InvalidArguments(m),
        }
    }
}
