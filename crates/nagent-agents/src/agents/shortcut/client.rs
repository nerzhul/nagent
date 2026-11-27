//! LLM-side wrapper around the pure [`nagent_shortcut::ShortcutClient`].
//!
//! The wire client (authenticated GET/POST, egress SSRF
//! pre-flight, cache, URL / redaction helpers) lives in the
//! `nagent-shortcut` crate so `nagent-server`'s pure-API dashboard
//! endpoints can share it without depending on the agent
//! machinery. This module only adds the things the chat tools need
//! on top:
//!
//! - [`ShortcutClient::auth`] — resolve the caller's credential
//!   from the [`UserContext`]: the OAuth `access_token` when the
//!   user connected Shortcut via OAuth (sent as `Authorization:
//!   Bearer …`), else the classic `api_token` (sent as
//!   `Shortcut-Token: …`). When the stored OAuth token is expired
//!   or within [`REFRESH_SKEW_SECS`] of expiry and a
//!   `refresh_token` is saved, the token is refreshed proactively
//!   and written back through the `SecretSink` (the X pattern,
//!   plan 1790695073418);
//! - [`ShortcutClient::get`] / [`post`] — the `ctx`-flavoured
//!   calls every `shortcut_*` tool uses (resolve auth → delegate →
//!   on a 401 with a saved refresh token, refresh once and retry
//!   once, so a token that expired between two chat turns does not
//!   surface as a hard failure);
//! - `From<ShortcutError> for AgentError` — map the wire client's
//!   error type onto the agent error type.
//!
//! The wrapper `Deref`s to the inner client, so field accesses
//! like `client.cfg` and method calls like `client.cache_get(…)`
//! in the tool implementations keep working unchanged.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! Credentials are resolved into `SecretString`s and only ever
//! handed to the inner client, which puts them in the auth header.
//! The refresh call sends the refresh token as a form field; its
//! error paths carry only the upstream status + truncated reply
//! body, never the token. See
//! `nagent_shortcut::redact_auth_headers` for the one safe
//! header-rendering helper.

use chrono::{DateTime, Duration, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::agents::{AgentError, UserContext};
use crate::ShortcutAgentConfig;

use super::optional_secret;

// Re-exported so `super::client::truncate` / `build_url` /
// `redact_auth_headers` keep resolving at their old paths.
pub use nagent_shortcut::{build_url, redact_auth_headers, truncate};

use nagent_shortcut::ShortcutAuth;

/// Default Shortcut OAuth token endpoint (agent-app install
/// flow). Overridable with `SHORTCUT_OAUTH_TOKEN_URL` so tests can
/// point the refresh call at a loopback fixture.
const DEFAULT_OAUTH_TOKEN_URL: &str = "https://app.shortcut.com/oauth/token";

/// Refresh the stored access token when it expires within this
/// many seconds (same 60 s safety margin as the X agent).
const REFRESH_SKEW_SECS: i64 = 60;

/// Decide whether the stored `token_expires_at` (RFC 3339) is past
/// or within [`REFRESH_SKEW_SECS`] of `now`. A missing or
/// unparseable value counts as "unknown": `false`, so a malformed
/// row never triggers a refresh loop — the 401 retry path covers
/// the actually-expired case.
fn token_needs_refresh(expires_at: Option<&str>, now: DateTime<Utc>) -> bool {
    let Some(text) = expires_at.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    match DateTime::parse_from_rfc3339(text) {
        Ok(expiry) => now + Duration::seconds(REFRESH_SKEW_SECS) >= expiry.with_timezone(&Utc),
        // Unparseable expiry: do not guess, let the 401 path decide.
        Err(_) => false,
    }
}

/// One refreshed token set, as returned by the token endpoint's
/// `grant_type=refresh_token` response. The manual `Debug` impl
/// redacts both tokens (AGENTS.md §6).
struct RefreshedToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: DateTime<Utc>,
    scope: Option<String>,
}

impl std::fmt::Debug for RefreshedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshedToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Operator-side OAuth client credentials, read from the process
/// environment — same posture as the X agent's refresh path: the
/// agent only needs to redeem the user's refresh token, not manage
/// the app registration, so the server config is not threaded
/// through the agent. `SHORTCUT_OAUTH_CLIENT_ID` (+ optional
/// `SHORTCUT_OAUTH_CLIENT_SECRET`) are the same vars the server's
/// `/api/auth/login/shortcut/*` handlers read at connect time;
/// `SHORTCUT_OAUTH_TOKEN_URL` overrides the token endpoint (tests).
struct OAuthClientCreds {
    client_id: String,
    client_secret: Option<String>,
    token_url: String,
}

fn oauth_client_creds() -> Option<OAuthClientCreds> {
    let client_id = std::env::var("SHORTCUT_OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())?;
    let client_secret = std::env::var("SHORTCUT_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.is_empty());
    let token_url = std::env::var("SHORTCUT_OAUTH_TOKEN_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_OAUTH_TOKEN_URL.to_string());
    Some(OAuthClientCreds {
        client_id,
        client_secret,
        token_url,
    })
}

/// Wrapper around [`nagent_shortcut::ShortcutClient`] that
/// resolves the caller's credential (OAuth bearer preferred,
/// API token fallback) from the [`UserContext`].
pub struct ShortcutClient {
    inner: nagent_shortcut::ShortcutClient,
    http: reqwest::Client,
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
            inner: nagent_shortcut::ShortcutClient::new(cfg, http.clone()),
            http,
        }
    }

    /// Resolve the caller's credential: the OAuth `access_token`
    /// when saved (refreshed first when stale), else the classic
    /// `api_token`. Missing both surfaces as `CredentialsMissing`
    /// so the chat UI prompts the user to connect Shortcut.
    pub async fn auth(&self, ctx: &UserContext) -> Result<ShortcutAuth, AgentError> {
        let access = optional_secret(ctx.secret("shortcut", "access_token").await)?;
        if let Some(access) = access {
            let expires_at = optional_secret(ctx.secret("shortcut", "token_expires_at").await)?;
            let stale =
                token_needs_refresh(expires_at.as_ref().map(|s| s.expose_secret()), Utc::now());
            if stale {
                if let Some(rt) = optional_secret(ctx.secret("shortcut", "refresh_token").await)? {
                    match self.refresh_access_token(rt.expose_secret()).await {
                        Ok(refreshed) => {
                            self.persist_refreshed(ctx, &refreshed).await;
                            return Ok(ShortcutAuth::Bearer(SecretString::from(
                                refreshed.access_token,
                            )));
                        }
                        Err(e) => {
                            // A failed proactive refresh is not
                            // fatal: fall through and try the
                            // stored token; the 401 retry path
                            // gets a second chance.
                            tracing::warn!(error = %e, "shortcut: proactive token refresh failed");
                        }
                    }
                }
            }
            return Ok(ShortcutAuth::Bearer(access));
        }
        ctx.secret("shortcut", "api_token")
            .await?
            .map(ShortcutAuth::ApiToken)
            .ok_or_else(|| AgentError::CredentialsMissing {
                service: "shortcut".to_string(),
                field: "api_token".to_string(),
            })
    }

    /// Exchange the saved `refresh_token` for a fresh token set via
    /// `grant_type=refresh_token` at the operator-configured token
    /// endpoint. The refresh token is sent as a form field and
    /// never appears in an error string (AGENTS.md §6) — error
    /// paths carry only the upstream status + truncated reply.
    async fn refresh_access_token(
        &self,
        refresh_token: &str,
    ) -> Result<RefreshedToken, AgentError> {
        let Some(creds) = oauth_client_creds() else {
            return Err(AgentError::AgentFailed(
                "shortcut oauth refresh: SHORTCUT_OAUTH_CLIENT_ID is not set on the server; \
                 set it to the Shortcut agent app's client id (same var the \
                 /api/auth/login/shortcut/start handler uses)"
                    .into(),
            ));
        };
        let mut form = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), creds.client_id.clone()),
        ];
        if let Some(secret) = &creds.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        let resp = self
            .http
            .post(&creds.token_url)
            .timeout(std::time::Duration::from_millis(self.inner.cfg.timeout_ms))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                // reqwest's Display carries the URL but never the
                // form body, so the refresh token cannot leak.
                AgentError::AgentFailed(format!("shortcut oauth refresh transport: {e}"))
            })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("shortcut oauth refresh read: {e}")))?;
        if !status.is_success() {
            return Err(AgentError::AgentFailed(format!(
                "shortcut oauth refresh rejected (HTTP {}): {}",
                status.as_u16(),
                nagent_shortcut::truncate(&body, 256)
            )));
        }
        let parsed: Value = serde_json::from_str(&body).map_err(|e| {
            AgentError::AgentFailed(format!(
                "shortcut oauth refresh parse: {e}; body={}",
                nagent_shortcut::truncate(&body, 256)
            ))
        })?;
        let access_token = parsed
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AgentError::AgentFailed(
                    "shortcut oauth refresh: token response has no `access_token`".into(),
                )
            })?
            .to_string();
        let refresh_token = parsed
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let expires_in = parsed
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600)
            .max(60);
        let scope = parsed
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(RefreshedToken {
            access_token,
            refresh_token,
            expires_at: Utc::now() + Duration::seconds(expires_in as i64),
            scope,
        })
    }

    /// Write the refreshed token set back to the vault through the
    /// `SecretSink`. A missing sink (test / direct-invoke
    /// contexts) only downgrades persistence — the fresh token is
    /// still used for the current call — so the error is logged,
    /// not propagated.
    async fn persist_refreshed(&self, ctx: &UserContext, refreshed: &RefreshedToken) {
        let mut fields: Vec<(&str, SecretString)> = vec![
            (
                "access_token",
                SecretString::from(refreshed.access_token.clone()),
            ),
            (
                "token_expires_at",
                SecretString::from(refreshed.expires_at.to_rfc3339()),
            ),
        ];
        if let Some(rt) = &refreshed.refresh_token {
            fields.push(("refresh_token", SecretString::from(rt.clone())));
        }
        if let Some(scope) = &refreshed.scope {
            fields.push(("token_scope", SecretString::from(scope.clone())));
        }
        if let Err(e) = ctx.update_secret("shortcut", &fields).await {
            tracing::warn!(
                error = %e,
                "shortcut: could not persist the refreshed token to the vault"
            );
        }
    }

    /// One request with a single refresh-and-retry on 401 when the
    /// caller authenticated with OAuth and a refresh token is
    /// saved. API-token callers get the 401 verbatim (the token was
    /// revoked server-side; only a reconnect helps).
    async fn send(
        &self,
        ctx: &UserContext,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Value, AgentError> {
        let mut auth = self.auth(ctx).await?;
        let mut retried = false;
        loop {
            let result = if method == reqwest::Method::GET {
                self.inner.get(&auth, path, query).await
            } else if method == reqwest::Method::POST {
                let body = body
                    .ok_or_else(|| AgentError::InvalidArguments("POST requires a body".into()))?;
                self.inner.post(&auth, path, body).await
            } else {
                return Err(AgentError::InvalidArguments(format!(
                    "unsupported method {method}"
                )));
            };
            match result {
                Err(nagent_shortcut::ShortcutError::Unauthorized(msg)) if !retried => {
                    let can_refresh = matches!(auth, ShortcutAuth::Bearer(_));
                    if can_refresh {
                        if let Some(rt) =
                            optional_secret(ctx.secret("shortcut", "refresh_token").await)?
                        {
                            retried = true;
                            let refreshed = self.refresh_access_token(rt.expose_secret()).await?;
                            self.persist_refreshed(ctx, &refreshed).await;
                            auth = ShortcutAuth::Bearer(SecretString::from(refreshed.access_token));
                            continue;
                        }
                    }
                    return Err(AgentError::from(
                        nagent_shortcut::ShortcutError::Unauthorized(msg),
                    ));
                }
                other => return other.map_err(AgentError::from),
            }
        }
    }

    /// Authenticated GET with the credential resolved from `ctx`
    /// (plus the one-shot 401 refresh retry). This shadows the
    /// inner client's auth-taking `get` — the tool implementations
    /// keep their `client.get(ctx, …)` shape.
    pub async fn get(
        &self,
        ctx: &UserContext,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, AgentError> {
        self.send(ctx, reqwest::Method::GET, path, query, None)
            .await
    }

    /// Authenticated POST with a JSON body, credential resolved from
    /// `ctx` (write operations: `shortcut_create_story`). The body
    /// is never logged or formatted into an error path (AGENTS.md
    /// §6).
    pub async fn post(
        &self,
        ctx: &UserContext,
        path: &str,
        body: &Value,
    ) -> Result<Value, AgentError> {
        self.send(ctx, reqwest::Method::POST, path, &[], Some(body))
            .await
    }

    /// `POST /stories` end-to-end from the chat context: resolve +
    /// refresh-on-401 + ship + project the created story.
    pub async fn create_story(&self, ctx: &UserContext, body: &Value) -> Result<Value, AgentError> {
        let raw = self.post(ctx, "/stories", body).await?;
        Ok(nagent_shortcut::project_story(&raw))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_skew_window_is_applied() {
        let now = Utc::now();
        let future = (now + Duration::seconds(3600)).to_rfc3339();
        let soon = (now + Duration::seconds(30)).to_rfc3339();
        let past = (now - Duration::seconds(5)).to_rfc3339();
        assert!(!token_needs_refresh(Some(&future), now));
        assert!(token_needs_refresh(Some(&soon), now));
        assert!(token_needs_refresh(Some(&past), now));
        // Missing / malformed values never trigger a refresh.
        assert!(!token_needs_refresh(None, now));
        assert!(!token_needs_refresh(Some(""), now));
        assert!(!token_needs_refresh(Some("not-a-date"), now));
    }

    #[test]
    fn refresh_form_never_leaks_into_debug() {
        // The refreshed-token struct must not expose the tokens
        // through Debug (AGENTS.md §6).
        let rt = RefreshedToken {
            access_token: "access_secret".into(),
            refresh_token: Some("refresh_secret".into()),
            expires_at: Utc::now(),
            scope: None,
        };
        assert!(!format!("{rt:?}").contains("access_secret"));
        assert!(!format!("{rt:?}").contains("refresh_secret"));
    }
}
