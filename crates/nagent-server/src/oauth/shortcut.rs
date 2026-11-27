//! Shortcut OAuth 2.0 (agent-app install) flow.
//!
//! Wire layer for the Shortcut OAuth dance — the per-workspace
//! install flow used by Shortcut "Open Agents" apps. Unlike the X
//! flow (which goes through the `oauth2` crate), the token
//! exchange here is a raw form POST: the Shortcut token response
//! carries a non-standard `permission_id` field (the agent app's
//! member id in the workspace) that the `oauth2` crate's standard
//! token type would drop.
//!
//! Wire surface:
//! - `GET  /api/auth/login/shortcut/start`     — mint PKCE + state, redirect to Shortcut.
//! - `GET  /api/auth/login/shortcut/callback`  — verify state, exchange code, validate the
//!   bearer against `GET {api_base_url}/members`, MERGE the OAuth fields into the `shortcut`
//!   vault service (the classic `api_token` / `username` / `default_team` rows survive).
//! - `POST /api/auth/login/shortcut/disconnect` — delete only the OAuth field rows.
//!
//! Refresh is handled by the `shortcut_*` chat agents themselves
//! (same locked decision as X: the agent refreshes on expiry / 401
//! through the `SecretSink`); this file only owns the
//! connect/disconnect half of the dance.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The access / refresh tokens only ever travel inside the
//! `Authorization` header or the vault ciphertext rows. Error
//! paths carry the upstream status + truncated reply body — never
//! the code, the PKCE verifier, or a token.

use std::sync::Arc;

use serde::Deserialize;

use crate::auth::error::AuthError;
use crate::auth::session::AuthUser;
use crate::config::shortcut_oauth::ShortcutOAuthConfig;
use crate::credentials::crypto::encrypt as vault_encrypt;
use crate::credentials::key::CredentialsKey;
use crate::oauth::pkce::PkcePair;
use crate::oauth::state::StateStore;

use nagent_db::NewAuthEvent;

/// Vault field keys owned by the OAuth flow. `disconnect` deletes
/// exactly these (the API-token fallback rows stay).
pub const OAUTH_FIELD_KEYS: &[&str] = &[
    "access_token",
    "refresh_token",
    "token_scope",
    "token_expires_at",
    "permission_id",
    "auth_mode",
];

#[derive(Clone)]
pub struct ShortcutOAuthState {
    pub cfg: Arc<ShortcutOAuthConfig>,
    pub state: StateStore,
    pub store: nagent_db::Db,
    pub key: Arc<CredentialsKey>,
    pub public_url: String,
    pub http: reqwest::Client,
}

impl std::fmt::Debug for ShortcutOAuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShortcutOAuthState")
            .field("cfg", &self.cfg)
            .field("state", &self.state)
            .field("store", &"<nagent_db::Db>")
            .field("key", &"<CredentialsKey>")
            .field("public_url", &self.public_url)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

pub fn build_state(
    cfg: Arc<ShortcutOAuthConfig>,
    store: nagent_db::Db,
    key: Arc<CredentialsKey>,
    public_url: String,
) -> Option<ShortcutOAuthState> {
    if !cfg.enabled || cfg.client_id.is_empty() {
        return None;
    }
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(cfg.timeout_ms.max(1_000)))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("reqwest client build");
    Some(ShortcutOAuthState {
        cfg,
        state: StateStore::new(),
        store,
        key,
        public_url,
        http,
    })
}

/// Build the Shortcut authorize URL: `authorize_url` +
/// `response_type=code`, `client_id`, `redirect_uri`, `state`,
/// PKCE `code_challenge` (S256), and optional `scope`. Factored
/// out of the handler so the query assembly is unit-testable.
pub fn build_authorize_url(
    cfg: &ShortcutOAuthConfig,
    public_url: &str,
    state_token: &str,
    pkce_challenge: &str,
) -> String {
    let mut pairs: Vec<(String, String)> = vec![
        ("response_type".to_string(), "code".to_string()),
        ("client_id".to_string(), cfg.client_id.clone()),
        ("redirect_uri".to_string(), cfg.redirect_url(public_url)),
        ("state".to_string(), state_token.to_string()),
        ("code_challenge".to_string(), pkce_challenge.to_string()),
        ("code_challenge_method".to_string(), "S256".to_string()),
    ];
    if !cfg.scopes.is_empty() {
        pairs.push(("scope".to_string(), cfg.scopes.join(" ")));
    }
    let qs = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .finish();
    let base = cfg.authorize_url.trim_end_matches('?');
    if base.contains('?') {
        format!("{base}&{qs}")
    } else {
        format!("{base}?{qs}")
    }
}

pub async fn start_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<AuthUser>,
) -> Result<axum::response::Response, AuthError> {
    let sc = auth
        .shortcut_oauth
        .as_ref()
        .ok_or_else(|| AuthError::Internal("Shortcut OAuth state not wired".into()))?;
    let user_id = auth_user.id;
    let pkce = PkcePair::generate();
    let state_token = sc.state.push(serde_json::json!({
        "user_id": user_id.to_string(),
        "pkce_verifier": pkce.verifier.as_str(),
        "created_at": chrono::Utc::now().to_rfc3339(),
    }));
    let authorize_url = build_authorize_url(&sc.cfg, &sc.public_url, &state_token, &pkce.challenge);
    tracing::info!(
        user_id = %user_id,
        "shortcut_oauth: redirecting browser to the Shortcut authorize URL"
    );
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, authorize_url)
        .body(axum::body::Body::empty())
        .expect("static response builder"))
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

pub async fn callback_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<AuthUser>,
    axum::extract::Query(q): axum::extract::Query<CallbackQuery>,
) -> Result<axum::response::Response, AuthError> {
    let sc = auth
        .shortcut_oauth
        .as_ref()
        .ok_or_else(|| AuthError::Internal("Shortcut OAuth state not wired".into()))?;
    let session_user_id = auth_user.id;
    let Some(state_token) = q.state.as_deref() else {
        return Ok(redirect_with_error("missing_state"));
    };
    let entry = match sc.state.pop(state_token) {
        Some(e) => e,
        None => {
            tracing::warn!("shortcut_oauth: state token absent (expired or never minted)");
            return Ok(redirect_with_error("expired_state"));
        }
    };
    let state_user_id_str = entry
        .payload
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AuthError::Internal("shortcut_oauth state payload missing user_id".into())
        })?;
    let state_user_id: uuid::Uuid = state_user_id_str
        .parse()
        .map_err(|e| AuthError::Internal(format!("shortcut_oauth user_id parse: {e}")))?;
    // Bind: the `/start` caller and the `/callback` caller MUST be
    // the same authenticated user (same check as the X flow).
    if state_user_id != session_user_id {
        tracing::warn!(
            session_user_id = %session_user_id,
            state_user_id = %state_user_id,
            "shortcut_oauth: /start and /callback user mismatch — refusing to bind tokens"
        );
        return Ok(redirect_with_error("user_mismatch"));
    }
    let pkce_verifier = entry
        .payload
        .get("pkce_verifier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AuthError::Internal("shortcut_oauth state payload missing pkce_verifier".into())
        })?
        .to_string();
    if let Some(err) = q.error.as_deref() {
        tracing::warn!(
            user_id = %session_user_id,
            error = err,
            description = q.error_description.as_deref().unwrap_or(""),
            "shortcut_oauth: Shortcut returned an error on /callback"
        );
        return Ok(redirect_with_error("oauth_error"));
    }
    let Some(code) = q.code.as_deref() else {
        return Ok(redirect_with_error("missing_code"));
    };

    let token = match exchange_code(sc, code, &pkce_verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(user_id = %session_user_id, error = %e, "shortcut_oauth: code exchange failed");
            return Ok(redirect_with_error("token_exchange_failed"));
        }
    };
    if let Err(e) = validate_token(sc, &token.access_token).await {
        tracing::warn!(
            user_id = %session_user_id,
            error = %e,
            "shortcut_oauth: fresh bearer rejected by GET /members"
        );
        return Ok(redirect_with_error("validation_failed"));
    }

    let expires_at =
        chrono::Utc::now() + chrono::Duration::seconds(token.expires_in.max(60) as i64);
    let rows = build_field_rows(
        sc,
        &token.access_token,
        token.refresh_token.as_deref(),
        token.scope.as_deref(),
        token.permission_id.as_deref(),
        expires_at,
    )?;
    // `merge_fields` (not `upsert`): the OAuth callback owns only
    // the OAuth rows of the shared `shortcut` service — the user's
    // `api_token` / `username` / `default_team` / `story_templates`
    // rows must survive the connect.
    sc.store
        .for_user(session_user_id)
        .credentials()
        .merge_fields("shortcut", &rows)
        .await
        .map_err(|e| AuthError::Internal(format!("shortcut_oauth credentials merge: {e}")))?;

    sc.store.admin().events.record(NewAuthEvent {
        user_id: Some(session_user_id),
        kind: "credential_oauth_shortcut_connected".to_string(),
        provider: "oauth_shortcut".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("shortcut".to_string()),
    });

    Ok(redirect_with_status("connected"))
}

pub async fn disconnect_handler(
    axum::extract::State(auth): axum::extract::State<crate::state::AuthState>,
    auth_user: axum::Extension<AuthUser>,
) -> Result<axum::response::Response, AuthError> {
    let sc = auth
        .shortcut_oauth
        .as_ref()
        .ok_or_else(|| AuthError::Internal("Shortcut OAuth state not wired".into()))?;
    let user_id = auth_user.id;
    // Delete only the OAuth rows; the classic `api_token` fallback
    // (and the non-credential rows) stay configured.
    let keys: Vec<String> = OAUTH_FIELD_KEYS.iter().map(|k| k.to_string()).collect();
    sc.store
        .for_user(user_id)
        .credentials()
        .delete_fields("shortcut", &keys)
        .await
        .map_err(|e| AuthError::Internal(format!("shortcut_oauth disconnect: {e}")))?;
    sc.store.admin().events.record(NewAuthEvent {
        user_id: Some(user_id),
        kind: "credential_oauth_shortcut_disconnected".to_string(),
        provider: "oauth_shortcut".to_string(),
        ip: None,
        user_agent: None,
        target_service: Some("shortcut".to_string()),
    });
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// Parsed Shortcut token-endpoint response. `permission_id` is the
/// non-standard field the agent-app flow adds (the app's member id
/// in the workspace); the `oauth2` crate would drop it, so the
/// exchange is a raw form POST parsed by hand.
#[derive(Debug, Clone)]
pub struct ShortcutTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub scope: Option<String>,
    pub permission_id: Option<String>,
}

/// Parse a token-endpoint response body. The body is upstream
/// data; on parse failure it is truncated in the error, never
/// echoed whole.
pub fn parse_token_response(body: &str) -> Result<ShortcutTokenResponse, String> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("parse: {e}; body={}", truncate(body, 256)))?;
    let access_token = parsed
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("no access_token in body={}", truncate(body, 256)))?
        .to_string();
    Ok(ShortcutTokenResponse {
        access_token,
        refresh_token: parsed
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        expires_in: parsed
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600),
        scope: parsed
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        permission_id: parsed
            .get("permission_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Exchange the authorization `code` for a token set at the
/// configured token endpoint (form POST, PKCE verifier included).
/// The code / verifier are sent as form fields and never appear in
/// the returned error string (AGENTS.md §6).
async fn exchange_code(
    state: &ShortcutOAuthState,
    code: &str,
    pkce_verifier: &str,
) -> Result<ShortcutTokenResponse, String> {
    let mut form = vec![
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
        .post(&state.cfg.token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("transport: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| format!("read: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "rejected (HTTP {}): {}",
            status.as_u16(),
            truncate(&body, 256)
        ));
    }
    parse_token_response(&body)
}

/// Validate the fresh bearer against `GET {api_base_url}/members`
/// so a broken install fails at connect time, not on first use.
async fn validate_token(state: &ShortcutOAuthState, bearer: &str) -> Result<(), String> {
    let url = format!("{}/members", state.cfg.api_base_url.trim_end_matches('/'));
    let resp = state
        .http
        .get(url)
        .bearer_auth(bearer)
        .header(reqwest::header::ACCEPT, "application/json")
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
    Ok(())
}

/// One vault-sealed credential field row: (field_key, nonce, ciphertext).
type FieldRow = (String, Vec<u8>, Vec<u8>);

fn build_field_rows(
    state: &ShortcutOAuthState,
    access_token: &str,
    refresh_token: Option<&str>,
    scope: Option<&str>,
    permission_id: Option<&str>,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<FieldRow>, AuthError> {
    let pairs: Vec<(&str, String)> = vec![
        ("access_token", access_token.to_string()),
        ("refresh_token", refresh_token.unwrap_or("").to_string()),
        ("token_scope", scope.unwrap_or("").to_string()),
        (
            "token_expires_at",
            expires_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        ("permission_id", permission_id.unwrap_or("").to_string()),
        ("auth_mode", "oauth".to_string()),
    ];
    let mut rows = Vec::with_capacity(pairs.len());
    for (field_key, plaintext) in pairs {
        let sealed = vault_encrypt(&state.key, &plaintext).map_err(|e| {
            AuthError::Internal(format!(
                "shortcut_oauth encrypt failed for field `{field_key}`: {e}"
            ))
        })?;
        rows.push((field_key.to_string(), sealed.nonce, sealed.ciphertext));
    }
    Ok(rows)
}

fn redirect_with_error(reason: &'static str) -> axum::response::Response {
    let url = format!("/settings/integrations?shortcut_error={reason}");
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FOUND)
        .header(axum::http::header::LOCATION, url)
        .body(axum::body::Body::empty())
        .expect("static redirect response")
}

fn redirect_with_status(reason: &'static str) -> axum::response::Response {
    let url = format!("/settings/integrations?shortcut_connected={reason}");
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

use axum::response::IntoResponse;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ShortcutOAuthConfig {
        ShortcutOAuthConfig {
            enabled: true,
            client_id: "sc-client-id".into(),
            client_secret: Some("sc-secret".into()),
            ..Default::default()
        }
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_redirect() {
        let cfg = cfg();
        let url = build_authorize_url(&cfg, "https://nagent.example.com/", "state123", "chal456");
        assert!(url.starts_with("https://app.shortcut.com/oauth/authorize?"));
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "sc-client-id");
        assert_eq!(
            q["redirect_uri"],
            "https://nagent.example.com/api/auth/login/shortcut/callback"
        );
        assert_eq!(q["state"], "state123");
        assert_eq!(q["code_challenge"], "chal456");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q.contains_key("scope"), "empty scopes must be omitted");
    }

    #[test]
    fn authorize_url_appends_scopes_when_configured() {
        let mut cfg = cfg();
        cfg.scopes = vec!["agent".to_string(), "webhook".to_string()];
        let url = build_authorize_url(&cfg, "https://n.example.com", "s", "c");
        let parsed = url::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["scope"], "agent webhook");
    }

    #[test]
    fn token_response_keeps_permission_id() {
        let body = r#"{
            "access_token": "at-123",
            "refresh_token": "rt-456",
            "token_type": "bearer",
            "expires_in": 7200,
            "scope": "agent",
            "permission_id": "member-uuid-9"
        }"#;
        let t = parse_token_response(body).expect("parse");
        assert_eq!(t.access_token, "at-123");
        assert_eq!(t.refresh_token.as_deref(), Some("rt-456"));
        assert_eq!(t.expires_in, 7200);
        assert_eq!(t.scope.as_deref(), Some("agent"));
        assert_eq!(t.permission_id.as_deref(), Some("member-uuid-9"));
    }

    #[test]
    fn token_response_tolerates_missing_optional_fields() {
        let t = parse_token_response(r#"{"access_token": "at"}"#).expect("parse");
        assert_eq!(t.access_token, "at");
        assert!(t.refresh_token.is_none());
        assert!(t.permission_id.is_none());
        assert_eq!(t.expires_in, 3600, "expires_in defaults to 1 h");
        assert!(parse_token_response(r#"{"error": "invalid_grant"}"#).is_err());
        assert!(parse_token_response("not json").is_err());
    }

    #[test]
    fn truncate_caps_at_byte_boundary() {
        let s = "é".repeat(10); // 20 bytes
        let t = truncate(&s, 3);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.starts_with("é"));
    }
}
