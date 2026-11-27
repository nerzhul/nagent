//! `shortcut_oauth` — `[shortcut_oauth]` section.
//!
//! Per-user Shortcut OAuth 2.0 (agent-app install flow, "Open
//! Agents"). Like `[x_oauth]`, this is a per-user **integration**,
//! not a nagent login backend, so it stays top-level and out of
//! `[auth.*]`.
//!
//! The operator registers one Shortcut agent app in their
//! workspace settings (app.shortcut.com → Settings → Apps /
//! "Build your own" agent), copies the client id / secret here,
//! and adds the redirect URL (`<public_url><redirect_path>`) under
//! the app's Connection settings. Each nagent user then installs
//! the app into their own Shortcut workspace via
//! `/api/auth/login/shortcut/start`.
//!
//! When `shortcut_oauth.client_id` is empty the Shortcut OAuth
//! routes are not registered and `AuthState.shortcut_oauth` is
//! `None`; users fall back to the classic `api_token` vault field.
//! The `shortcut_*` chat tools refresh the stored access token
//! themselves on expiry / 401 (they read the same
//! `SHORTCUT_OAUTH_CLIENT_ID` / `SHORTCUT_OAUTH_CLIENT_SECRET` /
//! `SHORTCUT_OAUTH_TOKEN_URL` env vars — see
//! `nagent-agents`' shortcut client).

use crate::config::file::TomlShortcutOAuthConfig;
use crate::config::{env_opt, resolve_csv, resolve_opt_string, resolve_primitive, ConfigError};

/// Default Shortcut OAuth authorize endpoint (agent-app install).
pub const DEFAULT_AUTHORIZE_URL: &str = "https://app.shortcut.com/oauth/authorize";
/// Default Shortcut OAuth token endpoint (code exchange + refresh).
pub const DEFAULT_TOKEN_URL: &str = "https://app.shortcut.com/oauth/token";
/// Default Shortcut REST API base the callback validates the fresh
/// token against (`GET {api_base_url}/members` with the bearer).
pub const DEFAULT_API_BASE_URL: &str = "https://api.app.shortcut.com/api/v3";

/// Shortcut OAuth 2.0 client config.
#[derive(Debug, Clone)]
pub struct ShortcutOAuthConfig {
    /// Master switch. When `false` the Shortcut OAuth routes are
    /// not registered. `SHORTCUT_OAUTH_ENABLED=false` is honoured
    /// even when the TOML overlay sets it to `true` (canonical
    /// 12-factor contract).
    pub enabled: bool,
    /// Shortcut agent-app client id. Empty → Shortcut OAuth disabled.
    pub client_id: String,
    /// Optional confidential-client `client_secret`.
    pub client_secret: Option<String>,
    /// OAuth redirect path — combined with `cfg.auth.public_url`
    /// to build the absolute callback URL registered in the
    /// Shortcut app's Connection settings. Default
    /// `/api/auth/login/shortcut/callback`.
    pub redirect_path: String,
    /// OAuth scopes requested at authorize time. Default empty —
    /// the Shortcut agent-app flow grants workspace API access
    /// without scope parameters.
    pub scopes: Vec<String>,
    /// Authorize endpoint. Default
    /// `https://app.shortcut.com/oauth/authorize`; overridable so
    /// tests can point at a loopback fixture.
    pub authorize_url: String,
    /// Token endpoint (code exchange + refresh). Default
    /// `https://app.shortcut.com/oauth/token`. The agents-side
    /// refresh reads the same value from `SHORTCUT_OAUTH_TOKEN_URL`.
    pub token_url: String,
    /// REST API base URL used to validate the fresh token at
    /// callback time. Default `https://api.app.shortcut.com/api/v3`.
    pub api_base_url: String,
    /// Per-request connect+read timeout in milliseconds for the
    /// token exchange + validation calls.
    pub timeout_ms: u64,
}

impl Default for ShortcutOAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            client_secret: None,
            redirect_path: "/api/auth/login/shortcut/callback".to_string(),
            scopes: Vec::new(),
            authorize_url: DEFAULT_AUTHORIZE_URL.to_string(),
            token_url: DEFAULT_TOKEN_URL.to_string(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            timeout_ms: 8_000,
        }
    }
}

impl ShortcutOAuthConfig {
    /// Absolute callback URL: `<public_url><redirect_path>`.
    pub fn redirect_url(&self, public_url: &str) -> String {
        let trimmed = public_url.trim_end_matches('/');
        format!("{}{}", trimmed, self.redirect_path)
    }

    pub fn from_env_with_toml(toml: Option<&TomlShortcutOAuthConfig>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let toml = toml.cloned().unwrap_or_default();

        let enabled = resolve_primitive(
            env_opt("SHORTCUT_OAUTH_ENABLED").as_deref(),
            toml.enabled,
            defaults.enabled,
            "SHORTCUT_OAUTH_ENABLED",
        )?;

        let client_id = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_CLIENT_ID").as_deref(),
            toml.client_id.as_deref(),
        )
        .unwrap_or_default();

        let client_secret = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_CLIENT_SECRET").as_deref(),
            toml.client_secret.as_deref(),
        )
        .filter(|s| !s.is_empty());

        let redirect_path = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_REDIRECT_PATH").as_deref(),
            toml.redirect_path.as_deref(),
        )
        .unwrap_or_else(|| defaults.redirect_path.clone());

        let scopes = resolve_csv(
            env_opt("SHORTCUT_OAUTH_SCOPES").as_deref(),
            toml.scopes.clone(),
            defaults.scopes.clone(),
        );

        let authorize_url = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_AUTHORIZE_URL").as_deref(),
            toml.authorize_url.as_deref(),
        )
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| defaults.authorize_url.clone());

        let token_url = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_TOKEN_URL").as_deref(),
            toml.token_url.as_deref(),
        )
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| defaults.token_url.clone());

        let api_base_url = resolve_opt_string(
            env_opt("SHORTCUT_OAUTH_API_BASE_URL").as_deref(),
            toml.api_base_url.as_deref(),
        )
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| defaults.api_base_url.clone());

        let timeout_ms = resolve_primitive(
            env_opt("SHORTCUT_OAUTH_TIMEOUT_MS").as_deref(),
            toml.timeout_ms,
            defaults.timeout_ms,
            "SHORTCUT_OAUTH_TIMEOUT_MS",
        )?;

        Ok(Self {
            enabled,
            client_id,
            client_secret,
            redirect_path,
            scopes,
            authorize_url,
            token_url,
            api_base_url,
            timeout_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_docs() {
        let cfg = ShortcutOAuthConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.client_id.is_empty());
        assert_eq!(cfg.redirect_path, "/api/auth/login/shortcut/callback");
        assert!(cfg.scopes.is_empty());
        assert_eq!(cfg.authorize_url, DEFAULT_AUTHORIZE_URL);
        assert_eq!(cfg.token_url, DEFAULT_TOKEN_URL);
        assert_eq!(cfg.api_base_url, DEFAULT_API_BASE_URL);
        assert_eq!(cfg.timeout_ms, 8_000);
    }

    #[test]
    fn redirect_url_joins_public_url_and_path() {
        let cfg = ShortcutOAuthConfig::default();
        assert_eq!(
            cfg.redirect_url("https://nagent.example.com/"),
            "https://nagent.example.com/api/auth/login/shortcut/callback"
        );
        assert_eq!(
            cfg.redirect_url("https://nagent.example.com"),
            "https://nagent.example.com/api/auth/login/shortcut/callback"
        );
    }
}
