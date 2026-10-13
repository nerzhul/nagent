//! X (Twitter) OAuth 2.0 PKCE flow + refresh-token client.
//!
//! Wire layer for the X OAuth dance. The OAuth-spec bits
//! (PKCE `S256`, authorize URL, `POST /oauth2/token` for code
//! exchange + refresh) go through `oauth2::Client`; the X-specific
//! bits (`/2/users/me`, vault encryption, audit row,
//! `(start, callback)` user_id bind) stay here.
//!
//! Wire surface:
//! - `GET  /api/auth/login/x/start`     — mint PKCE + state, redirect to X.
//! - `GET  /api/auth/login/x/callback`  — verify state, exchange code, UPSERT vault row.
//! - `POST /api/auth/login/x/disconnect` — clear the vault row.
//!
//! Refresh is handled in the agent itself (plan 1790695073418 —
//! locked decision); this file only owns the connect/disconnect
//! half of the dance.

use std::sync::Arc;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointSet,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use serde::Deserialize;

use crate::credentials::crypto::encrypt as vault_encrypt;
use crate::credentials::key::CredentialsKey;
use crate::oauth::refresh::{RefreshTokenClient, RefreshTokenError, TokenSet};
use crate::oauth::StateStore;

use nagent_db::NewAuthEvent;

#[derive(Debug, Clone)]
pub struct XOAuthConfig {
    pub enabled: bool,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_path: String,
    pub scopes: Vec<String>,
    pub timeout_ms: u64,
}

impl XOAuthConfig {
    pub fn redirect_url(&self, public_url: &str) -> String {
        let trimmed = public_url.trim_end_matches('/');
        format!("{}{}", trimmed, self.redirect_path)
    }
}

const X_AUTH_URL: &str = "https://x.com/i/oauth2/authorize";
const X_TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";

// `oauth2::Client` type-states the five endpoint markers
// (`HasAuthUrl`, `HasDeviceAuthUrl`, `HasIntrospectionUrl`,
// `HasRevocationUrl`, `HasTokenUrl`); `BasicClient` defaults
// every marker to `EndpointNotSet`. The `XOauthClient` alias
// keeps the auth + token URLs at `EndpointSet` so the reference
// returned by `XRefreshTokenClient::oauth_client` exposes
// `authorize_url` / `exchange_code` / `exchange_refresh_token`
// statically. A future refactor that drops back to plain
// `BasicClient` would silently make those methods uncallable.
type XOauthClient = BasicClient<
    EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    EndpointSet,
>;

fn build_oauth_client(cfg: &XOAuthConfig, public_url: &str) -> Result<XOauthClient, AuthError> {
    let auth_url = AuthUrl::new(X_AUTH_URL.to_string()).map_err(|e| {
        AuthError::Internal(format!("x_oauth: invalid auth endpoint {X_AUTH_URL}: {e}"))
    })?;
    let token_url = TokenUrl::new(X_TOKEN_URL.to_string()).map_err(|e| {
        AuthError::Internal(format!(
            "x_oauth: invalid token endpoint {X_TOKEN_URL}: {e}"
        ))
    })?;
    let redirect_uri_str = cfg.redirect_url(public_url);
    let redirect_url = RedirectUrl::new(redirect_uri_str.clone()).map_err(|e| {
        AuthError::Internal(format!(
            "x_oauth: invalid redirect URI `{redirect_uri_str}`: {e}"
        ))
    })?;
    let mut client = BasicClient::new(ClientId::new(cfg.client_id.clone()))
        .set_auth_uri(auth_url)
        .set_token_uri(token_url)
        .set_redirect_uri(redirect_url)
        .set_auth_type(AuthType::RequestBody);
    // `set_client_secret` does not take an `Option`, so wire
    // it conditionally; PKCE-only public clients skip it.
    if let Some(secret) = cfg.client_secret.as_deref() {
        client = client.set_client_secret(ClientSecret::new(secret.to_string()));
    }
    Ok(client)
}

#[derive(Clone)]
pub struct XOAuthState {
    pub cfg: Arc<XOAuthConfig>,
    pub state: StateStore,
    pub store: nagent_db::Db,
    pub key: Arc<CredentialsKey>,
    pub public_url: String,
    pub http: reqwest::Client,
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

#[derive(Clone)]
pub struct XRefreshTokenClient {
    pub http: reqwest::Client,
    pub cfg: Arc<XOAuthConfig>,
    client: XOauthClient,
}

impl std::fmt::Debug for XRefreshTokenClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XRefreshTokenClient")
            .field("http", &"<reqwest::Client>")
            .field("cfg", &self.cfg)
            .field("client", &"<oauth2::BasicClient>")
            .finish()
    }
}

impl XRefreshTokenClient {
    pub fn new(
        http: reqwest::Client,
        cfg: Arc<XOAuthConfig>,
        public_url: &str,
    ) -> Result<Self, AuthError> {
        let client = build_oauth_client(&cfg, public_url)?;
        Ok(Self { http, cfg, client })
    }

    pub fn oauth_client(&self) -> &XOauthClient {
        &self.client
    }
}

#[async_trait::async_trait]
impl RefreshTokenClient for XRefreshTokenClient {
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, RefreshTokenError> {
        let result = self
            .client
            .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
            .request_async(&self.http)
            .await
            .map_err(|e| match e {
                oauth2::RequestTokenError::ServerResponse(resp) => {
                    RefreshTokenError::Rejected(format!("X rejected refresh: {resp}"))
                }
                oauth2::RequestTokenError::Request(req) => {
                    RefreshTokenError::Transport(req.to_string())
                }
                oauth2::RequestTokenError::Parse(perr, body) => {
                    RefreshTokenError::Malformed(format!(
                        "{perr}; body={}",
                        truncate(&String::from_utf8_lossy(&body), 256)
                    ))
                }
                oauth2::RequestTokenError::Other(msg) => RefreshTokenError::Transport(msg),
            })?;
        let access_token = result.access_token().secret().clone();
        let new_refresh = result.refresh_token().map(|t| t.secret().clone());
        let expires_in = result
            .expires_in()
            .unwrap_or(std::time::Duration::from_secs(3600));
        let scope = result
            .scopes()
            .map(|v| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "));
        Ok(TokenSet {
            access_token,
            refresh_token: new_refresh,
            expires_in: expires_in.max(std::time::Duration::from_secs(60)),
            scope,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

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
    let refresh = match XRefreshTokenClient::new(http.clone(), cfg.clone(), &public_url) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "x_oauth: failed to build oauth2 client; X OAuth routes will not mount"
            );
            return None;
        }
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

pub async fn start_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<crate::auth::session::AuthUser>,
) -> Result<axum::response::Response, AuthError> {
    let x = auth
        .x
        .as_ref()
        .ok_or_else(|| AuthError::Internal("X OAuth state not wired".into()))?;
    let user_id = auth_user.id;
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let verifier_str = pkce_verifier.secret().clone();
    let state_token = x.state.push(serde_json::json!({
        "user_id": user_id.to_string(),
        "pkce_verifier": verifier_str,
        "created_at": chrono::Utc::now().to_rfc3339(),
    }));
    let scopes: Vec<Scope> = if x.cfg.scopes.is_empty() {
        vec![
            Scope::new("tweet.read".to_string()),
            Scope::new("users.read".to_string()),
            Scope::new("follows.read".to_string()),
        ]
    } else {
        x.cfg.scopes.iter().map(|s| Scope::new(s.clone())).collect()
    };
    let (authorize_url, _csrf) = x
        .refresh
        .oauth_client()
        // `state_fn` mints the `state` round-trip token; we
        // hand back the one we just stashed in `StateStore`
        // so `/callback` can recover the user_id + PKCE
        // verifier from it.
        .authorize_url(|| CsrfToken::new(state_token.clone()))
        .set_pkce_challenge(pkce_challenge)
        .add_scopes(scopes)
        .url();
    tracing::info!(
        user_id = %user_id,
        "x_oauth: redirecting browser to X /i/oauth2/authorize"
    );
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, authorize_url.as_str())
        .body(axum::body::Body::empty())
        .expect("static response builder"))
}

pub async fn callback_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<crate::auth::session::AuthUser>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> Result<axum::response::Response, AuthError> {
    let x = auth
        .x
        .as_ref()
        .ok_or_else(|| AuthError::Internal("X OAuth state not wired".into()))?;
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
    // Bind: the `/start` caller and the `/callback` caller
    // MUST be the same authenticated user. Without this
    // check a stolen `state` token replayed at `/callback`
    // by a different logged-in user would bind the
    // resulting tokens to the wrong account.
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

    let token_resp = match exchange_code(x, code, &pkce_verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(user_id = %session_user_id, error = %e, "x_oauth: code exchange failed");
            return Ok(redirect_with_error("token_exchange_failed"));
        }
    };
    let expires_at =
        chrono::Utc::now() + chrono::Duration::seconds(token_resp.expires_in.as_secs() as i64);
    let access_token = token_resp.access_token.clone();

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

struct XTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: std::time::Duration,
    scope: Option<String>,
}

async fn exchange_code(
    state: &XOAuthState,
    code: &str,
    pkce_verifier: &str,
) -> Result<XTokenResponse, String> {
    let result = state
        .refresh
        .oauth_client()
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_string()))
        .request_async(&state.http)
        .await
        .map_err(|e| match e {
            oauth2::RequestTokenError::ServerResponse(resp) => format!("rejected: {resp}"),
            oauth2::RequestTokenError::Request(req) => format!("transport: {req}"),
            oauth2::RequestTokenError::Parse(perr, body) => format!(
                "parse: {perr}; body={}",
                truncate(&String::from_utf8_lossy(&body), 256)
            ),
            oauth2::RequestTokenError::Other(msg) => msg.to_string(),
        })?;
    Ok(XTokenResponse {
        access_token: result.access_token().secret().clone(),
        refresh_token: result.refresh_token().map(|t| t.secret().clone()),
        expires_in: result
            .expires_in()
            .unwrap_or(std::time::Duration::from_secs(3600))
            .max(std::time::Duration::from_secs(60)),
        scope: result
            .scopes()
            .map(|v| v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")),
    })
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

fn build_field_rows(
    state: &XOAuthState,
    access_token: &str,
    refresh_token: Option<&str>,
    scope: Option<&str>,
    x_screen_name: &str,
    x_user_id: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
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
    // OAuth spec lets providers omit `refresh_token` /
    // `scope`; store an empty string in those slots.
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
    Ok(rows)
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

pub use crate::auth::error::AuthError;

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
    fn truncate_caps_at_byte_boundary() {
        let s = "é".repeat(10); // 20 bytes
        let t = truncate(&s, 3);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.starts_with("é"));
    }

    #[test]
    fn build_oauth_client_succeeds_for_valid_config() {
        let cfg = XOAuthConfig {
            enabled: true,
            client_id: "client".into(),
            client_secret: Some("secret".into()),
            redirect_path: "/api/auth/login/x/callback".into(),
            scopes: vec!["tweet.read".into()],
            timeout_ms: 8000,
        };
        let client = build_oauth_client(&cfg, "https://example.com")
            .expect("valid config + public_url must build the client");
        let redirect = client
            .redirect_uri()
            .expect("redirect URI must be set")
            .to_string();
        assert_eq!(redirect, "https://example.com/api/auth/login/x/callback");
    }
}
