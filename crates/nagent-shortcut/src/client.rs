//! Shared Shortcut REST API v3 client.
//!
//! One [`ShortcutClient`] per consumer shares the caller's
//! `reqwest::Client`, so the connection pool / TLS roots stay
//! warm. The per-call policy still applies: the URL is validated
//! by the egress SSRF pre-flight + hostname allow-list before
//! every request (the allow-list lives in
//! [`crate::ShortcutConfig`], default `["api.app.shortcut.com"]`),
//! and the per-call timeout comes from `timeout_ms`.
//!
//! ## Headers
//!
//! Every request carries:
//! - `Shortcut-Token: <api_token>` — the token is a **parameter**
//!   of [`ShortcutClient::get`], supplied by the caller (the chat
//!   agents resolve it from the vault via `UserContext::secret`;
//!   the server dashboard endpoints read the vault directly);
//! - `Accept: application/json` + `Content-Type:
//!   application/json`;
//! - `User-Agent: nagent-shortcut/<version>`.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The token only ever travels inside the `Shortcut-Token`
//! header. No code path formats the header (or the token) into
//! an error string, a log line, or a `Debug` output —
//! [`redact_shortcut_token`] is the single helper that knows how
//! to render a header map safely, and the request builder never
//! builds a debug string from the header map at all. The
//! upstream response body is safe to surface (truncated), same
//! "server reply is safe, client credential is not" model as
//! the IMAP `redact_command` rule.
//!
//! ## Error mapping
//!
//! - 401 → [`ShortcutError::Unauthorized`] with a reconnect hint
//!   (a 401 means the token was revoked or is wrong).
//! - 429 → [`ShortcutError::RateLimited`] with the `Retry-After`
//!   hint (Shortcut's quota is 200 req/min per token).
//! - 404 → [`ShortcutError::Upstream`] (not found).
//! - any other non-2xx → [`ShortcutError::Upstream`] with the
//!   body truncated.
//! - transport / parse errors → [`ShortcutError::Transport`].

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use nagent_support::egress::{EgressClient, EgressConfig, EgressError};

use crate::{ShortcutConfig, ShortcutError};

/// Hard cap on a raw upstream response body (4 MiB). List
/// endpoints are capped by `limit`, so this only fires on a
/// pathological response; the JSON parse below never sees a
/// multi-megabyte string.
pub const MAX_RAW_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Cap on the upstream error body surfaced to the caller.
const ERROR_BODY_CAP: usize = 2048;

/// One cached response: `(fetched_at, payload)`.
type CacheEntry = (DateTime<Utc>, String);

/// Cache map keyed by `(user_id, tool_name, canonical_args_json)`.
type CacheMap = std::collections::HashMap<(uuid::Uuid, String, String), CacheEntry>;

/// Shared client behind every Shortcut API call. `Clone` is
/// cheap: the cache is an `Arc<Mutex<…>>` and the HTTP client is
/// an `Arc` internally, so cloned handles share both.
#[derive(Clone)]
pub struct ShortcutClient {
    pub cfg: ShortcutConfig,
    http: reqwest::Client,
    /// In-process response cache keyed by
    /// `(user_id, tool_name, canonical_args_json)`. The `Mutex`
    /// is short-held (read or write one entry), same pattern as
    /// `x_timeline` / `github`.
    cache: Arc<Mutex<CacheMap>>,
}

impl std::fmt::Debug for ShortcutClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShortcutClient")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .field(
                "cache",
                &format!(
                    "<entries={}>",
                    self.cache.lock().map(|g| g.len()).unwrap_or(0)
                ),
            )
            .finish()
    }
}

impl ShortcutClient {
    pub fn new(cfg: ShortcutConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            cache: Arc::new(Mutex::new(CacheMap::new())),
        }
    }

    fn build_egress(&self) -> EgressClient {
        EgressClient::new(EgressConfig {
            timeout_ms: self.cfg.timeout_ms,
            allow_public: true,
            allowlist: self.cfg.allowlist.clone(),
            max_bytes: MAX_RAW_RESPONSE_BYTES,
            user_agent: format!(
                "nagent-shortcut/{} (+https://github.com/nagent/nagent)",
                env!("CARGO_PKG_VERSION")
            ),
        })
    }

    /// Read the cached payload for `(user_id, tool, args)` when it
    /// is still fresh. `cache_ttl_secs = 0` always misses.
    pub fn cache_get(&self, user_id: uuid::Uuid, tool: &str, args: &Value) -> Option<String> {
        if self.cfg.cache_ttl_secs == 0 {
            return None;
        }
        let key = cache_key(user_id, tool, args);
        let Ok(g) = self.cache.lock() else {
            return None;
        };
        let (fetched_at, payload) = g.get(&key)?;
        let age = Utc::now().signed_duration_since(*fetched_at).num_seconds();
        if age < 0 || age >= self.cfg.cache_ttl_secs as i64 {
            return None;
        }
        Some(payload.clone())
    }

    /// Store `payload` under `(user_id, tool, args)`.
    pub fn cache_put(&self, user_id: uuid::Uuid, tool: &str, args: &Value, payload: String) {
        if self.cfg.cache_ttl_secs == 0 {
            return;
        }
        let key = cache_key(user_id, tool, args);
        if let Ok(mut g) = self.cache.lock() {
            g.insert(key, (Utc::now(), payload));
        }
    }

    /// Perform one authenticated GET against the Shortcut REST
    /// API v3 and return the parsed JSON body. `token` is the
    /// caller's API token (never logged, never formatted into an
    /// error); `path` is the API-relative path starting with `/`
    /// (e.g. `/members`); `query` is a list of `(key, value)`
    /// pairs, percent-encoded by [`build_url`].
    pub async fn get(
        &self,
        token: &SecretString,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, ShortcutError> {
        let url = build_url(&self.cfg.base_url, path, query);

        // SSRF pre-flight + hostname allow-list, same posture as
        // `github`. The allow-list defaults to
        // `["api.app.shortcut.com"]`.
        let egress = self.build_egress();
        let validated = egress.validate(&url).await.map_err(map_egress_error)?;

        let resp = self
            .http
            .get(validated.url.as_str())
            .timeout(std::time::Duration::from_millis(self.cfg.timeout_ms))
            .header("Shortcut-Token", token.expose_secret())
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::USER_AGENT,
                format!("nagent-shortcut/{}", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            // reqwest's error Display carries the URL but never
            // the request headers, so the token cannot leak here.
            .map_err(|e| ShortcutError::Transport(e.to_string()))?;

        let status = resp.status();
        let headers = resp.headers().clone();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| ShortcutError::Transport(format!("shortcut read: {e}")))?;
        if body_bytes.len() > MAX_RAW_RESPONSE_BYTES {
            return Err(ShortcutError::ResponseExceeded {
                budget: MAX_RAW_RESPONSE_BYTES,
            });
        }
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        if status.as_u16() == 401 {
            return Err(ShortcutError::Unauthorized(
                "Shortcut rejected the API token (HTTP 401); reconnect the Shortcut \
                 integration via /settings/integrations (generate a new token at \
                 app.shortcut.com/settings/account/api-tokens)"
                    .into(),
            ));
        }
        if status.as_u16() == 429 {
            return Err(ShortcutError::RateLimited {
                hint: rate_limit_hint(&headers),
            });
        }
        if !status.is_success() {
            return Err(ShortcutError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, ERROR_BODY_CAP),
            });
        }
        serde_json::from_str(&body)
            .map_err(|e| ShortcutError::Transport(format!("shortcut parse (HTTP {status}): {e}")))
    }
}

/// Canonical cache key: `(user_id, tool, args-as-json)`.
fn cache_key(user_id: uuid::Uuid, tool: &str, args: &Value) -> (uuid::Uuid, String, String) {
    let canonical = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    (user_id, tool.to_string(), canonical)
}

/// Human-readable retry hint built from `Retry-After`. Never
/// contains the token (response headers only).
fn rate_limit_hint(headers: &reqwest::header::HeaderMap) -> String {
    if let Some(retry_after) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        return format!("Shortcut rate limit exhausted (200 req/min); retry after {retry_after}s");
    }
    "Shortcut rate limit exhausted (200 req/min); wait a few minutes before retrying".to_string()
}

/// Build `base_url + path + ?query` with the path segments and
/// query values percent-encoded. Path slashes are preserved.
pub fn build_url(base_url: &str, path: &str, query: &[(String, String)]) -> String {
    let base = base_url.trim_end_matches('/');
    let encoded_path = path
        .trim_start_matches('/')
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/");
    let mut url = format!("{base}/{encoded_path}");
    if !query.is_empty() {
        let qs = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .finish();
        url.push('?');
        url.push_str(&qs);
    }
    url
}

/// Percent-encode one URL path segment (RFC 3986 unreserved
/// chars pass through, everything else becomes `%XX`). Spaces
/// encode as `%20` — NOT `+` (a literal `+` in a path is a plus,
/// not a space).
fn encode_path_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Char-boundary-safe truncation with a marker appended.
pub fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    out
}

/// Render a header map for a debug / log line with the
/// `Shortcut-Token` value replaced by `<redacted>` (AGENTS.md
/// §6 — the `redact_command` rule for HTTP). The request builder
/// itself never formats the header map; this helper exists for
/// the rare diagnostic path and for the redaction test.
pub fn redact_shortcut_token(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let v = if name.as_str().eq_ignore_ascii_case("shortcut-token") {
                "<redacted>".to_string()
            } else {
                value.to_str().unwrap_or("<binary>").to_string()
            };
            (name.as_str().to_string(), v)
        })
        .collect()
}

fn map_egress_error(e: EgressError) -> ShortcutError {
    match e {
        EgressError::Scheme(s) => {
            ShortcutError::InvalidArguments(format!("unsupported scheme `{s}`"))
        }
        EgressError::NoHost => ShortcutError::InvalidArguments("url has no host".into()),
        EgressError::NotInAllowlist { host } => ShortcutError::SandboxDenied(format!(
            "host `{host}` is not in the Shortcut allowlist (set [agents.shortcut].allowlist)"
        )),
        EgressError::DisallowedAddress { host, addr } => ShortcutError::SandboxDenied(format!(
            "host `{host}` resolves to a disallowed address ({addr})"
        )),
        EgressError::Dns(msg) => ShortcutError::Transport(format!("dns resolve: {msg}")),
        EgressError::Upstream { status, body } => ShortcutError::Upstream { status, body },
        EgressError::ResponseExceeded { budget } => ShortcutError::ResponseExceeded { budget },
        EgressError::Transport(msg) => ShortcutError::Transport(format!("transport: {msg}")),
        EgressError::UrlParse(msg) => ShortcutError::InvalidArguments(format!("url parse: {msg}")),
    }
}

// ---------------------------------------------------------------------------
// Dashboard helpers — pure-API reads used by the nagent-server
// `GET /api/integrations/shortcut/stories…` endpoints. They never
// go through the LLM; they share the client's response cache.
// ---------------------------------------------------------------------------

/// Tool name used for the dashboard list cache entries (kept
/// distinct from the LLM tool names so the two never collide).
const API_LIST_TOOL: &str = "api_list_stories";
/// Tool name used for the dashboard detail cache entries.
const API_STORY_TOOL: &str = "api_story_detail";

/// `GET /search/stories?query=…&page_size=…` projected for the
/// dashboard story list. `limit` is clamped to
/// `[1, cfg.max_items]`; `query` defaults to
/// `cfg.backlog_query` when empty. The result is cached under
/// `(user_id, "api_list_stories", args)`.
pub async fn list_stories(
    client: &ShortcutClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    query: &str,
    limit: usize,
) -> Result<Value, ShortcutError> {
    let query = if query.trim().is_empty() {
        client.cfg.backlog_query.clone()
    } else {
        query.trim().to_string()
    };
    let limit = limit.clamp(1, client.cfg.max_items);
    let args = serde_json::json!({ "query": query, "limit": limit });
    if let Some(cached) = client.cache_get(user_id, API_LIST_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "shortcut api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| ShortcutError::Transport(format!("shortcut cache decode: {e}")));
    }
    let q = vec![
        ("query".to_string(), query.clone()),
        ("page_size".to_string(), limit.to_string()),
    ];
    let raw = client.get(token, "/search/stories", &q).await?;
    let stories: Vec<Value> = raw
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(crate::project::project_story).collect())
        .unwrap_or_default();
    let out = serde_json::json!({
        "query": query,
        "total": raw.get("total"),
        "count": stories.len(),
        "stories": stories,
    });
    let encoded = serde_json::to_string(&out)
        .map_err(|e| ShortcutError::Transport(format!("shortcut encode: {e}")))?;
    client.cache_put(user_id, API_LIST_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| ShortcutError::Transport(format!("shortcut encode: {e}")))
}

/// `GET /stories/{id}` + `GET /stories/{id}/comments` projected
/// for the dashboard story detail: the full description (no LLM
/// body cap) and every comment. Cached under
/// `(user_id, "api_story_detail", args)`.
pub async fn get_story_detail(
    client: &ShortcutClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    story_id: u64,
) -> Result<Value, ShortcutError> {
    if story_id == 0 {
        return Err(ShortcutError::InvalidArguments(
            "story id must be a positive integer".into(),
        ));
    }
    let args = serde_json::json!({ "story_id": story_id });
    if let Some(cached) = client.cache_get(user_id, API_STORY_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "shortcut api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| ShortcutError::Transport(format!("shortcut cache decode: {e}")));
    }
    let story = client
        .get(token, &format!("/stories/{story_id}"), &[])
        .await?;
    let comments = client
        .get(token, &format!("/stories/{story_id}/comments"), &[])
        .await?;
    let out = serde_json::json!({
        "story": crate::project::project_story_detail(&story),
        "comments": comments
            .as_array()
            .map(|arr| arr.iter().map(crate::project::project_comment).collect::<Vec<Value>>())
            .unwrap_or_default(),
    });
    let encoded = serde_json::to_string(&out)
        .map_err(|e| ShortcutError::Transport(format!("shortcut encode: {e}")))?;
    client.cache_put(user_id, API_STORY_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| ShortcutError::Transport(format!("shortcut encode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_encodes_query_but_keeps_path_slashes() {
        let url = build_url(
            "https://api.app.shortcut.com/api/v3/",
            "/stories/123",
            &[("query".to_string(), "is:story is:backlog".to_string())],
        );
        assert_eq!(
            url,
            "https://api.app.shortcut.com/api/v3/stories/123?query=is%3Astory+is%3Abacklog"
        );
    }

    #[test]
    fn build_url_without_query_has_no_question_mark() {
        let url = build_url("https://api.app.shortcut.com/api/v3", "/members", &[]);
        assert_eq!(url, "https://api.app.shortcut.com/api/v3/members");
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let s = "aé€b".repeat(100);
        let t = truncate(&s, 10);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.len() <= 10 + "\n…[truncated]".len());
    }

    #[test]
    fn redact_shortcut_token_masks_the_token_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "shortcut-token",
            reqwest::header::HeaderValue::from_static("sc_secrettoken123"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        let rendered = redact_shortcut_token(&headers);
        let token = rendered
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("shortcut-token"))
            .map(|(_, v)| v.clone())
            .expect("shortcut-token header present");
        assert_eq!(token, "<redacted>");
        let joined = format!("{rendered:?}");
        assert!(!joined.contains("sc_secrettoken123"));
    }

    #[test]
    fn rate_limit_hint_prefers_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("30"),
        );
        assert!(rate_limit_hint(&headers).contains("30s"));
        let empty = reqwest::header::HeaderMap::new();
        assert!(rate_limit_hint(&empty).contains("200 req/min"));
    }

    #[tokio::test]
    async fn cache_roundtrip_respects_ttl() {
        let client = ShortcutClient::new(ShortcutConfig::default(), reqwest::Client::new());
        let user = uuid::Uuid::new_v4();
        let args = serde_json::json!({"limit": 10});
        assert!(client.cache_get(user, "tool", &args).is_none());
        client.cache_put(user, "tool", &args, "payload".to_string());
        assert_eq!(
            client.cache_get(user, "tool", &args).as_deref(),
            Some("payload")
        );
        // A zero TTL disables the cache entirely.
        let cfg = ShortcutConfig {
            cache_ttl_secs: 0,
            ..ShortcutConfig::default()
        };
        let client = ShortcutClient::new(cfg, reqwest::Client::new());
        client.cache_put(user, "tool", &args, "payload".to_string());
        assert!(client.cache_get(user, "tool", &args).is_none());
    }
}
