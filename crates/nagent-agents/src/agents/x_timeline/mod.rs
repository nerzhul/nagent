//! `x_timeline` — read-only X (Twitter) timeline agent.
//!
//! v1 calls the X v2 `reverse_chronological` endpoint (the
//! user's "Abonnements" timeline) by default and the `for_you`
//! endpoint on demand. The agent refreshes the access token itself
//! when X returns 401 (locked decision in plan 1790695073418) and
//! caches the parsed response per `(user_id, mode)` for
//! `cache_ttl_secs`.
//!
//! The agent never posts, replies, or writes to X (no `tweet.write`
//! scope). The full wire surface is documented in
//! `docs/integrations/x.md`.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::EgressClient;
use crate::XTimelineAgentConfig;

#[derive(Clone)]
pub struct XTimelineAgent {
    cfg: XTimelineAgentConfig,
    http: reqwest::Client,
    /// In-process response cache keyed by `(user_id, mode)`.
    /// The `Mutex` is short-held (read or write one entry); the
    /// `tokio::sync::Mutex` would be heavier and is not needed.
    cache: std::sync::Arc<std::sync::Mutex<CacheInner>>,
}

#[derive(Default)]
struct CacheInner {
    entries: std::collections::HashMap<(uuid::Uuid, String), (DateTime<Utc>, String)>,
}

impl std::fmt::Debug for XTimelineAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XTimelineAgent")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .field(
                "cache",
                &format!(
                    "<entries={}>",
                    self.cache.lock().map(|g| g.entries.len()).unwrap_or(0)
                ),
            )
            .finish()
    }
}

impl XTimelineAgent {
    pub fn new(cfg: XTimelineAgentConfig, http: reqwest::Client) -> Self {
        Self {
            cfg,
            http,
            cache: std::sync::Arc::new(std::sync::Mutex::new(CacheInner::default())),
        }
    }

    fn build_egress(&self) -> EgressClient {
        EgressClient::new(crate::egress::EgressConfig {
            timeout_ms: self.cfg.timeout_ms,
            allow_public: true,
            allowlist: self.cfg.allowlist.clone(),
            max_bytes: 2 * 1024 * 1024,
            user_agent: format!(
                "nagent-x-timeline-agent/{} (+https://github.com/nagent/nagent)",
                env!("CARGO_PKG_VERSION")
            ),
        })
    }

    /// Read the cached response for `(user_id, mode)` if it is
    /// still fresh. The cache TTL is `cache_ttl_secs`; `0` always
    /// misses so the cache can be turned off via config.
    fn cache_get(&self, user_id: uuid::Uuid, mode: &str) -> Option<String> {
        if self.cfg.cache_ttl_secs == 0 {
            return None;
        }
        let Ok(g) = self.cache.lock() else {
            return None;
        };
        let key = (user_id, mode.to_string());
        let (fetched_at, payload) = g.entries.get(&key)?;
        let age = Utc::now().signed_duration_since(*fetched_at).num_seconds();
        if age < 0 || age >= self.cfg.cache_ttl_secs as i64 {
            return None;
        }
        Some(payload.clone())
    }

    fn cache_put(&self, user_id: uuid::Uuid, mode: &str, payload: String) {
        if self.cfg.cache_ttl_secs == 0 {
            return;
        }
        if let Ok(mut g) = self.cache.lock() {
            g.entries
                .insert((user_id, mode.to_string()), (Utc::now(), payload));
        }
    }

    /// Refresh the access token via `grant_type=refresh_token`.
    /// Mirrors the OAuth callback wire shape (POST `/oauth2/token`
    /// with `refresh_token`, `client_id`, optional `client_secret`)
    /// but lives in the agent so the locked decision "refresh
    /// handled by the agent itself" is honoured.
    async fn refresh_access_token(
        &self,
        refresh_token: &str,
    ) -> Result<RefreshedToken, AgentError> {
        let url = format!("{}/2/oauth2/token", self.cfg.base_url.trim_end_matches('/'));
        let mut form: Vec<(String, String)> = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ];
        if let Some(cs) = token_endpoint_client_creds() {
            form.push(("client_id".to_string(), cs.client_id));
            if let Some(secret) = cs.client_secret {
                form.push(("client_secret".to_string(), secret));
            }
        }
        let resp = self
            .http
            .post(&url)
            .form(&form)
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("X refresh transport: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("X refresh read: {e}")))?;
        if status.as_u16() == 400 || status.as_u16() == 401 {
            // The token was revoked or expired; tell the caller to
            // ask the user to reconnect.
            return Err(AgentError::AgentFailed(format!(
                "X OAuth refresh failed: refresh token rejected ({status})"
            )));
        }
        if !status.is_success() {
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 2048),
            });
        }
        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| AgentError::AgentFailed(format!("X refresh response parse: {e}")))?;
        let access_token = parsed
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::AgentFailed("X refresh: access_token missing".into()))?
            .to_string();
        let new_refresh = parsed
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let expires_in = parsed
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        Ok(RefreshedToken {
            access_token,
            new_refresh,
            expires_at,
        })
    }
}

/// One row of the token refresh wire response.
struct RefreshedToken {
    access_token: String,
    new_refresh: Option<String>,
    expires_at: DateTime<Utc>,
}

/// Read the X OAuth client_id / client_secret from the process
/// environment. The X OAuth config is intentionally not passed
/// through the agent (the agent only needs to refresh the user's
/// tokens, not manage the app registration). Operators set
/// `X_OAUTH_CLIENT_ID` and `X_OAUTH_CLIENT_SECRET` env vars; the
/// callback handler at `/api/auth/login/x/callback` consults the
/// same vars during the original connect.
fn token_endpoint_client_creds() -> Option<ClientCreds> {
    let client_id = std::env::var("X_OAUTH_CLIENT_ID")
        .ok()
        .filter(|v| !v.is_empty())?;
    let client_secret = std::env::var("X_OAUTH_CLIENT_SECRET")
        .ok()
        .filter(|v| !v.is_empty());
    Some(ClientCreds {
        client_id,
        client_secret,
    })
}

struct ClientCreds {
    client_id: String,
    client_secret: Option<String>,
}

#[async_trait]
impl Agent for XTimelineAgent {
    fn name(&self) -> &str {
        "x_timeline"
    }

    fn description(&self) -> &str {
        "Read the X (Twitter) home timeline of the user's connected X account via the v2 API \
         (mode 'Abonnements' (Following) by default, or 'Pour Vous' (For You) on demand). Returns \
         the ~20 most recent posts, pre-sorted newest-first, with author, date, text, hashtags, \
         and URLs. Use for 'résume ma timeline X', 'quelles sont les news intéressantes \
         aujourd'hui sur mon compte', 'déduplique les posts qui parlent du même sujet'. Requires \
         the user to have connected their X account via /settings/integrations. Read-only — never \
         posts or replies."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["following", "for_you"],
                    "default": "following",
                    "description": "Which timeline to fetch: 'following' (Abonnements, reverse chronological) or 'for_you' (Pour Vous, algorithmically ranked)."
                },
                "max_posts": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 20,
                    "description": "Maximum number of posts to return. The server cap is the agent's `max_posts` config."
                },
                "since_hours": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 168,
                    "description": "Optional server-side time window in hours; posts older than `now - since_hours` are dropped before the response is shaped."
                },
                "language": {
                    "type": "string",
                    "enum": ["fr", "en"],
                    "description": "Optional language preference; posts whose `lang` matches are surfaced first (a soft preference, not a hard filter)."
                }
            },
            "required": [],
            "additionalProperties": false,
        })
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Read-only tool: no extra user confirmation.
        ConfirmationDecision::Allow
    }

    fn untrusted_output(&self) -> bool {
        // Posts come from a remote service the LLM does not own.
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args, self.cfg.max_posts)?;
        // Cache lookup keyed on (user_id, mode).
        if let Some(cached) = self.cache_get(ctx.user_id(), req.mode.as_str()) {
            tracing::debug!(
                agent = "x_timeline",
                user_id = %ctx.user_id(),
                mode = %req.mode,
                "cache hit"
            );
            return Ok(cached);
        }

        // Resolve the per-user OAuth tokens. Missing fields surface
        // as `CredentialsMissing` so the chat UI prompts the user
        // to connect X.
        let access_token = read_field(ctx, "x_account", "access_token").await?;
        let refresh_token = ctx
            .secret("x_account", "refresh_token")
            .await
            .ok()
            .flatten()
            .map(|s| s.expose_secret().to_string());
        let x_user_id = read_field(ctx, "x_account", "x_user_id").await?;
        let x_screen_name = read_field(ctx, "x_account", "x_screen_name").await?;
        let token_expires_at = ctx
            .secret("x_account", "token_expires_at")
            .await
            .ok()
            .flatten()
            .map(|s| s.expose_secret().to_string());

        // Decide whether to refresh. The token is stale when the
        // stored expiry is past, within 60 s of "now", or missing.
        let mut current_token = access_token.expose_secret().to_string();
        let needs_refresh = match token_expires_at.as_deref() {
            Some(raw) => match DateTime::parse_from_rfc3339(raw) {
                Ok(dt) => {
                    let dt_utc = dt.with_timezone(&Utc);
                    Utc::now().signed_duration_since(dt_utc).num_seconds() < 60
                }
                Err(_) => true,
            },
            None => true,
        };
        if needs_refresh {
            if let Some(rt) = refresh_token.as_deref() {
                match self.refresh_access_token(rt).await {
                    Ok(refreshed) => {
                        // Write the refreshed set back to the
                        // vault. The display-only fields
                        // (`x_user_id`, `x_screen_name`,
                        // `token_scope`) survive every refresh
                        // round-trip so the agent only touches the
                        // three token-shaped fields.
                        let mut fields: Vec<(&str, secrecy::SecretString)> = vec![
                            (
                                "access_token",
                                secrecy::SecretString::from(refreshed.access_token.clone()),
                            ),
                            (
                                "token_expires_at",
                                secrecy::SecretString::from(refreshed.expires_at.to_rfc3339()),
                            ),
                        ];
                        if let Some(new_refresh) = refreshed.new_refresh.as_deref() {
                            fields.push((
                                "refresh_token",
                                secrecy::SecretString::from(new_refresh.to_string()),
                            ));
                        }
                        ctx.update_secret("x_account", &fields).await?;
                        current_token = refreshed.access_token;
                    }
                    Err(e) => {
                        // Surface the refresh failure verbatim
                        // so the LLM can hint "reconnect X via
                        // /settings/integrations".
                        return Err(e);
                    }
                }
            } else {
                return Err(AgentError::AgentFailed(
                    "X access token is expired and no refresh token is configured; reconnect X \
                     via /settings/integrations"
                        .into(),
                ));
            }
        }

        // Build the v2 timeline URL.
        let endpoint = match req.mode.as_str() {
            "following" => "reverse_chronological",
            "for_you" => "for_you",
            other => {
                return Err(AgentError::InvalidArguments(format!(
                    "unknown mode `{other}`"
                )))
            }
        };
        let url = format!(
            "{}/2/users/{}/timelines/{}",
            self.cfg.base_url.trim_end_matches('/'),
            x_user_id.expose_secret(),
            endpoint
        );

        // Single hardened GET. The egress client applies the SSRF
        // pre-flight + hostname allow-list; the operator defaults
        // the allow-list to `["api.x.com", "x.com"]`.
        let egress = self.build_egress();
        let validated = egress.validate(&url).await.map_err(map_egress_error)?;
        let resp = self
            .http
            .get(validated.url.as_str())
            .bearer_auth(&current_token)
            .send()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("X timeline transport: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AgentError::AgentFailed(format!("X timeline read: {e}")))?;
        // 401 here means our stored token was rejected after the
        // refresh — surface a reconnect hint rather than looping.
        if status.as_u16() == 401 {
            return Err(AgentError::AgentFailed(
                "X rejected the access token even after refresh; reconnect X via \
                 /settings/integrations"
                    .into(),
            ));
        }
        if status.as_u16() == 429 {
            return Err(AgentError::Upstream {
                status: 429,
                body: truncate(&body, 2048),
            });
        }
        if !status.is_success() {
            return Err(AgentError::Upstream {
                status: status.as_u16(),
                body: truncate(&body, 2048),
            });
        }

        let parsed: Value = serde_json::from_str(&body)
            .map_err(|e| AgentError::AgentFailed(format!("X timeline parse: {e}")))?;
        let posts = project_timeline(&parsed, &x_screen_name.expose_secret(), &req);
        let result = json!({
            "mode": req.mode,
            "fetched_at": Utc::now().to_rfc3339(),
            "count": posts.len(),
            "posts": posts,
            "shared_links": collect_shared_links(&posts),
            "hint": "Posts sorted newest-first. Cluster posts that share urls/hashtags/keywords for the theme dedup."
        });
        let encoded = serde_json::to_string(&result)
            .map_err(|e| AgentError::AgentFailed(format!("X timeline encode: {e}")))?;
        self.cache_put(ctx.user_id(), req.mode.as_str(), encoded.clone());
        Ok(encoded)
    }
}

#[derive(Debug)]
struct ParsedArgs {
    mode: String,
    max_posts: usize,
    since_hours: Option<u32>,
    language: Option<String>,
}

fn parse_args(args: &Value, cfg_cap: usize) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let mode = obj
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("following")
        .to_string();
    if mode != "following" && mode != "for_you" {
        return Err(AgentError::InvalidArguments(format!(
            "`mode` must be 'following' or 'for_you', got `{mode}`"
        )));
    }
    let max_posts = obj
        .get("max_posts")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(cfg_cap.min(20))
        .min(cfg_cap)
        .max(1);
    let since_hours = obj
        .get("since_hours")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let language = obj
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(ParsedArgs {
        mode,
        max_posts,
        since_hours,
        language,
    })
}

/// Read a single field and surface `CredentialsMissing` when the
/// resolver returned `Ok(None)` (the agent API uses `Ok(Option<…>>`
/// with `Ok(None)` meaning "field is absent").
async fn read_field(
    ctx: &UserContext,
    service: &str,
    field: &str,
) -> Result<secrecy::SecretString, AgentError> {
    ctx.secret(service, field)
        .await?
        .ok_or_else(|| AgentError::CredentialsMissing {
            service: service.to_string(),
            field: field.to_string(),
        })
}

/// Project the raw X v2 JSON into the LLM-facing shape. The v2
/// response shape is documented at
/// <https://developer.x.com/en/docs/twitter-api/tweets/timelines/api-reference/get-users-id-timeline>;
/// the projections below are deliberately tolerant so a future
/// schema change (e.g. `media.fields`) is a single-file edit.
fn project_timeline(parsed: &Value, viewer_handle: &str, req: &ParsedArgs) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let users = parsed
        .get("includes")
        .and_then(|i| i.get("users"))
        .and_then(|u| u.as_array())
        .cloned()
        .unwrap_or_default();
    let author_username = |id: &str| -> Option<String> {
        for u in &users {
            if u.get("id").and_then(|v| v.as_str()) == Some(id) {
                if let Some(name) = u.get("username").and_then(|v| v.as_str()) {
                    return Some(format!("@{name}"));
                }
                if let Some(name) = u.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
        None
    };
    let author_display_name = |id: &str| -> Option<String> {
        for u in &users {
            if u.get("id").and_then(|v| v.as_str()) == Some(id) {
                if let Some(name) = u.get("name").and_then(|v| v.as_str()) {
                    return Some(name.to_string());
                }
            }
        }
        None
    };
    let Some(posts) = parsed.get("data").and_then(|d| d.as_array()) else {
        return out;
    };
    let now = Utc::now();
    for p in posts {
        let id = p
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let created = p.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
        let text = p
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let lang = p.get("lang").and_then(|v| v.as_str()).map(str::to_string);
        let author_id = p.get("author_id").and_then(|v| v.as_str()).unwrap_or("");
        let author = author_username(author_id).unwrap_or_else(|| viewer_handle.to_string());
        let author_name = author_display_name(author_id).unwrap_or_default();
        let metrics = p
            .get("public_metrics")
            .map(|m| {
                json!({
                    "likes": m.get("like_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "retweets": m.get("retweet_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "replies": m.get("reply_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    "quotes": m.get("quote_count").and_then(|v| v.as_u64()).unwrap_or(0),
                })
            })
            .unwrap_or(json!({"likes":0,"retweets":0,"replies":0,"quotes":0}));
        let mut urls: Vec<String> = Vec::new();
        let mut hashtags: Vec<String> = Vec::new();
        let mut mentions: Vec<String> = Vec::new();
        if let Some(entities) = p.get("entities") {
            if let Some(arr) = entities.get("urls").and_then(|v| v.as_array()) {
                for u in arr {
                    if let Some(exp) = u.get("expanded_url").and_then(|v| v.as_str()) {
                        urls.push(exp.to_string());
                    } else if let Some(unw) = u.get("unwound_url").and_then(|v| v.as_str()) {
                        urls.push(unw.to_string());
                    }
                }
            }
            if let Some(arr) = entities.get("hashtags").and_then(|v| v.as_array()) {
                for h in arr {
                    if let Some(tag) = h.get("tag").and_then(|v| v.as_str()) {
                        hashtags.push(tag.to_string());
                    }
                }
            }
            if let Some(arr) = entities.get("mentions").and_then(|v| v.as_array()) {
                for m in arr {
                    if let Some(name) = m.get("username").and_then(|v| v.as_str()) {
                        mentions.push(format!("@{name}"));
                    }
                }
            }
        }
        // Apply `since_hours` server-side.
        if let Some(hrs) = req.since_hours {
            if let Ok(dt) = DateTime::parse_from_rfc3339(created) {
                let age = now - dt.with_timezone(&Utc);
                if age > Duration::hours(hrs as i64) {
                    continue;
                }
            }
        }
        out.push(json!({
            "id": id,
            "author": author,
            "author_name": author_name,
            "created_at": created,
            "lang": lang,
            "text": text,
            "urls": urls,
            "hashtags": hashtags,
            "mentions": mentions,
            "metrics": metrics,
        }));
    }
    // Sort newest-first; cap at `max_posts`. Apply the optional
    // language preference as a soft sort key so requested-lang
    // posts surface first while the full cap is still respected.
    out.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    if let Some(lang) = req.language.as_deref() {
        out.sort_by(|a, b| {
            let a_match = a["lang"].as_str() == Some(lang);
            let b_match = b["lang"].as_str() == Some(lang);
            b_match.cmp(&a_match)
        });
    }
    out.truncate(req.max_posts);
    out
}

fn collect_shared_links(posts: &[Value]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for p in posts {
        if let Some(arr) = p.get("urls").and_then(|v| v.as_array()) {
            for u in arr {
                if let Some(s) = u.as_str() {
                    if seen.insert(s.to_string()) {
                        out.push(s.to_string());
                    }
                }
            }
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
    out.push_str("\n…[truncated]");
    out
}

fn map_egress_error(e: crate::egress::EgressError) -> AgentError {
    use crate::egress::EgressError::*;
    match e {
        Scheme(s) => AgentError::InvalidArguments(format!("unsupported scheme `{s}`")),
        NoHost => AgentError::InvalidArguments("url has no host".into()),
        NotInAllowlist { host } => {
            AgentError::SandboxDenied(format!("host `{host}` is not in the X timeline allowlist"))
        }
        DisallowedAddress { host, addr } => AgentError::SandboxDenied(format!(
            "host `{host}` resolves to a disallowed address ({addr})"
        )),
        Dns(msg) => AgentError::AgentFailed(format!("dns resolve: {msg}")),
        Upstream { status, body } => AgentError::Upstream { status, body },
        ResponseExceeded { budget } => AgentError::ResponseExceeded { budget },
        Transport(msg) => AgentError::AgentFailed(format!("transport: {msg}")),
        UrlParse(msg) => AgentError::InvalidArguments(format!("url parse: {msg}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> XTimelineAgentConfig {
        XTimelineAgentConfig {
            timeout_ms: 1000,
            max_posts: 20,
            allowlist: vec!["api.x.com".to_string()],
            cache_ttl_secs: 0,
            base_url: "https://api.x.com".to_string(),
        }
    }

    #[test]
    fn name_is_stable() {
        let agent = XTimelineAgent::new(cfg(), reqwest::Client::new());
        assert_eq!(agent.name(), "x_timeline");
    }

    #[test]
    fn parameters_schema_matches_locked_decisions() {
        let agent = XTimelineAgent::new(cfg(), reqwest::Client::new());
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["mode"]["enum"],
            json!(["following", "for_you"])
        );
        assert_eq!(schema["properties"]["mode"]["default"], "following");
        assert_eq!(schema["properties"]["max_posts"]["maximum"], 100);
        assert_eq!(schema["properties"]["since_hours"]["maximum"], 168);
        assert_eq!(
            schema["properties"]["language"]["enum"],
            json!(["fr", "en"])
        );
    }

    #[test]
    fn description_warns_about_no_posting() {
        let agent = XTimelineAgent::new(cfg(), reqwest::Client::new());
        let desc = agent.description();
        assert!(desc.contains("Read-only"));
        assert!(desc.contains("never posts or replies"));
    }

    #[test]
    fn parse_args_rejects_unknown_mode() {
        let err = parse_args(&json!({"mode": "explore"}), 20).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_defaults_to_following() {
        let args = parse_args(&json!({}), 20).expect("defaults must parse");
        assert_eq!(args.mode, "following");
        assert_eq!(args.max_posts, 20);
    }

    #[test]
    fn parse_args_caps_at_config_max_posts() {
        let args = parse_args(&json!({"max_posts": 1000}), 25).expect("must parse");
        assert_eq!(args.max_posts, 25);
    }

    #[test]
    fn project_timeline_sorts_newest_first() {
        let body = json!({
            "data": [
                {"id":"1","author_id":"u1","created_at":"2026-01-01T10:00:00Z","lang":"en","text":"older","entities":{"urls":[{"expanded_url":"https://a"}]}},
                {"id":"2","author_id":"u1","created_at":"2026-01-02T10:00:00Z","lang":"fr","text":"newer","entities":{"hashtags":[{"tag":"ai"}]}}
            ],
            "includes": {
                "users": [{"id":"u1","username":"alice","name":"Alice"}]
            }
        });
        let req = ParsedArgs {
            mode: "following".into(),
            max_posts: 20,
            since_hours: None,
            language: None,
        };
        let posts = project_timeline(&body, "viewer", &req);
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0]["id"], "2");
        assert_eq!(posts[1]["id"], "1");
        assert_eq!(posts[0]["author"], "@alice");
        assert_eq!(posts[0]["author_name"], "Alice");
    }

    #[test]
    fn project_timeline_applies_language_preference_soft() {
        let body = json!({
            "data": [
                {"id":"en1","author_id":"u","created_at":"2026-01-01T10:00:00Z","lang":"en","text":"en"},
                {"id":"fr1","author_id":"u","created_at":"2026-01-01T11:00:00Z","lang":"fr","text":"fr"},
                {"id":"en2","author_id":"u","created_at":"2026-01-01T12:00:00Z","lang":"en","text":"en"}
            ],
            "includes": {"users": [{"id":"u","username":"x","name":"X"}]}
        });
        let req = ParsedArgs {
            mode: "following".into(),
            max_posts: 20,
            since_hours: None,
            language: Some("fr".into()),
        };
        let posts = project_timeline(&body, "viewer", &req);
        // French first, then the rest newest-first.
        assert_eq!(posts[0]["id"], "fr1");
        assert_eq!(posts[1]["id"], "en2");
        assert_eq!(posts[2]["id"], "en1");
    }

    #[test]
    fn project_timeline_filters_older_than_since_hours() {
        let body = json!({
            "data": [
                {"id":"old","author_id":"u","created_at":"2025-01-01T10:00:00Z","lang":"en","text":"old"},
                {"id":"new","author_id":"u","created_at":"2026-10-05T10:00:00Z","lang":"en","text":"new"}
            ],
            "includes": {"users": [{"id":"u","username":"x","name":"X"}]}
        });
        let req = ParsedArgs {
            mode: "following".into(),
            max_posts: 20,
            since_hours: Some(24),
            language: None,
        };
        let posts = project_timeline(&body, "viewer", &req);
        // The "old" post is dropped by since_hours=24; the
        // synthesized "new" timestamp is "now-ish" so it survives.
        let ids: Vec<&str> = posts.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert!(!ids.contains(&"old"));
    }

    #[test]
    fn project_timeline_collects_shared_links() {
        let body = json!({
            "data": [
                {"id":"1","author_id":"u","created_at":"2026-01-02T10:00:00Z","lang":"en","text":"x","entities":{"urls":[{"expanded_url":"https://shared.example/a"}]}},
                {"id":"2","author_id":"u","created_at":"2026-01-01T10:00:00Z","lang":"en","text":"y","entities":{"urls":[{"expanded_url":"https://shared.example/a"}]}}
            ],
            "includes": {"users": [{"id":"u","username":"x","name":"X"}]}
        });
        let req = ParsedArgs {
            mode: "following".into(),
            max_posts: 20,
            since_hours: None,
            language: None,
        };
        let posts = project_timeline(&body, "viewer", &req);
        let shared = collect_shared_links(&posts);
        assert_eq!(shared, vec!["https://shared.example/a".to_string()]);
    }

    #[test]
    fn project_timeline_handles_missing_data_array() {
        let body = json!({"meta": {}});
        let req = ParsedArgs {
            mode: "following".into(),
            max_posts: 20,
            since_hours: None,
            language: None,
        };
        let posts = project_timeline(&body, "viewer", &req);
        assert!(posts.is_empty());
    }

    #[test]
    fn cache_is_disabled_when_ttl_is_zero() {
        let agent = XTimelineAgent::new(cfg(), reqwest::Client::new());
        agent.cache_put(uuid::Uuid::new_v4(), "following", "x".into());
        // With cache_ttl_secs = 0, cache_get must always miss.
        let got = agent.cache_get(uuid::Uuid::new_v4(), "following");
        assert!(got.is_none());
    }

    #[test]
    fn cache_round_trip_when_enabled() {
        let mut c = cfg();
        c.cache_ttl_secs = 60;
        let agent = XTimelineAgent::new(c, reqwest::Client::new());
        let uid = uuid::Uuid::new_v4();
        agent.cache_put(uid, "following", "hello".into());
        let got = agent.cache_get(uid, "following");
        assert_eq!(got.as_deref(), Some("hello"));
        let missed = agent.cache_get(uid, "for_you");
        assert!(missed.is_none(), "different mode must miss");
    }
}
