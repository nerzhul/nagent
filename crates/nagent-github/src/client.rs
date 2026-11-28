//! Shared GitHub REST API client.
//!
//! One [`GithubClient`] per consumer shares the caller's
//! `reqwest::Client`, so the connection pool / TLS roots stay
//! warm. The per-call policy still applies: the URL is validated
//! by the egress SSRF pre-flight + hostname allow-list before
//! every request (the allow-list lives in [`crate::GithubConfig`],
//! default `["api.github.com"]`), and the per-call timeout comes
//! from `timeout_ms`.
//!
//! ## Headers
//!
//! Every request carries:
//! - `Authorization: Bearer <personal_access_token>` — the token
//!   is a **parameter** of [`GithubClient::get`], supplied by the
//!   caller (the chat agents resolve it from the vault via
//!   `UserContext::secret`; the server dashboard endpoints read
//!   the vault directly);
//! - `Accept: application/vnd.github+json`,
//! - `X-GitHub-Api-Version: 2022-11-28`,
//! - `User-Agent: nagent-github/<version>` (GitHub rejects
//!   requests without a `User-Agent`).
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The token only ever travels inside the `Authorization` header.
//! No code path formats the header (or the token) into an error
//! string, a log line, or a `Debug` output —
//! [`redact_authorization`] is the single helper that knows how
//! to render a header map safely, and the request builder never
//! builds a debug string from the header map at all. The
//! upstream response body is safe to surface (truncated), same
//! "server reply is safe, client credential is not" model as
//! the IMAP `redact_command` rule.
//!
//! ## Error mapping
//!
//! - 401 → [`GithubError::Unauthorized`] with a reconnect hint
//!   (PATs do not expire unless configured to; a 401 means the
//!   token was revoked or wrong).
//! - 403 / 429 with `X-RateLimit-Remaining: 0` →
//!   [`GithubError::RateLimited`] with the `Retry-After` /
//!   `X-RateLimit-Reset` hint.
//! - 404 → [`GithubError::Upstream`] (not found).
//! - any other non-2xx → [`GithubError::Upstream`] with the body
//!   truncated.
//! - transport / parse errors → [`GithubError::Transport`].

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use nagent_support::egress::{EgressClient, EgressConfig, EgressError};

use crate::{GithubConfig, GithubError};

/// GitHub API version header value pinned by the client.
pub const GITHUB_API_VERSION: &str = "2022-11-28";

/// Hard cap on a raw upstream response body (4 MiB). List
/// endpoints are capped by `per_page`, so this only fires on a
/// pathological response; the JSON parse below never sees a
/// multi-megabyte string.
pub const MAX_RAW_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Cap on the upstream error body surfaced to the caller.
const ERROR_BODY_CAP: usize = 2048;

/// Hard cap on the number of repos in the per-user
/// `dashboard_repos` vault field (plan 1795686864347). Bounds
/// the fan-out of `GET /api/integrations/github/prs`.
pub const MAX_DASHBOARD_REPOS: usize = 50;

/// One cached response: `(fetched_at, payload)`.
type CacheEntry = (DateTime<Utc>, String);

/// Cache map keyed by
/// `(user_id, account_name, tool_name, canonical_args_json)` — the
/// account is part of the key so the same tool called through two
/// different tokens never shares a cache entry (plan
/// 1795793328177 §1.3).
type CacheMap = std::collections::HashMap<(uuid::Uuid, String, String, String), CacheEntry>;

/// Repo → account resolution cache entry: `(account_name_or_None_for_negative, fetched_at)`.
/// `None` means "every account answered 404" — negative results are
/// cached too because 404s count against the rate limit.
type RepoAccountEntry = (Option<String>, DateTime<Utc>);

/// Shared client behind every GitHub API call. `Clone` is
/// cheap: the cache is an `Arc<Mutex<…>>` and the HTTP client is
/// an `Arc` internally, so cloned handles share both.
#[derive(Clone)]
pub struct GithubClient {
    pub cfg: GithubConfig,
    http: reqwest::Client,
    /// In-process response cache keyed by
    /// `(user_id, account, tool_name, canonical_args_json)`. The
    /// `Mutex` is short-held (read or write one entry), same pattern
    /// as `x_timeline` / `shortcut`.
    cache: Arc<Mutex<CacheMap>>,
    /// In-memory `(user_id, "owner/repo") → account` map used by the
    /// repo-scoped tools and the dashboard to avoid re-probing which
    /// token can read a repo. Populated by successful repo-scoped
    /// calls and by the probe-repo endpoint; TTL = `cache_ttl_secs`.
    repo_accounts: Arc<Mutex<std::collections::HashMap<(uuid::Uuid, String), RepoAccountEntry>>>,
}

impl std::fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClient")
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

impl GithubClient {
    pub fn new(cfg: GithubConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            cache: Arc::new(Mutex::new(CacheMap::new())),
            repo_accounts: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn build_egress(&self) -> EgressClient {
        EgressClient::new(EgressConfig {
            timeout_ms: self.cfg.timeout_ms,
            allow_public: true,
            allowlist: self.cfg.allowlist.clone(),
            max_bytes: MAX_RAW_RESPONSE_BYTES,
            user_agent: format!(
                "nagent-github/{} (+https://github.com/nagent/nagent)",
                env!("CARGO_PKG_VERSION")
            ),
        })
    }

    /// Read the cached payload for `(user_id, account, tool, args)`
    /// when it is still fresh. `cache_ttl_secs = 0` always misses.
    pub fn cache_get(
        &self,
        user_id: uuid::Uuid,
        account: &str,
        tool: &str,
        args: &Value,
    ) -> Option<String> {
        if self.cfg.cache_ttl_secs == 0 {
            return None;
        }
        let key = cache_key(user_id, account, tool, args);
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

    /// Store `payload` under `(user_id, account, tool, args)`.
    pub fn cache_put(
        &self,
        user_id: uuid::Uuid,
        account: &str,
        tool: &str,
        args: &Value,
        payload: String,
    ) {
        if self.cfg.cache_ttl_secs == 0 {
            return;
        }
        let key = cache_key(user_id, account, tool, args);
        if let Ok(mut g) = self.cache.lock() {
            g.insert(key, (Utc::now(), payload));
        }
    }

    /// Read the cached repo → account resolution for
    /// `(user_id, "owner/repo")`. Returns `Some(Some(account))` on
    /// a positive hit, `Some(None)` on a cached negative (all
    /// accounts 404'd), `None` on miss / expired.
    pub fn repo_account_get(&self, user_id: uuid::Uuid, repo: &str) -> Option<Option<String>> {
        if self.cfg.cache_ttl_secs == 0 {
            return None;
        }
        let Ok(g) = self.repo_accounts.lock() else {
            return None;
        };
        let (account, fetched_at) = g.get(&(user_id, repo.to_string()))?;
        let age = Utc::now().signed_duration_since(*fetched_at).num_seconds();
        if age < 0 || age >= self.cfg.cache_ttl_secs as i64 {
            return None;
        }
        Some(account.clone())
    }

    /// Cache the repo → account resolution (positive `Some(account)`
    /// or negative `None`).
    pub fn repo_account_put(&self, user_id: uuid::Uuid, repo: &str, account: Option<String>) {
        if self.cfg.cache_ttl_secs == 0 {
            return;
        }
        if let Ok(mut g) = self.repo_accounts.lock() {
            g.insert((user_id, repo.to_string()), (account, Utc::now()));
        }
    }

    /// Perform one authenticated GET against the GitHub REST API
    /// and return the parsed JSON body. `token` is the caller's
    /// PAT (never logged, never formatted into an error); `path`
    /// is the API-relative path starting with `/` (e.g. `/user`);
    /// `query` is a list of `(key, value)` pairs, percent-encoded
    /// by [`build_url`].
    pub async fn get(
        &self,
        token: &SecretString,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value, GithubError> {
        self.get_raw(token, path, query).await.map(|(v, _)| v)
    }

    /// Like [`GithubClient::get`] but also returns the response
    /// headers — used by the account probe to read
    /// `X-OAuth-Scopes` (safe to display; the token is not).
    pub async fn get_raw(
        &self,
        token: &SecretString,
        path: &str,
        query: &[(String, String)],
    ) -> Result<(Value, reqwest::header::HeaderMap), GithubError> {
        let url = build_url(&self.cfg.base_url, path, query);

        // SSRF pre-flight + hostname allow-list, same posture as
        // `x_timeline`. The allow-list defaults to
        // `["api.github.com"]`; GHES deployments override both
        // `base_url` and `allowlist`.
        let egress = self.build_egress();
        let validated = egress.validate(&url).await.map_err(map_egress_error)?;

        let resp = self
            .http
            .get(validated.url.as_str())
            .timeout(std::time::Duration::from_millis(self.cfg.timeout_ms))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", token.expose_secret()),
            )
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header(
                reqwest::header::USER_AGENT,
                format!("nagent-github/{}", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            // reqwest's error Display carries the URL but never
            // the request headers, so the token cannot leak here.
            .map_err(|e| GithubError::Transport(e.to_string()))?;

        let status = resp.status();
        let headers = resp.headers().clone();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| GithubError::Transport(format!("github read: {e}")))?;
        if body_bytes.len() > MAX_RAW_RESPONSE_BYTES {
            return Err(GithubError::ResponseExceeded {
                budget: MAX_RAW_RESPONSE_BYTES,
            });
        }
        let body = String::from_utf8_lossy(&body_bytes).into_owned();

        if status.as_u16() == 401 {
            return Err(GithubError::Unauthorized(
                "GitHub rejected the personal access token (HTTP 401); reconnect the GitHub \
                 integration via /settings/integrations"
                    .into(),
            ));
        }
        if matches!(status.as_u16(), 403 | 429) && rate_limit_remaining_is_zero(&headers) {
            return Err(GithubError::RateLimited {
                hint: rate_limit_hint(&headers),
            });
        }
        if !status.is_success() {
            return Err(GithubError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, ERROR_BODY_CAP),
            });
        }
        let parsed = serde_json::from_str(&body)
            .map_err(|e| GithubError::Transport(format!("github parse (HTTP {status}): {e}")))?;
        Ok((parsed, headers))
    }

    /// `GET /user` returning the profile plus the `X-OAuth-Scopes`
    /// header (the scopes are safe to display per AGENTS.md §6; the
    /// token is not). Used by the account add/update endpoint to
    /// capture `login` + scopes when a token is probed.
    pub async fn get_user(
        &self,
        token: &SecretString,
    ) -> Result<(Value, Option<String>), GithubError> {
        let (v, headers) = self.get_raw(token, "/user", &[]).await?;
        let scopes = headers
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok((v, scopes))
    }
}

/// Canonical cache key: `(user_id, account, tool, args-as-json)`.
fn cache_key(
    user_id: uuid::Uuid,
    account: &str,
    tool: &str,
    args: &Value,
) -> (uuid::Uuid, String, String, String) {
    let canonical = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    (user_id, account.to_string(), tool.to_string(), canonical)
}

/// `true` when the response headers say the primary rate limit is
/// exhausted (`X-RateLimit-Remaining: 0`).
fn rate_limit_remaining_is_zero(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false)
}

/// Human-readable retry hint built from `Retry-After` /
/// `X-RateLimit-Reset`. Never contains the token (response
/// headers only).
fn rate_limit_hint(headers: &reqwest::header::HeaderMap) -> String {
    if let Some(retry_after) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        return format!("GitHub rate limit exhausted; retry after {retry_after}s");
    }
    if let Some(reset) = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        let at = DateTime::from_timestamp(reset, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| reset.to_string());
        return format!("GitHub rate limit exhausted; quota resets at {at}");
    }
    "GitHub rate limit exhausted; wait a few minutes before retrying".to_string()
}

/// Build `base_url + path + ?query` with the path segments and
/// query values percent-encoded. Path slashes are preserved
/// (the contents endpoint takes `src/main.rs` as one path).
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
/// not a space, and GitHub would not decode it as one).
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
/// `Authorization` value replaced by `<redacted>` (AGENTS.md
/// §6 — the `redact_command` rule for HTTP). The request builder
/// itself never formats the header map; this helper exists for
/// the rare diagnostic path and for the redaction test.
pub fn redact_authorization(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let v = if name == reqwest::header::AUTHORIZATION {
                "<redacted>".to_string()
            } else {
                value.to_str().unwrap_or("<binary>").to_string()
            };
            (name.as_str().to_string(), v)
        })
        .collect()
}

/// GitHub owner / repo name rules: 1-100 chars, alphanumeric or
/// `-` / `_` / `.`, no leading or trailing hyphen, no `..`.
fn valid_github_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 100 {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') || s.contains("..") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Validate a `"owner/repo"` full name (the shape of the
/// `dashboard_repos` vault entries, of the `?repo=` filter, and
/// of the probe-repo body). Returns the `(owner, repo)` pair
/// when valid. Validated **before any network call** so a
/// malformed value can never smuggle a path traversal into the
/// API path.
pub fn valid_repo_full_name(raw: &str) -> Option<(&str, &str)> {
    let (owner, repo) = raw.split_once('/')?;
    if !valid_github_name(owner) || !valid_github_name(repo) {
        return None;
    }
    Some((owner, repo))
}

/// Validate a single GitHub owner (user login or org) name — the
/// same rules as one half of `owner/repo`. Used by the
/// `owners/:owner/repos` dashboard endpoint before any network
/// call.
pub fn valid_owner_name(s: &str) -> bool {
    valid_github_name(s)
}

fn map_egress_error(e: EgressError) -> GithubError {
    match e {
        EgressError::Scheme(s) => {
            GithubError::InvalidArguments(format!("unsupported scheme `{s}`"))
        }
        EgressError::NoHost => GithubError::InvalidArguments("url has no host".into()),
        EgressError::NotInAllowlist { host } => GithubError::SandboxDenied(format!(
            "host `{host}` is not in the GitHub allowlist (set [agents.github].allowlist)"
        )),
        EgressError::DisallowedAddress { host, addr } => GithubError::SandboxDenied(format!(
            "host `{host}` resolves to a disallowed address ({addr})"
        )),
        EgressError::Dns(msg) => GithubError::Transport(format!("dns resolve: {msg}")),
        EgressError::Upstream { status, body } => GithubError::Upstream { status, body },
        EgressError::ResponseExceeded { budget } => GithubError::ResponseExceeded { budget },
        EgressError::Transport(msg) => GithubError::Transport(format!("transport: {msg}")),
        EgressError::UrlParse(msg) => GithubError::InvalidArguments(format!("url parse: {msg}")),
    }
}

// ---------------------------------------------------------------------------
// Dashboard / probe helpers — pure-API reads used by the nagent-server
// `GET /api/integrations/github/prs…` endpoints and by the setup-only
// `POST /api/integrations/github/probe-repo`. They never go through
// the LLM; they share the client's response cache.
// ---------------------------------------------------------------------------

/// Tool name used for the repo probe cache entries.
const API_PROBE_REPO_TOOL: &str = "api_probe_repo";
/// Tool name used for the dashboard list cache entries (kept
/// distinct from the LLM tool names so the two never collide).
const API_LIST_PRS_TOOL: &str = "api_list_prs";
/// Tool name used for the dashboard detail cache entries.
const API_PR_DETAIL_TOOL: &str = "api_pr_detail";
/// Tool name used for the repo-picker owners cache entries.
const API_OWNERS_TOOL: &str = "api_owners";
/// Tool name used for the repo-picker per-owner repos cache entries.
const API_OWNER_REPOS_TOOL: &str = "api_owner_repos";
/// Tool name used for the raw accessible-repo list cache entries, so
/// the owners list and every per-owner view share one upstream fetch
/// per TTL window.
const API_USER_REPOS_TOOL: &str = "api_user_repos";
/// Tool name used for the per-commit CI status cache entries.
const API_CI_TOOL: &str = "api_ci_status";

/// `GET /repos/{owner}/{repo}` projected to the small summary the
/// probe-repo endpoint returns. Cached under
/// `(user_id, "api_probe_repo", args)`. Used to verify that the
/// saved PAT actually has access to a repo before the repo is
/// appended to `dashboard_repos`.
pub async fn get_repo(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    owner: &str,
    repo: &str,
) -> Result<Value, GithubError> {
    let args = serde_json::json!({ "repo": format!("{owner}/{repo}") });
    if let Some(cached) = client.cache_get(user_id, account, API_PROBE_REPO_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "github api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
    }
    let raw = client
        .get(token, &format!("/repos/{owner}/{repo}"), &[])
        .await?;
    let out = crate::project::project_repo_summary(&raw);
    let encoded = serde_json::to_string(&out)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(
        user_id,
        account,
        API_PROBE_REPO_TOOL,
        &args,
        encoded.clone(),
    );
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// Parameters for [`list_prs_for_repo`].
pub struct RepoPrsQuery<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    /// `open` / `closed` / `all`.
    pub state: &'a str,
    pub per_page: usize,
    /// Skip the cache read for this call (fresh value still writes
    /// through).
    pub refresh: bool,
}

/// `GET /repos/{owner}/{repo}/pulls?state=…&per_page=…` projected
/// to dashboard rows (one [`crate::project::project_pr_row`] per
/// PR, with `repo` set to `owner/repo`). Cached under
/// `(user_id, "api_list_prs", args)` — unless `refresh` is set, in
/// which case the cache read is skipped (the fresh result still
/// writes through, so the next plain call sees it).
pub async fn list_prs_for_repo(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    q: &RepoPrsQuery<'_>,
) -> Result<Vec<Value>, GithubError> {
    let owner = q.owner;
    let repo = q.repo;
    let per_page = q.per_page.clamp(1, client.cfg.max_items.clamp(1, 100));
    let args = serde_json::json!({
        "repo": format!("{owner}/{repo}"),
        "state": q.state,
        "per_page": per_page,
    });
    if !q.refresh {
        if let Some(cached) = client.cache_get(user_id, account, API_LIST_PRS_TOOL, &args) {
            tracing::debug!(user_id = %user_id, "github api cache hit");
            return serde_json::from_str(&cached)
                .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
        }
    }
    let query = vec![
        ("state".to_string(), q.state.to_string()),
        ("per_page".to_string(), per_page.to_string()),
    ];
    let raw = client
        .get(token, &format!("/repos/{owner}/{repo}/pulls"), &query)
        .await?;
    let full_name = format!("{owner}/{repo}");
    let prs: Vec<Value> = raw
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|p| crate::project::project_pr_row(&full_name, p))
                .collect()
        })
        .unwrap_or_default();
    let encoded = serde_json::to_string(&prs)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(user_id, account, API_LIST_PRS_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// `GET /repos/{owner}/{repo}/pulls/{number}` projected to the
/// detail shape (additions / deletions / changed_files / comments
/// included — the dashboard detail pane, unlike the LLM tools,
/// is not body-capped by the caller). Cached under
/// `(user_id, "api_pr_detail", args)`.
pub async fn get_pr_detail(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Value, GithubError> {
    if number == 0 {
        return Err(GithubError::InvalidArguments(
            "PR number must be a positive integer".into(),
        ));
    }
    let args = serde_json::json!({
        "repo": format!("{owner}/{repo}"),
        "number": number,
    });
    if let Some(cached) = client.cache_get(user_id, account, API_PR_DETAIL_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "github api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
    }
    let raw = client
        .get(token, &format!("/repos/{owner}/{repo}/pulls/{number}"), &[])
        .await?;
    let out = crate::project::project_pr_detail(&format!("{owner}/{repo}"), &raw);
    let encoded = serde_json::to_string(&out)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(user_id, account, API_PR_DETAIL_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// Query for "every repo the token can read": `visibility=all` is
/// what surfaces **private** repos, and
/// `affiliation=owner,collaborator,organization_member` is what
/// surfaces repos where the user is an outside collaborator or an
/// org member — including private repos of orgs they are NOT a
/// visible member of (`GET /user/orgs` hides private memberships, and
/// `GET /orgs/{org}/repos` hides outside-collaborator repos).
/// `type` must NOT be sent here: GitHub answers 422 when `type` is
/// combined with `visibility` or `affiliation`.
const ACCESSIBLE_REPOS_QUERY: &[(&str, &str)] = &[
    ("visibility", "all"),
    ("affiliation", "owner,collaborator,organization_member"),
    ("sort", "pushed"),
    ("per_page", "100"),
];

/// Page size of the accessible-repo list (`per_page` above).
const REPO_LIST_PER_PAGE: usize = 100;
/// Max pages fetched for the accessible-repo list (10 × 100 = 1000
/// repos): enough for realistic accounts while keeping one refresh
/// within a handful of calls against the PAT hourly quota. A short
/// page (fewer than `per_page` items) ends the walk early.
const MAX_REPO_LIST_PAGES: usize = 50;

/// Fetch every repo the token can read (`pushed` desc), walking
/// `GET /user/repos` pages until a short page or
/// `MAX_REPO_LIST_PAGES`. The merged list is cached under
/// `(user_id, "api_user_repos", {})` so the owners list and all
/// per-owner views share one upstream fetch per TTL window. Used
/// both to derive the picker's owner list and as the single source
/// for `list_owner_repos`.
async fn accessible_repos(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
) -> Result<Vec<Value>, GithubError> {
    let args = serde_json::json!({});
    if let Some(cached) = client.cache_get(user_id, account, API_USER_REPOS_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "github api cache hit");
        return serde_json::from_str::<Value>(&cached)
            .map(|v| v.as_array().cloned().unwrap_or_default())
            .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
    }
    let mut all: Vec<Value> = Vec::new();
    for page in 1..=MAX_REPO_LIST_PAGES {
        let mut query: Vec<(String, String)> = ACCESSIBLE_REPOS_QUERY
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        query.push(("page".to_string(), page.to_string()));
        let raw = client.get(token, "/user/repos", &query).await?;
        let items = raw.as_array().cloned().unwrap_or_default();
        let count = items.len();
        all.extend(items);
        if count < REPO_LIST_PER_PAGE {
            break;
        }
    }
    let encoded = serde_json::to_string(&all)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(user_id, account, API_USER_REPOS_TOOL, &args, encoded);
    Ok(all)
}

/// `GET /user` + `GET /user/orgs` + `GET /user/repos?visibility=all…` —
/// the owners whose repos the dashboard repo picker can list: the
/// authenticated user's login first (`via: "self"`), then every org
/// they belong to (`via: "org"`, from `/user/orgs`), **plus every
/// owner of an accessible repo** (`via: "repo"` — the `/user/repos`
/// pass is what catches orgs with a private membership or
/// outside-collaborator private repos, which `/user/orgs` misses).
/// Cached under `(user_id, "api_owners", {})`.
///
/// Every entry comes from an access-scoped API call, so the picker
/// can only ever offer repos the saved PAT can actually read.
pub async fn list_owners(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
) -> Result<Value, GithubError> {
    let args = serde_json::json!({});
    if let Some(cached) = client.cache_get(user_id, account, API_OWNERS_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "github api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
    }
    let user = client.get(token, "/user", &[]).await?;
    let login = user.get("login").and_then(|v| v.as_str()).unwrap_or("");
    let orgs_raw = client
        .get(
            token,
            "/user/orgs",
            &[("per_page".to_string(), "100".to_string())],
        )
        .await?;
    let mut owners: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !login.is_empty() && seen.insert(login.to_string()) {
        owners.push(serde_json::json!({"login": login, "via": "self"}));
    }
    for o in orgs_raw.as_array().cloned().unwrap_or_default() {
        if let Some(l) = o.get("login").and_then(|v| v.as_str()) {
            if seen.insert(l.to_string()) {
                owners.push(serde_json::json!({
                    "login": l,
                    "avatar_url": o.get("avatar_url"),
                    "via": "org",
                }));
            }
        }
    }
    // Owners that only appear through accessible repos (private org
    // membership, outside collaborator) — the whole point of the
    // `/user/repos` pass.
    for repo in accessible_repos(client, token, user_id, account).await? {
        if let Some(l) = repo
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(|v| v.as_str())
        {
            if seen.insert(l.to_string()) {
                owners.push(serde_json::json!({
                    "login": l,
                    "avatar_url": repo.get("owner").and_then(|o| o.get("avatar_url")),
                    "via": "repo",
                }));
            }
        }
    }
    let out = serde_json::json!({
        "user": login,
        "owners": owners,
    });
    let encoded = serde_json::to_string(&out)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(user_id, account, API_OWNERS_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// One owner's repos (`pushed` desc, up to 10 pages of 100), projected
/// with [`crate::project::project_repo_summary`].
///
/// The single source is `GET /user/repos?visibility=all&affiliation=owner,collaborator,organization_member`
/// (paginated, see [`accessible_repos`]) filtered by
/// `owner.login == owner`: this covers the user's own
/// repos, org-member repos (private included) AND outside-
/// collaborator private repos of orgs the user is not a visible
/// member of — the cases `/orgs/{owner}/repos` and
/// `affiliation=owner` miss. GitHub enforces visibility on the
/// upstream call, so an owner the PAT cannot see simply yields an
/// empty list. Archived repos are dropped unless
/// `include_archived` is set (picker "show archived" checkbox).
/// Cached under `(user_id, "api_owner_repos", {owner, include_archived})`.
pub async fn list_owner_repos(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    owner: &str,
    include_archived: bool,
) -> Result<Value, GithubError> {
    if !valid_owner_name(owner) {
        return Err(GithubError::InvalidArguments(format!(
            "invalid owner name `{owner}`"
        )));
    }
    let args = serde_json::json!({ "owner": owner, "include_archived": include_archived });
    if let Some(cached) = client.cache_get(user_id, account, API_OWNER_REPOS_TOOL, &args) {
        tracing::debug!(user_id = %user_id, "github api cache hit");
        return serde_json::from_str(&cached)
            .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
    }
    let repos: Vec<Value> = accessible_repos(client, token, user_id, account)
        .await?
        .into_iter()
        .filter(|r| {
            r.get("owner")
                .and_then(|o| o.get("login"))
                .and_then(|v| v.as_str())
                .map(|l| l.eq_ignore_ascii_case(owner))
                .unwrap_or(false)
        })
        // Archived repos are hidden unless explicitly requested.
        .filter(|r| {
            include_archived || !r.get("archived").and_then(|a| a.as_bool()).unwrap_or(false)
        })
        .map(|r| crate::project::project_repo_summary(&r))
        .collect();
    let out = serde_json::json!({
        "owner": owner,
        "count": repos.len(),
        "repos": repos,
    });
    let encoded = serde_json::to_string(&out)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(
        user_id,
        account,
        API_OWNER_REPOS_TOOL,
        &args,
        encoded.clone(),
    );
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// Collapse a `check-runs` payload into one `(state, detail)` pair:
/// any queued / in-progress run → `pending`; else any completed run
/// with a failing conclusion → `failure`; else any success →
/// `success`; else `none` ("no checks"). The detail is a compact
/// per-conclusion count, e.g. `"3 checks: 2 success, 1 failure"`.
fn aggregate_check_runs(payload: &Value) -> (String, String) {
    let runs = payload
        .get("check_runs")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    if runs.is_empty() {
        return ("none".to_string(), "no check runs".to_string());
    }
    let mut pending = 0usize;
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for run in &runs {
        let status = run.get("status").and_then(|s| s.as_str()).unwrap_or("");
        match status {
            "queued" | "in_progress" | "waiting" | "requested" | "pending" => pending += 1,
            "completed" => {
                let conclusion = run
                    .get("conclusion")
                    .and_then(|c| c.as_str())
                    .unwrap_or("neutral");
                *counts.entry(conclusion.to_string()).or_insert(0) += 1;
            }
            _ => {}
        }
    }
    let failing: usize = [
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "startup_failure",
    ]
    .iter()
    .map(|k| counts.get(*k).copied().unwrap_or(0))
    .sum();
    let state = if pending > 0 {
        "pending"
    } else if failing > 0 {
        "failure"
    } else if counts.get("success").copied().unwrap_or(0) > 0 {
        "success"
    } else {
        "none"
    };
    let mut parts: Vec<String> = counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    let detail = format!("{} checks: {}", runs.len(), parts.join(", "));
    (state.to_string(), detail)
}

/// CI state of one commit SHA: `GET /repos/{o}/{r}/commits/{sha}/check-runs`
/// (GitHub Actions et al.), falling back to the combined
/// `…/commits/{sha}/status` API when the repo reports no check runs
/// (legacy external CI). Returns `{"state": success|failure|pending|none,
/// "detail": "…"}`; the caller adds the `url` (the PR's /checks tab).
/// Cached under `(user_id, "api_ci_status", {repo, sha})` — CI state
/// only changes when the head SHA changes, and the SHA is part of
/// the key. `refresh` skips the cache read (the fresh value still
/// writes through). A 404 (no access to the repo anymore) maps to
/// `{"state": "unknown"}` rather than an error so one dead repo
/// cannot fail the whole dashboard enrichment.
pub async fn ci_status(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    repo: &str,
    sha: &str,
    refresh: bool,
) -> Result<Value, GithubError> {
    let args = serde_json::json!({ "repo": repo, "sha": sha });
    if !refresh {
        if let Some(cached) = client.cache_get(user_id, account, API_CI_TOOL, &args) {
            tracing::debug!(user_id = %user_id, "github api cache hit");
            return serde_json::from_str(&cached)
                .map_err(|e| GithubError::Transport(format!("github cache decode: {e}")));
        }
    }
    let Some((owner, name)) = valid_repo_full_name(repo) else {
        return Err(GithubError::InvalidArguments(format!(
            "invalid owner/repo `{repo}`"
        )));
    };
    let out = match client
        .get(
            token,
            &format!("/repos/{owner}/{name}/commits/{sha}/check-runs"),
            &[("per_page".to_string(), "100".to_string())],
        )
        .await
    {
        Ok(payload) => {
            let (state, detail) = aggregate_check_runs(&payload);
            if state == "none" {
                // No check runs: try the combined status API.
                match client
                    .get(
                        token,
                        &format!("/repos/{owner}/{name}/commits/{sha}/status"),
                        &[],
                    )
                    .await
                {
                    Ok(status_payload) => {
                        let raw = status_payload
                            .get("state")
                            .and_then(|s| s.as_str())
                            .unwrap_or("none");
                        let state = match raw {
                            "success" => "success",
                            "failure" | "error" => "failure",
                            "pending" => "pending",
                            _ => "none",
                        };
                        let total = status_payload.get("total_count").cloned();
                        serde_json::json!({
                            "state": state,
                            "detail": match total {
                                Some(t) => format!("status API: {raw} ({t} statuses)"),
                                None => format!("status API: {raw}"),
                            },
                        })
                    }
                    Err(_) => serde_json::json!({ "state": "none", "detail": "no check runs" }),
                }
            } else {
                serde_json::json!({ "state": state, "detail": detail })
            }
        }
        Err(GithubError::Upstream { status: 404, .. }) => {
            serde_json::json!({ "state": "unknown", "detail": "no access to CI status" })
        }
        Err(e) => return Err(e),
    };
    let encoded = serde_json::to_string(&out)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))?;
    client.cache_put(user_id, account, API_CI_TOOL, &args, encoded.clone());
    serde_json::from_str(&encoded)
        .map_err(|e| GithubError::Transport(format!("github encode: {e}")))
}

/// Parameters for [`list_dashboard_prs`].
pub struct DashboardPrsQuery<'a> {
    /// The configured `dashboard_repos` entries.
    pub repos: &'a [String],
    /// Optional `"owner/repo"` filter (already validated by the
    /// caller against `repos`).
    pub filter: Option<&'a str>,
    /// `open` / `closed` / `all`.
    pub state: &'a str,
    /// Row cap after the merge (clamped to `[1, cfg.max_items]`).
    pub limit: usize,
    /// Attach a per-row `ci` object (see [`ci_status`]) — costs up to
    /// one extra cached call per returned row, so opt-in.
    pub enrich: bool,
    /// Skip the response-cache reads for this call (fresh values
    /// still write through) — used by the UI's Refresh button so a
    /// just-merged PR disappears immediately instead of lingering
    /// for up to `cache_ttl_secs`.
    pub refresh: bool,
}

/// Aggregate the open (or `state`-filtered) PRs of every repo in
/// `repos` — the body of `GET /api/integrations/github/prs`.
///
/// - `filter`, when set, restricts the fan-out to that one repo
///   (already validated by the caller against `repos`).
/// - Each repo is queried with `per_page = 100` (first page only);
///   the rows are merged, sorted by `updated_at` descending, and
///   truncated to `limit` (clamped to `[1, cfg.max_items]`).
/// - `enrich` attaches a `ci` object (`state` + `detail` + `url`) to
///   every returned row, fetched per head SHA after the truncation
///   (so at most `limit` extra calls, cached per SHA). A failing CI
///   lookup degrades to `{"state": "unknown"}` on that row only.
/// - A repo answering 404 / 403 (access revoked since it was
///   added) does NOT fail the whole call: it lands in
///   `repo_errors`. Token-level failures (401, rate limit,
///   transport) propagate as `Err` because every repo would fail
///   the same way.
pub async fn list_dashboard_prs(
    client: &GithubClient,
    token: &SecretString,
    user_id: uuid::Uuid,
    account: &str,
    q: &DashboardPrsQuery<'_>,
) -> Result<Value, GithubError> {
    let limit = q.limit.clamp(1, client.cfg.max_items.max(1));
    let selected: Vec<&String> = match q.filter {
        Some(f) => q.repos.iter().filter(|r| r.as_str() == f).collect(),
        None => q.repos.iter().collect(),
    };
    let mut prs: Vec<Value> = Vec::new();
    let mut repo_errors: Vec<Value> = Vec::new();
    for repo in selected {
        let Some((owner, name)) = valid_repo_full_name(repo) else {
            repo_errors.push(serde_json::json!({
                "repo": repo,
                "error": "invalid owner/repo entry in the saved dashboard list",
            }));
            continue;
        };
        let repo_q = RepoPrsQuery {
            owner,
            repo: name,
            state: q.state,
            per_page: 100,
            refresh: q.refresh,
        };
        match list_prs_for_repo(client, token, user_id, account, &repo_q).await {
            Ok(mut rows) => {
                for row in &mut rows {
                    if let Some(obj) = row.as_object_mut() {
                        obj.insert(
                            "account".into(),
                            serde_json::Value::String(account.to_string()),
                        );
                    }
                }
                prs.extend(rows);
            }
            Err(e @ GithubError::Unauthorized(_))
            | Err(e @ GithubError::RateLimited { .. })
            | Err(e @ GithubError::Transport(_))
            | Err(e @ GithubError::SandboxDenied(_))
            | Err(e @ GithubError::ResponseExceeded { .. }) => return Err(e),
            Err(GithubError::Upstream { status, body }) => {
                let msg = if status == 404 || status == 403 {
                    "no access to this repo with the saved token (or it does not exist)".to_string()
                } else {
                    format!("HTTP {status}: {body}")
                };
                repo_errors.push(serde_json::json!({ "repo": repo, "account": account, "status": status, "error": msg }));
            }
            Err(GithubError::InvalidArguments(msg)) => {
                repo_errors
                    .push(serde_json::json!({ "repo": repo, "account": account, "error": msg }));
            }
        }
    }
    // Defense in depth for `state=open`: upstream never returns
    // merged PRs for it, but a cached page can predate a merge —
    // drop rows flagged merged so the open list stays honest.
    if q.state == "open" {
        prs.retain(|p| !p.get("merged").and_then(|m| m.as_bool()).unwrap_or(false));
    }
    // ISO 8601 timestamps sort lexicographically; missing values
    // sink to the bottom.
    prs.sort_by(|a, b| {
        let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    let total = prs.len();
    prs.truncate(limit);
    if q.enrich {
        for pr in &mut prs {
            let repo = pr.get("repo").and_then(|r| r.as_str()).unwrap_or("");
            let sha = pr.get("head_sha").and_then(|s| s.as_str()).unwrap_or("");
            let mut ci =
                match ci_status(client, token, user_id, account, repo, sha, q.refresh).await {
                    Ok(v) => v,
                    Err(_) => serde_json::json!({ "state": "unknown", "detail": "unavailable" }),
                };
            if let Some(obj) = ci.as_object_mut() {
                if let Some(html) = pr.get("html_url").and_then(|u| u.as_str()) {
                    obj.insert(
                        "url".into(),
                        serde_json::Value::String(format!("{html}/checks")),
                    );
                }
            }
            if let Some(obj) = pr.as_object_mut() {
                obj.insert("ci".into(), ci);
            }
        }
    }
    Ok(serde_json::json!({
        "repos": q.repos,
        "filter": q.filter,
        "state": q.state,
        "total": total,
        "count": prs.len(),
        "prs": prs,
        "repo_errors": repo_errors,
    }))
}

// ---------------------------------------------------------------------------
// Multi-account fan-out (plan 1795793328177): named accounts, repo →
// account resolution, and the cross-account dashboard aggregation.
// ---------------------------------------------------------------------------

/// One resolved `(name, token)` pair for the fan-out helpers.
pub struct AccountToken<'a> {
    pub name: &'a str,
    pub token: &'a SecretString,
}

/// Resolve which account can read `"owner/repo"` (plan
/// 1795793328177 §2.3):
///
/// 1. the client's in-memory `(user_id, repo) → account` map (fed by
///    the probe-repo endpoint and by previous successful calls);
/// 2. `persisted` — the `github.repo_accounts` vault map
///    (`"owner/repo" → account`), seeded into the in-memory map;
/// 3. otherwise try every account in order (default first), first
///    non-404 wins; the result (positive AND negative) is cached in
///    memory for `cache_ttl_secs` because 404s count against the
///    rate limit.
///
/// On total miss returns `GithubError::Upstream { status: 404 }`
/// whose body lists the account **names** tried — never tokens.
pub async fn resolve_repo_account(
    client: &GithubClient,
    accounts: &[AccountToken<'_>],
    user_id: uuid::Uuid,
    repo: &str,
    persisted: Option<&std::collections::HashMap<String, String>>,
) -> Result<(String, SecretString), GithubError> {
    if let Some(hit) = client.repo_account_get(user_id, repo) {
        return match hit {
            Some(name) => find_account(accounts, &name).map_err(|_| {
                GithubError::InvalidArguments(format!(
                    "repo `{repo}` is mapped to account `{name}` which no longer exists"
                ))
            }),
            None => Err(GithubError::Upstream {
                status: 404,
                body: format!(
                    "no configured account can read `{repo}` (tried: {})",
                    accounts
                        .iter()
                        .map(|a| a.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        };
    }
    if let Some(map) = persisted {
        if let Some(name) = map.get(repo) {
            client.repo_account_put(user_id, repo, Some(name.clone()));
            return find_account(accounts, name).map_err(|_| {
                GithubError::InvalidArguments(format!(
                    "repo `{repo}` is mapped to account `{name}` which no longer exists"
                ))
            });
        }
    }
    let Some((owner, name)) = valid_repo_full_name(repo) else {
        return Err(GithubError::InvalidArguments(format!(
            "invalid owner/repo `{repo}`"
        )));
    };
    // First non-404 account that can read the repo wins. 404 means
    // "cannot read"; 401/403 (revoked / forbidden token) also count
    // as "cannot read" for this account so one dead token does not
    // hide a repo another account can see — the first non-404
    // *error* is remembered and surfaced when NO account succeeds.
    let mut saw_404 = false;
    let mut first_non_404_err: Option<GithubError> = None;
    for acc in accounts {
        match client
            .get(acc.token, &format!("/repos/{owner}/{name}"), &[])
            .await
        {
            Ok(_) => {
                client.repo_account_put(user_id, repo, Some(acc.name.to_string()));
                return Ok((acc.name.to_string(), acc.token.clone()));
            }
            Err(GithubError::Upstream { status: 404, .. }) => {
                saw_404 = true;
                continue;
            }
            // 401 (revoked token → `Unauthorized`) and 403 count as
            // "this account cannot read the repo" — one dead token
            // must not hide a repo another account can see.
            Err(e @ GithubError::Unauthorized(_))
            | Err(e @ GithubError::Upstream { status: 403, .. }) => {
                if first_non_404_err.is_none() {
                    first_non_404_err = Some(e);
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    // Negative result is cached too (404s burn quota).
    client.repo_account_put(user_id, repo, None);
    // When at least one account answered 404 the repo is genuinely
    // invisible → the 404 listing the account names wins; when no
    // account 404'd (e.g. every token revoked) the credential error
    // is the useful signal.
    if !saw_404 {
        if let Some(e) = first_non_404_err {
            return Err(e);
        }
    }
    Err(GithubError::Upstream {
        status: 404,
        body: format!(
            "no configured account can read `{repo}` (tried: {})",
            accounts
                .iter()
                .map(|a| a.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Find an account by name in the fan-out slice.
fn find_account(
    accounts: &[AccountToken<'_>],
    name: &str,
) -> Result<(String, SecretString), GithubError> {
    accounts
        .iter()
        .find(|a| a.name == name)
        .map(|a| (a.name.to_string(), a.token.clone()))
        .ok_or_else(|| {
            GithubError::InvalidArguments(format!(
                "unknown GitHub account `{name}` (configured: {})",
                accounts
                    .iter()
                    .map(|a| a.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Parameters for [`list_dashboard_prs_multi`] — same as
/// [`DashboardPrsQuery`] plus the persisted repo → account map.
pub struct MultiDashboardPrsQuery<'a> {
    /// The configured `dashboard_repos` entries.
    pub repos: &'a [String],
    /// Optional `"owner/repo"` filter (already validated by the
    /// caller against `repos`).
    pub filter: Option<&'a str>,
    /// `open` / `closed` / `all`.
    pub state: &'a str,
    /// Row cap after the merge (clamped to `[1, cfg.max_items]`).
    pub limit: usize,
    /// Attach a per-row `ci` object — costs up to one extra cached
    /// call per returned row, so opt-in.
    pub enrich: bool,
    /// Skip the response-cache reads for this call.
    pub refresh: bool,
    /// The persisted `github.repo_accounts` map
    /// (`"owner/repo" → account name`), when present.
    pub repo_accounts: Option<&'a std::collections::HashMap<String, String>>,
}

/// Cross-account aggregation of the dashboard PR list (plan
/// 1795793328177 §3): every configured repo is resolved to the
/// account that can read it (via the persisted `repo_accounts` map,
/// else live resolution — see [`resolve_repo_account`]), fetched
/// with that account's token, and the rows are tagged
/// `"account": "<name>"`, merged by `updated_at` desc and truncated
/// to `limit`. Per-repo failures land in `repo_errors` (each entry
/// carries the `account` that failed); token-level failures of the
/// *resolving* account propagate. A repo visible through two
/// accounts appears once (deduped by `repo` + `number`, first
/// account in the list wins).
pub async fn list_dashboard_prs_multi(
    client: &GithubClient,
    accounts: &[AccountToken<'_>],
    user_id: uuid::Uuid,
    q: &MultiDashboardPrsQuery<'_>,
) -> Result<Value, GithubError> {
    let limit = q.limit.clamp(1, client.cfg.max_items.max(1));
    let selected: Vec<&String> = match q.filter {
        Some(f) => q.repos.iter().filter(|r| r.as_str() == f).collect(),
        None => q.repos.iter().collect(),
    };
    let mut prs: Vec<Value> = Vec::new();
    let mut repo_errors: Vec<Value> = Vec::new();
    for repo in selected {
        let Some((owner, name)) = valid_repo_full_name(repo) else {
            repo_errors.push(serde_json::json!({
                "repo": repo,
                "error": "invalid owner/repo entry in the saved dashboard list",
            }));
            continue;
        };
        // Resolve the account that can read this repo, then fetch
        // the PRs with that account's token.
        let (account, token) =
            match resolve_repo_account(client, accounts, user_id, repo, q.repo_accounts).await {
                Ok(pair) => pair,
                Err(GithubError::Upstream { status: 404, body }) => {
                    repo_errors.push(serde_json::json!({
                        "repo": repo,
                        "status": 404,
                        "error": body,
                    }));
                    continue;
                }
                Err(e) => return Err(e),
            };
        let repo_q = RepoPrsQuery {
            owner,
            repo: name,
            state: q.state,
            per_page: 100,
            refresh: q.refresh,
        };
        match list_prs_for_repo(client, &token, user_id, &account, &repo_q).await {
            Ok(mut rows) => {
                for row in &mut rows {
                    if let Some(obj) = row.as_object_mut() {
                        obj.insert("account".into(), serde_json::Value::String(account.clone()));
                    }
                }
                prs.extend(rows);
            }
            Err(e @ GithubError::Unauthorized(_))
            | Err(e @ GithubError::RateLimited { .. })
            | Err(e @ GithubError::Transport(_))
            | Err(e @ GithubError::SandboxDenied(_))
            | Err(e @ GithubError::ResponseExceeded { .. }) => return Err(e),
            Err(GithubError::Upstream { status, body }) => {
                let msg = if status == 404 || status == 403 {
                    "no access to this repo with this account's token (or it does not exist)"
                        .to_string()
                } else {
                    format!("HTTP {status}: {body}")
                };
                repo_errors.push(serde_json::json!({
                    "repo": repo,
                    "account": account,
                    "status": status,
                    "error": msg,
                }));
            }
            Err(GithubError::InvalidArguments(msg)) => {
                repo_errors
                    .push(serde_json::json!({ "repo": repo, "account": account, "error": msg }));
            }
        }
    }
    if q.state == "open" {
        prs.retain(|p| !p.get("merged").and_then(|m| m.as_bool()).unwrap_or(false));
    }
    // Dedupe by (repo, number): the same repo reachable through two
    // accounts appears once — first account in the list wins.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    prs.retain(|p| {
        let key = format!(
            "{}:{}",
            p.get("repo").and_then(|r| r.as_str()).unwrap_or(""),
            p.get("number").map(|n| n.to_string()).unwrap_or_default()
        );
        seen.insert(key)
    });
    prs.sort_by(|a, b| {
        let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    let total = prs.len();
    prs.truncate(limit);
    if q.enrich {
        for pr in &mut prs {
            let repo = pr.get("repo").and_then(|r| r.as_str()).unwrap_or("");
            let sha = pr.get("head_sha").and_then(|s| s.as_str()).unwrap_or("");
            let account = pr.get("account").and_then(|a| a.as_str()).unwrap_or("");
            let token = match find_account(accounts, account) {
                Ok((_, t)) => t,
                Err(_) => {
                    pr.as_object_mut().map(|obj| {
                        obj.insert(
                            "ci".into(),
                            serde_json::json!({"state": "unknown", "detail": "account gone"}),
                        )
                    });
                    continue;
                }
            };
            let mut ci =
                match ci_status(client, &token, user_id, account, repo, sha, q.refresh).await {
                    Ok(v) => v,
                    Err(_) => serde_json::json!({ "state": "unknown", "detail": "unavailable" }),
                };
            if let Some(obj) = ci.as_object_mut() {
                if let Some(html) = pr.get("html_url").and_then(|u| u.as_str()) {
                    obj.insert(
                        "url".into(),
                        serde_json::Value::String(format!("{html}/checks")),
                    );
                }
            }
            if let Some(obj) = pr.as_object_mut() {
                obj.insert("ci".into(), ci);
            }
        }
    }
    Ok(serde_json::json!({
        "repos": q.repos,
        "filter": q.filter,
        "state": q.state,
        "total": total,
        "count": prs.len(),
        "prs": prs,
        "repo_errors": repo_errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_encodes_query_but_keeps_path_slashes() {
        let url = build_url(
            "https://api.github.com/",
            "/repos/octo/hello world/contents/src/main.rs",
            &[("q".to_string(), "author:@me is:open repo:a/b".to_string())],
        );
        assert!(
            url.starts_with("https://api.github.com/repos/octo/hello%20world/contents/src/main.rs")
        );
        assert!(url.contains("q=author%3A%40me+is%3Aopen+repo%3Aa%2Fb"));
    }

    #[test]
    fn build_url_without_query_has_no_question_mark() {
        let url = build_url("https://api.github.com", "/user", &[]);
        assert_eq!(url, "https://api.github.com/user");
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        let s = "aé€b".repeat(100);
        let t = truncate(&s, 10);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.len() <= 10 + "\n…[truncated]".len());
    }

    #[test]
    fn aggregate_check_runs_maps_to_pending_failure_success_none() {
        let runs = |list: serde_json::Value| serde_json::json!({ "total_count": list.as_array().unwrap().len(), "check_runs": list });
        let run = |status: &str, conclusion: Option<&str>| {
            let mut r = serde_json::json!({ "status": status });
            if let Some(c) = conclusion {
                r["conclusion"] = serde_json::Value::String(c.to_string());
            }
            r
        };
        assert_eq!(aggregate_check_runs(&runs(serde_json::json!([]))).0, "none");
        assert_eq!(
            aggregate_check_runs(&runs(serde_json::json!([run("in_progress", None)]))).0,
            "pending"
        );
        assert_eq!(
            aggregate_check_runs(&runs(serde_json::json!([
                run("completed", Some("success")),
                run("completed", Some("failure")),
            ])))
            .0,
            "failure"
        );
        let (state, detail) = aggregate_check_runs(&runs(serde_json::json!([
            run("completed", Some("success")),
            run("completed", Some("success")),
            run("completed", Some("neutral")),
        ])));
        assert_eq!(state, "success");
        // counts are sorted by conclusion name (BTreeMap)
        assert!(
            detail.contains("3 checks: 1 neutral, 2 success"),
            "{detail}"
        );
        // A pending run wins even when a failure is also present.
        assert_eq!(
            aggregate_check_runs(&runs(serde_json::json!([
                run("completed", Some("failure")),
                run("queued", None),
            ])))
            .0,
            "pending"
        );
    }

    #[test]
    fn redact_authorization_masks_the_bearer_token() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_static("Bearer ghp_secrettoken123"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        let rendered = redact_authorization(&headers);
        let auth = rendered
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.clone())
            .expect("authorization header present");
        assert_eq!(auth, "<redacted>");
        let joined = format!("{rendered:?}");
        assert!(!joined.contains("ghp_secrettoken123"));
    }

    #[test]
    fn rate_limit_hint_prefers_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "retry-after",
            reqwest::header::HeaderValue::from_static("30"),
        );
        headers.insert(
            "x-ratelimit-remaining",
            reqwest::header::HeaderValue::from_static("0"),
        );
        assert!(rate_limit_remaining_is_zero(&headers));
        assert!(rate_limit_hint(&headers).contains("30s"));
    }

    #[test]
    fn rate_limit_detection_requires_remaining_zero() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-ratelimit-remaining",
            reqwest::header::HeaderValue::from_static("4999"),
        );
        assert!(!rate_limit_remaining_is_zero(&headers));
    }

    #[test]
    fn valid_repo_full_name_accepts_canonical_shapes() {
        assert_eq!(
            valid_repo_full_name("octo/hello-world"),
            Some(("octo", "hello-world"))
        );
        assert_eq!(
            valid_repo_full_name("Org_123/repo.name_v2"),
            Some(("Org_123", "repo.name_v2"))
        );
    }

    #[test]
    fn valid_repo_full_name_rejects_malformed_values() {
        for bad in [
            "noslash",
            "a/b/c",
            "../etc",
            "-bad/repo",
            "owner/",
            "",
            "/repo",
            "owner/repo extra",
            "owner/..",
        ] {
            assert!(
                valid_repo_full_name(bad).is_none(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn valid_owner_name_accepts_logins_and_orgs() {
        assert!(valid_owner_name("octo"));
        assert!(valid_owner_name("Org_123"));
        for bad in ["", "-bad", "a/b", "x..y", "with space"] {
            assert!(!valid_owner_name(bad), "expected `{bad}` rejected");
        }
    }

    #[tokio::test]
    async fn cache_roundtrip_respects_ttl() {
        let client = GithubClient::new(GithubConfig::default(), reqwest::Client::new());
        let user = uuid::Uuid::new_v4();
        let args = serde_json::json!({"limit": 10});
        assert!(client.cache_get(user, "default", "tool", &args).is_none());
        client.cache_put(user, "default", "tool", &args, "payload".to_string());
        assert_eq!(
            client.cache_get(user, "default", "tool", &args).as_deref(),
            Some("payload")
        );
        // The account is part of the cache key: the same tool+args
        // under another account misses.
        assert!(client.cache_get(user, "client-a", "tool", &args).is_none());
        // A zero TTL disables the cache entirely.
        let cfg = GithubConfig {
            cache_ttl_secs: 0,
            ..GithubConfig::default()
        };
        let client = GithubClient::new(cfg, reqwest::Client::new());
        client.cache_put(user, "default", "tool", &args, "payload".to_string());
        assert!(client.cache_get(user, "default", "tool", &args).is_none());
    }

    #[test]
    fn repo_account_cache_roundtrip_positive_and_negative() {
        let client = GithubClient::new(GithubConfig::default(), reqwest::Client::new());
        let user = uuid::Uuid::new_v4();
        assert!(client.repo_account_get(user, "octo/hello").is_none());
        client.repo_account_put(user, "octo/hello", Some("client-a".to_string()));
        assert_eq!(
            client.repo_account_get(user, "octo/hello"),
            Some(Some("client-a".to_string()))
        );
        // A negative (all accounts 404) is cached too.
        client.repo_account_put(user, "octo/gone", None);
        assert_eq!(client.repo_account_get(user, "octo/gone"), Some(None));
        // Another user / repo misses.
        assert!(client
            .repo_account_get(uuid::Uuid::new_v4(), "octo/hello")
            .is_none());
    }
}
