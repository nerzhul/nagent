//! LLM-side wrapper around the pure [`nagent_shortcut::ShortcutClient`].
//!
//! The wire client (authenticated GET, egress SSRF pre-flight,
//! cache, URL / redaction helpers) lives in the `nagent-shortcut`
//! crate so `nagent-server`'s pure-API dashboard endpoints can
//! share it without depending on the agent machinery. This module
//! only adds the two things the chat tools need on top:
//!
//! - [`ShortcutClient::token`] — resolve the per-user API token
//!   from the vault via `UserContext::secret`;
//! - [`ShortcutClient::get`] — the `ctx`-flavoured GET the ten
//!   `shortcut_*` tools call (resolve token → delegate);
//! - `From<ShortcutError> for AgentError` — map the wire client's
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
//! `Shortcut-Token` header. No error path formats it — see
//! `nagent_shortcut::redact_shortcut_token` for the one safe
//! header-rendering helper.

use secrecy::SecretString;
use serde_json::Value;

use crate::agents::{AgentError, UserContext};
use crate::ShortcutAgentConfig;

// Re-exported so `super::client::truncate` / `build_url` /
// `redact_shortcut_token` keep resolving at their old paths.
pub use nagent_shortcut::{build_url, redact_shortcut_token, truncate};

/// Wrapper around [`nagent_shortcut::ShortcutClient`] that
/// resolves the API token from the [`UserContext`].
pub struct ShortcutClient {
    inner: nagent_shortcut::ShortcutClient,
}

impl std::ops::Deref for ShortcutClient {
    type Target = nagent_shortcut::ShortcutClient;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::fmt::Debug for ShortcutClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.inner, f)
    }
}

impl ShortcutClient {
    pub fn new(cfg: ShortcutAgentConfig, http: reqwest::Client) -> Self {
        Self {
            inner: nagent_shortcut::ShortcutClient::new(cfg, http),
        }
    }

    /// Resolve the per-user API token. Missing field surfaces as
    /// `CredentialsMissing` so the chat UI prompts the user to
    /// connect Shortcut.
    pub async fn token(&self, ctx: &UserContext) -> Result<SecretString, AgentError> {
        ctx.secret("shortcut", "api_token")
            .await?
            .ok_or_else(|| AgentError::CredentialsMissing {
                service: "shortcut".to_string(),
                field: "api_token".to_string(),
            })
    }

    /// Authenticated GET with the token resolved from `ctx`.
    /// This shadows the inner client's token-taking `get` — the
    /// tool implementations keep their `client.get(ctx, …)` shape.
    pub async fn get(
        &self,
        ctx: &UserContext,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, AgentError> {
        let token = self.token(ctx).await?;
        self.inner
            .get(&token, path, query)
            .await
            .map_err(AgentError::from)
    }

    /// Authenticated POST with a JSON body, token resolved from
    /// `ctx` (write operations: `shortcut_create_story`). The body
    /// is never logged or formatted into an error path (AGENTS.md
    /// §6).
    pub async fn post(
        &self,
        ctx: &UserContext,
        path: &str,
        body: &Value,
    ) -> Result<Value, AgentError> {
        let token = self.token(ctx).await?;
        self.inner
            .post(&token, path, body)
            .await
            .map_err(AgentError::from)
    }
}

impl From<nagent_shortcut::ShortcutError> for AgentError {
    fn from(e: nagent_shortcut::ShortcutError) -> Self {
        use nagent_shortcut::ShortcutError as E;
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
