//! `x_oauth` — `[x_oauth]` section.
//!
//! Per-user X (Twitter) OAuth 2.0 PKCE flow. The OAuth dance is a
//! per-user **integration** (plan 1790695073418) — not a nagent
//! login backend — so this section is intentionally **not** under
//! `[auth.*]` (which controls *how the user signs into nagent*).
//!
//! When `x_oauth.client_id` is empty the X OAuth routes are not
//! registered and the `XOAuthState` is `None` on `AuthState`. The
//! `x_timeline` agent fails closed with `CredentialsMissing` for
//! any user that has not connected X via `/settings/integrations`.

use crate::config::file::TomlXOAuthConfig;
use crate::config::{env_opt, resolve_csv, resolve_opt_string, resolve_primitive, ConfigError};

/// X OAuth 2.0 PKCE client config.
#[derive(Debug, Clone)]
pub struct XOAuthConfig {
    /// Master switch. When `false` the X OAuth routes are not
    /// registered. `X_OAUTH_ENABLED=false` is honoured even when
    /// the TOML overlay sets it to `true` (canonical 12-factor
    /// contract).
    pub enabled: bool,
    /// X Developer app client id. Empty → X OAuth disabled.
    pub client_id: String,
    /// Optional confidential-client `client_secret`. Empty for
    /// PKCE-only public clients (Native / Single Page App mode).
    pub client_secret: Option<String>,
    /// OAuth redirect path — combined with `cfg.auth.public_url`
    /// to build the absolute callback URL the X Developer portal
    /// expects. Default `/api/auth/login/x/callback` so the
    /// default config can be enabled with `X_OAUTH_CLIENT_ID`
    /// alone.
    pub redirect_path: String,
    /// OAuth scopes requested at `/authorize` time. Default
    /// `["tweet.read", "users.read", "follows.read"]` (the
    /// read-only surface documented in the plan).
    pub scopes: Vec<String>,
    /// Per-request connect+read timeout in milliseconds for the
    /// `/oauth2/token` + `/users/me` calls.
    pub timeout_ms: u64,
}

impl Default for XOAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            client_secret: None,
            redirect_path: "/api/auth/login/x/callback".to_string(),
            scopes: vec![
                "tweet.read".to_string(),
                "users.read".to_string(),
                "follows.read".to_string(),
            ],
            timeout_ms: 8_000,
        }
    }
}

impl XOAuthConfig {
    pub fn from_env_with_toml(toml: Option<&TomlXOAuthConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        let enabled = resolve_primitive(
            env_opt("X_OAUTH_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "X_OAUTH_ENABLED",
        )?;

        let client_id = resolve_opt_string(
            env_opt("X_OAUTH_CLIENT_ID").as_deref(),
            toml.client_id.as_deref(),
        )
        .unwrap_or_default();

        let client_secret = resolve_opt_string(
            env_opt("X_OAUTH_CLIENT_SECRET").as_deref(),
            toml.client_secret.as_deref(),
        )
        .filter(|s| !s.is_empty());

        let redirect_path = resolve_opt_string(
            env_opt("X_OAUTH_REDIRECT_PATH").as_deref(),
            toml.redirect_path.as_deref(),
        )
        .unwrap_or_else(|| defaults.redirect_path.clone());

        let scopes = resolve_csv(
            env_opt("X_OAUTH_SCOPES").as_deref(),
            toml.scopes.clone(),
            defaults.scopes.clone(),
        );

        let timeout_ms = resolve_primitive(
            env_opt("X_OAUTH_TIMEOUT_MS").as_deref(),
            toml.timeout_ms,
            defaults.timeout_ms,
            "X_OAUTH_TIMEOUT_MS",
        )?;

        Ok(Self {
            enabled,
            client_id,
            client_secret,
            redirect_path,
            scopes,
            timeout_ms,
        })
    }
}
