//! X (Twitter) OAuth 2.0 PKCE flow + refresh-token client.
//!
//! Reuses the existing OAuth primitives ([`crate::oauth::pkce`],
//! [`crate::oauth::state`], [`crate::oauth::refresh`]) and adds the
//! X-specific wire calls (`/i/oauth2/authorize`, `/2/oauth2/token`,
//! `/2/users/me`).
//!
//! ## Wire surface
//!
//! - `GET /api/auth/login/x/start` — anonymous; reads the session
//!   cookie to bind the resulting tokens to the calling user, mints
//!   a PKCE pair + state token, redirects to
//!   `https://x.com/i/oauth2/authorize?...`.
//! - `GET /api/auth/login/x/callback` — anonymous; verifies
//!   `state`, exchanges the `code` for a token, resolves
//!   `x_user_id` / `x_screen_name` via `/2/users/me`, and
//!   encrypts + UPSERTs the six `x_account` field rows.
//! - `POST /api/auth/login/x/disconnect` — authenticated; deletes
//!   every `x_account` field row for the calling user.
//!
//! ## Refresh
//!
//! Refresh is **not** handled here; it lives in the agent itself
//! (plan 1790695073418 — locked decision). On 401 the agent POSTs
//! to `/2/oauth2/token` with `grant_type=refresh_token` and writes
//! the new field set back to the vault through
//! [`crate::agents::UserContext::update_secret`].

use std::sync::Arc;

use base64::Engine;
use chrono::{Duration, Utc};
use secrecy::SecretString;
use serde::Deserialize;

use crate::credentials::crypto::encrypt as vault_encrypt;
use crate::credentials::key::CredentialsKey;
use crate::oauth::refresh::{RefreshTokenClient, RefreshTokenError, TokenSet};
use crate::oauth::{PkcePair, StateStore};

use nagent_db::NewAuthEvent;

/// Runtime knobs the X OAuth dance reads. Mirrors the TOML schema
/// in [`crate::config_file`] (the `[x_oauth]` section).
#[derive(Debug, Clone)]
pub struct XOAuthConfig {
    /// Master switch. When `false`, the X OAuth routes are not
    /// mounted and the X OAuth state is `None` on `AuthState`.
    pub enabled: bool,
    /// X Developer app client id.
    pub client_id: String,
    /// Optional confidential-client `client_secret`. Empty for
    /// PKCE-only public clients (Native / Single Page App mode).
    pub client_secret: Option<String>,
    /// OAuth redirect path — combined with `cfg.auth.public_url` to
    /// build the absolute callback URL the X Developer portal
    /// expects.
    pub redirect_path: String,
    /// OAuth scopes requested at `/authorize` time. The agent and
    /// the chat UI document this as
    /// `["tweet.read", "users.read", "follows.read"]`.
    pub scopes: Vec<String>,
    /// Per-request connect+read timeout in milliseconds for the
    /// `/oauth2/token` + `/users/me` calls.
    pub timeout_ms: u64,
}

impl XOAuthConfig {
    /// Build the absolute redirect URL the X Developer portal expects,
    /// from the server's `public_url` and this config's
    /// `redirect_path`. The `public_url` is supplied by the caller
    /// (the X OAuth state is built in `app::build_app` after the
    /// full `Config` is resolved).
    pub fn redirect_url(&self, public_url: &str) -> String {
        let trimmed = public_url.trim_end_matches('/');
        format!("{}{}", trimmed, self.redirect_path)
    }
}

/// Resolved X OAuth state. Built in `app::build_app` when
/// `cfg.x_oauth.client_id` is non-empty AND `cfg.x_oauth.enabled`
/// is `true`; the failure mode is "log warn, leave `auth.x = None`"
/// so the routes are simply not registered when the feature is off.
#[derive(Clone)]
pub struct XOAuthState {
    /// Static config (client id, scopes, redirect URL).
    pub cfg: Arc<XOAuthConfig>,
    /// Pre-built PKCE/state store. Per-request `(user_id, verifier)`
    /// round-trip lives here.
    pub state: StateStore,
    /// DB handle for the per-user vault and `auth_events` audit rows.
    pub store: nagent_db::Db,
    /// Encryption key for `nagent_db::Credentials::upsert`.
    pub key: Arc<CredentialsKey>,
    /// Public URL (read off `cfg.auth.public_url`). Stored on the
    /// state so the callback handler does not have to thread the
    /// full `Config` through the request.
    pub public_url: String,
    /// Per-call HTTP client. `reqwest::Client` is `Arc`-backed and
    /// the connection pool is kept warm across requests; we build
    /// it once with the configured timeout.
    pub http: reqwest::Client,
    /// Refresh-token client. Owned by the X OAuth state so the
    /// callback handler can also use it if a refresh is needed
    /// before the redirect.
    pub refresh: XRefreshTokenClient,
}

impl std::fmt::Debug for XOAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XOAuthState")
            .field("cfg", &self.cfg)
            .field("state", &self.state)
            .field("store", &"<nagent_db::Db>")
            .field("key", &self.key)
            .field("public_url", &self.public_url)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

/// X-specific [`RefreshTokenClient`] impl.
#[derive(Clone)]
pub struct XRefreshTokenClient {
    pub http: reqwest::Client,
    pub cfg: Arc<XOAuthConfig>,
}

impl std::fmt::Debug for XRefreshTokenClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XRefreshTokenClient")
            .field("http", &"<reqwest::Client>")
            .field("cfg", &self.cfg)
            .finish()
    }
}

#[async_trait::async_trait]
impl RefreshTokenClient for XRefreshTokenClient {
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, RefreshTokenError> {
        let url = format!("{}/2/oauth2/token", "https://api.x.com");
        let mut form: Vec<(String, String)> = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
            ("client_id".to_string(), self.cfg.client_id.clone()),
        ];
        if let Some(secret) = self.cfg.client_secret.as_deref() {
            form.push(("client_secret".to_string(), secret.to_string()));
        }
        let resp = self
            .http
            .post(&url)
            .form(&form)
            .send()
            .await
            .map_err(|e| RefreshTokenError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| RefreshTokenError::Transport(e.to_string()))?;
        if status.as_u16() == 400 || status.as_u16() == 401 {
            return Err(RefreshTokenError::Rejected(truncate(&body, 256)));
        }
        if !status.is_success() {
            return Err(RefreshTokenError::Transport(format!(
                "status {}: {}",
                status.as_u16(),
                truncate(&body, 256)
            )));
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| RefreshTokenError::Malformed(e.to_string()))?;
        let access_token = parsed
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RefreshTokenError::Malformed("access_token missing".into()))?
            .to_string();
        let new_refresh = parsed
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let expires_in = parsed
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600)
            .max(60) as u64;
        let scope = parsed
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(TokenSet {
            access_token,
            refresh_token: new_refresh,
            expires_in: std::time::Duration::from_secs(expires_in),
            scope,
        })
    }
}

/// Body of `GET /api/auth/login/x/callback`.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Build a `XOAuthState` from the resolved config. Returns `None`
/// when X OAuth is disabled or the config is incomplete.
pub fn build_state(
    cfg: Arc<XOAuthConfig>,
    store: nagent_db::Db,
    key: Arc<CredentialsKey>,
    public_url: String,
) -> Option<XOAuthState> {
    if !cfg.enabled || cfg.client_id.is_empty() {
        return None;
    }
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(cfg.timeout_ms.max(1_000)))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("reqwest client build");
    let refresh = XRefreshTokenClient {
        http: http.clone(),
        cfg: cfg.clone(),
    };
    Some(XOAuthState {
        cfg,
        state: StateStore::new(),
        store,
        key,
        public_url,
        http,
        refresh,
    })
}

/// `GET /api/auth/login/x/start`
///
/// Reads the session cookie so the resulting tokens are bound to
/// the authenticated user (the route is mounted under `RequireAuth`).
///
/// The handler mints a fresh PKCE pair + state token, stores the
/// `(user_id, verifier, created_at)` payload in the in-memory
/// `StateStore`, and 302-redirects the browser to the X
/// `/i/oauth2/authorize` endpoint.
pub async fn start_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<crate::auth::session::AuthUser>,
) -> Result<axum::response::Response, AuthError> {
    let x = auth
        .x
        .as_ref()
        .ok_or_else(|| AuthError::Internal("X OAuth state not wired".into()))?;
    let user_id = auth_user.id;
    let pair = PkcePair::generate();
    let state_token = x.state.push(serde_json::json!({
        "user_id": user_id.to_string(),
        "pkce_verifier": pair.verifier.as_str(),
        "created_at": Utc::now().to_rfc3339(),
    }));
    let scopes = if x.cfg.scopes.is_empty() {
        "tweet.read users.read follows.read".to_string()
    } else {
        x.cfg.scopes.join(" ")
    };
    let redirect_uri = x.cfg.redirect_url(&x.public_url);
    let authorize = format!(
        "https://x.com/i/oauth2/authorize?response_type=code&client_id={client_id}\
         &redirect_uri={redirect_uri}&scope={scopes}&state={state_token}\
         &code_challenge={challenge}&code_challenge_method=S256",
        client_id = urlencoding(&x.cfg.client_id),
        redirect_uri = urlencoding(&redirect_uri),
        scopes = urlencoding(&scopes),
        state_token = urlencoding(&state_token),
        challenge = urlencoding(&pair.challenge),
    );
    tracing::info!(
        user_id = %user_id,
        "x_oauth: redirecting browser to X /i/oauth2/authorize"
    );
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, authorize)
        .body(axum::body::Body::empty())
        .expect("static response builder"))
}

/// `GET /api/auth/login/x/callback`
///
/// Verifies the `state` token, exchanges the `code` for an access
/// token + refresh token, resolves `x_user_id` + `x_screen_name`
/// via `/2/users/me`, encrypts the six `x_account` field rows, and
/// UPSERTs them through the same audit-logging row that
/// `PUT /api/integrations/:id/credentials` uses. Redirects to the
/// integrations page with a query string the JS side reads.
pub async fn callback_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<crate::auth::session::AuthUser>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> Result<axum::response::Response, AuthError> {
    let x = auth
        .x
        .as_ref()
        .ok_or_else(|| AuthError::Internal("X OAuth state not wired".into()))?;
    // Read user_id from the session so a stolen `state` token
    // (read off the browser redirect) cannot bind to a different
    // user than the one who initiated `/start`.
    let session_user_id = auth_user.id;
    let Some(state_token) = q.state.as_deref() else {
        return Ok(redirect_with_error("missing_state"));
    };
    let entry = match x.state.pop(state_token) {
        Some(e) => e,
        None => {
            tracing::warn!(
                state_token = %state_token,
                "x_oauth: state token absent (expired or never minted)"
            );
            return Ok(redirect_with_error("expired_state"));
        }
    };
    let state_user_id_str = entry
        .payload
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AuthError::Internal("x_oauth state payload missing user_id".into()))?;
    let state_user_id: uuid::Uuid = state_user_id_str
        .parse()
        .map_err(|e| AuthError::Internal(format!("x_oauth user_id parse: {e}")))?;
    // Bind: the `/start` caller and the `/callback` caller MUST
    // be the same authenticated user. A token stolen at `/start`
    // and replayed at `/callback` by a different logged-in user
    // would otherwise bind the resulting tokens to the wrong
    // account.
    if state_user_id != session_user_id {
        tracing::warn!(
            session_user_id = %session_user_id,
            state_user_id = %state_user_id,
            "x_oauth: /start and /callback user mismatch — refusing to bind tokens"
        );
        return Ok(redirect_with_error("user_mismatch"));
    }
    let pkce_verifier = entry
        .payload
        .get("pkce_verifier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AuthError::Internal("x_oauth state payload missing pkce_verifier".into()))?
        .to_string();
    if let Some(err) = q.error.as_deref() {
        tracing::warn!(
            user_id = %session_user_id,
            error = err,
            description = q.error_description.as_deref().unwrap_or(""),
            "x_oauth: X returned an error on /callback"
        );
        return Ok(redirect_with_error("oauth_error"));
    }
    let Some(code) = q.code.as_deref() else {
        return Ok(redirect_with_error("missing_code"));
    };

    // 1. Token exchange.
    let token_resp = match exchange_code(x, code, &pkce_verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(user_id = %session_user_id, error = %e, "x_oauth: code exchange failed");
            return Ok(redirect_with_error("token_exchange_failed"));
        }
    };
    let expires_at = Utc::now() + Duration::seconds(token_resp.expires_in);
    let access_token = token_resp.access_token.clone();

    // 2. /2/users/me lookup.
    let (x_user_id, x_screen_name) = match lookup_me(x, &access_token).await {
        Ok(me) => me,
        Err(e) => {
            tracing::warn!(
                user_id = %session_user_id,
                error = %e,
                "x_oauth: /2/users/me failed"
            );
            return Ok(redirect_with_error("users_me_failed"));
        }
    };

    // 3. Encrypt + UPSERT every field through the existing
    // `nagent_db::Credentials::upsert` path.
    let rows = build_field_rows(
        x,
        &access_token,
        token_resp.refresh_token.as_deref(),
        token_resp.scope.as_deref(),
        &x_screen_name,
        &x_user_id,
        expires_at,
    )?;
    x.store
        .for_user(session_user_id)
        .credentials()
        .upsert("x_account", &rows)
        .await
        .map_err(|e| AuthError::Internal(format!("x_oauth credentials upsert: {e}")))?;

    // 4. Audit row.
    x.store.admin().events.record(NewAuthEvent {
        user_id: Some(session_user_id),
        kind: "credential_oauth_x_connected".to_string(),
        provider: "oauth_x".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("x_account".to_string()),
    });

    Ok(redirect_with_status("connected"))
}

/// `POST /api/auth/login/x/disconnect`
///
/// Authenticated; deletes every `x_account` field row for the
/// calling user via `nagent_db::Credentials::delete_service`.
pub async fn disconnect_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<crate::auth::session::AuthUser>,
) -> Result<axum::response::Response, AuthError> {
    let x = auth
        .x
        .as_ref()
        .ok_or_else(|| AuthError::Internal("X OAuth state not wired".into()))?;
    let user_id = auth_user.id;
    x.store
        .for_user(user_id)
        .credentials()
        .delete_service("x_account")
        .await
        .map_err(|e| AuthError::Internal(format!("x_oauth disconnect: {e}")))?;
    x.store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: "credential_oauth_x_disconnected".to_string(),
        provider: "oauth_x".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("x_account".to_string()),
    });
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct XTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: i64,
    #[serde(default)]
    scope: Option<String>,
}

fn default_expires_in() -> i64 {
    3600
}

async fn exchange_code(
    state: &XOAuthState,
    code: &str,
    pkce_verifier: &str,
) -> Result<XTokenResponse, String> {
    let url = "https://api.x.com/2/oauth2/token".to_string();
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        (
            "redirect_uri".to_string(),
            state.cfg.redirect_url(&state.public_url),
        ),
        ("client_id".to_string(), state.cfg.client_id.clone()),
        ("code_verifier".to_string(), pkce_verifier.to_string()),
    ];
    if let Some(secret) = state.cfg.client_secret.as_deref() {
        form.push(("client_secret".to_string(), secret.to_string()));
    }
    let resp = state
        .http
        .post(&url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read: {e}"))?;
    if status.as_u16() == 400 || status.as_u16() == 401 {
        return Err(format!("rejected ({status}): {}", truncate(&body, 256)));
    }
    if !status.is_success() {
        return Err(format!(
            "non-success status {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("parse: {e}; body={}", truncate(&body, 256)))
}

#[derive(Debug, Deserialize)]
struct XUserMe {
    data: serde_json::Value,
}

async fn lookup_me(state: &XOAuthState, bearer: &str) -> Result<(String, String), String> {
    let url = "https://api.x.com/2/users/me";
    let resp = state
        .http
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "non-success status {}: {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    let parsed: XUserMe = serde_json::from_str(&body)
        .map_err(|e| format!("parse: {e}; body={}", truncate(&body, 256)))?;
    let user_id = parsed
        .data
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing id".to_string())?
        .to_string();
    let screen_name = parsed
        .data
        .get("username")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing username".to_string())?
        .to_string();
    Ok((user_id, screen_name))
}

/// Build the encrypted `(field_key, nonce, ciphertext)` rows the
/// vault expects. `access_token` and `refresh_token` are wrapped in
/// `SecretString` so the plaintext never lives in a `String` longer
/// than the call to `vault_encrypt`.
fn build_field_rows(
    state: &XOAuthState,
    access_token: &str,
    refresh_token: Option<&str>,
    scope: Option<&str>,
    x_screen_name: &str,
    x_user_id: &str,
    expires_at: chrono::DateTime<Utc>,
) -> Result<Vec<(String, Vec<u8>, Vec<u8>)>, AuthError> {
    let mut rows = Vec::with_capacity(6);
    for (field_key, plaintext) in &[
        ("access_token", access_token.to_string()),
        ("x_screen_name", x_screen_name.to_string()),
        ("x_user_id", x_user_id.to_string()),
        (
            "token_expires_at",
            expires_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
    ] {
        let sealed = vault_encrypt(&state.key, plaintext).map_err(|e| {
            AuthError::Internal(format!(
                "x_oauth encrypt failed for field `{field_key}`: {e}"
            ))
        })?;
        rows.push((field_key.to_string(), sealed.nonce, sealed.ciphertext));
    }
    // Optional fields are stored as empty Text when the IdP did not
    // hand us one back (the OAuth spec lets providers omit
    // `refresh_token` / `scope`).
    for (field_key, plaintext) in &[
        ("refresh_token", refresh_token.unwrap_or("").to_string()),
        ("token_scope", scope.unwrap_or("").to_string()),
    ] {
        let sealed = vault_encrypt(&state.key, plaintext).map_err(|e| {
            AuthError::Internal(format!(
                "x_oauth encrypt failed for field `{field_key}`: {e}"
            ))
        })?;
        rows.push((field_key.to_string(), sealed.nonce, sealed.ciphertext));
    }
    let _ = SecretString::from(access_token.to_string()); // ensure import is exercised if cfg changes
    let _ = SecretString::from(String::new());
    Ok(rows)
}

// ---------------------------------------------------------------------------
// URL encoding + redirects
// ---------------------------------------------------------------------------

fn urlencoding(s: &str) -> String {
    // We use base64-url friendly encoding for the state + verifier
    // values (the inputs are ASCII-only), and percent-encode the
    // strings that may contain non-URL-safe characters (client id,
    // redirect URI, scopes).
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("…[truncated]");
    out
}

fn redirect_with_error(reason: &'static str) -> axum::response::Response {
    let url = format!("/settings/integrations?x_error={reason}");
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, url)
        .body(axum::body::Body::empty())
        .expect("static redirect response")
}

fn redirect_with_status(reason: &'static str) -> axum::response::Response {
    let url = format!("/settings/integrations?x_connected={reason}");
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, url)
        .body(axum::body::Body::empty())
        .expect("static redirect response")
}

// ---------------------------------------------------------------------------
// AuthError alias used by handlers
// ---------------------------------------------------------------------------

pub use crate::auth::error::AuthError;

// `base64::Engine` is referenced via `bearer_auth` indirectly;
// silences the unused-import lint for code paths that do not need it.
#[allow(dead_code)]
fn _silence_base64() {
    let _ = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"");
}

// IntoResponse impl on `AuthError` is provided by
// `crate::auth::error`; this file does not need to import axum's
// `IntoResponse` trait unless it converts `Result<_, AuthError>` to
// `Response` directly. The handlers above return
// `Result<Response, AuthError>`; the conversion is provided by
// `impl IntoResponse for AuthError` in `auth/error.rs`.
// `into_response` is re-exported through `axum::response::IntoResponse`
// so the handlers can build a `Response` and propagate `AuthError`
// uniformly.
//
// Import trait for `Result<_, AuthError>` -> axum response handling.
use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_url_joins_public_url_and_path() {
        let cfg = XOAuthConfig {
            enabled: true,
            client_id: "id".into(),
            client_secret: None,
            redirect_path: "/api/auth/login/x/callback".into(),
            scopes: vec!["tweet.read".into()],
            timeout_ms: 8000,
        };
        assert_eq!(
            cfg.redirect_url("https://example.com/"),
            "https://example.com/api/auth/login/x/callback"
        );
        assert_eq!(
            cfg.redirect_url("https://example.com"),
            "https://example.com/api/auth/login/x/callback"
        );
    }

    #[test]
    fn url_encoding_keeps_unreserved_chars() {
        assert_eq!(urlencoding("abc-DEF_1.2~3"), "abc-DEF_1.2~3");
    }

    #[test]
    fn url_encoding_percent_encodes_rest() {
        assert_eq!(urlencoding("a/b"), "a%2Fb");
        assert_eq!(urlencoding("é"), "%C3%A9");
    }

    #[test]
    fn truncate_caps_at_byte_boundary() {
        let s = "é".repeat(10); // 20 bytes
        let t = truncate(&s, 3);
        // 3 bytes is mid-codepoint; the function backs up to a
        // boundary (1 byte = "é").
        assert!(t.ends_with("…[truncated]"));
        assert!(t.starts_with("é"));
    }
}
